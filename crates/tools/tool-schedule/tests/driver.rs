//! The Scheduler over a real log, a stepped clock, and a channel sink whose other end plays the
//! session: it receives each fired prompt and answers when "the turn" ends.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use aho_control::{ChannelSink, Clock, SinkRequest, SteppedClock, TurnEnd, branch};
use aho_goals::Actor;
use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_types::{AgentId, Principal, Timestamp};
use tokio::sync::{mpsc, oneshot};
use tool_schedule::spec::MINUTE;
use tool_schedule::{Event, Limits, NS, Scheduler, SchedulerLink, Schedules, Spec, parse_rfc3339};

const LIMITS: Limits = Limits { max_active: 20, min_interval_ms: MINUTE };

fn t(s: &str) -> Timestamp {
    parse_rfc3339(s).unwrap()
}

struct Rig {
    log: Arc<MemoryLog>,
    clock: Arc<SteppedClock>,
    link: SchedulerLink,
    prompts: mpsc::Receiver<SinkRequest>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

fn create(log: &MemoryLog, prompt: &str, spec: Spec, at: Timestamp) -> String {
    let s = tool_schedule::load(&branch(log).unwrap()).unwrap();
    let e = s.create(prompt, "", spec, &Actor::Agent(AgentId(0)), at, LIMITS).unwrap();
    log.append(None, Principal::system("schedule_create"), e.to_entry().unwrap()).unwrap();
    match e {
        Event::Created { schedule } => schedule.id,
        _ => unreachable!(),
    }
}

fn start(log: Arc<MemoryLog>, now: Timestamp) -> Rig {
    let clock = Arc::new(SteppedClock::new(now));
    let (sink, prompts) = ChannelSink::new(4);
    let link = SchedulerLink::new();
    let sched = Scheduler::new(log.clone(), Arc::new(sink), clock.clone(), link.clone());
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        sched
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    Rig { log, clock, link, prompts, stop: Some(stop), task }
}

impl Rig {
    async fn next_prompt(&mut self) -> SinkRequest {
        tokio::time::timeout(Duration::from_secs(5), self.prompts.recv()).await.expect("a prompt fires").expect("the sink is open")
    }
    async fn quiet(&mut self) {
        let got = tokio::time::timeout(Duration::from_millis(150), self.prompts.recv()).await;
        assert!(got.is_err(), "no prompt expected, got {:?}", got.ok().flatten().map(|r| r.key));
    }
    fn fired(&self) -> Vec<(String, u64, u64)> {
        branch(&*self.log)
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Custom { ns, name, body } if ns == NS && name == "Fired" => {
                    assert_eq!(e.by, Principal::system("schedule"));
                    Some((body["id"].as_str().unwrap().to_string(), body["n"].as_u64().unwrap(), body["skipped"].as_u64().unwrap()))
                }
                _ => None,
            })
            .collect()
    }
    async fn finish(mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        tokio::time::timeout(Duration::from_secs(5), self.task).await.unwrap().unwrap().unwrap();
    }
    fn schedules(&self) -> Schedules {
        tool_schedule::load(&branch(&*self.log).unwrap()).unwrap()
    }
}

fn answer(req: SinkRequest, text: &str) {
    req.reply.send(TurnEnd { final_text: text.into(), failed: None, ran: true }).unwrap();
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(30)).await;
}

#[tokio::test]
async fn fires_each_occurrence_when_the_clock_reaches_it_and_records_it_after_the_turn() {
    let log = Arc::new(MemoryLog::new());
    let t0 = t("2026-09-26T10:00:00Z");
    create(&log, "check the build", Spec::Every { every_ms: MINUTE, anchor: t0 }, t0);
    let mut r = start(log, t0);
    r.quiet().await;
    r.clock.set(t("2026-09-26T10:00:59Z"));
    r.quiet().await;
    r.clock.set(t("2026-09-26T10:01:00Z"));
    let p = r.next_prompt().await;
    assert_eq!((p.by, p.key.as_str()), ("schedule", "schedule:sched-1:1"));
    assert!(p.text.ends_with("check the build") && !p.text.contains("late"), "{}", p.text);
    settle().await;
    assert!(r.fired().is_empty(), "the record follows the turn, not the prompt");
    answer(p, "green");
    settle().await;
    assert_eq!(r.fired(), vec![("sched-1".to_string(), 1, 0)]);

    r.clock.set(t("2026-09-26T10:02:00Z"));
    let p = r.next_prompt().await;
    assert_eq!(p.key, "schedule:sched-1:2");
    answer(p, "green");
    settle().await;
    assert_eq!(r.fired().len(), 2);
    r.finish().await;
}

