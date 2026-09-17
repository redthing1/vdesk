use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use vdesk_protocol::{
    AccessibilitySnapshot, Action, ActionBatchRequest, ActionBatchResult, CapabilityReport,
    CaptureRequest, ClipboardContent, FileMetadata, FileScope, LaunchRequest, LaunchResult,
    Observation, ObserveMode, PROTOCOL_VERSION, ProcessInfo, ProcessOutput, ProcessSpawnRequest,
    WindowFocusRequest, WindowList,
};

use crate::state::{ClientDescriptor, Secret, SessionDescriptor};

const MAX_ERROR_BYTES: usize = 8 * 1024;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct DesktopClient {
    endpoint: String,
    token: Secret,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Health {
    pub protocol: u16,
    pub ready: bool,
    pub session_id: String,
    pub runtime_generation: u64,
}

impl DesktopClient {
    pub fn for_host(descriptor: &SessionDescriptor) -> Result<Self> {
        Self::new(&descriptor.host_endpoint, descriptor.data_token.clone())
    }

    pub fn from_client_descriptor(descriptor: &ClientDescriptor) -> Result<Self> {
        Self::new(&descriptor.endpoint, descriptor.data_token.clone())
    }

    pub fn new(endpoint: &str, token: Secret) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/').to_owned();
        if !valid_endpoint(&endpoint) {
            bail!("desktop endpoint must use HTTP on loopback or a private engine-network address");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(45))
            .build()
            .context("build desktop HTTP client")?;
        Ok(Self { endpoint, token, http })
    }

    pub async fn health(&self) -> Result<Health> {
        self.json(Method::GET, "/v2/health", Option::<&()>::None).await
    }

    pub async fn capabilities(&self) -> Result<CapabilityReport> {
        self.json(Method::GET, "/v2/capabilities", Option::<&()>::None).await
    }

    pub async fn accessibility(&self) -> Result<AccessibilitySnapshot> {
        self.json(Method::GET, "/v2/accessibility", Option::<&()>::None).await
    }

    pub async fn windows(&self) -> Result<WindowList> {
        self.json(Method::GET, "/v2/windows", Option::<&()>::None).await
    }

    pub async fn focus_window(&self, window_id: String) -> Result<WindowList> {
        self.json(
            Method::POST,
            "/v2/windows/focus",
            Some(&WindowFocusRequest { protocol: PROTOCOL_VERSION, window_id }),
        )
        .await
    }

    pub async fn read_clipboard(&self) -> Result<ClipboardContent> {
        self.json(Method::GET, "/v2/clipboard", Option::<&()>::None).await
    }

    pub async fn write_clipboard(&self, text: String) -> Result<ClipboardContent> {
        self.json(
            Method::PUT,
            "/v2/clipboard",
            Some(&ClipboardContent { protocol: PROTOCOL_VERSION, text }),
        )
        .await
    }

    pub async fn launch(&self, argv: Vec<String>) -> Result<LaunchResult> {
        self.json(
            Method::POST,
            "/v2/launch",
            Some(&LaunchRequest { protocol: PROTOCOL_VERSION, argv }),
        )
        .await
    }

    pub async fn spawn_process(
        &self,
        argv: Vec<String>,
        cwd: Option<FileScope>,
    ) -> Result<ProcessInfo> {
        self.json(
            Method::POST,
            "/v2/processes",
            Some(&ProcessSpawnRequest { protocol: PROTOCOL_VERSION, argv, cwd }),
        )
        .await
    }

    pub async fn process_status(&self, process_id: &str) -> Result<ProcessInfo> {
        self.json(
            Method::GET,
            &format!("/v2/processes/{}", encode_id(process_id)?),
            Option::<&()>::None,
        )
        .await
    }

    pub async fn process_output(&self, process_id: &str) -> Result<ProcessOutput> {
        self.json(
            Method::GET,
            &format!("/v2/processes/{}/output", encode_id(process_id)?),
            Option::<&()>::None,
        )
        .await
    }

    pub async fn kill_process(&self, process_id: &str) -> Result<ProcessInfo> {
        self.json(
            Method::DELETE,
            &format!("/v2/processes/{}", encode_id(process_id)?),
            Option::<&()>::None,
        )
        .await
    }

    pub async fn import_file(
        &self,
        scope: FileScope,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<FileMetadata> {
        if bytes.len() > MAX_IMAGE_BYTES {
            bail!("file exceeds the {MAX_IMAGE_BYTES}-byte client limit");
        }
        let response = self
            .http
            .put(self.file_url(scope, path)?)
            .bearer_auth(self.token.expose())
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .context("upload desktop file")?;
        ensure_success(response).await?.json().await.context("decode file metadata")
    }

    pub async fn export_file(&self, scope: FileScope, path: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(self.file_url(scope, path)?)
            .bearer_auth(self.token.expose())
            .send()
            .await
            .context("download desktop file")?;
        let response = ensure_success(response).await?;
        if response.content_length().is_some_and(|length| length > MAX_IMAGE_BYTES as u64) {
            bail!("file exceeds the {MAX_IMAGE_BYTES}-byte client limit");
        }
        let bytes = response.bytes().await.context("read desktop file")?;
        if bytes.len() > MAX_IMAGE_BYTES {
            bail!("file exceeds the {MAX_IMAGE_BYTES}-byte client limit");
        }
        Ok(bytes.to_vec())
    }

    pub async fn observe(&self, include_cursor: bool) -> Result<Observation> {
        self.json(
            Method::POST,
            "/v2/observations",
            Some(&CaptureRequest { protocol: PROTOCOL_VERSION, region: None, include_cursor }),
        )
        .await
    }

    pub async fn actions(
        &self,
        actions: Vec<Action>,
        expected_input_generation: Option<u64>,
        observe: ObserveMode,
    ) -> Result<ActionBatchResult> {
        let request = ActionBatchRequest {
            protocol: PROTOCOL_VERSION,
            request_id: format!("cli-{}", Uuid::new_v4()),
            expected_input_generation,
            actions,
            stop_on_error: true,
            observe,
        };
        self.action_batch(&request).await
    }

    pub async fn action_batch(&self, request: &ActionBatchRequest) -> Result<ActionBatchResult> {
        self.json(Method::POST, "/v2/actions", Some(request)).await
    }

    pub async fn image(&self, observation: &Observation) -> Result<Vec<u8>> {
        let href = &observation.image.href;
        if !href.starts_with("/v2/images/") || href.contains('?') || href.contains('#') {
            bail!("service returned an invalid image resource path");
        }
        let response = self
            .http
            .get(format!("{}{}", self.endpoint, href))
            .bearer_auth(self.token.expose())
            .send()
            .await
            .context("download desktop image")?;
        let response = ensure_success(response).await?;
        if response.content_length().is_some_and(|length| length > MAX_IMAGE_BYTES as u64) {
            bail!("desktop image exceeds the {MAX_IMAGE_BYTES}-byte client limit");
        }
        let bytes = response.bytes().await.context("read desktop image")?;
        if bytes.len() > MAX_IMAGE_BYTES {
            bail!("desktop image exceeds the {MAX_IMAGE_BYTES}-byte client limit");
        }
        Ok(bytes.to_vec())
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn file_url(&self, scope: FileScope, path: &str) -> Result<String> {
        let scope = match scope {
            FileScope::Workspace => "workspace",
            FileScope::Downloads => "downloads",
        };
        Ok(format!("{}/v2/files/{scope}/{}", self.endpoint, encode_relative_path(path)?))
    }

    async fn json<T, B>(&self, method: Method, path: &str, body: Option<&B>) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.endpoint, path))
            .bearer_auth(self.token.expose());
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.context("call desktop service")?;
        let response = ensure_success(response).await?;
        response.json().await.context("decode desktop service response")
    }
}

