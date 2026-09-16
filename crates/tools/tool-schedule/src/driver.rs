//! The scheduler driver: hosted next to a live session (daemon, CLI), never inside a tool.
//!
//! It folds the `schedule/*` entries of the session log, sleeps until the earliest occurrence,
//! fires it through the [`PromptSink`] as a new turn keyed `schedule:<id>:<n>`, and appends
//! `schedule/Fired` once that turn has ended. Missed-while-down is explicit: on start (resume) or
//! after a long turn, a schedule whose occurrences piled up fires **once**, late, and its `Fired`
//! record counts the rest in `skipped`; its next occurrence is the first one after that fire.

use std::sync::Arc;

use aho_control::{Clock, PromptSink, branch, record};
use aho_log::LogStore;
use tokio::sync::Notify;

use crate::fold::{Event, NS, Schedules, load};

/// Principal of the driver's records: `System("schedule")`.
pub const DRIVER: &str = "schedule";
/// Service a host seeds when it runs a [`Scheduler`] for the session; without it the schedule
/// tools refuse to build (a schedule nothing fires would be a lie).
pub const SERVICE: &str = "aho.driver.schedule";

/// The host's handle on a running scheduler: seeded as [`SERVICE`] before boot, then handed to
/// [`Scheduler::new`]. `wake()` makes the driver re-read the log (call it on every `schedule/*` entry).
#[derive(Clone, Default)]
pub struct SchedulerLink {
    wake: Arc<Notify>,
}

impl SchedulerLink {
    pub fn new() -> SchedulerLink {
        SchedulerLink::default()
    }
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

pub struct Scheduler {
    log: Arc<dyn LogStore>,
    sink: Arc<dyn PromptSink>,
    clock: Arc<dyn Clock>,
    link: SchedulerLink,
}

impl Scheduler {
    pub fn new(log: Arc<dyn LogStore>, sink: Arc<dyn PromptSink>, clock: Arc<dyn Clock>, link: SchedulerLink) -> Scheduler {
        Scheduler { log, sink, clock, link }
    }

    /// The schedule set the log folds to right now.
    pub fn schedules(&self) -> Result<Schedules, String> {
        load(&branch(&*self.log)?).map_err(|e| format!("the schedules in the session log do not replay ({e}); the scheduler stopped"))
    }

    /// Fires the earliest occurrence due now, waits for its turn, and records it. `None` when
    /// nothing is due. A schedule cancelled while its turn ran is not recorded (it has no next
    /// occurrence to advance).
    pub async fn fire_next(&self) -> Result<Option<Event>, String> {
        let now = self.clock.now();
        let Some(due) = self.schedules()?.next_due(now).map_err(|e| e.to_string())? else { return Ok(None) };
        self.sink.prompt(DRIVER, due.text, due.key).await.map_err(|e| e.to_string())?;
        if self.schedules()?.stage(std::slice::from_ref(&due.event)).is_ok() {
            record(&*self.log, DRIVER, NS, due.event.name(), due.event.body().map_err(|e| e.to_string())?)?;
        }
        Ok(Some(due.event))
    }

    /// Runs until `stop` resolves (a fired turn in flight is left to the session; the restart
    /// re-sends its key, which the session has already admitted).
    pub async fn run(&self, stop: impl Future<Output = ()>) -> Result<(), String> {
        tokio::pin!(stop);
        loop {
            loop {
                tokio::select! {
                    () = &mut stop => return Ok(()),
                    fired = self.fire_next() => if fired?.is_none() { break },
                }
            }
            let wake_at = self.schedules()?.next_wake();
            let nap = async {
                match wake_at {
                    Some(t) => self.clock.sleep_until(t).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                () = &mut stop => return Ok(()),
                () = self.link.wake.notified() => {}
                () = nap => {}
            }
        }
    }
}
