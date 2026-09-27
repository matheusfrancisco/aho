//! One loop. `core::next` is the pure "when do we call the model" rule set; this
//! module is the tokio driver that executes it: requests, async-first tool calls with a grace
//! window and heartbeat, the decision chain + approvals, spill, compaction, checkpoints,
//! rewind, resume, and the `Session` API every surface drives.

#![forbid(unsafe_code)]

mod ceiling;
pub mod core;
mod maintenance;
mod tools;

pub use maintenance::{COMPACTION_PROMPT, compaction_range};
pub use tools::tool_events;
use tools::{LoggedApproval, Started};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_approval::{Approval, AskPolicy};
use aho_cap::{
    CancelToken, Composition, ExecOutcome, ExecRequest, ExecView, HookEvent, Plan, PolicyView, PostDecision, PreDecision, Registry,
    ToolDecl, ToolExec,
};
use aho_config::Loadout;
use aho_fs::SnapshotHandle;
use aho_llm::{LateResults, Llm, LlmError, LlmRoute, Request, StreamEvent};
use aho_log::{
    ApprovalOutcome, CancelReason, CheckpointCause, CustomFolds, Entry, EntryKind, GrantScope, LogError, LogStore, OpStatus, RewindMode,
    State, ToolOutcome, Transition,
};
use aho_policy::{CallUnderReview, ChainOutcome, DenialCounter, Operator, Rule, SandboxPolicy};
use aho_proc::{OpTable, OperationsHandle, SpawnSpec, SubprocessHandle};
use aho_types::*;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};

use crate::core::{Next, Pending, Wake};

// ───────────────────────────── config ─────────────────────────────

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub provider: String,
    pub model: String,
    pub max_tokens: u32,
    /// `Some`: this catalogued model supports reasoning and `model.thinking` is on.
    pub reasoning: Option<aho_llm::Reasoning>,
    pub grace_ms: u64,
    pub heartbeat_s: u64,
    pub max_parallel: usize,
    pub max_steps: u64,
    pub spill_budget: usize,
    pub spill_dir: PathBuf,
    pub context_window: u64,
    pub compaction_threshold: f64,
    pub compaction_retain: f64,
    pub checkpoint_per_call: bool,
}

impl SessionConfig {
    pub fn from_loadout(l: &Loadout, spill_dir: PathBuf) -> Self {
        let u = |k: &str, d: u64| l.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
        let f = |k: &str, d: f64| l.get(k).and_then(|v| v.as_f64()).unwrap_or(d);
        let provider = l.str("model.provider").unwrap_or("openai").to_string();
        let model = l.str("model.name").unwrap_or("gpt-4.1-mini").to_string();
        let reasoning = (l.get("model.thinking").and_then(|v| v.as_bool()).unwrap_or(true)
            && aho_llm::models::lookup(&provider, &model).is_some_and(|m| m.reasoning))
        .then(|| aho_llm::Reasoning { budget_tokens: u("model.thinking_budget", 4096) as u32 });
        SessionConfig {
            provider,
            model,
            max_tokens: u("model.max_tokens", 8192) as u32,
            reasoning,
            grace_ms: u("tools.grace_ms", 1000),
            heartbeat_s: u("tools.heartbeat_s", 600),
            max_parallel: u("tools.max_parallel", 10) as usize,
            max_steps: u("session.max_turns", 200),
            spill_budget: u("tools.spill_budget", 200_000) as usize,
            spill_dir,
            context_window: u("model.context_window", 128_000),
            compaction_threshold: f("compaction.threshold", 0.8),
            compaction_retain: f("compaction.retain", 0.16),
            checkpoint_per_call: l.str("checkpoint.granularity").unwrap_or("call") != "turn",
        }
    }
}

// ───────────────────────────── events & errors ─────────────────────────────

/// Live events for surfaces. Stream chunks are never durable; `Entry` mirrors the log.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    Stream(StreamEvent),
    Entry(Box<Entry>),
    Idle { turn: TurnId, transition: Transition },
    Error(String),
}

