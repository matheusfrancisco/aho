//! `10`/`12` — subagents. Child policy = parent ⊓ decl; fork excludes the open turn;
//! lifecycle is recorded in the *parent* log; a subagent is a `Remote` operation (16).

#![forbid(unsafe_code)]

use aho_cap::SeamId;
use aho_policy::SandboxPolicy;
use aho_types::*;

pub const SEAM: SeamId = SeamId("subagent");
/// Registry service name under which the bound provider registers itself as
/// `Arc<dyn SubagentProvider>`; `tool-subagent` resolves it from its `Deps`.
pub const SERVICE: &str = "subagent";

/// One child as its provider sees it (for `list_agents` and the parent's lifecycle entries).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildInfo {
    pub child: SessionId,
    /// Capability id of the provider that runs it (`Subagent/Started.provider`).
    pub provider: String,
    pub label: String,
    /// Delegation depth; the root session is 0.
    pub depth: u32,
    pub running: bool,
    pub last_stop: Option<StopReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    OneShot,
    Continuable,
}

#[derive(Clone, Debug)]
pub struct SpawnRequest {
    pub label: String,
    pub prompt: String,
    pub mode: Mode,
    pub persona: Option<String>,
    pub tool_filter: Vec<String>,
    pub model: Option<String>,
    pub decl_cut: ConsequenceSet,
    pub is_async: bool,
    pub bubble: bool,
    /// `Some(at)` = fork the parent log at `at` (must be a `TurnEnd`); `None` = spawn fresh.
    pub fork_at: Option<EntryId>,
    pub depth: u32,
}

#[derive(Clone, Debug)]
pub struct Result_ {
    pub child: SessionId,
    pub stop: StopReason,
    pub last_message: Option<String>,
    /// ≤ 4096 bytes, secret-free.
    pub diagnostic: Option<String>,
}

#[derive(Debug)]
pub enum SubagentError {
    DepthLimit(u32),
    Unsupported(&'static str),
    Provider(String),
}

/// What a provider supports; a request needing an unsupported feature is rejected, never degraded.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    pub fork: bool,
    pub persona: bool,
    pub tool_filter: bool,
    pub output_schema: bool,
    pub max_depth: u32,
}

pub trait SubagentProvider: Send + Sync {
    fn features(&self) -> Features;
    /// The parent's fork point: its last `Turn/End` on the head branch at which no call or
    /// operation is open (06: a fork never inherits the in-flight turn). `None` when the parent
    /// has not completed a turn yet or the provider cannot fork.
    fn fork_point(&self) -> Option<EntryId> {
        None
    }
    fn spawn(&self, req: SpawnRequest, policy: SandboxPolicy) -> BoxFuture<'_, Result<SessionId, SubagentError>>;
    fn send(&self, child: SessionId, text: String) -> BoxFuture<'_, Result<(), SubagentError>>;
    fn interrupt(&self, child: SessionId) -> BoxFuture<'_, Result<(), SubagentError>>;
    fn wait(&self, child: SessionId) -> BoxFuture<'_, Result<Result_, SubagentError>>;
    /// What the provider knows about one of its children; `None` for an unknown id.
    fn child_info(&self, _child: SessionId) -> Option<ChildInfo> {
        None
    }
    /// Every child this provider started for the current session, oldest first.
    fn children(&self) -> Vec<ChildInfo> {
        Vec::new()
    }
}

/// Monotone: `max(inherited, requested)`; a resumed child cannot reset to 0.
pub fn depth(inherited: u32, requested: u32) -> u32 {
    inherited.max(requested)
}
