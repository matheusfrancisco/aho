//! The delivery chain. Before a finished job is logged as `JobDelivered`, every
//! `Registrar::delivery` callback and then every `HookEvent::JobDone` hook, each in registration
//! order, sees it and may replace its content, suppress it (a marker naming the capability is
//! delivered instead: the entry is never dropped) or defer it (held, then offered to the whole
//! chain again at the first delivery point after the hold). `Suppress` and `Defer` end the chain.
//! Each run is a `HookInvoked` + `HookResult` pair under the capability; the logged
//! `JobDelivered` holds exactly the content the chain decided, so replay sees what the model saw.
//! Split out of the session driver for the file-size ratchet; child modules share the root's
//! private state, and items crossing modules are `pub(crate)`.

use std::time::Instant;

use super::*;
use crate::tools::split_outcome;
use aho_cap::{CapabilityId, Delivery, JobView, OnError};

/// Most characters of a job's content a `JobDone` hook payload carries.
pub(crate) const PAYLOAD_CHARS: usize = 4096;

/// Deferrals one delivery may take; past it a `Defer` counts as `Keep`, so a callback that
/// always defers cannot hold a job (and grow the log) forever.
pub(crate) const MAX_DEFERRALS: u32 = 16;

/// Where the chain left one delivery.
enum Decided {
    Deliver(Vec<ContentBlock>),
    Hold(Instant),
}

impl Session {
    /// Finished jobs not delivered yet: due at the next delivery point, or held by a deferral.
    pub(crate) fn deliveries_waiting(&self) -> usize {
        let b = lk(&self.jobs);
        b.ready.len() + b.held.len()
    }

    /// Runs the chain for every finished job not decided yet. Appends no `JobDelivered`, only
    /// the chain's audit pairs, so it may run before a turn opens. Holds that ended are released
    /// first; outcomes of ops this branch no longer holds are dropped; deferred ones are held.
    /// Returns how many decided deliveries wait for the next delivery point.
    pub(crate) async fn triage_deliveries(self: &Arc<Self>) -> Result<usize, SessionError> {
        self.release_held(false);
        let state = self.fold()?;
        let (queued, with) = {
            let mut b = lk(&self.jobs);
            let mut queued = Vec::new();
            for f in std::mem::take(&mut b.ready) {
                if state.open_ops.contains_key(&f.op) {
                    queued.push(f);
                } else {
                    b.decided.remove(&f.op);
                    b.deferrals.remove(&f.op);
                }
            }
            let with: Vec<OpId> = queued.iter().map(|f| f.op).filter(|op| !b.decided.contains_key(op)).collect();
            (queued, with)
        };
        let mut kept = Vec::with_capacity(queued.len());
        for f in queued {
            if !with.contains(&f.op) {
                kept.push(f);
                continue;
            }
            match self.delivery_chain(&state, &f, &with).await {
                Decided::Deliver(content) => {
                    lk(&self.jobs).decided.insert(f.op, content);
                    kept.push(f);
                }
                Decided::Hold(until) => self.hold(f, until),
            }
        }
        let mut b = lk(&self.jobs);
        // Jobs that finished while the chain ran stay behind the ones it decided.
        let newer = std::mem::replace(&mut b.ready, kept);
        b.ready.extend(newer);
        Ok(b.ready.iter().filter(|f| b.decided.contains_key(&f.op)).count())
    }

    /// One delivery through the chain. A job the fold does not hold as undelivered skips it.
    async fn delivery_chain(&self, state: &State, f: &Finished, with: &[OpId]) -> Decided {
        let (_, _, content, _) = split_outcome(f.outcome.clone());
        let Some(job) = state.jobs.get(&f.op).filter(|j| j.delivered.is_none()) else { return Decided::Deliver(content) };
        let mut view = JobView {
            op: f.op,
            call: f.call.clone(),
            tool: state.calls.get(&f.call).map(|c| c.0.clone()).unwrap_or_default(),
            status: f.status.clone(),
            cause: job.cause,
            content,
            deferred: lk(&self.jobs).deferrals.get(&f.op).copied().unwrap_or(0),
            with: with.to_vec(),
        };
        for (cap, run) in &self.reg.deliveries {
            let started = self.hook_invoked(*cap, "delivery");
            let d = run(&view);
            if let Some(end) = self.settle(*cap, d, &mut view, started, false) {
                return end;
            }
        }
        for (cap, h) in &self.reg.hooks {
            if !h.on.contains(&HookEvent::JobDone) || h.if_tool.is_some_and(|t| t != view.tool) {
                continue;
            }
            let started = self.hook_invoked(*cap, h.if_tool.unwrap_or(""));
            let out = tokio::time::timeout(Duration::from_millis(h.timeout_ms.max(1)), (h.run)(HookEvent::JobDone, &payload(&view))).await;
            let (d, timed_out) = match out {
                Ok(o) => (o.delivery, false),
                // Veto-`⊓` like every hook: a timed-out hook never lets unreviewed content through
                // unless it says so.
                Err(_) if h.on_error == OnError::Allow => (Delivery::Keep, true),
                Err(_) => (Delivery::Suppress, true),
            };
            if let Some(end) = self.settle(*cap, d, &mut view, started, timed_out) {
                return end;
            }
        }
        Decided::Deliver(view.content)
    }

