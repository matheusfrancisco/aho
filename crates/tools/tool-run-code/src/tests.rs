use super::*;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use aho_config::{ResolveInputs, Schema, resolve};
use world_host::{HostSubprocess, HostWorld};

fn build(root: &Path, nested: Option<ToolCallSlot>, cfg: RunCodeConfig) -> ToolRunCode {
    let proc = SubprocessHandle(Arc::new(HostSubprocess::new(root)));
    ToolRunCode::build(RunCodeDeps { proc, ops: None, nested }, cfg).unwrap()
}

fn tool(root: &Path) -> ToolRunCode {
    build(root, None, RunCodeConfig::default())
}

fn policy(allowed: ConsequenceSet, world: BoundWorld) -> PolicyView {
    PolicyView { epoch: Epoch(0), allowed, world, workspace: "/work/proj".into(), home: "/Users/someone".into() }
}

fn req(args: Value) -> ExecRequest {
    let view = ExecView {
        call: CallId("call-7".into()),
        tool: "run_code".into(),
        class: Consequence::SpawnsProcess,
        args,
        policy: policy(ConsequenceSet::ALL, BoundWorld::host()),
        by: Principal::system("test"),
    };
    ExecRequest { view, cancel: CancelToken::new(), progress: None }
}

fn value(o: &ExecOutcome) -> Value {
    match o {
        ExecOutcome::Done { value, .. } => value.clone(),
        other => panic!("expected Done, got {other:?}"),
    }
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(|b| b.as_text()).collect(),
        ExecOutcome::Err { code, reason } => format!("{code}: {reason}"),
        ExecOutcome::Cancelled => "<cancelled>".into(),
    }
}

/// A nested dispatcher standing in for the session: `echo` returns what it got, anything else is denied.
fn echo_slot(seen: Arc<Mutex<Vec<(String, String)>>>) -> ToolCallSlot {
    let slot: ToolCallSlot = Arc::default();
    let f: NestedToolCall = Arc::new(move |parent: ExecView, _cancel: CancelToken, tool: String, args: Value| {
        seen.lock().unwrap().push((parent.call.0.clone(), tool.clone()));
        Box::pin(async move {
            if tool == "echo" {
                ExecOutcome::Done { value: json!({"tool": tool, "args": args}), content: vec![], meta: None }
            } else {
                ExecOutcome::err("DENIED", format!("`{tool}` is not allowed here"))
            }
        })
    });
    assert!(slot.set(f).is_ok());
    slot
}

#[tokio::test]
async fn python_output_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let o =
        tool(dir.path()).run(&req(json!({"language": "python", "code": "import sys\nprint('hi from py', 6 * 7)\nsys.exit(3)\n"}))).await;
    let v = value(&o);
    assert_eq!(v["output"], json!("hi from py 42\n"));
    assert_eq!((v["exit"].clone(), v["timed_out"].clone()), (json!(3), json!(false)));
    assert!(text(&o).ends_with("[exit 3]"), "{}", text(&o));
    assert_eq!(v["classes"], json!(["host_write", "spawns_process"]));
}

#[tokio::test]
async fn node_output_and_cwd_is_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "from the workspace").unwrap();
    let code = "const fs = require('fs');\nconsole.log('hi from node', 6 * 7);\nconsole.log(fs.readFileSync('data.txt', 'utf8'));\nawait new Promise((r) => setTimeout(r, 10));\nconsole.error('to stderr');\n";
    let o = tool(dir.path()).run(&req(json!({"language": "node", "code": code}))).await;
    let v = value(&o);
    assert_eq!(v["output"], json!("hi from node 42\nfrom the workspace\nto stderr\n"), "{}", text(&o));
    assert_eq!(v["exit"], json!(0));
    let err = tool(dir.path()).run(&req(json!({"language": "node", "code": "throw new Error('boom')"}))).await;
    assert_eq!(value(&err)["exit"], json!(1));
    assert!(value(&err)["output"].as_str().unwrap().contains("Error: boom"), "{}", text(&err));
}

#[tokio::test]
async fn a_program_past_its_time_limit_is_killed_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path());
    for (language, code) in
        [("python", "import time\nprint('start', flush=True)\ntime.sleep(30)\n"), ("node", "console.log('start');\nwhile (true) {}\n")]
    {
        let started = Instant::now();
        let o = t.run(&req(json!({"language": language, "code": code, "timeout_ms": 700}))).await;
        let v = value(&o);
        assert_eq!(v["timed_out"], json!(true), "{language}: {}", text(&o));
        assert!(v["output"].as_str().unwrap().starts_with("start"), "{language}: {}", text(&o));
        assert!(text(&o).contains("[killed after 700 ms: the time limit was reached"), "{}", text(&o));
        assert!(started.elapsed() < Duration::from_secs(8), "{language} took {:?}", started.elapsed());
    }
}

