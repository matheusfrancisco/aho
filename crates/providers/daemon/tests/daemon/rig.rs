//! A real `Session` over an empty composition (host world) and a scripted model.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aho_approval::Approval;
use aho_cap::{Composition, Env, Os, PolicyInput, Registry};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_llm::LlmRoute;
use aho_log::{EntryKind, LogStore};
use aho_session::{Boot, Session};
use aho_types::*;
use daemon::SessionHost;
use llm_scripted::ScriptedLlm;

pub struct Rig {
    pub comp: Composition,
    pub plan: aho_cap::Plan,
    pub loadout: aho_config::Loadout,
    pub root: PathBuf,
    pub home: PathBuf,
    pub dir: tempfile::TempDir,
}

pub fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let comp = Composition::new("t");
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let inputs = ResolveInputs {
        sets: vec![("model.provider".into(), "scripted".into()), ("model.name".into(), "script".into())],
        ..ResolveInputs::default()
    };
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let world = aho_cap::BoundWorld::host();
    let allowed = aho_policy::preset("workspace").unwrap().cut(&world);
    let plan = aho_cap::plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed },
        vec![],
    )
    .unwrap();
    Rig { comp, plan, loadout, root, home, dir }
}

pub async fn boot(
    r: &Rig,
    id: SessionId,
    llm: Arc<ScriptedLlm>,
    log: Arc<dyn LogStore>,
    approval: Arc<dyn Approval>,
) -> Result<Arc<Session>, String> {
    let mut seed = Registry::default();
    seed.services.insert("llm:scripted".into(), Arc::new(LlmRoute(llm)));
    Session::boot(Boot {
        id,
        composition: &r.comp,
        plan: &r.plan,
        loadout: &r.loadout,
        bootstrap: vec![EntryKind::PolicyEpoch {
            epoch: Epoch(1),
            allowed: r.plan.allowed,
            preset: r.plan.preset.clone(),
            approval_ref: None,
        }],
        log,
        approval,
        operator: aho_policy::Operator::authenticate(aho_policy::OperatorProof::local_tty()),
        workspace: r.root.display().to_string(),
        home: r.home.display().to_string(),
        spill_dir: r.home.join("spill"),
        seed,
    })
    .await
    .map_err(|e| e.to_string())
}

/// Stands in for the app: sessions under `dir`, all booted with one scripted model. `journal`
/// records every hook the daemon calls (`boot <id>`, `closing <id>`, `before_exec`,
/// `exec_failed`, `call <method>`), in order.
pub struct Host {
    pub rig: Rig,
    pub llm: Arc<ScriptedLlm>,
    pub dir: PathBuf,
    pub journal: Mutex<Vec<String>>,
}

impl Host {
    pub fn new(rig: Rig, llm: Arc<ScriptedLlm>, dir: PathBuf) -> Host {
        Host { rig, llm, dir, journal: Mutex::new(Vec::new()) }
    }
    pub fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn note(&self, what: String) {
        self.journal.lock().unwrap_or_else(|p| p.into_inner()).push(what);
    }
}

impl SessionHost for Host {
    fn sessions_dir(&self) -> PathBuf {
        self.dir.clone()
    }
    fn boot(&self, id: SessionId, log: Arc<dyn LogStore>, approval: Arc<dyn Approval>) -> BoxFuture<'_, Result<Arc<Session>, String>> {
        self.note(format!("boot {}", id.0));
        Box::pin(boot(&self.rig, id, self.llm.clone(), log, approval))
    }
    /// `echo` answers its params with the session id; any other method is refused.
    fn call<'a>(&'a self, session: &'a Arc<Session>, method: &'a str, params: Value) -> BoxFuture<'a, Result<Value, String>> {
        self.note(format!("call {method}"));
        Box::pin(async move {
            match method {
                "echo" => Ok(serde_json::json!({ "session": session.id, "params": params })),
                other => Err(format!("no host method `{other}`")),
            }
        })
    }
    fn closing(&self, id: SessionId) -> BoxFuture<'_, Vec<String>> {
        self.note(format!("closing {}", id.0));
        Box::pin(async { Vec::new() })
    }
    fn before_exec(&self) -> BoxFuture<'_, Vec<String>> {
        self.note("before_exec".into());
        Box::pin(async { vec!["driver scheduler: stopped late".to_string()] })
    }
    fn exec_failed(&self) -> BoxFuture<'_, Vec<String>> {
        self.note("exec_failed".into());
        Box::pin(async { Vec::new() })
    }
}
