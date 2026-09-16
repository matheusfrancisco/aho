//! The ralph loop state machine: planner, promise matching, and fold-validated transitions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use aho_control::TurnEnd;
use aho_goals::Actor;
use aho_types::{AgentId, Timestamp};
use tool_workflow::{Event, LoopPlan, Outcome, Phase, WorkflowError, Workflows, fold_workflows, project, promise_in};

const AGENT: Actor = Actor::Agent(AgentId(0));

fn apply(w: &mut Workflows, events: &[Event]) {
    *w = w.stage(events).unwrap();
}

fn end(text: &str) -> TurnEnd {
    TurnEnd { final_text: text.into(), failed: None, ran: true }
}

fn feed(w: &Workflows) -> (String, u32, String, String) {
    match w.next(Timestamp(5)).unwrap() {
        LoopPlan::Feed { id, n, key, text } => (id, n, key, text),
        other => panic!("expected a feed, got {other:?}"),
    }
}

#[test]
fn the_promise_counts_only_inside_its_tags() {
    assert!(promise_in("all green\n<promise>DONE</promise>", "DONE"));
    assert!(promise_in("<promise>  ALL   TESTS\nPASS </promise>", "ALL TESTS PASS"));
    assert!(promise_in("<promise>no</promise> then <promise>DONE</promise>", "DONE"), "any tagged occurrence");
    assert!(!promise_in("I will print DONE when finished", "DONE"), "a bare mention is not the promise");
    assert!(!promise_in("<promise>done</promise>", "DONE"), "case matters");
    assert!(!promise_in("<promise>DONE", "DONE"), "an unclosed tag is not a promise");
    assert!(!promise_in("<promise>NOT DONE</promise>", "DONE"));
    assert!(!promise_in("<promise></promise>", " "));
}

#[test]
fn the_loop_feeds_the_same_prompt_until_an_iteration_keeps_the_promise() {
    let mut w = Workflows::default();
    assert_eq!(w.next(Timestamp(1)).unwrap(), LoopPlan::Idle);
    let s = w.start("fix the failing tests", " DONE ", 5, &AGENT, Timestamp(1)).unwrap();
    apply(&mut w, &[s]);
    let (id, n, key, text) = feed(&w);
    assert_eq!((id.as_str(), n, key.as_str()), ("ralph-1", 1, "workflow:ralph-1:1"));
    assert!(text.starts_with("fix the failing tests\n\n[Ralph loop ralph-1: iteration 1 of 5."), "{text}");
    assert!(text.contains("<promise>DONE</promise>"), "{text}");

    let evs = w.after_turn(&id, 1, &end("still two failures; I will say DONE later"), Timestamp(10)).unwrap();
    apply(&mut w, &evs);
    let (_, n, key, text2) = feed(&w);
    assert_eq!((n, key.as_str()), (2, "workflow:ralph-1:2"));
    assert_eq!(text2.split("\n\n").next(), text.split("\n\n").next(), "the prompt itself never changes");

    let evs = w.after_turn(&id, 2, &end("all pass\n<promise>DONE</promise>"), Timestamp(20)).unwrap();
    apply(&mut w, &evs);
    let LoopPlan::Finish { event } = w.next(Timestamp(21)).unwrap() else { panic!() };
    assert!(matches!(&event, Event::Finished { outcome: Outcome::Completed, by: Actor::System(s), .. } if s == "workflow"));
    apply(&mut w, &[event]);
    let r = w.get("ralph-1").unwrap();
    assert_eq!((r.phase, r.iterations), (Phase::Completed, 2));
    assert_eq!(w.next(Timestamp(30)).unwrap(), LoopPlan::Idle);
}

#[test]
fn the_budget_ends_an_unfinished_loop_and_a_failed_turn_ends_it_early() {
    let mut w = Workflows::default();
    let s = w.start("p", "DONE", 2, &AGENT, Timestamp(1)).unwrap();
    apply(&mut w, &[s]);
    for n in 1..=2 {
        let evs = w.after_turn("ralph-1", n, &end("not yet"), Timestamp(2)).unwrap();
        apply(&mut w, &evs);
    }
    let LoopPlan::Finish { event } = w.next(Timestamp(3)).unwrap() else { panic!() };
    assert!(matches!(event, Event::Finished { outcome: Outcome::Exhausted, .. }));
    apply(&mut w, &[event]);

    let s = w.start("p", "DONE", 5, &AGENT, Timestamp(4)).unwrap();
    apply(&mut w, &[s]);
    let broken = TurnEnd { final_text: String::new(), failed: Some("model call failed: 503".into()), ran: true };
    let evs = w.after_turn("ralph-2", 1, &broken, Timestamp(5)).unwrap();
    assert_eq!(evs.len(), 2);
    apply(&mut w, &evs);
    let r = w.get("ralph-2").unwrap();
    assert_eq!(r.phase, Phase::Failed);
    assert!(r.reason.as_deref().unwrap().contains("model call failed: 503"));
    // but a failed turn that still carried the promise completes
    let s = w.start("p", "DONE", 5, &AGENT, Timestamp(6)).unwrap();
    apply(&mut w, &[s]);
    let late = TurnEnd { final_text: "<promise>DONE</promise>".into(), failed: Some("stream cut".into()), ran: true };
    let evs = w.after_turn("ralph-3", 1, &late, Timestamp(7)).unwrap();
    assert_eq!(evs.len(), 1);
    apply(&mut w, &evs);
    assert!(matches!(w.next(Timestamp(8)).unwrap(), LoopPlan::Finish { event: Event::Finished { outcome: Outcome::Completed, .. } }));
}

