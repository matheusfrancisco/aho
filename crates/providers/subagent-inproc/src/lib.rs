//! In-process subagents. A child is a full [`aho_session::Session`] over its own log
//! (`<session.dir>/<parent>/<child>.jsonl`, or memory), run to idle inside the parent's tool
//! operation as a `Remote` op, so the parent sees a placeholder and keeps working while the
//! child runs.
//!
//! * Policy = `aho_policy::child_policy(parent, decl_cut, is_async, bubble)`: narrow-only;
//!   an async child cannot reach a human unless bubbled. Operator rules survive into the child
//!   because it resolves the same loadout.
//! * The child log starts with `Subagent/Descriptor`; a fork then re-appends the parent's
//!   completed-turn prefix in order (never the open turn); then
//!   `Policy/Epoch{by: System("delegation")}` so the child's fold is self-contained.
//! * Depth is monotone: `max(inherited + 1, requested)`, capped by `subagent.max_depth`.
//! * The parent log gets `Subagent/Started` / `Subagent/Ended`: `tool-subagent` returns them in
//!   its result's `value.events`, which the session appends after the tool result.
//!
//! Embedders put a [`SubagentRuntime`] (composition, plan, loadout, model routes, operator
//! factory, the session's log) into `Boot::seed` with [`SubagentRuntime::seed_into`]; every
//! child is booted with its own runtime one level down, so grandchildren work the same way.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use aho_approval::{Approval, ApprovalHandle, NeverApproval};
use aho_cap::*;
use aho_config::{ConfigKey, Layer, Loadout, Origin, Sourced, Ty, Where};
use aho_log::{CapState, CustomFolds, Entry, EntryKind, JsonlLog, LogStore, MemoryLog, Transition};
use aho_policy::{Operator, SandboxPolicy};
use aho_session::{Boot, Session};
use aho_subagent::{ChildInfo, Features, Mode, Result_, SpawnRequest, SubagentError, SubagentProvider};
use aho_types::*;
use serde_json::json;
use tokio::sync::watch;

/// Capability id; also `Subagent/Started.provider` and `Subagent/Descriptor.provider`.
pub const PROVIDER: &str = "subagent-inproc";
pub const DEFAULT_MAX_DEPTH: u32 = 3;
/// `Result_::diagnostic` bound (bytes).
pub const DIAGNOSTIC_MAX: usize = 4096;
/// Handle kind of the per-session [`SubagentRuntime`] the embedder seeds.
pub const RUNTIME_HANDLE: HandleKind = HandleKind::Custom("subagent-runtime");

/// Mints the operator principal for a child session. Only the embedder that authenticated the
/// operator can build one (`aho_policy::Operator` is not `Clone`).
pub type OperatorFactory = Arc<dyn Fn() -> Operator + Send + Sync>;

/// Everything a session needs to boot its own children. Seeded by the embedder for the root
/// session; derived one level down for every child.
#[derive(Clone)]
pub struct SubagentRuntime {
    pub composition: Composition,
    pub plan: Plan,
    pub loadout: Loadout,
    /// Bootstrap entries of the root session (config, activation, plan seal). A spawned child's
    /// log repeats them, minus the parent's policy epoch, so its fold is self-contained.
    pub bootstrap: Vec<EntryKind>,
    /// Handles/services every child session is booted with (model routes the embedder owns).
    pub seed: Registry,
    pub operator: OperatorFactory,
    pub spill_dir: PathBuf,
    /// `Some(dir)`: child logs live at `<dir>/<parent>/<child>.jsonl`; `None`: in memory.
    pub session_dir: Option<PathBuf>,
    /// The session this runtime belongs to: the parent of every child it starts.
    pub session: SessionId,
    /// That session's log (the fork source).
    pub log: Arc<dyn LogStore>,
    /// That session's delegation depth; the root is 0.
    pub depth: u32,
}

impl Handle for SubagentRuntime {
    const KIND: HandleKind = RUNTIME_HANDLE;
}

