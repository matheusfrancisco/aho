//! The one renderer: every decision becomes the sentence the model, the approval card,
//! the HUD and `aho explain` show. Message voice: what happened, what we did, how to
//! change it; no internal nouns.

use aho_cap::{BoundWorld, ToolDecl};
use aho_log::ApprovalOutcome;
use aho_types::*;

use crate::chain::{ChainOutcome, DecisionReason, SafetyKind};
use crate::{Preset, SandboxPolicy};

/// The reason, as a clause that completes "`tool` …".
fn clause(r: &DecisionReason) -> String {
    match r {
        DecisionReason::Rule { origin, rule } => format!("rule `{rule}` from {origin}"),
        DecisionReason::Preset(p) if p == "spec" => "spec mode, where writes are limited to spec/".to_string(),
        DecisionReason::Preset(p) => format!("preset `{p}`"),
        DecisionReason::Hook(h) if h.is_empty() => "a hook".to_string(),
        DecisionReason::Hook(h) => format!("a hook ({h})"),
        DecisionReason::Guard(g) => format!("a guard ({g})"),
        DecisionReason::SafetyCheck(SafetyKind::Catastrophic) => "the safety check for irreversible damage".to_string(),
        DecisionReason::SafetyCheck(SafetyKind::Confirm) => "the safety check for irreversible commands".to_string(),
        DecisionReason::SafetyCheck(SafetyKind::ProtectedPath) => "the protected-path check".to_string(),
        DecisionReason::Classifier => "the safety classifier".to_string(),
        DecisionReason::WorkingDir => "the working-directory check".to_string(),
        DecisionReason::AsyncAgent => "the background-agent rule".to_string(),
        DecisionReason::Delegation => "the delegating agent's policy".to_string(),
        DecisionReason::Escalation => "an escalation request".to_string(),
        DecisionReason::Unavailable => "the approval channel".to_string(),
        DecisionReason::Passthrough => "the current preset".to_string(),
        DecisionReason::ToolCheck(t) => format!("the tool's own check ({t})"),
    }
}

fn deny_sentence(r: &DecisionReason, tool: &str) -> String {
    match r {
        DecisionReason::Rule { origin, rule } => {
            format!("`{tool}` was not run: rule `{rule}` from {origin} denies it. Edit or remove that rule to allow it.")
        }
        DecisionReason::Preset(p) if p == "spec" => format!(
            "`{tool}` was not run: in spec mode, writes are limited to spec/. Leave spec mode (`aho preset workspace`) to change code."
        ),
        DecisionReason::Preset(p) => format!(
            "`{tool}` was not run: preset `{p}` does not allow it and no one can approve it here. Switch to a wider preset (`aho preset …`) to allow it."
        ),
        DecisionReason::Hook(h) => {
            let why = if h.is_empty() { String::new() } else { format!(" ({h})") };
            format!("`{tool}` was not run: a hook refused it{why}. Change or disable that hook to allow it.")
        }
        DecisionReason::Guard(g) => format!("`{tool}` was not run: a guard refused it ({g})."),
        DecisionReason::SafetyCheck(SafetyKind::Catastrophic) => format!(
            "`{tool}` was not run: it cannot be confirmed; the user must run this themselves. No approval or justification lifts this."
        ),
        DecisionReason::SafetyCheck(SafetyKind::Confirm) => format!(
            "`{tool}` was not run: it is irreversible and was not confirmed. Re-issue it with a `justification` (at least 25 characters, not a bare affirmation) or ask the user."
        ),
        DecisionReason::SafetyCheck(SafetyKind::ProtectedPath) => format!(
            "`{tool}` was not run: it writes a protected location (.git/ or .aho/ internals, credentials, or system files). The user can make this change themselves."
        ),
        DecisionReason::Classifier => format!("`{tool}` was not run: the safety classifier refused it."),
        DecisionReason::WorkingDir => {
            format!("`{tool}` was not run: it reaches outside the working directory. Work inside the workspace or ask the user.")
        }
        DecisionReason::AsyncAgent => format!(
            "`{tool}` was not run: it needs approval and this background agent cannot reach the user. Run the agent in the foreground (bubble) to allow prompts."
        ),
        DecisionReason::Delegation => {
            format!("`{tool}` was not run: the delegating agent's policy does not allow it. Ask the parent agent to do it.")
        }
        DecisionReason::Escalation => {
            format!("`{tool}` was not run: the escalation it asked for is not available in this world.")
        }
        DecisionReason::Unavailable => format!(
            "`{tool}` was not run: it needs approval but no approval channel is available. Enable prompts or widen the preset to allow it."
        ),
        DecisionReason::Passthrough => format!("`{tool}` was not run: nothing allows it under the current preset."),
        DecisionReason::ToolCheck(t) => format!("`{tool}` was not run: {t}"),
    }
}

