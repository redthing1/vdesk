use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt as _;
use std::path::{Path as FsPath, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;
use vdesk_protocol::{
    AccessibilitySnapshot, AccessibilityStatus, Action, ActionBatchRequest, ActionBatchResult,
    ActionResult, ActionStatus, ApiError, BatchStatus, CapabilityReport, CaptureRequest,
    ClipboardContent, CoordinateSpace, DeliveryStatus, EffectStatus, ErrorCode, ErrorEnvelope,
    FileMetadata, FileScope, HealthResponse, ImageResource, LaunchRequest, LaunchResult,
    Observation, ObserveMode, PROTOCOL_VERSION, ProcessInfo, ProcessOutput, ProcessSpawnRequest,
    ProcessState, ProtocolLimits, WindowFocusRequest, WindowList,
};

use crate::desktop::{DesktopDriver, DriverCapture};
use crate::state::Secret;
use crate::viewer::{self, RfbTarget, ViewerConfig};

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARGV_ITEMS: usize = 128;
const MAX_ARGV_BYTES: usize = 64 * 1024;
const MAX_PROCESSES: usize = 32;
const MAX_PROCESS_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_IMAGES: usize = 8;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_RESULTS: usize = 128;
const API_TIMEOUT: Duration = Duration::from_secs(40);
const DRIVER_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub session_id: String,
    pub runtime_generation: u64,
    pub data_token: Secret,
    pub viewer_token: Secret,
    pub rfb_addr: SocketAddr,
    pub human_input_socket: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub downloads_root: Option<PathBuf>,
    pub process_environment: Vec<(String, String)>,
}

pub struct ServiceState {
    config: ServiceConfig,
    driver: Arc<dyn DesktopDriver>,
    observation_id: AtomicU64,
    input_generation: AtomicU64,
    images: Mutex<ImageStore>,
    results: Mutex<ResultStore>,
    action_lock: Mutex<()>,
    processes: Mutex<ProcessStore>,
}

impl ServiceState {
    pub fn new(config: ServiceConfig, driver: Arc<dyn DesktopDriver>) -> Arc<Self> {
        Arc::new(Self {
            config,
            driver,
            observation_id: AtomicU64::new(0),
            input_generation: AtomicU64::new(0),
            images: Mutex::new(ImageStore::default()),
            results: Mutex::new(ResultStore::default()),
            action_lock: Mutex::new(()),
            processes: Mutex::new(ProcessStore::default()),
        })
    }

    pub fn note_human_input(&self) -> u64 {
        self.input_generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

#[derive(Default)]
struct ImageStore {
    entries: VecDeque<ImageEntry>,
    total_bytes: usize,
}

#[derive(Clone)]
struct ImageEntry {
    metadata: ImageResource,
    bytes: Arc<[u8]>,
}

impl ImageStore {
    fn insert(&mut self, entry: ImageEntry) -> Result<(), ApiFailure> {
        if entry.bytes.len() > MAX_IMAGE_BYTES {
            return Err(ApiFailure::internal("captured image exceeds the resource limit"));
        }
        while self.entries.len() >= MAX_IMAGES
            || self.total_bytes.saturating_add(entry.bytes.len()) > MAX_IMAGE_BYTES
        {
            let Some(removed) = self.entries.pop_front() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(removed.bytes.len());
        }
        self.total_bytes += entry.bytes.len();
        self.entries.push_back(entry);
        Ok(())
    }

    fn get(&self, id: &str) -> Option<ImageEntry> {
        self.entries.iter().find(|entry| entry.metadata.id == id).cloned()
    }
}

#[derive(Default)]
struct ResultStore {
    entries: VecDeque<CachedResult>,
}

#[derive(Clone)]
struct CachedResult {
    request_id: String,
    request_hash: [u8; 32],
    result: ActionBatchResult,
}

enum CacheLookup {
    Miss,
    Hit(Box<ActionBatchResult>),
    Conflict,
}

impl ResultStore {
    fn get(&self, request_id: &str, request_hash: &[u8; 32]) -> CacheLookup {
        match self.entries.iter().find(|entry| entry.request_id == request_id) {
            Some(entry) if &entry.request_hash == request_hash => {
                CacheLookup::Hit(Box::new(entry.result.clone()))
            }
            Some(_) => CacheLookup::Conflict,
            None => CacheLookup::Miss,
        }
    }

    fn insert(&mut self, entry: CachedResult) {
        while self.entries.len() >= MAX_CACHED_RESULTS {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }
}

#[derive(Default)]
struct ProcessStore {
    entries: HashMap<String, ManagedProcess>,
}

struct ManagedProcess {
    child: Child,
    output: Arc<StdMutex<BoundedOutput>>,
    started_at_ms: u64,
    exit_code: Option<i32>,
    finished: bool,
}

#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedOutput {
    fn append(&mut self, bytes: &[u8]) {
        let remaining = MAX_PROCESS_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.truncated |= bytes.len() > remaining;
    }
}

impl ManagedProcess {
    fn info(&mut self, process_id: String) -> std::io::Result<ProcessInfo> {
        if !self.finished
            && let Some(status) = self.child.try_wait()?
        {
            self.exit_code = status.code();
            self.finished = true;
        }
        let output_truncated = self.output.lock().map(|value| value.truncated).unwrap_or(true);
        Ok(ProcessInfo {
            protocol: PROTOCOL_VERSION,
            process_id,
            pid: self.child.id(),
            state: if self.finished { ProcessState::Exited } else { ProcessState::Running },
            exit_code: self.exit_code,
            started_at_ms: self.started_at_ms,
            output_truncated,
        })
    }
}

impl Drop for ProcessStore {
    fn drop(&mut self) {
        for process in self.entries.values_mut() {
            if !process.finished {
                kill_process_group(process.child.id());
                let _ = process.child.wait();
            }
        }
    }
}

#[derive(Debug)]
struct ApiFailure {
    status: StatusCode,
    envelope: ErrorEnvelope,
}

impl ApiFailure {
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { status, envelope: ErrorEnvelope::new(code, message) }
    }

    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized, "missing or invalid token")
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, message)
    }

    fn as_api_error(&self) -> ApiError {
        self.envelope.error.clone()
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (self.status, Json(self.envelope)).into_response()
    }
}

