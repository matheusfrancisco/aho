//! Operator-approved recipes and leased machines. The session sandbox is never a lab machine.
#![forbid(unsafe_code)]

use aho_cap::{Handle, HandleKind, SeamId};
use aho_display::Desktop;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub const SEAM: SeamId = SeamId("vm");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    Aarch64,
    X86_64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Acceleration {
    Tcg,
    Kvm,
    Hvf,
}

/// No arbitrary QEMU arguments: recipes cannot add host mounts or a second NIC.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub name: String,
    pub architecture: Architecture,
    pub acceleration: Acceleration,
    pub memory_mib: u32,
    pub cpus: u16,
    pub disk_bytes: u64,
    pub base_media: Option<String>,
    pub cdrom_media: Option<String>,
    pub kernel_media: Option<String>,
    pub initrd_media: Option<String>,
    pub firmware_media: Option<String>,
    #[serde(default)]
    pub kernel_args: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
}
fn default_workspace() -> String {
    "/workspace".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Raw,
    Qcow2,
    Iso,
    Kernel,
    Initrd,
    Firmware,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Media {
    pub name: String,
    pub kind: MediaKind,
    pub hash: String,
    pub bytes: u64,
    pub path: PathBuf,
}

/// Granted by the operator, not tool arguments. A log entry must exist before opening the lab.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseLimits {
    pub id: String,
    pub granted_ref: String,
    pub expires_at_ms: u64,
    pub max_machines: u32,
    pub max_disk_bytes: u64,
    pub max_cpu_s: u64,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineRole {
    Sandbox,
    Lab,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MachineState {
    Stopped,
    Running,
    Paused,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Disk {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub owner_session: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub owner_session: String,
    pub role: MachineRole,
    pub recipe: Recipe,
    pub lease_id: String,
    pub state: MachineState,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    /// Persisted wall-clock CPU upper bound (vCPU count times running duration).
    pub cpu_budget_used_ms: u64,
    pub uuid: String,
    pub pid: Option<u32>,
    pub disks: Vec<Disk>,
    pub mac: Option<[u8; 6]>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsoleChunk {
    pub text: String,
    pub cursor: u64,
    pub dropped: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrphanDisk {
    pub owner_session: String,
    pub machine: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub created_at_ms: u64,
    pub lease_expired: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum VmError {
    Invalid(String),
    NotFound(String),
    Locked(String),
    Io(String),
    Protocol(String),
    Quota(String),
    RevocationEffective { lease: String, reason: String },
}
impl VmError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "VM_BAD_ARGS",
            Self::NotFound(_) => "VM_NOT_FOUND",
            Self::Locked(_) => "VM_LOCKED",
            Self::Io(_) => "VM_IO",
            Self::Protocol(_) => "VM_PROTOCOL",
            Self::Quota(_) => "VM_QUOTA",
            Self::RevocationEffective { .. } => "RevocationEffective",
        }
    }
}
impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(s) | Self::NotFound(s) | Self::Locked(s) | Self::Io(s) | Self::Protocol(s) | Self::Quota(s) => f.write_str(s),
            Self::RevocationEffective { lease, reason } => write!(
                f,
                "RevocationEffective: lease {lease}: {reason}; no VM effect was performed. Ask the operator to grant a new lease."
            ),
        }
    }
}
impl std::error::Error for VmError {}

/// Methods are blocking (bounded QMP/RFB I/O); callers dispatch outside the async runtime.
pub trait VmLab: Send + Sync {
    fn check_lease(&self) -> Result<(), VmError>;
    fn recipes(&self) -> Result<Vec<Recipe>, VmError>;
    fn list(&self) -> Result<Vec<Machine>, VmError>;
    fn status(&self, name: &str) -> Result<Machine, VmError>;
    fn create(&self, name: &str, recipe: &str) -> Result<Machine, VmError>;
    fn disk_create(&self, name: &str, disk: &str, bytes: u64) -> Result<Disk, VmError>;
    fn start(&self, name: &str) -> Result<Machine, VmError>;
    fn stop(&self, name: &str, force: bool) -> Result<Machine, VmError>;
    fn console_read(&self, name: &str, cursor: u64, max_bytes: usize) -> Result<ConsoleChunk, VmError>;
    fn console_write(&self, name: &str, bytes: &[u8]) -> Result<(), VmError>;
    fn desktop(&self, name: &str) -> Result<Arc<dyn Desktop>, VmError>;
}

#[derive(Clone)]
pub struct VmControl(pub Arc<dyn VmLab>);
impl Handle for VmControl {
    const KIND: HandleKind = HandleKind::VmControl;
}
