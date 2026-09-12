//! The command transport against real child processes (`sh` scripts speaking ACP line by line),
//! and the config → capability path.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::BTreeMap;
use std::sync::Arc;

use aho_approval::{ApprovalHandle, NeverApproval};
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_policy::SandboxPolicy;
use aho_subagent::{Mode, SpawnRequest, SubagentProvider};
use aho_types::*;
use serde_json::json;
use subagent_acp::{AcpConfig, AcpDeps, AcpProvider, AgentCommand, Launch, ProviderSettings, SubagentAcp, resolve_program};

fn sh_available() -> bool {
    if resolve_program("sh").is_none() {
        eprintln!("skipped: no `sh` on PATH to run the scripted ACP agent");
        return false;
    }
    true
}

fn provider(agents: Vec<(&str, AgentCommand)>, log: Arc<dyn LogStore>) -> AcpProvider {
    let settings = ProviderSettings {
        agents: agents.into_iter().map(|(n, a)| (n.to_string(), Launch::Command(a))).collect(),
        broken: BTreeMap::new(),
        default: "sh".into(),
        max_depth: 3,
        handshake: std::time::Duration::from_secs(10),
        secrets: vec!["s3cret-value".into()],
    };
    AcpProvider::new(settings, Arc::new(NeverApproval), log)
}

fn sh(script: &str, env: Vec<(String, String)>) -> AgentCommand {
    AgentCommand { program: "sh".into(), args: vec!["-c".into(), script.into()], env }
}

fn policy(dir: &std::path::Path, allowed: ConsequenceSet) -> SandboxPolicy {
    SandboxPolicy::new(allowed, BoundWorld::host(), dir.display().to_string(), dir.display().to_string(), Epoch(1))
}

fn request(prompt: &str) -> SpawnRequest {
    SpawnRequest {
        label: "helper".into(),
        prompt: prompt.into(),
        mode: Mode::OneShot,
        persona: None,
        tool_filter: vec![],
        model: None,
        decl_cut: ConsequenceSet::ALL,
        is_async: false,
        bubble: false,
        fork_at: None,
        depth: 0,
    }
}

fn mirrored(log: &dyn LogStore) -> Vec<(String, Value)> {
    log.read_from(None)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::Custom { ns, name, body } if ns == "Subagent" => Some((name, body)),
            _ => None,
        })
        .collect()
}

const AGENT: &str = r#"
read -r l; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"sh-agent","version":"0"}}}'
read -r l; case "$l" in *'"cwd"'*) ;; *) echo "session/new without cwd: $l" >&2; exit 2;; esac
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"sh-1"}}'
read -r l; case "$l" in *'"list the files"'*) ;; *) echo "unexpected prompt: $l" >&2; exit 2;; esac
printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sh-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"in %s with %s"}}}}\n' "$PWD" "$FOO"
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"other","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"not for you"}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sh-1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"List files","kind":"read","status":"pending","rawInput":{"path":"."}}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sh-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"a.txt"}}]}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sh-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Found a.txt."}}}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
read -r l
"#;

#[tokio::test]
async fn a_child_process_speaks_acp_over_its_pipes_and_is_mirrored_into_the_parent() {
    if !sh_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(vec![("sh", sh(AGENT, vec![("FOO".into(), "bar".into())]))], log.clone());
    let child = p.spawn(request("list the files"), policy(dir.path(), ConsequenceSet::ALL)).await.unwrap();
    assert_eq!(p.acp_session(child).as_deref(), Some("sh-1"));
    let info = p.child_info(child).unwrap();
    assert_eq!((info.provider.as_str(), info.depth, info.label.as_str()), ("subagent-acp", 1, "helper"));
    let r = p.wait(child).await.unwrap();
    assert_eq!((r.stop, r.last_message.as_deref(), r.diagnostic.as_deref()), (StopReason::EndTurn, Some("Found a.txt."), None));
    assert!(p.child_info(child).is_none(), "a one-shot child is gone after wait");

    let m = mirrored(&*log);
    let names: Vec<&str> = m.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["Attached", "Sent", "Message", "ToolUse", "ToolResult", "Message", "TurnEnd"], "{m:#?}");
    for (_, body) in &m {
        assert_eq!((body["child"].clone(), body["provider"].clone()), (json!(child.0.to_string()), json!("subagent-acp")));
    }
    assert_eq!((m[0].1["session"].clone(), m[0].1["agent_info"]["name"].clone()), (json!("sh-1"), json!("sh-agent")));
    assert_eq!(m[1].1["text"], "list the files");
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(
        m[2].1["text"],
        format!("in {} with bar", cwd.display()),
        "runs in the child's workspace with its env; other sessions' updates are ignored"
    );
    assert_eq!((m[3].1["id"].clone(), m[3].1["kind"].clone(), m[3].1["input"].clone()), (json!("t1"), json!("read"), json!({"path": "."})));
    assert_eq!((m[4].1["is_error"].clone(), m[4].1["content"].clone()), (json!(false), json!("a.txt")));
    assert_eq!((m[6].1["stop"].clone(), m[6].1["last_message"].clone()), (json!("end_turn"), json!("Found a.txt.")));
}