impl SubagentRuntime {
    /// Puts this runtime into a session's seed registry so `subagent-inproc` can resolve it.
    pub fn seed_into(self, reg: &mut Registry) {
        reg.handles.insert(RUNTIME_HANDLE, (SubagentInproc::ID_, Arc::new(self)));
    }

    fn for_child(&self, child: SessionId, log: Arc<dyn LogStore>, depth: u32) -> SubagentRuntime {
        SubagentRuntime { session: child, log, depth, ..self.clone() }
    }
}

// ───────────────────────────── capability ─────────────────────────────

pub struct InprocDeps {
    pub runtime: SubagentRuntime,
    pub approval: ApprovalHandle,
}

impl Deps for InprocDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Approval, RUNTIME_HANDLE];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(InprocDeps { runtime: r.handle()?, approval: r.handle()? })
    }
}

#[derive(Clone, Debug)]
pub struct InprocConfig {
    pub max_depth: u32,
}

impl Config for InprocConfig {
    const PREFIX: &'static str = "subagent";
    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new(
            "subagent.max_depth",
            Ty::Int,
            Some(json!(DEFAULT_MAX_DEPTH)),
            "Deepest delegation allowed: the root session is depth 0, its children 1, … A spawn past it is refused.",
        )]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let raw = cfg_u64(l, "subagent.max_depth", u64::from(DEFAULT_MAX_DEPTH));
        let max_depth = u32::try_from(raw)
            .map_err(|_| ConfigError::Type { key: "subagent.max_depth".into(), expected: "a small non-negative integer" })?;
        Ok(InprocConfig { max_depth })
    }
}

/// The capability: registers the provider as the `subagent` service (and itself, concretely, as
/// `subagent-inproc` for embedders that want child sessions or log paths).
pub struct SubagentInproc {
    provider: Arc<InProc>,
}

impl Capability for SubagentInproc {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = InprocDeps;
    type Config = InprocConfig;

