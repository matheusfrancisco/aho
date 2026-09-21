//! The ACP agent side against a raw JSON-RPC client over an in-memory duplex: real Sessions,
//! scripted model, test tools (a cancel-aware `slow`, a network `fetch` that needs approval, a
//! `todo_note` that writes a plan).
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic on setup failure

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_approval::ChannelApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{LogStore, MemoryLog, OpKind};
use aho_policy::{Operator, OperatorProof};
use aho_session::Boot;
use aho_types::*;
use llm_scripted::{ScriptedLlm, Step};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf};

use super::*;

// ───────────────────────────── test tools ─────────────────────────────

#[derive(Clone)]
struct Egress;
impl Handle for Egress {
    const KIND: HandleKind = HandleKind::Egress(Route::Sandbox);
}
struct ToolDeps;
impl Deps for ToolDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Egress(Route::Sandbox)];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        r.handle::<Egress>().map(|_| ToolDeps)
    }
}
static TOOLS: &[ToolDecl] = &[
    ToolDecl::new("slow", "Sleeps until cancelled.", Consequence::ReadOnly).safe().op(OpKind::Timer, false, false),
    ToolDecl::new("fetch", "Fetch a URL.", Consequence::NetworkEgress),
    ToolDecl::new("todo_note", "Write the plan.", Consequence::ReadOnly).safe(),
];
struct TestTools;
impl Capability for TestTools {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ToolDeps;
    type Config = ();
    fn build(_: ToolDeps, _: ()) -> Result<Self, BuildError> {
        Ok(TestTools)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(
            &TOOLS[0],
            Arc::new(|req: ExecRequest| {
                Box::pin(async move {
                    for _ in 0..1500 {
                        if req.cancel.is_cancelled() {
                            return ExecOutcome::Cancelled;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    ExecOutcome::text("slept")
                })
            }),
        );
        r.tool(&TOOLS[1], Arc::new(|_| Box::pin(async { ExecOutcome::text("fetched") })));
        r.tool(
            &TOOLS[2],
            Arc::new(|_| {
                Box::pin(async {
                    let todos =
                        json!([{"content": "read the code", "status": "completed"}, {"content": "write the fix", "status": "in_progress"}]);
                    ExecOutcome::Done {
                        value: json!({"events": [{"ns": "Todo", "name": "Written", "body": {"by": "agent:1", "todos": todos}}]}),
                        content: vec![ContentBlock::text("noted")],
                        meta: None,
                    }
                })
            }),
        );
    }
}
declare_capability!(TestTools, "acp-test-tools", kind = Kind::Tool, provides = [], requires = [], tools = TOOLS);

// ───────────────────────────── factory ─────────────────────────────

pub(crate) struct Factory {
    comp: Composition,
    plan: Plan,
    loadout: aho_config::Loadout,
    llm: Arc<ScriptedLlm>,
    dir: tempfile::TempDir,
    logs: Mutex<BTreeMap<String, Arc<dyn LogStore>>>,
    next: Mutex<u128>,
    pub(crate) opened: Mutex<Vec<SessionRequest>>,
}

impl Factory {
    fn new(steps: Vec<Step>) -> Arc<Factory> {
        let comp = Composition::new("acp-test").consume::<TestTools>();
        let mut schema = Schema::default();
        comp.config_keys().into_iter().for_each(|k| schema.add(k));
        let text = "[model]\nprovider = \"scripted\"\nname = \"script\"\n[tools]\ngrace_ms = 10000\nheartbeat_s = 60\n[policy]\npreset = \"workspace\"\n";
        let mut inputs = ResolveInputs::default();
        inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: text.into() });
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
        Arc::new(Factory {
            comp,
            plan,
            loadout,
            llm: Arc::new(ScriptedLlm::from_steps(steps)),
            dir: tempfile::tempdir().unwrap(),
            logs: Mutex::new(BTreeMap::new()),
            next: Mutex::new(0x100),
            opened: Mutex::new(Vec::new()),
        })
    }
}

