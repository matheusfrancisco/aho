//! The one extension point. A tool, a provider, a world, a surface, a panel, a hook:
//! all implement `Capability`. There is no other plugin API.
//!
//! Two phases, both here:
//! * `plan(manifests, loadout, env)`: pure. Seam bindings, exclusive bundles, requirement
//!   checks, states, fixes, ceilings. `aho plan` is this plus rendering.
//! * `Composition::start(plan, loadout)`: builds Active rows in plan order, lets each register,
//!   and diffs what it registered against its manifest (`ManifestDrift` → Broken).
//!
//! ```compile_fail
//! // An unclassified tool does not compile: `class` is a required argument.
//! use aho_cap::*;
//! static T: ToolDecl = ToolDecl::new("x", "a tool with no consequence class");
//! ```
//!
//! ```compile_fail
//! // Reserved prefixes are rejected at compile time.
//! use aho_cap::*;
//! struct Squat;
//! impl Capability for Squat {
//!     const ID: CapabilityId = Self::ID_;
//!     const MANIFEST: &'static Manifest = &Self::MANIFEST_;
//!     type Deps = (); type Config = ();
//!     fn build(_: (), _: ()) -> Result<Self, BuildError> { Ok(Squat) }
//!     fn register(&self, _: &mut Registrar<'_>) {}
//! }
//! declare_capability!(Squat, "mcp__github", kind = Kind::Tool, provides = [], requires = [], tools = &[]);
//! ```
//!
//! ```
//! // The same capability with a legal name compiles, so the test above fails only on the prefix.
//! use aho_cap::*;
//! struct Fine;
//! impl Capability for Fine {
//!     const ID: CapabilityId = Self::ID_;
//!     const MANIFEST: &'static Manifest = &Self::MANIFEST_;
//!     type Deps = (); type Config = ();
//!     fn build(_: (), _: ()) -> Result<Self, BuildError> { Ok(Fine) }
//!     fn register(&self, _: &mut Registrar<'_>) {}
//! }
//! declare_capability!(Fine, "github-tools", kind = Kind::Tool, provides = [], requires = [], tools = &[]);
//! assert_eq!(Fine::ID.name, "github-tools");
//! ```

#![forbid(unsafe_code)]

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use aho_config::{ConfigKey, Loadout, Origin};
use aho_log::{CapState, CustomFold, DetachCause, OpKind, OpStatus, State};
use aho_types::*;
use serde::Serialize;

mod composition;
mod plan;
#[cfg(test)]
mod tests;

pub use composition::{BuildFn, BuiltinProfile, Composition, Resolver, core_keys, render_origin};
pub use plan::{ADDITIVE_SEAMS, Because, Missing, Plan, PlanError, PlanRow, PolicyInput, binaries_needed, fingerprint, follows, plan};

// ───────────────────────────────── identity ─────────────────────────────────

/// Minted only by `declare_capability!`: crate-level name + semver-major.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CapabilityId {
    pub name: &'static str,
    pub major: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SeamId(pub &'static str);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Kind {
    Provider,
    Tool,
    Surface,
    Panel,
    Hook,
    PromptSection,
    Template,
    Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Origin_ {
    Bundled,
    UserInstalled,
    Workspace,
    Experimental,
}
pub use Origin_ as CapOrigin;

// ───────────────────────────────── handles ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Route {
    /// The bound world's network: aho-net in sandbox worlds (enforced); under `world = host` the world
    /// provides the host network and says `enforced by: policy only`. Tools hold only this route.
    Sandbox,
    /// The host's own network. Reachable only under `world = host`.
    Host,
}

/// Runtime discriminant of a handle type; reflected into manifests so ceilings are derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum HandleKind {
    FsRead,
    FsWrite,
    HostFsRead,
    HostFsWrite,
    Subprocess,
    Terminals,
    Operations,
    Egress(Route),
    VmControl,
    Desktop,
    SessionAppend,
    SecretRead,
    NetAttach,
    Snapshot,
    Llm,
    Embed,
    Approval,
    Formal,
    Custom(&'static str),
}

/// A handle newtype (`FsRead(Arc<dyn Fs>)`) declares its kind so `Deps` can be reflected and resolved.
pub trait Handle: Clone + Send + Sync + 'static {
    const KIND: HandleKind;
}

