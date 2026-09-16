//! The goal tools through a real composition, log and fold (events appended as the session does).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use aho_cap::{BoundWorld, CancelToken, Composition, Env, ExecOutcome, ExecRequest, ExecView, Os, PolicyInput, PolicyView, Registry, plan};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::{CapState, EntryKind, LogStore, MemoryLog, State, fold};
use aho_types::{AgentId, CallId, Consequence, ConsequenceSet, Epoch, Principal, TurnId, Value};
use serde_json::json;
use tool_goals::{TOOLS, ToolGoals};

struct Session {
    reg: Registry,
    log: MemoryLog,
}

fn agent(n: u128) -> Principal {
    Principal::Agent { id: AgentId(n), turn: TurnId(1) }
}

impl Session {
    fn boot(sets: &[(&str, &str)]) -> Session {
        let comp = Composition::new("t").consume::<ToolGoals>();
        let mut schema = Schema::default();
        for k in comp.config_keys() {
            schema.add(k);
        }
        let mut inputs = ResolveInputs::default();
        inputs.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let loadout = resolve(&inputs, &schema, false).unwrap();
        let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
        let p = plan(&comp.manifests(), &loadout, &Env::empty(Os::current()), &policy, vec![]).unwrap();
        assert!(matches!(p.row("tool-goals").unwrap().state, CapState::Active));
        let (reg, broken) = comp.start(&p, &loadout);
        assert!(broken.is_empty(), "{broken:?}");
        let s = Session { reg, log: MemoryLog::new() };
        s.step();
        s
    }
    fn state(&self) -> State {
        match self.log.head() {
            Some(h) => fold(&self.log.read_branch(h).unwrap(), &self.reg.entry_kinds).unwrap(),
            None => State::default(),
        }
    }
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
                allowed: ConsequenceSet::of(&[Consequence::ReadOnly]),
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
        self.step();
        out
    }
    fn ok(&self, tool: &str, args: Value, by: Principal) -> (String, Value) {
        match self.call(tool, args, by) {
            ExecOutcome::Done { value, content, .. } => (content.iter().filter_map(|c| c.as_text()).collect(), value),
            other => panic!("{tool} failed: {other:?}"),
        }
    }
    fn err(&self, tool: &str, args: Value, by: Principal) -> (String, String) {
        match self.call(tool, args, by) {
            ExecOutcome::Err { code, reason } => (code, reason),
            other => panic!("{tool} should have been refused: {other:?}"),
        }
    }
    fn folded(&self) -> aho_goals::Goals {
        aho_goals::project(self.state().custom.get("Goal")).unwrap()
    }
}

#[test]
fn registers_the_three_tools_and_the_goal_fold() {
    let s = Session::boot(&[]);
    for t in TOOLS {
        assert!(s.reg.tool(t.name).is_some());
    }
    assert!(s.reg.entry_kinds.contains_key("Goal"));
}

#[test]
fn get_goal_without_an_active_goal_says_so() {
    let s = Session::boot(&[]);
    let (text, v) = s.ok("get_goal", json!({}), agent(0));
    assert_eq!(text, "No goal is active.");
    assert_eq!((v["goal"].clone(), v["events"].clone()), (Value::Null, json!([])));
    s.ok("create_goal", json!({"objective": "ship the parser"}), agent(0));
    s.ok("update_goal", json!({"id": "goal-1", "action": "pause"}), agent(0));
    let (text, _) = s.ok("get_goal", json!({}), agent(0));
    assert!(text.starts_with("No goal is active. goal-1 (\"ship the parser\") is paused"), "{text}");
}

#[test]
fn create_get_update_round_trip_through_the_fold() {
    let s = Session::boot(&[("goals.max_rounds", "3")]);
    let (text, v) = s.ok("create_goal", json!({"objective": "ship the parser", "max_rounds": 0}), agent(0));
    assert!(text.starts_with("Created goal-1 · active · round 0/3"), "zero counts as omitted: {text}");
    assert_eq!((v["goal"]["owner"].as_str(), v["events"][0]["ns"].as_str()), (Some("agent:0"), Some("Goal")));
    let (text, v) = s.ok("get_goal", json!({}), agent(0));
    assert!(text.contains("goal-1") && v["goal"]["revision"] == 1);

    let (code, why) = s.err("update_goal", json!({"id": "goal-1", "action": "complete", "revision": 7}), agent(0));
    assert_eq!(code, "stale_revision");
    assert!(why.contains("revision 1, not 7"));
    let (code, _) = s.err("update_goal", json!({"id": "goal-1", "action": "block"}), agent(0));
    assert_eq!(code, "goal_refused", "block needs a reason");
    let (text, v) =
        s.ok("update_goal", json!({"id": "goal-1", "action": "block", "reason": "needs the grammar decision", "revision": 1}), agent(0));
    assert!(text.starts_with("Goal blocked:") && v["goal"]["blocked_reason"]["code"] == "model_reported", "{text}");
    s.ok("update_goal", json!({"id": "goal-1", "action": "edit", "objective": "ship the parser v2"}), agent(0));
    s.ok("update_goal", json!({"id": "goal-1", "action": "resume"}), agent(0));
    let (code, _) = s.err("update_goal", json!({"id": "goal-1", "action": "resume", "objective": "x", "wrong": 1}), agent(9));
    assert_eq!(code, "not_allowed", "another agent may not resume a goal it did not create");
    s.ok("update_goal", json!({"id": "goal-1", "action": "complete"}), agent(0));
    let folded = s.folded();
    let g = folded.get("goal-1").unwrap();
    assert_eq!((g.phase, g.revision, g.objective.as_str()), (aho_goals::Phase::Complete, 5, "ship the parser v2"));
}

#[test]
fn one_active_goal_and_updates_name_the_current_one() {
    let s = Session::boot(&[]);
    s.ok("create_goal", json!({"objective": "first"}), agent(0));
    let (code, why) = s.err("create_goal", json!({"objective": "second"}), agent(0));
    assert_eq!(code, "another_active");
    assert!(why.contains("`goal-1`"), "{why}");

    s.ok("update_goal", json!({"id": "goal-1", "action": "pause"}), agent(0));
    s.ok("create_goal", json!({"objective": "second"}), agent(0));
    let (code, why) = s.err("update_goal", json!({"id": "goal-1", "action": "complete"}), agent(0));
    assert_eq!(code, "not_current");
    assert!(why.contains("`goal-2` (\"second\") is active"), "{why}");

    // a user goal takes priority: the agent's active goal is paused first
    let (text, v) = s.ok("create_goal", json!({"objective": "the operator's goal"}), Principal::Operator);
    assert_eq!(v["events"].as_array().unwrap().len(), 2);
    assert!(text.contains("goal-2 was paused: user goals take priority"), "{text}");
    let (code, _) = s.err("update_goal", json!({"id": "goal-3", "action": "pause"}), agent(0));
    assert_eq!(code, "not_allowed", "an agent does not pause a user goal");
    let folded = s.folded();
    assert_eq!(folded.current().map(|g| g.id.as_str()), Some("goal-3"));
    assert_eq!(folded.get("goal-2").map(|g| g.phase), Some(aho_goals::Phase::Paused));
}
