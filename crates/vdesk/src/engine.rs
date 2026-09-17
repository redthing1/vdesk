use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use tokio::process::Command;

use crate::state::{EngineKind, SessionDescriptor};

const API_PORT: &str = "7777/tcp";
const MANAGED_LABEL: &str = "io.vdesk.managed";
const SESSION_LABEL: &str = "io.vdesk.session";

#[derive(Debug, Clone)]
pub struct Engine {
    kind: EngineKind,
    executable: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerInspection {
    pub exists: bool,
    pub running: bool,
    pub status: String,
    pub exit_code: Option<i64>,
    pub host_endpoint: Option<String>,
    pub container_ip: Option<String>,
    #[serde(skip_serializing)]
    managed_session: Option<String>,
}

#[derive(Debug, Clone)]
struct GpuAccess {
    render_node: PathBuf,
    group_id: u32,
}

impl Engine {
    pub fn discover(preferred: Option<EngineKind>) -> Result<Self> {
        let candidates: &[EngineKind] = match preferred {
            Some(EngineKind::Podman) => &[EngineKind::Podman],
            Some(EngineKind::Docker) => &[EngineKind::Docker],
            None => &[EngineKind::Podman, EngineKind::Docker],
        };
        for kind in candidates {
            let name = kind.to_string();
            if let Some(executable) = find_in_path(&name) {
                return Ok(Self { kind: *kind, executable });
            }
        }
        let wanted = preferred.map_or_else(|| "Podman or Docker".into(), |kind| kind.to_string());
        bail!("{wanted} was not found in PATH")
    }

    pub fn kind(&self) -> EngineKind {
        self.kind
    }

    pub async fn build(&self, tag: &str, repository: &Path, target: &str) -> Result<()> {
        let containerfile = repository.join("container/Containerfile");
        if !containerfile.is_file() {
            bail!("Containerfile not found at {}", containerfile.display());
        }
        self.checked([
            OsString::from("build"),
            OsString::from("--tag"),
            OsString::from(tag),
            OsString::from("--target"),
            OsString::from(target),
            OsString::from("--file"),
            containerfile.into_os_string(),
            repository.as_os_str().to_owned(),
        ])
        .await?;
        Ok(())
    }

    async fn create_network(&self, descriptor: &SessionDescriptor) -> Result<()> {
        let mut args = vec![OsString::from("network"), OsString::from("create")];
        if descriptor.offline {
            args.push(OsString::from("--internal"));
        }
        args.extend(label_args(descriptor));
        args.push(OsString::from(&descriptor.network_name));
        self.checked(args).await?;
        Ok(())
    }

    async fn ensure_network(&self, descriptor: &SessionDescriptor) -> Result<()> {
        if !self.network_exists(descriptor).await? {
            self.create_network(descriptor).await?;
        }
        Ok(())
    }

    pub async fn remove_network(&self, descriptor: &SessionDescriptor) -> Result<()> {
        if !self.network_exists(descriptor).await? {
            return Ok(());
        }
        let output = self.output(["network", "rm", &descriptor.network_name]).await?;
        if output.status.success() || stderr_is_missing(&output.stderr) {
            return Ok(());
        }
        bail!(
            "{} could not remove network {}: {}",
            self.kind,
            descriptor.network_name,
            concise(&output.stderr)
        )
    }

    async fn network_exists(&self, descriptor: &SessionDescriptor) -> Result<bool> {
        let output = self.output(["network", "inspect", &descriptor.network_name]).await?;
        if !output.status.success() {
            if stderr_is_missing(&output.stderr) {
                return Ok(false);
            }
            bail!(
                "{} could not inspect network {}: {}",
                self.kind,
                descriptor.network_name,
                concise(&output.stderr)
            );
        }
        let values: Vec<Value> = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("parse {} network inspect output", self.kind))?;
        let value = values.first().context("network inspect returned no records")?;
        let labels = value.get("labels").or_else(|| value.get("Labels"));
        let session_id = descriptor.session_id.to_string();
        if !labels_match(labels, &session_id) {
            bail!(
                "network {} does not have matching vdesk ownership labels",
                descriptor.network_name
            );
        }
        Ok(true)
    }

