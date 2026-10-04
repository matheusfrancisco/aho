//! Hosts the `aho-control` drivers (schedules and workflows) next to a live
//! `aho_session::Session`. The daemon and CLI call this library; it is not a capability.
//!
//! * [`SessionSink`]: `PromptSink` over a session. A running turn is waited out, never
//!   interrupted; a key the log already holds runs nothing and reports the earlier turn.
//! * [`DriverSeed`]: the services to seed *before* `Session::boot`. `tool-schedule` and
//!   `tool-workflow` build only when a driver will run for the session.
//! * [`host`]: after boot, spawns the scheduler, the ralph loop driver, the forwarder that
//!   wakes them (a `schedule/*` entry wakes the scheduler, every turn end wakes the loop driver),
//!   and the jobs wake driver (`jobs.wake = idle`); [`host_jobs`] spawns the jobs driver alone.
//! * [`settle`]: print mode waits for the session's jobs and runs their wake turns before exit.

#![forbid(unsafe_code)]

mod jobs;

pub use jobs::{Settled, job_list, prompt_when_idle, settle};

use std::sync::Arc;
use std::time::Duration;

use aho_cap::Registry;
use aho_control::{Clock, DriveError, PromptSink, TurnEnd, admitted, branch, turn_report};
use aho_log::{EntryKind, Transition};
use aho_session::{Session, SessionError, SessionEvent};
use aho_types::{BoxFuture, Principal};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tool_schedule::{Scheduler, SchedulerLink};
use tool_workflow::{LoopLink, Looper};

/// Wait this long for an `Idle` before trying a busy session again anyway (covers an `Idle`
/// sent before we started listening, while the finishing turn still held the session).
pub(crate) const BUSY_RETRY: Duration = Duration::from_millis(500);

pub struct SessionSink {
    session: Arc<Session>,
}

impl SessionSink {
    pub fn new(session: Arc<Session>) -> SessionSink {
        SessionSink { session }
    }

    fn report(&self, key: &str) -> Result<Option<TurnEnd>, DriveError> {
        let entries = branch(&**self.session.log()).map_err(DriveError::Failed)?;
        Ok(turn_report(&entries, key))
    }
}

impl PromptSink for SessionSink {
    /// The prompt is recorded as the driver's (`System(by)`), never the operator's; the key
    /// (`schedule:<id>:<n>`) names the occurrence so a redelivery is admitted once.
    fn prompt(&self, by: &'static str, text: String, key: String) -> BoxFuture<'_, Result<TurnEnd, DriveError>> {
        Box::pin(async move {
            loop {
                let entries = branch(&**self.session.log()).map_err(DriveError::Failed)?;
                if admitted(&entries, &key) {
                    let earlier = turn_report(&entries, &key).unwrap_or_default();
                    return Ok(TurnEnd { ran: false, ..earlier });
                }
                let mut events = self.session.subscribe();
                match self.session.prompt_as(Principal::system(by), text.clone(), Some(key.clone())).await {
                    Ok(transition) => {
                        let failed = match &transition {
                            Transition::Aborted(why) => Some(why.clone()),
                            _ => None,
                        };
                        let r = self.report(&key)?.unwrap_or(TurnEnd { final_text: String::new(), failed: None, ran: true });
                        return Ok(TurnEnd { failed: r.failed.or(failed), ran: true, ..r });
                    }
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
                    Err(e) => {
                        return Err(DriveError::Failed(format!(
                            "the session could not run the {by} driver's prompt `{key}` ({e}); the driver stopped and will re-send it when restarted"
                        )));
                    }
                }
            }
        })
    }
}

/// Links to seed before boot. Keep it and pass it to [`host`] after boot.
#[derive(Clone, Default)]
pub struct DriverSeed {
    pub schedule: SchedulerLink,
    pub workflow: LoopLink,
}

impl DriverSeed {
    pub fn new() -> DriverSeed {
        DriverSeed::default()
    }
    /// Adds `aho.driver.schedule` and `aho.driver.workflow` to the boot seed.
    pub fn seed_into(&self, seed: &mut Registry) {
        seed.services.insert(tool_schedule::SERVICE.into(), Arc::new(self.schedule.clone()));
        seed.services.insert(tool_workflow::SERVICE.into(), Arc::new(self.workflow.clone()));
    }
}

/// The running drivers of one session. Keep it as long as the session is hosted: dropping it
/// stops the drivers (a fired turn in flight finishes in the session; its record is written
/// by the next host, which re-sends the same key).
pub struct Drivers {
    stop: watch::Sender<bool>,
    tasks: Vec<(&'static str, JoinHandle<Result<(), String>>)>,
}

impl Drivers {
    /// Names of the drivers running (`scheduler`, `ralph`, `jobs`), for status lines.
    pub fn running(&self) -> Vec<&'static str> {
        self.tasks.iter().filter(|(n, t)| *n != "forwarder" && !t.is_finished()).map(|(n, _)| *n).collect()
    }

