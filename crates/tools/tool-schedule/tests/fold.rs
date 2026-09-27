//! The schedule set is a pure fold over `schedule/*` entries; every recomputable field is checked.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use aho_goals::Actor;
use aho_log::{CustomFolds, EntryKind, LogStore, MemoryLog, fold};
use aho_types::{AgentId, Principal, Timestamp};
use tool_schedule::spec::MINUTE;
use tool_schedule::{Event, Limits, NS, ScheduleError, Schedules, Spec, Status, fold_schedules, parse_rfc3339, project};

const LIMITS: Limits = Limits { max_active: 3, min_interval_ms: MINUTE };
const AGENT: Actor = Actor::Agent(AgentId(0));

fn t(s: &str) -> Timestamp {
    parse_rfc3339(s).unwrap()
}

fn t0() -> Timestamp {
    t("2026-09-26T10:00:00Z")
}

fn every(min: u64) -> Spec {
    Spec::Every { every_ms: min * MINUTE, anchor: t0() }
}

fn apply(s: &mut Schedules, e: &Event) {
    *s = s.stage(std::slice::from_ref(e)).unwrap();
}

#[test]
fn created_schedules_get_ids_and_their_first_occurrence() {
    let mut s = Schedules::default();
    let e = s.create("  check the build  ", "ci", every(30), &AGENT, t0(), LIMITS).unwrap();
    apply(&mut s, &e);
    let e2 = s.create("standup", "", Spec::Cron { expr: "0 9 * * 1-5".into() }, &Actor::Operator, t0(), LIMITS).unwrap();
    apply(&mut s, &e2);
    let a = s.get("sched-1").unwrap();
    assert_eq!((a.prompt.as_str(), a.label.as_str(), a.status), ("check the build", "ci", Status::Active));
    assert_eq!(a.next_due, Some(t("2026-09-26T10:30:00Z")));
    assert_eq!(s.get("sched-2").unwrap().next_due, Some(t("2026-09-28T09:00:00Z")), "2026-09-26 is a Saturday");
    assert_eq!(s.next_wake(), Some(t("2026-09-26T10:30:00Z")));
    // the projection round-trips through the CustomFold
    let mut v = None;
    for e in [&e, &e2] {
        v = Some(fold_schedules(v.as_ref(), e.name(), &e.body().unwrap()).unwrap());
    }
    assert_eq!(project(v.as_ref()).unwrap(), s);
}

#[test]
fn creation_refuses_what_would_never_fire_or_break_the_limits() {
    let s = Schedules::default();
    let err = |spec: Spec| s.create("x", "", spec, &AGENT, t0(), LIMITS).unwrap_err();
    assert!(matches!(s.create(" ", "", every(5), &AGENT, t0(), LIMITS), Err(ScheduleError::EmptyPrompt)));
    assert!(matches!(err(Spec::At { at: t("2026-09-26T09:00:00Z") }), ScheduleError::InPast { .. }));
    assert!(matches!(err(Spec::At { at: t0() }), ScheduleError::InPast { .. }), "now is not in the future");
    assert!(matches!(err(Spec::Cron { expr: "0 0 30 2 *".into() }), ScheduleError::NeverFires(_)));
    let fast = err(Spec::Every { every_ms: 30_000, anchor: t0() });
    assert!(matches!(fast, ScheduleError::TooFrequent { gap_ms: 30_000, min_ms: 60_000 }));
    assert!(fast.to_string().contains("schedules.min_interval_s"), "{fast}");

    let mut full = Schedules::default();
    for _ in 0..3 {
        let e = full.create("x", "", every(5), &AGENT, t0(), LIMITS).unwrap();
        apply(&mut full, &e);
    }
    assert!(matches!(full.create("x", "", every(5), &AGENT, t0(), LIMITS), Err(ScheduleError::TooMany { max: 3 })));
    let c = full.cancel("sched-2", &AGENT, t0()).unwrap();
    apply(&mut full, &c);
    let e = full.create("x", "", every(5), &AGENT, t0(), LIMITS).unwrap();
    assert!(
        matches!(&e, Event::Created { schedule } if schedule.id == "sched-4"),
        "a cancelled schedule frees a slot; ids are never reused"
    );
}

