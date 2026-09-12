//! Process-level end to end, no harness: this binary spawns itself as the agent. The child runs
//! aho's ACP surface (`surface_acp::serve_stdio`) on its real stdin/stdout; the parent drives it
//! through `subagent-acp`'s command transport.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test binary: setup failures may panic
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod common;

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_policy::SandboxPolicy;
use aho_subagent::{Mode, SpawnRequest, SubagentProvider};
use aho_types::*;
use common::ChildAho;
use llm_scripted::Step;
use serde_json::json;
use subagent_acp::{AcpProvider, AgentCommand, Launch, ProviderSettings};
use surface_acp::AcpOptions;

const AGENT_ENV: &str = "AHO_ACP_STDIO_TEST_AGENT";

fn main() -> ExitCode {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    if std::env::var_os(AGENT_ENV).is_some() {
        let child = ChildAho::new(vec![
            Step::text("Checking.").with_call(Some("n1"), "note", json!({})),
            Step::text("Answered over stdio.").expect_tool_result("noted"),
        ]);
        return match rt.block_on(surface_acp::serve_stdio(child, AcpOptions::default())) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    rt.block_on(parent());
    println!("stdio: aho drove aho over ACP on real pipes: ok");
    ExitCode::SUCCESS
}

async fn parent() {
    let dir = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let me = std::env::current_exe().unwrap().display().to_string();
    let settings = ProviderSettings {
        agents: BTreeMap::from([(
            "self".to_string(),
            Launch::Command(AgentCommand { program: me, args: vec![], env: vec![(AGENT_ENV.into(), "1".into())] }),
        )]),
        broken: BTreeMap::new(),
        default: "self".into(),
        max_depth: 3,
        handshake: Duration::from_secs(20),
        secrets: vec![],
    };
    let provider = AcpProvider::new(settings, Arc::new(NeverApproval), log.clone());
    let d = dir.path().display().to_string();
    let policy = SandboxPolicy::new(ConsequenceSet::ALL, aho_cap::BoundWorld::host(), d.clone(), d, Epoch(1));
    let req = SpawnRequest {
        label: "mirror".into(),
        prompt: "look".into(),
        mode: Mode::OneShot,
        persona: None,
        tool_filter: vec![],
        model: None,
        decl_cut: ConsequenceSet::ALL,
        is_async: false,
        bubble: false,
        fork_at: None,
        depth: 0,
    };
    let child = provider.spawn(req, policy).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(60), provider.wait(child)).await.expect("the child answers within 60 s").unwrap();
    assert_eq!((r.stop, r.last_message.as_deref(), r.diagnostic.as_deref()), (StopReason::EndTurn, Some("Answered over stdio."), None));
    let names: Vec<String> = log
        .read_from(None)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::Custom { ns, name, .. } if ns == "Subagent" => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["Attached", "Sent", "Message", "ToolUse", "ToolResult", "Message", "TurnEnd"]);
}
