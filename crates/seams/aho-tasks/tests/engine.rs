#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
use aho_log::{CustomFold, CustomFolds, EntryKind, LogError, LogStore, MemoryLog, ToolOutcome, fold};
use aho_tasks::{
    Actor, ChildSpec, Confidence, Control, Derived, Dispatch, Event, FailReason, GateError, Graph, HandoffArtifact, Hydrate, Injection,
    InvariantSeen, InvariantStatus, Kind, NS, ROOT_GATE, Salvage, SeedSpec, Settings, Status, TaskError, TaskId, fold_tasks, project,
};
use aho_types::{AgentId, CallId, Consequence, ContentBlock, Principal, TurnId};
use serde_json::json;

fn a(n: u128) -> AgentId {
    AgentId(n)
}
fn id(s: &str) -> TaskId {
    TaskId::new(s)
}
fn spec(i: &str, kind: Kind, deps: &[&str]) -> SeedSpec {
    SeedSpec { id: id(i), content: format!("do {i}"), kind, depends_on: deps.iter().map(|d| id(d)).collect(), parent: None }
}
fn child(i: &str, kind: Kind, deps: &[&str]) -> ChildSpec {
    ChildSpec { id: id(i), content: format!("do {i}"), kind, depends_on: deps.iter().map(|d| id(d)).collect() }
}
fn art(conf: &str) -> HandoffArtifact {
    HandoffArtifact::parse(&json!({"findings": "found it", "confidence": conf, "what_i_did_not_check": ["windows paths"]})).unwrap()
}
fn gate_art(covers: &[&str]) -> HandoffArtifact {
    HandoffArtifact::parse(&json!({
        "findings": "reviewed", "confidence": "high", "what_i_did_not_check": ["perf"], "covers": covers,
    }))
    .unwrap()
}
fn code_art() -> HandoffArtifact {
    HandoffArtifact::parse(&json!({
        "findings": "wrote the parser", "confidence": "medium", "what_i_did_not_check": ["unicode input"],
        "contract": {"outputs": [{"path": "src/parser.rs", "format": "rust"}], "units": ["byte offsets"], "version": "1",
                     "acceptance": ["cargo test -p parser"]},
    }))
    .unwrap()
}
fn commit(g: &mut Graph, ev: Vec<Event>) {
    *g = g.stage(&ev).unwrap();
}
fn seeded(specs: Vec<SeedSpec>, settings: Settings) -> Graph {
    let mut g = Graph::default();
    let ev = g.configure(&Actor::Operator, &settings).unwrap();
    commit(&mut g, ev);
    let ev = g.seed(&Actor::Operator, specs).unwrap();
    commit(&mut g, ev);
    g
}
fn start(g: &mut Graph, w: AgentId) -> TaskId {
    match g.dispatch(w).unwrap() {
        Dispatch::Started { id, events } => {
            commit(g, events);
            id
        }
        other => panic!("expected a start for {w:?}, got {other:?}"),
    }
}
fn finish(g: &mut Graph, w: AgentId, a: HandoffArtifact) -> TaskId {
    let t = start(g, w);
    let ev = g.complete(w, &t, a, vec![]).unwrap();
    commit(g, ev);
    t
}
/// Replays events through JSON bodies and `fold_tasks`, as the session log would.
fn replay(events: &[Event]) -> Result<Graph, String> {
    let mut state = None;
    for e in events {
        state = Some(fold_tasks(state.as_ref(), e.name(), &e.body().map_err(|e| e.to_string())?)?);
    }
    project(state.as_ref())
}

// ───────────────────────────── seed ─────────────────────────────

#[test]
fn seed_is_idempotent_by_id_and_content() {
    let mut g = seeded(vec![spec("a", Kind::Explore, &[]), spec("b", Kind::Code, &["a"])], Settings::default());
    assert_eq!(g.node(&id(ROOT_GATE)).map(|n| n.depends_on.clone()), Some(vec![id("a"), id("b")]), "root gate over all roots");

    assert_eq!(g.seed(&Actor::Operator, vec![spec("a", Kind::Explore, &[])]).unwrap(), vec![], "same (id, content) is a no-op");
    let mut other = spec("a", Kind::Explore, &[]);
    other.content = "something else".into();
    assert_eq!(g.seed(&Actor::Operator, vec![other.clone()]), Err(TaskError::DuplicateNode(id("a"))));
    assert_eq!(
        g.seed(&Actor::Operator, vec![spec("c", Kind::Explore, &[]), spec("c", Kind::Explore, &[])]).unwrap().len(),
        2,
        "a duplicate inside one batch collapses (Seeded c + Rescoped root)"
    );

    // the fold itself is idempotent by (id, content) and refuses the conflicting seed
    let same = Event::Seeded {
        id: id("a"),
        content: "do a".into(),
        kind: Kind::Explore,
        depends_on: vec![],
        origin: aho_tasks::Origin::Seed,
        parent: None,
        by: Actor::Operator,
    };
    let before = g.clone();
    g.apply(&same).unwrap();
    assert_eq!(g, before);
    let clash = Event::Seeded {
        id: id("a"),
        content: "something else".into(),
        kind: Kind::Explore,
        depends_on: vec![],
        origin: aho_tasks::Origin::Seed,
        parent: None,
        by: Actor::Operator,
    };
    assert!(matches!(g.apply(&clash), Err(TaskError::DuplicateNode(_))));
}