    pub async fn create_container(&self, descriptor: &SessionDescriptor) -> Result<()> {
        self.ensure_network(descriptor).await?;
        let mut args = vec![
            OsString::from("run"),
            OsString::from("--detach"),
            OsString::from("--name"),
            OsString::from(&descriptor.container_name),
        ];
        args.extend(label_args(descriptor));
        args.extend([
            OsString::from("--network"),
            OsString::from(&descriptor.network_name),
            OsString::from("--cap-drop=ALL"),
            OsString::from("--security-opt=no-new-privileges"),
            OsString::from("--pids-limit=512"),
            OsString::from("--memory=2g"),
            OsString::from("--shm-size=256m"),
            OsString::from("--publish"),
            OsString::from("127.0.0.1::7777"),
        ]);
        if descriptor.gpu {
            args.extend(self.gpu_args()?);
        }
        if let Some(workspace) = &descriptor.workspace {
            if self.kind == EngineKind::Podman {
                args.push(OsString::from("--userns=keep-id"));
            }
            let workspace = workspace.to_str().context("workspace path is not valid UTF-8")?;
            if workspace.contains(':') {
                bail!("workspace paths containing ':' are not supported by container engines");
            }
            args.extend([
                OsString::from("--volume"),
                OsString::from(format!("{workspace}:/workspace:rw")),
            ]);
        }
        args.extend(environment_args(descriptor));
        args.push(OsString::from(&descriptor.image));
        self.checked(args).await?;
        Ok(())
    }

    pub async fn start(&self, descriptor: &SessionDescriptor) -> Result<()> {
        self.inspect_session(descriptor).await?;
        self.checked(["start", &descriptor.container_name]).await?;
        Ok(())
    }

    pub async fn stop(&self, descriptor: &SessionDescriptor) -> Result<()> {
        self.inspect_session(descriptor).await?;
        let output = self.output(["stop", "--time", "10", &descriptor.container_name]).await?;
        if output.status.success() || stderr_is_missing(&output.stderr) {
            return Ok(());
        }
        bail!(
            "{} could not stop {}: {}",
            self.kind,
            descriptor.container_name,
            concise(&output.stderr)
        )
    }

    pub async fn remove_container(
        &self,
        descriptor: &SessionDescriptor,
        force: bool,
    ) -> Result<()> {
        let inspection = self.inspect_session(descriptor).await?;
        if !inspection.exists {
            return Ok(());
        }
        let mut args = vec![OsString::from("rm")];
        if force {
            args.push(OsString::from("--force"));
        }
        args.push(OsString::from(&descriptor.container_name));
        let output = self.output(args).await?;
        if output.status.success() || stderr_is_missing(&output.stderr) {
            return Ok(());
        }
        bail!(
            "{} could not remove {}: {}",
            self.kind,
            descriptor.container_name,
            concise(&output.stderr)
        )
    }

    pub async fn inspect(&self, name: &str) -> Result<ContainerInspection> {
        let output = self.output(["inspect", name]).await?;
        if !output.status.success() {
            if stderr_is_missing(&output.stderr) {
                return Ok(ContainerInspection {
                    exists: false,
                    running: false,
                    status: "absent".into(),
                    exit_code: None,
                    host_endpoint: None,
                    container_ip: None,
                    managed_session: None,
                });
            }
            bail!("{} could not inspect {name}: {}", self.kind, concise(&output.stderr));
        }
        let values: Vec<Value> = serde_json::from_slice(&output.stdout)
            .with_context(|| format!("parse {} inspect output", self.kind))?;
        let value = values.first().context("container inspect returned no records")?;
        let running = value.pointer("/State/Running").and_then(Value::as_bool).unwrap_or(false);
        let status =
            value.pointer("/State/Status").and_then(Value::as_str).unwrap_or("unknown").to_owned();
        let exit_code = value.pointer("/State/ExitCode").and_then(Value::as_i64);
        let host_endpoint = value
            .pointer("/NetworkSettings/Ports")
            .and_then(|ports| ports.get(API_PORT))
            .and_then(Value::as_array)
            .and_then(|bindings| bindings.first())
            .and_then(|binding| binding.get("HostPort"))
            .and_then(Value::as_str)
            .map(|port| format!("http://127.0.0.1:{port}"));
        let container_ip = value
            .pointer("/NetworkSettings/Networks")
            .and_then(Value::as_object)
            .and_then(|networks| {
                networks.values().find_map(|network| {
                    network
                        .get("IPAddress")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })
            })
            .map(str::to_owned);
        let managed_session = value
            .pointer("/Config/Labels")
            .filter(|labels| labels.get(MANAGED_LABEL).and_then(Value::as_str) == Some("true"))
            .and_then(|labels| labels.get(SESSION_LABEL))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(ContainerInspection {
            exists: true,
            running,
            status,
            exit_code,
            host_endpoint,
            container_ip,
            managed_session,
        })
    }

