//! The live ceiling session modes narrow; only an operator approval widens it
//! (`aho_control::SessionCeiling`). Held weakly: the registry lives inside the session.

use std::sync::{OnceLock, Weak};

use aho_control::{SessionCeiling, Widen, Widened};

use super::*;

pub(crate) struct Ceiling(pub(crate) Arc<OnceLock<Weak<Session>>>);

impl Ceiling {
    fn session(&self) -> Option<Arc<Session>> {
        self.0.get().and_then(Weak::upgrade)
    }
}

impl SessionCeiling for Ceiling {
    fn view(&self) -> PolicyView {
        match self.session() {
            Some(s) => s.policy_view(),
            None => PolicyView {
                epoch: Epoch(0),
                allowed: ConsequenceSet::EMPTY,
                world: aho_cap::BoundWorld::host(),
                workspace: String::new(),
                home: String::new(),
            },
        }
    }
    fn narrow(&self, mode: &str, cut: ConsequenceSet) -> Result<PolicyView, String> {
        self.session().ok_or_else(|| "the session is closed; nothing was narrowed".to_string())?.narrow_for(mode, cut)
    }
    fn request_widen(&self, w: Widen) -> BoxFuture<'_, Widened> {
        Box::pin(async move {
            match self.session() {
                Some(s) => s.widen_for(w).await,
                None => Widened { outcome: ApprovalOutcome::Unavailable, view: self.view() },
            }
        })
    }
}

impl Session {
    /// Refreshes a policy epoch already committed by an authenticated operator. Approval
    /// brokers use this while a turn waits on its validated card; it never invents authority.
    pub fn refresh_operator_policy(&self, _operator: &Operator) -> Result<(), SessionError> {
        let mut policy = lk(&self.policy);
        let state = self.fold()?;
        let world = policy.world().clone();
        let workspace = policy.workspace().to_string();
        let home = policy.home().to_string();
        *policy = match aho_policy::preset(&state.policy.preset) {
            Some(preset) => SandboxPolicy::from_preset(preset, world, workspace, home, state.policy.epoch).narrow(state.policy.allowed),
            None => SandboxPolicy::new(state.policy.allowed, world, workspace, home, state.policy.epoch),
        };
        Ok(())
    }

    /// Applies an authenticated operator's preset between turns. Unlike a mode's meet, this
    /// may widen authority or leave spec mode; `authority` is the operator's `Command/Run`
    /// entry the epoch cites (a widening needs an operator reference).
    pub fn set_preset(&self, operator: &Operator, name: &str, authority: EntryId) -> Result<PolicyView, SessionError> {
        let preset = aho_policy::preset(name)
            .ok_or_else(|| SessionError::NotFound(format!("unknown preset `{name}`; use read-only, workspace, dev, lab, full, or spec")))?;
        let _admission = self.try_mutation()?;
        let mut policy = lk(&self.policy);
        let next = SandboxPolicy::from_preset(
            preset,
            policy.world().clone(),
            policy.workspace().to_string(),
            policy.home().to_string(),
            Epoch(policy.epoch().0.saturating_add(1)),
        );
        self.append(
            operator.principal(),
            EntryKind::PolicyEpoch {
                epoch: next.epoch(),
                allowed: next.allowed(),
                preset: preset.name().into(),
                approval_ref: Some(authority),
            },
        )?;
        *policy = next;
        Ok(policy.view())
    }

    fn narrow_for(&self, mode: &str, cut: ConsequenceSet) -> Result<PolicyView, String> {
        let mut p = lk(&self.policy);
        let allowed = p.allowed().intersect(cut);
        if allowed == p.allowed() {
            return Ok(p.view());
        }
        let next = SandboxPolicy::new(
            allowed,
            p.world().clone(),
            p.workspace().to_string(),
            p.home().to_string(),
            Epoch(p.epoch().0.saturating_add(1)),
        );
        self.append(
            Principal::system(mode),
            EntryKind::PolicyEpoch { epoch: next.epoch(), allowed, preset: next.preset().name().into(), approval_ref: None },
        )
        .map_err(|e| format!("could not record the narrowed policy ({e}); nothing was narrowed"))?;
        *p = next;
        Ok(p.view())
    }

    async fn widen_for(&self, w: Widen) -> Widened {
        let same = |outcome| Widened { outcome, view: self.policy_view() };
        let open = self.fold().map(|s| s.turn.is_some()).unwrap_or(false);
        if !open || self.approval.policy() == AskPolicy::Never {
            return same(ApprovalOutcome::Unavailable);
        }
        let (wants, card) = {
            let p = lk(&self.policy);
            let card = aho_policy::ApprovalCard {
                tool: w.tool.clone(),
                class: Consequence::TalksToHuman,
                args: w.args.clone(),
                reason: aho_policy::DecisionReason::ToolCheck(format!("`{}` asks to restore {}", w.tool, w.to)),
                world: p.world().clone(),
                suggested_cut: None,
                rendered: w.rendered.clone(),
            };
            (ConsequenceSet(w.to.0 & !p.allowed().0), card)
        };
        let Ok(req) = self.append(
            Principal::system("policy"),
            EntryKind::ApprovalRequested { wants, for_cap: w.tool.clone(), call: None, reason: w.rendered.clone() },
        ) else {
            return same(ApprovalOutcome::Unavailable);
        };
        let d = self.approval.request(card).await.normalized();
        // the turn may have ended meanwhile: the loop already cancelled this request
        if !self.fold().map(|s| s.pending_approvals.contains_key(&req.id)).unwrap_or(false) {
            return same(ApprovalOutcome::Cancelled);
        }
        if self.append(Principal::Operator, EntryKind::ApprovalDecided { req: req.id, outcome: d.outcome, scope: d.scope }).is_err() {
            return same(ApprovalOutcome::Unavailable);
        }
        if d.outcome != ApprovalOutcome::AllowedOnce {
            return same(d.outcome);
        }
        let mut p = lk(&self.policy);
        let n = p.widen_by_approval(&self.operator, req.id, w.to);
        let logged = self.append(
            Principal::Operator,
            EntryKind::PolicyEpoch { epoch: n.epoch(), allowed: n.allowed(), preset: n.preset().name().into(), approval_ref: Some(req.id) },
        );
        if logged.is_err() {
            return Widened { outcome: ApprovalOutcome::Unavailable, view: p.view() };
        }
        *p = n;
        Widened { outcome: ApprovalOutcome::AllowedOnce, view: p.view() }
    }
}
