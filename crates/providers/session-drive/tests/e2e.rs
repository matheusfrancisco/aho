//! End to end: a real Session (scripted model, JSONL log) with tool-schedule and tool-workflow,
//! driven by `host()`: the agent schedules / starts a loop through its tools, the drivers fire
//! the prompts as real turns, and the log replays.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_control::{PromptSink, SteppedClock};
use aho_llm::{LateResults, LlmRoute};
use aho_log::{Entry, EntryKind, JsonlLog, LogStore, Transition};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, Step};
use serde_json::json;
use session_drive::{DriverSeed, SessionSink, host};
use tool_schedule::ToolSchedule;
use tool_workflow::ToolWorkflow;

struct Rig {
    session: Arc<Session>,
    llm: Arc<ScriptedLlm>,
    seed: DriverSeed,
    _dir: tempfile::TempDir,
}

async fn boot(steps: Vec<Step>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ws")).unwrap();
    let comp = Composition::new("drive-test").consume::<ToolSchedule>().consume::<ToolWorkflow>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    let text = "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 10\n".to_string();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let allowed = aho_policy::preset("workspace").unwrap().cut(&BoundWorld::host());
    let plan = aho_cap::plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed },
        vec![],
    )
    .unwrap();
    let llm = Arc::new(ScriptedLlm::from_steps(steps));
    let seed = DriverSeed::new();
    let mut reg = Registry::default();
    reg.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    seed.seed_into(&mut reg);
    let log: Arc<dyn LogStore> = Arc::new(JsonlLog::open(dir.path().join("s.jsonl")).unwrap());
    let session = Session::boot(Boot {
        id: SessionId(1),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed, preset: "workspace".into(), approval_ref: None }],
        log,
        approval: Arc::new(NeverApproval),
        operator: Operator::authenticate(OperatorProof::local_tty()),
        workspace: dir.path().join("ws").display().to_string(),
        home: dir.path().display().to_string(),
        spill_dir: dir.path().join("spill"),
        seed: reg,
    })
    .await
    .unwrap();
    Rig { session, llm, seed, _dir: dir }
}

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