fn ask_sentence(r: &DecisionReason, tool: &str) -> String {
    match r {
        DecisionReason::SafetyCheck(SafetyKind::Confirm) => format!(
            "`{tool}` is irreversible or its target cannot be read ahead, so the user is asked first. \
             To proceed without asking, re-issue it with a `justification` (at least 25 characters, not a bare affirmation)."
        ),
        DecisionReason::Escalation => {
            format!("`{tool}` asks to go beyond the current preset for this one call; the user is asked before it runs.")
        }
        DecisionReason::Preset(p) => format!("`{tool}` is outside preset `{p}`; the user is asked before it runs."),
        DecisionReason::ToolCheck(t) => format!("`{tool}` needs approval: {t} The user is asked before it runs."),
        other => format!("`{tool}` needs approval by {}; the user is asked before it runs.", clause(other)),
    }
}

fn escalate_list(targets: &[Consequence]) -> String {
    targets.iter().map(|t| format!("\"{}\"", t.name())).collect::<Vec<_>>().join(", ")
}

/// The one renderer. `DenyEscalatable` renders the `[sandbox: …]` marker and escalation hint.
pub fn render(outcome: &ChainOutcome, tool: &str) -> String {
    match outcome {
        ChainOutcome::Allow(r) => format!("`{tool}` was allowed by {}.", clause(r)),
        ChainOutcome::Ask(r) => ask_sentence(r, tool),
        ChainOutcome::Deny(r) => deny_sentence(r, tool),
        ChainOutcome::DenyEscalatable { reason, targets } => {
            let under = match reason {
                DecisionReason::Preset(p) => format!("preset `{p}`"),
                other => clause(other),
            };
            format!(
                "[sandbox: `{tool}` denied under {under}]\n\
                 [sandbox: escalation available; retry this exact call once with escalate: [{}] and a justification]",
                escalate_list(targets)
            )
        }
    }
}

/// The four approval outcomes read differently so the model neither retries a human "no" nor
/// wastes a turn asking when prompts are off.
pub fn render_outcome(o: ApprovalOutcome, tool: &str) -> String {
    match o {
        ApprovalOutcome::Rejected => {
            format!("The user rejected `{tool}`. Do not retry it; ask the user what they want instead.")
        }
        ApprovalOutcome::Cancelled => {
            format!("Approval for `{tool}` was cancelled before anyone answered, so it did not run. Ask again only if it is still needed.")
        }
        ApprovalOutcome::Unavailable => format!(
            "`{tool}` needs approval but no approval channel is available, so it did not run. Do not ask again; work within the current preset or tell the user what would need to change."
        ),
        ApprovalOutcome::AllowedOnce => {
            format!("The user allowed `{tool}` once. The next call like it needs its own approval.")
        }
    }
}

/// Smallest lattice change that would make this call pass, e.g. `preset lab` or
/// `remove rule `deny bash(rm *)` (./aho.toml:14)`. `None` when no cut applies (safety checks
/// are never lifted; a justification is not a cut).
pub fn suggested_cut(outcome: &ChainOutcome, _tool: &str, _args: &Value) -> Option<String> {
    match outcome {
        ChainOutcome::Allow(_) => None,
        ChainOutcome::Ask(r) | ChainOutcome::Deny(r) => match r {
            DecisionReason::Rule { origin, rule } => Some(format!("remove rule `{rule}` ({origin})")),
            _ => None,
        },
        ChainOutcome::DenyEscalatable { targets, .. } => {
            let confining = BoundWorld { name: String::new(), confines: true, enforced_by: String::new() };
            Preset::smallest_containing(ConsequenceSet::of(targets), &confining).map(|p| format!("preset {}", p.name()))
        }
    }
}

fn class_aware_cut(outcome: &ChainOutcome, class: Consequence, args: &Value, policy: &SandboxPolicy) -> Option<String> {
    match outcome {
        ChainOutcome::Ask(DecisionReason::Preset(_)) | ChainOutcome::Deny(DecisionReason::Preset(_)) => {
            Preset::smallest_containing(ConsequenceSet::EMPTY.with(class), policy.world()).map(|p| format!("preset {}", p.name()))
        }
        other => suggested_cut(other, "", args),
    }
}

/// Approval card payload: never a live tool object. Cites the class, never the vendor.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalCard {
    pub tool: String,
    pub class: Consequence,
    pub args: Value,
    pub reason: DecisionReason,
    pub world: BoundWorld,
    /// Smallest lattice change that would make this call pass, e.g. `preset lab`.
    pub suggested_cut: Option<String>,
    pub rendered: String,
}

impl ApprovalCard {
    /// Builds the card for a call the chain sent to a human; knows the class, so a preset
    /// refusal suggests the smallest preset that contains it.
    pub fn new(decl: &ToolDecl, args: &Value, outcome: &ChainOutcome, policy: &SandboxPolicy) -> ApprovalCard {
        ApprovalCard {
            tool: decl.name.to_string(),
            class: decl.class,
            args: args.clone(),
            reason: outcome.reason().clone(),
            world: policy.world().clone(),
            suggested_cut: class_aware_cut(outcome, decl.class, args, policy),
            rendered: render(outcome, decl.name),
        }
    }
}
