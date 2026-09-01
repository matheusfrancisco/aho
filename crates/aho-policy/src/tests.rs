use aho_cap::{BoundWorld, Decision, PolicyView, ToolDecl};
use aho_log::ApprovalOutcome;
use serde_json::json;

use crate::*;

const WS: &str = "/Users/me/proj";
const HOME: &str = "/Users/me";

static BASH: ToolDecl = ToolDecl::new("bash", "run a command", Consequence::SpawnsProcess);
static WRITE: ToolDecl = ToolDecl::new("write", "write a file", Consequence::WorkspaceWrite);
static READ: ToolDecl = ToolDecl::new("read", "read a file", Consequence::ReadOnly);
static HOST_WRITE: ToolDecl = ToolDecl::new("host_write", "write a host file", Consequence::HostWrite);
static FETCH: ToolDecl = ToolDecl::new("fetch", "fetch a url", Consequence::NetworkEgress);
static VM_START: ToolDecl = ToolDecl::new("vm_start", "start a vm", Consequence::ControlsVM);

fn refuse_unread(args: &Value, _: &PolicyView) -> Option<Decision> {
    match args.get("path").and_then(Value::as_str) {
        Some("unread.rs") => Some(Decision::Ask("the file was not read first.".into())),
        Some("locked.rs") => Some(Decision::Deny("the file is locked.".into())),
        Some("mine.rs") => Some(Decision::Allow),
        _ => None,
    }
}
static EDIT: ToolDecl = ToolDecl::new("edit", "edit a file", Consequence::WorkspaceWrite).check(refuse_unread);

fn confining() -> BoundWorld {
    BoundWorld { name: "namespace".into(), confines: true, enforced_by: "user namespaces".into() }
}

fn policy(p: Preset, world: BoundWorld) -> SandboxPolicy {
    SandboxPolicy::from_preset(p, world, WS.into(), HOME.into(), Epoch(1))
}

fn rules(lines: &[&str]) -> Vec<Rule> {
    parse_rules(&lines.iter().map(|l| (l.to_string(), "./aho.toml:1".to_string())).collect::<Vec<_>>()).unwrap()
}

struct Case<'a> {
    decl: &'a ToolDecl,
    args: Value,
    rules: Vec<Rule>,
    policy: SandboxPolicy,
    guards: Vec<(String, String)>,
    escalate: Vec<Consequence>,
}

impl<'a> Case<'a> {
    fn new(decl: &'a ToolDecl, args: Value, policy: SandboxPolicy) -> Self {
        Case { decl, args, rules: Vec::new(), policy, guards: Vec::new(), escalate: Vec::new() }
    }
    fn rules(mut self, lines: &[&str]) -> Self {
        self.rules = rules(lines);
        self
    }
    fn decide(&self) -> ChainOutcome {
        let by = Principal::Agent { id: AgentId(1), turn: TurnId(1) };
        decide(&CallUnderReview {
            decl: self.decl,
            args: &self.args,
            by: &by,
            rules: &self.rules,
            policy: &self.policy,
            guard_denials: self.guards.clone(),
            escalate: self.escalate.clone(),
            tool_name: None,
        })
    }
}

fn catastrophic() -> ChainOutcome {
    ChainOutcome::Deny(DecisionReason::SafetyCheck(SafetyKind::Catastrophic))
}

// ───────────────────────────── chain order ─────────────────────────────

#[test]
fn deny_rule_beats_allow_rule_and_widest_preset() {
    let c = Case::new(&BASH, json!({"command": "rm -f notes.txt"}), policy(Preset::Full, BoundWorld::host()))
        .rules(&["allow bash(*)", "deny bash(rm *)"]);
    assert_eq!(c.decide(), ChainOutcome::Deny(DecisionReason::Rule { origin: "./aho.toml:1".into(), rule: "deny bash(rm *)".into() }));
}

#[test]
fn deny_rule_sees_every_command_in_a_compound_line() {
    let c =
        Case::new(&BASH, json!({"command": "ls && sudo rm -f notes.txt"}), policy(Preset::Full, confining())).rules(&["deny bash(rm *)"]);
    assert!(matches!(c.decide(), ChainOutcome::Deny(DecisionReason::Rule { .. })));
}

