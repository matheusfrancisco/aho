//! Wall time for drivers. Timestamps are UTC milliseconds (`aho_types::Timestamp`).

use std::time::Duration;

use aho_types::{BoxFuture, Timestamp};
use tokio::sync::watch;

pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
    /// Resolves at or after `t`; immediately when `t` has passed.
    fn sleep_until(&self, t: Timestamp) -> BoxFuture<'_, ()>;
}

/// The host's wall clock. Sleeps in naps of at most a minute and re-reads the time after each, so
/// a suspended laptop or a clock step is noticed within a minute rather than at the old deadline.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

const MAX_NAP_MS: u64 = 60_000;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
    fn sleep_until(&self, t: Timestamp) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            loop {
                let now = Timestamp::now();
                if now >= t {
                    return;
                }
                tokio::time::sleep(Duration::from_millis((t.0 - now.0).min(MAX_NAP_MS))).await;
            }
        })
    }
}

/// Time the embedder moves forward itself. Never goes backwards.
#[derive(Debug)]
pub struct SteppedClock {
    tx: watch::Sender<Timestamp>,
}

impl SteppedClock {
    pub fn new(start: Timestamp) -> SteppedClock {
        SteppedClock { tx: watch::Sender::new(start) }
    }
    /// Moves time to `t` (ignored when `t` is earlier than now) and wakes sleepers that are due.
    pub fn set(&self, t: Timestamp) {
        self.tx.send_if_modified(|cur| {
            if t > *cur {
                *cur = t;
                true
            } else {
                false
            }
        });
    }
    pub fn advance(&self, ms: u64) {
        let next = Timestamp(self.now().0.saturating_add(ms));
        self.set(next);
    }
}

impl Clock for SteppedClock {
    fn now(&self) -> Timestamp {
        *self.tx.borrow()
    }
    fn sleep_until(&self, t: Timestamp) -> BoxFuture<'_, ()> {
        let mut rx = self.tx.subscribe();
        Box::pin(async move {
            while *rx.borrow_and_update() < t {
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}
