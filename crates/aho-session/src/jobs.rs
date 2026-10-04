//! Session jobs. Detach is an event: a running op whose tool allows it leaves its turn
//! (`OpDetached` + the call's `ToolResult` naming the job) and keeps running; its outcome waits
//! in the delivery queue and is logged as `JobDelivered` + `OpDone` at the next request boundary.
//! Also the abort scopes, `Interrupt::Cancel`, and the `jobs.*` config.
//! Split out of the session driver for the file-size ratchet; child modules share the root's
//! private state, and items crossing modules are `pub(crate)`.

use super::*;
use crate::tools::LateCheck;
use aho_cap::{BoundWorld, Detach, Interrupt};
use aho_log::{DetachCause, Job};

/// `prompts/jobs.md`: the session's fixed job wording, one `key: template` per line.
const TEMPLATES: &str = include_str!("../prompts/jobs.md");

pub(crate) fn template(key: &str) -> &'static str {
    TEMPLATES.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(": ")).unwrap_or("")
}

/// The model-visible schema of a tool whose calls the model may send to the background: an
/// optional `background` boolean, unless the tool defines its own.
pub(crate) fn with_background(spec: &mut ToolSpec) {
    if let Some(props) = spec.input_schema.0.get_mut("properties").and_then(Value::as_object_mut) {
        props.entry("background").or_insert_with(|| serde_json::json!({ "type": "boolean", "description": template("background") }));
    }
}

// ───────────────────────────── config ─────────────────────────────

/// How a tool's running calls may become jobs, after `jobs.detach.<tool>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachRule {
    Never,
    /// The model may pass `background: true`; the operator may detach.
    OnRequest,
    /// Detached at start.
    Always,
    /// On request, and automatically once a call has run this long.
    After(Duration),
}

impl DetachRule {
    /// `never | on_request | always | after:<n>ms|s|m|h`.
    pub fn parse(s: &str) -> Option<DetachRule> {
        match s {
            "never" => Some(DetachRule::Never),
            "on_request" => Some(DetachRule::OnRequest),
            "always" => Some(DetachRule::Always),
            _ => duration(s.strip_prefix("after:")?).map(DetachRule::After),
        }
    }

    /// The model may ask for this call to run in the background.
    pub fn on_request(self) -> bool {
        matches!(self, DetachRule::OnRequest | DetachRule::After(_))
    }
}

fn duration(s: &str) -> Option<Duration> {
    let digits = s.find(|c: char| !c.is_ascii_digit())?;
    let n: u64 = s[..digits].parse().ok()?;
    let ms = match &s[digits..] {
        "ms" => n,
        "s" => n.checked_mul(1_000)?,
        "m" => n.checked_mul(60_000)?,
        "h" => n.checked_mul(3_600_000)?,
        _ => return None,
    };
    Some(Duration::from_millis(ms))
}

/// What a rewind does to a job the target branch still holds (`jobs.on_rewind`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnRewind {
    /// Cancel it; the new branch closes it (`OpDone Cancelled{Rewind}`).
    #[default]
    Cancel,
    /// Keep it running; it delivers into the new branch.
    Keep,
}

/// The `jobs.*` keys. The defaults (`Default`) are the keys' declared defaults.
#[derive(Clone, Debug)]
pub struct JobsConfig {
    /// `jobs.detach.<tool>`.
    pub detach: BTreeMap<String, DetachRule>,
    pub on_rewind: OnRewind,
    /// `jobs.wake`: what starts a turn for jobs that finish while none runs.
    pub wake: WakeMode,
    /// `jobs.wake_budget`: wake turns allowed since the operator's last prompt.
    pub wake_budget: u32,
    /// `jobs.deliver`: which request boundaries deliver finished jobs.
    pub deliver: DeliverMode,
    /// `jobs.max`: jobs that may run at once while no operator jobs lease is live.
    pub max: usize,
    /// `jobs.settle_timeout_s`: how long print mode waits for jobs before exiting.
    pub settle_timeout: Duration,
}

impl JobsConfig {
    pub fn from_loadout(l: &Loadout) -> Result<JobsConfig, String> {
        let mut detach = BTreeMap::new();
        for (tool, v) in l.table("jobs.detach") {
            let rule = v.as_str().and_then(DetachRule::parse).ok_or_else(|| {
                format!("`jobs.detach.{tool}` = {v}: expected \"never\", \"on_request\", \"always\" or \"after:<n>ms|s|m|h\"")
            })?;
            detach.insert(tool, rule);
        }
        let on_rewind = match l.str("jobs.on_rewind").unwrap_or("cancel") {
            "cancel" => OnRewind::Cancel,
            "keep" => OnRewind::Keep,
            other => return Err(format!("`jobs.on_rewind` = {other:?}: expected \"cancel\" or \"keep\"")),
        };
        let mut c = JobsConfig { detach, on_rewind, ..JobsConfig::default() };
        c.read_policy(l)?;
        Ok(c)
    }