#[test]
fn class_level_deny_rule_refuses_every_tool_of_that_class() {
    let c = Case::new(&BASH, json!({"command": "ls"}), policy(Preset::Full, confining())).rules(&["deny SpawnsProcess"]);
    assert_eq!(c.decide(), ChainOutcome::Deny(DecisionReason::Rule { origin: "./aho.toml:1".into(), rule: "deny SpawnsProcess".into() }));
    let other = Case::new(&READ, json!({"path": "a"}), policy(Preset::Full, confining())).rules(&["deny SpawnsProcess"]);
    assert_eq!(other.decide(), ChainOutcome::Allow(DecisionReason::Passthrough));
}

#[test]
fn ask_rule_beats_allow_rule() {
    let c = Case::new(&BASH, json!({"command": "git push origin main"}), policy(Preset::Dev, confining()))
        .rules(&["allow bash(git *)", "ask bash(git push *)"]);
    assert!(matches!(c.decide(), ChainOutcome::Ask(DecisionReason::Rule { .. })));
}

#[test]
fn allow_rule_must_cover_every_command_it_allows() {
    let p = policy(Preset::Dev, confining());
    let single = Case::new(&BASH, json!({"command": "git status"}), p.clone()).rules(&["allow bash(git *)"]);
    assert!(matches!(single.decide(), ChainOutcome::Allow(DecisionReason::Rule { .. })));
    let compound = Case::new(&BASH, json!({"command": "git status && make"}), p).rules(&["allow bash(git *)"]);
    assert_eq!(compound.decide(), ChainOutcome::Allow(DecisionReason::Passthrough));
}

#[test]
fn path_rules_match_workspace_relative_paths() {
    let p = policy(Preset::Workspace, confining());
    let c = Case::new(&WRITE, json!({"path": "./src/../secrets/key.pem"}), p.clone()).rules(&["deny write(secrets/**)"]);
    assert!(matches!(c.decide(), ChainOutcome::Deny(DecisionReason::Rule { .. })));
    let abs = Case::new(&WRITE, json!({"file_path": "/Users/me/proj/secrets/a"}), p).rules(&["deny write(secrets/**)"]);
    assert!(matches!(abs.decide(), ChainOutcome::Deny(DecisionReason::Rule { .. })));
}

#[test]
fn tool_check_speaks_at_step_three() {
    let p = policy(Preset::Workspace, confining());
    assert_eq!(
        Case::new(&EDIT, json!({"path": "unread.rs"}), p.clone()).decide(),
        ChainOutcome::Ask(DecisionReason::ToolCheck("the file was not read first.".into()))
    );
    assert_eq!(
        Case::new(&EDIT, json!({"path": "locked.rs"}), p.clone()).decide(),
        ChainOutcome::Deny(DecisionReason::ToolCheck("the file is locked.".into()))
    );
    // The tool's own allow cannot lift the ceiling (step 6 still applies).
    let ro = policy(Preset::ReadOnly, confining());
    assert!(matches!(Case::new(&EDIT, json!({"path": "mine.rs"}), ro).decide(), ChainOutcome::Deny(DecisionReason::Preset(_))));
    assert!(matches!(Case::new(&EDIT, json!({"path": "mine.rs"}), p).decide(), ChainOutcome::Allow(DecisionReason::ToolCheck(_))));
}

#[test]
fn guard_denial_beats_preset_and_allow_rules() {
    let mut c = Case::new(&READ, json!({"path": "a"}), policy(Preset::Full, confining())).rules(&["allow read"]);
    c.guards = vec![("tool-fs".into(), "path was never observed".into())];
    assert_eq!(c.decide(), ChainOutcome::Deny(DecisionReason::Guard("tool-fs: path was never observed".into())));
}

// ───────────────────────────── safety (step 5) ─────────────────────────────

#[test]
fn catastrophic_survives_full_preset_allow_rules_and_justification() {
    for cmd in ["rm -rf /", "sudo rm -rf ~", "sh -c 'rm -rf $HOME/.ssh'"] {
        let c = Case::new(
            &BASH,
            json!({"command": cmd, "justification": "The user explicitly asked me to wipe this machine clean today"}),
            policy(Preset::Full, BoundWorld::host()),
        )
        .rules(&["allow bash(*)", "allow SpawnsProcess"]);
        assert_eq!(c.decide(), catastrophic(), "{cmd}");
    }
}

