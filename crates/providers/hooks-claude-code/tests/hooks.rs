//! Real `sh` hooks against the capability as composed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::BTreeMap;
use std::path::Path;

use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_types::*;
use hooks_claude_code::{ClaudeCodeHooks, HooksConfig, HooksStatus, STATUS_SERVICE};
use serde_json::json;

fn hook(command: &str) -> Value {
    json!({"type": "command", "command": command})
}

fn settings(events: Value) -> String {
    json!({"hooks": events}).to_string()
}

fn start_with(project: &Path, settings: Option<&str>, sets: &[(&str, &str)], env: &[(&str, &str)]) -> Registry {
    if let Some(s) = settings {
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::write(project.join(".claude/settings.json"), s).unwrap();
    }
    let mut schema = Schema::default();
    core_keys().into_iter().chain(<HooksConfig as Config>::keys()).for_each(|k| schema.add(k));
    let mut sets: Vec<(String, String)> = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    sets.push(("hooks.claude_code.project_dir".into(), project.display().to_string()));
    let env: BTreeMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let l = resolve(&ResolveInputs { sets, env, ..ResolveInputs::default() }, &schema, false).unwrap();
    let comp = Composition::new("t").provide::<ClaudeCodeHooks>();
    let mut e = Env::empty(Os::current());
    e.binaries.insert("sh".into());
    let p = plan(&comp.manifests(), &l, &e, &PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL }, vec![]).unwrap();
    assert!(p.active().any(|r| r.id.name == "hooks-claude-code"));
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    reg
}

fn start(project: &Path, s: &str) -> Registry {
    start_with(project, Some(s), &[], &[])
}

fn view(tool: &'static str, call: &str, args: Value) -> ExecView {
    ExecView {
        call: CallId(call.into()),
        tool: tool.into(),
        class: Consequence::SpawnsProcess,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: String::new(),
            home: String::new(),
        },
        by: Principal::Agent { id: AgentId(0), turn: TurnId(1) },
    }
}

fn pre(reg: &Registry, v: &ExecView) -> PreDecision {
    assert_eq!(reg.pre.len(), 1);
    (reg.pre[0].1)(v)
}

fn post(reg: &Registry, v: &ExecView, o: &ExecOutcome) -> PostDecision {
    (reg.post[0].1)(v, o)
}

fn event(reg: &Registry, e: HookEvent, payload: Value) -> HookOutput {
    let spec = reg.hooks.iter().map(|(_, h)| h).find(|h| h.on.contains(&e)).expect("hook registered");
    futures::executor::block_on((spec.run)(e, &payload))
}

#[test]
fn exit_2_blocks_with_stderr_and_the_hook_gets_the_claude_code_payload() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = "cat > '${CLAUDE_PROJECT_DIR}/payload.json'; printf %s \"$AHO_HOOKS_DISABLED\" > guard.txt; echo 'rm is not allowed here' >&2; exit 2";
    let reg = start(dir.path(), &settings(json!({"PreToolUse": [{"matcher": "Bash", "hooks": [hook(cmd)]}]})));
    event(&reg, HookEvent::SessionStart, json!({"session": "s-1"}));

    let d = pre(&reg, &view("bash", "call-7", json!({"command": "rm -rf /"})));
    assert_eq!(d, PreDecision::Deny { reason: "rm is not allowed here".into() });
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("payload.json")).unwrap()).unwrap();
    assert_eq!(payload["hook_event_name"], "PreToolUse");
    assert_eq!(payload["tool_name"], "Bash");
    assert_eq!(payload["tool_input"], json!({"command": "rm -rf /"}));
    assert_eq!(payload["tool_use_id"], "call-7");
    assert_eq!(payload["session_id"], "s-1");
    assert_eq!(payload["cwd"], dir.path().display().to_string());
    assert_eq!(std::fs::read_to_string(dir.path().join("guard.txt")).unwrap(), "1", "children carry the recursion guard");

    std::fs::remove_file(dir.path().join("payload.json")).unwrap();
    assert_eq!(pre(&reg, &view("read", "c2", json!({"path": "a"}))), PreDecision::Allow);
    assert!(!dir.path().join("payload.json").exists(), "a non-matching matcher runs nothing");
}