    fn build(deps: InprocDeps, cfg: InprocConfig) -> Result<Self, BuildError> {
        Ok(SubagentInproc { provider: Arc::new(InProc::new(deps.runtime, deps.approval.0, cfg.max_depth)) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let dynp: Arc<dyn SubagentProvider> = self.provider.clone();
        r.service(aho_subagent::SERVICE, dynp);
        r.service(PROVIDER, self.provider.clone());
    }
}

declare_capability!(
    SubagentInproc,
    "subagent-inproc",
    kind = Kind::Provider,
    provides = [aho_subagent::SEAM],
    requires = [],
    tools = &[],
    entry_kinds = ["Subagent/Descriptor", "Subagent/Started", "Subagent/Ended"],
    doc = "In-process subagents: each child is a Session over its own log under parent ⊓ declaration; forks inherit completed turns only; depth capped by subagent.max_depth."
);

// ───────────────────────────── provider ─────────────────────────────

#[derive(Clone, Default)]
struct Run {
    /// Number of runs started; 0 = never started.
    runs: u64,
    /// `None` while the current run is going.
    result: Option<Result_>,
}

struct Child {
    id: SessionId,
    session: Arc<Session>,
    label: String,
    depth: u32,
    mode: Mode,
    log_path: Option<PathBuf>,
    state: watch::Sender<Run>,
    interrupted: AtomicBool,
}

impl Child {
    fn running(&self) -> bool {
        let r = self.state.borrow();
        r.runs > 0 && r.result.is_none()
    }
    fn info(&self) -> ChildInfo {
        let r = self.state.borrow();
        ChildInfo {
            child: self.id,
            provider: PROVIDER.into(),
            label: self.label.clone(),
            depth: self.depth,
            running: r.runs > 0 && r.result.is_none(),
            last_stop: r.result.as_ref().map(|x| x.stop),
        }
    }
}

/// One session's in-process subagent provider.
pub struct InProc {
    rt: SubagentRuntime,
    parent_approval: Arc<dyn Approval>,
    max_depth: u32,
    children: Mutex<Vec<Arc<Child>>>,
    seq: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn provider_err(e: impl std::fmt::Display) -> SubagentError {
    SubagentError::Provider(e.to_string())
}

impl InProc {
    pub fn new(rt: SubagentRuntime, parent_approval: Arc<dyn Approval>, max_depth: u32) -> InProc {
        InProc { rt, parent_approval, max_depth, children: Mutex::new(Vec::new()), seq: AtomicU64::new(0) }
    }

    /// The live child session, e.g. for a surface that wants to show it.
    pub fn child_session(&self, child: SessionId) -> Option<Arc<Session>> {
        self.find(child).map(|c| c.session.clone())
    }

    /// The child's JSONL log path (`None` for in-memory children or unknown ids).
    pub fn child_log_path(&self, child: SessionId) -> Option<PathBuf> {
        self.find(child).and_then(|c| c.log_path.clone())
    }

    fn find(&self, child: SessionId) -> Option<Arc<Child>> {
        lock(&self.children).iter().find(|c| c.id == child).cloned()
    }

    fn get(&self, child: SessionId) -> Result<Arc<Child>, SubagentError> {
        self.find(child).ok_or_else(|| {
            SubagentError::Provider(format!("there is no subagent {} in this session; list_agents shows the live ones.", child.0))
        })
    }

    fn secrets(&self) -> Vec<String> {
        self.rt.loadout.secrets.values().map(|s| s.expose().to_string()).filter(|s| !s.is_empty()).collect()
    }

    /// Fresh child id, kept below 2^53 so it stays exact wherever a model or SDK reads it as a number.
    fn next_id(&self) -> SessionId {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let h = Hash::of_json(&(self.rt.session.0.to_string(), n, Timestamp::now().0));
        let mut b = [0u8; 8];
        b.copy_from_slice(&h.0[..8]);
        let v = u64::from_le_bytes(b) & ((1u64 << 53) - 1);
        SessionId(u128::from(v.max(1)))
    }

    /// Starts a run unless one is going; returns whether it started.
    fn start_run(&self, child: &Arc<Child>, text: String) -> bool {
        let started = child.state.send_if_modified(|r| {
            if r.runs == 0 || r.result.is_some() {
                r.runs += 1;
                r.result = None;
                true
            } else {
                false
            }
        });
        if !started {
            return false;
        }
        child.interrupted.store(false, Ordering::SeqCst);
        let c = child.clone();
        let secrets = self.secrets();
        tokio::spawn(async move {
            let from = c.session.log().head();
            let outcome = c.session.prompt(text, None).await.map_err(|e| e.to_string());
            let result = summarize(&c, from, outcome, &secrets);
            c.state.send_modify(|r| r.result = Some(result));
        });
        true
    }

    async fn spawn_inner(&self, req: SpawnRequest, policy: SandboxPolicy) -> Result<SessionId, SubagentError> {
        let depth = aho_subagent::depth(self.rt.depth.saturating_add(1), req.depth);
        if depth > self.max_depth {
            return Err(SubagentError::DepthLimit(self.max_depth));
        }
        if req.prompt.trim().is_empty() {
            return Err(SubagentError::Provider("the subagent prompt is empty; nothing was started. Say what the child should do.".into()));
        }
        let (plan, loadout) =
            child_plan(&self.rt.plan, &self.rt.loadout, &self.rt.composition.manifests(), &req.tool_filter, req.model.as_deref())?;
        let prefix = match req.fork_at {
            Some(at) => Some(fork_prefix(&*self.rt.log, at)?),
            None => None,
        };
        // Approvals granted inside the parent's open turn are not inherited: the open turn is not.
        let policy = match &prefix {
            Some(p) if p.epoch.0 > 0 => policy.narrow(p.allowed),
            _ => policy,
        };

        let id = self.next_id();
        let (log, log_path): (Arc<dyn LogStore>, Option<PathBuf>) = match &self.rt.session_dir {
            Some(dir) => {
                let p = dir.join(self.rt.session.0.to_string()).join(format!("{}.jsonl", id.0));
                let l = JsonlLog::open(&p)
                    .map_err(|e| provider_err(format!("could not create the child log {} ({e}); nothing was started", p.display())))?;
                (Arc::new(l), Some(p))
            }
            None => (Arc::new(MemoryLog::new()), None),
        };
        seed_child_log(&*log, &self.rt, &req, &policy, depth, prefix.as_ref())?;

        // An async child cannot talk to a human unless bubbled; a foreground child asks
        // through the parent's own approval channel.
        let approval: Arc<dyn Approval> = if !req.is_async || req.bubble { self.parent_approval.clone() } else { Arc::new(NeverApproval) };
        let mut seed = self.rt.seed.clone();
        self.rt.for_child(id, log.clone(), depth).seed_into(&mut seed);
        let session = Session::boot(Boot {
            id,
            composition: &self.rt.composition,
            plan: &plan,
            loadout: &loadout,
            bootstrap: vec![],
            log,
            approval,
            operator: (self.rt.operator)(),
            workspace: policy.workspace().to_string(),
            home: policy.home().to_string(),
            spill_dir: self.rt.spill_dir.clone(),
            seed,
        })
        .await
        .map_err(|e| provider_err(format!("the child session could not start: {e}")))?;

        let child = Arc::new(Child {
            id,
            session,
            label: req.label.clone(),
            depth,
            mode: req.mode,
            log_path,
            state: watch::channel(Run::default()).0,
            interrupted: AtomicBool::new(false),
        });
        lock(&self.children).push(child.clone());
        self.start_run(&child, req.prompt);
        Ok(id)
    }
}

impl SubagentProvider for InProc {
    fn features(&self) -> Features {
        Features { fork: true, persona: true, tool_filter: true, output_schema: false, max_depth: self.max_depth }
    }

    fn fork_point(&self) -> Option<EntryId> {
        fork_point(&*self.rt.log)
    }

    fn spawn(&self, req: SpawnRequest, policy: SandboxPolicy) -> BoxFuture<'_, Result<SessionId, SubagentError>> {
        Box::pin(self.spawn_inner(req, policy))
    }

    fn send(&self, child: SessionId, text: String) -> BoxFuture<'_, Result<(), SubagentError>> {
        Box::pin(async move {
            let c = self.get(child)?;
            if !self.start_run(&c, text.clone()) {
                // A run is going: the text joins it at the next request boundary.
                c.session.steer(text, false);
            }
            Ok(())
        })
    }

    fn interrupt(&self, child: SessionId) -> BoxFuture<'_, Result<(), SubagentError>> {
        Box::pin(async move {
            let c = self.get(child)?;
            if c.running() {
                c.interrupted.store(true, Ordering::SeqCst);
                // `abort` ends the child's turn with no further model request; a steer here would
                // sit queued and reach the child's next run as a stale instruction.
                c.session.abort();
            }
            Ok(())
        })
    }