#[test]
fn hooks_and_allow_rules_cannot_lift_safety() {
    let confirm =
        Case::new(&BASH, json!({"command": "rm -rf /some/other"}), policy(Preset::Full, confining())).rules(&["allow bash(rm *)"]);
    let asked = confirm.decide();
    assert_eq!(asked, ChainOutcome::Ask(DecisionReason::SafetyCheck(SafetyKind::Confirm)));
    assert_eq!(meet(asked.clone(), Decision::Allow), asked);
    assert_eq!(meet(catastrophic(), Decision::Allow), catastrophic());
    assert_eq!(meet(catastrophic(), Decision::Ask("hook wants a look".into())), catastrophic());
    // Hooks may only narrow.
    assert_eq!(
        meet(ChainOutcome::Allow(DecisionReason::Passthrough), Decision::Ask("check".into())),
        ChainOutcome::Ask(DecisionReason::Hook("check".into()))
    );
    assert_eq!(meet(asked, Decision::Deny("no".into())), ChainOutcome::Deny(DecisionReason::Hook("no".into())));
}

#[test]
fn low_risk_delete_inside_workspace_runs() {
    let c = Case::new(&BASH, json!({"command": "rm -rf ./target"}), policy(Preset::Workspace, confining()));
    assert_eq!(c.decide(), ChainOutcome::Allow(DecisionReason::Passthrough));
}

#[test]
fn justification_refines_inside_the_ceiling_only() {
    let cmd = "rm -rf /some/other";
    let good = "The user asked me to delete /some/other in their last message";
    let ws = policy(Preset::Workspace, BoundWorld::host());
    let ask = ChainOutcome::Ask(DecisionReason::SafetyCheck(SafetyKind::Confirm));

    assert_eq!(Case::new(&BASH, json!({"command": cmd}), ws.clone()).decide(), ask);
    assert_eq!(Case::new(&BASH, json!({"command": cmd, "justification": "yes, I am sure, go ahead"}), ws.clone()).decide(), ask);
    assert_eq!(Case::new(&BASH, json!({"command": cmd, "justification": "too short"}), ws.clone()).decide(), ask);
    assert_eq!(
        Case::new(&BASH, json!({"command": cmd, "justification": good}), ws).decide(),
        ChainOutcome::Allow(DecisionReason::SafetyCheck(SafetyKind::Confirm))
    );
    // Outside the ceiling a justification never widens: a human decides.
    let ro = policy(Preset::ReadOnly, BoundWorld::host());
    assert_eq!(Case::new(&BASH, json!({"command": cmd, "justification": good}), ro).decide(), ask);
}

#[test]
fn protected_paths_refuse_writes_but_not_reads() {
    let full = policy(Preset::Full, BoundWorld::host());
    assert_eq!(
        Case::new(&WRITE, json!({"path": ".git/hooks/pre-commit"}), full.clone()).decide(),
        ChainOutcome::Deny(DecisionReason::SafetyCheck(SafetyKind::ProtectedPath))
    );
    assert_eq!(Case::new(&HOST_WRITE, json!({"path": "~/.ssh/authorized_keys"}), full.clone()).decide(), catastrophic());
    assert_eq!(Case::new(&READ, json!({"path": ".git/config"}), full).decide(), ChainOutcome::Allow(DecisionReason::Passthrough));
}

#[test]
fn safety_check_entry_point_matches_the_chain() {
    assert_eq!(safety_check("bash", &json!({"command": "rm -rf /"}), WS, HOME).map(|x| x.0), Some(SafetyKind::Catastrophic));
    assert_eq!(safety_check("bash", &json!({"command": "rm -rf /some/other"}), WS, HOME).map(|x| x.0), Some(SafetyKind::Confirm));
    assert_eq!(safety_check("bash", &json!({"command": "rm -rf ./target"}), WS, HOME), None);
    assert_eq!(safety_check("edit", &json!({"path": ".aho/log.jsonl"}), WS, HOME).map(|x| x.0), Some(SafetyKind::ProtectedPath));
    assert_eq!(safety_check("read", &json!({"path": ".aho/log.jsonl"}), WS, HOME), None);
    let (_, why) = safety_check("bash", &json!({"command": "rm -rf /some/other"}), WS, HOME).unwrap();
    assert!(why.contains("justification") && why.contains("/some/other"), "{why}");
}

// ───────────────────────────── ceiling (step 6) ─────────────────────────────