#[tokio::test]
async fn an_agent_that_dies_fails_the_spawn_with_its_stderr_and_secrets_redacted() {
    if !sh_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(vec![("sh", sh("echo 'boom: key s3cret-value rejected' >&2; exit 3", vec![]))], log);
    let e = p.spawn(request("hi"), policy(dir.path(), ConsequenceSet::ALL)).await.unwrap_err();
    let msg = format!("{e:?}");
    assert!(msg.contains("initialize") && msg.contains("boom: key [redacted] rejected") && !msg.contains("s3cret-value"), "{msg}");
    assert!(p.children().is_empty());
}

#[tokio::test]
async fn an_agent_that_dies_mid_turn_ends_the_run_with_an_error() {
    if !sh_available() {
        return;
    }
    let script = r#"
read -r l; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
read -r l; printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}'
read -r l; echo 'out of memory' >&2; exit 9
"#;
    let dir = tempfile::tempdir().unwrap();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    let p = provider(vec![("sh", sh(script, vec![]))], log.clone());
    let child = p.spawn(request("hi"), policy(dir.path(), ConsequenceSet::ALL)).await.unwrap();
    let r = p.wait(child).await.unwrap();
    assert_eq!(r.stop, StopReason::Error);
    let d = r.diagnostic.unwrap();
    assert!(d.contains("closed") && d.contains("out of memory"), "{d}");
    let names: Vec<String> = mirrored(&*log).into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&"TurnEnd".to_string()) && names.contains(&"Error".to_string()), "{names:?}");
}

#[tokio::test]
async fn an_agent_on_another_protocol_version_is_stopped() {
    if !sh_available() {
        return;
    }
    let script = r#"read -r l; printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}}'; read -r l"#;
    let dir = tempfile::tempdir().unwrap();
    let p = provider(vec![("sh", sh(script, vec![]))], Arc::new(MemoryLog::new()));
    let e = format!("{:?}", p.spawn(request("hi"), policy(dir.path(), ConsequenceSet::ALL)).await.unwrap_err());
    assert!(e.contains("protocol version 2"), "{e}");
}

#[tokio::test]
async fn spawns_the_provider_cannot_honor_are_refused_before_anything_starts() {
    let dir = tempfile::tempdir().unwrap();
    let ghost = AgentCommand { program: "no-such-acp-agent-4242".into(), args: vec![], env: vec![] };
    let p = provider(vec![("sh", sh("exit 0", vec![])), ("ghost", ghost)], Arc::new(MemoryLog::new()));
    let all = policy(dir.path(), ConsequenceSet::ALL);

    let mut r = request("hi");
    r.model = Some("ghost".into());
    let e = format!("{:?}", p.spawn(r, all.clone()).await.unwrap_err());
    assert!(e.contains("Check `caps.subagent-acp.agents.ghost.command`"), "{e}");

    let mut r = request("hi");
    r.model = Some("nobody".into());
    let e = format!("{:?}", p.spawn(r, all.clone()).await.unwrap_err());
    assert!(e.contains("no ACP agent `nobody`") && e.contains("ghost, sh"), "{e}");

    let read_only = policy(dir.path(), ConsequenceSet::of(&[Consequence::ReadOnly]));
    let e = format!("{:?}", p.spawn(request("hi"), read_only).await.unwrap_err());
    assert!(e.contains("excludes SpawnsProcess"), "{e}");

    let mut r = request("hi");
    r.persona = Some("pirate".into());
    assert!(matches!(p.spawn(r, all.clone()).await, Err(aho_subagent::SubagentError::Unsupported(_))));
    let mut r = request("hi");
    r.fork_at = Some(EntryId(3));
    assert!(matches!(p.spawn(r, all.clone()).await, Err(aho_subagent::SubagentError::Unsupported(_))));
    let mut r = request("hi");
    r.depth = 9;
    assert!(matches!(p.spawn(r, all.clone()).await, Err(aho_subagent::SubagentError::DepthLimit(3))));
    let e = format!("{:?}", p.spawn(request("  "), all).await.unwrap_err());
    assert!(e.contains("empty"), "{e}");
    let f = p.features();
    assert!(!f.fork && !f.persona && !f.tool_filter);
}