impl SessionFactory for Factory {
    fn open(&self, req: SessionRequest) -> BoxFuture<'_, Result<Opened, String>> {
        Box::pin(async move {
            self.opened.lock().unwrap().push(req.clone());
            let (id, log, fresh) = match &req.load {
                Some(hex) => {
                    let log = self.logs.lock().unwrap().get(hex).cloned().ok_or_else(|| format!("no session {hex}"))?;
                    (SessionId(u128::from_str_radix(hex, 16).map_err(|e| e.to_string())?), log, false)
                }
                None => {
                    let mut n = self.next.lock().unwrap();
                    *n += 1;
                    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
                    self.logs.lock().unwrap().insert(format!("{:x}", *n), log.clone());
                    (SessionId(*n), log, true)
                }
            };
            let mut seed = Registry::default();
            seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(self.llm.clone())));
            seed.handles.insert(HandleKind::Egress(Route::Sandbox), (CapabilityId { name: "test", major: 0 }, Arc::new(Egress)));
            let (approval, rx) = ChannelApproval::new(4);
            let bootstrap = if fresh {
                vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: self.plan.allowed, preset: "workspace".into(), approval_ref: None }]
            } else {
                vec![]
            };
            let session = Session::boot(Boot {
                id,
                composition: &self.comp,
                plan: &self.plan,
                loadout: &self.loadout,
                bootstrap,
                log,
                approval: Arc::new(approval),
                operator: Operator::authenticate(OperatorProof::local_tty()),
                workspace: req.cwd.display().to_string(),
                home: self.dir.path().display().to_string(),
                spill_dir: self.dir.path().join("spill"),
                seed,
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(Opened { session, approvals: Some(rx) })
        })
    }
    fn supports_load(&self) -> bool {
        true
    }
}

// ───────────────────────────── raw client ─────────────────────────────

struct Client {
    w: WriteHalf<DuplexStream>,
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    next: i64,
}

impl Client {
    fn connect(f: &Arc<Factory>) -> (Client, tokio::task::JoinHandle<Result<(), AcpError>>) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (r, w) = tokio::io::split(b);
        let server = tokio::spawn(serve_io(r, w, f.clone(), AcpOptions::default()));
        let (rd, w) = tokio::io::split(a);
        (Client { w, lines: BufReader::new(rd).lines(), next: 1 }, server)
    }

    async fn send(&mut self, v: Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.w.write_all(s.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        id
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
            .await
            .expect("a frame within 20 s")
            .unwrap()
            .expect("open");
        serde_json::from_str(&line).unwrap()
    }

    /// Frames up to and including the response to `id`.
    async fn until_response(&mut self, id: i64) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let f = self.recv().await;
            let done = f.get("method").is_none() && f["id"] == id;
            seen.push(f);
            if done {
                return seen;
            }
        }
    }

    async fn until(&mut self, pred: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let f = self.recv().await;
            let done = pred(&f);
            seen.push(f);
            if done {
                return seen;
            }
        }
    }

    async fn initialize(&mut self) -> Value {
        let id = self.request("initialize", json!({"protocolVersion": 1, "clientInfo": {"name": "zed", "version": "1"}})).await;
        self.until_response(id).await.pop().unwrap()
    }

    async fn new_session(&mut self) -> String {
        let id = self.request("session/new", json!({"cwd": "/tmp/ws", "mcpServers": []})).await;
        let r = self.until_response(id).await.pop().unwrap();
        r["result"]["sessionId"].as_str().expect("a session id").to_string()
    }
}

fn updates(frames: &[Value]) -> Vec<&Value> {
    frames.iter().filter(|f| f["method"] == "session/update").map(|f| &f["params"]["update"]).collect()
}

fn kinds(frames: &[Value]) -> Vec<String> {
    updates(frames).iter().map(|u| u["sessionUpdate"].as_str().unwrap_or("?").to_string()).collect()
}

// ───────────────────────────── protocol ─────────────────────────────

