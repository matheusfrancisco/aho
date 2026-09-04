//! `04`/`10` — the approval channel. One-shot outcomes; a dropped or failing answerer
//! normalizes to `Unavailable` (fail closed); `never` short-circuits in the service, not in a
//! listener. The harness appends `Approval/Requested` before calling [`Approval::request`] and
//! `Approval/Decided` after, both inside the open turn.

#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::{Handle, HandleKind, SeamId};
use aho_log::{ApprovalOutcome, GrantScope};
use aho_policy::ApprovalCard;
use aho_types::*;
use tokio::sync::{mpsc, oneshot};

pub const SEAM: SeamId = SeamId("approval");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskPolicy {
    Ask,
    /// Prompts disabled; the policy context says so up front so the model does not waste a turn.
    Never,
}

/// One answer to one card. Only an allow carries a scope wider than `Once`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decided {
    pub outcome: ApprovalOutcome,
    pub scope: GrantScope,
}

impl Decided {
    pub const UNAVAILABLE: Decided = Decided { outcome: ApprovalOutcome::Unavailable, scope: GrantScope::Once };

    pub fn allowed(scope: GrantScope) -> Decided {
        Decided { outcome: ApprovalOutcome::AllowedOnce, scope }
    }
    pub fn rejected() -> Decided {
        Decided { outcome: ApprovalOutcome::Rejected, scope: GrantScope::Once }
    }
    pub fn cancelled() -> Decided {
        Decided { outcome: ApprovalOutcome::Cancelled, scope: GrantScope::Once }
    }

    /// A refusal never widens anything: its scope is forced to `Once`.
    pub fn normalized(self) -> Decided {
        match self.outcome {
            ApprovalOutcome::AllowedOnce => self,
            outcome => Decided { outcome, scope: GrantScope::Once },
        }
    }
}

pub trait Approval: Send + Sync {
    fn policy(&self) -> AskPolicy;
    /// Waits for exactly one decision on `card`. Never errors: no answer is `Unavailable`.
    fn request(&self, card: ApprovalCard) -> BoxFuture<'_, Decided>;
}

/// `TalksToHuman` class; absent under an async subagent unless bubbled.
#[derive(Clone)]
pub struct ApprovalHandle(pub Arc<dyn Approval>);
impl Handle for ApprovalHandle {
    const KIND: HandleKind = HandleKind::Approval;
}

/// Prompts are off (headless, CI, async agent): every request is `Unavailable`, immediately.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverApproval;

impl Approval for NeverApproval {
    fn policy(&self) -> AskPolicy {
        AskPolicy::Never
    }
    fn request(&self, _card: ApprovalCard) -> BoxFuture<'_, Decided> {
        Box::pin(std::future::ready(Decided::UNAVAILABLE))
    }
}

/// What the answering surface (TUI, web, remote client) receives: the card and the one-shot
/// reply slot. Dropping the slot without sending is `Unavailable`.
pub type ApprovalRequest = (ApprovalCard, oneshot::Sender<Decided>);

/// Routes cards to a surface over a channel. The surface owns the receiver.
#[derive(Clone)]
pub struct ChannelApproval {
    tx: mpsc::Sender<ApprovalRequest>,
}

impl ChannelApproval {
    /// `buffer` bounds how many cards may wait for the surface at once.
    pub fn new(buffer: usize) -> (ChannelApproval, mpsc::Receiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::channel(buffer.max(1));
        (ChannelApproval { tx }, rx)
    }
}

impl Approval for ChannelApproval {
    fn policy(&self) -> AskPolicy {
        if self.tx.is_closed() { AskPolicy::Never } else { AskPolicy::Ask }
    }
    fn request(&self, card: ApprovalCard) -> BoxFuture<'_, Decided> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if self.tx.send((card, reply)).await.is_err() {
                return Decided::UNAVAILABLE;
            }
            match answer.await {
                Ok(d) => d.normalized(),
                Err(_) => Decided::UNAVAILABLE,
            }
        })
    }
}

/// `ask_user_question` payload; answers are `Operator`-authored entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub question: String,
    pub detail: Option<String>,
    /// `(label, description)`.
    pub options: Vec<(String, Option<String>)>,
    pub multi: bool,
    /// e.g. `plan-review` with a named approve option (10).
    pub intent: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_cap::BoundWorld;
    use aho_policy::DecisionReason;

    fn card() -> ApprovalCard {
        ApprovalCard {
            tool: "vm_start".into(),
            class: Consequence::ControlsVM,
            args: serde_json::json!({"name": "dev"}),
            reason: DecisionReason::Preset("workspace".into()),
            world: BoundWorld::host(),
            suggested_cut: Some("preset lab".into()),
            rendered: "`vm_start` is outside preset `workspace`; the user is asked before it runs.".into(),
        }
    }

    #[tokio::test]
    async fn never_is_unavailable_up_front() {
        let a = NeverApproval;
        assert_eq!(a.policy(), AskPolicy::Never);
        assert_eq!(a.request(card()).await, Decided::UNAVAILABLE);
    }

    #[tokio::test]
    async fn channel_delivers_the_card_and_returns_the_answer() {
        let (a, mut rx) = ChannelApproval::new(4);
        assert_eq!(a.policy(), AskPolicy::Ask);
        let surface = tokio::spawn(async move {
            let (c, reply) = rx.recv().await.unwrap();
            assert_eq!(c.tool, "vm_start");
            assert_eq!(c.suggested_cut.as_deref(), Some("preset lab"));
            reply.send(Decided::allowed(GrantScope::Session)).unwrap();
        });
        assert_eq!(a.request(card()).await, Decided { outcome: ApprovalOutcome::AllowedOnce, scope: GrantScope::Session });
        surface.await.unwrap();
    }

    #[tokio::test]
    async fn dropped_answerer_fails_closed() {
        // The surface takes the card and drops the reply slot without answering.
        let (a, mut rx) = ChannelApproval::new(1);
        let surface = tokio::spawn(async move {
            let (_card, reply) = rx.recv().await.unwrap();
            drop(reply);
        });
        assert_eq!(a.request(card()).await, Decided::UNAVAILABLE);
        surface.await.unwrap();

        // No surface at all: the receiver is gone before the request.
        let (a, rx) = ChannelApproval::new(1);
        drop(rx);
        assert_eq!(a.policy(), AskPolicy::Never);
        assert_eq!(a.request(card()).await, Decided::UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_refusal_never_carries_a_wide_scope() {
        let (a, mut rx) = ChannelApproval::new(1);
        tokio::spawn(async move {
            let (_c, reply) = rx.recv().await.unwrap();
            let _ = reply.send(Decided { outcome: ApprovalOutcome::Rejected, scope: GrantScope::UntilRevoked });
        });
        assert_eq!(a.request(card()).await, Decided::rejected());
    }

    #[tokio::test]
    async fn handle_is_the_approval_kind_and_dispatches_dynamically() {
        assert_eq!(ApprovalHandle::KIND, HandleKind::Approval);
        let h = ApprovalHandle(Arc::new(NeverApproval));
        assert_eq!(h.0.request(card()).await.outcome, ApprovalOutcome::Unavailable);
        assert_eq!(Decided::cancelled().outcome, ApprovalOutcome::Cancelled);
    }
}
