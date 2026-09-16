//! A real session (host world, tool-fs, a scripted model, a JSONL log) writes, rewinds, and then
//! queries its own history through the loop and through the registered tool bodies.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::Arc;

use aho_approval::NeverApproval;
use aho_cap::{CancelToken, Composition, Env, ExecOutcome, ExecRequest, ExecView, Os, PolicyInput, Registry};
use aho_config::{ConfigKey, FileSnapshot, Layer, ResolveInputs, Schema, Ty, resolve};
use aho_llm::LlmRoute;
use aho_log::{CheckpointCause, EntryKind, JsonlLog, LogStore, RewindMode, Transition};
use aho_session::{Boot, Session};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::{Value, json};

const SECRET: &str = "sk-test-SECRET-9f8e7d";

fn composition() -> Composition {
    Composition::new("session-query-test")
        .provide::<world_host::HostWorld>()
        .consume::<tool_fs::ToolFs>()
        .consume::<tool_session::ToolSession>()
}

fn have_git() -> bool {
    std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

async fn call(s: &Session, tool: &str, own: &str, args: Value) -> ExecOutcome {
    let (decl, exec) = s.registry().tool(tool).unwrap();
    let view = ExecView {
        call: CallId(own.into()),
        tool: tool.into(),
        class: decl.class,
        args,
        policy: s.policy_view(),
        by: Principal::system("test"),
    };
    exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }).await
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(|b| b.as_text()).collect(),
        other => panic!("expected a result, got {other:?}"),
    }
}

fn value(o: &ExecOutcome) -> Value {
    match o {
        ExecOutcome::Done { value, .. } => value.clone(),
        other => panic!("expected a result, got {other:?}"),
    }
}

