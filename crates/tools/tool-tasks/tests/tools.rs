//! The tools through a real composition, a real log and the real fold: each call's `value.events`
//! is appended as `Custom` entries (what `aho_session::tool_events` does) and the capability's
//! context hook sees the folded state before the next call, like before each model request.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use std::sync::{Arc, Mutex, MutexGuard};

use aho_approval::{Approval, ApprovalHandle, AskPolicy, Decided};
use aho_cap::{
    BoundWorld, CancelToken, CapabilityId, Composition, Env, ExecOutcome, ExecRequest, ExecView, HandleKind, Os, PolicyInput, PolicyView,
    Registry, plan,
};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::{ApprovalOutcome, CapState, EntryKind, GrantScope, LogStore, MemoryLog, State, fold};
use aho_policy::ApprovalCard;
use aho_types::{AgentId, BoxFuture, CallId, Consequence, ConsequenceSet, Epoch, Principal, TurnId, Value};
use serde_json::json;
use tool_tasks::{TOOLS, ToolTasks};

struct Human {
    policy: AskPolicy,
    outcome: ApprovalOutcome,
    asked: Mutex<Vec<ApprovalCard>>,
}

impl Human {
    /// Cards the operator was shown (poison-tolerant: a panicking test must not hide them).
    fn cards(&self) -> MutexGuard<'_, Vec<ApprovalCard>> {
        self.asked.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Approval for Human {
    fn policy(&self) -> AskPolicy {
        self.policy
    }
    fn request(&self, card: ApprovalCard) -> BoxFuture<'_, Decided> {
        self.cards().push(card);
        let outcome = self.outcome;
        Box::pin(async move { Decided { outcome, scope: GrantScope::Once } })
    }
}

fn human(policy: AskPolicy, outcome: ApprovalOutcome) -> Arc<Human> {
    Arc::new(Human { policy, outcome, asked: Mutex::new(vec![]) })
}

struct Session {
    reg: Registry,
    log: MemoryLog,
}

fn agent(n: u128) -> Principal {
    Principal::Agent { id: AgentId(n), turn: TurnId(1) }
}

impl Session {
    fn boot(h: Arc<Human>, log: MemoryLog, sets: &[(&str, &str)]) -> Session {
        let comp = Composition::new("t").consume::<ToolTasks>();
        let mut schema = Schema::default();
        for k in comp.config_keys() {
            schema.add(k);
        }
        let mut inputs = ResolveInputs::default();
        inputs.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let loadout = resolve(&inputs, &schema, false).unwrap();
        let policy =
            PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::TalksToHuman]) };
        let p = plan(&comp.manifests(), &loadout, &Env::empty(Os::current()), &policy, vec![]).unwrap();
        assert!(matches!(p.row("tool-tasks").unwrap().state, CapState::Active), "{:?}", p.row("tool-tasks"));
        let mut seed = Registry::default();
        seed.handles.insert(HandleKind::Approval, (CapabilityId { name: "test", major: 0 }, Arc::new(ApprovalHandle(h))));
        let (reg, broken) = comp.start_seeded(&p, &loadout, seed);
        assert!(broken.is_empty(), "{broken:?}");
        let s = Session { reg, log };
        s.step();
        s
    }

    fn state(&self) -> State {
        match self.log.head() {
            Some(h) => fold(&self.log.read_branch(h).unwrap(), &self.reg.entry_kinds).unwrap(),
            None => State::default(),
        }
    }

    /// The session's work before each model request.
    fn step(&self) {
        let st = self.state();
        for (_, f) in &self.reg.contexts {
            f(&st);
        }
    }

    fn call(&self, tool: &str, args: Value, by: Principal) -> ExecOutcome {
        let (decl, exec) = self.reg.tool(tool).unwrap();
        let view = ExecView {
            call: CallId(format!("call-{tool}")),
            tool: tool.into(),
            class: decl.class,
            args,
            policy: PolicyView {
                epoch: Epoch(1),
                allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::TalksToHuman]),
                world: BoundWorld::host(),
                workspace: "/work".into(),
                home: "/home".into(),
            },
            by: by.clone(),
        };
        let out = futures::executor::block_on(exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }));
        if let ExecOutcome::Done { value, .. } = &out {
            for e in value["events"].as_array().unwrap() {
                let kind = EntryKind::Custom {
                    ns: e["ns"].as_str().unwrap().into(),
                    name: e["name"].as_str().unwrap().into(),
                    body: e["body"].clone(),
                };
                self.log.append(None, by.clone(), kind).unwrap();
            }
        }
        out
    }

    /// Call, require success, run the next step's context sync, return (text, value).
    fn ok(&self, tool: &str, args: Value, by: Principal) -> (String, Value) {
        match self.call(tool, args, by) {
            ExecOutcome::Done { value, content, .. } => {
                self.step();
                (content.iter().filter_map(|c| c.as_text()).collect(), value)
            }
            other => panic!("{tool} failed: {other:?}"),
        }
    }

    fn err(&self, tool: &str, args: Value, by: Principal) -> (String, String) {
        match self.call(tool, args, by) {
            ExecOutcome::Err { code, reason } => (code, reason),
            other => panic!("{tool} should have been refused: {other:?}"),
        }
    }
}