    /// Config wins over the decl only toward what the decl allows: `Detach::Never` stays never.
    pub fn rule(&self, decl: &ToolDecl, tool: &str) -> DetachRule {
        match decl.detach {
            Detach::Never => DetachRule::Never,
            d => self.detach.get(tool).copied().unwrap_or(if d == Detach::Always { DetachRule::Always } else { DetachRule::OnRequest }),
        }
    }
}

// ───────────────────────────── API types ─────────────────────────────

/// What [`Session::abort_scope`] cancels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortScope {
    /// The running turn and its ops; jobs keep running. What [`Session::abort`] does.
    Turn,
    /// The turn, every running op, and every job.
    All,
    /// One op or job.
    Op(OpId),
}

impl AbortScope {
    /// The wire form (`session.abort{scope?, op?}` in rpc and the daemon): `turn` (default),
    /// `all`, or `op` with `op`.
    pub fn from_wire(scope: Option<&str>, op: Option<OpId>) -> Result<AbortScope, String> {
        match (scope.unwrap_or("turn"), op) {
            ("turn", None) => Ok(AbortScope::Turn),
            ("all", None) => Ok(AbortScope::All),
            ("op", Some(op)) => Ok(AbortScope::Op(op)),
            ("op", None) => Err("abort scope `op` needs `op`: the operation to cancel; nothing was cancelled".into()),
            ("turn" | "all", Some(_)) => Err("`op` goes with scope `op` only; nothing was cancelled".into()),
            (other, _) => Err(format!("abort scope `{other}` is not one of turn, all, op; nothing was cancelled")),
        }
    }
}

/// One open job on the head branch ([`Session::jobs`]).
#[derive(Clone, Debug, PartialEq)]
pub struct JobInfo {
    pub op: OpId,
    pub call: CallId,
    pub tool: String,
    pub cause: DetachCause,
    /// `None` while running; else finished, waiting for delivery at the next request boundary.
    pub status: Option<OpStatus>,
    /// When the op started; `None` when this process does not run it (a job left by a restart).
    pub started: Option<Timestamp>,
}

impl JobInfo {
    /// The wire shape surfaces send (`job.list` in rpc and the daemon).
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "op": self.op,
            "call": self.call,
            "tool": self.tool,
            "cause": self.cause,
            "status": self.status,
            "started": self.started,
        })
    }
}

/// The delivery queue and the reasons behind session-initiated cancels.
pub(crate) struct Book {
    /// Finished jobs in finish order, delivered at the next request boundary.
    pub(crate) ready: Vec<Finished>,
    /// The content the delivery chain decided for an op in `ready` (`delivery.rs`).
    pub(crate) decided: BTreeMap<OpId, Vec<ContentBlock>>,
    /// Deferred deliveries and when their hold ends; back to `ready` after it.
    pub(crate) held: Vec<(Finished, std::time::Instant)>,
    /// How many times each op's delivery was deferred.
    pub(crate) deferrals: BTreeMap<OpId, u32>,
    /// Why the session cancelled a running op; an op cancelled without one folds to `Aborted`.
    why: BTreeMap<OpId, CancelReason>,
    /// No turn is running: a finishing job announces itself (`SessionEvent::JobFinished`).
    pub(crate) idle: bool,
}

impl Book {
    pub(crate) fn new() -> Book {
        Book { ready: Vec::new(), decided: BTreeMap::new(), held: Vec::new(), deferrals: BTreeMap::new(), why: BTreeMap::new(), idle: true }
    }
}

/// Held by `prompt_as` for the turn: deliveries wait for boundaries until it drops, then jobs that
/// finished after the last boundary are announced.
pub(crate) struct TurnScope<'a>(&'a Session);

impl Drop for TurnScope<'_> {
    fn drop(&mut self) {
        let mut b = lk(&self.0.jobs);
        b.idle = true;
        for f in &b.ready {
            self.0.emit(SessionEvent::JobFinished { op: f.op });
        }
    }
}

impl Session {
    // ───────────────────────────── public API ─────────────────────────────