fn err_code(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Err { code, .. } => code.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn hit_ids(o: &ExecOutcome) -> Vec<u64> {
    value(o)["hits"].as_array().unwrap().iter().map(|h| h["id"].as_u64().unwrap()).collect()
}

#[tokio::test]
async fn a_real_session_searches_traces_and_redacts_its_own_history_across_a_rewind() {
    if !have_git() {
        eprintln!("skipped: `git` is not on PATH, and the host world's checkpoints need it");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (root, home) = (dir.path().join("ws"), dir.path().join("home"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap();

    let comp = composition();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    schema.add(ConfigKey::new("provider.test.key", Ty::String, None, "a credential for this test").secret());
    let file = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 150\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n\
         [policy]\npreset = \"workspace\"\n[provider.test]\nkey = \"{SECRET}\"\n[session_query]\nmax_hits = 5\n",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: file });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    assert!(loadout.secrets.contains_key("provider.test.key"));
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let world = aho_cap::BoundWorld::host();
    let allowed = aho_policy::preset("workspace").unwrap().cut(&world);
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();

    let llm = Arc::new(ScriptedLlm::from_steps(vec![
        Step::tool_call("write", json!({"path": "notes.txt", "content": format!("token {SECRET} and the word pineapple\n")}))
            .with_stop(ScriptedStop::ToolUse),
        Step::text("wrote the notes"),
        Step::tool_call("write", json!({"path": "notes.txt", "content": "v2 mango\n"})).with_stop(ScriptedStop::ToolUse),
        Step::text("changed to mango"),
        Step::tool_call("session_search", json!({"query": "pineapple"})).with_stop(ScriptedStop::ToolUse),
        Step::text("found it").expect_tool_result("the word pineapple"),
    ]));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let log: Arc<dyn LogStore> = Arc::new(JsonlLog::open(home.join("session.jsonl")).unwrap());
    let s = Session::boot(Boot {
        id: SessionId(11),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![
            EntryKind::ConfigResolved { key: "provider.test.key".into(), sourced: loadout.config["provider.test.key"].clone() },
            EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: plan.allowed, preset: plan.preset.clone(), approval_ref: None },
        ],
        log: log.clone(),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: root.display().to_string(),
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    assert!(s.registry().tool("session_search").is_some(), "tool-session is active");

    assert_eq!(s.prompt("write the notes", None).await.unwrap(), Transition::Completed);
    assert_eq!(s.prompt("now change it", None).await.unwrap(), Transition::Completed);
    let before_rewind = log.head().unwrap();
    let all = log.read_from(None);
    let calls: Vec<(EntryId, CallId)> = all
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::ToolCall { call, .. } => Some((e.id, call.clone())),
            _ => None,
        })
        .collect();
    let [(first_call_entry, first), (_, second)] = calls.as_slice() else { panic!("two writes: {calls:?}") };
    let cp = all
        .iter()
        .find_map(|e| match &e.kind {
            EntryKind::WorkspaceCheckpoint { id, cause: CheckpointCause::ToolResult(c), .. } if c == first => Some(*id),
            _ => None,
        })
        .expect("the first write was checkpointed");
    let op = aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty());
    s.rewind(&op, cp, RewindMode::Both).await.unwrap();
    assert!(std::fs::read_to_string(root.join("notes.txt")).unwrap().contains("pineapple"));

    // Through the loop: the scripted model's step asserts the search result reached it.
    assert_eq!(s.prompt("find the fruit", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.remaining(), 0);
    let search_call = log
        .read_from(None)
        .into_iter()
        .find_map(|e| match e.kind {
            EntryKind::ToolCall { call, tool, .. } if tool == "session_search" => Some(call),
            _ => None,
        })
        .unwrap();
    // The PreRewind checkpoint extended the old branch past `before_rewind`; its leaf is the other head.
    let old_head = log.heads().into_iter().find(|h| Some(*h) != log.head() && *h >= before_rewind).expect("the abandoned branch is a head");

    // Hits and misses; the rewound-away branch is not on the current one.
    let hit = call(&s, "session_search", "probe", json!({"query": "PINEAPPLE", "kinds": ["tool_call"]})).await;
    assert!(hit_ids(&hit).contains(&first_call_entry.0), "{}", text(&hit));
    assert!(text(&hit).contains("[redacted]") && !text(&hit).contains(SECRET), "{}", text(&hit));
    let own = call(&s, "session_search", &search_call.0, json!({"query": "pineapple", "kinds": ["tool_call"]})).await;
    assert_eq!(hit_ids(&own), vec![first_call_entry.0], "the calling call never matches itself");
    let other = call(&s, "session_search", "probe", json!({"query": "pineapple", "kinds": ["tool_call"]})).await;
    assert_eq!(hit_ids(&other).len(), 2, "a different caller sees the search call too: {}", text(&other));
    let miss = call(&s, "session_search", "probe", json!({"query": "mango"})).await;
    assert!(hit_ids(&miss).is_empty() && text(&miss).contains("No entries match"), "{}", text(&miss));
    let old = call(&s, "session_search", "probe", json!({"query": "mango", "branch": old_head.0})).await;
    assert!(!hit_ids(&old).is_empty(), "the abandoned branch still holds it: {}", text(&old));
    let secret = call(&s, "session_search", "probe", json!({"query": SECRET, "kinds": ["tool", "config_resolved"]})).await;
    assert!(hit_ids(&secret).is_empty(), "a secret is never searchable: {}", text(&secret));
    let regex = call(&s, "session_search", "probe", json!({"query": "pine(apple", "regex": true})).await;
    assert_eq!(err_code(&regex), "BAD_REGEX");
    let ok_regex = call(&s, "session_search", "probe", json!({"query": "pine\\w+", "regex": true, "tool": "write"})).await;
    assert!(!hit_ids(&ok_regex).is_empty(), "{}", text(&ok_regex));

    // Trace across the fork: the first call's checkpoint is the fork point the rewind came back to.
    let t1 = text(&call(&s, "session_trace", "probe", json!({"call": first.0})).await);
    for needle in ["tool_call", "op_started", "op_done", "tool_result", "workspace_checkpoint", "fork point", "rewound the workspace back"]
    {
        assert!(t1.contains(needle), "{needle} missing from:\n{t1}");
    }
    let t2 = text(&call(&s, "session_trace", "probe", json!({"call": second.0})).await);
    assert!(t2.contains("off-branch") && t2.contains("forked away before this call"), "{t2}");
    assert!(t1.contains("[current branch]"), "{t1}");

    // event_get of secret-bearing entries: the resolver's digest and the plaintext in tool args.
    let cfg_entry = log.read_from(None).into_iter().find(|e| matches!(e.kind, EntryKind::ConfigResolved { .. })).unwrap();
    let got = text(&call(&s, "event_get", "probe", json!({"id": cfg_entry.id.0})).await);
    let digest = Hash::of(SECRET.as_bytes()).to_hex();
    assert!(got.contains("[secret]") && !got.contains(&digest) && !got.contains(SECRET), "{got}");
    let got = text(&call(&s, "event_get", "probe", json!({"id": first_call_entry.0})).await);
    assert!(got.contains("[redacted]") && !got.contains(SECRET) && got.contains("current branch"), "{got}");
    assert_eq!(err_code(&call(&s, "event_get", "probe", json!({"id": 999_999})).await), "NO_SUCH_ENTRY");

    // Caps: `session_query.max_hits = 5` bounds a broad search; event_list honours `limit`.
    let broad =
        call(&s, "session_search", "probe", json!({"query": "e", "kinds": ["tool", "message", "config_resolved", "policy_epoch"]})).await;
    let v = value(&broad);
    assert_eq!(v["hits"].as_array().unwrap().len(), 5, "{}", text(&broad));
    assert!(v["total"].as_u64().unwrap() > 5 && v["capped"] == true);
    let list = call(&s, "event_list", "probe", json!({"kinds": ["tool_call"], "limit": 1})).await;
    assert_eq!(value(&list)["entries"].as_array().unwrap().len(), 1);
    assert!(text(&list).contains("earlier; page back"), "{}", text(&list));
}
