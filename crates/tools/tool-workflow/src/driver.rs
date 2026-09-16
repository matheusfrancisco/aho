//! The ralph loop driver, hosted next to the session like the scheduler.
//!
//! After every turn end (the host calls [`LoopLink::wake`] on each `Idle`) it folds the
//! `workflow/*` entries and asks the planner ([`Workflows::next`]) what the running loop needs:
//! feed iteration `n` through the [`PromptSink`] (key `workflow:<id>:<n>`), then append
//! `workflow/Iteration` with whether that turn's final text carried the promise; or close the
//! loop (`completed` / `exhausted`). A loop survives restarts: its state is the log, and a
//! re-sent key the session already admitted runs nothing.

use std::sync::Arc;

use aho_control::{Clock, PromptSink, branch, record};
use aho_log::LogStore;
use tokio::sync::Notify;

use crate::fold::{Event, LoopPlan, NS, Phase, Workflows, load};

/// Principal of the driver's records: `System("workflow")`.
pub const DRIVER: &str = "workflow";
/// Service a host seeds when it runs a [`Looper`]; without it the ralph tools refuse to build.
pub const SERVICE: &str = "aho.driver.workflow";

/// The host's handle on a running loop driver: seeded as [`SERVICE`], then handed to
/// [`Looper::new`]. `wake()` after every turn end.
#[derive(Clone, Default)]
pub struct LoopLink {
    wake: Arc<Notify>,
}

impl LoopLink {
    pub fn new() -> LoopLink {
        LoopLink::default()
    }
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

pub struct Looper {
    log: Arc<dyn LogStore>,
    sink: Arc<dyn PromptSink>,
    clock: Arc<dyn Clock>,
    link: LoopLink,
}

fn put(log: &dyn LogStore, e: &Event) -> Result<(), String> {
    record(log, DRIVER, NS, e.name(), e.body().map_err(|x| x.to_string())?).map(|_| ())
}

impl Looper {
    pub fn new(log: Arc<dyn LogStore>, sink: Arc<dyn PromptSink>, clock: Arc<dyn Clock>, link: LoopLink) -> Looper {
        Looper { log, sink, clock, link }
    }

    pub fn workflows(&self) -> Result<Workflows, String> {
        load(&branch(&*self.log)?).map_err(|e| format!("the ralph loops in the session log do not replay ({e}); the loop driver stopped"))
    }

    /// One planning pass. `false` when no loop needs anything.
    pub async fn step(&self) -> Result<bool, String> {
        match self.workflows()?.next(self.clock.now()).map_err(|e| e.to_string())? {
            LoopPlan::Idle => Ok(false),
            LoopPlan::Finish { event } => put(&*self.log, &event).map(|()| true),
            LoopPlan::Feed { id, n, key, text } => {
                let end = self.sink.prompt(DRIVER, text, key).await.map_err(|e| e.to_string())?;
                let now = self.workflows()?;
                // stopped during its own iteration (ralph_stop in that turn, or the operator): nothing to record
                if now.get(&id).is_some_and(|w| w.phase == Phase::Running) {
                    for e in now.after_turn(&id, n, &end, self.clock.now()).map_err(|e| e.to_string())? {
                        put(&*self.log, &e)?;
                    }
                }
                Ok(true)
            }
        }
    }

    /// Runs until `stop` resolves; a fed turn in flight is left to the session.
    pub async fn run(&self, stop: impl Future<Output = ()>) -> Result<(), String> {
        tokio::pin!(stop);
        loop {
            loop {
                tokio::select! {
                    () = &mut stop => return Ok(()),
                    more = self.step() => if !more? { break },
                }
            }
            tokio::select! {
                () = &mut stop => return Ok(()),
                () = self.link.wake.notified() => {}
            }
        }
    }
}