struct ApiJson<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = ApiFailure;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(request, state)
            .await
            .map_err(|error: JsonRejection| ApiFailure::invalid(error.body_text()))?;
        Ok(Self(value))
    }
}

pub fn router(config: ServiceConfig, driver: Arc<dyn DesktopDriver>) -> Router {
    router_from_state(ServiceState::new(config, driver))
}

fn router_from_state(state: Arc<ServiceState>) -> Router {
    let viewer = viewer::router(ViewerConfig {
        token: state.config.viewer_token.clone(),
        target: RfbTarget::Tcp(state.config.rfb_addr),
    });
    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/observations", post(observe))
        .route("/v1/images/{image_id}", get(image))
        .route("/v1/actions", post(actions))
        .route("/v1/windows", get(windows))
        .route("/v1/windows/focus", post(focus_window))
        .route("/v1/clipboard", get(read_clipboard).put(write_clipboard))
        .route("/v1/launch", post(launch))
        .route("/v1/accessibility", get(accessibility))
        .route("/v1/processes", post(spawn_process))
        .route("/v1/processes/{process_id}", get(process_status).delete(kill_process))
        .route("/v1/processes/{process_id}/output", get(process_output))
        .with_state(Arc::clone(&state))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(StatusCode::REQUEST_TIMEOUT, API_TIMEOUT));
    let files = Router::new()
        .route("/v1/files/{scope}/{*path}", get(read_file).put(write_file).delete(delete_file))
        .with_state(Arc::clone(&state))
        .layer(DefaultBodyLimit::max(MAX_FILE_BYTES));
    Router::new().merge(api).merge(files).merge(viewer)
}

pub async fn serve_on(
    listener: tokio::net::TcpListener,
    config: ServiceConfig,
    driver: Arc<dyn DesktopDriver>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let human_input_socket = config.human_input_socket.clone();
    let state = ServiceState::new(config, driver);
    let human_listener = if let Some(path) = human_input_socket {
        let socket = bind_human_input_socket(&path).await?;
        Some(tokio::spawn(human_input_listener(socket, Arc::clone(&state))))
    } else {
        None
    };
    let result =
        axum::serve(listener, router_from_state(state)).with_graceful_shutdown(shutdown).await;
    if let Some(task) = human_listener {
        task.abort();
    }
    result.map_err(Into::into)
}

async fn bind_human_input_socket(path: &FsPath) -> anyhow::Result<tokio::net::UnixDatagram> {
    use std::os::unix::fs::FileTypeExt;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_socket() => tokio::fs::remove_file(path).await?,
        Ok(_) => anyhow::bail!("human-input path is not a socket: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    tokio::net::UnixDatagram::bind(path).map_err(Into::into)
}

async fn human_input_listener(
    socket: tokio::net::UnixDatagram,
    state: Arc<ServiceState>,
) -> anyhow::Result<()> {
    let mut buffer = [0_u8; 64];
    loop {
        let read = socket.recv(&mut buffer).await?;
        if read > 0 {
            state.note_human_input();
        }
    }
}

async fn health(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
) -> Result<Json<HealthResponse>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    Ok(Json(HealthResponse {
        protocol: PROTOCOL_VERSION,
        ready: true,
        session_id: state.config.session_id.clone(),
        runtime_generation: state.config.runtime_generation,
    }))
}

