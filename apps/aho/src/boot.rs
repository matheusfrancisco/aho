//! Booting a session the way every verb that owns one does: the composition and its
//! plan, the root subagent runtime (`subagent-inproc` boots children from it), and for
//! long-lived hosts (`aho serve`, `aho tui`, `aho web`, `aho webhook`) the schedule and workflow
//! drivers, so `schedule_*` and `ralph_*` tools have something that fires them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_approval::Approval;
use aho_cap::{Composition, Registry};
use aho_compose::Planned;
use aho_log::{EntryKind, LogStore};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session};
use aho_types::Principal;
use aho_types::SessionId;
use session_drive::{DriverSeed, Drivers};
use subagent_inproc::SubagentRuntime;

use crate::ctx::{Ctx, hex, session_dir_of, workspace_of};

pub fn operator() -> Operator {
    // Whoever holds this terminal (or owns this process's stdio) is the operator.
    Operator::authenticate(OperatorProof::local_tty())
}

/// Everything a boot needs, owned, so a daemon can keep it for the life of the process.
#[derive(Clone)]
pub struct Host {
    pub comp: Composition,
    pub planned: Planned,
    /// `session.dir`: logs at `<dir>/<hex id>.jsonl`, children under `<dir>/<parent>/`.
    pub dir: PathBuf,
    pub workspace: String,
    pub home: String,
}

pub struct Live {
    pub session: Arc<Session>,
    /// `Some` for long-lived hosts; shut down when the session closes.
    pub drivers: Option<Drivers>,
    /// The drivers' seed, to host them again (`session_drive::host`).
    pub seed: Option<DriverSeed>,
}

impl Host {
    pub fn new(ctx: &Ctx, planned: Planned, home: &Path) -> Host {
        Host::at(ctx.comp.clone(), &ctx.project, planned, home)
    }

    pub fn at(comp: Composition, project: &Path, planned: Planned, home: &Path) -> Host {
        Host {
            comp,
            dir: session_dir_of(project, &planned),
            workspace: workspace_of(project, &planned).display().to_string(),
            home: home.display().to_string(),
            planned,
        }
    }
    pub fn log_path(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{}.jsonl", hex(id)))
    }

    fn spill_dir(&self, id: SessionId) -> PathBuf {
        self.dir.join(format!("{}.spill", hex(id)))
    }

    /// Boots (or resumes) session `id` over `log`. `drive` hosts the schedule/workflow drivers.
    /// A resumed log whose sealed plan is not this plan first records the change.
    pub async fn boot(&self, id: SessionId, log: Arc<dyn LogStore>, approval: Arc<dyn Approval>, drive: bool) -> Result<Live, String> {
        let p = &self.planned;
        if log.head().is_some() {
            record_resume(&*log, p)?;
        }
        let mut seed = Registry::default();
        // `session_query`: the cross-session index next to these logs, for the agent's tool.
        seed.services
            .insert(tool_session::INDEX_SERVICE.into(), Arc::new(crate::ctx::session_index(&self.dir).map_err(|e| e.to_string())?));
        SubagentRuntime {
            composition: self.comp.clone(),
            plan: p.plan.clone(),
            loadout: p.loadout.clone(),
            bootstrap: aho_compose::bootstrap_entries(p),
            seed: Registry::default(),
            operator: Arc::new(operator),
            spill_dir: self.spill_dir(id),
            session_dir: Some(self.dir.clone()),
            session: id,
            log: log.clone(),
            depth: 0,
        }
        .seed_into(&mut seed);
        let seed_drivers = drive.then(DriverSeed::new);
        if let Some(d) = &seed_drivers {
            d.seed_into(&mut seed);
        }
        let session = Session::boot(Boot {
            id,
            composition: &self.comp,
            plan: &p.plan,
            loadout: &p.loadout,
            bootstrap: aho_compose::bootstrap_entries(p),
            log,
            approval,
            operator: operator(),
            workspace: self.workspace.clone(),
            home: self.home.clone(),
            spill_dir: self.spill_dir(id),
            seed,
        })
        .await
        .map_err(|e| e.to_string())?;
        let drivers = seed_drivers.as_ref().map(|d| session_drive::host(d, session.clone(), Arc::new(aho_control::SystemClock)));
        Ok(Live { session, drivers, seed: seed_drivers })
    }
}

/// A session resumed under a plan other than the one its log sealed says so before it runs:
/// `CommandRun{resume, argv}` by the operator who started this process, the entries that make
/// its fold match the plan (config, capability states, policy epoch citing that command, plan
/// seal), then `CommandDone`. `aho explain`, `session.fold` and replay then see what it runs under.
/// Returns how many plan entries were written (0 when the plan is unchanged).
pub fn record_resume(log: &dyn LogStore, p: &Planned) -> Result<usize, String> {
    let entries = aho_control::branch(log)?;
    let st = aho_log::fold(&entries, &Default::default())
        .map_err(|e| format!("the session log does not fold ({e}), so it was not resumed. Inspect it with `aho session verify`"))?;
    if st.plan_hash == Some(p.plan.hash) {
        return Ok(0);
    }
    let why = |e: aho_log::LogError| format!("could not record the new plan in the session log ({e}), so it was not resumed");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run = log.append(None, Principal::Operator, EntryKind::CommandRun { name: "resume".into(), args, key: None }).map_err(why)?.id;
    let kinds = surface_rpc::caps::plan_entries(&st, p, run, "resumed under a new plan");
    let n = kinds.len();
    for k in kinds {
        log.append(None, Principal::Operator, k).map_err(why)?;
    }
    log.append(None, Principal::Operator, EntryKind::CommandDone { run, kind: "ok".into() }).map_err(why)?;
    Ok(n)
}

/// Stops a session's drivers and says what failed, if anything (their state is in the log).
pub async fn stop_drivers(drivers: Option<Drivers>) {
    if let Some(d) = drivers {
        for e in d.shutdown().await {
            eprintln!("aho: a session driver did not stop cleanly ({e}); its schedules resume from the log next time");
        }
    }
}