fn valid_endpoint(endpoint: &str) -> bool {
    if endpoint.starts_with("http://127.0.0.1:")
        || endpoint.starts_with("http://localhost:")
        || endpoint.starts_with("http://vdesk-")
    {
        return true;
    }
    endpoint
        .strip_prefix("http://")
        .and_then(|authority| authority.rsplit_once(':'))
        .and_then(|(address, port)| Some((address.parse::<std::net::Ipv4Addr>().ok()?, port)))
        .is_some_and(|(address, port)| {
            (address.is_private() || address.is_link_local()) && port.parse::<u16>().is_ok()
        })
}

fn encode_relative_path(path: &str) -> Result<String> {
    let parsed = Path::new(path);
    if path.is_empty()
        || parsed.is_absolute()
        || parsed
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("desktop file path must be a normalized relative path");
    }
    let mut output = String::new();
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(char::from(*byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(output)
}

fn encode_id(value: &str) -> Result<&str> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("resource ID is malformed");
    }
    Ok(value)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = path.file_name().context("output path must name a file")?;
    let temporary = parent.join(format!(".{}.{}.tmp", file_name.to_string_lossy(), Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).context("create temporary output")?;
        file.write_all(bytes).context("write temporary output")?;
        file.sync_all().context("sync temporary output")?;
        fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn client_descriptor_path(state_root: &Path, session: &str) -> PathBuf {
    state_root.join("clients").join(format!("{session}.json"))
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let bytes = response.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_ERROR_BYTES)]);
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
            value.pointer("/error/message").and_then(Value::as_str).map(str::to_owned)
        })
        .unwrap_or_else(|| text.trim().to_owned());
    if status == StatusCode::UNAUTHORIZED {
        bail!("desktop service rejected its session credential");
    }
    bail!("desktop service returned {status}: {message}")
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn endpoints_are_narrow() {
        let token = Secret::parse("abcdefghijklmnop".into()).unwrap();
        assert!(DesktopClient::new("http://127.0.0.1:7777", token.clone()).is_ok());
        assert!(DesktopClient::new("http://vdesk-abc:7777", token.clone()).is_ok());
        assert!(DesktopClient::new("http://10.89.1.2:7777", token.clone()).is_ok());
        assert!(DesktopClient::new("http://8.8.8.8:7777", token.clone()).is_err());
        assert!(DesktopClient::new("https://attacker.invalid", token).is_err());
    }

    #[test]
    fn atomic_output_replaces_whole_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("capture.png");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"two");
    }

    #[test]
    fn file_paths_are_normalized_and_encoded() {
        assert_eq!(encode_relative_path("folder/a b.txt").unwrap(), "folder/a%20b.txt");
        assert!(encode_relative_path("../escape").is_err());
        assert!(encode_relative_path("/absolute").is_err());
    }
}