fn names(v: &Value) -> Vec<String> {
    v["events"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect()
}

fn artifact(conf: &str) -> Value {
    json!({"findings": "found the entry points", "confidence": conf, "what_i_did_not_check": ["the plugin loader"]})
}

#[test]
fn registers_every_declared_tool_and_the_task_fold() {
    let s = Session::boot(human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce), MemoryLog::new(), &[]);
    for t in TOOLS {
        assert!(s.reg.tool(t.name).is_some(), "{} not registered", t.name);
    }
    assert!(s.reg.entry_kinds.contains_key("Task"));
    assert_eq!(s.reg.tool("task_control").unwrap().0.class, Consequence::TalksToHuman);
    assert!(Composition::new("t").consume::<ToolTasks>().config_keys().iter().any(|k| k.path == "swarm.max_live_workers"));
}

#[test]
fn seed_claim_complete_round_trips_through_the_log() {
    let s = Session::boot(human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce), MemoryLog::new(), &[]);
    let tasks = json!({"tasks": [
        {"id": "scan", "content": "find the entry points", "kind": "explore"},
        {"id": "port", "content": "port them", "kind": "code", "depends_on": ["scan"]},
    ]});
    let (text, v) = s.ok("task_seed", tasks.clone(), Principal::Operator);
    assert_eq!(v["seeded"], json!(["scan", "port"]));
    assert!(v["events"].as_array().unwrap().iter().all(|e| e["ns"] == "Task"));
    assert!(text.contains("Ready now: scan."), "{text}");
    let (text, v) = s.ok("task_seed", tasks, Principal::Operator);
    assert!(names(&v).is_empty() && text.contains("Nothing new"), "reseeding is a no-op");

    let (input, v) = s.ok("task_claim", json!({}), agent(1));
    assert_eq!(v["id"], "scan");
    assert!(input.starts_with("# Task scan (explore)"), "{input}");
    let (again, _) = s.ok("task_claim", json!({}), agent(1));
    assert!(again.starts_with("You already hold this task."));

    let before = s.log.read_from(None).len();
    let (code, why) = s.err(
        "task_complete",
        json!({"id": "scan", "artifact": {"findings": "x", "confidence": "high", "what_i_did_not_check": []}}),
        agent(1),
    );
    assert_eq!(code, "thin_artifact");
    assert!(why.contains("what_i_did_not_check"), "{why}");
    assert_eq!(s.log.read_from(None).len(), before, "a refusal appends nothing");
    let (code, _) = s.err("task_complete", json!({"id": "scan", "artifact": artifact("high")}), agent(2));
    assert_eq!(code, "not_allowed");

    let (text, v) = s.ok("task_complete", json!({"id": "scan", "artifact": artifact("fairly sure")}), agent(1));
    assert!(text.contains("Completed scan (confidence medium)"), "{text}");
    assert_eq!(v["ready"], json!(["port"]));

    let (input, _) = s.ok("task_claim", json!({}), agent(2));
    assert!(input.contains("# Inputs from completed dependencies") && input.contains("## scan (explore) · confidence medium"), "{input}");
    let (status, _) = s.ok("task_status", json!({}), Principal::Operator);
    assert!(
        status.contains("- scan [explore] completed (medium) by agent:1") && status.contains("- port [code] running agent:2"),
        "{status}"
    );
    let (one, v) = s.ok("task_status", json!({"id": "scan"}), agent(2));
    assert!(one.contains("Artifact:") && v["task"]["artifact"]["confidence"] == "medium", "{one}");

    let (code, _) = s.err("task_claim", json!({}), Principal::Operator);
    assert_eq!(code, "not_allowed");
    let folded = aho_tasks::project(s.state().custom.get("Task")).unwrap();
    assert_eq!(folded.node(&aho_tasks::TaskId::new("port")).unwrap().owner, Some(AgentId(2)));
}

