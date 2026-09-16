use super::*;
use aho_cap::{Composition, Env, Os, PolicyInput, plan};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::{CapState, CustomFold, CustomFolds, EntryKind, LogError, LogStore, MemoryLog, fold};
use aho_types::{AgentId, ConsequenceSet, TurnId};

fn agent(n: u128) -> Principal {
    Principal::Agent { id: AgentId(n), turn: TurnId(1) }
}

fn todo(content: &str, status: TodoStatus) -> Todo {
    Todo { content: content.into(), status }
}

#[test]
fn parse_trims_and_enforces_the_invariants() {
    let ok = parse(&json!({"todos": [
        {"content": "  read the spec ", "status": "completed"},
        {"content": "write tests", "status": "In Progress"},
        {"content": "ship", "status": "pending"},
    ]}))
    .unwrap();
    assert_eq!(ok[0], todo("read the spec", TodoStatus::Completed));
    assert_eq!(ok[1].status, TodoStatus::InProgress);

    let bad = |v: Value| parse(&v).unwrap_err();
    assert_eq!(bad(json!({"todos": "a, b"})), TodoError::NotAList);
    assert_eq!(bad(json!({"todos": [{"content": "a"}]})), TodoError::BadItem(0));
    assert_eq!(bad(json!({"todos": [{"content": "   ", "status": "pending"}]})), TodoError::Empty(0));
    assert_eq!(bad(json!({"todos": [{"content": "a", "status": "doing"}]})), TodoError::BadStatus { index: 0, got: "doing".into() });
    assert_eq!(
        bad(json!({"todos": [{"content": "a", "status": "pending"}, {"content": " a", "status": "completed"}]})),
        TodoError::Duplicate("a".into()),
        "uniqueness is judged after trimming"
    );
    let two = bad(json!({"todos": [{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}]}));
    assert_eq!(two, TodoError::SeveralInProgress(vec!["a".into(), "b".into()]));
    assert!(two.to_string().contains("Keep exactly one item in progress"));
    assert_eq!(parse(&json!({"todos": []})).unwrap(), vec![], "an empty list clears it");
}

#[test]
fn summary_line_names_progress_and_the_active_item() {
    use TodoStatus::*;
    assert_eq!(summary(&[]), "Todo list cleared.");
    assert_eq!(summary(&[todo("a", Completed), todo("b", InProgress), todo("c", Pending)]), "1/3 done · in progress: b");
    assert_eq!(summary(&[todo("a", Completed), todo("c", Pending)]), "1/2 done · nothing in progress · next: c");
    assert_eq!(summary(&[todo("a", Completed)]), "1/1 done · all done");
}

#[test]
fn the_fold_replaces_per_agent_and_rejects_broken_snapshots() {
    let write = |prev: Option<&Value>, by: &str, todos: Vec<Todo>| {
        let e = Event::Written { by: by.into(), todos };
        fold_todos(prev, e.name(), &e.body().unwrap())
    };
    let s1 = write(None, "agent:1", vec![todo("a", TodoStatus::InProgress)]).unwrap();
    let s2 = write(Some(&s1), "agent:2", vec![todo("x", TodoStatus::Pending)]).unwrap();
    let s3 = write(Some(&s2), "agent:1", vec![todo("a", TodoStatus::Completed), todo("b", TodoStatus::InProgress)]).unwrap();
    let t: Todos = serde_json::from_value(s3.clone()).unwrap();
    assert_eq!(t.list("agent:1"), [todo("a", TodoStatus::Completed), todo("b", TodoStatus::InProgress)], "last write wins");
    assert_eq!(t.list("agent:2"), [todo("x", TodoStatus::Pending)], "lists are per agent");

    for (by, todos, why) in [
        ("agent:1", vec![todo(" a", TodoStatus::Pending)], "untrimmed"),
        ("agent:1", vec![todo("", TodoStatus::Pending)], "empty"),
        ("agent:1", vec![todo("a", TodoStatus::Pending), todo("a", TodoStatus::Completed)], "duplicate"),
        ("agent:1", vec![todo("a", TodoStatus::InProgress), todo("b", TodoStatus::InProgress)], "two in progress"),
        ("operator", vec![todo("a", TodoStatus::Pending)], "not an agent"),
    ] {
        assert!(write(Some(&s3), by, todos).is_err(), "{why} must fail the fold");
    }
    assert!(fold_todos(None, "Erased", &json!({})).is_err());
}

#[test]
fn the_tool_writes_a_snapshot_event_that_the_session_fold_accepts() {
    let comp = Composition::new("t").consume::<ToolTodo>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let loadout = resolve(&ResolveInputs::default(), &schema, false).unwrap();
    let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &loadout, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    assert!(matches!(p.row("tool-todo").unwrap().state, CapState::Active));
    let (reg, broken) = comp.start(&p, &loadout);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.tool("todo_write").is_some() && reg.entry_kinds.contains_key(NS));

    let args = json!({"todos": [{"content": "write tests", "status": "in_progress"}, {"content": "ship", "status": "pending"}]});
    let ExecOutcome::Done { value, content, .. } = write(&args, &agent(3)) else { panic!() };
    assert_eq!(content[0].as_text(), Some("0/2 done · in progress: write tests"));
    assert_eq!(value["events"][0]["body"]["by"], "agent:3");
    assert!(matches!(write(&args, &Principal::Operator), ExecOutcome::Err { code, .. } if code == "not_allowed"));
    assert!(
        matches!(write(&json!({"todos": [{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}]}), &agent(3)),
        ExecOutcome::Err { code, .. } if code == "bad_todos")
    );
    assert_eq!(present_call(&args), Card::Generic { title: "Todos".into(), body: "0/2 done · in progress: write tests".into() });

    // what the session does with `value.events`
    let log = MemoryLog::new();
    let mut folds = CustomFolds::new();
    folds.insert(NS.into(), fold_todos as CustomFold);
    let e = &value["events"][0];
    log.append(
        None,
        agent(3),
        EntryKind::Custom { ns: e["ns"].as_str().unwrap().into(), name: e["name"].as_str().unwrap().into(), body: e["body"].clone() },
    )
    .unwrap();
    let st = fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds).unwrap();
    let t: Todos = serde_json::from_value(st.custom[NS].clone()).unwrap();
    assert_eq!(t.list("agent:3").len(), 2);
    // a hand-written snapshot with two items in progress fails replay loudly
    let bad = json!({"by": "agent:3", "todos": [{"content": "a", "status": "in_progress"}, {"content": "b", "status": "in_progress"}]});
    log.append(None, agent(3), EntryKind::Custom { ns: NS.into(), name: "Written".into(), body: bad }).unwrap();
    assert!(matches!(fold(&log.read_branch(log.head().unwrap()).unwrap(), &folds), Err(LogError::Invariant { rule: "custom.fold", .. })));
}