async fn capabilities(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
) -> Result<Json<CapabilityReport>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    Ok(Json(CapabilityReport {
        protocol: PROTOCOL_VERSION,
        geometry: state.driver.geometry(),
        actions: [
            "move",
            "click",
            "mouse_down",
            "mouse_up",
            "drag",
            "scroll",
            "type_text",
            "key_press",
            "key_down",
            "key_up",
            "hold",
            "wait",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        screenshot_formats: vec!["image/png".into()],
        accessibility: true,
        windows: true,
        clipboard: true,
        files: scoped_roots_available(&state.config),
        process: scoped_roots_available(&state.config),
        viewer: true,
        limits: ProtocolLimits::default(),
    }))
}

async fn accessibility(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
) -> Result<Json<AccessibilitySnapshot>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let driver = Arc::clone(&state.driver);
    let snapshot = tokio::time::timeout(
        Duration::from_secs(7),
        tokio::task::spawn_blocking(move || driver.accessibility_snapshot()),
    )
    .await
    .map_err(|_| {
        ApiFailure::new(
            StatusCode::REQUEST_TIMEOUT,
            ErrorCode::DeadlineExceeded,
            "accessibility snapshot exceeded its deadline",
        )
    })?
    .map_err(|_| ApiFailure::internal("accessibility worker failed"))?
    .map_err(|_| {
        ApiFailure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::CapabilityUnavailable,
            "accessibility snapshot is temporarily unavailable",
        )
    })?;
    Ok(Json(snapshot))
}

async fn windows(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
) -> Result<Json<WindowList>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let driver = Arc::clone(&state.driver);
    let windows = tokio::task::spawn_blocking(move || driver.windows())
        .await
        .map_err(|_| ApiFailure::internal("window worker failed"))?
        .map_err(|_| ApiFailure::internal("window inspection failed"))?;
    Ok(Json(WindowList { protocol: PROTOCOL_VERSION, windows }))
}

async fn focus_window(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<WindowFocusRequest>,
) -> Result<Json<WindowList>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_protocol_value(request.protocol)?;
    if request.window_id.len() > 32 || request.window_id.is_empty() {
        return Err(ApiFailure::invalid("invalid window ID"));
    }
    let guard = state.action_lock.lock().await;
    let driver = Arc::clone(&state.driver);
    let window_id = request.window_id;
    tokio::task::spawn_blocking(move || driver.focus_window(&window_id))
        .await
        .map_err(|_| ApiFailure::internal("window focus worker failed"))?
        .map_err(|_| ApiFailure::invalid("window could not be focused"))?;
    state.input_generation.fetch_add(1, Ordering::SeqCst);
    drop(guard);
    windows(State(state), headers).await
}

async fn read_clipboard(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
) -> Result<Json<ClipboardContent>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let driver = Arc::clone(&state.driver);
    let text = tokio::task::spawn_blocking(move || driver.read_clipboard(MAX_REQUEST_BODY_BYTES))
        .await
        .map_err(|_| ApiFailure::internal("clipboard worker failed"))?
        .map_err(|_| {
            ApiFailure::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::CapabilityUnavailable,
                "clipboard does not contain bounded UTF-8 text",
            )
        })?;
    Ok(Json(ClipboardContent { protocol: PROTOCOL_VERSION, text }))
}

async fn write_clipboard(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ClipboardContent>,
) -> Result<Json<ClipboardContent>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_protocol_value(request.protocol)?;
    if request.text.len() > MAX_REQUEST_BODY_BYTES {
        return Err(ApiFailure::invalid("clipboard text exceeds the size limit"));
    }
    let _guard = state.action_lock.lock().await;
    let driver = Arc::clone(&state.driver);
    let text = request.text.clone();
    tokio::task::spawn_blocking(move || driver.write_clipboard(&text))
        .await
        .map_err(|_| ApiFailure::internal("clipboard worker failed"))?
        .map_err(|_| ApiFailure::internal("clipboard write failed"))?;
    Ok(Json(request))
}

