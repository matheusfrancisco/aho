//! Jobs policy: which request boundaries deliver finished jobs (`jobs.deliver`), the wake turn a
//! host driver starts for jobs that finish while no turn runs (`jobs.wake`, `jobs.wake_budget`),
//! and how many jobs may run at once (`jobs.max`, or the operator's jobs lease, whose end cancels
//! the jobs still running).
//! Split out of the session driver for the file-size ratchet; child modules share the root's
//! private state, and items crossing modules are `pub(crate)`.

use super::*;
use crate::jobs::template;
use aho_types::LeaseId;

/// `jobs.wake`: what happens when a job finishes while no turn runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WakeMode {
    /// A host driver starts a wake turn (`Principal::system("jobs")`) that delivers it.
    #[default]
    Idle,
    /// No turn; surfaces tell the operator (`SessionEvent::JobFinished`).
    Operator,
    /// No turn and no announcement; it is delivered with the next prompt.
    Never,
}

/// `jobs.deliver`: which request boundaries of a running turn deliver finished jobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeliverMode {
    /// Every boundary.
    #[default]
    Boundary,
    /// A turn's start, and the boundaries while a job-awaiting call (`ToolDecl::awaits_jobs`,
    /// `job_wait`) is open or right after its result; outcomes are held at the others.
    OnWait,
}

/// What [`Session::wake`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum Woke {
    /// A wake turn ran and ended with this transition.
    Ran(Transition),
    /// Nothing is due: no finished job waits, or a deferral holds every one.
    Nothing,
    /// Finished jobs wait, but `jobs.wake_budget` is spent since the operator's last prompt.
    BudgetSpent { waiting: usize },
}

impl Default for JobsConfig {
    fn default() -> JobsConfig {
        JobsConfig {
            detach: BTreeMap::new(),
            on_rewind: OnRewind::default(),
            wake: WakeMode::Idle,
            wake_budget: 10,
            deliver: DeliverMode::Boundary,
            max: 8,
            settle_timeout: Duration::from_secs(1800),
        }
    }
}

impl JobsConfig {
    /// `jobs.wake`, `jobs.wake_budget`, `jobs.deliver`, `jobs.max`, `jobs.settle_timeout_s`.
    pub(crate) fn read_policy(&mut self, l: &Loadout) -> Result<(), String> {
        let int = |k: &str, d: u64| match l.get(k) {
            None => Ok(d),
            Some(v) => v.as_u64().ok_or_else(|| format!("`{k}` = {v}: expected a whole number, 0 or more")),
        };
        self.wake = match l.str("jobs.wake").unwrap_or("idle") {
            "idle" => WakeMode::Idle,
            "operator" => WakeMode::Operator,
            "never" => WakeMode::Never,
            other => return Err(format!("`jobs.wake` = {other:?}: expected \"idle\", \"operator\" or \"never\"")),
        };
        self.deliver = match l.str("jobs.deliver").unwrap_or("boundary") {
            "boundary" => DeliverMode::Boundary,
            "on_wait" => DeliverMode::OnWait,
            other => return Err(format!("`jobs.deliver` = {other:?}: expected \"boundary\" or \"on_wait\"")),
        };
        self.wake_budget = u32::try_from(int("jobs.wake_budget", 10)?).unwrap_or(u32::MAX);
        self.max = usize::try_from(int("jobs.max", 8)?).unwrap_or(usize::MAX);
        self.settle_timeout = Duration::from_secs(int("jobs.settle_timeout_s", 1800)?);
        Ok(())
    }
}

// ───────────────────────────── jobs lease ─────────────────────────────

/// `LeaseGranted.subject` prefix of an operator jobs lease; the rest is `{"max": <n>}`.
pub const JOBS_LEASE: &str = "jobs:";

/// The `LeaseGranted.subject` of a jobs lease that allows `max` jobs at once.
pub fn jobs_lease_subject(max: u32) -> String {
    format!("{JOBS_LEASE}{}", serde_json::json!({ "max": max }))
}

/// An operator jobs lease: up to `max` jobs at once until `expires`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobsLease {
    pub lease: LeaseId,
    pub max: u32,
    pub expires: Timestamp,
    /// The `LeaseGranted` entry.
    pub granted: EntryId,
}

