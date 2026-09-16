//! The Looper over a real log and a channel sink whose other end plays the session.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use aho_control::{ChannelSink, SinkRequest, SteppedClock, TurnEnd, branch};
use aho_goals::Actor;
use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_types::{AgentId, Principal, Timestamp};
use tokio::sync::{mpsc, oneshot};
use tool_workflow::{LoopLink, Looper, NS, Phase, Workflows};

struct Rig {
    log: Arc<MemoryLog>,
    link: LoopLink,
    prompts: mpsc::Receiver<SinkRequest>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

fn loops(log: &MemoryLog) -> Workflows {
    tool_workflow::load(&branch(log).unwrap()).unwrap()
}

fn start_loop(log: &MemoryLog, by: Actor, max: u32) {
    let e = loops(log).start("make the tests pass", "DONE", max, &by, Timestamp(1)).unwrap();
    log.append(None, Principal::system("ralph_start"), e.to_entry().unwrap()).unwrap();
}

fn start(log: Arc<MemoryLog>) -> Rig {
    let (sink, prompts) = ChannelSink::new(4);
    let link = LoopLink::new();
    let looper = Looper::new(log.clone(), Arc::new(sink), Arc::new(SteppedClock::new(Timestamp(100))), link.clone());
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        looper
            .run(async {
                let _ = stopped.await;
            })
            .await
    });
    Rig { log, link, prompts, stop: Some(stop), task }
}

impl Rig {
    async fn next_prompt(&mut self) -> SinkRequest {
        tokio::time::timeout(Duration::from_secs(5), self.prompts.recv()).await.expect("a prompt is fed").expect("sink open")
    }
    async fn quiet(&mut self) {
        let got = tokio::time::timeout(Duration::from_millis(150), self.prompts.recv()).await;
        assert!(got.is_err(), "no prompt expected, got {:?}", got.ok().flatten().map(|r| r.key));
    }
    fn records(&self) -> Vec<String> {
        branch(&*self.log)
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Custom { ns, name, body } if ns == NS && name != "Started" => {
                    if name == "Iteration" || body["by"] == "system:workflow" {
                        assert_eq!(e.by, Principal::system("workflow"), "driver records are the driver's");
                    }
                    Some(match name.as_str() {
                        "Iteration" => format!("Iteration {} found={}", body["n"], body["promise_found"]),
                        _ => format!("Finished {}", body["outcome"].as_str().unwrap()),
                    })
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
}

fn answer(req: SinkRequest, text: &str) {
    req.reply.send(TurnEnd { final_text: text.into(), failed: None, ran: true }).unwrap();
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(30)).await;
}

#[tokio::test]
async fn the_loop_refeeds_after_each_turn_until_the_promise() {
    let log = Arc::new(MemoryLog::new());
    let mut r = start(log.clone());
    r.quiet().await;
    start_loop(&log, Actor::Agent(AgentId(0)), 5);
    r.link.wake(); // the turn that called ralph_start ended
    let p = r.next_prompt().await;
    assert_eq!((p.by, p.key.as_str()), ("workflow", "workflow:ralph-1:1"));
    assert!(p.text.starts_with("make the tests pass\n\n[Ralph loop ralph-1: iteration 1 of 5."));
    settle().await;
    assert!(r.records().is_empty(), "an iteration is recorded after its turn");
    answer(p, "fixed one; <promise>NOT YET</promise>");
    let p = r.next_prompt().await;
    assert_eq!(p.key, "workflow:ralph-1:2");
    answer(p, "all green\n<promise>DONE</promise>");
    r.quiet().await;
    assert_eq!(r.records(), vec!["Iteration 1 found=false", "Iteration 2 found=true", "Finished completed"]);
    r.link.wake(); // later turn ends do nothing
    r.quiet().await;
    r.finish().await;
}

#[tokio::test]
async fn the_loop_stops_at_its_budget() {
    let log = Arc::new(MemoryLog::new());
    start_loop(&log, Actor::Operator, 2);
    let mut r = start(log.clone()); // a loop already running when the driver starts (resume) is fed at once
    for n in 1..=2 {
        let p = r.next_prompt().await;
        assert_eq!(p.key, format!("workflow:ralph-1:{n}"));
        answer(p, "working on it");
    }
    r.quiet().await;
    assert_eq!(r.records(), vec!["Iteration 1 found=false", "Iteration 2 found=false", "Finished exhausted"]);
    assert_eq!(loops(&log).get("ralph-1").unwrap().phase, Phase::Exhausted);
    r.finish().await;
}

#[tokio::test]
async fn a_loop_stopped_during_its_iteration_is_not_fed_again() {
    let log = Arc::new(MemoryLog::new());
    start_loop(&log, Actor::Agent(AgentId(0)), 5);
    let mut r = start(log.clone());
    let p = r.next_prompt().await;
    // ralph_stop inside the fed turn
    let e = loops(&log).stop(None, &Actor::Agent(AgentId(0)), Timestamp(50), Some("done differently".into())).unwrap();
    log.append(None, Principal::system("ralph_stop"), e.to_entry().unwrap()).unwrap();
    answer(p, "stopping");
    r.quiet().await;
    assert_eq!(r.records(), vec!["Finished stopped"], "no Iteration record lands after the stop");
    r.finish().await;
}

#[tokio::test]
async fn a_failing_turn_fails_the_loop_instead_of_spending_the_budget() {
    let log = Arc::new(MemoryLog::new());
    start_loop(&log, Actor::Agent(AgentId(0)), 10);
    let mut r = start(log.clone());
    let p = r.next_prompt().await;
    p.reply.send(TurnEnd { final_text: String::new(), failed: Some("model call failed: 401".into()), ran: true }).unwrap();
    r.quiet().await;
    assert_eq!(r.records(), vec!["Iteration 1 found=false", "Finished failed"]);
    let w = loops(&log);
    assert!(w.get("ralph-1").unwrap().reason.as_deref().unwrap().contains("401"));
    assert!(matches!(w.all()[0].phase, Phase::Failed));
    r.finish().await;
}

#[tokio::test]
async fn a_restart_mid_iteration_resends_the_same_key() {
    let log = Arc::new(MemoryLog::new());
    start_loop(&log, Actor::Agent(AgentId(0)), 3);
    let mut r = start(log.clone());
    let p = r.next_prompt().await;
    drop(p);
    let err = tokio::time::timeout(Duration::from_secs(5), r.task).await.unwrap().unwrap().unwrap_err();
    assert!(err.contains("nothing is running turns"), "{err}");
    let mut again = start(log);
    let p = again.next_prompt().await;
    assert_eq!(p.key, "workflow:ralph-1:1", "the unrecorded iteration is re-sent under its key; the session dedupes it");
    answer(p, "<promise>DONE</promise>");
    again.quiet().await;
    assert_eq!(again.records(), vec!["Iteration 1 found=true", "Finished completed"]);
    again.finish().await;
}