#[test]
fn ceiling_under_confining_world_is_escalatable() {
    let ws = policy(Preset::Workspace, confining());
    let mut c = Case::new(&HOST_WRITE, json!({"path": "/etc-like/x"}), ws.clone());
    let refused = c.decide();
    assert_eq!(
        refused,
        ChainOutcome::DenyEscalatable { reason: DecisionReason::Preset("workspace".into()), targets: vec![Consequence::HostWrite] }
    );
    let text = render(&refused, "host_write");
    assert!(text.starts_with("[sandbox: `host_write` denied under preset `workspace`]"), "{text}");
    assert!(text.contains("escalate: [\"HostWrite\"]") && text.contains("justification"), "{text}");

    c.escalate = vec![Consequence::HostWrite];
    assert_eq!(c.decide(), ChainOutcome::Ask(DecisionReason::Escalation));
    // Targets that do not cover the class, or that the world cannot grant, are not an escalation.
    c.escalate = vec![Consequence::NetworkEgress];
    assert!(matches!(c.decide(), ChainOutcome::DenyEscalatable { .. }));
    c.escalate = vec![Consequence::HostWrite, Consequence::ControlsVM];
    assert!(matches!(c.decide(), ChainOutcome::DenyEscalatable { .. }));
}

#[test]
fn ceiling_asks_or_denies_when_escalation_cannot_grant() {
    // VMs go through leases, not escalation: the human is asked.
    assert_eq!(
        Case::new(&VM_START, json!({"name": "a"}), policy(Preset::Workspace, confining())).decide(),
        ChainOutcome::Ask(DecisionReason::Preset("workspace".into()))
    );
    // Host world: nothing to escalate into, ask the human.
    let mut c = Case::new(&FETCH, json!({"url": "https://x"}), policy(Preset::Workspace, BoundWorld::host()));
    c.escalate = vec![Consequence::NetworkEgress];
    assert_eq!(c.decide(), ChainOutcome::Ask(DecisionReason::Preset("workspace".into())));
    // No one to ask (read-only has no TalksToHuman): refused outright.
    assert_eq!(
        Case::new(&FETCH, json!({"url": "https://x"}), policy(Preset::ReadOnly, confining())).decide(),
        ChainOutcome::Deny(DecisionReason::Preset("read-only".into()))
    );
}

#[test]
fn passthrough_inside_ceiling_allows() {
    assert_eq!(
        Case::new(&FETCH, json!({"url": "https://x"}), policy(Preset::Dev, confining())).decide(),
        ChainOutcome::Allow(DecisionReason::Passthrough)
    );
}

#[test]
fn spec_preset_limits_writes_to_spec_dir() {
    let spec = policy(Preset::Spec, confining());
    let deny = ChainOutcome::Deny(DecisionReason::Preset("spec".into()));
    assert_eq!(Case::new(&WRITE, json!({"path": "spec/SPEC.md"}), spec.clone()).decide(), ChainOutcome::Allow(DecisionReason::Passthrough));
    assert_eq!(
        Case::new(&WRITE, json!({"path": "/Users/me/proj/spec/tla/A.tla"}), spec.clone()).decide(),
        ChainOutcome::Allow(DecisionReason::Passthrough)
    );
    for p in ["src/main.rs", "spec/../src/main.rs", "spec", "specs/x.md", "/tmp/x"] {
        assert_eq!(Case::new(&WRITE, json!({"path": p}), spec.clone()).decide(), deny, "{p}");
    }
    assert_eq!(Case::new(&WRITE, json!({"content": "no path"}), spec.clone()).decide(), deny);
    // An allow rule cannot lift the preset's restriction (step 6 precedes step 7).
    assert_eq!(Case::new(&WRITE, json!({"path": "src/main.rs"}), spec.clone()).rules(&["allow write(**)"]).decide(), deny);
    // Reads and checkers are unaffected.
    assert_eq!(Case::new(&READ, json!({"path": "src/main.rs"}), spec.clone()).decide(), ChainOutcome::Allow(DecisionReason::Passthrough));
    // Widening by an operator keeps the restriction.
    let op = Operator::authenticate(OperatorProof::local_tty());
    let wide = spec.widen_by_approval(&op, EntryId(9), ConsequenceSet::of(&[Consequence::NetworkEgress]));
    assert_eq!(Case::new(&WRITE, json!({"path": "src/main.rs"}), wide).decide(), deny);
    assert_eq!(
        render(&deny, "write"),
        "`write` was not run: in spec mode, writes are limited to spec/. Leave spec mode (`aho preset workspace`) to change code."
    );
}