#[test]
fn deep_mode_comes_from_config_and_requires_contracts() {
    let s = Session::boot(human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce), MemoryLog::new(), &[("swarm.deep", "true")]);
    let (_, v) = s.ok("task_seed", json!({"tasks": [{"id": "k", "content": "write it", "kind": "code"}]}), Principal::Operator);
    assert_eq!(names(&v)[0], "Configured", "settings are logged before the first change");
    s.ok("task_claim", json!({}), agent(1));
    let (code, why) = s.err("task_complete", json!({"id": "k", "artifact": artifact("high")}), agent(1));
    assert_eq!(code, "thin_artifact");
    assert!(why.contains("contract"), "{why}");
    let mut with_contract = artifact("high");
    with_contract["contract"] = json!({"outputs": [{"path": "src/k.rs", "format": "rust"}], "units": ["ms"], "acceptance": ["cargo test"]});
    s.ok("task_complete", json!({"id": "k", "artifact": with_contract}), agent(1));
}

#[test]
fn expansion_gates_and_injection_through_the_tools() {
    let s = Session::boot(human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce), MemoryLog::new(), &[]);
    s.ok("task_seed", json!({"tasks": [{"id": "p", "content": "understand the system", "kind": "explore"}]}), Principal::Operator);
    s.ok("task_claim", json!({}), agent(1));
    let (text, v) = s.ok(
        "task_expand",
        json!({"id": "p", "children": [
            {"id": "c1", "content": "the API", "kind": "explore"},
            {"id": "c2", "content": "the storage", "kind": "explore"},
        ]}),
        agent(1),
    );
    assert_eq!(v["gates"], json!(["p/critique"]));
    assert!(text.contains("synthesis join"));
    for (who, conf) in [(2, "high"), (3, "not confident")] {
        let (_, v) = s.ok("task_claim", json!({}), agent(who));
        let id = v["id"].as_str().unwrap().to_string();
        s.ok("task_complete", json!({"id": id, "artifact": artifact(conf)}), agent(who));
    }
    let (input, v) = s.ok("task_claim", json!({}), agent(2));
    assert_eq!(v["id"], "p/critique");
    assert!(input.contains("c2 (explore) completed · confidence low  ← LOW"), "{input}");

    let mut pass = artifact("high");
    pass["covers"] = json!(["c1"]);
    let (code, why) = s.err("task_complete", json!({"id": "p/critique", "artifact": pass}), agent(2));
    assert_eq!(code, "gate_refused");
    assert!(why.contains("`c2`") && why.contains("covers"), "{why}");
    let (code, _) = s.err("task_complete", json!({"id": "p/critique", "artifact": artifact("high"), "inject": []}), agent(2));
    assert_eq!(code, "bad_args");

    let (text, v) = s.ok(
        "task_complete",
        json!({"id": "p/critique", "inject": [{"id": "c2.fix", "content": "re-check storage", "kind": "explore", "addresses": ["c2"]}]}),
        agent(2),
    );
    assert_eq!(names(&v), ["Injected", "Released"]);
    assert!(text.contains("c2.fix"));
    let (status, _) = s.ok("task_status", json!({}), agent(2));
    assert!(status.contains("- p/critique [gate:critique] blocked on c2.fix"), "{status}");
}

/// Outside a session there is no `aho.log`: the gate judges the checks the context hook saw, and
/// with no registration known every checked invariant counts as covering it.
#[test]
fn a_verify_gate_follows_the_checks_the_context_hook_saw() {
    let s = Session::boot(human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce), MemoryLog::new(), &[]);
    s.ok("task_seed", json!({"tasks": [{"id": "p", "content": "port the parser", "kind": "code"}]}), Principal::Operator);
    s.ok("task_claim", json!({}), agent(1));
    s.ok("task_expand", json!({"id": "p", "children": [{"id": "k", "content": "the lexer", "kind": "code"}]}), agent(1));
    s.ok("task_claim", json!({}), agent(2));
    s.ok("task_complete", json!({"id": "k", "artifact": artifact("high")}), agent(2));
    let (_, v) = s.ok("task_claim", json!({}), agent(3));
    assert_eq!(v["id"], "p/verify");
    let check = |verdict: aho_log::Verdict| {
        let kind = EntryKind::InvariantChecked {
            id: "inv.lex.total".into(),
            mode: aho_log::CheckMode::Proof,
            verdict,
            code_hash: aho_types::Hash::ZERO,
        };
        s.log.append(None, Principal::system("lean_build"), kind).unwrap();
        s.step();
    };
    let mut pass = artifact("high");
    pass["covers"] = json!(["k"]);

    check(aho_log::Verdict::Fail { counterexample: aho_types::Hash::of(b"cx") });
    let (code, why) = s.err("task_complete", json!({"id": "p/verify", "artifact": pass.clone()}), agent(3));
    assert_eq!(code, "gate_refused");
    assert!(why.contains("gate `p/verify`") && why.contains("`inv.lex.total` fails"), "{why}");

    check(aho_log::Verdict::Pass);
    let (text, v) = s.ok("task_complete", json!({"id": "p/verify", "artifact": pass}), agent(3));
    assert!(text.contains("Invariants covering it: inv.lex.total (pass)."), "{text}");
    let done = v["events"].as_array().unwrap().iter().find(|e| e["name"] == "Completed").unwrap();
    assert_eq!(done["body"]["invariants"], json!([{"id": "inv.lex.total", "status": "pass"}]));
}