async fn launch(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<LaunchRequest>,
) -> Result<Json<LaunchResult>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_protocol_value(request.protocol)?;
    validate_argv(&request.argv)?;
    let driver = Arc::clone(&state.driver);
    let argv = request.argv;
    let pid = tokio::task::spawn_blocking(move || driver.launch(&argv))
        .await
        .map_err(|_| ApiFailure::internal("launch worker failed"))?
        .map_err(|_| {
            ApiFailure::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::CapabilityUnavailable,
                "application could not be launched",
            )
        })?;
    Ok(Json(LaunchResult { protocol: PROTOCOL_VERSION, pid }))
}

async fn spawn_process(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ProcessSpawnRequest>,
) -> Result<Json<ProcessInfo>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_protocol_value(request.protocol)?;
    validate_argv(&request.argv)?;
    let (program, args) = request.argv.split_first().expect("validated non-empty argv");
    let cwd = configured_scope_root(&state.config, request.cwd.unwrap_or(FileScope::Workspace))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(state.config.process_environment.iter().cloned())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut store = state.processes.lock().await;
    if store.entries.len() >= MAX_PROCESSES {
        let removable = store
            .entries
            .iter_mut()
            .find_map(|(id, process)| process.child.try_wait().ok().flatten().map(|_| id.clone()));
        if let Some(id) = removable {
            store.entries.remove(&id);
        } else {
            return Err(ApiFailure::new(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::Conflict,
                "the managed process limit is reached",
            ));
        }
    }
    let mut child = command.spawn().map_err(|_| {
        ApiFailure::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::CapabilityUnavailable,
            "process could not be started",
        )
    })?;
    let output = Arc::new(StdMutex::new(BoundedOutput::default()));
    if let Some(stdout) = child.stdout.take() {
        spawn_output_drain(stdout, Arc::clone(&output));
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_drain(stderr, Arc::clone(&output));
    }
    let process_id = format!("proc-{}", Uuid::new_v4());
    let started_at_ms = now_ms();
    let pid = child.id();
    store.entries.insert(
        process_id.clone(),
        ManagedProcess { child, output, started_at_ms, exit_code: None, finished: false },
    );
    Ok(Json(ProcessInfo {
        protocol: PROTOCOL_VERSION,
        process_id,
        pid,
        state: ProcessState::Running,
        exit_code: None,
        started_at_ms,
        output_truncated: false,
    }))
}

async fn process_status(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path(process_id): Path<String>,
) -> Result<Json<ProcessInfo>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_process_id(&process_id)?;
    let mut store = state.processes.lock().await;
    let process = store.entries.get_mut(&process_id).ok_or_else(|| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "process not found")
    })?;
    let info =
        process.info(process_id).map_err(|_| ApiFailure::internal("process status failed"))?;
    Ok(Json(info))
}

async fn process_output(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path(process_id): Path<String>,
) -> Result<Json<ProcessOutput>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_process_id(&process_id)?;
    let store = state.processes.lock().await;
    let process = store.entries.get(&process_id).ok_or_else(|| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "process not found")
    })?;
    let output =
        process.output.lock().map_err(|_| ApiFailure::internal("process output lock failed"))?;
    Ok(Json(ProcessOutput {
        protocol: PROTOCOL_VERSION,
        process_id,
        output: String::from_utf8_lossy(&output.bytes).into_owned(),
        truncated: output.truncated,
    }))
}

async fn kill_process(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path(process_id): Path<String>,
) -> Result<Json<ProcessInfo>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    validate_process_id(&process_id)?;
    let mut store = state.processes.lock().await;
    let process = store.entries.get_mut(&process_id).ok_or_else(|| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "process not found")
    })?;
    if !process.finished {
        kill_process_group(process.child.id());
        let status =
            process.child.wait().map_err(|_| ApiFailure::internal("process wait failed"))?;
        process.exit_code = status.code();
        process.finished = true;
    }
    Ok(Json(process.info(process_id).map_err(|_| ApiFailure::internal("process status failed"))?))
}

fn kill_process_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn spawn_output_drain(
    mut reader: impl Read + Send + 'static,
    output: Arc<StdMutex<BoundedOutput>>,
) {
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if let Ok(mut output) = output.lock() {
                        output.append(&buffer[..read]);
                    }
                }
            }
        }
    });
}

fn validate_process_id(process_id: &str) -> Result<(), ApiFailure> {
    if process_id.len() <= 64
        && process_id.starts_with("proc-")
        && process_id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        Ok(())
    } else {
        Err(ApiFailure::invalid("invalid process ID"))
    }
}