#[tokio::test]
async fn initialize_negotiates_and_advertises_what_aho_speaks() {
    let f = Factory::new(vec![]);
    let (mut c, _server) = Client::connect(&f);
    let early = c.request("session/new", json!({"cwd": "/tmp/ws", "mcpServers": []})).await;
    let r = c.until_response(early).await.pop().unwrap();
    assert_eq!(r["error"]["code"], aho_acp::code::INVALID_REQUEST, "session calls before initialize are refused");

    let id = c.request("initialize", json!({"protocolVersion": 7})).await;
    let r = c.until_response(id).await.pop().unwrap();
    let res = &r["result"];
    assert_eq!(res["protocolVersion"], 1, "an unsupported version is answered with the latest supported");
    assert_eq!(res["agentCapabilities"]["loadSession"], true);
    assert_eq!(res["agentCapabilities"]["promptCapabilities"], json!({"image": false, "audio": false, "embeddedContext": true}));
    assert_eq!(res["agentCapabilities"]["mcpCapabilities"], json!({"http": false, "sse": false}));
    assert_eq!(res["authMethods"], json!([]));
    assert_eq!(res["agentInfo"]["name"], "aho");

    let m = c.request("session/set_mode", json!({"sessionId": "x", "modeId": "y"})).await;
    assert_eq!(c.until_response(m).await.pop().unwrap()["error"]["code"], aho_acp::code::METHOD_NOT_FOUND);
    let a = c.request("authenticate", json!({"methodId": "x"})).await;
    assert_eq!(c.until_response(a).await.pop().unwrap()["error"]["code"], aho_acp::code::INVALID_PARAMS);
    let rel = c.request("session/new", json!({"cwd": "relative/dir", "mcpServers": []})).await;
    assert_eq!(c.until_response(rel).await.pop().unwrap()["error"]["code"], aho_acp::code::INVALID_PARAMS);
    let p = c.request("session/prompt", json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "hi"}]})).await;
    assert_eq!(c.until_response(p).await.pop().unwrap()["error"]["code"], aho_acp::code::RESOURCE_NOT_FOUND);
    assert!(f.opened.lock().unwrap().is_empty(), "nothing was booted for refused requests");
}

#[tokio::test]
async fn a_prompt_streams_its_turn_and_answers_after_the_last_update() {
    let f = Factory::new(vec![
        Step::text("Looking.").with_thinking("check the plan").with_call(Some("c1"), "todo_note", json!({})),
        Step::text("Done.").expect_tool_result("noted"),
    ]);
    let (mut c, _server) = Client::connect(&f);
    c.initialize().await;
    let sid = c.new_session().await;
    assert_eq!(f.opened.lock().unwrap()[0].client.as_deref(), Some("zed"));
    let id = c
        .request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [
                {"type": "text", "text": "fix the bug"},
                {"type": "resource_link", "name": "main.rs", "uri": "file:///tmp/ws/main.rs"},
                {"type": "resource", "resource": {"uri": "file:///tmp/ws/notes.md", "text": "the bug is in parse()"}}
            ]}),
        )
        .await;
    let frames = c.until_response(id).await;
    let last = frames.last().unwrap();
    assert_eq!(last["result"], json!({"stopReason": "end_turn"}), "{last}");
    assert_eq!(
        kinds(&frames),
        ["agent_thought_chunk", "agent_message_chunk", "tool_call", "tool_call_update", "tool_call_update", "plan", "agent_message_chunk"],
        "{frames:#?}"
    );
    let u = updates(&frames);
    assert!(u.iter().all(|x| frames.iter().any(|f| f["params"]["sessionId"] == sid && &&f["params"]["update"] == x)));
    assert_eq!(u[1]["content"], json!({"type": "text", "text": "Looking."}));
    assert_eq!(u[0]["messageId"], u[1]["messageId"], "one model response is one message");
    assert_ne!(u[1]["messageId"], u[6]["messageId"], "the next response is a new message");
    assert_eq!((u[2]["toolCallId"].clone(), u[2]["kind"].clone(), u[2]["status"].clone()), (json!("c1"), json!("think"), json!("pending")));
    assert_eq!(u[3]["status"], "in_progress");
    assert_eq!(u[4]["status"], "completed");
    assert_eq!(u[4]["content"], json!([{"type": "content", "content": {"type": "text", "text": "noted"}}]));
    assert_eq!(
        u[5]["entries"],
        json!([{"content": "read the code", "priority": "medium", "status": "completed"}, {"content": "write the fix", "priority": "medium", "status": "in_progress"}])
    );
    assert_eq!(u[6]["content"]["text"], "Done.");

    // the prompt text carries the link and the inlined resource
    let first = f.logs.lock().unwrap().values().next().unwrap().clone();
    let user = first
        .read_from(None)
        .into_iter()
        .find_map(|e| if let EntryKind::UserMessage { text, .. } = e.kind { Some(text) } else { None })
        .unwrap();
    assert!(user.contains("[resource: main.rs <file:///tmp/ws/main.rs>]") && user.contains("the bug is in parse()"), "{user}");
}