#[test]
fn a_late_driver_fires_once_and_counts_what_it_missed() {
    let mut s = Schedules::default();
    let e = s.create("check the build", "", every(1), &AGENT, t0(), LIMITS).unwrap();
    apply(&mut s, &e);
    assert_eq!(s.next_due(Timestamp(t0().0 + 59_999)).unwrap(), None, "nothing is due before the first occurrence");

    // down from 10:00 to 10:05:30: occurrences 10:01 … 10:05 are due
    let now = t("2026-09-26T10:05:30Z");
    let due = s.next_due(now).unwrap().unwrap();
    assert_eq!((due.id.as_str(), due.n, due.due, due.skipped), ("sched-1", 1, t("2026-09-26T10:01:00Z"), 4));
    assert_eq!(due.key, "schedule:sched-1:1");
    assert!(due.text.ends_with("\n\ncheck the build"), "{}", due.text);
    assert!(due.text.contains("fired late, at 2026-09-26T10:05:30Z") && due.text.contains("4 later occurrence(s)"), "{}", due.text);
    apply(&mut s, &due.event);
    let x = s.get("sched-1").unwrap();
    assert_eq!((x.fires, x.skipped, x.last_fired_at), (1, 4, Some(now)));
    assert_eq!(x.next_due, Some(t("2026-09-26T10:06:00Z")), "the next occurrence is the first after the fire");
    assert_eq!(s.next_due(now).unwrap(), None, "fired once, not five times");

    // on time: no late note, nothing skipped
    let on_time = s.next_due(t("2026-09-26T10:06:00Z")).unwrap().unwrap();
    assert_eq!((on_time.n, on_time.skipped), (2, 0));
    assert!(!on_time.text.contains("late") && !on_time.text.contains("missed"), "{}", on_time.text);
}

#[test]
fn a_one_shot_is_done_after_it_fires_even_when_late() {
    let mut s = Schedules::default();
    let e = s.create("ship it", "", Spec::At { at: t("2026-09-26T12:00:00Z") }, &AGENT, t0(), LIMITS).unwrap();
    apply(&mut s, &e);
    let due = s.next_due(t("2026-09-27T08:00:00Z")).unwrap().unwrap();
    assert_eq!((due.n, due.skipped), (1, 0));
    apply(&mut s, &due.event);
    assert_eq!(s.get("sched-1").unwrap().status, Status::Done);
    assert_eq!((s.next_wake(), s.next_due(t("2027-01-01T00:00:00Z")).unwrap()), (None, None));
}

#[test]
fn the_fold_rejects_records_its_spec_does_not_produce() {
    let mut s = Schedules::default();
    let e = s.create("x", "", every(1), &AGENT, t0(), LIMITS).unwrap();
    apply(&mut s, &e);
    let fired = |n: u64, due: &str, at: &str, skipped: u64| Event::Fired { id: "sched-1".into(), n, due: t(due), at: t(at), skipped };
    assert!(s.stage(&[fired(1, "2026-09-26T10:01:00Z", "2026-09-26T10:03:00Z", 2)]).is_ok());
    for bad in [
        fired(2, "2026-09-26T10:01:00Z", "2026-09-26T10:03:00Z", 2),
        fired(1, "2026-09-26T10:02:00Z", "2026-09-26T10:03:00Z", 1),
        fired(1, "2026-09-26T10:01:00Z", "2026-09-26T10:03:00Z", 0),
        fired(1, "2026-09-26T10:01:00Z", "2026-09-26T10:00:30Z", 0),
    ] {
        assert!(matches!(s.stage(std::slice::from_ref(&bad)), Err(ScheduleError::Tampered { .. })), "{bad:?}");
    }
    let Event::Created { mut schedule } = e.clone() else { panic!() };
    schedule.next_due = Some(t("2026-09-26T10:00:30Z"));
    assert!(matches!(Schedules::default().stage(&[Event::Created { schedule }]), Err(ScheduleError::Tampered { .. })));
    assert!(matches!(s.stage(std::slice::from_ref(&e)), Err(ScheduleError::BadEvent(_))), "an id is created once");

    let c = s.cancel("sched-1", &AGENT, t0()).unwrap();
    apply(&mut s, &c);
    assert!(matches!(
        s.stage(&[fired(1, "2026-09-26T10:01:00Z", "2026-09-26T10:01:00Z", 0)]),
        Err(ScheduleError::NotActive { status: Status::Cancelled, .. })
    ));
}