    pub async fn inspect_session(
        &self,
        descriptor: &SessionDescriptor,
    ) -> Result<ContainerInspection> {
        let inspection = self.inspect(&descriptor.container_name).await?;
        let session_id = descriptor.session_id.to_string();
        if inspection.exists && inspection.managed_session.as_deref() != Some(session_id.as_str()) {
            bail!(
                "container {} does not have matching vdesk ownership labels",
                descriptor.container_name
            );
        }
        Ok(inspection)
    }

    pub async fn logs(&self, name: &str, tail: usize) -> Result<String> {
        let output = self.output(["logs", "--tail", &tail.to_string(), name]).await?;
        if !output.status.success() && stderr_is_missing(&output.stderr) {
            return Ok(String::new());
        }
        let mut bytes = output.stdout;
        bytes.extend_from_slice(&output.stderr);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn copy_from_container(
        &self,
        container: &str,
        source: &str,
        destination: &Path,
    ) -> Result<()> {
        let parent = destination.parent().context("client destination has no parent")?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let source = format!("{container}:{source}");
        self.checked([
            OsString::from("cp"),
            OsString::from(source),
            destination.as_os_str().to_owned(),
        ])
        .await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
                .context("make extracted vdesk client executable")?;
        }
        Ok(())
    }

    pub async fn run_agent(
        &self,
        descriptor: &SessionDescriptor,
        agent_image: &str,
        client_binary: &Path,
        client_descriptor: &Path,
        command: &[String],
    ) -> Result<i32> {
        self.network_exists(descriptor).await?;
        let binary = utf8_path(client_binary)?;
        let descriptor_path = utf8_path(client_descriptor)?;
        let mut args = vec![
            OsString::from("run"),
            OsString::from("--rm"),
            OsString::from("--network"),
            OsString::from(&descriptor.network_name),
            OsString::from("--cap-drop=ALL"),
            OsString::from("--security-opt=no-new-privileges"),
            OsString::from("--volume"),
            OsString::from(format!("{binary}:/usr/local/bin/vdesk:ro")),
            OsString::from("--volume"),
            OsString::from(format!("{descriptor_path}:/run/vdesk/session.json:ro")),
            OsString::from("--env"),
            OsString::from("VDESK_DESCRIPTOR=/run/vdesk/session.json"),
            OsString::from(agent_image),
        ];
        args.extend(command.iter().map(OsString::from));
        let status = Command::new(&self.executable)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .with_context(|| format!("run agent with {}", self.kind))?;
        Ok(status.code().unwrap_or(125))
    }

    fn gpu_args(&self) -> Result<Vec<OsString>> {
        let access = discover_gpu_access()?;
        Ok(gpu_args_for(self.kind, &access, unsafe { libc::geteuid() } != 0))
    }

    async fn checked<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(args).await?;
        if !output.status.success() {
            bail!("{} command failed: {}", self.kind, concise(&output.stderr));
        }
        Ok(output.stdout)
    }

    async fn output<I, S>(&self, args: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(&self.executable)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("run {}", self.kind))
    }
}

fn gpu_args_for(engine: EngineKind, access: &GpuAccess, rootless: bool) -> Vec<OsString> {
    let mut args = vec![OsString::from("--device"), access.render_node.as_os_str().to_owned()];
    if engine == EngineKind::Podman && rootless {
        args.extend([OsString::from("--group-add"), OsString::from("keep-groups")]);
    } else {
        args.extend([OsString::from("--group-add"), OsString::from(access.group_id.to_string())]);
    }
    args
}

fn discover_gpu_access() -> Result<GpuAccess> {
    let entries = match fs::read_dir("/dev/dri") {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("no GPU render node was found")
        }
        Err(error) => return Err(error).context("read GPU devices from /dev/dri"),
    };
    let mut render_nodes = Vec::new();
    for entry in entries {
        let entry = entry.context("read GPU device entry")?;
        if is_render_node_name(&entry.file_name())
            && entry.file_type().context("inspect GPU device entry")?.is_char_device()
        {
            render_nodes.push(entry.path());
        }
    }
    render_nodes.sort();
    let render_node = render_nodes.into_iter().next().context("no GPU render node was found")?;
    let metadata = fs::metadata(&render_node)
        .with_context(|| format!("inspect GPU render node {}", render_node.display()))?;
    let name =
        render_node.file_name().and_then(OsStr::to_str).context("invalid render node name")?;
    let vendor =
        fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor")).unwrap_or_default();
    reject_broad_gpu_profiles(&vendor)?;
    Ok(GpuAccess { render_node, group_id: metadata.gid() })
}

fn reject_broad_gpu_profiles(vendor: &str) -> Result<()> {
    if vendor.trim().eq_ignore_ascii_case("0x10de") {
        bail!(
            "managed NVIDIA GPU access is not supported because its standard CDI profile grants broader device access; use embedded mode with an outer GPU policy"
        );
    }
    Ok(())
}

