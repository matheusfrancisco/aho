//! `04` — the live session ceiling as a mode sees it. The lattice is meet-only: [`SessionCeiling::narrow`]
//! needs no approval; growth exists only as an operator decision the implementation obtains itself
//! ([`SessionCeiling::request_widen`]) and logs as `Policy/Epoch{by: Operator, approval_ref}`.
//! Children inherit the narrowed policy because it *is* the session policy (a guard would not
//! reach them: a subagent's policy is derived from the parent's, `04` "Children").

use std::sync::Arc;

use aho_cap::PolicyView;
use aho_log::ApprovalOutcome;
use aho_types::*;

/// Service name under which the session seeds a [`CeilingHandle`].
pub const CEILING_SERVICE: &str = "aho.ceiling";

#[derive(Clone)]
pub struct CeilingHandle(pub Arc<dyn SessionCeiling>);

/// What a widening asks the operator.
#[derive(Clone, Debug, PartialEq)]
pub struct Widen {
    /// The tool (or command) asking; the card and `Approval/Requested.for_cap` name it.
    pub tool: String,
    /// Classes to add back: the result is `allowed ∪ to`.
    pub to: ConsequenceSet,
    /// Shown on the card (e.g. the plan).
    pub args: Value,
    /// The sentence the operator reads.
    pub rendered: String,
}

#[derive(Clone, Debug)]
pub struct Widened {
    pub outcome: ApprovalOutcome,
    /// The policy after the decision (unchanged unless `outcome` is `AllowedOnce`).
    pub view: PolicyView,
}

/// Implemented by the session over its own `SandboxPolicy`.
///
/// Contract, checked by the log fold (`policy.widen_requires_operator_approval`):
/// * `narrow(mode, cut)` sets `allowed ∩ cut` in a new epoch and logs
///   `PolicyEpoch{epoch+1, allowed, preset, approval_ref: None}` as `System(mode)`. A cut that
///   removes nothing changes nothing and logs nothing.
/// * `request_widen(w)` logs `Approval/Requested{wants: w.to \ allowed, for_cap: w.tool}`, raises
///   the card on the session's approval channel, logs `Approval/Decided` as `Operator`, and only on
///   `AllowedOnce` sets `allowed ∪ w.to` in a new epoch, logged as
///   `PolicyEpoch{by: Operator, approval_ref: Some(request)}`. Both approval entries sit inside the
///   open turn; outside a turn, or with prompts off, it answers `Unavailable` without asking.
pub trait SessionCeiling: Send + Sync {
    /// The policy the next call is reviewed under.
    fn view(&self) -> PolicyView;
    /// Meet the live policy with `cut` on behalf of `mode`.
    fn narrow(&self, mode: &str, cut: ConsequenceSet) -> Result<PolicyView, String>;
    /// Ask the operator to widen the live policy by `w.to`.
    fn request_widen(&self, w: Widen) -> BoxFuture<'_, Widened>;
}
