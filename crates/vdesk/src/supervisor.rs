use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep, timeout};

use crate::cli::ServeArgs;
use crate::desktop::{DesktopDriver, X11Driver};
use crate::service::{self, ServiceConfig};
use crate::state::Secret;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

struct RuntimeConfig {
    bind: SocketAddr,
    session_id: String,
    runtime_generation: u64,
    data_token: Secret,
    viewer_token: Secret,
    display: String,
    width: u32,
    height: u32,
    dpi: u16,
    scale: u16,
    novnc_dir: PathBuf,
    rfb_addr: SocketAddr,
    human_input_socket: PathBuf,
    dbus_address: String,
    dbus_socket: PathBuf,
}

impl RuntimeConfig {
    fn from_environment(args: ServeArgs) -> Result<Self> {
        let bind = args.bind.parse().context("parse service bind address")?;
        let session_id = required_env("VDESK_SESSION_ID")?;
        let runtime_generation = env::var("VDESK_RUNTIME_GENERATION")
            .unwrap_or_else(|_| "1".into())
            .parse()
            .context("parse VDESK_RUNTIME_GENERATION")?;
        let data_token = Secret::parse(required_env("VDESK_DATA_TOKEN")?)?;
        let viewer_token = Secret::parse(required_env("VDESK_VIEWER_TOKEN")?)?;
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
        let novnc_dir = PathBuf::from(
            env::var("VDESK_NOVNC_DIR").unwrap_or_else(|_| "/usr/share/novnc".into()),
        );
        let rfb_addr = env::var("VDESK_RFB_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:5900".into())
            .parse()
            .context("parse VDESK_RFB_ADDR")?;
        let human_input_socket = PathBuf::from(
            env::var("VDESK_HUMAN_INPUT_SOCKET")
                .unwrap_or_else(|_| "/run/vdesk/human-input.sock".into()),
        );
        let dbus_address = env::var("DBUS_SESSION_BUS_ADDRESS")
            .unwrap_or_else(|_| "unix:path=/run/vdesk/bus".into());
        let dbus_socket = dbus_address
            .strip_prefix("unix:path=")
            .map(PathBuf::from)
            .context("DBUS_SESSION_BUS_ADDRESS must be a unix:path address")?;
        if !is_bounded_runtime_path(&dbus_socket) {
            bail!("D-Bus socket must stay under /run/vdesk or /tmp/vdesk");
        }
        if !novnc_dir.join("vnc.html").is_file() {
            bail!("noVNC assets are missing at {}", novnc_dir.display());
        }
        Ok(Self {
            bind,
            session_id,
            runtime_generation,
            data_token,
            viewer_token,
            display,
            width,
            height,
            dpi,
            scale,
            novnc_dir,
            rfb_addr,
            human_input_socket,
            dbus_address,
            dbus_socket,
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

pub async fn run(args: ServeArgs) -> Result<()> {
    let config = RuntimeConfig::from_environment(args)?;

    let mut children = Vec::new();
    let result = run_inner(&config, &mut children).await;
    for child in children.iter_mut().rev() {
        child.terminate().await;
    }
    result
}

async fn run_inner(config: &RuntimeConfig, children: &mut Vec<ManagedChild>) -> Result<()> {
    remove_stale_socket(&config.dbus_socket)?;
    let dbus = spawn(
        "dbus-daemon",
        ["--session", "--nofork", "--nopidfile", &format!("--address={}", config.dbus_address)],
        &[],
    )?;
    children.push(dbus);
    wait_for_socket(&config.dbus_socket, "D-Bus").await?;

    let xvfb = spawn(
        "Xvfb",
        [
            config.display.as_str(),
            "-screen",
            "0",
            &format!("{}x{}x24", config.width, config.height),
            "-dpi",
            &config.dpi.to_string(),
            "-nolisten",
            "tcp",
            "-ac",
        ],
        &[],
    )?;
    children.push(xvfb);
    wait_for_display(config).await?;

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
    wait_for_tcp(config.rfb_addr).await?;

    let driver = Arc::new(X11Driver::connect(&config.display, config.dpi, config.scale)?);
    if driver.geometry().width != config.width || driver.geometry().height != config.height {
        bail!("X display geometry does not match the requested session geometry");
    }
    let service_config = ServiceConfig {
        bind: config.bind,
        session_id: config.session_id.clone(),
        runtime_generation: config.runtime_generation,
        data_token: config.data_token.clone(),
        viewer_token: config.viewer_token.clone(),
        novnc_dir: config.novnc_dir.clone(),
        rfb_addr: config.rfb_addr,
        human_input_socket: Some(config.human_input_socket.clone()),
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut server = tokio::spawn(service::serve(service_config, driver, async move {
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

async fn wait_for_display(config: &RuntimeConfig) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if X11Driver::connect(&config.display, config.dpi, config.scale).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("X display did not become ready within {STARTUP_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_tcp(address: SocketAddr) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return Ok(());
        }
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

async fn wait_for_socket(path: &Path, name: &str) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("{name} did not become ready within {STARTUP_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).with_context(|| format!("remove stale {}", path.display()))
        }
        Ok(_) => bail!("refusing non-socket D-Bus path at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
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
}
