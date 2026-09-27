#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
use std::cell::RefCell;
use std::rc::Rc;

use aho_tasks::sim::{Action, Outcome, Script, Sim, Turn};
use aho_tasks::{Actor, ChildSpec, GateError, HandoffArtifact, Injection, Kind, ROOT_GATE, SeedSpec, Settings, Status, TaskError, TaskId};
use aho_types::AgentId;
use serde_json::json;

fn id(s: &str) -> TaskId {
    TaskId::new(s)
}
fn art(conf: &str) -> HandoffArtifact {
    HandoffArtifact::parse(&json!({"findings": "found it", "confidence": conf, "what_i_did_not_check": ["load behaviour"]})).unwrap()
}
fn covers(ids: &[TaskId]) -> HandoffArtifact {
    let ids: Vec<&str> = ids.iter().map(TaskId::as_str).collect();
    HandoffArtifact::parse(&json!({"findings": "reviewed", "confidence": "high", "what_i_did_not_check": ["perf"], "covers": ids})).unwrap()
}

/// One script for every worker: the behaviour depends only on the node.
fn script(inputs: Rc<RefCell<Vec<(TaskId, String)>>>) -> Script {
    Box::new(move |t: &Turn<'_>| {
        inputs.borrow_mut().push((t.node.id.clone(), t.input.clone()));
        match t.node.id.as_str() {
            "survey" if !t.node.expanded => Action::Expand(vec![
                ChildSpec { id: id("survey.a"), content: "map the API".into(), kind: Kind::Explore, depends_on: vec![] },
                ChildSpec { id: id("survey.b"), content: "map the storage".into(), kind: Kind::Explore, depends_on: vec![] },
            ]),
            "survey" => Action::Complete(art("high")),
            "survey.a" => Action::Complete(art("0.9")),
            "survey.b" => Action::Complete(art("not confident")),
            "survey.b2" => Action::Complete(art("7/10")),
            "survey/critique" => {
                if t.graph.node(&id("survey.b2")).is_some() {
                    Action::Complete(covers(&t.node.depends_on))
                } else if t.node.no_artifact_requeues == 0 {
                    // first try: rubber-stamp over the LOW sibling, which the structure refuses
                    Action::Complete(covers(&t.node.depends_on))
                } else {
                    Action::Inject(vec![Injection {
                        id: id("survey.b2"),
                        content: "re-check the storage map".into(),
                        kind: Kind::Explore,
                        depends_on: vec![],
                        addresses: vec![id("survey.b")],
                    }])
                }
            }
            "impl" if t.node.no_artifact_requeues == 0 => Action::Complete(art("high")), // no contract in deep mode
            "impl" => Action::Complete(
                HandoffArtifact::parse(&json!({
                    "findings": "implemented the adapter", "confidence": "medium", "what_i_did_not_check": ["concurrent writers"],
                    "contract": {"outputs": [{"path": "src/adapter.rs", "format": "rust"}], "units": ["ms"], "version": "1",
                                 "acceptance": ["cargo test -p adapter"]},
                }))
                .unwrap_or_else(|e| panic!("{e}")),
            ),
            ROOT_GATE => Action::Complete(covers(&t.node.depends_on)),
            other => panic!("unexpected task {other}"),
        }
    })
}