    /// Cancels by scope. `false` only when `Op` names no running op.
    pub fn abort_scope(&self, scope: AbortScope) -> bool {
        match scope {
            AbortScope::Turn => {
                lk(&self.turn_cancel).cancel();
                true
            }
            AbortScope::All => {
                lk(&self.turn_cancel).cancel();
                for (op, _, _) in self.ops.running() {
                    self.ops.cancel(op);
                }
                true
            }
            AbortScope::Op(op) => self.ops.cancel(op),
        }
    }

    /// Detaches a running turn op: its call gets a result naming the job now, and its outcome is
    /// delivered later. Applied at once while no turn runs; during a turn, by its driver at the
    /// next wait or step boundary (unless the op finishes first).
    pub fn detach(&self, op: OpId, cause: DetachCause) -> Result<(), SessionError> {
        let info = self.ops.info(op).ok_or_else(|| SessionError::NotFound(format!("there is no operation {}", op.0)))?;
        if info.status.is_some() || info.detached {
            return Err(SessionError::Refused(format!("operation {} already finished or is already a job; nothing was detached", op.0)));
        }
        if self.detach_rule(&info.tool) == DetachRule::Never {
            return Err(SessionError::Refused(format!(
                "`{}` cannot run as a job (its tool is not detachable, or `jobs.detach.{}` = \"never\"); nothing was detached",
                info.tool, info.tool
            )));
        }
        if let Some(why) = self.job_slot_refused()? {
            return Err(SessionError::Refused(format!("operation {} was not detached: {why}", op.0)));
        }
        if self.route_detach(op, cause)? {
            Ok(())
        } else {
            Err(SessionError::Refused(format!("operation {} has no open call to release; nothing was detached", op.0)))
        }
    }

    /// Cancels one running job (`job.kill`); its `Cancelled` outcome is delivered like any other.
    /// A turn op is refused: the turn's own abort (`AbortScope::Turn` or `Op`) cancels it.
    pub fn kill_job(&self, op: OpId) -> Result<(), SessionError> {
        let info =
            self.ops.info(op).ok_or_else(|| SessionError::NotFound(format!("there is no operation {}; nothing was cancelled", op.0)))?;
        if !info.detached {
            return Err(SessionError::Refused(format!(
                "operation {} is not a job: it belongs to the running turn, which an abort cancels; nothing was cancelled",
                op.0
            )));
        }
        if info.status.is_none() && self.ops.cancel(op) {
            Ok(())
        } else {
            Err(SessionError::Refused(format!("job {} already finished; nothing was cancelled", op.0)))
        }
    }

    /// Open jobs on the head branch: running, or finished and waiting for delivery.
    pub fn jobs(&self) -> Result<Vec<JobInfo>, SessionError> {
        let state = self.fold()?;
        let ready: BTreeMap<OpId, OpStatus> = lk(&self.jobs).ready.iter().map(|f| (f.op, f.status.clone())).collect();
        Ok(state
            .jobs
            .iter()
            .map(|(op, j)| JobInfo {
                op: *op,
                call: j.call.clone(),
                tool: state.calls.get(&j.call).map(|c| c.0.clone()).unwrap_or_default(),
                cause: j.cause,
                status: ready.get(op).cloned().or_else(|| self.ops.status(*op).flatten()).or_else(|| j.delivered.clone()),
                started: self.ops.info(*op).map(|i| i.started),
            })
            .collect())
    }

    // ───────────────────────────── detach ─────────────────────────────

    /// The rule for `tool` (static by name or alias, or runtime-registered: never).
    pub(crate) fn detach_rule(&self, tool: &str) -> DetachRule {
        self.lookup(tool).map(|(d, name, _)| self.jobs_cfg.rule(d, &name)).unwrap_or(DetachRule::Never)
    }

    /// `background` is the session's own argument for this tool: the model may ask, and the
    /// tool's schema does not define a `background` of its own.
    pub(crate) fn takes_background(&self, decl: &ToolDecl, name: &str, world: &BoundWorld) -> bool {
        self.jobs_cfg.rule(decl, name).on_request() && (decl.schema)(world).0.pointer("/properties/background").is_none()
    }

    /// The args a tool body sees: without the session's `background`.
    pub(crate) fn exec_args(&self, decl: &ToolDecl, name: &str, mut args: Value) -> Value {
        let world = lk(&self.policy).view().world;
        if self.takes_background(decl, name, &world)
            && let Some(m) = args.as_object_mut()
        {
            m.remove("background");
        }
        args
    }