#[test]
fn a_reseed_widens_and_reopens_the_root_gate() {
    let mut g = seeded(vec![spec("a", Kind::Explore, &[])], Settings::default());
    finish(&mut g, a(1), art("high"));
    finish(&mut g, a(1), gate_art(&["a"]));
    assert_eq!(g.node(&id(ROOT_GATE)).unwrap().status, Status::Completed);
    let ev = g.seed(&Actor::Operator, vec![spec("b", Kind::Explore, &[])]).unwrap();
    assert!(matches!(&ev[1], Event::Rescoped { id, add, .. } if id.as_str() == ROOT_GATE && add == &vec![aho_tasks::TaskId::new("b")]));
    commit(&mut g, ev);
    let root = g.node(&id(ROOT_GATE)).unwrap();
    assert_eq!((root.status, root.reopened, root.artifact.is_none()), (Status::Queued, 1, true));
}

#[test]
fn cycles_are_rejected_before_anything_commits() {
    let g = seeded(vec![spec("a", Kind::Explore, &[])], Settings::default());
    assert!(matches!(g.seed(&Actor::Operator, vec![spec("x", Kind::Explore, &["y"]), spec("y", Kind::Explore, &["x"])]),
        Err(TaskError::Cycle(ids)) if ids == vec![id("x"), id("y")]));
    // a top-level task waiting on the root gate would close a loop through it
    assert!(matches!(g.seed(&Actor::Operator, vec![spec("z", Kind::Explore, &[ROOT_GATE])]), Err(TaskError::Cycle(_))));
    assert!(matches!(g.seed(&Actor::Operator, vec![spec("w", Kind::Explore, &["nope"])]), Err(TaskError::UnknownDependency { .. })));

    let mut g = g;
    let t = start(&mut g, a(1));
    assert!(matches!(
        g.expand(a(1), &t, vec![child("c1", Kind::Explore, &["c2"]), child("c2", Kind::Explore, &["c1"])]),
        Err(TaskError::Cycle(_))
    ));
    assert!(matches!(g.expand(a(1), &t, vec![child("c1", Kind::Explore, &["a"])]), Err(TaskError::ChildDependsOnParent { .. })));
    assert_eq!(g.node(&t).unwrap().status, Status::Running, "a refused expansion changes nothing");
}

// ───────────────────────────── ownership ─────────────────────────────

#[test]
fn only_the_owner_expands_or_completes_and_only_once() {
    let mut g = seeded(vec![spec("p", Kind::Explore, &[])], Settings::default());
    let p = start(&mut g, a(1));
    assert!(matches!(g.expand(a(2), &p, vec![child("c", Kind::Explore, &[])]), Err(TaskError::NotOwner { by, .. }) if by == a(2)));
    assert!(matches!(g.complete(a(2), &p, art("high"), vec![]), Err(TaskError::NotOwner { .. })));
    let ev = g.expand(a(1), &p, vec![child("c", Kind::Explore, &[]), child("k", Kind::Code, &["c"])]).unwrap();
    commit(&mut g, ev);
    let pn = g.node(&p).unwrap();
    assert_eq!((pn.status, pn.expanded, pn.planner), (Status::Queued, true, Some(a(1))));
    assert_eq!(pn.depends_on, vec![id("p/critique"), id("p/verify")], "critique over explore, verify over code, before the join");
    assert_eq!(g.node(&id("p/verify")).unwrap().depends_on, vec![id("k")]);
    assert!(matches!(g.expand(a(1), &p, vec![child("d", Kind::Explore, &[])]), Err(TaskError::AlreadyExpanded(_))));

    // gates cannot expand; an agent stops only its own subtree; control is operator-only
    let mut h = seeded(vec![spec("x", Kind::Explore, &[])], Settings::default());
    let x = start(&mut h, a(5));
    assert!(matches!(h.stop(&Actor::Agent(a(6)), &x), Err(TaskError::NotInSubtree { .. })));
    assert!(h.stop(&Actor::Agent(a(5)), &x).is_ok());
    assert!(matches!(g.stop(&Actor::Agent(a(9)), &id("c")), Err(TaskError::NotInSubtree { .. })));
    assert!(g.stop(&Actor::Agent(a(1)), &id("c")).is_ok(), "the planner may stop its children");
    assert!(matches!(h.control(&Actor::Agent(a(5)), &x, Control::Replace, None, None), Err(TaskError::OperatorOnly(Control::Replace))));
}

