use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_ACTIONS_PER_BATCH: usize = 64;
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_KEYS_PER_CHORD: usize = 8;
pub const MAX_ACTION_DURATION_MS: u64 = 30_000;
pub const MAX_WAIT_MS: u64 = 30_000;
pub const MAX_REQUEST_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub dpi: u16,
    pub scale: u16,
}

impl Geometry {
    pub fn validate(self) -> Result<(), ValidationError> {
        if !(1..=8192).contains(&self.width) || !(1..=8192).contains(&self.height) {
            return Err(ValidationError::GeometryOutOfRange);
        }
        if !(36..=600).contains(&self.dpi) {
            return Err(ValidationError::DpiOutOfRange);
        }
        if !(1..=8).contains(&self.scale) {
            return Err(ValidationError::ScaleOutOfRange);
        }
        Ok(())
    }

    pub fn contains(self, point: Point) -> bool {
        point.x >= 0
            && point.y >= 0
            && (point.x as u32) < self.width
            && (point.y as u32) < self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn validate(self, geometry: Geometry) -> Result<(), ValidationError> {
        if self.width == 0 || self.height == 0 {
            return Err(ValidationError::EmptyRegion);
        }
        if !geometry.contains(Point { x: self.x, y: self.y }) {
            return Err(ValidationError::CoordinateOutOfBounds { x: self.x, y: self.y });
        }
        let right = i64::from(self.x) + i64::from(self.width);
        let bottom = i64::from(self.y) + i64::from(self.height);
        if right > i64::from(geometry.width) || bottom > i64::from(geometry.height) {
            return Err(ValidationError::RegionOutOfBounds);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    #[default]
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    None,
    #[default]
    Final,
    Each,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Move {
        x: i32,
        y: i32,
        #[serde(default)]
        duration_ms: u64,
    },
    Click {
        x: i32,
        y: i32,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "default_click_count")]
        count: u8,
    },
    MouseDown {
        #[serde(default)]
        button: MouseButton,
    },
    MouseUp {
        #[serde(default)]
        button: MouseButton,
    },
    Drag {
        from: Point,
        to: Point,
        #[serde(default)]
        button: MouseButton,
        #[serde(default = "default_drag_duration_ms")]
        duration_ms: u64,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    TypeText {
        text: String,
        #[serde(default)]
        delay_ms: u64,
    },
    KeyPress {
        keys: Vec<String>,
        #[serde(default = "default_repeat")]
        repeat: u16,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    Hold {
        keys: Vec<String>,
        duration_ms: u64,
    },
    Wait {
        duration_ms: u64,
    },
}

impl Action {
    pub fn validate(&self, geometry: Geometry) -> Result<(), ValidationError> {
        geometry.validate()?;
        match self {
            Self::Move { x, y, duration_ms } => {
                validate_point(geometry, Point { x: *x, y: *y })?;
                validate_duration(*duration_ms)?;
            }
            Self::Click { x, y, count, .. } => {
                validate_point(geometry, Point { x: *x, y: *y })?;
                if !(1..=3).contains(count) {
                    return Err(ValidationError::ClickCountOutOfRange);
                }
            }
            Self::MouseDown { .. } | Self::MouseUp { .. } => {}
            Self::Drag { from, to, duration_ms, .. } => {
                validate_point(geometry, *from)?;
                validate_point(geometry, *to)?;
                validate_duration(*duration_ms)?;
            }
            Self::Scroll { dx, dy } => {
                if *dx == 0 && *dy == 0 {
                    return Err(ValidationError::EmptyScroll);
                }
                if dx.unsigned_abs() > 10_000 || dy.unsigned_abs() > 10_000 {
                    return Err(ValidationError::ScrollOutOfRange);
                }
            }
            Self::TypeText { text, delay_ms } => {
                if text.len() > MAX_TEXT_BYTES {
                    return Err(ValidationError::TextTooLarge);
                }
                if *delay_ms > 1_000 {
                    return Err(ValidationError::DelayOutOfRange);
                }
            }
            Self::KeyPress { keys, repeat } => {
                validate_keys(keys)?;
                if !(1..=100).contains(repeat) {
                    return Err(ValidationError::RepeatOutOfRange);
                }
            }
            Self::KeyDown { key } | Self::KeyUp { key } => validate_key(key)?,
            Self::Hold { keys, duration_ms } => {
                validate_keys(keys)?;
                validate_duration(*duration_ms)?;
            }
            Self::Wait { duration_ms } => {
                if *duration_ms > MAX_WAIT_MS {
                    return Err(ValidationError::WaitOutOfRange);
                }
            }
        }
        Ok(())
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Move { .. } => "move",
            Self::Click { .. } => "click",
            Self::MouseDown { .. } => "mouse_down",
            Self::MouseUp { .. } => "mouse_up",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::TypeText { .. } => "type_text",
            Self::KeyPress { .. } => "key_press",
            Self::KeyDown { .. } => "key_down",
            Self::KeyUp { .. } => "key_up",
            Self::Hold { .. } => "hold",
            Self::Wait { .. } => "wait",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBatchRequest {
    pub protocol: u16,
    pub request_id: String,
    #[serde(default)]
    pub expected_input_generation: Option<u64>,
    pub actions: Vec<Action>,
    #[serde(default = "default_true")]
    pub stop_on_error: bool,
    #[serde(default)]
    pub observe: ObserveMode,
}

impl ActionBatchRequest {
    pub fn validate(&self, geometry: Geometry) -> Result<(), ValidationError> {
        validate_protocol(self.protocol)?;
        validate_request_id(&self.request_id)?;
        if self.actions.is_empty() {
            return Err(ValidationError::EmptyBatch);
        }
        if self.actions.len() > MAX_ACTIONS_PER_BATCH {
            return Err(ValidationError::TooManyActions);
        }
        for action in &self.actions {
            action.validate(geometry)?;
        }
        validate_input_sequence(&self.actions)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub protocol: u16,
    #[serde(default)]
    pub region: Option<Rect>,
    #[serde(default)]
    pub include_cursor: bool,
}

impl CaptureRequest {
    pub fn validate(&self, geometry: Geometry) -> Result<(), ValidationError> {
        validate_protocol(self.protocol)?;
        geometry.validate()?;
        if let Some(region) = self.region {
            region.validate(geometry)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub protocol: u16,
    pub session_id: String,
    pub runtime_generation: u64,
    pub observation_id: u64,
    pub input_generation: u64,
    pub captured_at_ms: u64,
    pub geometry: Geometry,
    pub image: ImageResource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_window: Option<WindowSummary>,
    pub accessibility: AccessibilityStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageResource {
    pub id: String,
    pub content_type: String,
    pub byte_length: u64,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub coordinate_space: CoordinateSpace,
    pub cursor_included: bool,
    pub href: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoordinateSpace {
    Desktop { display_id: String },
    Window { window_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSummary {
    pub id: String,
    pub title: String,
    pub bounds: Rect,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowList {
    pub protocol: u16,
    pub windows: Vec<WindowSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowFocusRequest {
    pub protocol: u16,
    pub window_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardContent {
    pub protocol: u16,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub protocol: u16,
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchResult {
    pub protocol: u16,
    pub pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileScope {
    Workspace,
    Downloads,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMetadata {
    pub protocol: u16,
    pub scope: FileScope,
    pub path: String,
    pub byte_length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSpawnRequest {
    pub protocol: u16,
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<FileScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessInfo {
    pub protocol: u16,
    pub process_id: String,
    pub pid: u32,
    pub state: ProcessState,
    pub exit_code: Option<i32>,
    pub started_at_ms: u64,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessOutput {
    pub protocol: u16,
    pub process_id: String,
    pub output: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessibilityStatus {
    Available { snapshot_id: String, node_count: u32 },
    Degraded { reason: String },
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessibilityNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub name: String,
    pub description: String,
    pub text: String,
    pub bounds: Option<Rect>,
    pub states: Vec<String>,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessibilitySnapshot {
    pub protocol: u16,
    pub snapshot_id: String,
    pub nodes: Vec<AccessibilityNode>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Attempted,
    Confirmed,
    Refused,
    NotAttempted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Confirmed,
    Unverified,
    NoChangeObserved,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResult {
    pub index: usize,
    pub action: String,
    pub status: ActionStatus,
    pub delivery: DeliveryStatus,
    pub effect: EffectStatus,
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation_error: Option<ApiError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Completed,
    Partial,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBatchResult {
    pub protocol: u16,
    pub request_id: String,
    pub status: BatchStatus,
    pub requested: usize,
    pub executed: usize,
    pub failed: usize,
    pub input_generation_before: u64,
    pub input_generation_after: u64,
    pub interference_detected: bool,
    pub started_at_ms: u64,
    pub completed_at_ms: u64,
    pub actions: Vec<ActionResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_observation: Option<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_observation_error: Option<ApiError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthResponse {
    pub protocol: u16,
    pub ready: bool,
    pub session_id: String,
    pub runtime_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityReport {
    pub protocol: u16,
    pub geometry: Geometry,
    pub hardware_acceleration: bool,
    pub actions: Vec<String>,
    pub screenshot_formats: Vec<String>,
    pub accessibility: bool,
    pub windows: bool,
    pub clipboard: bool,
    pub files: bool,
    pub process: bool,
    pub viewer: bool,
    pub limits: ProtocolLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolLimits {
    pub max_actions_per_batch: usize,
    pub max_text_bytes: usize,
    pub max_action_duration_ms: u64,
    pub max_wait_ms: u64,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_actions_per_batch: MAX_ACTIONS_PER_BATCH,
            max_text_bytes: MAX_TEXT_BYTES,
            max_action_duration_ms: MAX_ACTION_DURATION_MS,
            max_wait_ms: MAX_WAIT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    pub protocol: u16,
    pub ok: bool,
    pub error: ApiError,
}

impl ErrorEnvelope {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            ok: false,
            error: ApiError { code, message: message.into(), retryable: None, details: None },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    InvalidRequest,
    UnsupportedProtocol,
    StaleObservation,
    Conflict,
    NotFound,
    CapabilityUnavailable,
    DeadlineExceeded,
    Cancelled,
    Internal,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ValidationError {
    #[error("unsupported protocol version")]
    UnsupportedProtocol,
    #[error("geometry is outside the supported range")]
    GeometryOutOfRange,
    #[error("DPI is outside the supported range")]
    DpiOutOfRange,
    #[error("scale is outside the supported range")]
    ScaleOutOfRange,
    #[error("coordinate ({x},{y}) is outside the framebuffer")]
    CoordinateOutOfBounds { x: i32, y: i32 },
    #[error("capture region must not be empty")]
    EmptyRegion,
    #[error("capture region extends outside the framebuffer")]
    RegionOutOfBounds,
    #[error("request_id is empty or contains unsupported characters")]
    InvalidRequestId,
    #[error("action batch must not be empty")]
    EmptyBatch,
    #[error("action batch exceeds the action limit")]
    TooManyActions,
    #[error("click count must be between one and three")]
    ClickCountOutOfRange,
    #[error("action duration exceeds the limit")]
    DurationOutOfRange,
    #[error("scroll must have a nonzero axis")]
    EmptyScroll,
    #[error("scroll amount exceeds the limit")]
    ScrollOutOfRange,
    #[error("text payload exceeds the byte limit")]
    TextTooLarge,
    #[error("per-character delay exceeds the limit")]
    DelayOutOfRange,
    #[error("key chord is empty or exceeds the key limit")]
    InvalidKeyChord,
    #[error("key name is empty or invalid")]
    InvalidKey,
    #[error("repeat count must be between one and one hundred")]
    RepeatOutOfRange,
    #[error("wait duration exceeds the limit")]
    WaitOutOfRange,
    #[error("mouse/key down and up actions must form a valid, balanced batch")]
    InvalidInputSequence,
}

pub fn validate_protocol(protocol: u16) -> Result<(), ValidationError> {
    if protocol == PROTOCOL_VERSION { Ok(()) } else { Err(ValidationError::UnsupportedProtocol) }
}

fn validate_request_id(request_id: &str) -> Result<(), ValidationError> {
    if request_id.is_empty()
        || request_id.len() > MAX_REQUEST_ID_BYTES
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ValidationError::InvalidRequestId);
    }
    Ok(())
}

fn validate_point(geometry: Geometry, point: Point) -> Result<(), ValidationError> {
    if geometry.contains(point) {
        Ok(())
    } else {
        Err(ValidationError::CoordinateOutOfBounds { x: point.x, y: point.y })
    }
}

fn validate_duration(duration_ms: u64) -> Result<(), ValidationError> {
    if duration_ms <= MAX_ACTION_DURATION_MS {
        Ok(())
    } else {
        Err(ValidationError::DurationOutOfRange)
    }
}

fn validate_keys(keys: &[String]) -> Result<(), ValidationError> {
    if keys.is_empty() || keys.len() > MAX_KEYS_PER_CHORD {
        return Err(ValidationError::InvalidKeyChord);
    }
    for key in keys {
        validate_key(key)?;
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), ValidationError> {
    if key.is_empty()
        || key.len() > 64
        || !key.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ValidationError::InvalidKey);
    }
    Ok(())
}

fn validate_input_sequence(actions: &[Action]) -> Result<(), ValidationError> {
    let mut buttons = HashSet::new();
    let mut keys = HashSet::new();
    for action in actions {
        match action {
            Action::MouseDown { button } => {
                if !buttons.insert(*button) {
                    return Err(ValidationError::InvalidInputSequence);
                }
            }
            Action::MouseUp { button } => {
                if !buttons.remove(button) {
                    return Err(ValidationError::InvalidInputSequence);
                }
            }
            Action::KeyDown { key } => {
                if !keys.insert(key.to_ascii_lowercase()) {
                    return Err(ValidationError::InvalidInputSequence);
                }
            }
            Action::KeyUp { key } if !keys.remove(&key.to_ascii_lowercase()) => {
                return Err(ValidationError::InvalidInputSequence);
            }
            _ => {}
        }
    }
    if buttons.is_empty() && keys.is_empty() {
        Ok(())
    } else {
        Err(ValidationError::InvalidInputSequence)
    }
}

const fn default_true() -> bool {
    true
}

const fn default_click_count() -> u8 {
    1
}

const fn default_drag_duration_ms() -> u64 {
    500
}

const fn default_repeat() -> u16 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> Geometry {
        Geometry { width: 1280, height: 800, dpi: 96, scale: 1 }
    }

    #[test]
    fn framebuffer_boundaries_are_exact() {
        let geometry = geometry();
        for point in [Point { x: 0, y: 0 }, Point { x: 1279, y: 799 }] {
            assert!(geometry.contains(point));
        }
        for point in [
            Point { x: -1, y: 0 },
            Point { x: 0, y: -1 },
            Point { x: 1280, y: 799 },
            Point { x: 1279, y: 800 },
        ] {
            assert!(!geometry.contains(point));
        }
    }

    #[test]
    fn action_json_rejects_unknown_fields() {
        let input = r#"{"type":"click","x":0,"y":0,"surprise":true}"#;
        assert!(serde_json::from_str::<Action>(input).is_err());
    }

    #[test]
    fn batch_defaults_are_explicit() {
        let input = r#"{
          "protocol":2,
          "request_id":"req-1",
          "actions":[{"type":"click","x":0,"y":0}]
        }"#;
        let request: ActionBatchRequest = serde_json::from_str(input).unwrap();
        assert!(request.stop_on_error);
        assert_eq!(request.observe, ObserveMode::Final);
        assert_eq!(request.actions.len(), 1);
        request.validate(geometry()).unwrap();
    }

    #[test]
    fn batch_rejects_unsupported_protocol() {
        let request = ActionBatchRequest {
            protocol: 99,
            request_id: "req-1".into(),
            expected_input_generation: None,
            actions: vec![Action::Wait { duration_ms: 0 }],
            stop_on_error: true,
            observe: ObserveMode::None,
        };
        assert_eq!(request.validate(geometry()), Err(ValidationError::UnsupportedProtocol));
    }

    #[test]
    fn batch_rejects_bad_coordinates_and_ids() {
        let request = ActionBatchRequest {
            protocol: PROTOCOL_VERSION,
            request_id: "spaces are not okay".into(),
            expected_input_generation: None,
            actions: vec![Action::Click { x: 1280, y: 0, button: MouseButton::Left, count: 1 }],
            stop_on_error: true,
            observe: ObserveMode::Final,
        };
        assert_eq!(request.validate(geometry()), Err(ValidationError::InvalidRequestId));

        let valid_id = ActionBatchRequest { request_id: "req-1".into(), ..request };
        assert_eq!(
            valid_id.validate(geometry()),
            Err(ValidationError::CoordinateOutOfBounds { x: 1280, y: 0 })
        );
    }

    #[test]
    fn rect_checks_extent_without_overflow() {
        assert!(Rect { x: 1279, y: 799, width: 1, height: 1 }.validate(geometry()).is_ok());
        assert_eq!(
            Rect { x: 1279, y: 799, width: u32::MAX, height: 1 }.validate(geometry()),
            Err(ValidationError::RegionOutOfBounds)
        );
    }

    #[test]
    fn error_envelope_has_stable_shape() {
        let value =
            serde_json::to_value(ErrorEnvelope::new(ErrorCode::Unauthorized, "nope")).unwrap();
        assert_eq!(value["protocol"], PROTOCOL_VERSION);
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "unauthorized");
        assert!(value["error"].get("details").is_none());
    }

    #[test]
    fn held_input_must_be_balanced_inside_one_batch() {
        let base = ActionBatchRequest {
            protocol: PROTOCOL_VERSION,
            request_id: "req-1".into(),
            expected_input_generation: None,
            actions: vec![Action::MouseDown { button: MouseButton::Left }],
            stop_on_error: true,
            observe: ObserveMode::None,
        };
        assert_eq!(base.validate(geometry()), Err(ValidationError::InvalidInputSequence));

        let balanced = ActionBatchRequest {
            actions: vec![
                Action::MouseDown { button: MouseButton::Left },
                Action::Move { x: 10, y: 10, duration_ms: 0 },
                Action::MouseUp { button: MouseButton::Left },
            ],
            ..base
        };
        balanced.validate(geometry()).unwrap();
    }
}