#[test]
fn simulator_drives_seed_expand_gates_inject_and_finishes() {
    let inputs = Rc::new(RefCell::new(Vec::new()));
    let mut sim = Sim::new(Settings { deep: true, max_live_workers: 2, ..Settings::default() }).unwrap();
    sim.seed(
        Actor::Operator,
        vec![
            SeedSpec { id: id("survey"), content: "survey the codebase".into(), kind: Kind::Explore, depends_on: vec![], parent: None },
            SeedSpec {
                id: id("impl"),
                content: "implement the adapter".into(),
                kind: Kind::Code,
                depends_on: vec![id("survey")],
                parent: None,
            },
        ],
    )
    .unwrap();
    sim.add_worker(AgentId(1), script(inputs.clone()));
    sim.add_worker(AgentId(2), script(inputs.clone()));

    let outcome = sim.run(50).unwrap();
    assert!(matches!(outcome, Outcome::Finished { .. }), "{outcome:?}\n{}", sim.graph().render_status());
    assert!(sim.graph().nodes().iter().all(|n| n.status == Status::Completed), "{}", sim.graph().render_status());

    // the gate refused to pass over the LOW sibling; deep mode refused the contract-less code edge
    let refusals: Vec<(&str, &TaskError)> = sim.rejected().iter().map(|(_, t, e)| (t.as_str(), e)).collect();
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    assert!(
        matches!(refusals[0], ("survey/critique", TaskError::Gate { err: GateError::UnaddressedLowConfidence(ids), .. }) if ids == &vec![id("survey.b")])
    );
    assert!(matches!(refusals[1], ("impl", TaskError::ContractRequired(_))));

    let names = sim.names();
    for kind in ["Configured", "Seeded", "Expanded", "Started", "Completed", "Requeued", "Injected", "Released"] {
        assert!(names.contains(&kind), "missing Task/{kind} in {names:?}");
    }
    let growth = sim.graph().growth();
    assert_eq!((growth.seed, growth.expand, growth.gap, growth.gate, growth.gates), (2, 2, 0, 1, 2));

    // the synthesis join saw its children's artifacts through the gate
    let inputs = inputs.borrow();
    let (_, join) = inputs.iter().rev().find(|(t, _)| t.as_str() == "survey").unwrap();
    assert!(join.contains("# Inputs from completed dependencies"));
    for dep in ["## survey/critique", "## survey.a", "## survey.b ", "## survey.b2"] {
        assert!(join.contains(dep), "{dep} missing from\n{join}");
    }
    let (_, impl_input) = inputs.iter().rev().find(|(t, _)| t.as_str() == "impl").unwrap();
    assert!(impl_input.contains("## survey (explore)"));

    // replaying the recorded Task/* bodies through fold_tasks reproduces the graph exactly
    assert_eq!(&sim.replay().unwrap(), sim.graph());
}

#[test]
fn simulator_reclaims_a_dead_worker_and_salvage_is_not_needed_to_finish() {
    let mut sim = Sim::new(Settings::default()).unwrap();
    sim.seed(
        Actor::Operator,
        vec![SeedSpec { id: id("t"), content: "long job".into(), kind: Kind::Explore, depends_on: vec![], parent: None }],
    )
    .unwrap();
    let mut died = false;
    sim.add_worker(
        AgentId(1),
        Box::new(move |t: &Turn<'_>| {
            if t.node.id.as_str() == "t" && !died {
                died = true;
                return Action::Die;
            }
            Action::Complete(covers(&t.node.depends_on))
        }),
    );
    sim.add_worker(
        AgentId(2),
        Box::new(|t: &Turn<'_>| match t.node.id.as_str() {
            "t" if t.attempt == 1 => Action::Checkpoint { summary: "halfway".into(), recent: vec!["grep → 3 hits".into()] },
            "t" => Action::Complete(art("medium")),
            _ => Action::Complete(covers(&t.node.depends_on)),
        }),
    );
    let outcome = sim.run(20).unwrap();
    assert!(matches!(outcome, Outcome::Finished { .. }), "{outcome:?}");
    let t = sim.graph().node(&id("t")).unwrap();
    assert_eq!((t.reclaims, t.checkpoint.as_deref(), t.last_owner), (1, Some("halfway"), Some(AgentId(2))));
    assert!(sim.names().contains(&"Reclaimed") && sim.names().contains(&"Checkpoint"));
    assert_eq!(&sim.replay().unwrap(), sim.graph());
}

#[test]
fn simulator_reports_stuck_when_failed_work_blocks_the_rest() {
    let mut sim = Sim::new(Settings::default()).unwrap();
    sim.seed(
        Actor::Operator,
        vec![
            SeedSpec { id: id("a"), content: "never delivers".into(), kind: Kind::Explore, depends_on: vec![], parent: None },
            SeedSpec { id: id("b"), content: "needs a".into(), kind: Kind::Explore, depends_on: vec![id("a")], parent: None },
        ],
    )
    .unwrap();
    sim.add_worker(AgentId(1), Box::new(|_: &Turn<'_>| Action::EndTurn));
    let outcome = sim.run(20).unwrap();
    assert!(matches!(outcome, Outcome::Stuck { .. }), "{outcome:?}");
    assert_eq!(sim.graph().node(&id("a")).unwrap().status, Status::Failed);
    assert_eq!(sim.names().iter().filter(|n| **n == "Requeued").count(), 1, "requeued exactly once before failing");
}