async fn read_file(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path((scope, path)): Path<(String, String)>,
) -> Result<Response, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let (scope, target) = resolve_scoped_file(&state.config, &scope, &path, false)?;
    let metadata = fs::metadata(&target).map_err(|_| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "file not found")
    })?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(ApiFailure::invalid("file is not a bounded regular file"));
    }
    let bytes = fs::read(&target).map_err(|_| ApiFailure::internal("file read failed"))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response.headers_mut().insert(
        "x-vdesk-sha256",
        HeaderValue::from_str(&digest).map_err(|_| ApiFailure::internal("invalid file digest"))?,
    );
    response.headers_mut().insert(
        "x-vdesk-scope",
        HeaderValue::from_static(match scope {
            FileScope::Workspace => "workspace",
            FileScope::Downloads => "downloads",
        }),
    );
    Ok(response)
}

async fn write_file(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path((scope, path)): Path<(String, String)>,
    body: Body,
) -> Result<Json<FileMetadata>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let (scope, target) = resolve_scoped_file(&state.config, &scope, &path, true)?;
    let bytes = to_bytes(body, MAX_FILE_BYTES)
        .await
        .map_err(|_| ApiFailure::invalid("file exceeds the upload limit"))?;
    let temporary = target.with_extension(format!("vdesk-{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, &bytes).map_err(|_| ApiFailure::internal("file write failed"))?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(ApiFailure::internal(format!("file publish failed: {error}")));
    }
    Ok(Json(FileMetadata {
        protocol: PROTOCOL_VERSION,
        scope,
        path,
        byte_length: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(&bytes)),
    }))
}

async fn delete_file(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path((scope, path)): Path<(String, String)>,
) -> Result<StatusCode, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let (_, target) = resolve_scoped_file(&state.config, &scope, &path, false)?;
    let metadata = fs::symlink_metadata(&target).map_err(|_| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "file not found")
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ApiFailure::invalid("target is not a regular file"));
    }
    fs::remove_file(target).map_err(|_| ApiFailure::internal("file deletion failed"))?;
    Ok(StatusCode::NO_CONTENT)
}

fn resolve_scoped_file(
    config: &ServiceConfig,
    scope: &str,
    path: &str,
    create_parent: bool,
) -> Result<(FileScope, PathBuf), ApiFailure> {
    let scope = match scope {
        "workspace" => FileScope::Workspace,
        "downloads" => FileScope::Downloads,
        _ => return Err(ApiFailure::invalid("file scope must be workspace or downloads")),
    };
    let root = configured_scope_root(config, scope)?;
    let relative = FsPath::new(path);
    if path.is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ApiFailure::invalid("file path must be a normalized relative path"));
    }
    let root =
        fs::canonicalize(root).map_err(|_| ApiFailure::internal("file scope is unavailable"))?;
    let target = root.join(relative);
    let parent = target.parent().ok_or_else(|| ApiFailure::invalid("file has no parent"))?;
    if create_parent {
        fs::create_dir_all(parent)
            .map_err(|_| ApiFailure::internal("create file directory failed"))?;
    }
    let canonical_parent = fs::canonicalize(parent).map_err(|_| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "file directory not found")
    })?;
    if !canonical_parent.starts_with(&root) {
        return Err(ApiFailure::invalid("file path escapes its scope"));
    }
    let safe_target = canonical_parent
        .join(target.file_name().ok_or_else(|| ApiFailure::invalid("file path is empty"))?);
    if let Ok(metadata) = fs::symlink_metadata(&safe_target)
        && metadata.file_type().is_symlink()
    {
        return Err(ApiFailure::invalid("symbolic-link file targets are refused"));
    }
    Ok((scope, safe_target))
}

fn scoped_roots_available(config: &ServiceConfig) -> bool {
    config.workspace_root.is_some() && config.downloads_root.is_some()
}

fn configured_scope_root(config: &ServiceConfig, scope: FileScope) -> Result<&FsPath, ApiFailure> {
    let root = match scope {
        FileScope::Workspace => config.workspace_root.as_deref(),
        FileScope::Downloads => config.downloads_root.as_deref(),
    };
    root.ok_or_else(|| {
        ApiFailure::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::CapabilityUnavailable,
            "scoped files and processes are unavailable in this runtime",
        )
    })
}

fn validate_protocol_value(protocol: u16) -> Result<(), ApiFailure> {
    if protocol == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ApiFailure::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::UnsupportedProtocol,
            "unsupported protocol version",
        ))
    }
}

fn validate_argv(argv: &[String]) -> Result<(), ApiFailure> {
    if argv.is_empty()
        || argv.len() > MAX_ARGV_ITEMS
        || argv.iter().any(|value| value.is_empty() || value.contains('\0'))
        || argv.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES
    {
        return Err(ApiFailure::invalid("argv is empty, malformed, or exceeds its limit"));
    }
    Ok(())
}

