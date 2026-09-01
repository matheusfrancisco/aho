//! `04` — the decision chain. Ordered, bypass-immune; steps 1–5 are never lifted by a preset,
//! a mode, an allow rule or a hook.
//!
//! ```text
//! 1. deny rules   2. ask rules   3. tool.check   4. guards (deny-only)   5. safety checks
//! 6. preset ceiling   7. allow rules   8. passthrough (the preset is the allow)
//! ```

use aho_cap::{Decision, ToolDecl};
use aho_types::*;

use crate::paths::{self, Places, Zone};
use crate::rules::{self, Rule, RuleBehavior, Subject};
use crate::shell::{self, RiskLevel};
use crate::{ESCALATABLE, Preset, SandboxPolicy};

/// Closed. One renderer turns it into the sentence the model, the card, the HUD and `aho explain` show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionReason {
    Rule {
        origin: String,
        rule: String,
    },
    Preset(String),
    Hook(String),
    Guard(String),
    SafetyCheck(SafetyKind),
    Classifier,
    WorkingDir,
    AsyncAgent,
    Delegation,
    Escalation,
    Unavailable,
    Passthrough,
    /// The tool's own `check` (step 3), with its sentence.
    ToolCheck(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafetyKind {
    Catastrophic,
    Confirm,
    ProtectedPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainOutcome {
    Allow(DecisionReason),
    Ask(DecisionReason),
    Deny(DecisionReason),
    /// Refused by the ceiling under a confining world; the result carries the escalation hint.
    DenyEscalatable {
        reason: DecisionReason,
        targets: Vec<Consequence>,
    },
}

impl ChainOutcome {
    pub fn reason(&self) -> &DecisionReason {
        match self {
            ChainOutcome::Allow(r) | ChainOutcome::Ask(r) | ChainOutcome::Deny(r) => r,
            ChainOutcome::DenyEscalatable { reason, .. } => reason,
        }
    }
    pub fn is_allow(&self) -> bool {
        matches!(self, ChainOutcome::Allow(_))
    }
}

/// Input to the chain: the model's bytes plus what the resolver knows.
pub struct CallUnderReview<'a> {
    pub decl: &'a ToolDecl,
    pub args: &'a Value,
    pub by: &'a Principal,
    pub rules: &'a [Rule],
    pub policy: &'a SandboxPolicy,
    /// `(capability, reason)` for every guard that refused this call.
    pub guard_denials: Vec<(String, String)>,
    /// Escalation targets the call requested (already parsed by the harness).
    pub escalate: Vec<Consequence>,
    /// Runtime tool name for dynamically registered tools (MCP), whose `decl` is a class template;
    /// `None` means `decl.name`.
    pub tool_name: Option<&'a str>,
}

impl CallUnderReview<'_> {
    pub fn name(&self) -> &str {
        self.tool_name.unwrap_or(self.decl.name)
    }
}

fn rule_reason(r: &Rule) -> DecisionReason {
    DecisionReason::Rule { origin: r.origin.clone(), rule: r.to_string() }
}

/// `04` — ordered, bypass-immune. Steps 1–5 cannot be lifted by any later step.
pub fn decide(call: &CallUnderReview<'_>) -> ChainOutcome {
    let policy = call.policy;
    let class = call.decl.class;
    let allowed = policy.allowed();
    let places = Places::new(policy.workspace(), policy.home());
    let subject = Subject::of(call.args, &places);
    let first = |b: RuleBehavior| call.rules.iter().find(|r| r.behavior == b && r.matches(call.decl, call.name(), &subject));

    // 1. deny rules
    if let Some(r) = first(RuleBehavior::Deny) {
        return ChainOutcome::Deny(rule_reason(r));
    }
    // 2. ask rules
    if let Some(r) = first(RuleBehavior::Ask) {
        return ChainOutcome::Ask(rule_reason(r));
    }
    // 3. the tool's own opinion
    let tool_allowed = match (call.decl.check)(call.args, &policy.view()) {
        Some(Decision::Deny(why)) => return ChainOutcome::Deny(DecisionReason::ToolCheck(why)),
        Some(Decision::Ask(why)) => return ChainOutcome::Ask(DecisionReason::ToolCheck(why)),
        Some(Decision::Allow) => true,
        None => false,
    };
    // 4. guards, deny-only
    if let Some((cap, why)) = call.guard_denials.first() {
        return ChainOutcome::Deny(DecisionReason::Guard(format!("{cap}: {why}")));
    }
    let spec_blocked = spec_blocks(policy, class, call.args, &places);
    let within_ceiling = allowed.contains(class) && !spec_blocked;
    // 5. safety checks — lexical, never lifted
    match classify(call.args, &places, class != Consequence::ReadOnly) {
        Some((SafetyKind::Catastrophic, _)) => return ChainOutcome::Deny(DecisionReason::SafetyCheck(SafetyKind::Catastrophic)),
        Some((SafetyKind::ProtectedPath, _)) => return ChainOutcome::Deny(DecisionReason::SafetyCheck(SafetyKind::ProtectedPath)),
        Some((SafetyKind::Confirm, _)) => {
            // A justification refines inside the ceiling; it never widens (12).
            let justified = call.args.get("justification").and_then(Value::as_str).is_some_and(shell::justification_ok);
            return if justified && within_ceiling {
                ChainOutcome::Allow(DecisionReason::SafetyCheck(SafetyKind::Confirm))
            } else {
                ChainOutcome::Ask(DecisionReason::SafetyCheck(SafetyKind::Confirm))
            };
        }
        None => {}
    }
    // 6. preset ceiling
    let preset_reason = DecisionReason::Preset(policy.preset().name().to_string());
    if !allowed.contains(class) {
        let human = allowed.contains(Consequence::TalksToHuman);
        let world_grants = policy.world().confines && human;
        let escalation_valid = !call.escalate.is_empty()
            && world_grants
            && call.escalate.contains(&class)
            && call.escalate.iter().all(|t| ESCALATABLE.contains(*t) && !allowed.contains(*t));
        if escalation_valid {
            return ChainOutcome::Ask(DecisionReason::Escalation);
        }
        if world_grants && ESCALATABLE.contains(class) {
            return ChainOutcome::DenyEscalatable { reason: preset_reason, targets: vec![class] };
        }
        return if human { ChainOutcome::Ask(preset_reason) } else { ChainOutcome::Deny(preset_reason) };
    }
    if spec_blocked {
        return ChainOutcome::Deny(DecisionReason::Preset(Preset::Spec.name().to_string()));
    }
    // 7. allow rules
    if let Some(r) = first(RuleBehavior::Allow) {
        return ChainOutcome::Allow(rule_reason(r));
    }
    // 8. passthrough: past step 6 the class is inside the ceiling, and the preset is the allow
    if tool_allowed {
        ChainOutcome::Allow(DecisionReason::ToolCheck(format!("`{}` allowed this call itself", call.name())))
    } else {
        ChainOutcome::Allow(DecisionReason::Passthrough)
    }
}