    /// Stops every driver and returns the errors any of them stopped with, as `name: message`.
    pub async fn shutdown(self) -> Vec<String> {
        self.stop.send_replace(true);
        let mut errors = Vec::new();
        for (name, task) in self.tasks {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => errors.push(format!("{name}: {e}")),
                Err(e) => errors.push(format!("{name}: the driver task ended abnormally ({e})")),
            }
        }
        errors
    }
}

async fn stopped(mut rx: watch::Receiver<bool>) {
    // a dropped sender means stop as well
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            return;
        }
    }
}

/// Spawns the drivers for `session`: the scheduler when `tool-schedule` is active in it, the
/// ralph loop driver when `tool-workflow` is, the forwarder that wakes them, and the jobs wake
/// driver when `jobs.wake = idle`.
pub fn host(seed: &DriverSeed, session: Arc<Session>, clock: Arc<dyn Clock>) -> Drivers {
    let (stop, stop_rx) = watch::channel(false);
    let sink: Arc<dyn PromptSink> = Arc::new(SessionSink::new(session.clone()));
    let log = session.log().clone();
    let kinds = &session.registry().entry_kinds;
    let (with_schedule, with_loop) = (kinds.contains_key(tool_schedule::NS), kinds.contains_key(tool_workflow::NS));
    let mut tasks: Vec<(&'static str, JoinHandle<Result<(), String>>)> = Vec::new();
    if with_schedule {
        let s = Scheduler::new(log.clone(), sink.clone(), clock.clone(), seed.schedule.clone());
        let rx = stop_rx.clone();
        tasks.push(("scheduler", tokio::spawn(async move { s.run(stopped(rx)).await })));
    }
    if with_loop {
        let l = Looper::new(log, sink, clock, seed.workflow.clone());
        let rx = stop_rx.clone();
        tasks.push(("ralph", tokio::spawn(async move { l.run(stopped(rx)).await })));
    }
    spawn_jobs(&mut tasks, &session, &stop_rx);
    let (sched, looper) = (seed.schedule.clone(), seed.workflow.clone());
    let mut events = session.subscribe();
    tasks.push((
        "forwarder",
        tokio::spawn(async move {
            let stop = stopped(stop_rx);
            tokio::pin!(stop);
            loop {
                tokio::select! {
                    () = &mut stop => return Ok(()),
                    ev = events.recv() => match ev {
                        Ok(SessionEvent::Entry(e)) => {
                            if let EntryKind::Custom { ns, .. } = &e.kind && ns == tool_schedule::NS {
                                sched.wake();
                            }
                        }
                        Ok(SessionEvent::Idle { .. }) => looper.wake(),
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            sched.wake();
                            looper.wake();
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    },
                }
            }
        }),
    ));
    Drivers { stop, tasks }
}

/// The jobs wake driver alone, for hosts that run no schedules or workflows (`aho run`'s
/// interactive loop). Nothing runs unless `jobs.wake = idle`.
pub fn host_jobs(session: Arc<Session>) -> Drivers {
    let (stop, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    spawn_jobs(&mut tasks, &session, &stop_rx);
    Drivers { stop, tasks }
}

fn spawn_jobs(tasks: &mut Vec<(&'static str, JoinHandle<Result<(), String>>)>, session: &Arc<Session>, stop: &watch::Receiver<bool>) {
    if session.jobs_config().wake == aho_session::WakeMode::Idle {
        tasks.push(("jobs", tokio::spawn(jobs::wake_driver(session.clone(), stopped(stop.clone())))));
    }
}