// ───────────────────────────── artifacts ─────────────────────────────

#[test]
fn thin_artifacts_are_rejected_and_the_task_stays_running() {
    let thin = |v: serde_json::Value| HandoffArtifact::parse(&v).unwrap_err();
    assert_eq!(thin(json!({"findings": " ", "confidence": "high", "what_i_did_not_check": ["x"]})), TaskError::ThinArtifact("findings"));
    assert_eq!(
        thin(json!({"findings": "f", "confidence": "high", "what_i_did_not_check": []})),
        TaskError::ThinArtifact("what_i_did_not_check")
    );
    assert_eq!(thin(json!({"findings": "f", "confidence": "high"})), TaskError::ThinArtifact("what_i_did_not_check"));
    assert_eq!(
        thin(json!({"findings": "f", "confidence": "banana", "what_i_did_not_check": ["x"]})),
        TaskError::ThinArtifact("confidence")
    );
    assert_eq!(thin(json!({"findings": "f", "what_i_did_not_check": ["x"]})), TaskError::ThinArtifact("confidence"));
    assert!(matches!(thin(json!("just text")), TaskError::BadArtifact(_)));

    let mut g = seeded(vec![spec("a", Kind::Explore, &[])], Settings::default());
    let t = start(&mut g, a(1));
    let mut typed = art("high");
    typed.what_i_did_not_check = vec!["  ".into()];
    assert_eq!(g.complete(a(1), &t, typed.clone(), vec![]), Err(TaskError::ThinArtifact("what_i_did_not_check")));
    assert_eq!(g.node(&t).unwrap().status, Status::Running);
    // a forged thin completion fails the fold
    assert!(g.apply(&Event::Completed { id: t.clone(), by: a(1), artifact: typed, invariants: vec![] }).is_err());
    assert!(g.clone().apply(&Event::Completed { id: t, by: a(1), artifact: art("low"), invariants: vec![] }).is_ok());
}

#[test]
fn deep_mode_requires_a_contract_on_code_and_synthesize_edges() {
    let deep = Settings { deep: true, ..Settings::default() };
    let mut g = seeded(vec![spec("e", Kind::Explore, &[]), spec("k", Kind::Code, &[]), spec("s", Kind::Synthesize, &["e", "k"])], deep);
    let e = start(&mut g, a(1));
    let ev = g.complete(a(1), &e, art("high"), vec![]).unwrap();
    commit(&mut g, ev); // explore needs no contract
    let k = start(&mut g, a(1));
    assert_eq!(g.complete(a(1), &k, art("high"), vec![]), Err(TaskError::ContractRequired(id("k"))));
    let mut weak = code_art();
    if let Some(c) = weak.contract.as_mut() {
        c.acceptance.clear();
    }
    assert_eq!(g.complete(a(1), &k, weak, vec![]), Err(TaskError::ThinArtifact("contract.acceptance")));
    let ev = g.complete(a(1), &k, code_art(), vec![]).unwrap();
    commit(&mut g, ev);
    let s = start(&mut g, a(1));
    assert_eq!(g.complete(a(1), &s, art("high"), vec![]), Err(TaskError::ContractRequired(id("s"))));

    // the contract reaches the dependent's input, acceptance checks first-class
    let input = g.assemble_input(&s, Hydrate::Reference).unwrap();
    assert!(input.contains("# Inputs from completed dependencies"));
    assert!(input.contains("- output src/parser.rs (rust)") && input.contains("- cargo test -p parser"), "{input}");
    assert!(input.contains("contract {outputs"), "the hand-off section states the deep-mode contract rule");

    let mut normal = seeded(vec![spec("k", Kind::Code, &[])], Settings::default());
    let k = start(&mut normal, a(1));
    assert!(normal.complete(a(1), &k, art("high"), vec![]).is_ok(), "outside deep mode the contract is optional");
}

#[test]
fn confidence_parses_leniently_and_conservatively() {
    let c = |s: &str| Confidence::parse_text(s);
    assert_eq!(c("not confident"), Some(Confidence::Low));
    assert_eq!(c("I'm not very sure"), Some(Confidence::Low));
    assert_eq!(c("not entirely sure"), Some(Confidence::Medium));
    assert_eq!(c("isn't low"), Some(Confidence::Medium));
    assert_eq!(c("7/10"), Some(Confidence::Medium));
    assert_eq!(c("9 out of 10"), Some(Confidence::High));
    assert_eq!(c("0.9"), Some(Confidence::High));
    assert_eq!(c("85%"), Some(Confidence::High));
    assert_eq!(c("3"), Some(Confidence::Low));
    assert_eq!(c("HIGH"), Some(Confidence::High));
    assert_eq!(c("medium-high"), Some(Confidence::Medium), "several signals resolve to the lowest");
    assert_eq!(c("fairly confident"), Some(Confidence::Medium));
    assert_eq!(c("high, but 0.3 on the edge cases"), Some(Confidence::Low));
    assert_eq!(c("banana"), None);
    assert_eq!(c("not moderate"), None);
    assert_eq!(c(""), None);
    assert_eq!(Confidence::parse_lenient(&json!(0.55)), Some(Confidence::Medium));
    assert_eq!(Confidence::parse_lenient(&json!(250)), None);
    assert_eq!(Confidence::parse_lenient(&json!(true)), None);
}

