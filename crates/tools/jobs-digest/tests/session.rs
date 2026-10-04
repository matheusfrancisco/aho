//! `jobs-digest` in a live session: two background jobs that finish close together are held,
//! then delivered at one boundary as one digest notice plus a pointer, each op keeping its own
//! `JobDelivered` (status, `OpDone`) and the log replaying to the requests the model received.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aho_approval::NeverApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{AdapterInfo, LateResults, Llm, LlmError, LlmRoute, Request, Response, Sink};
use aho_log::{Entry, EntryKind, MemoryLog, OpKind, OpStatus, Transition};
use aho_session::{Boot, Session, SessionEvent, verify_replay};
use aho_types::*;
use llm_scripted::{ScriptedLlm, ScriptedStop, Step};
use serde_json::json;

struct JobTools;
static JOB_TOOLS: &[ToolDecl] =
    &[ToolDecl::new("sleep", "Sleeps.", Consequence::ReadOnly).safe().schema(schema).detachable().op(OpKind::Timer, false, false)];

fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": {"ms": {"type": "integer"}, "text": {"type": "string"}}}))
}

impl Capability for JobTools {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(JobTools)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.tool(
            &JOB_TOOLS[0],
            Arc::new(|req: ExecRequest| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(req.view.args["ms"].as_u64().unwrap_or(0))).await;
                    ExecOutcome::text(req.view.args["text"].as_str().unwrap_or("").to_string())
                })
            }),
        );
    }
}
declare_capability!(JobTools, "job-tools", kind = Kind::Tool, provides = [], requires = [], tools = JOB_TOOLS);

struct Recording(ScriptedLlm, Mutex<Vec<Request>>);

impl Llm for Recording {
    fn info(&self) -> AdapterInfo {
        self.0.info()
    }
    fn stream<'a>(&'a self, req: Request, sink: Sink<'a>) -> BoxFuture<'a, Result<Response, LlmError>> {
        self.1.lock().unwrap_or_else(PoisonError::into_inner).push(req.clone());
        self.0.stream(req, sink)
    }
}

async fn boot(steps: Vec<Step>, dir: &std::path::Path) -> (Arc<Session>, Arc<Recording>) {
    let (root, home) = (dir.join("ws"), dir.join("home"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::process::Command::new("git").arg("init").arg("-q").current_dir(&root).status().unwrap();
    let comp =
        Composition::new("digest-test").provide::<world_host::HostWorld>().consume::<JobTools>().provide::<jobs_digest::JobsDigest>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let text = format!(
        "[model]\nprovider = \"scripted\"\nname = \"script\"\n[workspace]\nroot = \"{}\"\nshadow_dir = \"{}\"\n[policy]\npreset = \"workspace\"\n[jobs_digest]\nwindow_ms = 400\n",
        root.display(),
        home.join("shadow").display()
    );
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let allowed = aho_policy::preset("workspace").unwrap().cut(&BoundWorld::host());
    let plan = aho_cap::plan(&comp.manifests(), &loadout, &env, &PolicyInput { preset: "workspace".into(), allowed }, vec![]).unwrap();
    let llm = Arc::new(Recording(ScriptedLlm::from_steps(steps), Mutex::default()));
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm.clone())));
    let s = Session::boot(Boot {
        id: SessionId(13),
        composition: &comp,
        plan: &plan,
        loadout: &loadout,
        bootstrap: vec![EntryKind::PolicyEpoch { epoch: Epoch(1), allowed: plan.allowed, preset: plan.preset.clone(), approval_ref: None }],
        log: Arc::new(MemoryLog::new()),
        approval: Arc::new(NeverApproval),
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: root.display().to_string(),
        home: home.display().to_string(),
        spill_dir: home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    (s, llm)
}

fn entries(s: &Session) -> Vec<Entry> {
    s.log().read_branch(s.log().head().unwrap()).unwrap()
}

#[tokio::test]
async fn jobs_finishing_together_reach_the_model_as_one_digest_and_each_keeps_its_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let (s, llm) = boot(
        vec![
            Step::tool_call("sleep", json!({"ms": 100, "text": "alpha-out", "background": true}))
                .with_call(None, "sleep", json!({"ms": 150, "text": "beta-out", "background": true}))
                .with_stop(ScriptedStop::ToolUse),
            Step::text("both started"),
            Step::text("waiting"),
            Step::text("both in").expect_user("in the digest delivered with job"),
        ],
        dir.path(),
    )
    .await;
    let mut events = s.subscribe();
    assert_eq!(s.prompt("start two", None).await.unwrap(), Transition::Completed);
    let mut finished = Vec::new();
    while finished.len() < 2 {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.expect("both jobs finish while idle");
        if let Ok(SessionEvent::JobFinished { op }) = ev {
            finished.push(op);
        }
    }

    // The next turn offers both: the digest holds them for its window, so nothing lands.
    assert_eq!(s.prompt("status?", None).await.unwrap(), Transition::Completed);
    assert!(!entries(&s).iter().any(|e| matches!(e.kind, EntryKind::JobDelivered { .. })), "held for the window");
    // The window ends: the batch is announced again and delivered at the next turn's start.
    tokio::time::timeout(Duration::from_secs(5), async { while !matches!(events.recv().await, Ok(SessionEvent::JobFinished { .. })) {} })
        .await
        .expect("the held batch is announced when the window ends");
    assert_eq!(s.prompt("and now?", None).await.unwrap(), Transition::Completed);
    assert_eq!(llm.0.remaining(), 0);

    let es = entries(&s);
    let got: Vec<(usize, OpId, OpStatus, String)> = es
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match &e.kind {
            EntryKind::JobDelivered { op, status, content, .. } => {
                Some((i, *op, status.clone(), content.iter().filter_map(|b| b.as_text()).collect::<String>()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(got.len(), 2, "one JobDelivered per op");
    let (lead, other) = (&got[0], &got[1]);
    assert!(lead.3.starts_with("2 background jobs finished within 400 ms"), "{}", lead.3);
    assert!(lead.3.contains("alpha-out") && lead.3.contains("beta-out"), "the lead carries both outcomes: {}", lead.3);
    assert_eq!(other.3, format!("Its outcome is in the digest delivered with job {}.", lead.1.0));
    for (i, op, status, _) in &got {
        assert_eq!(status, &OpStatus::Completed { code: 0 });
        assert!(matches!(es[i + 1].kind, EntryKind::OpDone { op: o, .. } if o == *op), "each op closes after its own delivery");
    }
    assert_eq!(other.0, lead.0 + 2, "delivered together, at one boundary");

    let reqs = llm.1.lock().unwrap_or_else(PoisonError::into_inner);
    let shown: Vec<String> = reqs.last().unwrap().messages.iter().map(Message::text).filter(|t| t.starts_with("Background job")).collect();
    assert_eq!(shown.len(), 2);
    assert!(shown[0].ends_with(&lead.3) && shown[1].ends_with(&other.3), "the model saw what the log holds");
    drop(reqs);
    assert!(verify_replay(&es, &Default::default(), LateResults::SameCallId).unwrap() >= 4);
}