/// `14`: under spec mode a `WorkspaceWrite` call must name only paths under `<workspace>/spec/`.
/// A write that names no path cannot be checked and is refused.
fn spec_blocks(policy: &SandboxPolicy, class: Consequence, args: &Value, places: &Places) -> bool {
    if !policy.writes_limited_to_spec() || class != Consequence::WorkspaceWrite {
        return false;
    }
    let Some(ws) = places.workspace.as_deref() else { return true };
    let spec = format!("{ws}/spec");
    let ps = rules::path_args(args);
    ps.is_empty() || ps.iter().any(|p| !rules::resolve_tool_path(p, places).is_some_and(|abs| paths::strictly_within(&abs, &spec)))
}

/// Step 5: lexical, parser-independent, never lifted. Shell commands (a string `command`
/// argument) go through [`shell::assess`]; tools writing a path get the protected-path list.
/// Read-only fs tools (`read`, `glob`, `grep`, `ls`, …) are exempt from the path list.
pub fn safety_check(tool: &str, args: &Value, workspace: &str, home: &str) -> Option<(SafetyKind, String)> {
    const READERS: &[&str] = &["read", "glob", "grep", "ls", "list", "stat", "view", "find", "search"];
    classify(args, &Places::new(workspace, home), !READERS.contains(&tool))
}

fn classify(args: &Value, places: &Places, writes: bool) -> Option<(SafetyKind, String)> {
    if let Some(cmd) = args.get("command").and_then(Value::as_str) {
        let (ws, home) = (places.workspace.as_deref().unwrap_or(""), places.home.as_deref().unwrap_or(""));
        let risk = shell::assess(cmd, ws, home);
        return match risk.level {
            RiskLevel::Catastrophic => Some((SafetyKind::Catastrophic, shell::reflection_prompt(&risk))),
            RiskLevel::Confirm => Some((SafetyKind::Confirm, shell::reflection_prompt(&risk))),
            RiskLevel::Low | RiskLevel::Safe => None,
        };
    }
    if !writes {
        return None;
    }
    for p in rules::path_args(args) {
        let Some(abs) = rules::resolve_tool_path(&p, places) else { continue };
        match paths::zone(&abs, false, places) {
            z @ (Zone::Root | Zone::Device | Zone::Credential | Zone::Home | Zone::System) => {
                return Some((
                    SafetyKind::Catastrophic,
                    format!("Writing {abs} ({}) cannot be confirmed; the user must make this change themselves.", z.describe()),
                ));
            }
            Zone::Protected => {
                return Some((
                    SafetyKind::ProtectedPath,
                    format!("{abs} is inside .git/ or .aho/, which agents may not write; the user can make this change themselves."),
                ));
            }
            _ => {}
        }
    }
    None
}

/// A hook's decision is `⊓ current`: it may turn Allow into Ask or Deny, Ask into Deny; its
/// Allow never lifts anything (steps 1–5 above all).
pub fn meet(current: ChainOutcome, hook: Decision) -> ChainOutcome {
    match (current, hook) {
        (c @ (ChainOutcome::Deny(_) | ChainOutcome::DenyEscalatable { .. }), _) => c,
        (_, Decision::Deny(why)) => ChainOutcome::Deny(DecisionReason::Hook(why)),
        (ChainOutcome::Allow(_), Decision::Ask(why)) => ChainOutcome::Ask(DecisionReason::Hook(why)),
        (c, _) => c,
    }
}
