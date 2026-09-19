use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncWriteExt, copy};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, timeout};

use crate::cli::ServeArgs;
use crate::client::DesktopClient;
use crate::desktop::{DesktopDriver, X11Driver};
use crate::service::{self, ServiceConfig};
use crate::state::{
    ClientDescriptor, DESCRIPTOR_VERSION, LocalRuntimeStore, Secret, validate_session_name,
};
use uuid::Uuid;
use vdesk_protocol::Geometry;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

struct RuntimeConfig {
    bind: SocketAddr,
    session_id: Uuid,
    session_name: String,
    runtime_generation: u64,
    data_token: Secret,
    viewer_token: Secret,
    display: String,
    width: u32,
    height: u32,
    dpi: u16,
    scale: u16,
    rfb_addr: SocketAddr,
    human_input_socket: PathBuf,
    xvnc_rfb_socket: PathBuf,
    dbus_address: String,
    dbus_socket: PathBuf,
    workspace_root: Option<PathBuf>,
    downloads_root: Option<PathBuf>,
    local_store: Option<LocalRuntimeStore>,
}

impl RuntimeConfig {
    fn from_environment(args: ServeArgs, session_name: &str) -> Result<Self> {
        let bind: SocketAddr = args
            .bind
            .unwrap_or_else(|| if args.local { "127.0.0.1:0" } else { "0.0.0.0:7777" }.into())
            .parse()
            .context("parse service bind address")?;
        if args.local && !bind.ip().is_loopback() {
            bail!("a local runtime must bind its service to a loopback address");
        }
        if args.workspace_root.is_some() != args.downloads_root.is_some() {
            bail!("--workspace-root and --downloads-root must be provided together");
        }
        let local_store = args.local.then(LocalRuntimeStore::discover).transpose()?;
        if let Some(store) = &local_store {
            store.ensure()?;
        }
        let session_id = if args.local {
            Uuid::new_v4()
        } else {
            required_env("VDESK_SESSION_ID")?.parse().context("parse VDESK_SESSION_ID")?
        };
        validate_session_name(session_name)?;
        let runtime_generation = if args.local {
            1
        } else {
            env::var("VDESK_RUNTIME_GENERATION")
                .unwrap_or_else(|_| "1".into())
                .parse()
                .context("parse VDESK_RUNTIME_GENERATION")?
        };
        let data_token = if args.local {
            Secret::generate()?
        } else {
            Secret::parse(required_env("VDESK_DATA_TOKEN")?)?
        };
        let viewer_token = if args.local {
            Secret::generate()?
        } else {
            Secret::parse(required_env("VDESK_VIEWER_TOKEN")?)?
        };
        let display = env::var("VDESK_DISPLAY").unwrap_or_else(|_| ":99".into());
        let (width, height) =
            parse_size(&env::var("VDESK_SIZE").unwrap_or_else(|_| "1280x800".into()))?;
        let dpi = env::var("VDESK_DPI")
            .unwrap_or_else(|_| "96".into())
            .parse()
            .context("parse VDESK_DPI")?;
        let scale = env::var("VDESK_SCALE")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .context("parse VDESK_SCALE")?;
        let rfb_addr = if args.local {
            "127.0.0.1:5900".parse().expect("fixed RFB address is valid")
        } else {
            env::var("VDESK_RFB_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:5900".into())
                .parse()
                .context("parse VDESK_RFB_ADDR")?
        };
        let local_root = local_store.as_ref().map(|store| store.root());
        let human_input_socket = local_root.map_or_else(
            || {
                PathBuf::from(
                    env::var("VDESK_HUMAN_INPUT_SOCKET")
                        .unwrap_or_else(|_| "/run/vdesk/human-input.sock".into()),
                )
            },
            |root| root.join("human-input.sock"),
        );
        let xvnc_rfb_socket = local_root.map_or_else(
            || PathBuf::from("/run/vdesk/xvnc-rfb.sock"),
            |root| root.join("xvnc-rfb.sock"),
        );
        let dbus_address = local_root.map_or_else(
            || {
                env::var("DBUS_SESSION_BUS_ADDRESS")
                    .unwrap_or_else(|_| "unix:path=/run/vdesk/bus".into())
            },
            |root| format!("unix:path={}", root.join("bus").display()),
        );
        let dbus_socket = dbus_address
            .strip_prefix("unix:path=")
            .map(PathBuf::from)
            .context("DBUS_SESSION_BUS_ADDRESS must be a unix:path address")?;
        let dbus_is_bounded =
            local_store.as_ref().is_some_and(|store| dbus_socket.starts_with(store.root()))
                || is_bounded_runtime_path(&dbus_socket);
        if !dbus_is_bounded {
            bail!("D-Bus socket must stay inside the private vdesk runtime directory");
        }
        Ok(Self {
            bind,
            session_id,
            session_name: session_name.to_owned(),
            runtime_generation,
            data_token,
            viewer_token,
            display,
            width,
            height,
            dpi,
            scale,
            rfb_addr,
            human_input_socket,
            xvnc_rfb_socket,
            dbus_address,
            dbus_socket,
            workspace_root: if args.local {
                args.workspace_root
                    .map(|path| validate_scope_root(path, "workspace"))
                    .transpose()?
            } else {
                Some(PathBuf::from("/workspace"))
            },
            downloads_root: if args.local {
                args.downloads_root
                    .map(|path| validate_scope_root(path, "downloads"))
                    .transpose()?
            } else {
                Some(PathBuf::from("/downloads"))
            },
            local_store,
        })
    }
}