#[test]
fn json_decisions_meet_and_additional_context_is_deferred_to_after_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let out = |j: Value| format!("echo '{j}'");
    let reg = start(
        dir.path(),
        &settings(json!({"PreToolUse": [
            {"matcher": "Write", "hooks": [hook(&out(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": "no writes today"}})))]},
            {"matcher": "Edit|Write", "hooks": [hook(&out(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "ask", "permissionDecisionReason": "check the diff"}})))]},
            {"matcher": "Read", "hooks": [hook(&out(json!({"decision": "block", "reason": "secrets live there"})))]},
            {"matcher": "Glob", "hooks": [hook(&out(json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "additionalContext": "prefer rg"}})))]},
            {"matcher": "mcp__.*", "hooks": [hook(&out(json!({"continue": false, "stopReason": "no MCP in CI"})))]}
        ]})),
    );
    assert_eq!(pre(&reg, &view("write", "w", json!({}))), PreDecision::Deny { reason: "no writes today".into() }, "deny beats ask");
    assert_eq!(pre(&reg, &view("edit", "e", json!({}))), PreDecision::Ask { reason: Some("check the diff".into()) });
    assert_eq!(pre(&reg, &view("read", "r", json!({}))), PreDecision::Deny { reason: "secrets live there".into() });
    assert_eq!(pre(&reg, &view("mcp__gh__x", "m", json!({}))), PreDecision::Deny { reason: "no MCP in CI".into() });
    let g = view("glob", "g", json!({"pattern": "*"}));
    assert_eq!(pre(&reg, &g), PreDecision::Allow);
    assert_eq!(post(&reg, &g, &ExecOutcome::text("x")), PostDecision::Accept { content: None, deferred_context: vec!["prefer rg".into()] });
    assert_eq!(post(&reg, &g, &ExecOutcome::text("x")), PostDecision::Accept { content: None, deferred_context: vec![] }, "drained once");
}

#[test]
fn updated_input_is_parsed_and_refused() {
    let dir = tempfile::tempdir().unwrap();
    let j =
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "updatedInput": {"command": "ls"}}});
    let reg = start(dir.path(), &settings(json!({"PreToolUse": [{"matcher": "", "hooks": [hook(&format!("echo '{j}'"))]}]})));
    let v = view("bash", "b1", json!({"command": "rm -rf build"}));
    let PreDecision::Ask { reason: Some(reason) } = pre(&reg, &v) else {
        panic!("an allow with a rewrite must not run the original silently")
    };
    assert!(reason.contains("returned `updatedInput`") && reason.contains("the rewrite was refused"), "{reason}");
    assert_eq!(v.args, json!({"command": "rm -rf build"}), "the logged arguments are untouched");
    let PostDecision::Accept { deferred_context, .. } = post(&reg, &v, &ExecOutcome::text("ok")) else { panic!() };
    assert_eq!(deferred_context, vec![reason], "the warning is attached after the call");
}

#[test]
fn hook_failures_fail_closed_unless_the_hook_opts_out() {
    let dir = tempfile::tempdir().unwrap();
    let reg = start(
        dir.path(),
        &settings(json!({"PreToolUse": [
            {"matcher": "Bash", "hooks": [hook("echo broken >&2; exit 1")]},
            {"matcher": "Grep", "hooks": [{"type": "command", "command": "exit 1", "on_error": "allow"}]},
            {"matcher": "Glob", "hooks": [{"type": "command", "command": "sleep 5", "timeout": 0.3}]},
            {"matcher": "Read", "hooks": [hook("echo '{not json'")]},
            {"matcher": "LS", "hooks": [hook("echo plain text is fine")]}
        ]})),
    );
    let PreDecision::Deny { reason } = pre(&reg, &view("bash", "a", json!({}))) else { panic!() };
    assert!(reason.contains("failed with exit 1: broken") && reason.contains("fails closed"), "{reason}");
    assert_eq!(pre(&reg, &view("grep", "b", json!({}))), PreDecision::Allow);
    let t = std::time::Instant::now();
    let PreDecision::Deny { reason } = pre(&reg, &view("glob", "c", json!({}))) else { panic!() };
    assert!(reason.contains("ran past its 0.3s timeout"), "{reason}");
    assert!(t.elapsed() < std::time::Duration::from_secs(3));
    let PreDecision::Deny { reason } = pre(&reg, &view("read", "d", json!({}))) else { panic!() };
    assert!(reason.contains("not valid JSON"), "{reason}");
    assert_eq!(pre(&reg, &view("ls", "e", json!({}))), PreDecision::Allow);
}

#[test]
fn post_tool_use_blocks_and_sees_the_tool_response() {
    let dir = tempfile::tempdir().unwrap();
    let reg = start(
        dir.path(),
        &settings(json!({"PostToolUse": [
            {"matcher": "Write", "hooks": [hook("cat > post.json; echo 'lint failed: missing semicolon' >&2; exit 2")]},
            {"matcher": "Edit", "hooks": [hook("echo '{\"hookSpecificOutput\": {\"hookEventName\": \"PostToolUse\", \"additionalContext\": \"formatted\"}}'")]}
        ]})),
    );
    assert!(reg.pre.is_empty(), "no PreToolUse hooks → no pre_execute");
    let w = view("write", "w1", json!({"path": "src/a.rs", "content": "x"}));
    let outcome = ExecOutcome::Done { value: json!({"written": 1}), content: vec![], meta: None };
    assert_eq!(
        post(&reg, &w, &outcome),
        PostDecision::Block { feedback: "lint failed: missing semicolon".into(), deferred_context: vec![] }
    );
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("post.json")).unwrap()).unwrap();
    assert_eq!(payload["hook_event_name"], "PostToolUse");
    assert_eq!(payload["tool_response"], json!({"written": 1}));
    assert_eq!(payload["tool_input"]["file_path"], "src/a.rs", "`path` is mirrored as Claude Code's `file_path`");
    assert_eq!(
        post(&reg, &view("edit", "e1", json!({})), &ExecOutcome::err("E", "x")),
        PostDecision::Accept { content: None, deferred_context: vec!["formatted".into()] }
    );
}