async fn until(s: &Session, what: &str, f: impl Fn(&[Entry]) -> bool) {
    for _ in 0..500 {
        if f(&entries(s)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn custom<'a>(es: &'a [Entry], ns: &str, name: &str) -> Vec<(usize, &'a Entry)> {
    es.iter().enumerate().filter(|(_, e)| matches!(&e.kind, EntryKind::Custom { ns: n, name: m, .. } if n == ns && m == name)).collect()
}

fn keyed(es: &[Entry], key: &str) -> Option<usize> {
    es.iter().position(|e| matches!(&e.kind, EntryKind::UserMessage { client_key: Some(k), .. } if k == key))
}

fn replays(s: &Session) {
    let n = verify_replay(&entries(s), &s.registry().entry_kinds, LateResults::SameCallId).unwrap();
    assert!(n >= 2, "{n} request headers re-derived");
    s.fold().unwrap();
}

#[tokio::test]
async fn a_schedule_the_agent_created_fires_as_a_real_turn_once_despite_missed_occurrences() {
    let r = boot(vec![
        Step::tool_call("schedule_create", json!({"prompt": "check the build", "every": "1m", "label": "ci"})),
        Step::text("I will check every minute.").expect_tool_result("Scheduled sched-1 \"ci\": every 1m"),
        Step::text("build is green"),
    ])
    .await;
    assert_eq!(r.session.prompt("keep an eye on CI", None).await.unwrap(), Transition::Completed);
    assert_eq!(custom(&entries(&r.session), "schedule", "Created").len(), 1);

    // the host was down for three and a half minutes after that
    let clock = Arc::new(SteppedClock::new(Timestamp::now()));
    let drivers = host(&r.seed, r.session.clone(), clock.clone());
    assert_eq!(drivers.running(), vec!["scheduler", "ralph"]);
    clock.advance(210_000);
    until(&r.session, "the Fired record", |es| !custom(es, "schedule", "Fired").is_empty()).await;
    let es = entries(&r.session);
    let prompt_at = keyed(&es, "schedule:sched-1:1").expect("the fired prompt carries its key");
    let EntryKind::UserMessage { text, .. } = &es[prompt_at].kind else { unreachable!() };
    assert!(text.starts_with("[Scheduled prompt sched-1 \"ci\" — every 1m, occurrence 1") && text.ends_with("check the build"), "{text}");
    assert!(text.contains("2 later occurrence(s)"), "{text}");
    let (fired_at, fired) = custom(&es, "schedule", "Fired")[0];
    assert_eq!(fired.by, Principal::system("schedule"));
    let EntryKind::Custom { body, .. } = &fired.kind else { unreachable!() };
    assert_eq!((body["n"].clone(), body["skipped"].clone()), (json!(1), json!(2)));
    let turn_end = es[prompt_at..].iter().position(|e| matches!(e.kind, EntryKind::TurnEnd { .. })).unwrap() + prompt_at;
    assert!(turn_end < fired_at, "the record follows the fired turn");
    assert!(es.iter().any(
        |e| matches!(&e.kind, EntryKind::AssistantMessage { content, .. } if content.iter().any(|b| b.as_text() == Some("build is green")))
    ));
    assert_eq!(r.llm.remaining(), 0);
    assert!(drivers.shutdown().await.is_empty());
    replays(&r.session);
}

#[tokio::test]
async fn a_ralph_loop_refeeds_the_prompt_until_the_promise_then_stops() {
    let r = boot(vec![
        Step::tool_call(
            "ralph_start",
            json!({"prompt": "make the parser tests pass", "completion_promise": "PARSER DONE", "max_iterations": 3}),
        ),
        Step::text("Loop started.").expect_tool_result("Started ralph-1"),
        Step::text("two tests still fail"),
        Step::text("all pass\n<promise>PARSER DONE</promise>"),
    ])
    .await;
    let drivers = host(&r.seed, r.session.clone(), Arc::new(SteppedClock::new(Timestamp::now())));
    assert_eq!(r.session.prompt("start a loop on the parser", None).await.unwrap(), Transition::Completed);
    until(&r.session, "the loop to finish", |es| !custom(es, "workflow", "Finished").is_empty()).await;
    let es = entries(&r.session);
    let (one, two) = (keyed(&es, "workflow:ralph-1:1").unwrap(), keyed(&es, "workflow:ralph-1:2").unwrap());
    assert!(one < two && keyed(&es, "workflow:ralph-1:3").is_none(), "two iterations, no third");
    let iterations: Vec<Value> = custom(&es, "workflow", "Iteration")
        .into_iter()
        .map(|(_, e)| match &e.kind {
            EntryKind::Custom { body, .. } => body["promise_found"].clone(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(iterations, vec![json!(false), json!(true)]);
    let (_, fin) = custom(&es, "workflow", "Finished")[0];
    let EntryKind::Custom { body, .. } = &fin.kind else { unreachable!() };
    assert_eq!(body["outcome"], "completed");
    assert_eq!(r.llm.remaining(), 0);
    // later turn ends do not feed a finished loop
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(keyed(&entries(&r.session), "workflow:ralph-1:3"), None);
    assert!(drivers.shutdown().await.is_empty());
    replays(&r.session);
}

#[tokio::test]
async fn the_session_sink_runs_a_key_once() {
    let r = boot(vec![Step::text("ran")]).await;
    let sink = SessionSink::new(r.session.clone());
    let first = sink.prompt("schedule", "do it".into(), "k-1".into()).await.unwrap();
    assert_eq!((first.final_text.as_str(), first.ran), ("ran", true));
    let again = sink.prompt("schedule", "do it".into(), "k-1".into()).await.unwrap();
    assert_eq!((again.final_text.as_str(), again.ran), ("ran", false), "no second turn; the script has no step left for one");
    assert_eq!(entries(&r.session).iter().filter(|e| matches!(e.kind, EntryKind::TurnStart { .. })).count(), 1);
}