struct ManagedChild {
    name: &'static str,
    child: Child,
}

impl ManagedChild {
    fn ensure_running(&mut self) -> Result<()> {
        if let Some(status) =
            self.child.try_wait().with_context(|| format!("poll {}", self.name))?
        {
            bail!("{} exited unexpectedly with {status}", self.name);
        }
        Ok(())
    }

    async fn terminate(&mut self) {
        let Some(pid) = self.child.id() else {
            return;
        };
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status().await;
        if timeout(CHILD_SHUTDOWN_TIMEOUT, self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
            let _ = self.child.wait().await;
        }
    }
}

pub async fn run(args: ServeArgs, session_name: &str) -> Result<()> {
    let config = RuntimeConfig::from_environment(args, session_name)?;

    let mut children = Vec::new();
    let result = run_inner(&config, &mut children).await;
    for child in children.iter_mut().rev() {
        child.terminate().await;
    }
    let _ = remove_stale_socket(&config.xvnc_rfb_socket, "Xvnc RFB");
    if let Some(store) = &config.local_store {
        store.remove(config.session_id)?;
    }
    result
}

pub async fn rfb_stdio() -> Result<()> {
    let store = LocalRuntimeStore::discover()?;
    let descriptor = store
        .load()?
        .context("no local vdesk runtime is available; start `vdesk serve --local`")?;
    let client = DesktopClient::from_client_descriptor(&descriptor)?;
    let health = client.health().await.context("local vdesk runtime is not healthy")?;
    if !health.ready
        || health.session_id != descriptor.session_id.to_string()
        || health.runtime_generation != descriptor.runtime_generation
    {
        bail!("local vdesk descriptor does not match the running service");
    }

    let stream = tokio::net::TcpStream::connect("127.0.0.1:5900")
        .await
        .context("connect to the local vdesk RFB service")?;
    let (mut rfb_read, mut rfb_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = copy(&mut stdin, &mut rfb_write) => {
            result.context("forward viewer input")?;
            rfb_write.shutdown().await.context("close viewer input")?;
        }
        result = copy(&mut rfb_read, &mut stdout) => {
            result.context("forward viewer output")?;
            stdout.flush().await.context("flush viewer output")?;
        }
    }
    Ok(())
}