// ───────────────────────────── lattice ─────────────────────────────

#[test]
fn preset_cuts_follow_the_world() {
    use Consequence::*;
    let c = confining();
    let h = BoundWorld::host();
    let workspace = ConsequenceSet::of(&[ReadOnly, WorkspaceWrite, SpawnsProcess, TalksToHuman]);
    assert_eq!(Preset::ReadOnly.cut(&c), ConsequenceSet::of(&[ReadOnly]));
    assert_eq!(Preset::Workspace.cut(&c), workspace);
    assert_eq!(Preset::Workspace.cut(&h), workspace);
    assert_eq!(Preset::Dev.cut(&c), workspace.with(NetworkEgress));
    assert_eq!(Preset::Dev.cut(&h), workspace.with(NetworkEgress).with(HostWrite));
    assert_eq!(Preset::Lab.cut(&c), workspace.with(NetworkEgress).with(ControlsVM).with(ControlsDesktop));
    assert!(Preset::Lab.cut(&h).contains(HostWrite));
    assert_eq!(Preset::Full.cut(&c), ConsequenceSet::ALL);
    assert_eq!(Preset::Spec.cut(&c), workspace);
    for p in Preset::SWITCHABLE {
        assert_eq!(preset(p.name()), Some(p));
    }
    assert_eq!(preset("custom"), None);
    assert_eq!(preset("root"), None);
    assert_eq!(Preset::derive(workspace, &c), Preset::Workspace);
    assert_eq!(Preset::derive(ConsequenceSet::of(&[ReadOnly, NetworkEgress]), &c), Preset::Custom);
}

#[test]
fn narrow_never_grows() {
    let world = confining();
    for a in 0u16..=0xff {
        let p = SandboxPolicy::new(ConsequenceSet(a), world.clone(), WS.into(), HOME.into(), Epoch(3));
        for b in 0u16..=0xff {
            let n = p.narrow(ConsequenceSet(b));
            assert!(n.allowed().is_subset(&p.allowed()) && n.allowed().is_subset(&ConsequenceSet(b)));
            assert_eq!(n.epoch(), Epoch(3));
        }
    }
    let spec = policy(Preset::Spec, world);
    assert!(spec.narrow(ConsequenceSet::ALL.without(Consequence::NetworkEgress)).writes_limited_to_spec());
}

#[test]
fn widening_needs_an_operator_and_opens_an_epoch() {
    let op = Operator::authenticate(OperatorProof::local_tty());
    let ro = policy(Preset::ReadOnly, confining());
    let w = ro.widen_by_approval(&op, EntryId(42), ConsequenceSet::of(&[Consequence::WorkspaceWrite]));
    assert_eq!(w.allowed(), ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]));
    assert_eq!(w.epoch(), Epoch(2));
    assert_eq!(w.approval_ref(), Some(EntryId(42)));
    assert_eq!(w.preset(), Preset::Custom);
    // Widening never narrows what was already allowed.
    let same = w.widen_by_approval(&op, EntryId(43), ConsequenceSet::EMPTY);
    assert_eq!(same.allowed(), w.allowed());
    let v = w.view();
    assert_eq!((v.epoch, v.allowed, v.workspace.as_str(), v.home.as_str()), (Epoch(2), w.allowed(), WS, HOME));
}

#[test]
fn child_policy_drops_talks_to_human_for_async_unbubbled() {
    let parent = policy(Preset::Dev, confining());
    let decl = ConsequenceSet::ALL;
    let async_child = child_policy(&parent, decl, true, false);
    assert!(!async_child.allowed().contains(Consequence::TalksToHuman));
    assert_eq!(async_child.allowed(), parent.allowed().without(Consequence::TalksToHuman));
    assert!(child_policy(&parent, decl, true, true).allowed().contains(Consequence::TalksToHuman));
    assert!(child_policy(&parent, decl, false, false).allowed().contains(Consequence::TalksToHuman));
    // An agent declaration can narrow, never widen.
    let narrow = child_policy(&parent, ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::ControlsVM]), false, false);
    assert_eq!(narrow.allowed(), ConsequenceSet::of(&[Consequence::ReadOnly]));
    // An async child cannot ask, so a ceiling miss is refused rather than asked.
    let c = Case::new(&VM_START, json!({}), async_child);
    assert_eq!(c.decide(), ChainOutcome::Deny(DecisionReason::Preset("custom".into())));
}