fn is_render_node_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.strip_prefix("renderD").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn label_args(descriptor: &SessionDescriptor) -> Vec<OsString> {
    vec![
        OsString::from("--label"),
        OsString::from(format!("{MANAGED_LABEL}=true")),
        OsString::from("--label"),
        OsString::from(format!("{SESSION_LABEL}={}", descriptor.session_id)),
    ]
}

fn labels_match(labels: Option<&Value>, session_id: &str) -> bool {
    labels.is_some_and(|labels| {
        labels.get(MANAGED_LABEL).and_then(Value::as_str) == Some("true")
            && labels.get(SESSION_LABEL).and_then(Value::as_str) == Some(session_id)
    })
}

fn environment_args(descriptor: &SessionDescriptor) -> Vec<OsString> {
    let size = format!("{}x{}", descriptor.geometry.width, descriptor.geometry.height);
    [
        ("VDESK_SESSION_ID", descriptor.session_id.to_string()),
        ("VDESK_RUNTIME_GENERATION", descriptor.runtime_generation.to_string()),
        ("VDESK_DATA_TOKEN", descriptor.data_token.expose().to_owned()),
        ("VDESK_VIEWER_TOKEN", descriptor.viewer_token.expose().to_owned()),
        ("VDESK_SIZE", size),
    ]
    .into_iter()
    .flat_map(|(name, value)| [OsString::from("--env"), OsString::from(format!("{name}={value}"))])
    .collect()
}

fn stderr_is_missing(bytes: &[u8]) -> bool {
    let value = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    value.contains("no such container")
        || value.contains("no such object")
        || value.contains("no container with name or id")
        || value.contains("network not found")
        || (value.contains("network ") && value.contains(" not found"))
        || value.contains("network does not exist")
}

fn concise(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().chars().take(2_000).collect()
}

fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn find_in_path(command: &str) -> Option<PathBuf> {
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search).map(|directory| directory.join(command)).find(|candidate| {
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_engine_messages_are_recognized_without_masking_other_errors() {
        assert!(stderr_is_missing(b"Error: no such container"));
        assert!(stderr_is_missing(b"Error: no such object"));
        assert!(stderr_is_missing(b"network does not exist"));
        assert!(stderr_is_missing(b"unable to find network: network not found"));
        assert!(stderr_is_missing(b"network vdesk-example not found"));
        assert!(!stderr_is_missing(b"helper executable not found"));
        assert!(!stderr_is_missing(b"permission denied"));
    }

    #[test]
    fn command_errors_are_bounded() {
        assert_eq!(concise(&vec![b'x'; 3_000]).len(), 2_000);
    }

    #[test]
    fn ownership_requires_both_exact_labels() {
        let labels = json!({
            (MANAGED_LABEL): "true",
            (SESSION_LABEL): "session-id",
        });
        assert!(labels_match(Some(&labels), "session-id"));
        assert!(!labels_match(Some(&labels), "other"));
        assert!(!labels_match(Some(&json!({ (SESSION_LABEL): "session-id" })), "session-id"));
        assert!(!labels_match(None, "session-id"));
    }

    #[test]
    fn render_node_names_are_strict() {
        assert!(is_render_node_name(OsStr::new("renderD128")));
        assert!(!is_render_node_name(OsStr::new("card0")));
        assert!(!is_render_node_name(OsStr::new("renderD")));
        assert!(!is_render_node_name(OsStr::new("renderD128-extra")));
    }

    #[test]
    fn managed_nvidia_is_rejected_at_the_device_boundary() {
        assert!(reject_broad_gpu_profiles("0x10de\n").is_err());
        assert!(reject_broad_gpu_profiles("0x8086\n").is_ok());
        assert!(reject_broad_gpu_profiles("").is_ok());
    }

    #[test]
    fn rootless_podman_preserves_the_callers_device_groups() {
        let access = GpuAccess { render_node: PathBuf::from("/dev/dri/renderD128"), group_id: 105 };
        assert_eq!(
            gpu_args_for(EngineKind::Podman, &access, true),
            ["--device", "/dev/dri/renderD128", "--group-add", "keep-groups"].map(OsString::from)
        );
    }

    #[test]
    fn docker_uses_the_render_nodes_numeric_group() {
        let access = GpuAccess { render_node: PathBuf::from("/dev/dri/renderD128"), group_id: 105 };
        assert_eq!(
            gpu_args_for(EngineKind::Docker, &access, true),
            ["--device", "/dev/dri/renderD128", "--group-add", "105"].map(OsString::from)
        );
    }
}