// ───────────────────────────── config → capability ─────────────────────────────

fn loadout(toml: &str) -> aho_config::Loadout {
    let comp = Composition::new("acp-cfg").provide::<SubagentAcp>();
    let mut schema = Schema::default();
    comp.config_keys().into_iter().for_each(|k| schema.add(k));
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml.into() });
    resolve(&inputs, &schema, false).unwrap()
}

fn deps() -> AcpDeps {
    AcpDeps { approval: ApprovalHandle(Arc::new(NeverApproval)), log: Some(Arc::new(MemoryLog::new())) }
}

#[test]
fn agents_parse_from_config_with_their_errors_and_fixes() {
    let l = loadout(
        r#"
[caps.subagent-acp]
default = "claude"
max_depth = 2
[caps.subagent-acp.agents.claude]
command = "claude-code-acp --stdio 'two words'"
env = { MODE = "fast" }
[caps.subagent-acp.agents.gemini]
command = "gemini"
args = ["--experimental-acp"]
[caps.subagent-acp.agents.broken]
env = { KEY = "cred:gemini-key" }
"#,
    );
    let cfg = AcpConfig::from_loadout(&l).unwrap();
    assert_eq!((cfg.default.as_str(), cfg.max_depth), ("claude", 2));
    assert_eq!(
        cfg.agents["claude"].as_ref().unwrap(),
        &AgentCommand {
            program: "claude-code-acp".into(),
            args: vec!["--stdio".into(), "two words".into()],
            env: vec![("MODE".into(), "fast".into())]
        }
    );
    assert_eq!(cfg.agents["gemini"].as_ref().unwrap().args, ["--experimental-acp"]);
    let (what, fix) = cfg.agents["broken"].as_ref().unwrap_err();
    assert!(what.contains("caps.subagent-acp.agents.broken.command") && fix.starts_with("aho set"), "{what} / {fix}");
}

#[test]
fn building_checks_the_default_agent_and_its_program() {
    let missing = AcpConfig::from_loadout(&loadout("[caps.subagent-acp]\ndefault = \"x\"\n")).unwrap();
    let e = SubagentAcp::build(deps(), missing).err().unwrap().to_string();
    assert!(e.contains("names no agent"), "{e}");

    let absent = AcpConfig::from_loadout(&loadout(
        "[caps.subagent-acp]\ndefault = \"x\"\n[caps.subagent-acp.agents.x]\ncommand = \"no-such-acp-agent-4242 --acp\"\n",
    ))
    .unwrap();
    let e = SubagentAcp::build(deps(), absent).err().unwrap().to_string();
    assert!(e.contains("`no-such-acp-agent-4242` was not found"), "{e}");

    let no_log = AcpDeps { approval: ApprovalHandle(Arc::new(NeverApproval)), log: None };
    let cfg = AcpConfig::from_loadout(&loadout("[caps.subagent-acp]\ndefault = \"x\"\n[caps.subagent-acp.agents.x]\ncommand = \"sh\"\n"))
        .unwrap();
    assert!(SubagentAcp::build(no_log, cfg.clone()).err().unwrap().to_string().contains("aho.log"));
    if sh_available() {
        assert!(SubagentAcp::build(deps(), cfg).is_ok());
    }
}

#[test]
fn without_a_default_agent_the_capability_is_locked_with_a_fix() {
    let l = loadout("");
    let comp = Composition::new("acp-cfg").provide::<SubagentAcp>();
    let m = SubagentAcp::MANIFEST_;
    assert!(matches!(m.requires[0], Requirement::Setting { key: "caps.subagent-acp.default", pred: Pred::Set }));
    let plan = aho_cap::plan(
        &comp.manifests(),
        &l,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL },
        vec![],
    )
    .unwrap();
    let text = format!("{plan:?}");
    assert!(text.contains("aho set caps.subagent-acp.default"), "{text}");
}