// ───────────────────────────── gates ─────────────────────────────

/// `p` expanded into explore children c1 (high) and c2 (low); `p/critique` running under agent 2.
fn gate_fixture() -> Graph {
    let mut g = seeded(vec![spec("p", Kind::Explore, &[])], Settings::default());
    let p = start(&mut g, a(1));
    let ev = g.expand(a(1), &p, vec![child("c1", Kind::Explore, &[]), child("c2", Kind::Explore, &[])]).unwrap();
    commit(&mut g, ev);
    assert_eq!(finish(&mut g, a(3), art("high")), id("c1"));
    assert_eq!(finish(&mut g, a(3), art("not confident")), id("c2"));
    assert_eq!(start(&mut g, a(2)), id("p/critique"));
    g
}

#[test]
fn a_gate_cannot_pass_uncovered_or_over_low_confidence() {
    let mut g = gate_fixture();
    let gate = id("p/critique");
    assert_eq!(
        g.complete(a(2), &gate, gate_art(&["c1"]), vec![]),
        Err(TaskError::Gate { gate: gate.clone(), err: GateError::UncoveredSiblings(vec![id("c2")]) })
    );
    assert_eq!(
        g.complete(a(2), &gate, gate_art(&["c1", "c2"]), vec![]),
        Err(TaskError::Gate { gate: gate.clone(), err: GateError::UnaddressedLowConfidence(vec![id("c2")]) })
    );
    assert!(
        g.apply(&Event::Completed { id: gate.clone(), by: a(2), artifact: gate_art(&["c1", "c2"]), invariants: vec![] }).is_err(),
        "the fold refuses it too"
    );

    // the gate turns the miss into work, then goes back to the queue
    let ev = g
        .inject_from_gate(
            a(2),
            &gate,
            vec![Injection {
                id: id("c2.redo"),
                content: "re-check c2".into(),
                kind: Kind::Explore,
                depends_on: vec![],
                addresses: vec![id("c2")],
            }],
        )
        .unwrap();
    assert_eq!(ev.iter().map(Event::name).collect::<Vec<_>>(), ["Injected", "Released"]);
    commit(&mut g, ev);
    assert_eq!(g.node(&gate).unwrap().status, Status::Queued);
    assert_eq!(g.derived(&gate), Some(Derived::Blocked(vec![id("c2.redo")])));
    assert_eq!(finish(&mut g, a(3), art("medium")), id("c2.redo"));
    assert_eq!(start(&mut g, a(2)), gate);
    let ev = g.complete(a(2), &gate, gate_art(&["c1", "c2", "c2.redo"]), vec![]).unwrap();
    commit(&mut g, ev);
    assert_eq!(g.growth().gate, 1, "growth accounting sees gate-injected work");
}

