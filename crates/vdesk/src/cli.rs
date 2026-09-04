use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;
use tokio::time::{Instant, sleep};
use uuid::Uuid;
use vdesk_protocol::{Action, ActionBatchRequest, BatchStatus, Geometry, MouseButton, ObserveMode};

use crate::client::{DesktopClient, write_atomic};
use crate::engine::{ContainerInspection, Engine};
use crate::state::{
    EngineKind, SessionDescriptor, StateStore, load_client_descriptor, validate_session_name,
};
use crate::{pipe_input, supervisor};

#[derive(Debug, Parser)]
#[command(name = "vdesk", version, about = "Agent-friendly graphical Linux desktops")]
pub struct Cli {
    /// Emit stable JSON rather than human-oriented text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Session name to operate on.
    #[arg(long, global = true, default_value = "default", env = "VDESK_SESSION")]
    pub session: String,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Check local dependencies and state.
    Doctor,
    /// Report capabilities of a running session.
    Capabilities,
    /// Capture a bounded AT-SPI accessibility snapshot.
    A11y,
    /// Build or inspect the desktop image.
    Image(ImageArgs),
    /// Create or reconnect to a desktop session.
    Open(OpenArgs),
    /// Inspect a session.
    Status,
    /// List known sessions.
    Sessions,
    /// Capture an exact desktop observation.
    See(SeeArgs),
    /// Report the current pointer position.
    Cursor,
    /// Click at framebuffer coordinates.
    Click(ClickArgs),
    /// Move the pointer.
    Move(MoveArgs),
    /// Drag between framebuffer coordinates.
    Drag(DragArgs),
    /// Scroll on either axis.
    Scroll(ScrollArgs),
    /// Type text directly or from stdin.
    Type(TypeArgs),
    /// Press a key or chord such as CTRL+S.
    Key(KeyArgs),
    /// Wait without injecting input.
    Wait(WaitArgs),
    /// Execute an ordered JSON action batch.
    Batch(BatchArgs),
    /// List or focus windows.
    Windows(WindowsArgs),
    /// Read or write the desktop text clipboard.
    Clipboard(ClipboardArgs),
    /// Launch an application with structured argv.
    Launch(LaunchArgs),
    /// Start and inspect bounded managed processes inside the desktop.
    Process(ProcessArgs),
    /// Import a local file into the scoped desktop filesystem.
    Import(TransferArgs),
    /// Export a scoped desktop file locally.
    Export(TransferArgs),
    /// Open or print the interactive noVNC viewer URL.
    View(ViewArgs),
    /// Stop a session without discarding it.
    Stop,
    /// Recreate the desktop runtime while retaining its workspace attachment.
    Reset,
    /// Delete a session and its owned runtime state.
    Delete,
    /// Launch an agent container beside its desktop.
    Run(RunArgs),
    #[command(hide = true)]
    Serve(ServeArgs),
    #[command(hide = true)]
    PipeInput(PipeInputArgs),
}

#[derive(Debug, Args)]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    Build(ImageBuildArgs),
}

#[derive(Debug, Args)]
pub struct ImageBuildArgs {
    #[arg(long, value_enum)]
    pub engine: Option<EngineArg>,
    #[arg(long, default_value = "localhost/vdesk:dev")]
    pub tag: String,
    #[arg(long, value_enum, default_value = "default")]
    pub profile: ImageProfileArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ImageProfileArg {
    Default,
    Minimal,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EngineArg {
    Podman,
    Docker,
}

impl From<EngineArg> for EngineKind {
    fn from(value: EngineArg) -> Self {
        match value {
            EngineArg::Podman => Self::Podman,
            EngineArg::Docker => Self::Docker,
        }
    }
}

#[derive(Debug, Args)]
pub struct OpenArgs {
    #[arg(long, value_enum)]
    pub engine: Option<EngineArg>,
    #[arg(long, default_value = "localhost/vdesk:dev", env = "VDESK_IMAGE")]
    pub image: String,
    #[arg(long, default_value = "1280x800", value_parser = parse_size)]
    pub size: (u32, u32),
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long, conflicts_with = "workspace")]
    pub no_workspace: bool,
    #[arg(long, conflicts_with = "offline")]
    pub online: bool,
    #[arg(long, conflicts_with = "online")]
    pub offline: bool,
}

#[derive(Debug, Args)]
pub struct SeeArgs {
    #[arg(long, default_value = "screenshot.png")]
    pub output: PathBuf,
    #[arg(long)]
    pub include_cursor: bool,
}