#[tokio::test]
async fn output_past_the_cap_keeps_head_and_tail_and_counts_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let code = "print('BEGIN')\nfor i in range(20000):\n    print('x' * 20)\nprint('END')\n";
    let o = tool(dir.path()).run(&req(json!({"language": "python", "code": code, "max_output_bytes": 1000}))).await;
    let v = value(&o);
    let out = v["output"].as_str().unwrap();
    assert!(out.starts_with("BEGIN\n") && out.ends_with("END\n"), "{out}");
    assert!(out.len() < 1100, "kept {} bytes", out.len());
    let total = v["output_bytes"].as_u64().unwrap();
    assert_eq!(total, 6 + 20000 * 21 + 4);
    assert_eq!(v["omitted_bytes"].as_u64().unwrap(), total - 1000);
    assert!(text(&o).contains("[output capped at 1000 bytes:"), "{}", &text(&o)[text(&o).len() - 200..]);
}

#[tokio::test]
async fn nested_calls_go_through_the_session_service_in_both_languages() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let t = build(dir.path(), Some(echo_slot(seen.clone())), RunCodeConfig::default());
    let py = "v = aho.call('echo', {'x': 1, 'blob': 'y' * 5000})\nprint(v['args']['x'], len(v['args']['blob']))\ntry:\n    aho.call('bash', command='ls')\nexcept aho.ToolError as e:\n    print('ERR', e.code)\nimport aho as again\nprint(again.call('echo', {'s': 'h\\u00e9llo \\u2713'})['args']['s'])\n";
    let o = t.run(&req(json!({"language": "python", "code": py}))).await;
    let v = value(&o);
    assert_eq!(v["output"], json!("1 5000\nERR DENIED\nhéllo ✓\n"), "{}", text(&o));
    assert_eq!(v["tool_calls"], json!(3));
    assert!(text(&o).contains("[3 nested tool calls, 1 failed]"), "{}", text(&o));
    let js = "const v = aho.call('echo', { x: 2, s: 'h\\u00e9llo \\u2713', blob: 'z'.repeat(3000) });\nconsole.log(v.args.x, v.args.s, v.args.blob.length);\ntry { aho.call('write', { path: 'a' }); } catch (e) { console.log('ERR', e.code, e instanceof aho.ToolError); }\n";
    let o = t.run(&req(json!({"language": "node", "code": js}))).await;
    assert_eq!(value(&o)["output"], json!("2 héllo ✓ 3000\nERR DENIED true\n"), "{}", text(&o));
    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 5);
    assert!(calls.iter().all(|(parent, _)| parent == "call-7"), "{calls:?}");
    assert_eq!(calls.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(), vec!["echo", "bash", "echo", "echo", "write"]);

    // the log copy: one Call + one Result per nested call; big values keep size + digest only
    let events = value(&o)["events"].as_array().unwrap().clone();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["body"]["args"]["x"], json!(2));
    let copied = &events[1]["body"]["value"];
    assert!(copied.get("tool").is_some() || copied["truncated"] == json!(true));
    assert_eq!(events[3]["body"]["outcome"], json!("err"));
    let mut state = None;
    for e in &events {
        state = Some(fold_ptc(state.as_ref(), e["name"].as_str().unwrap(), &e["body"]).unwrap());
    }
    assert_eq!(state.unwrap(), json!({"calls": 2, "errors": 1, "open": []}));
}

#[tokio::test]
async fn without_the_session_service_nested_calls_fail_clearly_and_the_program_continues() {
    let dir = tempfile::tempdir().unwrap();
    let code = "try:\n    aho.call('read', {'path': 'x'})\nexcept aho.ToolError as e:\n    print(e.code)\n    print(e.reason)\ntry:\n    aho.call('run_code', {'language': 'python', 'code': '1'})\nexcept aho.ToolError as e:\n    print(e.code)\nprint('still running')\n";
    let o = tool(dir.path()).run(&req(json!({"language": "python", "code": code}))).await;
    let out = value(&o)["output"].as_str().unwrap().to_string();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "PTC_UNAVAILABLE");
    assert!(lines[1].contains("service `aho.tool_call`") && lines[1].contains("`read` did not run"), "{out}");
    assert_eq!(&lines[2..], ["PTC_RECURSION", "still running"]);
}