#[tokio::test]
async fn approval_cards_become_permission_requests_and_the_answer_decides() {
    let f = Factory::new(vec![
        Step::tool_call("fetch", json!({"url": "https://example.com"})),
        Step::text("got it").expect_tool_result("fetched"),
        Step::tool_call("fetch", json!({"url": "https://example.org"})),
        Step::text("fine").expect_tool_result("rejected"),
    ]);
    let (mut c, _server) = Client::connect(&f);
    c.initialize().await;
    let sid = c.new_session().await;

    // allow once
    let id = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]})).await;
    let before = c.until(|f| f["method"] == "session/request_permission").await;
    let ask = before.last().unwrap().clone();
    let announced: Vec<&Value> =
        updates(&before).into_iter().filter(|u| u["sessionUpdate"] == "tool_call" && u["name"] == "fetch").collect();
    assert_eq!(announced.len(), 1, "the call is announced before permission is asked: {before:#?}");
    assert_eq!(ask["params"]["sessionId"], sid);
    assert_eq!(ask["params"]["toolCall"]["toolCallId"], announced[0]["toolCallId"]);
    assert_eq!(ask["params"]["toolCall"]["kind"], "fetch");
    assert_eq!(ask["params"]["toolCall"]["title"], "fetch: https://example.com");
    let options: Vec<(Value, Value)> =
        ask["params"]["options"].as_array().unwrap().iter().map(|o| (o["optionId"].clone(), o["kind"].clone())).collect();
    assert_eq!(
        options,
        [
            (json!("allow-once"), json!("allow_once")),
            (json!("allow-always"), json!("allow_always")),
            (json!("reject-once"), json!("reject_once"))
        ]
    );
    c.send(json!({"jsonrpc": "2.0", "id": ask["id"], "result": {"outcome": {"outcome": "selected", "optionId": "allow-once"}}})).await;
    let frames = c.until_response(id).await;
    assert_eq!(frames.last().unwrap()["result"]["stopReason"], "end_turn");
    let done = updates(&frames)
        .into_iter()
        .find(|u| u["toolCallId"] == announced[0]["toolCallId"] && u["status"] == "completed")
        .expect("the allowed call completed");
    assert_eq!(done["content"][0]["content"]["text"], "fetched");

    // reject
    let id = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "again"}]})).await;
    let ask = c.until(|f| f["method"] == "session/request_permission").await.pop().unwrap();
    c.send(json!({"jsonrpc": "2.0", "id": ask["id"], "result": {"outcome": {"outcome": "selected", "optionId": "reject-once"}}})).await;
    let frames = c.until_response(id).await;
    assert_eq!(frames.last().unwrap()["result"]["stopReason"], "end_turn");
    let failed = updates(&frames).into_iter().find(|u| u["status"] == "failed").expect("the rejected call failed");
    assert!(failed["content"][0]["content"]["text"].as_str().unwrap().contains("rejected"), "{failed}");
    assert_eq!(f.llm.remaining(), 0);
}

#[tokio::test]
async fn cancel_mid_turn_stops_the_tool_and_answers_cancelled() {
    let f = Factory::new(vec![
        Step::tool_call("slow", json!({})),
        Step::text("back again"),
        // the cancelled call's late result may land inside the next turn and earn one more request (16)
        Step::text("back again"),
    ]);
    let (mut c, _server) = Client::connect(&f);
    c.initialize().await;
    let sid = c.new_session().await;
    let id = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "sleep"}]})).await;
    let started = c.until(|f| f["params"]["update"]["status"] == "in_progress").await;
    let call = updates(&started).last().unwrap()["toolCallId"].clone();
    c.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})).await;
    let frames = c.until_response(id).await;
    assert_eq!(frames.last().unwrap()["result"], json!({"stopReason": "cancelled"}));
    let u = updates(&frames);
    assert!(u.iter().any(|x| x["toolCallId"] == call && x["status"] == "failed"), "the running call is reported over: {frames:#?}");
    assert!(!kinds(&frames).contains(&"agent_message_chunk".to_string()), "the model is not called again after the cancel");
    assert_eq!(f.llm.remaining(), 2);

    // the session takes the next prompt normally
    let next = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "x"}]})).await;
    let frames = c.until_response(next).await;
    assert_eq!(frames.last().unwrap()["result"]["stopReason"], "end_turn");
    assert!(updates(&frames).iter().any(|x| x["content"]["text"] == "back again"));
}

