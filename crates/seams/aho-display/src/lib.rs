//! Session desktop authority. Providers must check their supplied guard before every effect.
//! Coordinates are native framebuffer pixels, even when a capture is downscaled.
#![forbid(unsafe_code)]

pub mod input;

use aho_cap::{Handle, HandleKind, SeamId};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

pub const SEAM: SeamId = SeamId("desktop");
pub type Guard = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Session lifecycle authority supplied by the world/composition, not by desktop tools.
#[derive(Clone)]
pub struct DesktopGuardHandle(pub Guard);
impl Handle for DesktopGuardHandle {
    const KIND: HandleKind = HandleKind::Custom("desktop-guard");
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub png: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyEvent {
    /// X11/RFB keysym, not a hardware scan code. Unicode: 0x01000000 | codepoint.
    pub keysym: u32,
    pub down: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerEvent {
    pub x: u16,
    pub y: u16,
    /// RFB button mask: left=1, middle=2, right=4, wheel up=8, wheel down=16.
    pub buttons: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerState {
    pub width: u16,
    pub height: u16,
    pub x: u16,
    pub y: u16,
    pub buttons: u8,
    /// False until this connection sends a pointer event or receives PointerPos.
    /// RFB cannot query another client's button state; buttons are last sent here.
    pub position_known: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum DisplayRequest {
    Screenshot(CaptureRequest),
    Key(KeyEvent),
    Pointer(PointerEvent),
    State,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DisplayResult {
    Screenshot(Screenshot),
    State(PointerState),
    Applied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum DisplayError {
    Denied(String),
    BadArgs(String),
    Protocol(String),
    Io(String),
    Unsupported(String),
}

impl DisplayError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied(_) => "RevocationEffective",
            Self::BadArgs(_) => "TOOL_BAD_ARGS",
            Self::Protocol(_) => "DISPLAY_PROTOCOL",
            Self::Io(_) => "DISPLAY_IO",
            Self::Unsupported(_) => "DISPLAY_UNSUPPORTED",
        }
    }
}
impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied(m) | Self::BadArgs(m) | Self::Protocol(m) | Self::Io(m) | Self::Unsupported(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for DisplayError {}

/// Blocking, serialized desktop operations. Async callers must use a blocking worker.
/// Implementations must fail closed after protocol loss and check authority before I/O.
pub trait Desktop: Send + Sync {
    fn screenshot(&self, request: &CaptureRequest) -> Result<Screenshot, DisplayError>;
    fn key(&self, event: KeyEvent) -> Result<(), DisplayError>;
    fn pointer(&self, event: PointerEvent) -> Result<(), DisplayError>;
    fn state(&self) -> Result<PointerState, DisplayError>;
    fn execute(&self, request: DisplayRequest) -> Result<DisplayResult, DisplayError> {
        match request {
            DisplayRequest::Screenshot(r) => self.screenshot(&r).map(DisplayResult::Screenshot),
            DisplayRequest::Key(e) => self.key(e).map(|()| DisplayResult::Applied),
            DisplayRequest::Pointer(e) => self.pointer(e).map(|()| DisplayResult::Applied),
            DisplayRequest::State => self.state().map(DisplayResult::State),
        }
    }
}

#[derive(Clone)]
pub struct DesktopHandle(pub Arc<dyn Desktop>);
impl Handle for DesktopHandle {
    const KIND: HandleKind = HandleKind::Desktop;
}