#[tokio::test]
async fn the_program_cannot_read_the_tool_channel_as_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let o = tool(dir.path())
        .run(&req(json!({"language": "python", "code": "import sys\nprint(repr(sys.stdin.read()))\n", "timeout_ms": 5000})))
        .await;
    assert_eq!(value(&o)["output"], json!("''\n"), "{}", text(&o));
}

#[tokio::test]
async fn a_missing_interpreter_is_reported_with_the_setting_that_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RunCodeConfig { python: "python3-not-installed-here".into(), ..RunCodeConfig::default() };
    let o = build(dir.path(), None, cfg).run(&req(json!({"language": "python", "code": "print(1)"}))).await;
    assert_eq!(
        text(&o),
        "PROC_NOT_FOUND: `python3-not-installed-here` was not found in this world; nothing ran. Install it, or point `caps.tool-run-code.python` at it."
    );
    let bad = tool(dir.path()).run(&req(json!({"language": "ruby", "code": "puts 1"}))).await;
    assert!(text(&bad).starts_with("TOOL_BAD_ARGS: `language` must be"), "{}", text(&bad));
}

#[test]
fn host_write_is_required_under_a_world_that_does_not_confine() {
    use Consequence::*;
    let host = BoundWorld::host();
    let workspace = ConsequenceSet::of(&[ReadOnly, WorkspaceWrite, SpawnsProcess, TalksToHuman]);
    let ask = check_run_code(&json!({}), &policy(workspace, host.clone()));
    assert!(matches!(&ask, Some(Decision::Ask(m)) if m.contains("HostWrite") && m.contains("`aho preset dev`")), "{ask:?}");
    let deny = check_run_code(&json!({}), &policy(workspace.without(TalksToHuman), host.clone()));
    assert!(matches!(&deny, Some(Decision::Deny(m)) if m.contains("Nothing ran")), "{deny:?}");
    assert_eq!(check_run_code(&json!({}), &policy(workspace.with(HostWrite), host.clone())), None);
    let ns = BoundWorld { name: "namespace".into(), confines: true, enforced_by: "Linux namespaces".into() };
    assert_eq!(check_run_code(&json!({}), &policy(workspace, ns.clone())), None);
    assert_eq!(effective_classes(&ns), ConsequenceSet::of(&[SpawnsProcess]));
    assert_eq!(effective_classes(&host), ConsequenceSet::of(&[SpawnsProcess, HostWrite]));
}

#[test]
fn the_ptc_fold_refuses_results_without_calls_and_duplicate_calls() {
    let call = json!({"parent": "c1", "n": 1, "tool": "read"});
    let s = fold_ptc(None, "Call", &call).unwrap();
    assert!(fold_ptc(Some(&s), "Call", &call).unwrap_err().contains("logged twice"));
    assert!(fold_ptc(Some(&s), "Result", &json!({"parent": "c1", "n": 2, "outcome": "ok"})).unwrap_err().contains("has no call"));
    assert!(fold_ptc(Some(&s), "Other", &call).is_err());
}

#[tokio::test]
async fn runs_through_a_composition_with_the_session_seeded_slot() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let comp = Composition::new("t").provide::<HostWorld>().consume::<ToolRunCode>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.sets = vec![
        ("workspace.root".into(), ws.path().to_string_lossy().into_owned()),
        ("workspace.shadow_dir".into(), home.path().to_string_lossy().into_owned()),
    ];
    let l = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.extend(["git".to_string(), "python3".to_string()]);
    let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    // Seeded like the session would: an empty slot, filled after the composition started.
    let slot: ToolCallSlot = Arc::default();
    let mut seed = Registry::default();
    seed.services.insert(TOOL_CALL_SERVICE.into(), Arc::new(slot.clone()));
    let (reg, broken) = comp.start_seeded(&p, &l, seed);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.entry_kinds.contains_key("Ptc"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let filled = echo_slot(seen.clone());
    assert!(slot.set(filled.get().unwrap().clone()).is_ok());
    let (decl, exec) = reg.tool("run_code").unwrap();
    assert_eq!((decl.class, decl.op_kind, decl.external_effect), (Consequence::SpawnsProcess, OpKind::Process, true));
    let o = exec(req(json!({"language": "python", "code": "print(aho.call('echo', {'k': 'v'})['args']['k'])"}))).await;
    assert_eq!(value(&o)["output"], json!("v\n"), "{}", text(&o));
    assert_eq!(seen.lock().unwrap().len(), 1);
}
