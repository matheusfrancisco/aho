#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use aho_log::MemoryLog;

use super::*;

fn user(log: &MemoryLog, text: &str, key: Option<&str>) {
    log.append(
        None,
        Principal::Operator,
        EntryKind::UserMessage { form: Form::Prompt, text: text.into(), client_key: key.map(str::to_string) },
    )
    .unwrap();
}

fn assistant(log: &MemoryLog, blocks: Vec<ContentBlock>) {
    log.append(
        None,
        Principal::system("model"),
        EntryKind::AssistantMessage { content: blocks, usage: Usage::default(), stop: StopReason::EndTurn },
    )
    .unwrap();
}

fn close(log: &MemoryLog, turn: u64, transition: Transition) {
    log.append(None, Principal::system("loop"), EntryKind::StepEnd { step: StepId(0), transition }).unwrap();
    log.append(None, Principal::system("loop"), EntryKind::TurnEnd { turn: TurnId(turn) }).unwrap();
}

fn open(log: &MemoryLog, turn: u64) {
    log.append(None, Principal::system("loop"), EntryKind::TurnStart { turn: TurnId(turn) }).unwrap();
}

#[test]
fn turn_report_reads_only_the_keyed_turn_and_keeps_the_last_non_empty_text() {
    let log = MemoryLog::new();
    open(&log, 1);
    user(&log, "first", Some("k1"));
    assistant(&log, vec![ContentBlock::text("from turn one")]);
    close(&log, 1, Transition::Completed);
    open(&log, 2);
    user(&log, "second", Some("k2"));
    assistant(
        &log,
        vec![ContentBlock::text("working"), ContentBlock::ToolUse { id: CallId("c".into()), name: "read".into(), args: Value::Null }],
    );
    assistant(&log, vec![ContentBlock::text("   ")]);
    close(&log, 2, Transition::Aborted("model call failed: 500".into()));
    open(&log, 3);
    assistant(&log, vec![ContentBlock::text("a later turn")]);

    let es = branch(&log).unwrap();
    let one = turn_report(&es, "k1").unwrap();
    assert_eq!((one.final_text.as_str(), one.failed.as_deref()), ("from turn one", None));
    let two = turn_report(&es, "k2").unwrap();
    assert_eq!(two.final_text, "working", "whitespace-only text does not replace the last real text; the next turn is not read");
    assert_eq!(two.failed.as_deref(), Some("model call failed: 500"));
    assert!(turn_report(&es, "k3").is_none());
    assert!(admitted(&es, "k2") && !admitted(&es, "k3"));
}

#[test]
fn record_appends_a_system_attributed_custom_entry() {
    let log = MemoryLog::new();
    let id = record(&log, "schedule", "schedule", "Fired", serde_json::json!({"id": "sched-1"})).unwrap();
    let e = log.get(id).unwrap();
    assert_eq!(e.by, Principal::system("schedule"));
    assert!(matches!(&e.kind, EntryKind::Custom { ns, name, .. } if ns == "schedule" && name == "Fired"));
}

#[tokio::test]
async fn channel_sink_delivers_the_prompt_and_returns_the_answer() {
    let (sink, mut rx) = ChannelSink::new(1);
    let runner = tokio::spawn(async move {
        let req = rx.recv().await.unwrap();
        assert_eq!((req.by, req.text.as_str(), req.key.as_str()), ("schedule", "do it", "k"));
        req.reply.send(TurnEnd { final_text: "done".into(), failed: None, ran: true }).unwrap();
    });
    let end = sink.prompt("schedule", "do it".into(), "k".into()).await.unwrap();
    assert_eq!(end.final_text, "done");
    runner.await.unwrap();
}

#[tokio::test]
async fn channel_sink_reports_a_dropped_runner_as_closed() {
    let (sink, mut rx) = ChannelSink::new(1);
    tokio::spawn(async move {
        let req = rx.recv().await.unwrap();
        drop(req.reply);
    });
    assert!(matches!(sink.prompt("schedule", "x".into(), "k".into()).await, Err(DriveError::Closed(_))));
    let (sink, rx) = ChannelSink::new(1);
    drop(rx);
    assert!(matches!(sink.prompt("schedule", "x".into(), "k".into()).await, Err(DriveError::Closed(_))));
}

#[tokio::test]
async fn stepped_clock_wakes_sleepers_only_when_time_reaches_them() {
    let clock = Arc::new(SteppedClock::new(Timestamp(1_000)));
    let c = clock.clone();
    let sleeper = tokio::spawn(async move {
        c.sleep_until(Timestamp(5_000)).await;
        c.now()
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    clock.advance(1_000);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!sleeper.is_finished(), "2 000 < 5 000: still asleep");
    clock.set(Timestamp(500));
    assert_eq!(clock.now(), Timestamp(2_000), "time never goes backwards");
    clock.set(Timestamp(7_000));
    assert_eq!(tokio::time::timeout(Duration::from_secs(5), sleeper).await.unwrap().unwrap(), Timestamp(7_000));
    // a deadline in the past resolves at once
    tokio::time::timeout(Duration::from_secs(1), clock.sleep_until(Timestamp(10))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), SystemClock.sleep_until(Timestamp(10))).await.unwrap();
}