#[test]
fn one_loop_at_a_time_and_agents_stop_only_agent_loops() {
    let mut w = Workflows::default();
    let s = w.start("p", "DONE", 3, &Actor::Operator, Timestamp(1)).unwrap();
    apply(&mut w, &[s]);
    assert!(matches!(w.start("q", "X", 3, &AGENT, Timestamp(2)), Err(WorkflowError::AnotherRunning { .. })));
    let e = w.stop(None, &AGENT, Timestamp(2), None).unwrap_err();
    assert!(matches!(e, WorkflowError::NotYours { owner: Actor::Operator, .. }), "{e}");
    let stop = w.stop(None, &Actor::Operator, Timestamp(3), Some("changed my mind".into())).unwrap();
    apply(&mut w, &[stop]);
    assert_eq!(w.get("ralph-1").unwrap().phase, Phase::Stopped);
    assert!(matches!(w.stop(None, &Actor::Operator, Timestamp(4), None), Err(WorkflowError::NoneRunning)));
    assert!(matches!(w.stop(Some("ralph-1"), &Actor::Operator, Timestamp(4), None), Err(WorkflowError::NotRunning { .. })));
    assert!(matches!(w.start(" ", "X", 3, &AGENT, Timestamp(5)), Err(WorkflowError::Empty("prompt"))));
    assert!(matches!(w.start("q", "  ", 3, &AGENT, Timestamp(5)), Err(WorkflowError::Empty("completion_promise"))));
}

#[test]
fn the_fold_refuses_transitions_the_planner_would_never_make() {
    let mut w = Workflows::default();
    let s = w.start("p", "DONE", 2, &AGENT, Timestamp(1)).unwrap();
    apply(&mut w, std::slice::from_ref(&s));
    let it = |n: u32, found: bool| Event::Iteration { id: "ralph-1".into(), n, promise_found: found, at: Timestamp(2) };
    let fin = |o: Outcome, by: Actor| Event::Finished { id: "ralph-1".into(), outcome: o, by, at: Timestamp(3), reason: None };
    let driver = Actor::System("workflow".into());
    assert!(w.stage(&[it(2, false)]).is_err(), "skipped iteration");
    assert!(w.stage(&[it(1, false), it(2, false), it(3, false)]).is_err(), "past the budget");
    assert!(w.stage(&[fin(Outcome::Completed, driver.clone())]).is_err(), "completed without the promise");
    assert!(w.stage(&[it(1, true), fin(Outcome::Completed, AGENT)]).is_err(), "only the driver completes a loop");
    assert!(w.stage(&[it(1, false), fin(Outcome::Exhausted, driver.clone())]).is_err(), "budget not spent");
    assert!(w.stage(&[fin(Outcome::Failed, driver.clone())]).is_err(), "a failure names its reason");
    assert!(w.stage(&[it(1, true), fin(Outcome::Completed, driver.clone())]).is_ok());
    assert!(w.stage(std::slice::from_ref(&s)).is_err(), "an id starts once");

    // the projection round-trips through the CustomFold, and replays the same result
    let mut v = None;
    let mut replay = Workflows::default();
    for e in [s, it(1, false), it(2, true), fin(Outcome::Completed, driver)] {
        v = Some(fold_workflows(v.as_ref(), e.name(), &e.body().unwrap()).unwrap());
        replay = replay.stage(&[e]).unwrap();
    }
    assert_eq!(project(v.as_ref()).unwrap(), replay);
    assert!(
        fold_workflows(v.as_ref(), "Iteration", &serde_json::json!({"id": "ralph-1", "n": 3, "promise_found": false, "at": 9})).is_err()
    );
    assert!(fold_workflows(None, "Resumed", &serde_json::json!({})).is_err());
}