#[derive(Debug)]
pub enum SessionError {
    Log(LogError),
    Llm(LlmError),
    Boot(String),
    Busy,
    NotFound(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Log(e) => write!(f, "{e}"),
            SessionError::Llm(e) => write!(f, "model call failed: {e}"),
            SessionError::Boot(e) => write!(f, "session could not start: {e}"),
            SessionError::Busy => write!(f, "a turn is already running in this session; queue input with steer/follow_up"),
            SessionError::NotFound(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<LogError> for SessionError {
    fn from(e: LogError) -> Self {
        SessionError::Log(e)
    }
}

/// Input queued while a turn runs (soft interrupts).
#[derive(Clone, Debug)]
pub struct Input {
    pub by: Principal,
    pub form: Form,
    pub text: String,
    pub urgent: bool,
    pub client_key: Option<String>,
}

struct Finished {
    call: CallId,
    op: OpId,
    outcome: ExecOutcome,
    post_context: Vec<String>,
}

// ───────────────────────────── session ─────────────────────────────

pub struct Session {
    pub id: SessionId,
    log: Arc<dyn LogStore>,
    reg: Registry,
    llm: Arc<dyn Llm>,
    late: LateResults,
    policy: Mutex<SandboxPolicy>,
    rules: Vec<Rule>,
    approval: Arc<dyn Approval>,
    operator: Operator,
    ops: OpTable,
    cfg: SessionConfig,
    plan_hash: Hash,
    events: broadcast::Sender<SessionEvent>,
    inbox_tx: mpsc::UnboundedSender<Input>,
    inbox_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Input>>,
    results_tx: mpsc::UnboundedSender<Finished>,
    results_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Finished>>,
    folds: CustomFolds,
    denials: Mutex<DenialCounter>,
    turn_cancel: Mutex<CancelToken>,
    last_snapshots: Mutex<BTreeMap<String, String>>,
    last_sections: Mutex<Vec<Hash>>,
    compaction_failures: Mutex<u32>,
    running: Arc<tokio::sync::Mutex<()>>,
    /// Capabilities the operator allowed past the preset (`caps.<id>.allow = true`).
    allowed_caps: BTreeSet<String>,
    /// Sequence for nested (PTC) call ids: `<parent call>.ptc<n>`.
    ptc_seq: std::sync::atomic::AtomicU64,
}

/// Service `aho.tool_call` (PTC): `(parent call's view, cancel, tool, args) → outcome`, one
/// nested call through this session's lookup, decision chain, approvals and pre/post hooks.
pub type NestedToolCall = Arc<dyn Fn(ExecView, CancelToken, String, Value) -> BoxFuture<'static, ExecOutcome> + Send + Sync>;

/// Inputs to `Session::boot`; everything comes from `aho-compose` + the surface.
pub struct Boot<'a> {
    pub id: SessionId,
    pub composition: &'a Composition,
    pub plan: &'a Plan,
    pub loadout: &'a Loadout,
    /// Entries 0..k from `aho_compose::bootstrap_entries`; appended only when the log is new.
    pub bootstrap: Vec<EntryKind>,
    pub log: Arc<dyn LogStore>,
    pub approval: Arc<dyn Approval>,
    pub operator: Operator,
    pub workspace: String,
    pub home: String,
    pub spill_dir: PathBuf,
    /// Extra handles/services the embedder provides (e.g. a scripted model in tests).
    pub seed: Registry,
}

fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Empty replies (no text, no calls) nudged per turn before the turn is left to end.
const EMPTY_STOP_RETRIES: u32 = 2;

impl Session {
    /// Starts the composition's capabilities, opens (or resumes) the log, and returns a live session.
    pub async fn boot(b: Boot<'_>) -> Result<Arc<Session>, SessionError> {
        let ops = OpTable::default();
        let mut seed = b.seed;
        seed.handles.insert(
            aho_cap::HandleKind::Operations,
            (aho_cap::CapabilityId { name: "aho-session", major: 0 }, Arc::new(OperationsHandle(ops.clone()))),
        );
        // Tools that ask (invariant_add, task_control, …) go through a wrapper that logs the pair.
        let logged: Arc<dyn Approval> = Arc::new(LoggedApproval { inner: b.approval.clone(), log: b.log.clone() });
        seed.handles.insert(
            aho_cap::HandleKind::Approval,
            (aho_cap::CapabilityId { name: "aho-session", major: 0 }, Arc::new(aho_approval::ApprovalHandle(logged))),
        );
        // Read-only projections the inspect tools serve to the agent (the same data the operator sees).
        seed.services.insert("aho.plan".into(), Arc::new(Arc::new(b.plan.clone())));
        seed.services.insert("aho.manifests".into(), Arc::new(Arc::new(b.composition.manifests())));
        // The session's own log, read-only by type use (`session_search`/`session_trace`): tools read what the
        // model already saw plus what the fold derives; appends still go through `value.events` (`tool_events`).
        seed.services.insert("aho.log".into(), Arc::new(b.log.clone()));
        seed.services.insert("aho.session_id".into(), Arc::new(b.id));
        seed.services.insert("aho.session_dir".into(), Arc::new(b.spill_dir.parent().unwrap_or(std::path::Path::new(".")).to_path_buf()));
        seed.services.insert("aho.workspace".into(), Arc::new(b.workspace.clone()));
        seed.services.insert("aho.home".into(), Arc::new(b.home.clone()));
        // PTC: `run_code` programs call tools back through this session. Capabilities build
        // before the session exists, so the slot is seeded empty and filled once it does.
        let nested: Arc<std::sync::OnceLock<NestedToolCall>> = Arc::default();
        seed.services.insert("aho.tool_call".into(), Arc::new(nested.clone()));
        // Session modes (plan mode) narrow the live policy and ask the operator to widen it back.
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Session>>> = Arc::default();
        seed.services
            .insert(aho_control::CEILING_SERVICE.into(), Arc::new(aho_control::CeilingHandle(Arc::new(ceiling::Ceiling(me.clone())))));
        let (reg, broken) = b.composition.start_seeded(b.plan, b.loadout, seed);

        let cfg = SessionConfig::from_loadout(b.loadout, b.spill_dir);
        let route = reg
            .service::<LlmRoute>(&format!("llm:{}", cfg.provider))
            .ok_or_else(|| SessionError::Boot(format!(
                "no model route `llm:{}` is active. Run `aho tree` to see why its adapter is locked, or set `model.provider` to an active route",
                cfg.provider
            )))?;
        let late = route.0.info().late_results;

        let rules_src: Vec<(String, String)> = b
            .loadout
            .get("policy.rules")
            .and_then(|v| v.as_array())
            .map(|a| {
                let origin = b.loadout.config.get("policy.rules").map(|s| s.winner.render()).unwrap_or_default();
                a.iter().filter_map(|x| x.as_str()).map(|s| (s.to_string(), origin.clone())).collect()
            })
            .unwrap_or_default();
        let rules = aho_policy::parse_rules(&rules_src).map_err(SessionError::Boot)?;

        let fresh = b.log.head().is_none();
        if fresh {
            for k in b.bootstrap {
                b.log.append(None, Principal::system("bootstrap"), k)?;
            }
        }
        for (id, err) in &broken {
            b.log.append(
                None,
                Principal::system("resolver"),
                EntryKind::CapDelta {
                    id: id.name.to_string(),
                    op: aho_log::CapDeltaOp::Broken { err: err.clone() },
                    reason: "build or registration failed".into(),
                },
            )?;
        }

        let state = fold_head(&*b.log, &reg.entry_kinds)?;
        let world = b.plan.world.clone();
        // Commands and paths resolve against the bound world's own root: under a confining
        // world that is the sandbox's `/workspace` or the remote directory, not the host project,
        // and the decision chain's safety checks (step 5) must judge them there.
        let workspace = match reg.handle::<aho_fs::FsRead>() {
            Some(fs) if world.confines => fs.0.root(),
            _ => b.workspace.clone(),
        };
        let epoch = if state.policy.epoch.0 == 0 { Epoch(1) } else { state.policy.epoch };
        let allowed = if state.policy.epoch.0 == 0 { b.plan.allowed } else { state.policy.allowed };
        let preset = if state.policy.epoch.0 == 0 { &b.plan.preset } else { &state.policy.preset };
        let policy = match aho_policy::preset(preset) {
            Some(preset) => SandboxPolicy::from_preset(preset, world, workspace, b.home.clone(), epoch).narrow(allowed),
            None => SandboxPolicy::new(allowed, world, workspace, b.home.clone(), epoch),
        };

        let (events, _) = broadcast::channel(1024);
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        let folds = reg.entry_kinds.clone();
        let allowed_caps: BTreeSet<String> = b
            .loadout
            .config
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("caps.")?.strip_suffix(".allow").filter(|_| v.value == Value::Bool(true)).map(str::to_string)
            })
            .collect();
        let s = Arc::new(Session {
            id: b.id,
            log: b.log,
            reg,
            llm: route.0,
            late,
            policy: Mutex::new(policy),
            rules,
            approval: b.approval,
            operator: b.operator,
            ops,
            cfg,
            plan_hash: b.plan.hash,
            events,
            inbox_tx,
            inbox_rx: tokio::sync::Mutex::new(inbox_rx),
            results_tx,
            results_rx: tokio::sync::Mutex::new(results_rx),
            folds,
            denials: Mutex::new(DenialCounter::default()),
            turn_cancel: Mutex::new(CancelToken::new()),
            last_snapshots: Mutex::new(BTreeMap::new()),
            last_sections: Mutex::new(Vec::new()),
            compaction_failures: Mutex::new(0),
            running: Arc::new(tokio::sync::Mutex::new(())),
            allowed_caps,
            ptc_seq: std::sync::atomic::AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&s);
        nested.get_or_init(move || -> NestedToolCall {
            Arc::new(move |parent, cancel, tool, args| {
                let weak = weak.clone();
                Box::pin(async move {
                    match weak.upgrade() {
                        Some(s) => s.nested_call(parent, cancel, tool, args).await,
                        None => ExecOutcome::err("SESSION_GONE", "the session has ended, so the nested tool call did not run."),
                    }
                })
            })
        });
        me.get_or_init(|| Arc::downgrade(&s));
        s.append(
            Principal::system("session"),
            EntryKind::SessionMeta {
                title: None,
                tags: vec![],
                last_prompt: None,
                process: Some(aho_log::ProcessIdentity { pid: std::process::id(), start_time: Timestamp::now() }),
            },
        )?;
        // Crash-honest receipts: every child a tool ties to a live op is recorded under this
        // boot's Session/Meta as `Child/Spawned`, so `aho gc` can reap it once this process is gone.
        let receipts = Arc::downgrade(&s);
        s.ops.on_attach(Arc::new(move |info: &aho_proc::OpInfo| {
            if let (Some(s), Some(child)) = (receipts.upgrade(), info.identity) {
                s.append_reported(Principal::system(aho_proc::RECEIPT_BY), aho_proc::spawned(child, aho_proc::ChildKind::Job, &info.tool));
            }
        }));
        if !fresh {
            s.recover_open_ops(&state).await?;
        }
        s.run_hooks(HookEvent::SessionStart, serde_json::json!({ "session": s.id.0.to_string() })).await;
        Ok(s)
    }

    pub fn log(&self) -> &Arc<dyn LogStore> {
        &self.log
    }
    pub fn registry(&self) -> &Registry {
        &self.reg
    }
    /// The model route, model id and budgets this session runs with.
    pub fn config(&self) -> &SessionConfig {
        &self.cfg
    }
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }
    pub fn policy_view(&self) -> PolicyView {
        lk(&self.policy).view()
    }
    pub fn ops(&self) -> &OpTable {
        &self.ops
    }
    pub fn fold(&self) -> Result<State, SessionError> {
        Ok(fold_head(&*self.log, &self.folds)?)
    }
    /// Reconnect replays from the last seen id (ids are monotonic per log).
    pub fn subscribe_from(&self, from: Option<EntryId>) -> Vec<Entry> {
        self.log.read_from(from)
    }