#[derive(Debug, Args)]
pub struct ClickArgs {
    pub x: i32,
    pub y: i32,
    #[arg(long, value_enum, default_value = "left")]
    pub button: ButtonArg,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub count: u8,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ButtonArg {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    pub x: i32,
    pub y: i32,
    #[arg(long, default_value_t = 0)]
    pub duration_ms: u64,
}

#[derive(Debug, Args)]
pub struct DragArgs {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    #[arg(long, value_enum, default_value = "left")]
    pub button: ButtonArg,
    #[arg(long, default_value_t = 500)]
    pub duration_ms: u64,
}

#[derive(Debug, Args)]
pub struct ScrollArgs {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Debug, Args)]
pub struct TypeArgs {
    pub text: Option<String>,
    #[arg(long, conflicts_with = "text")]
    pub stdin: bool,
    #[arg(long, default_value_t = 0)]
    pub delay_ms: u64,
}

#[derive(Debug, Args)]
pub struct KeyArgs {
    pub chord: String,
    #[arg(long, default_value_t = 1)]
    pub repeat: u16,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    pub duration: String,
}

#[derive(Debug, Args)]
pub struct BatchArgs {
    pub input: PathBuf,
}

#[derive(Debug, Args)]
pub struct LaunchArgs {
    pub application: String,
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ProcessArgs {
    #[command(subcommand)]
    pub command: ProcessCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProcessCommand {
    Start(ProcessStartArgs),
    Status(ProcessIdArgs),
    Output(ProcessIdArgs),
    Wait(ProcessWaitArgs),
    Kill(ProcessIdArgs),
}

#[derive(Debug, Args)]
pub struct ProcessStartArgs {
    #[arg(long, value_enum)]
    pub cwd: Option<ScopeArg>,
    #[arg(last = true, required = true)]
    pub argv: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ProcessIdArgs {
    pub process_id: String,
}

#[derive(Debug, Args)]
pub struct ProcessWaitArgs {
    pub process_id: String,
    #[arg(long, default_value = "30s")]
    pub timeout: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ScopeArg {
    Workspace,
    Downloads,
}

impl From<ScopeArg> for vdesk_protocol::FileScope {
    fn from(value: ScopeArg) -> Self {
        match value {
            ScopeArg::Workspace => Self::Workspace,
            ScopeArg::Downloads => Self::Downloads,
        }
    }
}

#[derive(Debug, Args)]
pub struct WindowsArgs {
    /// Focus this window ID before returning the list.
    #[arg(long)]
    pub focus: Option<String>,
}

#[derive(Debug, Args)]
pub struct ClipboardArgs {
    #[command(subcommand)]
    pub command: ClipboardCommand,
}

#[derive(Debug, Subcommand)]
pub enum ClipboardCommand {
    Get,
    Set(ClipboardSetArgs),
}

#[derive(Debug, Args)]
pub struct ClipboardSetArgs {
    pub text: Option<String>,
    #[arg(long, conflicts_with = "text")]
    pub stdin: bool,
}

#[derive(Debug, Args)]
pub struct TransferArgs {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Debug, Args)]
pub struct ViewArgs {
    /// Print the URL without opening a browser.
    #[arg(long)]
    pub print_url: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long)]
    pub agent_image: String,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    #[arg(long, default_value = "0.0.0.0:7777")]
    pub bind: String,
}

#[derive(Debug, Args)]
pub struct PipeInputArgs {
    #[arg(long)]
    pub socket: PathBuf,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    version: &'static str,
    state_dir: String,
    podman: CommandStatus,
    docker: CommandStatus,
}

#[derive(Debug, Serialize)]
struct CommandStatus {
    found: bool,
    path: Option<String>,
}

pub async fn run(cli: Cli) -> Result<()> {
    let Cli { json, session, command } = cli;
    match command {
        Commands::Doctor => doctor(json),
        Commands::Capabilities => capabilities(&session, json).await,
        Commands::A11y => a11y(&session, json).await,
        Commands::Image(args) => image(args).await,
        Commands::Open(args) => open(&session, args, json).await,
        Commands::Status => status(&session, json).await,
        Commands::Sessions => sessions(json).await,
        Commands::See(args) => see(&session, args, json).await,
        Commands::Cursor => cursor(&session, json).await,
        Commands::Click(args) => {
            action(
                &session,
                vec![Action::Click {
                    x: args.x,
                    y: args.y,
                    button: args.button.into(),
                    count: args.count,
                }],
                json,
            )
            .await
        }
        Commands::Move(args) => {
            action(
                &session,
                vec![Action::Move { x: args.x, y: args.y, duration_ms: args.duration_ms }],
                json,
            )
            .await
        }
        Commands::Drag(args) => {
            action(
                &session,
                vec![Action::Drag {
                    from: vdesk_protocol::Point { x: args.x1, y: args.y1 },
                    to: vdesk_protocol::Point { x: args.x2, y: args.y2 },
                    button: args.button.into(),
                    duration_ms: args.duration_ms,
                }],
                json,
            )
            .await
        }
        Commands::Scroll(args) => {
            action(&session, vec![Action::Scroll { dx: args.dx, dy: args.dy }], json).await
        }
        Commands::Type(args) => type_text(&session, args, json).await,
        Commands::Key(args) => {
            action(
                &session,
                vec![Action::KeyPress { keys: parse_chord(&args.chord)?, repeat: args.repeat }],
                json,
            )
            .await
        }
        Commands::Wait(args) => {
            action(
                &session,
                vec![Action::Wait { duration_ms: parse_duration_ms(&args.duration)? }],
                json,
            )
            .await
        }
        Commands::Batch(args) => batch(&session, args, json).await,
        Commands::Windows(args) => windows(&session, args, json).await,
        Commands::Clipboard(args) => clipboard(&session, args, json).await,
        Commands::Launch(args) => launch(&session, args, json).await,
        Commands::Process(args) => process(&session, args, json).await,
        Commands::Import(args) => import_file(&session, args, json).await,
        Commands::Export(args) => export_file(&session, args, json).await,
        Commands::View(args) => view(&session, args, json),
        Commands::Stop => stop(&session, json).await,
        Commands::Reset => reset(&session, json).await,
        Commands::Delete => delete(&session, json).await,
        Commands::Run(args) => run_agent(&session, args).await,
        Commands::Serve(args) => supervisor::run(args).await,
        Commands::PipeInput(args) => pipe_input::run(&args.socket),
    }
}

impl From<ButtonArg> for MouseButton {
    fn from(value: ButtonArg) -> Self {
        match value {
            ButtonArg::Left => Self::Left,
            ButtonArg::Middle => Self::Middle,
            ButtonArg::Right => Self::Right,
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenReport<'a> {
    session: crate::state::PublicSessionDescriptor<'a>,
    state: &'static str,
    viewer_url: String,
}

#[derive(Debug, Serialize)]
struct StatusReport<'a> {
    session: crate::state::PublicSessionDescriptor<'a>,
    container: ContainerInspection,
    service_ready: bool,
}

async fn image(args: ImageArgs) -> Result<()> {
    let ImageCommand::Build(args) = args.command;
    let engine = Engine::discover(args.engine.map(Into::into))?;
    let repository = repository_root()?;
    let target = match args.profile {
        ImageProfileArg::Default => "default",
        ImageProfileArg::Minimal => "minimal",
    };
    engine.build(&args.tag, &repository, target).await?;
    println!("built {} with {}", args.tag, engine.kind());
    Ok(())
}

async fn open(name: &str, args: OpenArgs, json_output: bool) -> Result<()> {
    validate_session_name(name)?;
    let store = StateStore::discover()?;
    store.ensure()?;
    if let Ok(mut descriptor) = store.load(name) {
        let engine = Engine::discover(Some(descriptor.engine))?;
        let inspection = engine.inspect(&descriptor.container_name).await?;
        if inspection.exists {
            if !inspection.running {
                engine.start(&descriptor.container_name).await?;
            }
            let inspection = wait_for_endpoint(&engine, &descriptor.container_name).await?;
            descriptor.host_endpoint = usable_host_endpoint(&inspection, descriptor.offline)?;
            if let Some(address) = inspection.container_ip.clone() {
                descriptor.container_endpoint = format!("http://{address}:7777");
            }
            store.save(&descriptor)?;
            wait_for_service(&descriptor, &engine).await?;
            print_open(&descriptor, "ready", json_output)?;
            return Ok(());
        }
        let _ = engine.remove_network(&descriptor.network_name).await;
        store.remove(name)?;
        store.remove_client_descriptor(name)?;
    }

    let engine = Engine::discover(args.engine.map(Into::into))?;
    let workspace = workspace_from_args(&args)?;
    let id = Uuid::new_v4();
    let suffix = &id.simple().to_string()[..10];
    let stem: String = name.chars().take(36).collect();
    let container_name = format!("vdesk-{stem}-{suffix}");
    let network_name = format!("{container_name}-net");
    let geometry = Geometry { width: args.size.0, height: args.size.1, dpi: 96, scale: 1 };
    let mut descriptor = SessionDescriptor::new(
        name.to_owned(),
        engine.kind(),
        container_name,
        network_name,
        args.image,
        "pending".into(),
        "pending".into(),
        geometry,
        now_ms(),
    )?;
    descriptor.workspace = workspace;
    descriptor.offline = args.offline;
    descriptor.container_endpoint = format!("http://{}:7777", descriptor.container_name);
    store.save(&descriptor)?;

    let create_result = async {
        engine.create_network(&descriptor).await?;
        engine.create_container(&descriptor).await?;
        let inspection = wait_for_endpoint(&engine, &descriptor.container_name).await?;
        descriptor.host_endpoint = usable_host_endpoint(&inspection, descriptor.offline)?;
        descriptor.container_endpoint = format!(
            "http://{}:7777",
            inspection.container_ip.context("desktop does not have a private network address")?
        );
        store.save(&descriptor)?;
        wait_for_service(&descriptor, &engine).await
    }
    .await;

    if let Err(error) = create_result {
        let logs = engine.logs(&descriptor.container_name, 80).await.unwrap_or_default();
        let _ = engine.remove_container(&descriptor.container_name, true).await;
        let _ = engine.remove_network(&descriptor.network_name).await;
        let _ = store.remove(name);
        bail!("desktop failed to become ready: {error:#}\n{}", tail_for_error(&logs));
    }
    print_open(&descriptor, "created", json_output)
}

fn workspace_from_args(args: &OpenArgs) -> Result<Option<PathBuf>> {
    if args.no_workspace {
        return Ok(None);
    }
    let path = match &args.workspace {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("get current directory")?,
    };
    let path =
        fs::canonicalize(&path).with_context(|| format!("resolve workspace {}", path.display()))?;
    if !path.is_dir() {
        bail!("workspace is not a directory: {}", path.display());
    }
    Ok(Some(path))
}

async fn wait_for_endpoint(engine: &Engine, container: &str) -> Result<ContainerInspection> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspection = engine.inspect(container).await?;
        if !inspection.exists {
            bail!("desktop container disappeared during startup");
        }
        if !inspection.running {
            bail!(
                "desktop container exited during startup with status {} and code {:?}",
                inspection.status,
                inspection.exit_code
            );
        }
        if inspection.host_endpoint.is_some() || inspection.container_ip.is_some() {
            return Ok(inspection);
        }
        if Instant::now() >= deadline {
            bail!("container engine did not assign a desktop endpoint within 10 seconds");
        }
        sleep(Duration::from_millis(100)).await;
    }
}