impl JobsLease {
    pub fn live(&self, now: Timestamp) -> bool {
        now.0 < self.expires.0
    }
}

/// The latest `LeaseGranted` with a jobs subject not revoked since (expired or not).
pub fn jobs_lease(entries: &[Entry]) -> Option<JobsLease> {
    let mut held: Option<JobsLease> = None;
    for e in entries {
        match &e.kind {
            EntryKind::LeaseGranted { lease, subject, expires } => {
                let max = subject
                    .strip_prefix(JOBS_LEASE)
                    .and_then(|t| serde_json::from_str::<Value>(t).ok())
                    .and_then(|v| v.get("max")?.as_u64())
                    .and_then(|m| u32::try_from(m).ok());
                if let Some(max) = max {
                    held = Some(JobsLease { lease: *lease, max, expires: *expires, granted: e.id });
                }
            }
            EntryKind::LeaseRevoked { lease, .. } if held.is_some_and(|l| l.lease == *lease) => held = None,
            _ => {}
        }
    }
    held
}

impl Session {
    /// The `jobs.*` keys this session booted with.
    pub fn jobs_config(&self) -> &JobsConfig {
        &self.jobs_cfg
    }

    fn branch(&self) -> Result<Vec<Entry>, SessionError> {
        match self.log.head() {
            Some(h) => Ok(self.log.read_branch(h)?),
            None => Ok(Vec::new()),
        }
    }

    // ───────────────────────────── limit ─────────────────────────────

    /// How many jobs may run at once now: the live jobs lease's `max`, else `jobs.max`.
    pub fn job_limit(&self) -> Result<(usize, Option<JobsLease>), SessionError> {
        let lease = jobs_lease(&self.branch()?).filter(|l| l.live(Timestamp::now()));
        Ok((lease.map_or(self.jobs_cfg.max, |l| l.max as usize), lease))
    }

    /// Jobs running now; finished ones waiting for delivery do not count.
    pub fn running_jobs(&self) -> usize {
        self.ops.list().into_iter().filter(|i| i.detached && i.status.is_none()).count()
    }

    /// Why one more job would exceed the limit, if it would.
    pub(crate) fn job_slot_refused(&self) -> Result<Option<String>, SessionError> {
        let (limit, lease) = self.job_limit()?;
        let running = self.running_jobs();
        if running < limit {
            return Ok(None);
        }
        let by = match lease {
            Some(l) => format!("the operator's jobs lease {} allows {}", l.lease.0, l.max),
            None => format!("`jobs.max` is {limit} and no operator jobs lease is live"),
        };
        Ok(Some(format!("{running} job(s) already run and {by}")))
    }

    /// The jobs limit refused a detach; the call stays in its turn. A call the model sent to the
    /// background hears why after its result; any other cause is reported to the surfaces.
    pub(crate) fn detach_refused(&self, op: OpId, cause: DetachCause, why: String) {
        match cause {
            DetachCause::Requested => {
                lk(&self.job_notes).insert(op, template("not_detached").replace("{why}", &why));
            }
            _ => self.emit(SessionEvent::Error(format!("operation {} was not detached: {why}; it stays in its turn", op.0))),
        }
    }

