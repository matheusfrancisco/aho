//! The child side shared by the end-to-end tests: a real aho Session behind `surface-acp`, with a
//! scripted model and test tools (`note`, a network `fetch` that needs approval, a cancel-aware
//! `slow`).
#![allow(dead_code)] // each test binary uses its own subset
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aho_approval::ChannelApproval;
use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{EntryKind, MemoryLog, OpKind};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session};
use aho_types::*;
use llm_scripted::{ScriptedLlm, Step};
use subagent_acp::{Connector, Duplex};
use surface_acp::{AcpOptions, Opened, SessionFactory, SessionRequest};

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
    ToolDecl::new("note", "Take a note.", Consequence::ReadOnly).safe(),
    ToolDecl::new("fetch", "Fetch a URL.", Consequence::NetworkEgress),
    ToolDecl::new("slow", "Sleeps until cancelled.", Consequence::ReadOnly).safe().op(OpKind::Timer, false, false),
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
        r.tool(&TOOLS[0], Arc::new(|_| Box::pin(async { ExecOutcome::text("noted") })));
        r.tool(&TOOLS[1], Arc::new(|_| Box::pin(async { ExecOutcome::text("fetched") })));
        r.tool(
            &TOOLS[2],
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
    }
}
declare_capability!(TestTools, "acp-e2e-tools", kind = Kind::Tool, provides = [], requires = [], tools = TOOLS);

/// Boots one child Session per ACP `session/new`.
pub struct ChildAho {
    comp: Composition,
    plan: Plan,
    loadout: aho_config::Loadout,
    pub llm: Arc<ScriptedLlm>,
    dir: tempfile::TempDir,
    pub sessions: Mutex<Vec<Arc<Session>>>,
}

impl ChildAho {
    pub fn new(steps: Vec<Step>) -> Arc<ChildAho> {
        let comp = Composition::new("acp-e2e-child").consume::<TestTools>();
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
        Arc::new(ChildAho {
            comp,
            plan,
            loadout,
            llm: Arc::new(ScriptedLlm::from_steps(steps)),
            dir: tempfile::tempdir().unwrap(),
            sessions: Mutex::new(vec![]),
        })
    }
}

impl SessionFactory for ChildAho {
    fn open(&self, req: SessionRequest) -> BoxFuture<'_, Result<Opened, String>> {
        Box::pin(async move {
            let mut seed = Registry::default();
            seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(self.llm.clone())));
            seed.handles.insert(HandleKind::Egress(Route::Sandbox), (CapabilityId { name: "test", major: 0 }, Arc::new(Egress)));
            let (approval, rx) = ChannelApproval::new(4);
            let session = Session::boot(Boot {
                id: SessionId(0xc41d),
                composition: &self.comp,
                plan: &self.plan,
                loadout: &self.loadout,
                bootstrap: vec![EntryKind::PolicyEpoch {
                    epoch: Epoch(1),
                    allowed: self.plan.allowed,
                    preset: "workspace".into(),
                    approval_ref: None,
                }],
                log: Arc::new(MemoryLog::new()),
                approval: Arc::new(approval),
                operator: Operator::authenticate(OperatorProof::local_tty()),
                workspace: req.cwd.display().to_string(),
                home: self.dir.path().display().to_string(),
                spill_dir: self.dir.path().join("spill"),
                seed,
            })
            .await
            .map_err(|e| e.to_string())?;
            self.sessions.lock().unwrap().push(session.clone());
            Ok(Opened { session, approvals: Some(rx) })
        })
    }
}

/// An in-process "agent command": each connection serves a fresh surface-acp over a duplex.
pub fn connector(child: Arc<ChildAho>) -> Connector {
    Arc::new(move |_cwd| {
        let child = child.clone();
        Box::pin(async move {
            let (client, server) = tokio::io::duplex(1 << 20);
            let (sr, sw) = tokio::io::split(server);
            tokio::spawn(surface_acp::serve_io(sr, sw, child, AcpOptions::default()));
            let (cr, cw) = tokio::io::split(client);
            Ok(Duplex { reader: Box::new(cr), writer: Box::new(cw) })
        })
    })
}
