use super::*;
use aho_log::{CustomFolds, LogError, LogStore, MemoryLog, fold};
use aho_types::TurnId;

const AGENT: Actor = Actor::Agent(AgentId(7));
const OTHER: Actor = Actor::Agent(AgentId(9));

fn t(ms: u64) -> Timestamp {
    Timestamp(ms)
}

fn create(objective: &str) -> Op {
    Op::Create { title: String::new(), objective: objective.into(), max_rounds: 3, paused: false }
}

/// Plans and commits, like a tool followed by the session appending `value.events`.
fn run(gs: &mut Goals, target: Option<&str>, op: Op, by: &Actor, at: u64) -> Result<Vec<Event>, GoalError> {
    let ev = gs.plan(target, op, by, t(at))?;
    *gs = gs.stage(&ev)?;
    Ok(ev)
}

/// Replays events through the JSON body + `fold_goals` path the session log uses.
fn replay(events: &[Event]) -> Result<Goals, String> {
    let mut state = None;
    for e in events {
        state = Some(fold_goals(state.as_ref(), e.name(), &e.body().map_err(|e| e.to_string())?)?);
    }
    project(state.as_ref())
}

fn changed(goal: Goal, op: Op, by: Actor) -> Event {
    Event::Changed { op, goal, by }
}

#[test]
fn transition_table_accepts_every_legal_op_and_preserves_counters() {
    let mut gs = Goals::default();
    let mut log = run(&mut gs, None, create("ship the parser"), &AGENT, 100).unwrap();
    let g = gs.current().unwrap().clone();
    assert_eq!((g.id.as_str(), g.revision, g.phase, g.title.as_str()), ("goal-1", 1, Phase::Active, "ship the parser"));

    log.extend(
        gs.next_round(AgentId(7), true, t(110))
            .map(|p| match p {
                RoundPlan::Start { events, round, .. } => {
                    assert_eq!(round, 1);
                    events
                }
                other => panic!("{other:?}"),
            })
            .unwrap(),
    );
    gs = gs.stage(&log[log.len() - 1..]).unwrap();

    let steps: Vec<(Op, Phase)> = vec![
        (Op::Pause, Phase::Paused),
        (Op::Edit { title: Some("parser".into()), objective: None, max_rounds: Some(5) }, Phase::Paused),
        (Op::Resume, Phase::Active),
        (Op::Block { reason: BlockReason { code: MODEL_REPORTED.into(), message: "needs a grammar decision".into() } }, Phase::Blocked),
        (Op::Resume, Phase::Active),
        (Op::Complete, Phase::Complete),
        (Op::Clear, Phase::Complete),
    ];
    for (i, (op, phase)) in steps.into_iter().enumerate() {
        log.extend(run(&mut gs, Some("goal-1"), op, &AGENT, 200 + i as u64).unwrap());
        let g = gs.get("goal-1").unwrap();
        assert_eq!(g.phase, phase);
        assert_eq!(g.revision, 2 + i as u32, "every op bumps revision by one");
        assert_eq!((g.created_at, g.rounds_started), (t(100), 1), "created_at and rounds_started survive every op");
    }
    let g = gs.get("goal-1").unwrap();
    assert!(g.cleared, "clear leaves a tombstone");
    assert_eq!((g.title.as_str(), g.max_rounds), ("parser", 5));
    assert!(gs.current().is_none());
    assert_eq!(replay(&log).unwrap(), gs, "the fold reproduces the engine's projection");
}

#[test]
fn illegal_transitions_are_refused_and_fail_the_fold() {
    let mut gs = Goals::default();
    let mut log = run(&mut gs, None, create("a"), &AGENT, 1).unwrap();
    log.extend(run(&mut gs, Some("goal-1"), Op::Pause, &AGENT, 2).unwrap());
    let paused = gs.get("goal-1").unwrap().clone();

    for (op, phase) in
        [(Op::Pause, Phase::Paused), (Op::Block { reason: BlockReason { code: "x".into(), message: "y".into() } }, Phase::Paused)]
    {
        let tag = op.tag();
        assert_eq!(gs.plan(Some("goal-1"), op.clone(), &AGENT, t(3)), Err(GoalError::Illegal { id: "goal-1".into(), op: tag, phase }));
        // a forged event claiming the same transition fails replay
        let mut forged = paused.clone();
        forged.revision += 1;
        forged.updated_at = t(3);
        forged.phase = if tag == "pause" { Phase::Paused } else { Phase::Blocked };
        let mut bad = log.clone();
        bad.push(changed(forged, op, AGENT));
        let err = replay(&bad).unwrap_err();
        assert!(err.contains("applies only to an active goal"), "{err}");
    }

    log.extend(run(&mut gs, Some("goal-1"), Op::Complete, &AGENT, 4).unwrap());
    assert!(matches!(gs.plan(Some("goal-1"), Op::Complete, &AGENT, t(5)), Err(GoalError::Illegal { .. })));
    assert!(matches!(gs.plan(Some("goal-1"), Op::Resume, &AGENT, t(5)), Err(GoalError::Illegal { .. })));
    assert!(matches!(
        gs.plan(Some("goal-1"), Op::Edit { title: Some("b".into()), objective: None, max_rounds: None }, &AGENT, t(5)),
        Err(GoalError::Illegal { op: "edit", .. })
    ));
    log.extend(run(&mut gs, Some("goal-1"), Op::Clear, &AGENT, 6).unwrap());
    assert_eq!(gs.plan(Some("goal-1"), Op::Clear, &AGENT, t(7)), Err(GoalError::Cleared("goal-1".into())));
    assert!(replay(&log).is_ok());
}