    /// For paths that cannot return an error (compaction, hooks): a failed append is reported to
    /// every surface instead of being dropped: the log is the only store.
    fn append_reported(&self, by: Principal, kind: EntryKind) {
        if let Err(e) = self.append(by, kind) {
            let _ =
                self.events.send(SessionEvent::Error(format!("could not write to the session log ({e}); the last action is not recorded")));
        }
    }

    /// Host-side appends (daemon `session.command`, slash commands): written and broadcast like the
    /// session's own entries, so every attached surface sees them live. Capabilities never hold a
    /// `Session`; tools append only through `value.events` (`tool_events`).
    pub fn append_entry(&self, by: Principal, kind: EntryKind) -> Result<Entry, LogError> {
        self.append(by, kind)
    }

    /// Reserves the same admission lock as every turn driver. Keep it through an operator's
    /// log mutation and replacement boot; no foreground, schedule or workflow turn can start
    /// in that interval. Refuses rather than interrupting a running turn.
    pub fn try_mutation(&self) -> Result<tokio::sync::OwnedMutexGuard<()>, SessionError> {
        self.running.clone().try_lock_owned().map_err(|_| SessionError::Busy)
    }

    fn append(&self, by: Principal, kind: EntryKind) -> Result<Entry, LogError> {
        let e = self.log.append(None, by, kind)?;
        let _ = self.events.send(SessionEvent::Entry(Box::new(e.clone())));
        Ok(e)
    }