async fn observe(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CaptureRequest>,
) -> Result<Json<Observation>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    request
        .validate(state.driver.geometry())
        .map_err(|error| ApiFailure::invalid(error.to_string()))?;
    capture_observation(&state, request).await.map(Json)
}

async fn image(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    Path(image_id): Path<String>,
) -> Result<Response, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    let entry = state.images.lock().await.get(&image_id).ok_or_else(|| {
        ApiFailure::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "image not found")
    })?;
    let mut response = Response::new(Body::from(entry.bytes.to_vec()));
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", entry.metadata.sha256)) {
        response.headers_mut().insert(ETAG, etag);
    }
    Ok(response)
}

async fn actions(
    State(state): State<Arc<ServiceState>>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<ActionBatchRequest>,
) -> Result<Json<ActionBatchResult>, ApiFailure> {
    authorize(&headers, &state.config.data_token)?;
    request
        .validate(state.driver.geometry())
        .map_err(|error| ApiFailure::invalid(error.to_string()))?;
    let request_hash: [u8; 32] = Sha256::digest(
        serde_json::to_vec(&request).map_err(|_| ApiFailure::internal("serialize request"))?,
    )
    .into();

    match state.results.lock().await.get(&request.request_id, &request_hash) {
        CacheLookup::Hit(result) => return Ok(Json(*result)),
        CacheLookup::Conflict => {
            return Err(ApiFailure::new(
                StatusCode::CONFLICT,
                ErrorCode::Conflict,
                "request_id was already used for a different action batch",
            ));
        }
        CacheLookup::Miss => {}
    }

    let _action_guard = state.action_lock.lock().await;
    match state.results.lock().await.get(&request.request_id, &request_hash) {
        CacheLookup::Hit(result) => return Ok(Json(*result)),
        CacheLookup::Conflict => {
            return Err(ApiFailure::new(
                StatusCode::CONFLICT,
                ErrorCode::Conflict,
                "request_id was already used for a different action batch",
            ));
        }
        CacheLookup::Miss => {}
    }

    let input_generation_before = state.input_generation.load(Ordering::SeqCst);
    if let Some(expected) = request.expected_input_generation
        && expected != input_generation_before
    {
        return Err(ApiFailure::new(
            StatusCode::CONFLICT,
            ErrorCode::StaleObservation,
            format!(
                "input generation changed: expected {expected}, current {input_generation_before}; observe again"
            ),
        ));
    }

    let started_at_ms = now_ms();
    let mut action_results = Vec::with_capacity(request.actions.len());
    let mut executed = 0;
    let mut failed = 0;
    let mut own_input_events = 0_u64;
    let mut stopped = false;

    for (index, action) in request.actions.iter().enumerate() {
        if stopped {
            let timestamp = now_ms();
            action_results.push(ActionResult {
                index,
                action: action.name().into(),
                status: ActionStatus::Skipped,
                delivery: DeliveryStatus::NotAttempted,
                effect: EffectStatus::Unknown,
                started_at_ms: timestamp,
                completed_at_ms: timestamp,
                error: None,
                observation: None,
                observation_error: None,
            });
            continue;
        }

        executed += 1;
        let action_started = now_ms();
        let driver = Arc::clone(&state.driver);
        let owned_action = action.clone();
        let execution = tokio::time::timeout(
            DRIVER_TIMEOUT,
            tokio::task::spawn_blocking(move || driver.execute(&owned_action)),
        )
        .await;
        let execution_error = match execution {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(_))) => Some(ApiFailure::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ErrorCode::Internal,
                format!("{} action could not be delivered", action.name()),
            )),
            Ok(Err(_)) => Some(ApiFailure::internal("desktop action worker failed")),
            Err(_) => Some(ApiFailure::new(
                StatusCode::REQUEST_TIMEOUT,
                ErrorCode::DeadlineExceeded,
                format!("{} action exceeded its deadline", action.name()),
            )),
        };

        if let Some(failure) = execution_error {
            failed += 1;
            let cleanup_driver = Arc::clone(&state.driver);
            let _ = tokio::task::spawn_blocking(move || cleanup_driver.release_held_input()).await;
            action_results.push(ActionResult {
                index,
                action: action.name().into(),
                status: ActionStatus::Failed,
                delivery: DeliveryStatus::Refused,
                effect: EffectStatus::Unknown,
                started_at_ms: action_started,
                completed_at_ms: now_ms(),
                error: Some(failure.as_api_error()),
                observation: None,
                observation_error: None,
            });
            stopped = request.stop_on_error;
            continue;
        }

        if action_injects_input(action) {
            state.input_generation.fetch_add(1, Ordering::SeqCst);
            own_input_events += 1;
        }
        let (observation, observation_error) = if request.observe == ObserveMode::Each {
            match capture_observation(
                &state,
                CaptureRequest { protocol: PROTOCOL_VERSION, region: None, include_cursor: false },
            )
            .await
            {
                Ok(observation) => (Some(observation), None),
                Err(error) => (None, Some(error.as_api_error())),
            }
        } else {
            (None, None)
        };
        action_results.push(ActionResult {
            index,
            action: action.name().into(),
            status: ActionStatus::Completed,
            delivery: if action_injects_input(action) {
                DeliveryStatus::Attempted
            } else {
                DeliveryStatus::NotAttempted
            },
            effect: EffectStatus::Unverified,
            started_at_ms: action_started,
            completed_at_ms: now_ms(),
            error: None,
            observation,
            observation_error,
        });
    }

    let (final_observation, final_observation_error) = if request.observe == ObserveMode::Final {
        match capture_observation(
            &state,
            CaptureRequest { protocol: PROTOCOL_VERSION, region: None, include_cursor: false },
        )
        .await
        {
            Ok(observation) => (Some(observation), None),
            Err(error) => (None, Some(error.as_api_error())),
        }
    } else {
        (None, None)
    };
    let input_generation_after = state.input_generation.load(Ordering::SeqCst);
    let interference_detected =
        input_generation_after != input_generation_before.saturating_add(own_input_events);
    let status = if failed == 0 {
        BatchStatus::Completed
    } else if failed == executed {
        BatchStatus::Failed
    } else {
        BatchStatus::Partial
    };
    let result = ActionBatchResult {
        protocol: PROTOCOL_VERSION,
        request_id: request.request_id.clone(),
        status,
        requested: request.actions.len(),
        executed,
        failed,
        input_generation_before,
        input_generation_after,
        interference_detected,
        started_at_ms,
        completed_at_ms: now_ms(),
        actions: action_results,
        final_observation,
        final_observation_error,
    };
    state.results.lock().await.insert(CachedResult {
        request_id: request.request_id,
        request_hash,
        result: result.clone(),
    });
    Ok(Json(result))
}