#[test]
fn tampered_snapshots_fail_the_fold() {
    let mut gs = Goals::default();
    let base = run(&mut gs, None, create("a"), &AGENT, 10).unwrap();
    let good = gs.plan(Some("goal-1"), Op::Pause, &AGENT, t(20)).unwrap();
    let Event::Changed { goal, .. } = &good[0] else { panic!() };

    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Box<dyn Fn(&mut Goal)>)> = vec![
        ("skipped revision", Box::new(|g| g.revision += 1)),
        ("rounds_started reset", Box::new(|g| g.rounds_started = 2)),
        ("created_at moved", Box::new(|g| g.created_at = t(11))),
        ("updated_at went backwards", Box::new(|g| g.updated_at = t(5))),
        ("objective changed by pause", Box::new(|g| g.objective = "b".into())),
    ];
    for (what, tamper) in cases {
        let mut g = goal.clone();
        tamper(&mut g);
        let mut log = base.clone();
        log.push(changed(g, Op::Pause, AGENT));
        assert!(replay(&log).is_err(), "{what} must fail replay");
    }
    let mut log = base.clone();
    log.extend(good.clone());
    assert!(replay(&log).is_ok());

    // an edit may not change phase
    let mut g = goal.clone();
    g.title = "new".into();
    let mut log = base.clone();
    log.push(changed(g, Op::Edit { title: Some("new".into()), objective: None, max_rounds: None }, AGENT));
    assert!(replay(&log).unwrap_err().contains("does not match"));

    // a reused id fails even after clear
    let mut log = base.clone();
    log.push(Event::Changed { op: create("again"), goal: apply(None, &create("again"), &AGENT, t(30), "goal-1").unwrap(), by: AGENT });
    assert!(replay(&log).unwrap_err().contains("never reused"));
}

#[test]
fn one_active_goal_and_user_goals_take_priority() {
    let mut gs = Goals::default();
    let mut log = run(&mut gs, None, create("agent work"), &AGENT, 1).unwrap();

    // an agent never displaces the active goal; the error names it
    let err = gs.plan(None, create("more agent work"), &OTHER, t(2)).unwrap_err();
    assert_eq!(err, GoalError::AnotherActive { active: "goal-1".into(), title: "agent work".into() });
    assert!(err.to_string().contains("`goal-1`"));

    // a user goal pauses the agent's goal first
    let ev = run(&mut gs, None, create("user work"), &Actor::Operator, 3).unwrap();
    assert_eq!(ev.len(), 2);
    assert!(matches!(&ev[0], Event::Changed { op: Op::Pause, goal, by: Actor::System(_) } if goal.id == "goal-1"));
    log.extend(ev);
    assert_eq!(gs.current().map(|g| g.id.as_str()), Some("goal-2"));
    assert_eq!(gs.get("goal-1").unwrap().phase, Phase::Paused);

    // the agent may not resume its goal over the user's; a second user goal is refused too
    assert!(matches!(gs.plan(Some("goal-1"), Op::Resume, &AGENT, t(4)), Err(GoalError::AnotherActive { .. })));
    assert!(matches!(gs.plan(None, create("user 2"), &Actor::Operator, t(4)), Err(GoalError::AnotherActive { .. })));

    // a forged second Active goal fails replay
    let mut bad = log.clone();
    let mut resumed = gs.get("goal-1").unwrap().clone();
    resumed.revision += 1;
    resumed.phase = Phase::Active;
    resumed.updated_at = t(5);
    bad.push(changed(resumed, Op::Resume, AGENT));
    assert!(replay(&bad).unwrap_err().contains("only one goal can be active"));
    assert_eq!(replay(&log).unwrap(), gs);
}