    /// Keeps the jobs lease for the session's life: when the lease jobs run under ends (expires,
    /// or is revoked with no new grant in its place), every running job is cancelled
    /// (`Cancelled{LeaseExpired}`) and delivered like any other outcome.
    pub(crate) fn watch_jobs_lease(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let mut events = self.subscribe();
        tokio::spawn(async move {
            let mut held: Option<JobsLease> = None;
            loop {
                let wait = {
                    let Some(s) = weak.upgrade() else { return };
                    let now = Timestamp::now();
                    let lease = match s.branch() {
                        Ok(es) => jobs_lease(&es).filter(|l| l.live(now)),
                        Err(e) => {
                            s.emit(SessionEvent::Error(format!("the jobs lease is no longer watched: the log could not be read ({e})")));
                            return;
                        }
                    };
                    if let (Some(ended), None) = (held, lease) {
                        s.end_jobs_lease(ended);
                    }
                    held = lease;
                    lease.map(|l| Duration::from_millis(l.expires.0.saturating_sub(now.0)))
                };
                let expiry = async {
                    match wait {
                        Some(d) => tokio::time::sleep(d).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::pin!(expiry);
                loop {
                    tokio::select! {
                        () = &mut expiry => break,
                        ev = events.recv() => match ev {
                            Ok(SessionEvent::Entry(e)) if matches!(e.kind, EntryKind::LeaseGranted { .. } | EntryKind::LeaseRevoked { .. }) => break,
                            Err(broadcast::error::RecvError::Lagged(_)) => break,
                            Err(broadcast::error::RecvError::Closed) => return,
                            Ok(_) => {}
                        },
                    }
                }
            }
        });
    }

    fn end_jobs_lease(&self, lease: JobsLease) {
        let cancelled = self
            .ops
            .list()
            .into_iter()
            .filter(|i| i.detached && i.status.is_none() && self.cancel_op(i.op, CancelReason::LeaseExpired))
            .count();
        if cancelled > 0 {
            self.emit(SessionEvent::Error(format!(
                "the operator's jobs lease {} ended, so its {cancelled} running job(s) were cancelled",
                lease.lease.0
            )));
        }
    }

    // ───────────────────────────── delivery ─────────────────────────────

    /// Whether this request boundary delivers finished jobs: always under `boundary` and at a
    /// turn's start; under `on_wait` only while a job-awaiting call is open or at the boundary
    /// right after its result (which then leaves `awaiting`).
    pub(crate) fn deliveries_open(&self, start: bool, awaiting: &mut Vec<CallId>) -> Result<bool, SessionError> {
        if self.jobs_cfg.deliver == DeliverMode::Boundary || start {
            return Ok(true);
        }
        if awaiting.is_empty() {
            return Ok(false);
        }
        let open = self.fold()?.open_calls;
        awaiting.retain(|c| open.contains(c));
        Ok(true)
    }

    /// The job-awaiting calls among a response's tool uses (`jobs.deliver = on_wait` only).
    pub(crate) fn job_waits(&self, uses: &[(CallId, String, Value)]) -> Vec<CallId> {
        if self.jobs_cfg.deliver == DeliverMode::Boundary {
            return Vec::new();
        }
        uses.iter().filter(|(_, name, _)| self.lookup(name).is_some_and(|(d, _, _)| d.awaits_jobs)).map(|(c, _, _)| c.clone()).collect()
    }

    // ───────────────────────────── wake ─────────────────────────────

    /// Wake turns since the operator's last prompt on the head branch.
    pub fn wake_turns(&self) -> Result<u32, SessionError> {
        let jobs = Principal::system("jobs");
        let mut n = 0;
        for e in self.branch()?.iter().rev() {
            if let EntryKind::UserMessage { form: Form::Prompt, .. } = e.kind {
                if e.by == Principal::Operator {
                    break;
                }
                n += u32::from(e.by == jobs);
            }
        }
        Ok(n)
    }

    /// Finished jobs not delivered yet (due, or held by a deferral).
    pub fn jobs_waiting(&self) -> usize {
        self.deliveries_waiting()
    }

    /// One wake turn for finished jobs, prompted as `Principal::system("jobs")`; its start delivers
    /// them. A host driver calls this on `SessionEvent::JobFinished` under `jobs.wake = idle`, and
    /// print mode to settle. No turn opens when nothing would be delivered or `jobs.wake_budget` is
    /// spent since the operator's last prompt. `Busy` while a turn runs.
    pub async fn wake(self: &Arc<Self>) -> Result<Woke, SessionError> {
        let _guard = self.running.try_lock().map_err(|_| SessionError::Busy)?;
        let waiting = self.deliveries_waiting();
        if waiting == 0 {
            return Ok(Woke::Nothing);
        }
        if self.wake_turns()? >= self.jobs_cfg.wake_budget {
            self.emit(SessionEvent::Error(format!(
                "`jobs.wake_budget` ({}) is spent since the operator's last prompt, so no turn was started: {waiting} finished job(s) are delivered with the next prompt",
                self.jobs_cfg.wake_budget
            )));
            return Ok(Woke::BudgetSpent { waiting });
        }
        if self.triage_deliveries().await? == 0 {
            return Ok(Woke::Nothing);
        }
        self.turn(Principal::system("jobs"), template("wake").to_string(), None).await.map(Woke::Ran)
    }
}