#[test]
fn under_preset_is_a_meet() {
    let full = policy(Preset::Full, confining());
    let plan = full.under_preset(Preset::ReadOnly);
    assert_eq!(plan.allowed(), ConsequenceSet::of(&[Consequence::ReadOnly]));
    assert_eq!(plan.preset(), Preset::ReadOnly);
    let ro = policy(Preset::ReadOnly, confining());
    let spec = ro.under_preset(Preset::Spec);
    assert_eq!(spec.allowed(), ro.allowed());
    assert!(spec.writes_limited_to_spec());
    assert_eq!(spec.preset(), Preset::Custom);
}

#[test]
fn denial_counter_trips_and_resets() {
    let mut d = DenialCounter::default();
    d.record(true);
    d.record(true);
    assert!(!d.tripped());
    d.record(true);
    assert!(d.tripped());
    d.record(false);
    assert!(!d.tripped());
    assert_eq!(d.total, 3);
    for _ in 0..17 {
        d.record(true);
        d.record(false);
    }
    assert!(d.tripped(), "{d:?}");
}

// ───────────────────────────── rendering ─────────────────────────────

#[test]
fn four_approval_outcomes_read_differently() {
    let all = [ApprovalOutcome::Rejected, ApprovalOutcome::Cancelled, ApprovalOutcome::Unavailable, ApprovalOutcome::AllowedOnce];
    let texts: Vec<String> = all.iter().map(|o| render_outcome(*o, "vm_start")).collect();
    for (i, a) in texts.iter().enumerate() {
        assert!(a.contains("`vm_start`"), "{a}");
        for b in &texts[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert!(texts[0].contains("rejected") && texts[0].contains("Do not retry"));
    assert!(texts[1].contains("cancelled"));
    assert!(texts[2].contains("no approval channel") && texts[2].contains("Do not ask again"));
    assert!(texts[3].contains("allowed") && texts[3].contains("once"));
}

#[test]
fn renderer_uses_message_voice() {
    let cat = render(&catastrophic(), "bash");
    assert!(cat.contains("the user must run this themselves"), "{cat}");
    let rule = render(&ChainOutcome::Deny(DecisionReason::Rule { origin: "./aho.toml:14".into(), rule: "deny bash(rm *)".into() }), "bash");
    assert_eq!(rule, "`bash` was not run: rule `deny bash(rm *)` from ./aho.toml:14 denies it. Edit or remove that rule to allow it.");
    let confirm = render(&ChainOutcome::Ask(DecisionReason::SafetyCheck(SafetyKind::Confirm)), "bash");
    assert!(confirm.contains("justification"), "{confirm}");
    let ask = render(&ChainOutcome::Ask(DecisionReason::Preset("workspace".into())), "vm_start");
    assert_eq!(ask, "`vm_start` is outside preset `workspace`; the user is asked before it runs.");
}

#[test]
fn suggested_cuts_name_the_smallest_change() {
    let esc =
        ChainOutcome::DenyEscalatable { reason: DecisionReason::Preset("workspace".into()), targets: vec![Consequence::NetworkEgress] };
    assert_eq!(suggested_cut(&esc, "fetch", &json!({})).as_deref(), Some("preset dev"));
    let esc = ChainOutcome::DenyEscalatable { reason: DecisionReason::Preset("workspace".into()), targets: vec![Consequence::HostWrite] };
    assert_eq!(suggested_cut(&esc, "host_write", &json!({})).as_deref(), Some("preset full"));
    let rule = ChainOutcome::Ask(DecisionReason::Rule { origin: "./aho.toml:2".into(), rule: "ask bash(git push *)".into() });
    assert_eq!(suggested_cut(&rule, "bash", &json!({})).as_deref(), Some("remove rule `ask bash(git push *)` (./aho.toml:2)"));
    assert_eq!(suggested_cut(&ChainOutcome::Ask(DecisionReason::SafetyCheck(SafetyKind::Confirm)), "bash", &json!({})), None);

    let ws = policy(Preset::Workspace, confining());
    let asked = Case::new(&VM_START, json!({"name": "a"}), ws.clone()).decide();
    let card = ApprovalCard::new(&VM_START, &json!({"name": "a"}), &asked, &ws);
    assert_eq!(card.suggested_cut.as_deref(), Some("preset lab"));
    assert_eq!(card.class, Consequence::ControlsVM);
    assert_eq!(card.rendered, render(&asked, "vm_start"));
}