#[test]
fn operator_decisions_go_through_the_approval_channel() {
    let log = MemoryLog::new();
    {
        let s = Session::boot(human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce), log.clone(), &[]);
        s.ok("task_seed", json!({"tasks": [{"id": "t", "content": "long port", "kind": "code"}]}), Principal::Operator);
        s.ok("task_claim", json!({}), agent(1));
        let (text, _) =
            s.ok("task_checkpoint", json!({"id": "t", "summary": "ported 3 of 5 modules", "recent": ["cargo check → 2 errors"]}), agent(1));
        assert!(text.contains("Checkpoint recorded"));
        // a live worker's task is not a safe state: refused before anyone is asked
        let (code, why) = s.err("task_control", json!({"id": "t", "op": "salvage", "to": "agent:2"}), agent(3));
        assert_eq!(code, "task_refused");
        assert!(why.contains("stop it first"), "{why}");
    }

    // a new process over the same log: the running task is stale by fold
    let h = human(AskPolicy::Ask, ApprovalOutcome::AllowedOnce);
    let s = Session::boot(h.clone(), log.clone(), &[]);
    let (text, v) = s.ok("task_control", json!({"id": "t", "op": "salvage", "to": "agent:2", "reason": "agent 1 crashed"}), agent(3));
    assert_eq!(names(&v), ["Resumed", "Control"]);
    assert_eq!(v["events"][1]["body"]["by"], "operator", "the approved decision is the operator's");
    assert!(text.contains("salvage applied to t for agent:2"), "{text}");
    let asked = h.cards().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].tool.as_str(), asked[0].class), ("task_control", Consequence::TalksToHuman));
    assert!(asked[0].rendered.contains("operator decision"), "{}", asked[0].rendered);

    let (idle, _) = s.ok("task_claim", json!({}), agent(1));
    assert!(idle.contains("1 reassigned to other agents"), "the salvaged task is pinned to agent 2: {idle}");
    let (input, _) = s.ok("task_claim", json!({}), agent(2));
    assert!(input.contains("# Salvaged from agent:1") && input.contains("Checkpoint: ported 3 of 5 modules"), "{input}");
    assert!(input.contains("- cargo check → 2 errors"), "{input}");

    // declined, and prompts off: nothing changes, and with prompts off nobody is asked
    let before = log.read_from(None).len();
    let no = human(AskPolicy::Ask, ApprovalOutcome::Rejected);
    let s2 = Session::boot(no.clone(), log.clone(), &[]);
    let (code, _) = s2.err("task_control", json!({"id": "t", "op": "reassign", "to": "agent:4"}), agent(3));
    assert_eq!(code, "approval_rejected", "after this resume t is stale, so reassign is safe to ask; the operator declines");
    let (code, _) = s2.err("task_control", json!({"id": "t", "op": "stop"}), agent(3));
    assert_eq!(code, "not_allowed", "an agent stops only its own subtree");
    s2.ok("task_control", json!({"id": "t", "op": "stop"}), Principal::Operator);
    let (code, _) = s2.err("task_control", json!({"id": "t", "op": "retry"}), agent(3));
    assert_eq!(code, "approval_rejected");
    assert_eq!(no.cards().len(), 2);
    let off = human(AskPolicy::Never, ApprovalOutcome::AllowedOnce);
    let s3 = Session::boot(off.clone(), log.clone(), &[]);
    let (code, why) = s3.err("task_control", json!({"id": "t", "op": "retry"}), agent(3));
    assert_eq!(code, "approval_off");
    assert!(why.contains("policy.ask = never"));
    assert!(off.cards().is_empty());
    assert_eq!(log.read_from(None).len(), before + 2, "only the resume marker and the operator's own stop were appended");
}
