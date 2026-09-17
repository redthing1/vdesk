use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;
use vdesk_protocol::Geometry;

pub const DESCRIPTOR_VERSION: u16 = 1;
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).context("generate session credential")?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn parse(value: String) -> Result<Self> {
        if value.len() < 16 || value.len() > 256 || !value.is_ascii() {
            bail!("credential must be 16-256 ASCII characters");
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Serialize for Secret {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Podman,
    Docker,
}

impl fmt::Display for EngineKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDescriptor {
    pub descriptor_version: u16,
    pub session_id: Uuid,
    pub name: String,
    pub engine: EngineKind,
    pub container_name: String,
    pub network_name: String,
    pub image: String,
    pub host_endpoint: String,
    pub container_endpoint: String,
    pub data_token: Secret,
    pub viewer_token: Secret,
    pub geometry: Geometry,
    pub runtime_generation: u64,
    #[serde(default)]
    pub gpu: bool,
    pub offline: bool,
    pub workspace: Option<PathBuf>,
    pub created_at_ms: u64,
}

impl SessionDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        engine: EngineKind,
        container_name: String,
        network_name: String,
        image: String,
        host_endpoint: String,
        container_endpoint: String,
        geometry: Geometry,
        created_at_ms: u64,
    ) -> Result<Self> {
        validate_session_name(&name)?;
        geometry.validate().context("validate session geometry")?;
        Ok(Self {
            descriptor_version: DESCRIPTOR_VERSION,
            session_id: Uuid::new_v4(),
            name,
            engine,
            container_name,
            network_name,
            image,
            host_endpoint,
            container_endpoint,
            data_token: Secret::generate()?,
            viewer_token: Secret::generate()?,
            geometry,
            runtime_generation: 1,
            gpu: false,
            offline: false,
            workspace: None,
            created_at_ms,
        })
    }

    pub fn public(&self) -> PublicSessionDescriptor<'_> {
        PublicSessionDescriptor {
            descriptor_version: self.descriptor_version,
            session_id: self.session_id,
            name: &self.name,
            engine: self.engine,
            container_name: &self.container_name,
            network_name: &self.network_name,
            image: &self.image,
            host_endpoint: &self.host_endpoint,
            geometry: self.geometry,
            runtime_generation: self.runtime_generation,
            gpu: self.gpu,
            offline: self.offline,
            workspace: self.workspace.as_deref(),
            created_at_ms: self.created_at_ms,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PublicSessionDescriptor<'a> {
    pub descriptor_version: u16,
    pub session_id: Uuid,
    pub name: &'a str,
    pub engine: EngineKind,
    pub container_name: &'a str,
    pub network_name: &'a str,
    pub image: &'a str,
    pub host_endpoint: &'a str,
    pub geometry: Geometry,
    pub runtime_generation: u64,
    pub gpu: bool,
    pub offline: bool,
    pub workspace: Option<&'a Path>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientDescriptor {
    pub descriptor_version: u16,
    pub session_id: Uuid,
    pub name: String,
    pub endpoint: String,
    pub data_token: Secret,
    pub geometry: Geometry,
    pub runtime_generation: u64,
}

impl SessionDescriptor {
    pub fn client_descriptor(&self, inside_engine_network: bool) -> ClientDescriptor {
        ClientDescriptor {
            descriptor_version: self.descriptor_version,
            session_id: self.session_id,
            name: self.name.clone(),
            endpoint: if inside_engine_network {
                self.container_endpoint.clone()
            } else {
                self.host_endpoint.clone()
            },
            data_token: self.data_token.clone(),
            geometry: self.geometry,
            runtime_generation: self.runtime_generation,
        }
    }

    pub fn rotate_runtime(&mut self) -> Result<()> {
        self.runtime_generation =
            self.runtime_generation.checked_add(1).context("runtime generation is exhausted")?;
        self.data_token = Secret::generate()?;
        self.viewer_token = Secret::generate()?;
        self.host_endpoint = "pending".into();
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LocalRuntimeStore {
    root: PathBuf,
}

impl LocalRuntimeStore {
    pub fn discover() -> Result<Self> {
        if let Some(explicit) = std::env::var_os("VDESK_RUNTIME_DIR") {
            return Self::new(PathBuf::from(explicit));
        }
        if let Some(xdg_runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            return Self::new(PathBuf::from(xdg_runtime).join("vdesk"));
        }
        if let Some(xdg_state) = std::env::var_os("XDG_STATE_HOME") {
            return Self::new(PathBuf::from(xdg_state).join("vdesk/runtime"));
        }
        let Some(home) = std::env::var_os("HOME") else {
            bail!("cannot locate runtime directory: set VDESK_RUNTIME_DIR or HOME");
        };
        Self::new(PathBuf::from(home).join(".local/state/vdesk/runtime"))
    }

    pub fn new(root: PathBuf) -> Result<Self> {
        if root.as_os_str().is_empty() {
            bail!("runtime directory must not be empty");
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn client_path(&self) -> PathBuf {
        self.root.join("client.json")
    }

    pub fn ensure(&self) -> Result<()> {
        ensure_private_directory(&self.root)
    }

    pub fn save(&self, descriptor: &ClientDescriptor) -> Result<PathBuf> {
        validate_session_name(&descriptor.name)?;
        descriptor.geometry.validate().context("validate descriptor geometry")?;
        self.ensure()?;
        let target = self.client_path();
        refuse_symlink(&target)?;
        let bytes = serde_json::to_vec_pretty(descriptor).context("serialize local descriptor")?;
        if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES {
            bail!("local descriptor exceeds size limit");
        }
        write_atomic_private(&target, &bytes)?;
        Ok(target)
    }

    pub fn load(&self) -> Result<Option<ClientDescriptor>> {
        let target = self.client_path();
        if !target.exists() {
            return Ok(None);
        }
        load_client_descriptor(&target).map(Some)
    }

    pub fn remove(&self, session_id: Uuid) -> Result<()> {
        let target = self.client_path();
        let Some(descriptor) = self.load()? else {
            return Ok(());
        };
        if descriptor.session_id != session_id {
            return Ok(());
        }
        refuse_symlink(&target)?;
        fs::remove_file(&target).with_context(|| format!("remove {}", target.display()))
    }
}

impl StateStore {
    pub fn discover() -> Result<Self> {
        if let Some(explicit) = std::env::var_os("VDESK_STATE_DIR") {
            return Self::new(PathBuf::from(explicit));
        }
        if let Some(xdg_state) = std::env::var_os("XDG_STATE_HOME") {
            return Self::new(PathBuf::from(xdg_state).join("vdesk"));
        }
        let Some(home) = std::env::var_os("HOME") else {
            bail!("cannot locate state directory: set VDESK_STATE_DIR or HOME");
        };
        Self::new(PathBuf::from(home).join(".local/state/vdesk"))
    }

    pub fn new(root: PathBuf) -> Result<Self> {
        if root.as_os_str().is_empty() {
            bail!("state directory must not be empty");
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure(&self) -> Result<()> {
        ensure_private_directory(&self.root)
    }

    pub fn save(&self, descriptor: &SessionDescriptor) -> Result<PathBuf> {
        validate_session_name(&descriptor.name)?;
        self.ensure()?;
        let sessions = self.root.join("sessions");
        ensure_private_directory(&sessions)?;
        let target = sessions.join(format!("{}.json", descriptor.name));
        refuse_symlink(&target)?;

        let temporary = sessions.join(format!(".{}.{}.tmp", descriptor.name, Uuid::new_v4()));
        let bytes =
            serde_json::to_vec_pretty(descriptor).context("serialize session descriptor")?;
        if bytes.len() as u64 > MAX_DESCRIPTOR_BYTES {
            bail!("session descriptor exceeds size limit");
        }

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file =
            options.open(&temporary).with_context(|| format!("create {}", temporary.display()))?;
        if let Err(error) = write_and_sync(&mut file, &bytes) {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("write session descriptor");
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("publish {}", target.display()));
        }
        Ok(target)
    }

    pub fn load(&self, name: &str) -> Result<SessionDescriptor> {
        self.load_optional(name)?.with_context(|| format!("vdesk session '{name}' does not exist"))
    }

    pub fn load_optional(&self, name: &str) -> Result<Option<SessionDescriptor>> {
        validate_session_name(name)?;
        let target = self.root.join("sessions").join(format!("{name}.json"));
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("refusing symbolic link at {}", target.display())
            }
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read metadata for {}", target.display()));
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_DESCRIPTOR_BYTES {
            bail!("session descriptor is not a bounded regular file");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&target)
            .with_context(|| format!("open {}", target.display()))?
            .take(MAX_DESCRIPTOR_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("read session descriptor")?;
        let descriptor: SessionDescriptor =
            serde_json::from_slice(&bytes).context("parse session descriptor")?;
        if descriptor.descriptor_version != DESCRIPTOR_VERSION {
            bail!("unsupported session descriptor version");
        }
        if descriptor.name != name {
            bail!("session descriptor name does not match its filename");
        }
        Ok(Some(descriptor))
    }

    pub fn list(&self) -> Result<Vec<String>> {
        let sessions = self.root.join("sessions");
        if !sessions.exists() {
            return Ok(Vec::new());
        }
        refuse_symlink(&sessions)?;
        let mut names = Vec::new();
        for entry in fs::read_dir(&sessions).context("list session descriptors")? {
            let entry = entry.context("read session directory entry")?;
            let file_type = entry.file_type().context("inspect session directory entry")?;
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|value| value.to_str())
                && validate_session_name(name).is_ok()
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        validate_session_name(name)?;
        let target = self.root.join("sessions").join(format!("{name}.json"));
        refuse_symlink(&target)?;
        match fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", target.display())),
        }
    }

    pub fn save_client_descriptor(&self, descriptor: &ClientDescriptor) -> Result<PathBuf> {
        validate_session_name(&descriptor.name)?;
        self.ensure()?;
        let clients = self.root.join("clients");
        ensure_private_directory(&clients)?;
        let target = clients.join(format!("{}.json", descriptor.name));
        refuse_symlink(&target)?;
        let bytes = serde_json::to_vec_pretty(descriptor).context("serialize client descriptor")?;
        let temporary = clients.join(format!(".{}.{}.tmp", descriptor.name, Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The private 0700 parent protects this on the host. The file itself is mounted
            // read-only into agent containers whose UID is intentionally unconstrained.
            options.mode(0o644);
        }
        let mut file = options.open(&temporary).context("create client descriptor")?;
        if let Err(error) = write_and_sync(&mut file, &bytes) {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("write client descriptor");
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("publish client descriptor");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
                .context("make mounted client descriptor readable")?;
        }
        Ok(target)
    }

    pub fn remove_client_descriptor(&self, name: &str) -> Result<()> {
        validate_session_name(name)?;
        let target = self.root.join("clients").join(format!("{name}.json"));
        refuse_symlink(&target)?;
        match fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", target.display())),
        }
    }
}

pub fn load_client_descriptor(path: &Path) -> Result<ClientDescriptor> {
    refuse_symlink(path)?;
    let metadata = fs::metadata(path).with_context(|| format!("read {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_DESCRIPTOR_BYTES {
        bail!("client descriptor is not a bounded regular file");
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let descriptor: ClientDescriptor =
        serde_json::from_slice(&bytes).context("parse client descriptor")?;
    if descriptor.descriptor_version != DESCRIPTOR_VERSION {
        bail!("unsupported client descriptor version");
    }
    validate_session_name(&descriptor.name)?;
    descriptor.geometry.validate().context("validate descriptor geometry")?;
    Ok(descriptor)
}

pub fn validate_session_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("session name must be 1-64 ASCII letters, numbers, '.', '_' or '-'");
    }
    Ok(())
}

fn write_and_sync(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    file.write_all(bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn write_atomic_private(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target.parent().context("descriptor has no parent directory")?;
    let temporary = parent.join(format!(".client.{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file =
        options.open(&temporary).with_context(|| format!("create {}", temporary.display()))?;
    if let Err(error) = write_and_sync(&mut file, bytes) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("write local descriptor");
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("publish {}", target.display()));
    }
    Ok(())
}

fn refuse_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing symbolic link at {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("state path is not a real directory: {}", path.display());
        }
    } else {
        fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn descriptor() -> SessionDescriptor {
        SessionDescriptor::new(
            "work".into(),
            EngineKind::Podman,
            "vdesk-work".into(),
            "vdesk-work-net".into(),
            "localhost/vdesk:dev".into(),
            "http://127.0.0.1:12345".into(),
            "http://10.0.0.2:7777".into(),
            Geometry { width: 1280, height: 800, dpi: 96, scale: 1 },
            1,
        )
        .unwrap()
    }

    fn local_descriptor() -> ClientDescriptor {
        descriptor().client_descriptor(false)
    }

    #[test]
    fn session_names_are_narrow() {
        for valid in ["default", "my-project", "one.two", "A_1"] {
            validate_session_name(valid).unwrap();
        }
        for invalid in ["", ".", "..", "with space", "../escape", "slash/name"] {
            assert!(validate_session_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn secrets_are_redacted_from_debug_and_public_json() {
        let descriptor = descriptor();
        let debug = format!("{descriptor:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(descriptor.data_token.expose()));

        let public = serde_json::to_string(&descriptor.public()).unwrap();
        assert!(!public.contains(descriptor.data_token.expose()));
        assert!(!public.contains(descriptor.viewer_token.expose()));
    }

    #[test]
    fn descriptors_round_trip_and_list() {
        let temporary = tempdir().unwrap();
        let store = StateStore::new(temporary.path().join("state")).unwrap();
        let descriptor = descriptor();
        let path = store.save(&descriptor).unwrap();
        let loaded = store.load("work").unwrap();
        assert_eq!(loaded.session_id, descriptor.session_id);
        assert_eq!(loaded.data_token, descriptor.data_token);
        assert_eq!(store.list().unwrap(), vec!["work"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        store.remove("work").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.load_optional("work").unwrap().is_none());
    }

    #[test]
    fn invalid_descriptors_are_not_treated_as_absent() {
        let temporary = tempdir().unwrap();
        let store = StateStore::new(temporary.path().join("state")).unwrap();
        store.ensure().unwrap();
        fs::create_dir_all(store.root().join("sessions")).unwrap();
        fs::write(store.root().join("sessions/work.json"), "not JSON").unwrap();
        assert!(store.load_optional("work").is_err());
    }

    #[test]
    fn descriptors_without_gpu_state_remain_software_sessions() {
        let descriptor = descriptor();
        let mut json = serde_json::to_value(descriptor).unwrap();
        json.as_object_mut().unwrap().remove("gpu");
        let loaded: SessionDescriptor = serde_json::from_value(json).unwrap();
        assert!(!loaded.gpu);
    }

    #[test]
    fn client_descriptor_contains_data_authority_only() {
        let temporary = tempdir().unwrap();
        let store = StateStore::new(temporary.path().join("state")).unwrap();
        let session = descriptor();
        let client = session.client_descriptor(true);
        let path = store.save_client_descriptor(&client).unwrap();
        let bytes = fs::read_to_string(&path).unwrap();
        assert!(bytes.contains(session.data_token.expose()));
        assert!(!bytes.contains(session.viewer_token.expose()));
        assert_eq!(load_client_descriptor(&path).unwrap().endpoint, session.container_endpoint);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o644);
        }
    }

    #[test]
    fn local_runtime_descriptor_is_private_and_generation_safe() {
        let temporary = tempdir().unwrap();
        let store = LocalRuntimeStore::new(temporary.path().join("runtime")).unwrap();
        let descriptor = local_descriptor();
        let path = store.save(&descriptor).unwrap();

        assert_eq!(store.load().unwrap().unwrap().session_id, descriptor.session_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(store.root()).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        store.remove(Uuid::new_v4()).unwrap();
        assert!(store.load().unwrap().is_some());
        store.remove(descriptor.session_id).unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn local_runtime_descriptor_symlinks_are_refused() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let store = LocalRuntimeStore::new(temporary.path().join("runtime")).unwrap();
        store.ensure().unwrap();
        let victim = temporary.path().join("victim");
        fs::write(&victim, "do not touch").unwrap();
        symlink(&victim, store.client_path()).unwrap();

        assert!(store.save(&local_descriptor()).is_err());
        assert!(store.load().is_err());
        assert_eq!(fs::read_to_string(victim).unwrap(), "do not touch");
    }

    #[test]
    fn rotating_a_runtime_preserves_identity_and_rotates_authority() {
        let mut session = descriptor();
        session.gpu = true;
        let session_id = session.session_id;
        let data_token = session.data_token.clone();
        let viewer_token = session.viewer_token.clone();

        session.rotate_runtime().unwrap();

        assert_eq!(session.session_id, session_id);
        assert_eq!(session.runtime_generation, 2);
        assert_eq!(session.host_endpoint, "pending");
        assert!(session.gpu);
        assert_ne!(session.data_token, data_token);
        assert_ne!(session.viewer_token, viewer_token);
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_symlinks_are_refused() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let store = StateStore::new(temporary.path().join("state")).unwrap();
        store.ensure().unwrap();
        fs::create_dir_all(store.root().join("sessions")).unwrap();
        let victim = temporary.path().join("victim");
        fs::write(&victim, "do not touch").unwrap();
        symlink(&victim, store.root().join("sessions/work.json")).unwrap();
        assert!(store.save(&descriptor()).is_err());
        assert_eq!(fs::read_to_string(victim).unwrap(), "do not touch");
    }
}