    fn hook_invoked(&self, cap: CapabilityId, matcher: &str) -> Instant {
        self.append_reported(
            Principal::system(cap.name),
            EntryKind::HookInvoked { point: format!("{:?}", HookEvent::JobDone), cap: cap.name.into(), matcher: matcher.into() },
        );
        Instant::now()
    }

    /// Logs one run's `HookResult` and applies its decision; `Some` ends the chain.
    fn settle(&self, cap: CapabilityId, d: Delivery, view: &mut JobView, started: Instant, timed_out: bool) -> Option<Decided> {
        let d = match d {
            Delivery::Defer { .. } if view.deferred >= MAX_DEFERRALS => None,
            d => Some(d),
        };
        let word = match &d {
            None => format!("defer refused after {MAX_DEFERRALS} deferrals → keep"),
            Some(Delivery::Keep) => "keep".into(),
            Some(Delivery::Replace(_)) => "replace".into(),
            Some(Delivery::Suppress) => "suppress".into(),
            Some(Delivery::Defer { until }) => format!("defer {}ms", until.saturating_duration_since(Instant::now()).as_millis()),
        };
        self.append_reported(
            Principal::system(cap.name),
            EntryKind::HookResult {
                decision: if timed_out { format!("timeout → {word}") } else { word },
                exit: None,
                stderr_summary: String::new(),
                ms: started.elapsed().as_millis() as u64,
            },
        );
        match d? {
            Delivery::Keep => None,
            Delivery::Replace(content) => {
                view.content = content;
                None
            }
            Delivery::Suppress => Some(Decided::Deliver(vec![ContentBlock::text(jobs::template("suppressed").replace("{cap}", cap.name))])),
            Delivery::Defer { until } => Some(Decided::Hold(until)),
        }
    }

    /// Holds a deferred delivery; when the hold ends it is due again like a fresh finish.
    fn hold(self: &Arc<Self>, f: Finished, until: Instant) {
        {
            let mut b = lk(&self.jobs);
            *b.deferrals.entry(f.op).or_default() += 1;
            b.held.push((f, until));
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
                tokio::time::sleep(left).await;
            }
            if let Some(s) = weak.upgrade() {
                s.release_held(true);
            }
        });
    }

    /// Every held delivery whose hold ended goes back to `ready`, in hold order. With `announce`,
    /// while no turn runs each one is announced like a fresh finish (`JobFinished`).
    pub(crate) fn release_held(&self, announce: bool) {
        let now = Instant::now();
        let mut b = lk(&self.jobs);
        let (due, held): (Vec<_>, Vec<_>) = std::mem::take(&mut b.held).into_iter().partition(|(_, until)| *until <= now);
        b.held = held;
        for (f, _) in due {
            if announce && b.idle {
                self.emit(SessionEvent::JobFinished { op: f.op });
            }
            b.ready.push(f);
        }
    }
}

/// The `JobDone` hook payload: the view, with `content` as text cut to [`PAYLOAD_CHARS`].
fn payload(v: &JobView) -> Value {
    let text = v.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n");
    let cut: String = text.chars().take(PAYLOAD_CHARS).collect();
    serde_json::json!({
        "op": v.op,
        "call": v.call,
        "tool": v.tool,
        "status": v.status,
        "cause": v.cause,
        "content_truncated": cut.len() < text.len(),
        "content": cut,
        "deferred": v.deferred,
        "with": v.with,
    })
}