#[test]
fn a_gate_is_stale_when_its_scope_grew_after_it_started() {
    let mut g = gate_fixture();
    let gate = id("p/critique");
    // a gap under `p` enters the running gate's scope
    let gap =
        SeedSpec { id: id("c3"), content: "the missing angle".into(), kind: Kind::Explore, depends_on: vec![], parent: Some(id("p")) };
    let ev = g.seed(&Actor::Operator, vec![gap]).unwrap();
    assert_eq!(ev.iter().map(Event::name).collect::<Vec<_>>(), ["Seeded", "Rescoped"]);
    commit(&mut g, ev);
    assert_eq!(g.node(&id("c3")).unwrap().origin, aho_tasks::Origin::Gap);
    assert_eq!(
        g.complete(a(2), &gate, gate_art(&["c1", "c2", "c3"]), vec![]),
        Err(TaskError::Gate { gate: gate.clone(), err: GateError::StaleGateScope { entered: vec![id("c3")] } })
    );
    // claiming again releases the stale gate and hands the worker the new work
    match g.dispatch(a(2)).unwrap() {
        Dispatch::Started { id: next, events } => {
            assert_eq!(next, id("c3"));
            assert_eq!(events.iter().map(Event::name).collect::<Vec<_>>(), ["Released", "Started"]);
            commit(&mut g, events);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(g.node(&gate).unwrap().status, Status::Queued);
}

fn seen(id: &str, status: InvariantStatus) -> InvariantSeen {
    InvariantSeen { id: id.into(), status }
}

#[test]
fn a_verify_gate_cannot_pass_while_a_covering_invariant_fails_or_is_stale() {
    let mut g = seeded(vec![spec("p", Kind::Code, &[])], Settings::default());
    let p = start(&mut g, a(1));
    let ev = g.expand(a(1), &p, vec![child("k", Kind::Code, &[])]).unwrap();
    commit(&mut g, ev);
    assert_eq!(finish(&mut g, a(2), code_art()), id("k"));
    let gate = start(&mut g, a(3));
    assert_eq!(gate, id("p/verify"));
    assert_eq!(g.gate_files(&gate), ["src/parser.rs"], "what the gate reviews: the contracts' outputs in its scope");

    let failing = vec![seen("inv.parse.total", InvariantStatus::Fail), seen("inv.parse.utf8", InvariantStatus::Pass)];
    let err = g.complete(a(3), &gate, gate_art(&["k"]), failing.clone()).unwrap_err();
    assert_eq!(
        err,
        TaskError::Gate { gate: gate.clone(), err: GateError::InvariantNotPassing(vec![seen("inv.parse.total", InvariantStatus::Fail)]) }
    );
    let text = err.to_string();
    assert!(text.contains("`inv.parse.total` fails") && text.contains("aho invariants show inv.parse.total"), "{text}");
    assert!(text.contains("the gate stays running") && text.contains("`invariant: <id>`"), "{text}");
    let stale = vec![seen("inv.parse.total", InvariantStatus::Stale)];
    assert!(matches!(
        g.complete(a(3), &gate, gate_art(&["k"]), stale.clone()),
        Err(TaskError::Gate { err: GateError::InvariantNotPassing(v), .. }) if v == stale
    ));
    // a pass written without the engine is refused by the fold
    let forged = Event::Completed { id: gate.clone(), by: a(3), artifact: gate_art(&["k"]), invariants: failing };
    assert!(g.apply(&forged).is_err());
    assert!(fold_tasks(Some(&serde_json::to_value(&g).unwrap()), "Completed", &forged.body().unwrap()).is_err(), "and so is its replay");

    // Unknown and Vacuous do not block (docs 09, 13 name Fail and Stale); the list is recorded
    let fine = vec![seen("inv.parse.total", InvariantStatus::Pass), seen("inv.parse.utf8", InvariantStatus::Unknown)];
    let ev = g.complete(a(3), &gate, gate_art(&["k"]), fine.clone()).unwrap();
    assert!(matches!(&ev[0], Event::Completed { invariants, .. } if *invariants == fine));
    assert_eq!(Event::parse("Completed", &ev[0].body().unwrap()).unwrap(), ev[0], "the log body keeps the list");
    commit(&mut g, ev);
    assert_eq!(g.node(&gate).unwrap().status, Status::Completed);

    // only a verify gate's pass names invariants
    let mut w = seeded(vec![spec("x", Kind::Explore, &[])], Settings::default());
    let x = start(&mut w, a(1));
    assert!(matches!(w.complete(a(1), &x, art("high"), vec![seen("inv.a", InvariantStatus::Pass)]), Err(TaskError::BadEvent(_))));
}

// ───────────────────────────── scheduling ─────────────────────────────

#[test]
fn blocked_is_derived_and_the_synthesis_join_prefers_its_planner() {
    let mut g = seeded(
        vec![spec("p", Kind::Explore, &[]), spec("q", Kind::Explore, &[]), spec("after", Kind::Synthesize, &["p", "q"])],
        Settings::default(),
    );
    assert_eq!(g.derived(&id("after")), Some(Derived::Blocked(vec![id("p"), id("q")])));
    assert_eq!(g.node(&id("after")).unwrap().status, Status::Queued, "blocked is never stored");
    assert_eq!(g.ready(), vec![id("p"), id("q")]);

    assert_eq!(start(&mut g, a(1)), id("p"));
    let ev = g.expand(a(1), &id("p"), vec![child("c", Kind::Explore, &[])]).unwrap();
    commit(&mut g, ev);
    assert_eq!(start(&mut g, a(2)), id("q")); // agent 2 keeps q running
    assert_eq!(finish(&mut g, a(3), art("high")), id("c"));
    assert_eq!(finish(&mut g, a(3), gate_art(&["c"])), id("p/critique"));
    let ev = g.seed(&Actor::Operator, vec![spec("r", Kind::Explore, &[])]).unwrap();
    commit(&mut g, ev);
    assert_eq!(start(&mut g, a(3)), id("r"), "another worker takes unclaimed work before someone else's join");
    assert_eq!(start(&mut g, a(1)), id("p"), "the planner gets its synthesis join");
    let input = g.assemble_input(&id("p"), Hydrate::Reference).unwrap();
    assert!(input.contains("synthesis join") && input.contains("## c (explore)") && input.contains("## p/critique"), "{input}");
    assert!(matches!(g.dispatch(a(1)).unwrap(), Dispatch::Holding(t) if t == id("p")));
}

#[test]
fn live_worker_and_member_caps_hold() {
    let settings = Settings { max_live_workers: 1, max_members: 2, ..Settings::default() };
    let mut g = seeded(vec![spec("a", Kind::Explore, &[]), spec("b", Kind::Explore, &[]), spec("c", Kind::Explore, &[])], settings);
    start(&mut g, a(1));
    assert!(matches!(g.dispatch(a(2)).unwrap(), Dispatch::AtCapacity(why) if why.contains("swarm.max_live_workers")));
    let ev = g.complete(a(1), &id("a"), art("high"), vec![]).unwrap();
    commit(&mut g, ev);
    finish(&mut g, a(2), art("high"));
    assert!(matches!(g.dispatch(a(3)).unwrap(), Dispatch::AtCapacity(why) if why.contains("swarm.max_members")));
    assert_eq!(start(&mut g, a(1)), id("c"), "existing members still work");
}

#[test]
fn a_turn_without_an_artifact_requeues_once_then_fails() {
    let mut g = seeded(vec![spec("a", Kind::Explore, &[]), spec("b", Kind::Explore, &["a"])], Settings::default());
    start(&mut g, a(1));
    let ev = g.turn_ended(a(1)).unwrap();
    assert!(matches!(ev.as_slice(), [Event::Requeued { .. }]));
    commit(&mut g, ev);
    assert!(g.apply(&Event::Requeued { id: id("a"), by: a(1) }).is_err(), "requeue needs a running holder");
    start(&mut g, a(2));
    let forged = Event::Requeued { id: id("a"), by: a(2) };
    assert!(g.clone().apply(&forged).is_err(), "only one requeue without an artifact");
    let ev = g.turn_ended(a(2)).unwrap();
    assert!(matches!(ev.as_slice(), [Event::Failed { reason: FailReason::NoArtifact, .. }]));
    commit(&mut g, ev);
    assert_eq!(g.derived(&id("b")), Some(Derived::Blocked(vec![id("a")])));
    assert!(matches!(g.dispatch(a(3)).unwrap(), Dispatch::Idle { running: 0, .. }));
}

// ───────────────────────────── reclaim, resume, control ─────────────────────────────

#[test]
fn dead_assignees_are_reclaimed_up_to_the_cap_then_the_task_fails() {
    let settings = Settings { max_reclaims: 2, ..Settings::default() };
    let mut g = seeded(vec![spec("t", Kind::Explore, &[])], settings);
    for (n, w) in [(1u32, a(1)), (2, a(2))] {
        assert_eq!(start(&mut g, w), id("t"));
        let ev = g.reclaim_dead(w).unwrap();
        assert_eq!(ev, vec![Event::Reclaimed { id: id("t"), n, from: w }]);
        commit(&mut g, ev);
        assert_eq!((g.node(&id("t")).unwrap().status, g.node(&id("t")).unwrap().reclaims), (Status::Queued, n));
    }
    start(&mut g, a(3));
    let forged = Event::Reclaimed { id: id("t"), n: 3, from: a(3) };
    assert!(g.clone().apply(&forged).is_err(), "the fold enforces the cap");
    let ev = g.reclaim_dead(a(3)).unwrap();
    assert_eq!(ev, vec![Event::Failed { id: id("t"), reason: FailReason::ReclaimCap }]);
    commit(&mut g, ev);
    assert_eq!(g.derived(&id("t")), Some(Derived::Failed(Some(FailReason::ReclaimCap))));

    // operator retry resets the breaker
    assert!(g.control(&Actor::Operator, &id("t"), Control::Replace, None, None).is_ok());
    let ev = g.control(&Actor::Operator, &id("t"), Control::Retry, None, None).unwrap();
    commit(&mut g, ev);
    let t = g.node(&id("t")).unwrap();
    assert_eq!((t.status, t.reclaims, t.failure), (Status::Queued, 0, None));
}

#[test]
fn resume_marks_running_work_stale_until_its_worker_shows_life() {
    let mut g = seeded(vec![spec("a", Kind::Explore, &[]), spec("b", Kind::Explore, &[])], Settings::default());
    start(&mut g, a(1));
    start(&mut g, a(2));
    let ev = g.running_stale();
    assert_eq!(ev, vec![Event::Resumed {}]);
    commit(&mut g, ev);
    assert_eq!(g.derived(&id("a")), Some(Derived::Stale(Some(a(1)))));
    let ev = g.heartbeat(a(1), &id("a"), Some("read src/lib.rs → ok".into())).unwrap();
    commit(&mut g, ev);
    assert_eq!(g.node(&id("a")).unwrap().status, Status::Running);
    let ev = g.complete(a(2), &id("b"), art("high"), vec![]).unwrap();
    commit(&mut g, ev); // a stale holder may still finish
    assert!(g.running_stale().len() == 1 && g.node(&id("b")).unwrap().status == Status::Completed);
}

#[test]
fn control_ops_respect_safe_states() {
    let mut g = seeded(vec![spec("a", Kind::Explore, &[])], Settings::default());
    assert!(matches!(
        g.control(&Actor::Operator, &id("a"), Control::Retry, None, None),
        Err(TaskError::UnsafeControl { status: Status::Queued, .. })
    ));
    start(&mut g, a(1));
    let err = g.control(&Actor::Operator, &id("a"), Control::Replace, None, None).unwrap_err();
    assert!(err.to_string().contains("stop it first"), "{err}");
    assert!(matches!(g.control(&Actor::Operator, &id("a"), Control::Reassign, None, None), Err(TaskError::UnsafeControl { .. })));
    let ev = g.running_stale();
    commit(&mut g, ev);
    assert!(
        matches!(g.control(&Actor::Operator, &id("a"), Control::Reassign, None, None), Err(TaskError::BadEvent(_))),
        "reassign names a target"
    );
    assert!(matches!(g.control(&Actor::Operator, &id("a"), Control::Salvage, None, None), Err(TaskError::NothingToSalvage(_))));
    let ev = g.control(&Actor::Operator, &id("a"), Control::Reassign, Some(a(4)), None).unwrap();
    commit(&mut g, ev);
    assert!(matches!(g.dispatch(a(1)).unwrap(), Dispatch::Idle { .. }), "pinned to agent 4");
    assert_eq!(start(&mut g, a(4)), id("a"));
    let ev = g.complete(a(4), &id("a"), art("high"), vec![]).unwrap();
    commit(&mut g, ev);
    let err = g.control(&Actor::Operator, &id("a"), Control::Retry, None, None).unwrap_err();
    assert!(matches!(err, TaskError::UnsafeControl { status: Status::Completed, .. }));
}

#[test]
fn salvage_forwards_the_checkpoint_and_tool_summaries() {
    let mut g = seeded(vec![spec("t", Kind::Code, &[])], Settings::default());
    start(&mut g, a(1));
    let ev =
        g.checkpoint(a(1), &id("t"), "ported 3 of 5 modules", vec!["read src/a.rs → ok".into(), "cargo check → 2 errors".into()]).unwrap();
    commit(&mut g, ev);
    let ev = g.running_stale();
    commit(&mut g, ev);
    let ev = g.control(&Actor::Operator, &id("t"), Control::Salvage, Some(a(2)), None).unwrap();
    let Event::Control { salvage: Some(s), .. } = &ev[0] else { panic!("{ev:?}") };
    assert_eq!((s.from, s.checkpoint.as_deref(), s.summaries.len()), (Some(a(1)), Some("ported 3 of 5 modules"), 2));
    commit(&mut g, ev);
    assert_eq!(start(&mut g, a(2)), id("t"));
    let input = g.assemble_input(&id("t"), Hydrate::Reference).unwrap();
    assert!(input.contains("# Salvaged from agent:1"), "{input}");
    assert!(input.contains("Checkpoint: ported 3 of 5 modules") && input.contains("- cargo check → 2 errors"), "{input}");

    // the same snapshot as a fold over the dead worker's log
    let log = MemoryLog::new();
    let dead = Principal::Agent { id: a(1), turn: TurnId(3) };
    let other = Principal::Agent { id: a(9), turn: TurnId(3) };
    let call = |c: &str, tool: &str| EntryKind::ToolCall {
        call: CallId(c.into()),
        tool: tool.into(),
        args: json!({"path": "src/a.rs"}),
        class: Consequence::ReadOnly,
    };
    let result = |c: &str, text: &str| EntryKind::ToolResult {
        call: CallId(c.into()),
        outcome: ToolOutcome::Ok,
        value: json!(null),
        content: vec![ContentBlock::text(text)],
        spill_ref: None,
        meta: None,
        late: false,
    };
    log.append(None, dead.clone(), call("1", "read")).unwrap();
    log.append(None, Principal::system("session"), result("1", "fn main() {}")).unwrap();
    log.append(None, other.clone(), call("2", "grep")).unwrap();
    log.append(None, Principal::system("session"), result("2", "ignored")).unwrap();
    let mine = Event::Checkpoint { id: id("t"), by: a(1), summary: "halfway through the port".into(), recent: vec![] };
    log.append(None, dead.clone(), mine.to_entry().unwrap()).unwrap();
    log.append(None, dead, call("3", "bash")).unwrap();
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let s = Salvage::from_log(&entries, Some(a(1)), 8);
    assert_eq!(s.checkpoint.as_deref(), Some("halfway through the port"));
    assert_eq!(s.summaries.len(), 2, "{:?}", s.summaries);
    assert!(s.summaries[0].starts_with("read(") && s.summaries[0].contains("ok: fn main() {}"));
    assert!(s.summaries[1].contains("died mid-call"));
}

#[test]
fn a_failed_acceptance_check_reopens_the_producer() {
    let mut g = seeded(
        vec![spec("prod", Kind::Code, &[]), spec("cons", Kind::Code, &["prod"]), spec("other", Kind::Explore, &["prod"])],
        Settings::default(),
    );
    finish(&mut g, a(1), code_art());
    assert_eq!(start(&mut g, a(2)), id("cons"));
    assert!(matches!(g.reopen(a(2), &id("cons"), &id("other"), "x"), Err(TaskError::NotAnInput { .. })));
    assert!(matches!(g.reopen(a(3), &id("cons"), &id("prod"), "x"), Err(TaskError::NotOwner { .. })));
    let ev = g.reopen(a(2), &id("cons"), &id("prod"), "cargo test -p parser: 2 failures").unwrap();
    commit(&mut g, ev);
    let p = g.node(&id("prod")).unwrap();
    assert_eq!((p.status, p.reopened, p.artifact.is_none()), (Status::Queued, 1, true));
    assert_eq!(g.derived(&id("cons")), Some(Derived::Blocked(vec![id("prod")])));
    let input = g.assemble_input(&id("prod"), Hydrate::Inline).unwrap();
    assert!(input.contains("re-opened") && input.contains("2 failures"), "{input}");
}

// ───────────────────────────── the log ─────────────────────────────

#[test]
fn the_session_log_fold_accepts_engine_events_and_rejects_a_bad_one() {
    let log = MemoryLog::new();
    let mut folds = CustomFolds::new();
    folds.insert(NS.to_string(), fold_tasks as CustomFold);
    let by = Principal::Agent { id: a(1), turn: TurnId(1) };
    let mut g = Graph::default();
    let mut all = Vec::new();
    let mut push = |g: &mut Graph, ev: Vec<Event>| {
        for e in &ev {
            log.append(None, by.clone(), e.to_entry().unwrap()).unwrap();
        }
        all.extend(ev.clone());
        commit(g, ev);
    };
    let ev = g.seed(&Actor::Operator, vec![spec("a", Kind::Explore, &[])]).unwrap();
    push(&mut g, ev);
    let Dispatch::Started { events, .. } = g.dispatch(a(1)).unwrap() else { panic!() };
    push(&mut g, events);
    let ev = g.complete(a(1), &id("a"), art("0.9"), vec![]).unwrap();
    push(&mut g, ev);
    let state = fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds).unwrap();
    assert_eq!(project(state.custom.get(NS)).unwrap(), g, "the log fold equals the engine's staged graph");
    assert_eq!(replay(&all).unwrap(), g);

    // completing twice is not a legal transition: replay fails loudly
    let again = Event::Completed { id: id("a"), by: a(1), artifact: art("high"), invariants: vec![] };
    log.append(None, by, again.to_entry().unwrap()).unwrap();
    let err = fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds).unwrap_err();
    assert!(matches!(&err, LogError::Invariant { rule: "custom.fold", detail, .. } if detail.contains("Task/Completed")), "{err:?}");
    assert!(fold_tasks(None, "Exploded", &json!({})).unwrap_err().contains("unknown task event"));
}