fn usable_host_endpoint(inspection: &ContainerInspection, offline: bool) -> Result<String> {
    if let Some(endpoint) = &inspection.host_endpoint {
        return Ok(endpoint.clone());
    }
    if offline && let Some(address) = &inspection.container_ip {
        return Ok(format!("http://{address}:7777"));
    }
    bail!("desktop does not expose a host-reachable data endpoint")
}

async fn wait_for_service(descriptor: &SessionDescriptor, engine: &Engine) -> Result<()> {
    let client = DesktopClient::for_host(descriptor)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match client.health().await {
            Ok(health)
                if health.ready
                    && health.session_id == descriptor.session_id.to_string()
                    && health.runtime_generation == descriptor.runtime_generation =>
            {
                return Ok(());
            }
            _ => {}
        }
        let inspection = engine.inspect(&descriptor.container_name).await?;
        if !inspection.running {
            bail!("desktop container exited before its service became ready");
        }
        if Instant::now() >= deadline {
            bail!("desktop service did not become ready within 30 seconds");
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn print_open(
    descriptor: &SessionDescriptor,
    state: &'static str,
    json_output: bool,
) -> Result<()> {
    let report =
        OpenReport { session: descriptor.public(), state, viewer_url: viewer_url(descriptor) };
    if json_output {
        print_json(&report)
    } else {
        println!("vdesk '{}' is ready at {}", descriptor.name, descriptor.host_endpoint);
        println!("view: vdesk --session {} view", descriptor.name);
        Ok(())
    }
}

async fn status(name: &str, json_output: bool) -> Result<()> {
    let store = StateStore::discover()?;
    let descriptor = store.load(name)?;
    let engine = Engine::discover(Some(descriptor.engine))?;
    let container = engine.inspect(&descriptor.container_name).await?;
    let service_ready = if container.running {
        DesktopClient::for_host(&descriptor)?.health().await.is_ok()
    } else {
        false
    };
    let report = StatusReport { session: descriptor.public(), container, service_ready };
    if json_output {
        print_json(&report)
    } else {
        println!(
            "{}: {}{}",
            descriptor.name,
            report.container.status,
            if service_ready { " (ready)" } else { "" }
        );
        println!("engine: {}  image: {}", descriptor.engine, descriptor.image);
        println!("endpoint: {}", descriptor.host_endpoint);
        Ok(())
    }
}

async fn sessions(json_output: bool) -> Result<()> {
    let store = StateStore::discover()?;
    let mut reports = Vec::new();
    for name in store.list()? {
        let descriptor = store.load(&name)?;
        let engine = Engine::discover(Some(descriptor.engine))?;
        let container = engine.inspect(&descriptor.container_name).await?;
        reports.push(json!({ "session": descriptor.public(), "container": container }));
    }
    if json_output {
        print_json(&reports)
    } else if reports.is_empty() {
        println!("no vdesk sessions");
        Ok(())
    } else {
        for report in reports {
            println!(
                "{}\t{}\t{}",
                report["session"]["name"].as_str().unwrap_or("?"),
                report["container"]["status"].as_str().unwrap_or("unknown"),
                report["session"]["engine"].as_str().unwrap_or("unknown")
            );
        }
        Ok(())
    }
}

async fn capabilities(name: &str, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let value = client.capabilities().await?;
    if json_output {
        print_json(&value)
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    }
}

async fn a11y(name: &str, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let snapshot = client.accessibility().await?;
    if json_output {
        print_json(&snapshot)
    } else {
        println!(
            "{} accessibility nodes{} (snapshot {})",
            snapshot.nodes.len(),
            if snapshot.truncated { ", truncated" } else { "" },
            snapshot.snapshot_id
        );
        for node in snapshot.nodes {
            if !node.name.is_empty() || !node.text.is_empty() {
                println!(
                    "{}\t{}\t{}\t{}",
                    node.id,
                    node.role,
                    node.name,
                    node.text.replace('\n', " ")
                );
            }
        }
        Ok(())
    }
}

async fn see(name: &str, args: SeeArgs, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let observation = client.observe(args.include_cursor).await?;
    let bytes = client.image(&observation).await?;
    write_atomic(&args.output, &bytes)?;
    if json_output {
        print_json(&json!({ "observation": observation, "output": args.output }))
    } else {
        println!(
            "wrote {} ({}x{}, {} bytes, sha256 {})",
            args.output.display(),
            observation.image.width,
            observation.image.height,
            bytes.len(),
            observation.image.sha256
        );
        Ok(())
    }
}

async fn cursor(name: &str, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let observation = client.observe(false).await?;
    let output = json!({
        "cursor": observation.cursor,
        "input_generation": observation.input_generation,
        "observation_id": observation.observation_id,
        "geometry": observation.geometry,
    });
    if json_output {
        print_json(&output)
    } else if let Some(cursor) = observation.cursor {
        println!("{},{}", cursor.x, cursor.y);
        Ok(())
    } else {
        println!("cursor unavailable");
        Ok(())
    }
}

async fn action(name: &str, actions: Vec<Action>, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let result = client.actions(actions, None, ObserveMode::Final).await?;
    if json_output {
        print_json(&result)
    } else {
        println!(
            "{}: {}/{} actions completed; input generation {}",
            batch_status(result.status),
            result.executed,
            result.requested,
            result.input_generation_after
        );
        Ok(())
    }
}

async fn type_text(name: &str, args: TypeArgs, json_output: bool) -> Result<()> {
    let text = match (args.text, args.stdin) {
        (Some(text), false) => text,
        (None, true) => {
            let mut bytes = Vec::new();
            io::stdin()
                .take((vdesk_protocol::MAX_TEXT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .context("read text from stdin")?;
            if bytes.len() > vdesk_protocol::MAX_TEXT_BYTES {
                bail!("stdin text exceeds the protocol limit");
            }
            String::from_utf8(bytes).context("stdin is not valid UTF-8")?
        }
        (None, false) => bail!("provide text or pass --stdin"),
        (Some(_), true) => unreachable!("clap enforces the conflict"),
    };
    action(name, vec![Action::TypeText { text, delay_ms: args.delay_ms }], json_output).await
}

async fn batch(name: &str, args: BatchArgs, json_output: bool) -> Result<()> {
    let metadata = fs::metadata(&args.input)
        .with_context(|| format!("read batch metadata from {}", args.input.display()))?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        bail!("batch input must be a regular file no larger than 1 MiB");
    }
    let bytes = fs::read(&args.input).context("read action batch")?;
    let request: ActionBatchRequest =
        serde_json::from_slice(&bytes).context("parse action batch JSON")?;
    let client = load_client(name)?;
    let result = client.action_batch(&request).await?;
    if json_output {
        print_json(&result)
    } else {
        println!(
            "{}: {}/{} actions completed",
            batch_status(result.status),
            result.executed,
            result.requested
        );
        Ok(())
    }
}

async fn windows(name: &str, args: WindowsArgs, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let list = match args.focus {
        Some(window_id) => client.focus_window(window_id).await?,
        None => client.windows().await?,
    };
    if json_output {
        print_json(&list)
    } else if list.windows.is_empty() {
        println!("no application windows");
        Ok(())
    } else {
        for window in list.windows {
            println!(
                "{}{}\t{}x{}+{}+{}\t{}",
                if window.active { "*" } else { " " },
                window.id,
                window.bounds.width,
                window.bounds.height,
                window.bounds.x,
                window.bounds.y,
                window.title
            );
        }
        Ok(())
    }
}

async fn clipboard(name: &str, args: ClipboardArgs, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    match args.command {
        ClipboardCommand::Get => {
            let content = client.read_clipboard().await?;
            if json_output {
                print_json(&content)
            } else {
                print!("{}", content.text);
                Ok(())
            }
        }
        ClipboardCommand::Set(args) => {
            let text = read_bounded_text(args.text, args.stdin)?;
            let content = client.write_clipboard(text).await?;
            if json_output {
                print_json(
                    &json!({ "protocol": content.protocol, "written_bytes": content.text.len() }),
                )
            } else {
                println!("wrote {} UTF-8 bytes to the desktop clipboard", content.text.len());
                Ok(())
            }
        }
    }
}

async fn launch(name: &str, args: LaunchArgs, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    let mut argv = vec![args.application];
    argv.extend(args.args);
    let result = client.launch(argv).await?;
    if json_output {
        print_json(&result)
    } else {
        println!("launched application as PID {}", result.pid);
        Ok(())
    }
}

async fn process(name: &str, args: ProcessArgs, json_output: bool) -> Result<()> {
    let client = load_client(name)?;
    match args.command {
        ProcessCommand::Start(args) => {
            let info = client.spawn_process(args.argv, args.cwd.map(Into::into)).await?;
            if json_output {
                print_json(&info)
            } else {
                println!("{} (PID {})", info.process_id, info.pid);
                Ok(())
            }
        }
        ProcessCommand::Status(args) => {
            let info = client.process_status(&args.process_id).await?;
            if json_output {
                print_json(&info)
            } else {
                println!(
                    "{}: {}{}",
                    info.process_id,
                    process_state(info.state),
                    info.exit_code.map_or_else(String::new, |code| format!(" ({code})"))
                );
                Ok(())
            }
        }
        ProcessCommand::Output(args) => {
            let output = client.process_output(&args.process_id).await?;
            if json_output {
                print_json(&output)
            } else {
                print!("{}", output.output);
                Ok(())
            }
        }
        ProcessCommand::Wait(args) => {
            let timeout_ms = parse_duration_ms(&args.timeout)?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let info = client.process_status(&args.process_id).await?;
                if info.state == vdesk_protocol::ProcessState::Exited {
                    if json_output {
                        return print_json(&info);
                    }
                    println!(
                        "{} exited{}",
                        info.process_id,
                        info.exit_code.map_or_else(String::new, |code| format!(" with {code}"))
                    );
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!("process did not exit within {} ms", timeout_ms);
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
        ProcessCommand::Kill(args) => {
            let info = client.kill_process(&args.process_id).await?;
            if json_output {
                print_json(&info)
            } else {
                println!("{} stopped", info.process_id);
                Ok(())
            }
        }
    }
}

fn process_state(state: vdesk_protocol::ProcessState) -> &'static str {
    match state {
        vdesk_protocol::ProcessState::Running => "running",
        vdesk_protocol::ProcessState::Exited => "exited",
    }
}

async fn import_file(name: &str, args: TransferArgs, json_output: bool) -> Result<()> {
    let metadata =
        fs::metadata(&args.source).with_context(|| format!("inspect {}", args.source.display()))?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
        bail!("import source must be a regular file no larger than 64 MiB");
    }
    let bytes = fs::read(&args.source).context("read import source")?;
    let (scope, path) = parse_remote_path(&args.destination)?;
    let client = load_client(name)?;
    let result = client.import_file(scope, &path, bytes).await?;
    if json_output {
        print_json(&result)
    } else {
        println!(
            "imported {} bytes to {}/{} (sha256 {})",
            result.byte_length,
            scope_name(result.scope),
            result.path,
            result.sha256
        );
        Ok(())
    }
}

async fn export_file(name: &str, args: TransferArgs, json_output: bool) -> Result<()> {
    let (scope, path) = parse_remote_path(&args.source)?;
    let client = load_client(name)?;
    let bytes = client.export_file(scope, &path).await?;
    write_atomic(&args.destination, &bytes)?;
    if json_output {
        print_json(&json!({
            "source": format!("{}/{}", scope_name(scope), path),
            "output": args.destination,
            "byte_length": bytes.len(),
        }))
    } else {
        println!("exported {} bytes to {}", bytes.len(), args.destination.display());
        Ok(())
    }
}

fn view(name: &str, args: ViewArgs, json_output: bool) -> Result<()> {
    let store = StateStore::discover()?;
    let descriptor = store.load(name)?;
    let url = viewer_url(&descriptor);
    if args.print_url || json_output {
        if json_output {
            print_json(&json!({ "session": name, "url": url }))?;
        } else {
            println!("{url}");
        }
        return Ok(());
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    match Command::new(opener).arg(&url).spawn() {
        Ok(_) => Ok(()),
        Err(error) => {
            println!("{url}");
            bail!("could not open a browser with {opener}: {error}")
        }
    }
}

async fn stop(name: &str, json_output: bool) -> Result<()> {
    let store = StateStore::discover()?;
    let descriptor = store.load(name)?;
    let engine = Engine::discover(Some(descriptor.engine))?;
    engine.stop(&descriptor.container_name).await?;
    if json_output {
        print_json(&json!({ "session": name, "state": "stopped" }))
    } else {
        println!("stopped vdesk '{name}'; its container state is preserved");
        Ok(())
    }
}

async fn reset(name: &str, json_output: bool) -> Result<()> {
    let store = StateStore::discover()?;
    let mut descriptor = store.load(name)?;
    let engine = Engine::discover(Some(descriptor.engine))?;
    engine.remove_container(&descriptor.container_name, true).await?;
    descriptor.rotate_runtime()?;
    store.remove_client_descriptor(name)?;
    store.save(&descriptor)?;
    let reset_result = async {
        engine.create_container(&descriptor).await?;
        let inspection = wait_for_endpoint(&engine, &descriptor.container_name).await?;
        descriptor.host_endpoint = usable_host_endpoint(&inspection, descriptor.offline)?;
        descriptor.container_endpoint = format!(
            "http://{}:7777",
            inspection.container_ip.context("desktop does not have a private network address")?
        );
        store.save(&descriptor)?;
        wait_for_service(&descriptor, &engine).await
    }
    .await;
    if let Err(error) = reset_result {
        let logs = engine.logs(&descriptor.container_name, 80).await.unwrap_or_default();
        let _ = engine.remove_container(&descriptor.container_name, true).await;
        bail!("desktop reset failed: {error:#}\n{}", tail_for_error(&logs));
    }
    if json_output {
        print_json(&json!({
            "session": descriptor.public(),
            "state": "reset",
            "viewer_url": viewer_url(&descriptor),
        }))
    } else {
        println!("reset vdesk '{name}' to runtime generation {}", descriptor.runtime_generation);
        Ok(())
    }
}

async fn delete(name: &str, json_output: bool) -> Result<()> {
    let store = StateStore::discover()?;
    let descriptor = store.load(name)?;
    let engine = Engine::discover(Some(descriptor.engine))?;
    engine.remove_container(&descriptor.container_name, true).await?;
    engine.remove_network(&descriptor.network_name).await?;
    store.remove_client_descriptor(name)?;
    store.remove(name)?;
    if json_output {
        print_json(&json!({ "session": name, "state": "deleted" }))
    } else {
        println!("deleted vdesk '{name}' and its session container");
        Ok(())
    }
}

async fn run_agent(name: &str, args: RunArgs) -> Result<()> {
    let store = StateStore::discover()?;
    let descriptor = store.load(name)?;
    let engine = Engine::discover(Some(descriptor.engine))?;
    let inspection = engine.inspect(&descriptor.container_name).await?;
    if !inspection.running {
        bail!("vdesk '{name}' is not running; run `vdesk --session {name} open` first");
    }
    let client_descriptor = descriptor.client_descriptor(true);
    let client_descriptor_path = store.save_client_descriptor(&client_descriptor)?;
    let binary = store.root().join("clients").join(format!("{name}-vdesk"));
    engine.copy_from_container(&descriptor.container_name, "/usr/local/bin/vdesk", &binary).await?;
    let exit = engine
        .run_agent(&descriptor, &args.agent_image, &binary, &client_descriptor_path, &args.command)
        .await?;
    if exit != 0 {
        bail!("agent container exited with status {exit}; desktop '{name}' remains running");
    }
    Ok(())
}

fn load_client(name: &str) -> Result<DesktopClient> {
    if let Some(path) = std::env::var_os("VDESK_DESCRIPTOR") {
        let descriptor = load_client_descriptor(Path::new(&path))?;
        return DesktopClient::from_client_descriptor(&descriptor);
    }
    let descriptor = StateStore::discover()?.load(name)?;
    DesktopClient::for_host(&descriptor)
}

fn read_bounded_text(text: Option<String>, stdin: bool) -> Result<String> {
    match (text, stdin) {
        (Some(text), false) => Ok(text),
        (None, true) => {
            let mut bytes = Vec::new();
            io::stdin()
                .take((vdesk_protocol::MAX_TEXT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .context("read text from stdin")?;
            if bytes.len() > vdesk_protocol::MAX_TEXT_BYTES {
                bail!("stdin text exceeds the protocol limit");
            }
            String::from_utf8(bytes).context("stdin is not valid UTF-8")
        }
        (None, false) => bail!("provide text or pass --stdin"),
        (Some(_), true) => unreachable!("clap enforces the conflict"),
    }
}

fn parse_remote_path(path: &Path) -> Result<(vdesk_protocol::FileScope, String)> {
    let text = path.to_str().context("desktop path is not valid UTF-8")?;
    let Some((scope, relative)) = text.split_once('/') else {
        bail!("desktop path must begin with workspace/ or downloads/");
    };
    let scope = match scope {
        "workspace" => vdesk_protocol::FileScope::Workspace,
        "downloads" => vdesk_protocol::FileScope::Downloads,
        _ => bail!("desktop path must begin with workspace/ or downloads/"),
    };
    if !is_safe_relative(Path::new(relative)) || relative.is_empty() {
        bail!("desktop path must be normalized and remain inside its scope");
    }
    Ok((scope, relative.to_owned()))
}

fn scope_name(scope: vdesk_protocol::FileScope) -> &'static str {
    match scope {
        vdesk_protocol::FileScope::Workspace => "workspace",
        vdesk_protocol::FileScope::Downloads => "downloads",
    }
}

fn viewer_url(descriptor: &SessionDescriptor) -> String {
    format!(
        "{}/novnc/vnc.html?autoconnect=1&resize=scale&path=viewer%2Fws%3Ftoken%3D{}",
        descriptor.host_endpoint.trim_end_matches('/'),
        descriptor.viewer_token.expose()
    )
}

fn repository_root() -> Result<PathBuf> {
    let current = std::env::current_dir().context("get current directory")?;
    if current.join("container/Containerfile").is_file() {
        return Ok(current);
    }
    let compiled = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("resolve repository from build path")?;
    if compiled.join("container/Containerfile").is_file() {
        return Ok(compiled.to_owned());
    }
    bail!("run this command from a vdesk source checkout containing container/Containerfile")
}

fn parse_chord(chord: &str) -> Result<Vec<String>> {
    let keys: Vec<String> = chord.split('+').map(str::trim).map(str::to_owned).collect();
    if keys.iter().any(String::is_empty) {
        bail!("key chord contains an empty key");
    }
    Ok(keys)
}

fn parse_duration_ms(value: &str) -> Result<u64> {
    let (number, factor) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else {
        (value, 1)
    };
    let number: u64 = number.parse().context("duration must be an integer followed by ms or s")?;
    number.checked_mul(factor).context("duration is too large")
}

fn batch_status(status: BatchStatus) -> &'static str {
    match status {
        BatchStatus::Completed => "completed",
        BatchStatus::Partial => "partial",
        BatchStatus::Failed => "failed",
        BatchStatus::Cancelled => "cancelled",
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn tail_for_error(logs: &str) -> String {
    logs.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn doctor(json: bool) -> Result<()> {
    let state = StateStore::discover()?;
    state.ensure()?;
    let report = DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        state_dir: state.root().display().to_string(),
        podman: command_status("podman"),
        docker: command_status("docker"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("vdesk {}", report.version);
        println!("state: {}", report.state_dir);
        print_command_status("podman", &report.podman);
        print_command_status("docker", &report.docker);
    }
    Ok(())
}

fn print_command_status(name: &str, status: &CommandStatus) {
    match &status.path {
        Some(path) => println!("{name}: found at {path}"),
        None => println!("{name}: not found"),
    }
}

fn command_status(command: &str) -> CommandStatus {
    let path = find_in_path(command).map(|path| path.display().to_string());
    CommandStatus { found: path.is_some(), path }
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|directory| directory.join(command))
        .find(|candidate| candidate.is_file())
}

fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let Some((width, height)) = value.split_once('x') else {
        return Err("size must be WIDTHxHEIGHT".into());
    };
    let width: u32 = width.parse().map_err(|_| "width must be an integer")?;
    let height: u32 = height.parse().map_err(|_| "height must be an integer")?;
    if !(320..=8192).contains(&width) || !(240..=8192).contains(&height) {
        return Err("size must be between 320x240 and 8192x8192".into());
    }
    Ok((width, height))
}

#[allow(dead_code)]
fn command_exists(command: &str) -> bool {
    Command::new(command).arg("--version").output().is_ok()
}

#[allow(dead_code)]
fn is_safe_relative(path: &Path) -> bool {
    !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, std::path::Component::Normal(_) | std::path::Component::CurDir)
        })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn zero_coordinates_are_preserved() {
        let cli = Cli::try_parse_from(["vdesk", "click", "0", "0"]).unwrap();
        let Commands::Click(click) = cli.command else {
            panic!("wrong command");
        };
        assert_eq!((click.x, click.y, click.count), (0, 0, 1));
    }

    #[test]
    fn open_size_is_strict() {
        assert!(Cli::try_parse_from(["vdesk", "open", "--size", "1280x800"]).is_ok());
        assert!(Cli::try_parse_from(["vdesk", "open", "--size", "1280X800"]).is_err());
        assert!(Cli::try_parse_from(["vdesk", "open", "--size", "0x800"]).is_err());
    }

    #[test]
    fn type_requires_text_or_explicit_stdin_at_execution_time() {
        let cli = Cli::try_parse_from(["vdesk", "type", "--stdin"]).unwrap();
        let Commands::Type(input) = cli.command else {
            panic!("wrong command");
        };
        assert!(input.stdin);
        assert!(input.text.is_none());
    }

    #[test]
    fn agent_command_is_not_reparsed() {
        let cli = Cli::try_parse_from([
            "vdesk",
            "run",
            "--agent-image",
            "example/agent",
            "--",
            "agent",
            "--flag",
            "value with spaces",
        ])
        .unwrap();
        let Commands::Run(run) = cli.command else {
            panic!("wrong command");
        };
        assert_eq!(run.command, ["agent", "--flag", "value with spaces"]);
    }

    #[test]
    fn relative_scope_rejects_parent_and_absolute_paths() {
        assert!(is_safe_relative(Path::new("downloads/file.txt")));
        assert!(!is_safe_relative(Path::new("../secret")));
        assert!(!is_safe_relative(Path::new("/etc/passwd")));
    }
}
