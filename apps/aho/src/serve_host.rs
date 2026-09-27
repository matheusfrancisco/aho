//! Everything `aho serve` hosts next to each session: the boot from the project's config as it is
//! when the session opens, the schedule and workflow drivers (stopped when the daemon closes the
//! session or execs a reload, hosted again when that exec fails), and the methods clients reach
//! through `session.call`: `caps.*`, `session.fold`, `session.rewind`, `plan-mode.enter`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use aho_approval::Approval;
use aho_compose::Inputs;
use aho_log::LogStore;
use aho_session::Session;
use aho_types::*;
use serde_json::json;
use session_drive::{DriverSeed, Drivers};
use surface_rpc::wire;

use crate::boot::Host;
use crate::ctx::hex;

/// Host methods served over `session.call`.
pub const METHODS: &[&str] = &[
    "session.fold",
    "session.rewind",
    "plan-mode.enter",
    "caps.list",
    "caps.tree",
    "caps.explain",
    "caps.why",
    "caps.plan",
    "caps.set",
    "caps.enable",
    "caps.disable",
    "caps.allow",
    "caps.preset",
];

/// One hosted session's drivers: what they were seeded with, and the running set (`None` while
/// stopped for an exec).
struct Hosted {
    session: Weak<Session>,
    seed: DriverSeed,
    drivers: Option<Drivers>,
}

pub struct AppHost {
    pub comp: aho_cap::Composition,
    pub project: PathBuf,
    pub inputs: Inputs,
    pub dir: PathBuf,
    hosted: Mutex<BTreeMap<SessionId, Hosted>>,
}

impl AppHost {
    pub fn new(comp: aho_cap::Composition, project: PathBuf, inputs: Inputs, dir: PathBuf) -> AppHost {
        AppHost { comp, project, inputs, dir, hosted: Mutex::default() }
    }

    fn hosted(&self) -> MutexGuard<'_, BTreeMap<SessionId, Hosted>> {
        self.hosted.lock().unwrap_or_else(|p| p.into_inner())
    }

    async fn serve_call(&self, session: &Arc<Session>, method: &str, params: Value) -> Result<Value, String> {
        match method {
            "session.fold" => session.fold().map(|s| wire::state_payload(&s)).map_err(|e| e.to_string()),
            "session.rewind" => {
                let p: wire::RewindParams = serde_json::from_value(params)
                    .map_err(|e| format!("`session.rewind` params are not valid ({e}); nothing was rewound"))?;
                let mode = wire::rewind_mode(p.mode.as_deref())?;
                let e = session.rewind(&crate::boot::operator(), CheckpointId(p.checkpoint), mode).await.map_err(|e| e.to_string())?;
                Ok(json!({ "entry": e }))
            }
            "plan-mode.enter" => {
                let reason = params.get("reason").and_then(Value::as_str).filter(|r| !r.is_empty()).map(str::to_string);
                crate::slash::plan_mode(session, reason).map(|message| json!({ "message": message }))
            }
            m if m.starts_with("caps.") => {
                let caps = surface_rpc::CapsContext::new(self.comp.clone(), self.inputs.clone());
                surface_rpc::caps::call_in_session(&caps, session, m, &params).map_err(|e| e.to_string())
            }
            other => Err(format!(
                "this daemon serves no method `{other}` over a session, so nothing was done. It serves: {}",
                METHODS.join(", ")
            )),
        }
    }
}

impl daemon::SessionHost for AppHost {
    fn mutates(&self, method: &str, params: &Value) -> bool {
        surface_rpc::caps::is_apply(method, params)
    }

    fn approval_preset<'a>(&'a self, session: &'a Arc<Session>, preset: &'a str) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let caps = surface_rpc::CapsContext::new(self.comp.clone(), self.inputs.clone());
            let inputs = caps.fresh_inputs().map_err(|e| e.to_string())?;
            let proposal = surface_rpc::caps::propose(&self.comp, &inputs, vec![("policy.preset".into(), json!(preset))])
                .map_err(|e| e.to_string())?;
            let wrote = proposal.commit_approval_preset(session).map_err(|e| e.to_string())?;
            session.refresh_operator_policy(&crate::boot::operator()).map_err(|e| e.to_string())?;
            Ok(json!({ "preset": preset, "wrote": wrote }))
        })
    }

    fn sessions_dir(&self) -> PathBuf {
        self.dir.clone()
    }

    fn boot(&self, id: SessionId, log: Arc<dyn LogStore>, approval: Arc<dyn Approval>) -> BoxFuture<'_, Result<Arc<Session>, String>> {
        Box::pin(async move {
            let mut inputs = self.inputs.clone();
            inputs.capture_files().map_err(|e| e.to_string())?;
            let planned = aho_compose::load(&self.comp, &inputs).map_err(|e| e.to_string())?;
            let host = Host::at(self.comp.clone(), &self.project, planned, &inputs.home);
            let live = host.boot(id, log, approval, true).await?;
            if let Some(seed) = live.seed {
                let now = Hosted { session: Arc::downgrade(&live.session), seed, drivers: live.drivers };
                let old = self.hosted().insert(id, now);
                crate::boot::stop_drivers(old.and_then(|o| o.drivers)).await;
            }
            Ok(live.session)
        })
    }

    fn call<'a>(&'a self, session: &'a Arc<Session>, method: &'a str, params: Value) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(self.serve_call(session, method, params))
    }

    fn closing(&self, id: SessionId) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            let drivers = self.hosted().remove(&id).and_then(|h| h.drivers);
            match drivers {
                Some(d) => d.shutdown().await.into_iter().map(|e| format!("driver {e}")).collect(),
                None => Vec::new(),
            }
        })
    }

    fn before_exec(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            let running: Vec<(SessionId, Drivers)> =
                self.hosted().iter_mut().filter_map(|(id, h)| h.drivers.take().map(|d| (*id, d))).collect();
            let mut problems = Vec::new();
            for (id, d) in running {
                problems.extend(d.shutdown().await.into_iter().map(|e| format!("session {}: driver {e}", hex(id))));
            }
            problems
        })
    }

    fn exec_failed(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            let mut problems = Vec::new();
            for (id, h) in self.hosted().iter_mut().filter(|(_, h)| h.drivers.is_none()) {
                match h.session.upgrade() {
                    Some(s) => h.drivers = Some(session_drive::host(&h.seed, s, Arc::new(aho_control::SystemClock))),
                    None => problems
                        .push(format!("session {}: it closed while the reload ran, so its drivers were not started again", hex(*id))),
                }
            }
            problems
        })
    }
}