#[tokio::test]
async fn cancel_request_on_a_prompt_answers_request_cancelled_and_stops_the_turn() {
    let f = Factory::new(vec![Step::tool_call("slow", json!({})), Step::text("after"), Step::text("after")]);
    let (mut c, _server) = Client::connect(&f);
    c.initialize().await;
    let sid = c.new_session().await;
    let id = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "sleep"}]})).await;
    c.until(|f| f["params"]["update"]["status"] == "in_progress").await;
    c.send(json!({"jsonrpc": "2.0", "method": "$/cancel_request", "params": {"requestId": id}})).await;
    let r = c.until_response(id).await.pop().unwrap();
    assert_eq!(r["error"]["code"], aho_acp::code::REQUEST_CANCELLED);
    // dropping the prompt call aborted the turn it started
    let log = f.logs.lock().unwrap().values().next().unwrap().clone();
    let mut ended = false;
    for _ in 0..500 {
        ended = log.read_from(None).iter().any(|e| matches!(e.kind, EntryKind::TurnEnd { .. }));
        if ended {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ended, "the turn ended");
    assert_eq!(f.llm.remaining(), 2, "without another model call");
    let next = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "x"}]})).await;
    assert_eq!(c.until_response(next).await.pop().unwrap()["result"]["stopReason"], "end_turn");
}

#[tokio::test]
async fn load_replays_the_head_branch_before_answering() {
    // The session's policy notice is the last user text the model sees, so the script does not match on it.
    let f = Factory::new(vec![Step::text("hi there"), Step::text("welcome back")]);
    let sid = {
        let (mut c, server) = Client::connect(&f);
        c.initialize().await;
        let sid = c.new_session().await;
        let id = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "hello"}]})).await;
        c.until_response(id).await;
        drop(c);
        server.await.unwrap().unwrap();
        sid
    };
    let (mut c, _server) = Client::connect(&f);
    c.initialize().await;
    let id = c.request("session/load", json!({"sessionId": sid, "cwd": "/tmp/ws", "mcpServers": []})).await;
    let frames = c.until_response(id).await;
    assert_eq!(frames.last().unwrap()["result"], json!({}));
    let u = updates(&frames);
    assert_eq!(kinds(&frames), ["user_message_chunk", "agent_message_chunk"], "{frames:#?}");
    assert_eq!((u[0]["content"]["text"].clone(), u[1]["content"]["text"].clone()), (json!("hello"), json!("hi there")));
    assert_eq!(f.opened.lock().unwrap()[1].load.as_deref(), Some(sid.as_str()));

    let again = c.request("session/load", json!({"sessionId": sid, "cwd": "/tmp/ws", "mcpServers": []})).await;
    assert_eq!(c.until_response(again).await.pop().unwrap()["error"]["code"], aho_acp::code::INVALID_REQUEST);
    let id = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "again"}]})).await;
    let frames = c.until_response(id).await;
    assert_eq!(kinds(&frames), ["agent_message_chunk"], "live updates after the replay are not duplicated: {frames:#?}");
    assert_eq!(frames.last().unwrap()["result"]["stopReason"], "end_turn");
}

// ───────────────────────────── projections ─────────────────────────────

#[test]
fn prompts_take_text_links_and_text_resources_but_not_images() {
    use aho_acp::schema::ContentBlock as B;
    let blocks: Vec<B> = serde_json::from_value(json!([
        {"type": "text", "text": "look"},
        {"type": "resource", "resource": {"uri": "file:///a.bin", "blob": "AAAA", "mimeType": "application/octet-stream"}}
    ]))
    .unwrap();
    let t = map::prompt_text(&blocks).unwrap();
    assert!(t.starts_with("look\n") && t.contains("4 base64 characters, not included"), "{t}");
    let img: Vec<B> = serde_json::from_value(json!([{"type": "image", "data": "AA", "mimeType": "image/png"}])).unwrap();
    assert_eq!(map::prompt_text(&img).unwrap_err().code, aho_acp::code::INVALID_PARAMS);
    let empty: Vec<B> = serde_json::from_value(json!([{"type": "text", "text": "  "}])).unwrap();
    assert!(map::prompt_text(&empty).unwrap_err().message.contains("empty"));
}

