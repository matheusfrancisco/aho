#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
use std::sync::{Arc, Mutex};

use aho_cap::{Composition, Env, Os, PolicyInput, Registry, plan};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, Llm, LlmError, LlmRoute, Request, Response, Sink, StreamEvent};
use aho_log::CapState;
use aho_types::{CallId, ConsequenceSet, ContentBlock, Message, Role, StopReason};
use llm_scripted::{Scripted, ScriptedError, ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/scripted-two-turns.json");

fn request(messages: Vec<Message>) -> Request {
    Request { model: "scripted".into(), messages, max_tokens: 100, ..Default::default() }
}

async fn call(llm: &dyn Llm, messages: Vec<Message>) -> (Result<Response, LlmError>, Vec<StreamEvent>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let sink: Sink<'_> = Box::new(move |e| seen.lock().expect("lock").push(e));
    let res = llm.stream(request(messages), sink).await;
    let events = events.lock().expect("lock").clone();
    (res, events)
}

fn tool_turn(result: &str) -> Vec<Message> {
    vec![
        Message::user("please run the tests"),
        Message::assistant(vec![ContentBlock::ToolUse { id: CallId("c1".into()), name: "bash".into(), args: json!({}) }]),
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult { id: CallId("c1".into()), content: vec![ContentBlock::text(result)], is_error: false }],
        },
    ]
}

#[tokio::test]
async fn steps_play_in_order_with_events_and_default_ids() {
    let llm = ScriptedLlm::from_steps(vec![
        Step::text("Looking.").with_call(None, "read", json!({"path": "a.rs"})).with_call(Some("mine"), "glob", serde_json::Value::Null),
        Step::text("Done.").with_thinking("ok").with_stop(ScriptedStop::MaxTokens),
    ]);
    let (res, events) = call(&llm, vec![Message::user("hi")]).await;
    let res = res.expect("step 1");
    assert_eq!(res.stop, StopReason::ToolUse);
    let generated = match &res.content[1] {
        ContentBlock::ToolUse { id, .. } => id.clone(),
        other => panic!("expected a tool call, got {other:?}"),
    };
    assert!(generated.0.starts_with("scripted-") && generated.0.ends_with("-1-0"), "step 1, call 0: {generated:?}");
    assert_eq!(
        res.content,
        vec![
            ContentBlock::text("Looking."),
            ContentBlock::ToolUse { id: generated.clone(), name: "read".into(), args: json!({"path": "a.rs"}) },
            ContentBlock::ToolUse { id: CallId("mine".into()), name: "glob".into(), args: json!({}) },
        ]
    );
    assert_eq!(events[0], StreamEvent::Text("Looking.".into()));
    assert_eq!(events[1], StreamEvent::ToolUseStart { id: generated.clone(), name: "read".into() });
    assert_eq!(events[2], StreamEvent::ToolUseEnd { id: generated.clone(), name: "read".into(), args: json!({"path": "a.rs"}) });
    let fresh = ScriptedLlm::from_steps(vec![Step::tool_call("read", json!({}))]);
    let (again, _) = call(&fresh, vec![Message::user("hi")]).await;
    assert!(
        !matches!(&again.unwrap().content[0], ContentBlock::ToolUse { id, .. } if *id == generated),
        "a new model instance (a reopened session) must not reuse an earlier default id"
    );
    assert!(matches!(events.last(), Some(StreamEvent::Usage(u)) if u.input_tokens == 1 && u.output_tokens > 0));
    assert_eq!(llm.remaining(), 1);

    let (res, events) = call(&llm, vec![Message::user("hi")]).await;
    let res = res.expect("step 2");
    assert_eq!(res.stop, StopReason::MaxTokens);
    assert_eq!(res.content, vec![ContentBlock::Thinking { text: "ok".into() }, ContentBlock::text("Done.")]);
    assert_eq!(events[0], StreamEvent::Thinking("ok".into()));
    assert_eq!(llm.remaining(), 0);
    assert_eq!(llm.info().late_results, LateResults::SameCallId);
}