#[tokio::test]
async fn resuming_after_downtime_fires_each_schedule_once_with_the_missed_count() {
    let log = Arc::new(MemoryLog::new());
    let t0 = t("2026-09-26T10:00:00Z");
    create(&log, "every minute", Spec::Every { every_ms: MINUTE, anchor: t0 }, t0);
    create(&log, "at noon", Spec::At { at: t("2026-09-26T12:00:00Z") }, t0);
    create(&log, "quarter hours", Spec::Cron { expr: "*/15 * * * *".into() }, t0);
    // the host was down until 12:20:30
    let mut r = start(log, t("2026-09-26T12:20:30Z"));
    let mut keys = Vec::new();
    for _ in 0..3 {
        let p = r.next_prompt().await;
        assert!(p.text.contains("fired late"), "{}", p.text);
        keys.push(p.key.clone());
        answer(p, "ok");
    }
    r.quiet().await;
    assert_eq!(keys, vec!["schedule:sched-1:1", "schedule:sched-3:1", "schedule:sched-2:1"], "by due time: 10:01, 10:15, 12:00");
    settle().await;
    let mut fired = r.fired();
    fired.sort();
    // 10:01…12:20 is 140 minutes; 10:15…12:15 is 9 quarter hours; the one-shot misses nothing
    assert_eq!(fired, vec![("sched-1".to_string(), 1, 139), ("sched-2".to_string(), 1, 0), ("sched-3".to_string(), 1, 8)]);
    let s = r.schedules();
    assert_eq!(s.get("sched-1").unwrap().next_due, Some(t("2026-09-26T12:21:00Z")));
    assert_eq!(s.get("sched-3").unwrap().next_due, Some(t("2026-09-26T12:30:00Z")));
    assert_eq!(s.get("sched-2").unwrap().status, tool_schedule::Status::Done);
    r.finish().await;
}

#[tokio::test]
async fn a_schedule_created_while_the_driver_sleeps_is_picked_up_on_wake() {
    let log = Arc::new(MemoryLog::new());
    let t0 = t("2026-09-26T10:00:00Z");
    let mut r = start(log.clone(), t0);
    r.quiet().await;
    create(&log, "later", Spec::At { at: t("2026-09-26T10:30:00Z") }, t0);
    r.link.wake();
    settle().await;
    r.clock.set(t("2026-09-26T10:30:00Z"));
    let p = r.next_prompt().await;
    assert_eq!(p.key, "schedule:sched-1:1");
    answer(p, "done");
    r.finish().await;
}

#[tokio::test]
async fn a_cancelled_schedule_does_not_fire_and_cancel_during_its_turn_is_not_overwritten() {
    let log = Arc::new(MemoryLog::new());
    let t0 = t("2026-09-26T10:00:00Z");
    create(&log, "a", Spec::Every { every_ms: 5 * MINUTE, anchor: t0 }, t0);
    create(&log, "b", Spec::Every { every_ms: 5 * MINUTE, anchor: t0 }, t0);
    let cancel = |id: &str| {
        let s = tool_schedule::load(&branch(&*log).unwrap()).unwrap();
        let e = s.cancel(id, &Actor::Operator, t0).unwrap();
        log.append(None, Principal::Operator, e.to_entry().unwrap()).unwrap();
    };
    cancel("sched-2");
    let mut r = start(log.clone(), t0);
    r.clock.set(t("2026-09-26T10:05:00Z"));
    let p = r.next_prompt().await;
    assert_eq!(p.key, "schedule:sched-1:1");
    cancel("sched-1"); // the operator cancels while the fired turn runs
    answer(p, "ok");
    settle().await;
    assert!(r.fired().is_empty(), "no Fired record lands after the Cancelled one");
    r.clock.set(t("2026-09-26T11:00:00Z"));
    r.quiet().await;
    r.finish().await;
}

#[tokio::test]
async fn a_restart_between_prompt_and_record_resends_the_same_key() {
    let log = Arc::new(MemoryLog::new());
    let t0 = t("2026-09-26T10:00:00Z");
    create(&log, "once", Spec::At { at: t("2026-09-26T10:10:00Z") }, t0);
    let mut r = start(log.clone(), t("2026-09-26T10:10:00Z"));
    let p = r.next_prompt().await;
    let first_key = p.key.clone();
    drop(p); // the process died mid-turn: no answer, no record
    let err = tokio::time::timeout(Duration::from_secs(5), r.task).await.unwrap().unwrap().unwrap_err();
    assert!(err.contains("nothing is running turns"), "{err}");
    assert!(tool_schedule::load(&branch(&*log).unwrap()).unwrap().get("sched-1").unwrap().fires == 0);

    let mut again = start(log, t("2026-09-26T10:12:00Z"));
    let p = again.next_prompt().await;
    assert_eq!(p.key, first_key, "same occurrence, same key: the session dedupes it");
    answer(p, "ok");
    settle().await;
    assert_eq!(again.fired(), vec![("sched-1".to_string(), 1, 0)]);
    again.finish().await;
}

#[tokio::test]
async fn stop_ends_the_driver_even_while_a_fired_turn_is_running() {
    let log = Arc::new(MemoryLog::new());
    let t0 = t("2026-09-26T10:00:00Z");
    create(&log, "long", Spec::At { at: t("2026-09-26T10:01:00Z") }, t0);
    let mut r = start(log, t("2026-09-26T10:01:00Z"));
    let p = r.next_prompt().await;
    let fired_clock = r.clock.now();
    r.finish().await;
    drop(p);
    assert_eq!(fired_clock, t("2026-09-26T10:01:00Z"));
}