    /// Queue input for the running turn; injected at the next request boundary, never between a call and its result.
    pub fn steer(&self, text: impl Into<String>, urgent: bool) {
        let _ = self.inbox_tx.send(Input { by: Principal::Operator, form: Form::Prompt, text: text.into(), urgent, client_key: None });
    }

    /// Hard interrupt: cancels running operations of this turn.
    pub fn abort(&self) {
        lk(&self.turn_cancel).cancel();
    }

    /// Runs one turn to idle (or the step budget) and returns its final transition.
    pub async fn prompt(self: &Arc<Self>, text: impl Into<String>, client_key: Option<String>) -> Result<Transition, SessionError> {
        self.prompt_as(Principal::Operator, text, client_key).await
    }

    /// A prompt authored by someone other than the operator: a driver (`System("schedule")`,
    /// `System("workflow")`) or a surface acting for an external source. The log records who spoke.
    pub async fn prompt_as(
        self: &Arc<Self>,
        by: Principal,
        text: impl Into<String>,
        client_key: Option<String>,
    ) -> Result<Transition, SessionError> {
        let _guard = self.running.try_lock().map_err(|_| SessionError::Busy)?;
        if let Some(k) = &client_key {
            // inbox dedup by caller-supplied id, also across resume
            let seen =
                self.log.read_from(None).iter().any(|e| matches!(&e.kind, EntryKind::UserMessage { client_key: Some(c), .. } if c == k));
            if seen {
                return Ok(Transition::Completed);
            }
        }
        *lk(&self.turn_cancel) = CancelToken::new();
        let turn = TurnId(self.fold()?.turns + 1);
        self.append(Principal::system("loop"), EntryKind::TurnStart { turn })?;
        self.append(by, EntryKind::UserMessage { form: Form::Prompt, text: text.into(), client_key })?;
        let mut stop_hook_active = false;
        let transition = loop {
            let t = match self.run_turn(turn).await {
                Ok(t) => t,
                Err(e) => {
                    let _ = self.events.send(SessionEvent::Error(e.to_string()));
                    Transition::Aborted(e.to_string())
                }
            };
            let v = self
                .run_hooks(
                    HookEvent::TurnStopping,
                    serde_json::json!({ "turn": turn.0, "transition": format!("{t:?}"), "stop_hook_active": stop_hook_active }),
                )
                .await;
            // A Stop hook may keep the turn going once with its reason; never twice (recursion guard).
            match v.deny {
                Some(reason) if t == Transition::Completed && !stop_hook_active => {
                    stop_hook_active = true;
                    self.append(
                        Principal::system("hooks"),
                        EntryKind::StepEnd { step: StepId(0), transition: Transition::StopHookBlocking },
                    )?;
                    self.append(Principal::system("hooks"), EntryKind::UserMessage { form: Form::Notice, text: reason, client_key: None })?;
                }
                _ => break t,
            }
        };
        self.append(Principal::system("loop"), EntryKind::StepEnd { step: StepId(0), transition: transition.clone() })?;
        self.run_hooks(HookEvent::TurnEnd, serde_json::json!({ "turn": turn.0 })).await;
        self.checkpoint(CheckpointCause::TurnEnd).await;
        // An approval a tool is still waiting on cannot outlive the turn: cancel it first.
        for req in self.fold()?.pending_approvals.keys().copied().collect::<Vec<_>>() {
            self.append(
                Principal::system("loop"),
                EntryKind::ApprovalDecided { req, outcome: ApprovalOutcome::Cancelled, scope: GrantScope::Once },
            )?;
        }
        self.append(Principal::system("loop"), EntryKind::TurnEnd { turn })?;
        let _ = self.events.send(SessionEvent::Idle { turn, transition: transition.clone() });
        Ok(transition)
    }