async fn capture_observation(
    state: &Arc<ServiceState>,
    request: CaptureRequest,
) -> Result<Observation, ApiFailure> {
    let driver = Arc::clone(&state.driver);
    let capture = tokio::time::timeout(
        DRIVER_TIMEOUT,
        tokio::task::spawn_blocking(move || driver.capture(&request)),
    )
    .await
    .map_err(|_| {
        ApiFailure::new(
            StatusCode::REQUEST_TIMEOUT,
            ErrorCode::DeadlineExceeded,
            "desktop capture exceeded its deadline",
        )
    })?
    .map_err(|_| ApiFailure::internal("desktop capture worker failed"))?
    .map_err(|_| ApiFailure::internal("desktop capture failed"))?;
    store_capture(state, capture).await
}

async fn store_capture(
    state: &Arc<ServiceState>,
    capture: DriverCapture,
) -> Result<Observation, ApiFailure> {
    let observation_id = state.observation_id.fetch_add(1, Ordering::SeqCst) + 1;
    let image_id = format!("img-{}-{observation_id}", state.config.runtime_generation);
    let digest = hex::encode(Sha256::digest(&capture.png));
    let metadata = ImageResource {
        id: image_id.clone(),
        content_type: "image/png".into(),
        byte_length: capture.png.len() as u64,
        sha256: digest,
        width: capture.width,
        height: capture.height,
        coordinate_space: CoordinateSpace::Desktop { display_id: "primary".into() },
        cursor_included: capture.cursor_included,
        href: format!("/v1/images/{image_id}"),
    };
    state
        .images
        .lock()
        .await
        .insert(ImageEntry { metadata: metadata.clone(), bytes: capture.png.into() })?;
    Ok(Observation {
        protocol: PROTOCOL_VERSION,
        session_id: state.config.session_id.clone(),
        runtime_generation: state.config.runtime_generation,
        observation_id,
        input_generation: state.input_generation.load(Ordering::SeqCst),
        captured_at_ms: now_ms(),
        geometry: state.driver.geometry(),
        image: metadata,
        cursor: capture.cursor,
        active_window: None,
        accessibility: AccessibilityStatus::Degraded {
            reason: "bounded AT-SPI data is available from /v1/accessibility".into(),
        },
    })
}