    fn wait(&self, child: SessionId) -> BoxFuture<'_, Result<Result_, SubagentError>> {
        Box::pin(async move {
            let c = self.get(child)?;
            let mut rx = c.state.subscribe();
            let done = rx.wait_for(|r| r.runs > 0 && r.result.is_some()).await.map(|r| r.result.clone());
            let result = done.ok().flatten().ok_or_else(|| provider_err(format!("subagent {} went away before it finished", child.0)))?;
            if c.mode == Mode::OneShot {
                lock(&self.children).retain(|x| x.id != child);
            }
            Ok(result)
        })
    }

    fn child_info(&self, child: SessionId) -> Option<ChildInfo> {
        self.find(child).map(|c| c.info())
    }

    fn children(&self) -> Vec<ChildInfo> {
        lock(&self.children).iter().map(|c| c.info()).collect()
    }
}

// ───────────────────────────── log seeding ─────────────────────────────

struct Prefix {
    entries: Vec<Entry>,
    allowed: ConsequenceSet,
    epoch: Epoch,
}

fn balanced(s: &aho_log::State) -> bool {
    s.turn.is_none() && s.open_calls.is_empty() && s.open_ops.is_empty()
}

/// The parent's fork point: the last `Turn/End` on the head branch at which no turn, call, or
/// operation is open. The in-flight turn is unbalanced and never inherited.
pub fn fork_point(log: &dyn LogStore) -> Option<EntryId> {
    let entries = log.read_branch(log.head()?).ok()?;
    let folds = CustomFolds::new();
    let mut s = aho_log::State::default();
    let mut last = None;
    for e in &entries {
        s = aho_log::step(s, e, &folds).ok()?;
        if matches!(e.kind, EntryKind::TurnEnd { .. }) && balanced(&s) {
            last = Some(e.id);
        }
    }
    last
}