#[test]
fn agents_cancel_only_what_agents_created() {
    let mut s = Schedules::default();
    let mine = s.create("a", "", every(5), &AGENT, t0(), LIMITS).unwrap();
    apply(&mut s, &mine);
    let theirs = s.create("b", "", every(5), &Actor::Operator, t0(), LIMITS).unwrap();
    apply(&mut s, &theirs);
    let e = s.cancel("sched-2", &AGENT, t0()).unwrap_err();
    assert!(matches!(&e, ScheduleError::NotYours { owner: Actor::Operator, .. }), "{e}");
    assert!(e.to_string().contains("keeps running"));
    let c = s.cancel("sched-1", &Actor::Operator, t0()).unwrap();
    apply(&mut s, &c);
    assert_eq!(s.get("sched-1").unwrap().cancelled_by, Some(Actor::Operator));
    assert!(matches!(s.cancel("sched-1", &Actor::Operator, t0()), Err(ScheduleError::NotActive { .. })));
    assert!(matches!(s.cancel("sched-9", &Actor::Operator, t0()), Err(ScheduleError::Unknown(_))));
}

#[test]
fn the_earliest_due_schedule_fires_first() {
    let mut s = Schedules::default();
    for spec in [every(10), every(3), Spec::At { at: t("2026-09-26T10:03:00Z") }] {
        let e = s.create("x", "", spec, &AGENT, t0(), LIMITS).unwrap();
        apply(&mut s, &e);
    }
    let now = t("2026-09-26T10:20:00Z");
    let mut order = Vec::new();
    while let Some(d) = s.next_due(now).unwrap() {
        order.push((d.id.clone(), d.due));
        apply(&mut s, &d.event);
    }
    assert_eq!(
        order,
        vec![
            ("sched-2".to_string(), t("2026-09-26T10:03:00Z")),
            ("sched-3".to_string(), t("2026-09-26T10:03:00Z")),
            ("sched-1".to_string(), t("2026-09-26T10:10:00Z")),
        ],
        "by due time, then id; each once"
    );
}

#[test]
fn a_session_log_with_a_tampered_fire_does_not_replay() {
    let log = MemoryLog::new();
    let mut s = Schedules::default();
    let e = s.create("x", "", every(1), &AGENT, t0(), LIMITS).unwrap();
    apply(&mut s, &e);
    log.append(None, Principal::system("schedule_create"), e.to_entry().unwrap()).unwrap();
    let due = s.next_due(t("2026-09-26T10:02:00Z")).unwrap().unwrap();
    log.append(None, Principal::system("schedule"), due.event.to_entry().unwrap()).unwrap();
    let folds: CustomFolds = [(NS.to_string(), fold_schedules as aho_log::CustomFold)].into_iter().collect();
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let state = fold(&entries, &folds).unwrap();
    assert_eq!(project(state.custom.get(NS)).unwrap().get("sched-1").unwrap().fires, 1);
    assert_eq!(tool_schedule::load(&entries).unwrap(), project(state.custom.get(NS)).unwrap());

    log.append(
        None,
        Principal::system("schedule"),
        EntryKind::Custom {
            ns: NS.into(),
            name: "Fired".into(),
            body: serde_json::json!({"id": "sched-1", "n": 5, "due": 0, "at": 0, "skipped": 0}),
        },
    )
    .unwrap();
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let err = fold(&entries, &folds).unwrap_err().to_string();
    assert!(err.contains("schedule/Fired"), "{err}");
    assert!(tool_schedule::load(&entries).is_err());
}
