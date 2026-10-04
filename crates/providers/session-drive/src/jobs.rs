//! The jobs side of hosting a session: the wake driver (`jobs.wake = idle`: a job that finishes
//! while no turn runs gets a wake turn), print mode's settle (wait for running jobs and run their
//! wake turns before exiting, bounded by `jobs.settle_timeout_s`), and prompting a session a wake
//! turn may hold.

use std::sync::Arc;
use std::time::Duration;

use aho_log::Transition;
use aho_session::{AbortScope, JobInfo, Session, SessionError, SessionEvent, Woke};
use tokio::sync::{broadcast, watch};

use crate::BUSY_RETRY;

/// Runs a wake turn whenever `JobFinished` fires and no turn runs, until `stop`. A finish while a
/// turn runs is delivered by that turn; one announced as it ends finds the session busy for a
/// moment, so a busy session is tried again after its `Idle` or [`BUSY_RETRY`]. Outcomes waiting
/// when the driver starts (jobs a restart recovered) wait for the next finish or prompt. On
/// `stop` a wake turn in flight is aborted and settles before the driver returns.
pub(crate) async fn wake_driver(session: Arc<Session>, stop: impl Future<Output = ()>) -> Result<(), String> {
    tokio::pin!(stop);
    let mut events = session.subscribe();
    let mut due = false;
    loop {
        if due {
            let woke = session.wake();
            tokio::pin!(woke);
            let woke = tokio::select! {
                r = &mut woke => r,
                () = &mut stop => {
                    session.abort();
                    return woke.await.map(|_| ()).map_err(|e| format!("the wake turn did not settle ({e})"));
                }
            };
            match woke {
                Ok(Woke::Ran(_)) => continue,
                Ok(Woke::Nothing | Woke::BudgetSpent { .. }) => due = false,
                Err(SessionError::Busy) => {}
                Err(e) => {
                    return Err(format!("a wake turn for finished jobs could not run ({e}); jobs are delivered with the next prompt"));
                }
            }
        }
        let retry = due;
        tokio::select! {
            () = &mut stop => return Ok(()),
            ev = events.recv() => match ev {
                Ok(SessionEvent::JobFinished { .. }) | Err(broadcast::error::RecvError::Lagged(_)) => due = true,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Ok(_) => {}
            },
            () = tokio::time::sleep(BUSY_RETRY), if retry => {}
        }
    }
}

/// Runs an operator prompt, waiting out a turn already running (a wake or driver turn) instead
/// of failing `Busy`.
pub async fn prompt_when_idle(session: &Arc<Session>, text: String) -> Result<Transition, SessionError> {
    loop {
        let mut events = session.subscribe();
        match session.prompt(text.clone(), None).await {
            Err(SessionError::Busy) => {
                let idle = async {
                    loop {
                        match events.recv().await {
                            Ok(SessionEvent::Idle { .. }) | Err(broadcast::error::RecvError::Closed) => break,
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        }
                    }
                };
                tokio::select! {
                    () = idle => {}
                    () = tokio::time::sleep(BUSY_RETRY) => {}
                }
            }
            other => return other,
        }
    }
}

/// How [`settle`] ended.
#[derive(Debug, Default)]
pub struct Settled {
    /// The wake turns it ran, in order, with their transitions.
    pub wakes: Vec<Transition>,
    /// Jobs still running when the wait stopped; they were cancelled, and their outcomes are not
    /// delivered in this process.
    pub cancelled: Vec<JobInfo>,
    /// Finished jobs whose outcome was not delivered when the wait stopped.
    pub undelivered: usize,
    /// The wait hit its timeout (`jobs.settle_timeout_s`).
    pub timed_out: bool,
    /// `jobs.wake_budget` was spent with outcomes still waiting.
    pub budget_spent: bool,
    /// The operator interrupted the wait.
    pub interrupted: bool,
}

/// Print mode, before the process exits: waits for the session's jobs and runs their wake turns
/// (whatever `jobs.wake` says: no later prompt comes in this process), so the last answer
/// accounts for them. Stops at `timeout`, when `jobs.wake_budget` is spent, or once `interrupt`
/// turns true; then the jobs still running are cancelled and listed.
pub async fn settle(session: &Arc<Session>, timeout: Duration, mut interrupt: watch::Receiver<bool>) -> Result<Settled, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut events = session.subscribe();
    let mut out = Settled::default();
    let mut can_interrupt = true;
    loop {
        if *interrupt.borrow() {
            out.interrupted = true;
            break;
        }
        match session.wake().await {
            Ok(Woke::Ran(t)) => {
                out.wakes.push(t);
                continue;
            }
            Ok(Woke::BudgetSpent { .. }) => {
                out.budget_spent = true;
                break;
            }
            Ok(Woke::Nothing) | Err(SessionError::Busy) => {}
            Err(e) => return Err(format!("a wake turn for finished jobs could not run ({e})")),
        }
        if session.jobs().map_err(|e| e.to_string())?.is_empty() {
            return Ok(out);
        }
        tokio::select! {
            ev = events.recv() => if let Err(broadcast::error::RecvError::Closed) = ev {
                return Err("the session closed while its jobs were settling".into());
            },
            // a closed channel: nobody can interrupt any more, so only jobs and the timeout end the wait
            r = interrupt.changed(), if can_interrupt => can_interrupt = r.is_ok(),
            () = tokio::time::sleep_until(deadline) => {
                out.timed_out = true;
                break;
            }
        }
    }
    out.cancelled = session.jobs().map_err(|e| e.to_string())?.into_iter().filter(|j| j.status.is_none()).collect();
    for j in &out.cancelled {
        session.abort_scope(AbortScope::Op(j.op));
    }
    out.undelivered = session.jobs_waiting();
    Ok(out)
}

/// One line per cancelled job, for the operator: `op 3 (bash)`.
pub fn job_list(jobs: &[JobInfo]) -> String {
    jobs.iter().map(|j| format!("op {} ({})", j.op.0, j.tool)).collect::<Vec<_>>().join(", ")
}