/// Ceilings are derived from handles, not self-reported.
pub const fn ceiling(handles: &[HandleKind]) -> ConsequenceSet {
    let mut set = ConsequenceSet::EMPTY.with(Consequence::ReadOnly);
    let mut i = 0;
    while i < handles.len() {
        set = match handles[i] {
            HandleKind::FsWrite => set.with(Consequence::WorkspaceWrite),
            HandleKind::HostFsWrite => set.with(Consequence::HostWrite),
            HandleKind::Subprocess | HandleKind::Terminals | HandleKind::Operations | HandleKind::Formal => {
                set.with(Consequence::SpawnsProcess)
            }
            HandleKind::Egress(_) | HandleKind::Llm | HandleKind::Embed => set.with(Consequence::NetworkEgress),
            HandleKind::VmControl => set.with(Consequence::ControlsVM),
            HandleKind::Desktop => set.with(Consequence::ControlsDesktop),
            HandleKind::Approval => set.with(Consequence::TalksToHuman),
            _ => set,
        };
        i += 1;
    }
    set
}

const fn is_host_handle(h: HandleKind) -> bool {
    matches!(h, HandleKind::HostFsRead | HandleKind::HostFsWrite | HandleKind::Egress(Route::Host))
}

// ───────────────────────────────── contract ─────────────────────────────────

/// Exactly the handles a capability holds (real build: `#[derive(Deps)]`).
pub trait Deps: Sized + Send + Sync + 'static {
    const REQUIRES: &'static [HandleKind];
    fn resolve(resolver: &Resolver) -> Result<Self, MissingDep>;
}

/// Typed config slice (real build: `#[derive(Config)]` with `Deserialize + JsonSchema`).
pub trait Config: Sized + Send + Sync + 'static {
    const PREFIX: &'static str;
    fn keys() -> Vec<ConfigKey> {
        Vec::new()
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError>;
}

impl Deps for () {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(_: &Resolver) -> Result<Self, MissingDep> {
        Ok(())
    }
}

/// A capability holding exactly one handle (`type Deps = (SubprocessHandle,)`).
impl<H: Handle> Deps for (H,) {
    const REQUIRES: &'static [HandleKind] = &[H::KIND];
    fn resolve(resolver: &Resolver) -> Result<Self, MissingDep> {
        Ok((resolver.handle()?,))
    }
}

impl Config for () {
    const PREFIX: &'static str = "";
    fn from_loadout(_: &Loadout) -> Result<Self, ConfigError> {
        Ok(())
    }
}

pub trait Capability: Sized + Send + Sync + 'static {
    const ID: CapabilityId;
    const MANIFEST: &'static Manifest;
    type Deps: Deps;
    type Config: Config;

    fn build(deps: Self::Deps, cfg: Self::Config) -> Result<Self, BuildError>;
    /// Provide handles, tools, hooks, panels, entry kinds.
    fn register(&self, r: &mut Registrar<'_>);
}

#[derive(Debug, Clone)]
pub struct MissingDep(pub HandleKind);