#[test]
fn tool_kinds_follow_the_class_and_the_name() {
    use aho_acp::schema::ToolKind as K;
    assert_eq!(map::kind_of(Consequence::SpawnsProcess, "bash"), K::Execute);
    assert_eq!(map::kind_of(Consequence::NetworkEgress, "web_fetch"), K::Fetch);
    assert_eq!(map::kind_of(Consequence::WorkspaceWrite, "edit"), K::Edit);
    assert_eq!(map::kind_of(Consequence::WorkspaceWrite, "file_delete"), K::Delete);
    assert_eq!(map::kind_of(Consequence::WorkspaceWrite, "format"), K::Edit, "`rm` inside a word is not a delete");
    assert_eq!(map::kind_of(Consequence::ReadOnly, "grep"), K::Search);
    assert_eq!(map::kind_of(Consequence::ReadOnly, "read"), K::Read);
    assert_eq!(map::kind_of(Consequence::TalksToHuman, "ask_user_question"), K::Other);
    assert_eq!(map::title_of("bash", &json!({"command": "cargo   test\n --all"})), "bash: cargo test --all");
    assert_eq!(map::title_of("aho_why", &json!({"id": "x"})), "aho_why");
}

#[test]
fn stop_reasons_put_the_clients_cancel_first_and_errors_on_the_error_path() {
    use aho_acp::schema::StopReason as S;
    use aho_log::Transition as T;
    assert_eq!(map::stop_reason(&T::Completed, false, Some(StopReason::EndTurn)), Ok(S::EndTurn));
    assert_eq!(map::stop_reason(&T::Completed, false, Some(StopReason::MaxTokens)), Ok(S::MaxTokens));
    assert_eq!(map::stop_reason(&T::Completed, false, Some(StopReason::Refusal)), Ok(S::Refusal));
    assert_eq!(map::stop_reason(&T::MaxTurns, false, None), Ok(S::MaxTurnRequests));
    assert_eq!(map::stop_reason(&T::Aborted("model down".into()), true, None), Ok(S::Cancelled));
    assert!(map::stop_reason(&T::Aborted("model down".into()), false, None).unwrap_err().contains("model down"));
}

#[test]
fn client_mcp_servers_become_config_sets_that_parse_back_exactly() {
    let servers: Vec<aho_acp::schema::McpServer> = serde_json::from_value(json!([
        {"name": "fs", "command": "/opt/mcp fs", "args": ["--root", "true"], "env": [{"name": "TOKEN", "value": "3"}]}
    ]))
    .unwrap();
    let req = SessionRequest { cwd: "/tmp/ws".into(), mcp_servers: servers, load: None, client: None };
    let sets: BTreeMap<String, Value> = config_sets(&req).unwrap().into_iter().map(|(k, v)| (k, aho_config::parse_scalar(&v))).collect();
    assert_eq!(sets["workspace.root"], json!("/tmp/ws"));
    assert_eq!(sets["mcp.servers.fs.cmd"], json!("/opt/mcp fs"), "a command with a space stays one program path");
    assert_eq!(sets["mcp.servers.fs.args"], json!(["--root", "true"]));
    assert_eq!(sets["mcp.servers.fs.env.TOKEN"], json!("3"), "values stay strings, never coerced to numbers");
    let http: Vec<aho_acp::schema::McpServer> =
        serde_json::from_value(json!([{"type": "http", "name": "x", "url": "https://x", "headers": []}])).unwrap();
    assert!(map::mcp_sets(&http).unwrap_err().contains("stdio only"));
    let dotted: Vec<aho_acp::schema::McpServer> =
        serde_json::from_value(json!([{"name": "a.b", "command": "x", "args": [], "env": []}])).unwrap();
    assert!(map::mcp_sets(&dotted).unwrap_err().contains("config key"));
}