#[test]
fn session_and_prompt_events_map_onto_hook_points() {
    let dir = tempfile::tempdir().unwrap();
    let reg = start(
        dir.path(),
        &settings(json!({
            "SessionStart": [{"matcher": "startup", "hooks": [hook("echo 'branch: main'")]}],
            "UserPromptSubmit": [{"hooks": [hook("echo x >> prompts.log; echo 'prompt blocked: contains a secret' >&2; exit 2")]}],
            "Stop": [{"hooks": [hook("echo '{\"decision\": \"block\", \"reason\": \"tests are still failing\"}'")]}],
            "Notification": [{"hooks": [hook("true")]}]
        })),
    );
    let status = reg.service::<std::sync::Arc<HooksStatus>>(STATUS_SERVICE).unwrap();
    assert_eq!(status.groups, 3);
    assert!(status.warnings[0].contains("`Notification` has no aho equivalent"), "{:?}", status.warnings);

    let s = event(&reg, HookEvent::SessionStart, json!({"session": "s-9"}));
    assert_eq!(s.context, vec!["branch: main"]);
    let p = event(&reg, HookEvent::PreStep, json!({"turn": 1, "step": 1}));
    assert_eq!(p.stop.as_deref(), Some("prompt blocked: contains a secret"));
    assert!(matches!(p.decision, Some(Decision::Deny(_))));
    assert_eq!(event(&reg, HookEvent::PreStep, json!({"turn": 1, "step": 2})), HookOutput::default(), "once per turn");
    event(&reg, HookEvent::PreStep, json!({"turn": 2, "step": 1}));
    assert_eq!(std::fs::read_to_string(dir.path().join("prompts.log")).unwrap(), "x\nx\n");
    let stop = event(&reg, HookEvent::TurnStopping, json!({}));
    assert_eq!(stop.decision, Some(Decision::Deny("tests are still failing".into())));
    assert_eq!(stop.context, vec!["tests are still failing"], "the model is told why it must continue");
}

#[test]
fn an_unreadable_hooks_file_denies_every_call_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let reg = start(dir.path(), "{\"hooks\": {\"PreToolUse\": [,]}}");
    let PreDecision::Deny { reason } = pre(&reg, &view("read", "r", json!({}))) else { panic!() };
    assert!(reason.contains("settings.json is not valid JSON") && reason.contains("hooks.claude_code.on_error"), "{reason}");

    let other = tempfile::tempdir().unwrap();
    let reg = start_with(other.path(), Some("{\"hooks\": 3}"), &[("hooks.claude_code.on_error", "allow")], &[]);
    assert!(reg.pre.is_empty(), "on_error = allow skips the broken file");
    assert!(reg.service::<std::sync::Arc<HooksStatus>>(STATUS_SERVICE).unwrap().error.is_some());
}

#[test]
fn aho_hooks_disabled_turns_everything_off() {
    let dir = tempfile::tempdir().unwrap();
    let s = settings(json!({"PreToolUse": [{"hooks": [hook("exit 2")]}], "SessionStart": [{"hooks": [hook("true")]}]}));
    let reg = start_with(dir.path(), Some(&s), &[], &[("AHO_HOOKS_DISABLED", "1")]);
    assert!(reg.pre.is_empty() && reg.post.is_empty() && reg.hooks.is_empty());
    assert!(reg.service::<std::sync::Arc<HooksStatus>>(STATUS_SERVICE).unwrap().disabled);
    let reg = start_with(dir.path(), Some(&s), &[], &[]);
    assert_eq!(reg.pre.len(), 1);
}

#[test]
fn extra_files_and_hooks_json_shape_are_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("plugin-hooks.json"),
        json!({"PreToolUse": [{"matcher": "Bash", "hooks": [hook("exit 2")]}]}).to_string(),
    )
    .unwrap();
    let reg = start_with(dir.path(), None, &[("hooks.claude_code.files", "[\"plugin-hooks.json\"]")], &[]);
    assert!(matches!(pre(&reg, &view("bash", "b", json!({}))), PreDecision::Deny { reason } if reason.contains("exit 2")));
}