#[derive(Debug, Clone)]
pub enum ConfigError {
    MissingKey(String),
    Type { key: String, expected: &'static str },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::MissingKey(k) => write!(f, "missing config key `{k}`"),
            ConfigError::Type { key, expected } => write!(f, "`{key}` must be {expected}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum BuildError {
    Config(ConfigError),
    Missing(HandleKind),
    Probe(String),
    Unsupported(&'static str),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Config(e) => write!(f, "{e}"),
            BuildError::Missing(h) => write!(f, "missing handle {h:?}"),
            BuildError::Probe(s) => write!(f, "probe failed: {s}"),
            BuildError::Unsupported(s) => write!(f, "unsupported: {s}"),
        }
    }
}

/// Helpers used by `Config::from_loadout` implementations.
pub fn cfg_str(l: &Loadout, key: &str, default: &str) -> String {
    l.str(key).unwrap_or(default).to_string()
}
pub fn cfg_u64(l: &Loadout, key: &str, default: u64) -> u64 {
    l.get(key).and_then(|v| v.as_u64()).unwrap_or(default)
}
pub fn cfg_bool(l: &Loadout, key: &str, default: bool) -> bool {
    l.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

// ───────────────────────────────── manifest ─────────────────────────────────

/// Computed from the types by `declare_capability!`; never hand-maintained.
#[derive(Debug)]
pub struct Manifest {
    pub id: CapabilityId,
    pub kind: Kind,
    pub requires: &'static [Requirement],
    pub provides: &'static [SeamId],
    /// Handle kinds this capability puts into the resolver (providers).
    pub provides_handles: &'static [HandleKind],
    /// Handle kinds this capability holds (`Deps::REQUIRES`).
    pub handles: &'static [HandleKind],
    /// Seam bundle: bind all-or-nothing (a world = Fs + Subprocess + Sandbox).
    pub exclusive: bool,
    pub tools: &'static [ToolDecl],
    pub entry_kinds: &'static [&'static str],
    pub config_keys: fn() -> Vec<ConfigKey>,
    pub replaces: &'static [CapabilityId],
    pub unlock: Option<Fix>,
    /// For worlds: what enforces isolation (`policy only`, `Linux namespaces`, …).
    pub enforced_by: Option<&'static str>,
    pub invariants: &'static [Invariant],
    /// TLA+ modules / Lean theorems covering this capability's entry kinds.
    pub formal: &'static [SpecRef],
    pub doc: &'static str,
}

impl Manifest {
    pub fn declared(&self) -> ConsequenceSet {
        self.tools.iter().fold(ConsequenceSet::EMPTY, |s, t| s.with(t.class))
    }
    pub fn hash(&self) -> Hash {
        let tools: Vec<(&str, Consequence)> = self.tools.iter().map(|t| (t.name, t.class)).collect();
        Hash::of_json(&(self.id, self.kind, self.provides, self.handles, tools, self.entry_kinds))
    }
}

/// Closed (never a closure), so every Locked node renders a fix.
#[derive(Debug)]
pub enum Requirement {
    Seam(SeamId),
    Cap(&'static str),
    AnyOf(&'static [Requirement]),
    Setting { key: &'static str, pred: Pred },
    Credential(&'static str),
    Binary(&'static str),
    Platform(Os),
    Feature(&'static str),
}

#[derive(Debug)]
pub enum Pred {
    Eq(&'static str),
    OneOf(&'static [&'static str]),
    Set,
    True,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Windows
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Fix {
    pub cli: &'static str,
    pub ui_action: Option<&'static str>,
}

impl Fix {
    pub const fn cli(cli: &'static str) -> Self {
        Fix { cli, ui_action: None }
    }
}

#[derive(Debug)]
pub struct ToolDecl {
    pub name: &'static str,
    pub description: &'static str,
    pub aliases: &'static [&'static str],
    /// Non-optional: an unclassified tool does not compile.
    pub class: Consequence,
    pub destructive: bool,
    /// Schema may depend on the bound world (e.g. `escalate` exists only if the world confines).
    pub schema: fn(&BoundWorld) -> RootSchema,
    /// Anything but an explicit `Safe` is a barrier.
    pub concurrency: fn(&Value) -> Concurrency,
    pub interrupt: Interrupt,
    /// Whether a running call may become a session job (`OpDetached`); config `jobs.detach.<tool>`
    /// narrows or widens it, except that `Never` stays `Never`.
    pub detach: Detach,
    /// A call to this tool waits on session jobs (`job_wait`): under `jobs.deliver = on_wait` it
    /// releases the held job deliveries while it runs and at the boundary after its result.
    pub awaits_jobs: bool,
    /// Spill threshold in chars; `None` = never spill (`read`).
    pub max_inline: Option<usize>,
    /// Tool's own opinion in the decision chain (step 3). `None` = passthrough.
    pub check: fn(&Value, &PolicyView) -> Option<Decision>,
    pub present: Present,
    /// Operation metadata: how a crash mid-dispatch folds on resume.
    pub op_kind: OpKind,
    pub rerunnable: bool,
    pub external_effect: bool,
}

/// Defaults for every field except the ones a tool must state (`name`, `description`, `class`).
pub struct ToolDefaults {
    pub aliases: &'static [&'static str],
    pub destructive: bool,
    pub schema: fn(&BoundWorld) -> RootSchema,
    pub concurrency: fn(&Value) -> Concurrency,
    pub interrupt: Interrupt,
    pub detach: Detach,
    pub awaits_jobs: bool,
    pub max_inline: Option<usize>,
    pub check: fn(&Value, &PolicyView) -> Option<Decision>,
    pub present: Present,
    pub op_kind: OpKind,
    pub rerunnable: bool,
    pub external_effect: bool,
}

pub fn no_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(serde_json::json!({"type": "object", "properties": {}}))
}
pub fn always_safe(_: &Value) -> Concurrency {
    Concurrency::Safe
}
pub fn always_exclusive(_: &Value) -> Concurrency {
    Concurrency::Exclusive
}
pub fn passthrough(_: &Value, _: &PolicyView) -> Option<Decision> {
    None
}
fn generic_call(v: &Value) -> Card {
    Card::Generic { title: "call".into(), body: v.to_string() }
}
fn generic_result(_: &Value, r: &Value) -> Card {
    Card::Generic { title: "result".into(), body: r.to_string() }
}

impl ToolDecl {
    /// `ToolDecl { name, description, class, ..ToolDecl::DEFAULTS }` is not valid Rust for
    /// partially-defaulted required fields, so tools use `ToolDecl::new(...)` + `const` setters.
    pub const DEFAULTS: ToolDefaults = ToolDefaults {
        aliases: &[],
        destructive: false,
        schema: no_schema,
        concurrency: always_exclusive,
        interrupt: Interrupt::Block,
        detach: Detach::Never,
        awaits_jobs: false,
        max_inline: Some(50_000),
        check: passthrough,
        present: Present { call: generic_call, result: generic_result },
        op_kind: OpKind::Compute,
        rerunnable: false,
        external_effect: false,
    };
    pub const fn new(name: &'static str, description: &'static str, class: Consequence) -> ToolDecl {
        let d = ToolDecl::DEFAULTS;
        ToolDecl {
            name,
            description,
            aliases: d.aliases,
            class,
            destructive: d.destructive,
            schema: d.schema,
            concurrency: d.concurrency,
            interrupt: d.interrupt,
            detach: d.detach,
            awaits_jobs: d.awaits_jobs,
            max_inline: d.max_inline,
            check: d.check,
            present: d.present,
            op_kind: d.op_kind,
            rerunnable: d.rerunnable,
            external_effect: d.external_effect,
        }
    }
    pub const fn schema(mut self, f: fn(&BoundWorld) -> RootSchema) -> Self {
        self.schema = f;
        self
    }
    pub const fn safe(mut self) -> Self {
        self.concurrency = always_safe;
        self
    }
    pub const fn concurrency(mut self, f: fn(&Value) -> Concurrency) -> Self {
        self.concurrency = f;
        self
    }
    pub const fn cancel_on_interrupt(mut self) -> Self {
        self.interrupt = Interrupt::Cancel;
        self
    }
    /// The model may run this call in the background (`background: true`); `jobs.detach.<tool>` may
    /// change how (`always`, `after:<dur>`, `never`).
    pub const fn detachable(mut self) -> Self {
        self.detach = Detach::OnRequest;
        self
    }
    pub const fn detach(mut self, d: Detach) -> Self {
        self.detach = d;
        self
    }
    /// See [`ToolDecl::awaits_jobs`].
    pub const fn awaits_jobs(mut self) -> Self {
        self.awaits_jobs = true;
        self
    }
    pub const fn max_inline(mut self, n: Option<usize>) -> Self {
        self.max_inline = n;
        self
    }
    pub const fn check(mut self, f: fn(&Value, &PolicyView) -> Option<Decision>) -> Self {
        self.check = f;
        self
    }
    pub const fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
    pub const fn present(mut self, p: Present) -> Self {
        self.present = p;
        self
    }
    pub const fn op(mut self, kind: OpKind, rerunnable: bool, external_effect: bool) -> Self {
        self.op_kind = kind;
        self.rerunnable = rerunnable;
        self.external_effect = external_effect;
        self
    }
    pub const fn aliases(mut self, a: &'static [&'static str]) -> Self {
        self.aliases = a;
        self
    }
    pub fn spec(&self, world: &BoundWorld) -> ToolSpec {
        ToolSpec { name: self.name.into(), description: self.description.into(), input_schema: (self.schema)(world), class: self.class }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Concurrency {
    Safe,
    Exclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Interrupt {
    Cancel,
    Block,
}

/// How a running call of a tool may leave its turn and become a session job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Detach {
    /// Always turn-scoped; no config can detach it.
    Never,
    /// Detached when the model asks (`background: true`), by config, or by the operator.
    OnRequest,
    /// Detached at start.
    Always,
}

#[derive(Debug, Clone, Copy)]
pub struct Present {
    pub call: fn(&Value) -> Card,
    pub result: fn(&Value, &Value) -> Card,
}

/// The result of a tool's own `check` (step 3). The full chain and reasons live in `aho-policy`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

/// Read-only view of the folded policy handed to `ToolDecl::check` and hooks.
#[derive(Clone, Debug, Serialize)]
pub struct PolicyView {
    pub epoch: Epoch,
    pub allowed: ConsequenceSet,
    pub world: BoundWorld,
    pub workspace: String,
    pub home: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BoundWorld {
    pub name: String,
    pub confines: bool,
    pub enforced_by: String,
}

impl BoundWorld {
    pub fn host() -> BoundWorld {
        BoundWorld { name: "host".into(), confines: false, enforced_by: "policy only".into() }
    }
}

/// A fold-checker run in every test root and by `aho session verify`.
#[derive(Debug)]
pub struct Invariant {
    pub name: &'static str,
    pub check: fn(&State) -> Result<(), String>,
}

#[derive(Debug)]
pub struct SpecRef {
    pub kind: aho_log::FormalKind,
    pub path: &'static str,
}

// ───────────────────────────────── registrar ─────────────────────────────────

/// Minted only by the resolver for Active rows; `non_exhaustive` forbids construction outside aho-cap.
#[non_exhaustive]
pub struct Grant {
    pub cap: CapabilityId,
}

pub type PreExecute = Arc<dyn Fn(&ExecView) -> PreDecision + Send + Sync>;
pub type GuardFn = Arc<dyn Fn(&ExecView) -> Option<String> + Send + Sync>;
pub type PostExecute = Arc<dyn Fn(&ExecView, &ExecOutcome) -> PostDecision + Send + Sync>;
pub type ContextFn = Arc<dyn Fn(&State) -> Option<String> + Send + Sync>;
pub type SectionFn = Arc<dyn Fn(&State) -> String + Send + Sync>;
pub type ToolExec = Arc<dyn Fn(ExecRequest) -> BoxFuture<'static, ExecOutcome> + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
pub struct ExecView {
    pub call: CallId,
    pub tool: String,
    pub class: Consequence,
    pub args: Value,
    pub policy: PolicyView,
    pub by: Principal,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreDecision {
    Allow,
    Deny { reason: String },
    Ask { reason: Option<String> },
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PostDecision {
    Accept { content: Option<Vec<ContentBlock>>, deferred_context: Vec<String> },
    Block { feedback: String, deferred_context: Vec<String> },
}

/// The input to a tool body. `args` are the model's bytes; derived views are separate.
pub struct ExecRequest {
    pub view: ExecView,
    pub cancel: CancelToken,
    /// Streamed progress lines (`AHO_PROGRESS {json}` markers land here), never chain entries.
    pub progress: Option<tokio_like::Sender>,
}

pub mod tokio_like {
    //! A tiny progress sink so `aho-cap` needs no runtime dependency.
    use std::sync::{Arc, Mutex};
    #[derive(Clone, Default)]
    pub struct Sender(pub Arc<Mutex<Vec<String>>>);
    impl Sender {
        pub fn send(&self, line: String) {
            if let Ok(mut g) = self.0.lock() {
                g.push(line);
            }
        }
        pub fn drain(&self) -> Vec<String> {
            self.0.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecOutcome {
    /// Canonical value + model-visible content + card payload + applied diffs.
    Done {
        value: Value,
        content: Vec<ContentBlock>,
        meta: Option<Card>,
    },
    Err {
        code: String,
        reason: String,
    },
    Cancelled,
}

impl ExecOutcome {
    pub fn text(t: impl Into<String>) -> ExecOutcome {
        let t = t.into();
        ExecOutcome::Done { value: Value::String(t.clone()), content: vec![ContentBlock::text(t)], meta: None }
    }
    pub fn err(code: &str, reason: impl Into<String>) -> ExecOutcome {
        ExecOutcome::Err { code: code.into(), reason: reason.into() }
    }
}

/// Cancellation tree token with a fire epoch: children see a parent's cancel.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    inner: Arc<std::sync::atomic::AtomicU64>,
    parent: Option<Arc<std::sync::atomic::AtomicU64>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn child(&self) -> CancelToken {
        CancelToken { inner: Arc::default(), parent: Some(self.inner.clone()) }
    }
    /// Fires with a monotonically increasing epoch; returns it.
    pub fn cancel(&self) -> u64 {
        self.inner.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }
    pub fn epoch(&self) -> u64 {
        self.inner.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.epoch() > 0 || self.parent.as_ref().map(|p| p.load(std::sync::atomic::Ordering::SeqCst) > 0).unwrap_or(false)
    }
    /// Only resets if nobody fired again since `seen` (issue: a reset must not erase a newer fire).
    pub fn reset_if_epoch(&self, seen: u64) -> bool {
        self.inner.compare_exchange(seen, 0, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok()
    }
}

#[derive(Clone)]
pub struct HookSpec {
    pub on: &'static [HookEvent],
    pub if_class: Option<ConsequenceSet>,
    pub if_tool: Option<&'static str>,
    pub timeout_ms: u64,
    pub once: bool,
    pub on_error: OnError,
    pub run: HookFn,
}

pub type HookFn = Arc<dyn Fn(HookEvent, &Value) -> BoxFuture<'static, HookOutput> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum HookEvent {
    PreStep,
    Request,
    ToolPre,
    ToolPost,
    RequestError,
    TurnStopping,
    TurnEnd,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
    /// A finished job is about to be delivered; the payload is the bounded [`JobView`]. Only
    /// [`HookOutput::delivery`] counts here.
    JobDone,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct HookOutput {
    pub decision: Option<Decision>,
    pub context: Vec<String>,
    pub stop: Option<String>,
    /// `HookEvent::JobDone` only: what happens to the delivery.
    pub delivery: Delivery,
}

/// One finished job about to be logged as `JobDelivered`, as [`Registrar::delivery`] callbacks
/// see it (`HookEvent::JobDone` hooks get it as JSON with `content` cut to text).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobView {
    pub op: OpId,
    pub call: CallId,
    pub tool: String,
    pub status: OpStatus,
    pub cause: DetachCause,
    /// What the model will see: the tool's content, after any earlier `Replace` in the chain.
    pub content: Vec<ContentBlock>,
    /// How many times this delivery was deferred before.
    pub deferred: u32,
    /// Every op offered at this delivery point, in offer order (this one included).
    pub with: Vec<OpId>,
}

/// What a delivery callback or `JobDone` hook does with one delivery. The entry is always
/// logged: suppressing delivers a marker, deferring offers it again later.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Delivery {
    /// Deliver as offered.
    #[default]
    Keep,
    /// Deliver this content instead; later callbacks see it.
    Replace(Vec<ContentBlock>),
    /// Deliver a marker naming the capability instead of the content. Ends the chain.
    Suppress,
    /// Hold the delivery; from the first delivery point after `until`, the whole chain sees it
    /// again. Ends the chain.
    Defer { until: std::time::Instant },
}

pub type DeliveryFn = Arc<dyn Fn(&JobView) -> Delivery + Send + Sync>;

/// Hooks are veto-`⊓`: an erroring hook never becomes Allow. Default Deny.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum OnError {
    Allow,
    Deny,
}

/// Everything a capability registered. Consumed by `aho-session`.
#[derive(Default, Clone)]
pub struct Registry {
    pub handles: BTreeMap<HandleKind, (CapabilityId, Arc<dyn Any + Send + Sync>)>,
    pub tools: Vec<(CapabilityId, &'static ToolDecl, ToolExec)>,
    pub dynamic_tools: Vec<(CapabilityId, ToolSpec, ToolExec)>,
    pub pre: Vec<(CapabilityId, PreExecute)>,
    pub guards: Vec<(CapabilityId, GuardFn)>,
    pub post: Vec<(CapabilityId, PostExecute)>,
    pub hooks: Vec<(CapabilityId, HookSpec)>,
    pub deliveries: Vec<(CapabilityId, DeliveryFn)>,
    pub sections: Vec<(CapabilityId, i32, String, bool, SectionFn)>,
    pub contexts: Vec<(CapabilityId, ContextFn)>,
    pub panels: Vec<(CapabilityId, &'static str, &'static [&'static str])>,
    pub entry_kinds: BTreeMap<String, CustomFold>,
    pub services: BTreeMap<String, Arc<dyn Any + Send + Sync>>,
}

impl Registry {
    pub fn handle<H: Handle>(&self) -> Option<H> {
        self.handles.get(&H::KIND).and_then(|(_, a)| a.downcast_ref::<H>().cloned())
    }
    pub fn service<T: Any + Send + Sync + Clone>(&self, name: &str) -> Option<T> {
        self.services.get(name).and_then(|a| a.downcast_ref::<T>().cloned())
    }
    pub fn tool(&self, name: &str) -> Option<(&'static ToolDecl, ToolExec)> {
        self.tools.iter().find(|(_, d, _)| d.name == name || d.aliases.contains(&name)).map(|(_, d, e)| (*d, e.clone()))
    }
}

/// The registrations a capability may make, and the power of each.
pub struct Registrar<'a> {
    grant: &'a Grant,
    reg: &'a mut Registry,
    recorded_tools: Vec<&'static str>,
    recorded_handles: Vec<HandleKind>,
}

impl<'a> Registrar<'a> {
    pub fn cap(&self) -> CapabilityId {
        self.grant.cap
    }
    /// Put a handle into the resolver for dependents (providers).
    pub fn provide<H: Handle>(&mut self, h: H) {
        self.recorded_handles.push(H::KIND);
        self.reg.handles.insert(H::KIND, (self.grant.cap, Arc::new(h)));
    }
    /// A named shared object other capabilities may look up (e.g. the operations manager).
    pub fn service<T: Any + Send + Sync>(&mut self, name: impl Into<String>, v: T) {
        self.reg.services.insert(name.into(), Arc::new(v));
    }
    pub fn tool(&mut self, decl: &'static ToolDecl, exec: ToolExec) {
        self.recorded_tools.push(decl.name);
        self.reg.tools.push((self.grant.cap, decl, exec));
    }
    /// Runtime-discovered tools (MCP). Prefix reserved; the class is required or the tool is quarantined.
    pub fn dynamic(&mut self, spec: ToolSpec, exec: ToolExec) {
        self.reg.dynamic_tools.push((self.grant.cap, spec, exec));
    }
    pub fn pre_execute(&mut self, f: PreExecute) {
        self.reg.pre.push((self.grant.cap, f));
    }
    /// Monotonic: can only deny. Owner policy, not reorderable by plugins.
    pub fn guard(&mut self, f: GuardFn) {
        self.reg.guards.push((self.grant.cap, f));
    }
    pub fn post_execute(&mut self, f: PostExecute) {
        self.reg.post.push((self.grant.cap, f));
    }
    pub fn hook(&mut self, spec: HookSpec) {
        self.reg.hooks.push((self.grant.cap, spec));
    }
    /// Sees every finished job before its `JobDelivered` is logged and may replace, suppress or
    /// defer it. The chain: every `delivery` callback, then every `HookEvent::JobDone` hook, each
    /// in registration order; `Suppress` or `Defer` ends it.
    pub fn delivery(&mut self, f: DeliveryFn) {
        self.reg.deliveries.push((self.grant.cap, f));
    }
    /// Static prompt section; `cache_break` marks the end of the cache-stable prefix.
    pub fn prompt_section(&mut self, order: i32, name: &str, cache_break: bool, f: SectionFn) {
        self.reg.sections.push((self.grant.cap, order, name.to_string(), cache_break, f));
    }
    /// Dynamic text, logged as a `UserMessage{form: Snapshot}` only when it changes.
    pub fn context(&mut self, f: ContextFn) {
        self.reg.contexts.push((self.grant.cap, f));
    }
    pub fn panel(&mut self, slot: &'static str, projections: &'static [&'static str]) {
        self.reg.panels.push((self.grant.cap, slot, projections));
    }
    pub fn entry_kind(&mut self, ns: &str, fold: CustomFold) {
        self.reg.entry_kinds.insert(ns.to_string(), fold);
    }
}

// ───────────────────────────────── discovery ─────────────────────────────────

/// Runtime-found capabilities: SKILL.md dirs, MCP servers, recipes, other harnesses' logs.
pub trait Discoverer: Send + Sync {
    fn discover(&self, env: &Env) -> Vec<Discovered>;
}

#[derive(Clone, Debug, Serialize)]
pub struct Discovered {
    pub id: String,
    pub kind: Kind,
    pub origin: CapOrigin,
    pub source_path: Option<String>,
    pub classes: ConsequenceSet,
    pub requires: Vec<String>,
    pub broken: Option<String>,
    pub doc: String,
}

/// The only ambient environment a capability may see (tests redirect `home`).
#[derive(Clone, Debug, Serialize)]
pub struct Env {
    pub home: String,
    pub project: String,
    pub os: Os,
    pub binaries: BTreeSet<String>,
    pub credentials: BTreeSet<String>,
    pub features: BTreeSet<String>,
}

impl Env {
    pub fn empty(os: Os) -> Env {
        Env {
            home: String::new(),
            project: String::new(),
            os,
            binaries: BTreeSet::new(),
            credentials: BTreeSet::new(),
            features: BTreeSet::new(),
        }
    }
}

// ───────────────────────────────── macro ─────────────────────────────────

#[doc(hidden)]
pub const fn __starts_with(s: &str, p: &str) -> bool {
    let (s, p) = (s.as_bytes(), p.as_bytes());
    if p.len() > s.len() {
        return false;
    }
    let mut i = 0;
    while i < p.len() {
        if s[i] != p[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[doc(hidden)]
pub const fn __parse_major(v: &str) -> u32 {
    let b = v.as_bytes();
    let mut i = 0;
    let mut n = 0u32;
    while i < b.len() && b[i] != b'.' {
        n = n * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    n
}

/// Builds `ID_` and `MANIFEST_` from the types; rejects reserved prefixes at compile time.
#[macro_export]
macro_rules! declare_capability {
    (
        $ty:ident, $name:literal,
        kind = $kind:expr,
        provides = [$($seam:expr),* $(,)?],
        requires = [$($req:expr),* $(,)?],
        tools = $tools:expr
        $(, provides_handles = [$($ph:expr),* $(,)?])?
        $(, exclusive = $excl:expr)?
        $(, replaces = [$($rep:expr),* $(,)?])?
        $(, unlock = $fix:expr)?
        $(, enforced_by = $enf:expr)?
        $(, entry_kinds = [$($ek:literal),* $(,)?])?
        $(, doc = $doc:literal)?
        $(,)?
    ) => {
        const _: () = assert!(
            !$crate::__starts_with($name, "mcp__") && !$crate::__starts_with($name, "vm_") && !$crate::__starts_with($name, "aho_"),
            "reserved capability prefix (mcp__, vm_, aho_)"
        );
        impl $ty {
            pub const ID_: $crate::CapabilityId =
                $crate::CapabilityId { name: $name, major: $crate::__parse_major(env!("CARGO_PKG_VERSION")) };
            pub const MANIFEST_: $crate::Manifest = $crate::Manifest {
                id: Self::ID_,
                kind: $kind,
                requires: &[$($req),*],
                provides: &[$($seam),*],
                provides_handles: &[$($($ph),*)?],
                handles: <<$ty as $crate::Capability>::Deps as $crate::Deps>::REQUIRES,
                exclusive: false $(|| $excl)?,
                tools: $tools,
                entry_kinds: &[$($($ek),*)?],
                config_keys: <<$ty as $crate::Capability>::Config as $crate::Config>::keys,
                replaces: &[$($($rep),*)?],
                unlock: $crate::__opt!($($fix)?),
                enforced_by: $crate::__opt!($($enf)?),
                invariants: &[],
                formal: &[],
                doc: $crate::__or!($($doc)?; ""),
            };
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __opt {
    () => {
        None
    };
    ($x:expr) => {
        Some($x)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __or {
    (; $d:expr) => {
        $d
    };
    ($x:expr; $d:expr) => {
        $x
    };
}