async fn run_inner(config: &RuntimeConfig, children: &mut Vec<ManagedChild>) -> Result<()> {
    ensure_x11_socket_directory()?;
    remove_stale_socket(&config.dbus_socket, "D-Bus")?;
    let dbus = spawn(
        "dbus-daemon",
        ["--session", "--nofork", "--nopidfile", &format!("--address={}", config.dbus_address)],
        &[],
    )?;
    children.push(dbus);
    wait_for_socket(&config.dbus_socket, "D-Bus", children).await?;

    children.push(spawn_display(config)?);
    wait_for_display(config, children).await?;

    let desktop = spawn(
        "xfce4-session",
        std::iter::empty::<&str>(),
        &[
            ("DISPLAY", &config.display),
            ("NO_AT_BRIDGE", "0"),
            ("DBUS_SESSION_BUS_ADDRESS", &config.dbus_address),
        ],
    )?;
    children.push(desktop);
    wait_for_desktop(config, children).await?;

    let executable = env::current_exe().context("locate vdesk executable")?;
    let executable_text = executable.to_string_lossy();
    if executable_text.chars().any(char::is_whitespace) {
        bail!("vdesk executable path cannot contain whitespace inside the desktop image");
    }
    let pipe_input = format!(
        "tee,reopen:{} pipe-input --socket {}",
        executable_text,
        config.human_input_socket.display()
    );
    let x11vnc = spawn(
        "x11vnc",
        [
            "-display",
            config.display.as_str(),
            "-rfbport",
            &config.rfb_addr.port().to_string(),
            "-localhost",
            "-forever",
            "-shared",
            "-nopw",
            "-seldir",
            "recv",
            "-pipeinput",
            &pipe_input,
        ],
        &[("DISPLAY", &config.display)],
    )?;
    children.push(x11vnc);
    wait_for_tcp(config.rfb_addr, children).await?;

    let driver = Arc::new(
        X11Driver::connect(&config.display, config.dpi, config.scale)?
            .with_session_bus(&config.dbus_address),
    );
    if driver.geometry().width != config.width || driver.geometry().height != config.height {
        bail!("X display geometry does not match the requested session geometry");
    }
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("bind desktop service at {}", config.bind))?;
    let service_addr = listener.local_addr().context("read desktop service address")?;
    if let Some(store) = &config.local_store {
        store.save(&ClientDescriptor {
            descriptor_version: DESCRIPTOR_VERSION,
            session_id: config.session_id,
            name: config.session_name.clone(),
            endpoint: format!("http://{service_addr}"),
            data_token: config.data_token.clone(),
            geometry: Geometry {
                width: config.width,
                height: config.height,
                dpi: config.dpi,
                scale: config.scale,
            },
            runtime_generation: config.runtime_generation,
        })?;
    }
    let service_config = ServiceConfig {
        session_id: config.session_id.to_string(),
        runtime_generation: config.runtime_generation,
        data_token: config.data_token.clone(),
        viewer_token: config.viewer_token.clone(),
        rfb_addr: config.rfb_addr,
        human_input_socket: Some(config.human_input_socket.clone()),
        workspace_root: config.workspace_root.clone(),
        downloads_root: config.downloads_root.clone(),
        process_environment: vec![
            ("DISPLAY".into(), config.display.clone()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), config.dbus_address.clone()),
        ],
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut server =
        tokio::spawn(service::serve_on(listener, service_config, driver, async move {
            let _ = shutdown_rx.await;
        }));
    let mut monitor = tokio::time::interval(Duration::from_millis(500));
    monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupted =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let reason = loop {
        tokio::select! {
            _ = terminate.recv() => break None,
            _ = interrupted.recv() => break None,
            result = &mut server => {
                break Some(match result {
                    Ok(Ok(())) => anyhow::anyhow!("desktop service exited unexpectedly"),
                    Ok(Err(error)) => error.context("desktop service failed"),
                    Err(error) => anyhow::Error::new(error).context("desktop service task failed"),
                });
            }
            _ = monitor.tick() => {
                if let Err(error) = ensure_children(children) {
                    break Some(error);
                }
            }
        }
    };

    let _ = shutdown_tx.send(());
    if !server.is_finished() {
        let _ = timeout(Duration::from_secs(3), &mut server).await;
    }
    if let Some(error) = reason { Err(error) } else { Ok(()) }
}

fn spawn<I, S>(program: &'static str, args: I, environment: &[(&str, &str)]) -> Result<ManagedChild>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
    for (name, value) in environment {
        command.env(name, value);
    }
    let child = command.spawn().with_context(|| format!("start {program}"))?;
    Ok(ManagedChild { name: program, child })
}

fn spawn_display(config: &RuntimeConfig) -> Result<ManagedChild> {
    if let Some(program) = ["Xvnc", "Xtigervnc"].into_iter().find(|name| command_exists(name)) {
        remove_stale_socket(&config.xvnc_rfb_socket, "Xvnc RFB")?;
        return spawn(
            program,
            [
                config.display.clone(),
                "-geometry".into(),
                format!("{}x{}", config.width, config.height),
                "-depth".into(),
                "24".into(),
                "-dpi".into(),
                config.dpi.to_string(),
                "-s".into(),
                "0".into(),
                "-dpms".into(),
                "-nolisten".into(),
                "tcp".into(),
                "-ac".into(),
                "-rfbport".into(),
                "-1".into(),
                "-rfbunixpath".into(),
                config.xvnc_rfb_socket.display().to_string(),
                "-rfbunixmode".into(),
                "0600".into(),
                "-SecurityTypes".into(),
                "None".into(),
            ],
            &[],
        );
    }

    spawn(
        "Xvfb",
        [
            config.display.as_str(),
            "-screen",
            "0",
            &format!("{}x{}x24", config.width, config.height),
            "-dpi",
            &config.dpi.to_string(),
            "-s",
            "0",
            "-dpms",
            "-nolisten",
            "tcp",
            "-ac",
        ],
        &[],
    )
}