#[test]
fn inputs_are_hydrated_by_reference_by_default() {
    let mut g = seeded(vec![spec("a", Kind::Explore, &[]), spec("b", Kind::Synthesize, &["a"])], Settings::default());
    let long = "x".repeat(2000);
    let artifact = HandoffArtifact::parse(&json!({
        "findings": long, "evidence": ["log line 12"], "confidence": "low", "what_i_did_not_check": ["the retry path"],
        "open_questions": "is 3 retries enough?",
    }))
    .unwrap();
    finish(&mut g, a(1), artifact);
    start(&mut g, a(1));
    let by_ref = g.assemble_input(&id("b"), Hydrate::Reference).unwrap();
    assert!(by_ref.contains("# Inputs from completed dependencies"));
    assert!(by_ref.contains("full artifact: task_status {\"id\": \"a\"}") && by_ref.contains("… (excerpt)"));
    assert!(by_ref.contains("Not checked by the producer:\n- the retry path") && by_ref.contains("- is 3 retries enough?"));
    assert!(by_ref.contains("LOW confidence") && !by_ref.contains("log line 12"));
    let inline = g.assemble_input(&id("b"), Hydrate::Inline).unwrap();
    assert!(inline.contains("Evidence:\n- log line 12") && !inline.contains("(excerpt)"));
    assert!(g.render_status().contains("seeded 2 · grown 0"));
}