    async fn run_turn(self: &Arc<Self>, turn: TurnId) -> Result<Transition, SessionError> {
        let mut results = self.results_rx.lock().await;
        let mut inbox = self.inbox_rx.lock().await;
        let mut pending = Pending::default();
        let mut queued: Vec<Input> = Vec::new();
        let mut overflow_retries = 0u32;
        let mut empty_retries = 0u32;
        // Consecutive identical replies (same visible text, same tool names + args).
        let mut loop_sig: Option<Hash> = None;
        let mut loop_run = 0u32;

        // results that landed between turns are recorded first (late results)
        while let Ok(f) = results.try_recv() {
            self.record_result(f).await?;
        }

        // `abort()` fires this token: tool ops watch it, and the loop stops between steps, mid-request
        // and while waiting; an aborted turn never calls the model again.
        let token = lk(&self.turn_cancel).clone();
        const ABORTED: &str = "the operator aborted the turn";
        loop {
            if token.is_cancelled() {
                self.settle_after_abort(&mut results).await?;
                return Ok(Transition::Aborted(ABORTED.into()));
            }
            // inject queued inputs at the request boundary
            for i in queued.drain(..) {
                self.append(i.by, EntryKind::UserMessage { form: i.form, text: i.text, client_key: i.client_key })?;
            }
            pending.inputs = 0;
            pending.urgent = 0;
            pending.results_since_request = 0;
            let step = StepId(pending.steps + 1);
            self.append(Principal::system("loop"), EntryKind::StepStart { step })?;
            let prompt = self.fold().ok().and_then(|st| {
                st.items.iter().rev().find_map(|(_, it)| match it {
                    aho_log::Item::User { form: Form::Prompt, text } => Some(text.clone()),
                    _ => None,
                })
            });
            let v = self.run_hooks(HookEvent::PreStep, serde_json::json!({ "turn": turn.0, "step": step.0, "prompt": prompt })).await;
            if let Some(reason) = v.stop.or(v.deny) {
                self.append(
                    Principal::system("hooks"),
                    EntryKind::UserMessage {
                        form: Form::Notice,
                        text: format!("A pre-step hook stopped this turn: {reason}"),
                        client_key: None,
                    },
                )?;
                return Ok(Transition::StopHookBlocking);
            }
            self.maybe_compact(false).await;
            self.log_contexts()?;
            // The next reply is the last one `session.max_turns` allows: ask for a wrap-up
            // instead of truncating silently at the budget.
            if pending.steps + 1 == self.cfg.max_steps {
                self.append(
                    Principal::system("loop"),
                    EntryKind::UserMessage {
                        form: Form::Notice,
                        text: format!(
                            "Step budget: this is step {} of {}, the last one this turn. Wrap up now: give your best final answer from what you have; new tool calls will not be awaited.",
                            pending.steps + 1,
                            self.cfg.max_steps
                        ),
                        client_key: None,
                    },
                )?;
            }

            let request = tokio::select! {
                r = self.request(turn) => r,
                _ = until_cancelled(&token) => {
                    self.append(Principal::system("model"), EntryKind::AssistantAttempt { error: ABORTED.into() })?;
                    self.append(Principal::system("loop"), EntryKind::StepEnd { step, transition: Transition::Aborted(ABORTED.into()) })?;
                    self.settle_after_abort(&mut results).await?;
                    return Ok(Transition::Aborted(ABORTED.into()));
                }
            };
            let response = match request {
                Ok(r) => r,
                Err(LlmError::ContextWindowExceeded { .. }) if overflow_retries < 2 => {
                    overflow_retries += 1;
                    self.maybe_compact(true).await;
                    self.append(Principal::system("loop"), EntryKind::StepEnd { step, transition: Transition::ReactiveCompactRetry })?;
                    continue;
                }
                Err(LlmError::RateLimited { retry_after_ms }) if overflow_retries < 3 => {
                    overflow_retries += 1;
                    tokio::time::sleep(Duration::from_millis(retry_after_ms.min(30_000))).await;
                    continue;
                }
                Err(e) => {
                    self.append(Principal::system("model"), EntryKind::AssistantAttempt { error: e.to_string() })?;
                    return Err(SessionError::Llm(e));
                }
            };
            pending.steps += 1;
            let (resp, early) = response;
            self.append(
                Principal::system("model"),
                EntryKind::AssistantMessage { content: resp.content.clone(), usage: resp.usage, stop: resp.stop },
            )?;

            let uses: Vec<(CallId, String, Value)> = resp
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolUse { id, name, args } = b { Some((id.clone(), name.clone(), args.clone())) } else { None }
                })
                .collect();
            let issued: Vec<CallId> = uses.iter().map(|u| u.0.clone()).collect();