    /// Right after `OpStarted`: detach at once when asked (`background: true`) or `always`, else
    /// arm `after:<dur>`. `true` when the call was released.
    pub(crate) fn detach_at_start(self: &Arc<Self>, decl: &ToolDecl, name: &str, op: OpId, args: &Value) -> Result<bool, SessionError> {
        let rule = self.jobs_cfg.rule(decl, name);
        let world = lk(&self.policy).view().world;
        let asked = args.get("background").and_then(Value::as_bool) == Some(true) && self.takes_background(decl, name, &world);
        if rule == DetachRule::Always || asked {
            return self.detach_now(op, DetachCause::Requested);
        }
        if let DetachRule::After(d) = rule {
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(d).await;
                if let Some(s) = weak.upgrade()
                    && s.ops.status(op) == Some(None)
                    && let Err(e) = s.route_detach(op, DetachCause::After(d.as_millis() as u64))
                {
                    s.emit(SessionEvent::Error(format!("job {} could not be detached after {}ms: {e}", op.0, d.as_millis())));
                }
            });
        }
        Ok(false)
    }

    /// Applies a detach now while no turn runs, or hands it to the running turn's driver (the only
    /// writer of call results). `false`: nothing to detach.
    fn route_detach(&self, op: OpId, cause: DetachCause) -> Result<bool, SessionError> {
        match self.running.try_lock() {
            Ok(_idle) => self.detach_now(op, cause),
            Err(_) => {
                self.detach_tx.send((op, cause)).map_err(|_| SessionError::NotFound("the session has ended".into()))?;
                Ok(true)
            }
        }
    }

    /// Detach requests that reached the driver (operator, `after:<dur>`); the number applied.
    pub(crate) fn apply_detaches(&self, rx: &mut mpsc::UnboundedReceiver<(OpId, DetachCause)>) -> Result<usize, SessionError> {
        let mut n = 0;
        while let Ok((op, cause)) = rx.try_recv() {
            n += usize::from(self.detach_now(op, cause)?);
        }
        Ok(n)
    }

    /// Releases `op`'s call: `OpDetached`, then the call's `ToolResult` naming the job. Runs on
    /// the turn driver (or under the admission lock while idle), the only writer of results, so
    /// the call cannot get its real result in between. `false`: nothing to detach.
    pub(crate) fn detach_now(&self, op: OpId, cause: DetachCause) -> Result<bool, SessionError> {
        let Some(info) = self.ops.info(op) else { return Ok(false) };
        let state = self.fold()?;
        let call = info.call;
        if !state.open_calls.contains(&call) || state.open_ops.get(&op).map(|o| &o.0) != Some(&call) || state.jobs.contains_key(&op) {
            return Ok(false);
        }
        if let Some(why) = self.job_slot_refused()? {
            self.detach_refused(op, cause, why);
            return Ok(false);
        }
        // The table flag flips first: from here on the op's outcome goes to the delivery queue.
        if !self.ops.detach(op) {
            return Ok(false);
        }
        self.append(Principal::system("jobs"), EntryKind::OpDetached { op, call: call.clone(), cause })?;
        self.append(
            Principal::system("jobs"),
            EntryKind::ToolResult {
                late: state.last_request_after_call(&call),
                call,
                outcome: ToolOutcome::Ok,
                value: serde_json::json!({ "job": op.0 }),
                content: vec![ContentBlock::text(template("detached").replace("{op}", &op.0.to_string()))],
                spill_ref: None,
                meta: None,
            },
        )?;
        Ok(true)
    }

    // ───────────────────────────── finish & delivery ─────────────────────────────

    /// A detached op finished: queue its outcome for the next request boundary; while no turn
    /// runs, announce it.
    pub(crate) fn job_finished(&self, f: Finished) {
        let op = f.op;
        let mut b = lk(&self.jobs);
        b.ready.push(f);
        if b.idle {
            self.emit(SessionEvent::JobFinished { op });
        }
    }

    /// A turn starts: from now on deliveries wait for its boundaries.
    pub(crate) fn turn_scope(&self) -> TurnScope<'_> {
        lk(&self.jobs).idle = false;
        TurnScope(self)
    }

    /// This process holds `op`'s finished outcome, due or deferred, not delivered yet.
    pub(crate) fn outcome_waiting(&self, op: OpId) -> bool {
        let b = lk(&self.jobs);
        b.ready.iter().any(|f| f.op == op) || b.held.iter().any(|(f, _)| f.op == op)
    }

    /// Request boundary (or the turn's start): the delivery chain decides every finished job
    /// first (`triage_deliveries`), then each decided one is logged as `JobDelivered` + `OpDone`,
    /// in finish order. An outcome whose op this branch no longer holds (rewound away) is
    /// dropped. Returns how many were delivered; deferred ones come back after their hold.
    pub(crate) async fn deliver_jobs(self: &Arc<Self>) -> Result<usize, SessionError> {
        self.triage_deliveries().await?;
        let decided: Vec<Finished> = {
            let mut b = lk(&self.jobs);
            let (decided, rest) = std::mem::take(&mut b.ready).into_iter().partition(|f| b.decided.contains_key(&f.op));
            b.ready = rest;
            decided
        };
        let mut n = 0;
        for f in decided {
            lk(&self.jobs).deferrals.remove(&f.op);
            if self.fold()?.open_ops.contains_key(&f.op) {
                self.record_result(f).await?;
                n += 1;
            } else {
                lk(&self.jobs).decided.remove(&f.op);
            }
        }
        Ok(n)
    }

    // ───────────────────────────── cancel ─────────────────────────────

    /// Cancels `op` for `reason`: its outcome folds to `Cancelled { reason }`.
    pub(crate) fn cancel_op(&self, op: OpId, reason: CancelReason) -> bool {
        lk(&self.jobs).why.insert(op, reason);
        let cancelled = self.ops.cancel(op);
        if !cancelled {
            lk(&self.jobs).why.remove(&op);
        }
        cancelled
    }

    /// The `OpStatus` of an op's outcome; a cancel carries the reason the session recorded.
    pub(crate) fn status_of(&self, op: OpId, o: &ExecOutcome) -> OpStatus {
        let why = lk(&self.jobs).why.remove(&op);
        match o {
            ExecOutcome::Done { .. } => OpStatus::Completed { code: 0 },
            ExecOutcome::Err { reason, .. } => OpStatus::Failed { err: reason.clone() },
            ExecOutcome::Cancelled => OpStatus::Cancelled { reason: why.unwrap_or(CancelReason::Aborted) },
        }
    }

    /// Input reached a waiting turn: its running `Interrupt::Cancel` ops (`job_wait`, …) stop with
    /// the mechanical "interrupted" result. Jobs are never interrupted by input.
    pub(crate) fn interrupt_ops(&self) {
        let cancels = |tool: &str| self.lookup(tool).is_some_and(|(d, _, _)| d.interrupt == Interrupt::Cancel);
        for info in self.ops.list() {
            if info.status.is_none() && !info.detached && cancels(&info.tool) {
                self.cancel_op(info.op, CancelReason::UserInterrupted);
            }
        }
    }

    // ───────────────────────────── restart ─────────────────────────────

    /// A job open at load: delivered but not closed → `OpDone`; re-runnable → re-dispatched as a
    /// job under a new op; else queued for delivery as `Uncertain` (external effect) or `Failed`.
    pub(crate) fn recover_job(
        self: &Arc<Self>,
        state: &State,
        op: OpId,
        job: &Job,
        external: bool,
        rerun: bool,
    ) -> Result<(), SessionError> {
        if let Some(status) = &job.delivered {
            self.append(Principal::system("recovery"), EntryKind::OpDone { op, status: status.clone() })?;
            return Ok(());
        }
        if rerun
            && let Some((tool, _, args)) = state.calls.get(&job.call)
            && let Some((decl, name, exec)) = self.lookup(tool)
        {
            self.append(
                Principal::system("recovery"),
                EntryKind::OpDone { op, status: OpStatus::Failed { err: "re-dispatched after restart".into() } },
            )?;
            let s = self.spawn_exec(decl, name, exec, job.call.clone(), args.clone(), Principal::system("recovery"), vec![], true);
            self.log_op_started(&s, decl)?;
            self.append(Principal::system("recovery"), EntryKind::OpDetached { op: s.op, call: job.call.clone(), cause: job.cause })?;
            return Ok(());
        }
        let (code, reason, status) = if external {
            (
                "UNCERTAIN",
                "The harness restarted while this job was running. It has an external effect and may or may not have happened; check its effect before repeating it.",
                OpStatus::Uncertain { last_seen: Timestamp::now() },
            )
        } else {
            (
                "INTERRUPTED",
                "The harness restarted while this job was running; it did not complete.",
                OpStatus::Failed { err: "interrupted by a restart".into() },
            )
        };
        let f = Finished { call: job.call.clone(), op, outcome: ExecOutcome::err(code, reason), post_context: vec![], status };
        self.job_finished(f);
        Ok(())
    }
}