#[tokio::test]
async fn expectation_mismatch_is_fatal_with_step_and_actual_text() {
    let llm = ScriptedLlm::from_steps(vec![
        Step::text("x").expect_user("deploy"),
        Step::text("y").expect_tool_result("passed"),
        Step::text("z").expect_tool_result("a"),
    ]);
    let (res, events) = call(&llm, tool_turn("3 failed")).await;
    assert_eq!(
        res,
        Err(LlmError::Fatal(
            "scripted model: step 1 expected the last user message to contain \"deploy\", but it was \"please run the tests\"".into()
        ))
    );
    assert!(events.is_empty(), "a failed expectation emits nothing");
    let (res, _) = call(&llm, tool_turn("3 failed")).await;
    assert_eq!(
        res,
        Err(LlmError::Fatal(
            "scripted model: step 2 expected the latest tool result to contain \"passed\", but it was \"3 failed\"".into()
        ))
    );
    let (res, _) = call(&llm, vec![Message::user("hi")]).await;
    assert!(matches!(res, Err(LlmError::Fatal(m)) if m.contains("step 3") && m.contains("carries no tool result")));
    assert_eq!(llm.remaining(), 0, "failed steps are consumed");
}

#[tokio::test]
async fn exhausted_script_names_the_request_and_step_count() {
    let llm = ScriptedLlm::from_steps(vec![Step::text("only")]);
    assert!(call(&llm, vec![Message::user("one")]).await.0.is_ok());
    let (res, _) = call(&llm, vec![Message::user("two")]).await;
    assert!(
        matches!(res, Err(LlmError::Fatal(m)) if m.contains("request 2 has no step") && m.contains("1 step(s)") && m.contains("\"two\"")),
    );
}

#[tokio::test]
async fn error_steps_map_to_seam_errors_and_the_next_step_recovers() {
    let llm = ScriptedLlm::from_steps(vec![
        Step::error(ScriptedError::RateLimited),
        Step::error(ScriptedError::Transient),
        Step::error(ScriptedError::ContextWindow),
        Step::text("recovered"),
    ]);
    let msgs = || vec![Message::user("a".repeat(40))];
    assert_eq!(call(&llm, msgs()).await.0, Err(LlmError::RateLimited { retry_after_ms: 0 }));
    assert!(matches!(call(&llm, msgs()).await.0, Err(LlmError::Transient(_))));
    assert_eq!(call(&llm, msgs()).await.0, Err(LlmError::ContextWindowExceeded { requested: 10, available: 0 }));
    assert_eq!(call(&llm, msgs()).await.0.map(|r| r.content), Ok(vec![ContentBlock::text("recovered")]));
}

#[test]
fn script_documents_reject_unknown_fields() {
    let err = ScriptedLlm::from_json(r#"{"steps":[{"expect_last_user_contain":"x"}]}"#, "t.json").err().expect("typo rejected");
    assert!(err.contains("t.json") && err.contains("expect_last_user_contain"), "{err}");
    assert!(ScriptedLlm::from_json(r#"{"steps":[{"error":"nope"}]}"#, "t.json").is_err());
}

fn start(sets: &[(&str, &str)]) -> (CapState, Registry, Vec<String>) {
    let comp = Composition::new("t").provide::<Scripted>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let inputs = ResolveInputs { sets: sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..Default::default() };
    let loadout = resolve(&inputs, &schema, false).expect("resolve");
    let plan = plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL },
        vec![],
    )
    .expect("plan");
    let state = plan.row("llm-scripted").expect("row").state.clone();
    let (reg, broken) = comp.start(&plan, &loadout);
    (state, reg, broken.into_iter().map(|(_, e)| e).collect())
}

#[tokio::test]
async fn capability_loads_the_script_file_and_registers_llm_scripted() {
    let (state, reg, broken) = start(&[("provider.scripted.script", FIXTURE)]);
    assert_eq!(state, CapState::Active);
    assert!(broken.is_empty(), "{broken:?}");
    let route: LlmRoute = reg.service("llm:scripted").expect("route");
    let (res, _) = call(route.0.as_ref(), vec![Message::user("run the tests")]).await;
    assert_eq!(res.expect("step 1").stop, StopReason::ToolUse);
    let (res, _) = call(route.0.as_ref(), tool_turn("412 passed")).await;
    assert_eq!(res.expect("step 2").content[1], ContentBlock::text("All green."));
}

#[test]
fn capability_is_locked_without_a_script_and_broken_on_an_unreadable_one() {
    let (state, _, _) = start(&[]);
    assert!(matches!(state, CapState::Locked { missing } if missing[0].contains("provider.scripted.script")));
    let missing = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/does-not-exist.json");
    let (state, reg, broken) = start(&[("provider.scripted.script", missing)]);
    assert_eq!(state, CapState::Active);
    assert!(reg.service::<LlmRoute>("llm:scripted").is_none());
    assert!(broken[0].contains("could not read the model script") && broken[0].contains("does-not-exist.json"), "{broken:?}");
}