fn command_exists(command: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| {
            let Ok(metadata) = directory.join(command).metadata() else {
                return false;
            };
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        })
    })
}

async fn wait_for_display(config: &RuntimeConfig, children: &mut [ManagedChild]) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if X11Driver::connect(&config.display, config.dpi, config.scale).is_ok() {
            return Ok(());
        }
        ensure_children(children)?;
        if Instant::now() >= deadline {
            bail!("X display did not become ready within {STARTUP_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_tcp(address: SocketAddr, children: &mut [ManagedChild]) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return Ok(());
        }
        ensure_children(children)?;
        if Instant::now() >= deadline {
            bail!("RFB service did not become ready within {STARTUP_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_desktop(config: &RuntimeConfig, children: &mut [ManagedChild]) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        ensure_children(children)?;
        if visible_window_exists(&config.display, "xfce4-panel").await
            && visible_window_exists(&config.display, "Desktop").await
        {
            // Mapping precedes the first paint. Let both components service their queued exposes,
            // then verify that the supervised session stayed alive.
            sleep(Duration::from_millis(750)).await;
            ensure_children(children)?;
            if visible_window_exists(&config.display, "xfce4-panel").await
                && visible_window_exists(&config.display, "Desktop").await
            {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("graphical desktop did not become ready within {STARTUP_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn visible_window_exists(display: &str, name: &str) -> bool {
    Command::new("xdotool")
        .env("DISPLAY", display)
        .args(["search", "--onlyvisible", "--name", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

async fn wait_for_socket(path: &Path, name: &str, children: &mut [ManagedChild]) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        ensure_children(children)?;
        if Instant::now() >= deadline {
            bail!("{name} did not become ready within {STARTUP_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn remove_stale_socket(path: &Path, name: &str) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).with_context(|| format!("remove stale {}", path.display()))
        }
        Ok(_) => bail!("refusing non-socket {name} path at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_x11_socket_directory() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let path = Path::new("/tmp/.X11-unix");
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => bail!("refusing non-directory X11 socket path at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect X11 socket directory"),
    }
    match std::fs::create_dir(path) {
        Ok(()) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o1777))
            .context("set X11 socket directory permissions"),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata =
                std::fs::symlink_metadata(path).context("inspect X11 socket directory")?;
            if metadata.is_dir() {
                Ok(())
            } else {
                bail!("refusing non-directory X11 socket path at {}", path.display())
            }
        }
        Err(error) => Err(error).context("create X11 socket directory"),
    }
}

fn ensure_children(children: &mut [ManagedChild]) -> Result<()> {
    for child in children {
        child.ensure_running()?;
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is required"))
}

fn validate_scope_root(path: PathBuf, name: &str) -> Result<PathBuf> {
    let path = std::fs::canonicalize(&path)
        .with_context(|| format!("resolve {name} root {}", path.display()))?;
    if !path.is_dir() {
        bail!("{name} root is not a directory: {}", path.display());
    }
    Ok(path)
}

fn parse_size(value: &str) -> Result<(u32, u32)> {
    let Some((width, height)) = value.split_once('x') else {
        bail!("VDESK_SIZE must be WIDTHxHEIGHT");
    };
    let width: u32 = width.parse().context("parse desktop width")?;
    let height: u32 = height.parse().context("parse desktop height")?;
    if !(320..=8192).contains(&width) || !(240..=8192).contains(&height) {
        bail!("VDESK_SIZE must be between 320x240 and 8192x8192");
    }
    Ok((width, height))
}

#[allow(dead_code)]
fn is_bounded_runtime_path(path: &Path) -> bool {
    path.starts_with("/run/vdesk") || path.starts_with("/tmp/vdesk")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn runtime_size_parser_is_strict() {
        assert_eq!(parse_size("1280x800").unwrap(), (1280, 800));
        assert!(parse_size("1280X800").is_err());
        assert!(parse_size("8193x800").is_err());
    }

    #[test]
    fn human_input_socket_scope_is_narrow() {
        assert!(is_bounded_runtime_path(Path::new("/run/vdesk/input.sock")));
        assert!(!is_bounded_runtime_path(Path::new("/tmp/unrelated.sock")));
    }

    #[test]
    fn scope_roots_must_resolve_to_directories() {
        let temporary = tempdir().unwrap();
        assert_eq!(
            validate_scope_root(temporary.path().to_owned(), "workspace").unwrap(),
            temporary.path()
        );
        assert!(validate_scope_root(temporary.path().join("missing"), "workspace").is_err());
    }
}
