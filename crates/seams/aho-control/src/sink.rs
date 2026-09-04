//! The prompt sink: how a driver runs a turn.

use aho_types::BoxFuture;
use tokio::sync::{mpsc, oneshot};

/// How a driven turn ended.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TurnEnd {
    /// The last non-empty assistant text of the turn ("" when it produced none).
    pub final_text: String,
    /// `Some(reason)` when the turn aborted (the model or the session failed).
    pub failed: Option<String>,
    /// False when the key had already been admitted (a restart re-sent it) and no new turn ran;
    /// the report is then the earlier turn's.
    pub ran: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveError {
    /// Whoever runs the turns is gone (session closed, channel dropped).
    Closed(String),
    /// The turn could not be started or its outcome could not be read.
    Failed(String),
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::Closed(s) | DriveError::Failed(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for DriveError {}

/// Runs turns for drivers.
pub trait PromptSink: Send + Sync {
    /// Runs one turn whose prompt is `text` and whose inbox key is `key`, on behalf of driver `by`
    /// (`schedule`, `workflow`: the prompt's author is `System(by)`); resolves when the turn has
    /// ended. A key the session already admitted runs nothing (exactly once, across restarts). A
    /// turn already running is waited out, never interrupted.
    fn prompt(&self, by: &'static str, text: String, key: String) -> BoxFuture<'_, Result<TurnEnd, DriveError>>;
}

/// One request on a [`ChannelSink`]: run `text` keyed `key` for driver `by`, answer on `reply`
/// when it ended. Dropping `reply` unanswered reads as [`DriveError::Closed`].
#[derive(Debug)]
pub struct SinkRequest {
    pub by: &'static str,
    pub text: String,
    pub key: String,
    pub reply: oneshot::Sender<TurnEnd>,
}

/// Routes driver prompts over a channel to whoever runs the turns.
#[derive(Clone)]
pub struct ChannelSink {
    tx: mpsc::Sender<SinkRequest>,
}

impl ChannelSink {
    /// `buffer` bounds how many prompts may wait at once (drivers send one at a time).
    pub fn new(buffer: usize) -> (ChannelSink, mpsc::Receiver<SinkRequest>) {
        let (tx, rx) = mpsc::channel(buffer.max(1));
        (ChannelSink { tx }, rx)
    }
}

impl PromptSink for ChannelSink {
    fn prompt(&self, by: &'static str, text: String, key: String) -> BoxFuture<'_, Result<TurnEnd, DriveError>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let gone = || DriveError::Closed(format!("nothing is running turns for prompt `{key}` any more; the driver stopped"));
            if self.tx.send(SinkRequest { by, text, key: key.clone(), reply }).await.is_err() {
                return Err(gone());
            }
            answer.await.map_err(|_| gone())
        })
    }
}