fn fork_prefix(log: &dyn LogStore, at: EntryId) -> Result<Prefix, SubagentError> {
    let head =
        log.head().ok_or_else(|| provider_err("the parent log is empty; there is nothing to fork. Use `subagent` for a fresh child"))?;
    let entries = log.read_branch(head).map_err(|e| provider_err(format!("the parent log cannot be read ({e}); nothing was started")))?;
    let idx = entries
        .iter()
        .position(|e| e.id == at)
        .ok_or_else(|| provider_err(format!("fork point {} is not on the parent's current branch; nothing was started", at.0)))?;
    if !matches!(entries[idx].kind, EntryKind::TurnEnd { .. }) {
        return Err(provider_err(format!(
            "fork point {} is not a Turn/End: a fork inherits completed turns only; nothing was started",
            at.0
        )));
    }
    let prefix = entries[..=idx].to_vec();
    let state = aho_log::fold(&prefix, &CustomFolds::new())
        .map_err(|e| provider_err(format!("the parent log does not fold up to the fork point ({e}); nothing was started")))?;
    if !balanced(&state) {
        return Err(provider_err(format!(
            "the parent still had calls or operations open at entry {}; a fork needs a completed turn; nothing was started",
            at.0
        )));
    }
    Ok(Prefix { entries: prefix, allowed: state.policy.allowed, epoch: state.policy.epoch })
}

/// Descriptor first; then (fork) the parent's completed-turn prefix, re-appended in order, or
/// (spawn) the root bootstrap minus its policy epoch; then the delegation epoch; then the persona.
fn seed_child_log(
    log: &dyn LogStore,
    rt: &SubagentRuntime,
    req: &SpawnRequest,
    policy: &SandboxPolicy,
    depth: u32,
    prefix: Option<&Prefix>,
) -> Result<(), SubagentError> {
    let io = |e: aho_log::LogError| provider_err(format!("could not write the child log ({e}); nothing was started"));
    let delegation = Principal::system("delegation");
    log.append(
        None,
        delegation.clone(),
        EntryKind::SubagentDescriptor {
            provider: PROVIDER.into(),
            persona: req.persona.clone(),
            tool_filter: req.tool_filter.clone(),
            parent: rt.session,
            depth,
        },
    )
    .map_err(io)?;
    let mut epoch = policy.epoch().0;
    match prefix {
        Some(p) => {
            for e in &p.entries {
                log.append(None, e.by.clone(), e.kind.clone()).map_err(io)?;
            }
            epoch = epoch.max(p.epoch.0);
        }
        None => {
            for k in rt.bootstrap.iter().filter(|k| !matches!(k, EntryKind::PolicyEpoch { .. })) {
                log.append(None, Principal::system("bootstrap"), k.clone()).map_err(io)?;
            }
        }
    }
    log.append(
        None,
        delegation.clone(),
        EntryKind::PolicyEpoch {
            epoch: Epoch(epoch.max(1)),
            allowed: policy.allowed(),
            preset: policy.preset().name().to_string(),
            approval_ref: None,
        },
    )
    .map_err(io)?;
    if let Some(persona) = req.persona.as_ref().filter(|p| !p.trim().is_empty()) {
        log.append(None, delegation, EntryKind::UserMessage { form: Form::Instructions, text: persona.clone(), client_key: None })
            .map_err(io)?;
    }
    Ok(())
}

/// Depth recorded in a child log's descriptor: an embedder resuming a child must pass
/// `max(this, inherited)` so a resumed child cannot reset to 0.
pub fn descriptor_depth(log: &dyn LogStore) -> Option<u32> {
    log.read_from(None).into_iter().find_map(|e| match e.kind {
        EntryKind::SubagentDescriptor { depth, .. } => Some(depth),
        _ => None,
    })
}