            // Empty stop: no visible text, no tool call, model simply ended. Retrying the identical
            // request tends to repeat, so nudge with a notice and spend a bounded number of steps.
            let visible_text = resp.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if !text.trim().is_empty()));
            if issued.is_empty()
                && !visible_text
                && resp.stop == StopReason::EndTurn
                && empty_retries < EMPTY_STOP_RETRIES
                && pending.steps < self.cfg.max_steps
            {
                empty_retries += 1;
                self.append(
                    Principal::system("loop"),
                    EntryKind::UserMessage {
                        form: Form::Notice,
                        text: format!(
                            "The reply had no text and no tool call, so nothing advanced (retry {empty_retries}/{EMPTY_STOP_RETRIES}). Continue: give the final answer or make the next tool call."
                        ),
                        client_key: None,
                    },
                )?;
                self.append(Principal::system("loop"), EntryKind::StepEnd { step, transition: Transition::EmptyStopRetry })?;
                continue;
            }

            // Loop guard: a reply identical to the previous one (text and calls) makes no progress.
            // Warn at every third repeat; the step budget bounds the worst case.
            let sig = Hash::of_json(&(
                resp.content
                    .iter()
                    .filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None })
                    .collect::<Vec<_>>(),
                uses.iter().map(|(_, name, args)| (name, args)).collect::<Vec<_>>(),
            ));
            if loop_sig == Some(sig) {
                loop_run += 1;
            } else {
                loop_run = 0;
                loop_sig = Some(sig);
            }
            if loop_run >= 2 && (loop_run - 2).is_multiple_of(3) {
                self.append(
                    Principal::system("loop"),
                    EntryKind::UserMessage {
                        form: Form::Notice,
                        text: format!(
                            "Loop guard: the last {} replies were identical (same text, same tool calls), so repeating them will not progress. Change approach: a different tool or different arguments, or give your final answer.",
                            loop_run + 1
                        ),
                        client_key: None,
                    },
                )?;
            }
            self.dispatch_calls(turn, uses, early).await?;
            let state = self.fold()?;
            pending.open_calls = state.open_calls.clone();
            pending.last_stop_was_tool_use = !issued.is_empty();
            pending.grace_armed = !issued.is_empty() && issued.iter().any(|c| pending.open_calls.contains(c));
            pending.graced_all_done = !issued.iter().any(|c| pending.open_calls.contains(c));
            if !issued.is_empty() && pending.graced_all_done {
                pending.results_since_request = issued.len();
            }

            let grace = tokio::time::sleep(Duration::from_millis(if pending.grace_armed { self.cfg.grace_ms } else { 0 }));
            tokio::pin!(grace);
            let heartbeat = tokio::time::sleep(Duration::from_secs(self.cfg.heartbeat_s.max(1)));
            tokio::pin!(heartbeat);
            let mut wake = Wake::Response;
            let transition = loop {
                match core::next(&pending, wake, self.cfg.max_steps) {
                    Next::Request => {
                        self.append(
                            Principal::system("loop"),
                            EntryKind::StepEnd {
                                step,
                                transition: if pending.inputs > 0 { Transition::QueuedCommand } else { Transition::ToolUse },
                            },
                        )?;
                        break None;
                    }
                    Next::Idle => break Some(Transition::Completed),
                    Next::MaxTurns => break Some(Transition::MaxTurns),
                    Next::Heartbeat => {
                        let running: Vec<String> = self
                            .ops
                            .running()
                            .into_iter()
                            .map(|(op, call, age)| format!("{} (op {}, {}s)", call.0, op.0, age / 1000))
                            .collect();
                        self.append(
                            Principal::system("loop"),
                            EntryKind::UserMessage {
                                form: Form::Notice,
                                text: format!(
                                    "Heartbeat: waited {}s for tool calls. Running: {}",
                                    self.cfg.heartbeat_s,
                                    running.join(", ")
                                ),
                                client_key: None,
                            },
                        )?;
                        self.append(Principal::system("loop"), EntryKind::StepEnd { step, transition: Transition::WaitingOnOps })?;
                        break None;
                    }
                    Next::Wait => {}
                }
                tokio::select! {
                    Some(f) = results.recv() => {
                        let issued_by_last = issued.contains(&f.call);
                        self.record_result(f).await?;
                        pending.results_since_request += 1;
                        pending.open_calls = self.fold()?.open_calls;
                        if issued_by_last {
                            pending.graced_all_done = !issued.iter().any(|c| pending.open_calls.contains(c));
                        }
                        wake = Wake::Result;
                    }
                    Some(i) = inbox.recv() => {
                        pending.inputs += 1;
                        if i.urgent {
                            pending.urgent += 1;
                            self.skip_unstarted();
                        }
                        queued.push(i);
                        wake = Wake::Input;
                    }
                    _ = &mut grace, if pending.grace_armed => {
                        pending.grace_armed = false;
                        wake = Wake::GraceExpired;
                    }
                    _ = &mut heartbeat, if core::waiting_on_calls_only(&pending) => {
                        wake = Wake::HeartbeatDue;
                    }
                    _ = until_cancelled(&token) => {
                        self.append(Principal::system("loop"), EntryKind::StepEnd { step, transition: Transition::Aborted(ABORTED.into()) })?;
                        break Some(Transition::Aborted(ABORTED.into()));
                    }
                }
            };
            if let Some(t) = transition {
                if matches!(t, Transition::Aborted(_)) {
                    self.settle_after_abort(&mut results).await?;
                }
                return Ok(t);
            }
        }
    }

    /// After an abort, cancelled ops settle with synthetic `Cancelled` results; recording them here
    /// closes every call the aborted turn issued. Bounded: a tool that ignores cancellation stays
    /// open and its result lands late, at the next turn boundary.
    async fn settle_after_abort(&self, results: &mut mpsc::UnboundedReceiver<Finished>) -> Result<(), SessionError> {
        const SETTLE: Duration = Duration::from_secs(2);
        let deadline = tokio::time::Instant::now() + SETTLE;
        while !self.fold()?.open_calls.is_empty() {
            match tokio::time::timeout_at(deadline, results.recv()).await {
                Ok(Some(f)) => self.record_result(f).await?,
                _ => break,
            }
        }
        Ok(())
    }

    // ───────────────────────────── requests ─────────────────────────────

    fn system_sections(&self, state: &State) -> Vec<(String, String, bool)> {
        let mut secs: Vec<(i32, String, bool, String)> = vec![
            (-1000, "identity".into(), false, IDENTITY.into()),
            (-950, "contract".into(), false, CONTRACT.into()),
            (-900, "async-tools".into(), true, ASYNC_PREAMBLE.into()),
        ];
        for (_, order, name, cache_break, f) in &self.reg.sections {
            secs.push((*order, name.clone(), *cache_break, f(state)));
        }
        secs.sort_by_key(|s| s.0);
        secs.into_iter().map(|(_, n, cb, t)| (n, t, cb)).collect()
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        let world = lk(&self.policy).view().world;
        let mut v: Vec<ToolSpec> = self.reg.tools.iter().map(|(_, d, _)| d.spec(&world)).collect();
        v.extend(self.reg.dynamic_tools.iter().map(|(_, s, _)| s.clone()));
        v
    }

    fn policy_context(&self) -> String {
        let p = lk(&self.policy).view();
        let prompts = match self.approval.policy() {
            AskPolicy::Ask => "Calls outside the preset raise an approval prompt to the operator.",
            AskPolicy::Never => "Approval prompts are disabled: calls outside the preset are denied, so do not attempt them.",
        };
        format!(
            "Policy: epoch {}; allowed classes: {}. World: {} (enforced by {}). Workspace: {}. {prompts}",
            p.epoch.0, p.allowed, p.world.name, p.world.enforced_by, p.workspace
        )
    }

    /// Dynamic contexts are logged as `Snapshot` user messages only when their text changes.
    fn log_contexts(&self) -> Result<(), SessionError> {
        let state = self.fold()?;
        let mut items: Vec<(String, String)> = vec![("policy".into(), self.policy_context())];
        for (cap, f) in &self.reg.contexts {
            if let Some(t) = f(&state) {
                items.push((cap.name.to_string(), t));
            }
        }
        let mut last = lk(&self.last_snapshots);
        for (k, t) in items {
            if last.get(&k) != Some(&t) {
                self.append(Principal::system(&k), EntryKind::UserMessage { form: Form::Snapshot, text: t.clone(), client_key: None })?;
                last.insert(k, t);
            }
        }
        Ok(())
    }

    /// Frozen request from the fold; header logged with its hash so replay can prove equality.
    fn build_request(&self) -> Result<(Request, EntryKind), SessionError> {
        let state = self.fold()?;
        let system = self.system_sections(&state);
        let section_hashes: Vec<Hash> = system.iter().map(|(n, t, _)| Hash::of_json(&(n, t))).collect();
        {
            let mut last = lk(&self.last_sections);
            if *last != section_hashes {
                let cache_break_at = system.iter().position(|s| s.2).map(|i| i + 1).unwrap_or(0);
                self.append(
                    Principal::system("loop"),
                    EntryKind::SystemMessage {
                        sections: system.iter().zip(&section_hashes).map(|((n, _, _), h)| (n.clone(), *h)).collect(),
                        cache_break_at,
                    },
                )?;
                *last = section_hashes.clone();
            }
        }
        let state = self.fold()?;
        let messages = aho_llm::normalize_late_results(&aho_log::derive_messages(&state), self.late);
        let tools = self.tool_specs();
        let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        canary(&names, &self.reg)?;
        let header = EntryKind::RequestHeader {
            model: self.cfg.model.clone(),
            tools_visible: names.clone(),
            policy_epoch: lk(&self.policy).view().epoch,
            plan_hash: self.plan_hash,
            section_hashes,
            open_calls: state.open_calls.clone(),
            request_hash: aho_log::request_hash(&messages, &names),
        };
        let req = Request {
            model: self.cfg.model.clone(),
            system,
            messages,
            tools,
            max_tokens: self.cfg.max_tokens,
            reasoning: self.cfg.reasoning,
            cache_key: Some(self.id.0.to_string()),
        };
        Ok((req, header))
    }

    /// Streams one response. Calls the chain allows without asking and marks `Safe` start while
    /// the model is still streaming; their results are held until logged.
    async fn request(self: &Arc<Self>, turn: TurnId) -> Result<(aho_llm::Response, BTreeMap<CallId, Started>), LlmError> {
        let (req, header) = self.build_request().map_err(|e| LlmError::Fatal(e.to_string()))?;
        self.append(Principal::system("loop"), header).map_err(|e| LlmError::Fatal(e.to_string()))?;
        let (early_tx, mut early_rx) = mpsc::unbounded_channel::<(CallId, String, Value)>();
        let events = self.events.clone();
        let sink: aho_llm::Sink<'_> = Box::new(move |ev: StreamEvent| {
            if let StreamEvent::ToolUseEnd { id, name, args } = &ev {
                let _ = early_tx.send((id.clone(), name.clone(), args.clone()));
            }
            let _ = events.send(SessionEvent::Stream(ev));
        });
        let me = self.clone();
        let starter = tokio::spawn(async move {
            let mut started = BTreeMap::new();
            while let Some((call, name, args)) = early_rx.recv().await {
                if let Some(s) = me.try_early_start(turn, &call, &name, &args) {
                    started.insert(call, s);
                }
            }
            started
        });
        let resp = self.llm.stream(req, sink).await;
        let early = starter.await.unwrap_or_default();
        resp.map(|r| (r, early))
    }
}