fn authorize(headers: &HeaderMap, expected: &Secret) -> Result<(), ApiFailure> {
    let Some(value) = headers.get(AUTHORIZATION).and_then(|value| value.to_str().ok()) else {
        return Err(ApiFailure::unauthorized());
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return Err(ApiFailure::unauthorized());
    };
    let left = token.as_bytes();
    let right = expected.expose().as_bytes();
    if left.len() == right.len() && bool::from(left.ct_eq(right)) {
        Ok(())
    } else {
        Err(ApiFailure::unauthorized())
    }
}

fn action_injects_input(action: &Action) -> bool {
    !matches!(action, Action::Wait { .. })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;
    use vdesk_protocol::{Geometry, MouseButton};

    use super::*;

    struct FakeDriver {
        executions: AtomicUsize,
    }

    impl DesktopDriver for FakeDriver {
        fn geometry(&self) -> vdesk_protocol::Geometry {
            Geometry { width: 320, height: 240, dpi: 96, scale: 1 }
        }

        fn capture(&self, _request: &CaptureRequest) -> anyhow::Result<DriverCapture> {
            Ok(DriverCapture {
                png: b"fake-png".to_vec(),
                width: 320,
                height: 240,
                cursor: Some(vdesk_protocol::Point { x: 0, y: 0 }),
                cursor_included: false,
            })
        }

        fn execute(&self, _action: &Action) -> anyhow::Result<()> {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn release_held_input(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn test_config() -> ServiceConfig {
        ServiceConfig {
            session_id: "test-session".into(),
            runtime_generation: 1,
            data_token: Secret::parse("data-token-1234567890".into()).unwrap(),
            viewer_token: Secret::parse("viewer-token-123456".into()).unwrap(),
            rfb_addr: "127.0.0.1:5900".parse().unwrap(),
            human_input_socket: None,
            workspace_root: Some(PathBuf::from("/workspace")),
            downloads_root: Some(PathBuf::from("/downloads")),
            process_environment: Vec::new(),
        }
    }

    fn bearer() -> &'static str {
        "Bearer data-token-1234567890"
    }

    #[tokio::test]
    async fn health_requires_authentication() {
        let app = router(test_config(), Arc::new(FakeDriver { executions: AtomicUsize::new(0) }));
        let response = app
            .oneshot(Request::builder().uri("/v1/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn scoped_capabilities_are_explicitly_optional() {
        let mut config = test_config();
        config.workspace_root = None;
        config.downloads_root = None;
        let app = router(config, Arc::new(FakeDriver { executions: AtomicUsize::new(0) }));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/capabilities")
                    .header(AUTHORIZATION, bearer())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let capabilities: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(capabilities["files"], false);
        assert_eq!(capabilities["process"], false);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/processes")
                    .header(AUTHORIZATION, bearer())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"protocol":1,"argv":["/bin/true"],"cwd":null}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let envelope: ErrorEnvelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(envelope.error.code, ErrorCode::CapabilityUnavailable);
    }

    #[tokio::test]
    async fn malformed_json_uses_the_protocol_error_envelope() {
        let app = router(test_config(), Arc::new(FakeDriver { executions: AtomicUsize::new(0) }));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/observations")
                    .header(AUTHORIZATION, bearer())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"protocol":1,"surprise":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let envelope: ErrorEnvelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(envelope.error.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn observation_metadata_and_bytes_are_correlated() {
        let app = router(test_config(), Arc::new(FakeDriver { executions: AtomicUsize::new(0) }));
        let capture = serde_json::to_vec(&CaptureRequest {
            protocol: PROTOCOL_VERSION,
            region: None,
            include_cursor: false,
        })
        .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/observations")
                    .header(AUTHORIZATION, bearer())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(capture))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let observation: Observation = serde_json::from_slice(&body).unwrap();
        assert_eq!(observation.image.byte_length, 8);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(observation.image.href)
                    .header(AUTHORIZATION, bearer())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(&body[..], b"fake-png");
    }

    #[tokio::test]
    async fn action_request_is_idempotent_and_stale_aware() {
        let driver = Arc::new(FakeDriver { executions: AtomicUsize::new(0) });
        let app = router(test_config(), driver.clone());
        let request = ActionBatchRequest {
            protocol: PROTOCOL_VERSION,
            request_id: "req-1".into(),
            expected_input_generation: Some(0),
            actions: vec![Action::Click { x: 0, y: 0, button: MouseButton::Left, count: 1 }],
            stop_on_error: true,
            observe: ObserveMode::None,
        };
        let body = serde_json::to_vec(&request).unwrap();
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/actions")
                        .header(AUTHORIZATION, bearer())
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(driver.executions.load(Ordering::SeqCst), 1);

        let stale = ActionBatchRequest { request_id: "req-2".into(), ..request };
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/actions")
                    .header(AUTHORIZATION, bearer())
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&stale).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