// ───────────────────────────── child plan ─────────────────────────────

fn override_key(l: &mut Loadout, key: &str, value: Value, why: &str) {
    let winner = Origin::new(Layer::Runtime(why.to_string()), Where::Const);
    let shadowed = match l.config.remove(key) {
        Some(prev) => {
            let mut s = vec![(prev.winner, prev.value)];
            s.extend(prev.shadowed);
            s
        }
        None => Vec::new(),
    };
    l.config.insert(key.to_string(), Sourced { value, winner, shadowed });
}

/// The child's plan and loadout. A tool filter hides every capability none of whose tools it
/// names (its row is Disabled, so it never starts) and denies the unnamed tools of the rest
/// with `deny <tool>` rules; runtime-discovered (`mcp__`) tools cannot be named, so filtered
/// children get no MCP provider. Unknown tool names are refused, never ignored.
pub fn child_plan(
    plan: &Plan,
    loadout: &Loadout,
    manifests: &[&'static Manifest],
    tool_filter: &[String],
    model: Option<&str>,
) -> Result<(Plan, Loadout), SubagentError> {
    let mut plan = plan.clone();
    let mut loadout = loadout.clone();
    if let Some(m) = model {
        override_key(&mut loadout, "model.name", json!(m), "subagent model");
        // The parent's catalog limits were for its model; the child's log shows the child's.
        aho_llm::models::apply_catalog(&mut loadout);
    }
    if tool_filter.is_empty() {
        return Ok((plan, loadout));
    }
    if tool_filter.iter().any(|t| t.starts_with("mcp__")) {
        return Err(SubagentError::Unsupported("a tool filter naming runtime-discovered mcp__ tools"));
    }
    let wanted: BTreeSet<&str> = tool_filter.iter().map(String::as_str).collect();
    let by_name: BTreeMap<&str, &'static Manifest> = manifests.iter().map(|m| (m.id.name, *m)).collect();
    let known: BTreeSet<&str> = plan
        .rows
        .iter()
        .filter(|r| matches!(r.state, CapState::Active))
        .filter_map(|r| by_name.get(r.id.name))
        .flat_map(|m| m.tools.iter().map(|t| t.name))
        .collect();
    let unknown: Vec<&str> = wanted.iter().filter(|w| !known.contains(*w)).copied().collect();
    if !unknown.is_empty() {
        return Err(provider_err(format!(
            "the tool filter names tools this session does not have ({}); nothing was started. Available: {}",
            unknown.join(", "),
            known.iter().copied().collect::<Vec<_>>().join(", ")
        )));
    }
    let mut denies: Vec<Value> = Vec::new();
    for row in plan.rows.iter_mut().filter(|r| matches!(r.state, CapState::Active)) {
        let Some(m) = by_name.get(row.id.name) else { continue };
        let mcp = m.provides.iter().any(|s| s.0 == "mcp");
        if m.tools.is_empty() && !mcp {
            continue;
        }
        if !m.tools.iter().any(|t| wanted.contains(t.name)) {
            row.state = CapState::Disabled { why: "not in the subagent's tool filter".into() };
            continue;
        }
        denies.extend(m.tools.iter().filter(|t| !wanted.contains(t.name)).map(|t| Value::String(format!("deny {}", t.name))));
    }
    if !denies.is_empty() {
        let mut rules: Vec<Value> = loadout.get("policy.rules").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        rules.extend(denies);
        override_key(&mut loadout, "policy.rules", Value::Array(rules), "subagent tool filter");
    }
    plan.hash = Hash::of_json(&(plan.hash, tool_filter));
    Ok((plan, loadout))
}

// ───────────────────────────── results ─────────────────────────────

/// Maps how the child's turn ended onto the seam's closed `StopReason`, with an operator-voice
/// diagnostic for anything but a normal finish.
pub fn stop_reason(outcome: Result<&Transition, &str>, last_stop: Option<StopReason>, interrupted: bool) -> (StopReason, Option<String>) {
    match outcome {
        Err(e) => (StopReason::Error, Some(format!("The child session failed: {e}"))),
        Ok(_) if interrupted => (StopReason::Aborted, Some("The parent interrupted this subagent; its last message is what it had when it stopped.".into())),
        Ok(Transition::Completed) => (
            match last_stop {
                Some(s @ (StopReason::Refusal | StopReason::MaxTokens)) => s,
                _ => StopReason::EndTurn,
            },
            None,
        ),
        Ok(Transition::MaxTurns) => (
            StopReason::Aborted,
            Some("The child used its whole step budget (`session.max_turns`) without finishing. Give it a smaller task or raise `session.max_turns`.".into()),
        ),
        Ok(Transition::PromptTooLong) => {
            (StopReason::Error, Some("The child's conversation no longer fits the model's context window, even after compaction.".into()))
        }
        Ok(Transition::Aborted(r)) => (StopReason::Error, Some(format!("The child's turn stopped with an error: {r}"))),
        Ok(t) => (StopReason::Error, Some(format!("The child stopped at an unexpected step transition ({t:?}); its log has the details."))),
    }
}

/// Secret-free and at most [`DIAGNOSTIC_MAX`] bytes, cut on a character boundary.
pub fn diagnostic(text: &str, secrets: &[String]) -> String {
    clip(&redact(text, secrets), DIAGNOSTIC_MAX)
}

fn redact(text: &str, secrets: &[String]) -> String {
    secrets.iter().filter(|s| !s.is_empty()).fold(text.to_string(), |t, s| t.replace(s.as_str(), "[redacted]"))
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mark = "…";
    let mut end = max.saturating_sub(mark.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{mark}", &s[..end])
}

fn text_of(content: &[ContentBlock]) -> String {
    content.iter().filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("")
}

/// The run's result: the last non-empty assistant text since `from`, the mapped stop reason,
/// and a bounded, secret-free diagnostic.
fn summarize(c: &Child, from: Option<EntryId>, outcome: Result<Transition, String>, secrets: &[String]) -> Result_ {
    let mut last_message = None;
    let mut last_stop = None;
    for e in c.session.log().read_from(from) {
        if let EntryKind::AssistantMessage { content, stop, .. } = &e.kind {
            last_stop = Some(*stop);
            let t = text_of(content);
            if !t.trim().is_empty() {
                last_message = Some(t.trim().to_string());
            }
        }
    }
    let (stop, diag) = stop_reason(outcome.as_ref().map_err(String::as_str), last_stop, c.interrupted.load(Ordering::SeqCst));
    Result_ {
        child: c.id,
        stop,
        last_message: last_message.map(|m| redact(&m, secrets)),
        diagnostic: diag.map(|d| diagnostic(&d, secrets)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_is_bounded_on_a_char_boundary_and_secret_free() {
        let long = format!("key sk-live-123 then {}", "é".repeat(5000));
        let d = diagnostic(&long, &["sk-live-123".to_string()]);
        assert!(d.len() <= DIAGNOSTIC_MAX);
        assert!(d.starts_with("key [redacted] then"));
        assert!(d.ends_with('…'));
        assert_eq!(diagnostic("short", &[]), "short");
    }

    #[test]
    fn stop_reasons_map_every_transition() {
        assert_eq!(stop_reason(Ok(&Transition::Completed), Some(StopReason::EndTurn), false), (StopReason::EndTurn, None));
        assert_eq!(stop_reason(Ok(&Transition::Completed), Some(StopReason::Refusal), false).0, StopReason::Refusal);
        assert_eq!(stop_reason(Ok(&Transition::Completed), None, true).0, StopReason::Aborted);
        let (s, d) = stop_reason(Ok(&Transition::MaxTurns), None, false);
        assert_eq!(s, StopReason::Aborted);
        assert!(d.unwrap_or_default().contains("session.max_turns"));
        let (s, d) = stop_reason(Ok(&Transition::Aborted("model call failed: boom".into())), None, false);
        assert_eq!(s, StopReason::Error);
        assert!(d.unwrap_or_default().contains("boom"));
        assert_eq!(stop_reason(Err("no route"), None, false).0, StopReason::Error);
    }
}