#[test]
fn an_agent_completes_only_what_it_drives() {
    let mut gs = Goals::default();
    let mut log = run(&mut gs, None, create("user goal"), &Actor::Operator, 1).unwrap();
    for op in [Op::Pause, Op::Complete, Op::Clear, Op::Edit { title: Some("x".into()), objective: None, max_rounds: None }] {
        let tag = op.tag();
        assert!(matches!(gs.plan(Some("goal-1"), op, &AGENT, t(2)), Err(GoalError::NotYours { op, .. }) if op == tag));
    }
    // a round queued for the agent makes it the driver: complete/block now allowed, pause still not
    let RoundPlan::Start { events, .. } = gs.next_round(AgentId(7), true, t(3)).unwrap() else { panic!() };
    gs = gs.stage(&events).unwrap();
    log.extend(events);
    assert!(matches!(gs.plan(Some("goal-1"), Op::Pause, &AGENT, t(4)), Err(GoalError::NotYours { .. })));
    assert!(gs.plan(Some("goal-1"), Op::Complete, &OTHER, t(4)).is_err(), "another agent does not drive it");
    log.extend(run(&mut gs, Some("goal-1"), Op::Complete, &AGENT, 5).unwrap());
    assert_eq!(gs.get("goal-1").unwrap().phase, Phase::Complete);
    assert_eq!(replay(&log).unwrap(), gs);
}

#[test]
fn round_driver_queues_rounds_then_blocks_at_the_limit() {
    let mut gs = Goals::default();
    let mut log = run(&mut gs, None, create("loop"), &AGENT, 1).unwrap();
    assert_eq!(gs.next_round(AgentId(7), false, t(2)).unwrap(), RoundPlan::Disarmed { id: "goal-1".into() });
    for n in 1..=3 {
        let RoundPlan::Start { round, revision, prompt, events, .. } = gs.next_round(AgentId(7), true, t(2)).unwrap() else { panic!() };
        assert_eq!((round, revision), (n, 1), "rounds do not bump the revision");
        assert!(prompt.contains(&format!("Goal round {n}/3")));
        gs = gs.stage(&events).unwrap();
        log.extend(events);
    }
    let RoundPlan::Blocked { events, .. } = gs.next_round(AgentId(7), true, t(9)).unwrap() else { panic!() };
    gs = gs.stage(&events).unwrap();
    log.extend(events);
    let g = gs.get("goal-1").unwrap();
    assert_eq!((g.phase, g.blocked_reason.as_ref().map(|b| b.code.as_str())), (Phase::Blocked, Some(ROUND_LIMIT)));
    assert_eq!(gs.plan(Some("goal-1"), Op::Resume, &AGENT, t(10)), Err(GoalError::RoundsExhausted { id: "goal-1".into(), max_rounds: 3 }));
    assert!(matches!(
        gs.plan(Some("goal-1"), Op::Edit { title: None, objective: None, max_rounds: Some(2) }, &AGENT, t(10)),
        Err(GoalError::BadRounds { .. })
    ));
    log.extend(run(&mut gs, Some("goal-1"), Op::Edit { title: None, objective: None, max_rounds: Some(4) }, &AGENT, 11).unwrap());
    log.extend(run(&mut gs, Some("goal-1"), Op::Resume, &AGENT, 12).unwrap());
    assert_eq!(gs.current().map(|g| g.rounds_started), Some(3));
    assert_eq!(gs.next_round(AgentId(7), true, t(13)).map(|p| matches!(p, RoundPlan::Start { round: 4, .. })), Ok(true));

    // a skipped round number fails replay
    let mut bad = log.clone();
    bad.push(Event::Round { id: "goal-1".into(), revision: gs.get("goal-1").unwrap().revision, round: 6, driver: AgentId(7) });
    assert!(replay(&bad).unwrap_err().contains("not the next admitted round"));
    assert_eq!(replay(&log).unwrap(), gs);
}

#[test]
fn cleared_ids_are_never_reused() {
    let mut gs = Goals::default();
    run(&mut gs, None, create("a"), &AGENT, 1).unwrap();
    run(&mut gs, Some("goal-1"), Op::Clear, &AGENT, 2).unwrap();
    run(&mut gs, None, create("b"), &AGENT, 3).unwrap();
    assert_eq!(gs.current().map(|g| g.id.as_str()), Some("goal-2"));
    assert_eq!(gs.all().len(), 2);
}

#[test]
fn a_bad_goal_event_fails_the_session_log_fold() {
    let log = MemoryLog::new();
    let mut folds = CustomFolds::new();
    folds.insert(NS.to_string(), fold_goals as aho_log::CustomFold);
    let by = Principal::Agent { id: AgentId(7), turn: TurnId(1) };
    let gs = Goals::default();
    for e in gs.plan(None, create("a"), &AGENT, t(1)).unwrap() {
        log.append(None, by.clone(), e.to_entry().unwrap()).unwrap();
    }
    let state = fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds).unwrap();
    assert_eq!(project(state.custom.get(NS)).unwrap().current().map(|g| g.revision), Some(1));

    // pausing twice: the second event is illegal
    let folded = project(state.custom.get(NS)).unwrap();
    let pause = folded.plan(Some("goal-1"), Op::Pause, &AGENT, t(2)).unwrap();
    log.append(None, by.clone(), pause[0].to_entry().unwrap()).unwrap();
    log.append(None, by, pause[0].to_entry().unwrap()).unwrap();
    let err = fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds).unwrap_err();
    assert!(matches!(err, LogError::Invariant { rule: "custom.fold", .. }), "{err:?}");
}
