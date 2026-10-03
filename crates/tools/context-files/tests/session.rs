//! The instruction files in a live session's requests: AGENTS.md is in the first request the
//! model sees, and after the model rewrites it with the fs tools, the next request carries the
//! new text.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::{Arc, Mutex, PoisonError};

use aho_approval::NeverApproval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{AdapterInfo, Llm, LlmError, LlmRoute, Request, Response, Sink};
use aho_log::{EntryKind, LogStore, MemoryLog, Transition};
use aho_session::{Boot, Session};
use aho_types::{BoxFuture, ContentBlock, Epoch, Role, SessionId};
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

/// The scripted model, recording every request's user messages as text.
struct Capture {
    inner: ScriptedLlm,
    seen: Mutex<Vec<Vec<String>>>,
}

impl Llm for Capture {
    fn info(&self) -> AdapterInfo {
        self.inner.info()
    }
    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        let users = req
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| {
                m.content.iter().filter_map(|b| if let ContentBlock::Text { text } = b { Some(text.as_str()) } else { None }).collect()
            })
            .collect();
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).push(users);
        self.inner.stream(req, sink)
    }
}

/// The newest instruction-files snapshot a request carried.
fn instructions(users: &[String]) -> &str {
    users.iter().rev().find(|t| t.contains("<repo-instructions")).map(String::as_str).unwrap_or("")
}

#[tokio::test]
async fn agents_md_reaches_the_model_and_an_edit_reaches_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("AGENTS.md"), "Indent with tabs.\n").unwrap();

    let comp =
        Composition::new("ctx").provide::<world_host::HostWorld>().consume::<tool_fs::ToolFs>().consume::<context_files::ContextFiles>();
    let mut schema = Schema::default();
    comp.config_keys().into_iter().for_each(|k| schema.add(k));
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n",
        root.display(),
        home.join("shadow").display(),
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let allowed = aho_policy::preset("workspace").unwrap().cut(&aho_cap::BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();

    let steps = vec![
        Step::tool_call("read", json!({"path": "AGENTS.md"})).with_stop(ScriptedStop::ToolUse),
        Step::tool_call("write", json!({"path": "AGENTS.md", "content": "Indent with four spaces.\n"})).with_stop(ScriptedStop::ToolUse),
        Step::text("noted").with_stop(ScriptedStop::EndTurn),
    ];
    let llm = Arc::new(Capture { inner: ScriptedLlm::from_steps(steps), seen: Mutex::new(Vec::new()) });
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let s = Session::boot(Boot {
        id: SessionId(5),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: plan.allowed, preset: plan.preset.clone(), approval_ref: None }],
        log,
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: root.display().to_string(),
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();

    assert_eq!(s.prompt("switch the repo to spaces", None).await.unwrap(), Transition::Completed);
    let seen = llm.seen.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(seen.len(), 3, "{seen:#?}");
    let first = instructions(&seen[0]);
    assert!(first.contains("<repo-instructions path=\"AGENTS.md\">\nIndent with tabs.\n</repo-instructions>"), "{seen:#?}");
    assert_eq!(std::fs::read_to_string(root.join("AGENTS.md")).unwrap(), "Indent with four spaces.\n", "the write landed");
    let last = instructions(&seen[2]);
    assert!(last.contains("Indent with four spaces.") && !last.contains("tabs"), "{seen:#?}");
}