/// Resolves once `t` fires. `CancelToken` is a plain atomic (shared with tool ops), so this polls.
async fn until_cancelled(t: &CancelToken) {
    while !t.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn fold_head(log: &dyn LogStore, folds: &CustomFolds) -> Result<State, LogError> {
    match log.head() {
        Some(h) => aho_log::fold(&log.read_branch(h)?, folds),
        None => Ok(State::default()),
    }
}

/// Runtime canary: the tool list sent to the model equals what the registry holds.
pub fn canary(visible: &[String], reg: &Registry) -> Result<(), SessionError> {
    let mut expected: Vec<String> = reg.tools.iter().map(|(_, d, _)| d.name.to_string()).collect();
    expected.extend(reg.dynamic_tools.iter().map(|(_, s, _)| s.name.clone()));
    let mut v = visible.to_vec();
    v.sort();
    expected.sort();
    if v == expected {
        Ok(())
    } else {
        Err(SessionError::Boot(format!("invariant: model-visible tools {v:?} differ from the registry {expected:?}")))
    }
}

/// Replay equality: every `RequestHeader.request_hash` equals the hash re-derived from the log prefix before it.
pub fn verify_replay(entries: &[Entry], folds: &CustomFolds, late: LateResults) -> Result<usize, String> {
    let mut checked = 0;
    for (k, e) in entries.iter().enumerate() {
        if let EntryKind::RequestHeader { tools_visible, request_hash, .. } = &e.kind {
            let state = aho_log::fold(&entries[..k], folds).map_err(|x| x.to_string())?;
            let msgs = aho_llm::normalize_late_results(&aho_log::derive_messages(&state), late);
            let h = aho_log::request_hash(&msgs, tools_visible);
            if h != *request_hash {
                return Err(format!(
                    "request at entry {} does not replay: recorded {} vs derived {}",
                    e.id.0,
                    request_hash.short(),
                    h.short()
                ));
            }
            checked += 1;
        }
    }
    Ok(checked)
}

pub const IDENTITY: &str = include_str!("../prompts/identity.md");

/// The distilled delivery contract: the only judgment prose the harness ships. Everything
/// checkable lives in policy, guards, and the driver; this is the residue that cannot be gated.
pub const CONTRACT: &str = include_str!("../prompts/contract.md");

/// The model-facing contract for async tool calls.
pub const ASYNC_PREAMBLE: &str = include_str!("../prompts/async-preamble.md");
