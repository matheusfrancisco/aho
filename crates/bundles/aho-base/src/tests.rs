use std::path::PathBuf;
use std::sync::Arc;

use aho_cap::Registry;
use aho_compose::{Inputs, Planned, load};
use aho_config::Layer;
use aho_log::{CapDeltaOp, CapState, EntryKind, LogStore, MemoryLog};
use aho_policy::{Operator, OperatorProof};
use aho_session::{Boot, Session};
use aho_types::SessionId;
use serde_json::json;

struct Fx {
    _dir: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
}

fn fx(project_toml: Option<&str>) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    if let Some(t) = project_toml {
        std::fs::write(project.join("aho.toml"), t).unwrap();
    }
    Fx { _dir: dir, project, home }
}

fn planned(f: &Fx, sets: &[(&str, &str)]) -> Planned {
    let mut i = Inputs::new(&f.project, &f.home);
    if let Ok(p) = std::env::var("PATH") {
        i.env.insert("PATH".into(), p);
    }
    i.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    i.capture_files().unwrap();
    load(&super::spine(), &i).unwrap()
}

fn state(p: &Planned, id: &str) -> CapState {
    p.plan.row(id).unwrap_or_else(|| panic!("{id} is not linked")).state.clone()
}

#[test]
fn bundle_defaults_sit_below_every_operator_layer() {
    let f = fx(Some("[tools]\ngrace_ms = 150\n"));
    let p = planned(&f, &[]);
    let grace = &p.loadout.config["tools.grace_ms"];
    assert_eq!(grace.value, json!(150));
    assert_eq!(grace.winner.layer, Layer::FileProject);
    assert!(grace.shadowed.iter().any(|(o, v)| o.layer == Layer::Bundle(super::NAME.into()) && *v == json!(1000)));
    let preset = &p.loadout.config["policy.preset"];
    assert_eq!(preset.winner.layer, Layer::Bundle(super::NAME.into()));
    assert_eq!(p.plan.preset, "workspace");
}

#[test]
fn bare_machine_binds_the_host_world_and_locks_unconfigured_models() {
    let f = fx(None);
    let p = planned(&f, &[]);
    for id in [
        "world-host",
        "tool-fs",
        "tool-bash",
        "tool-jobs",
        "skills-fsdir",
        "tool-skills",
        "mcp-stdio",
        "tool-tasks",
        "tool-todo",
        "subagent-inproc",
        "tool-subagent",
        "pty-host",
    ] {
        assert_eq!(state(&p, id), CapState::Active, "{id}");
    }
    assert_eq!(p.plan.world.name, "host");
    let scripted = p.plan.row("llm-scripted").unwrap();
    assert!(matches!(scripted.state, CapState::Locked { .. }));
    assert!(scripted.missing.iter().any(|m| m.requirement.contains("provider.scripted.script")), "{:?}", scripted.missing);
    assert!(matches!(state(&p, "llm-openai-completions"), CapState::Locked { .. }), "no credential in the captured env");
    // Exclusive seams keep one provider bound; the others wait for the operator to switch.
    assert!(matches!(state(&p, "world-ssh"), CapState::Unlocked), "{:?}", state(&p, "world-ssh"));
    assert!(matches!(state(&p, "subagent-acp"), CapState::Unlocked), "{:?}", state(&p, "subagent-acp"));
}

#[test]
fn opt_in_capabilities_wait_for_the_operator_and_an_operator_layer_turns_them_on() {
    let f = fx(None);
    let p = planned(&f, &[]);
    for id in ["tool-terminal", "hooks-claude-code"] {
        let row = p.plan.row(id).unwrap();
        assert_eq!(row.state, CapState::Unlocked, "05: {id} ships `○ Unlocked  enable: aho enable {id}`");
        assert!(matches!(row.because, aho_cap::Because::OptIn { .. }), "{id}: {:?}", row.because);
    }
    let on = planned(&f, &[("caps.tool-terminal.enabled", "true"), ("caps.hooks-claude-code.enabled", "true")]);
    assert_eq!(state(&on, "tool-terminal"), CapState::Active);
    assert_eq!(state(&on, "hooks-claude-code"), CapState::Active);
    let acp = planned(&f, &[("seams.subagent", "subagent-acp")]);
    assert!(matches!(state(&acp, "subagent-inproc"), CapState::Unlocked), "switching the seam unbinds in-process subagents");
}

/// What an embedder seeds (apps/aho `boot.rs`): the root subagent runtime and the drivers. With
/// them, a real session over the scripted spine builds every active capability: nothing Broken.
#[tokio::test]
async fn scripted_spine_boots_a_session_with_every_active_capability_built() {
    let f = fx(None);
    let script = f.project.join("script.json");
    std::fs::write(&script, r#"{"steps": [{"text": "hi"}]}"#).unwrap();
    let (root, shadow) = (f.project.display().to_string(), f.home.join("shadow").display().to_string());
    let p = planned(
        &f,
        &[
            ("model.provider", "scripted"),
            ("provider.scripted.script", &script.display().to_string()),
            ("workspace.root", &root),
            ("workspace.shadow_dir", &shadow),
        ],
    );
    assert_eq!(state(&p, "llm-scripted"), CapState::Active);
    let comp = super::spine();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::default());
    let operator: subagent_inproc::OperatorFactory = Arc::new(|| Operator::authenticate(OperatorProof::local_tty()));
    let mut seed = Registry::default();
    subagent_inproc::SubagentRuntime {
        composition: comp.clone(),
        plan: p.plan.clone(),
        loadout: p.loadout.clone(),
        bootstrap: aho_compose::bootstrap_entries(&p),
        seed: Registry::default(),
        operator: operator.clone(),
        spill_dir: f.home.join("spill"),
        session_dir: None,
        session: SessionId(1),
        log: log.clone(),
        depth: 0,
    }
    .seed_into(&mut seed);
    let drivers = session_drive::DriverSeed::new();
    drivers.seed_into(&mut seed);
    let s = Session::boot(Boot {
        id: SessionId(1),
        composition: &comp,
        plan: &p.plan,
        loadout: &p.loadout,
        bootstrap: aho_compose::bootstrap_entries(&p),
        log: log.clone(),
        approval: Arc::new(aho_approval::NeverApproval),
        operator: operator(),
        workspace: root.clone(),
        home: f.home.display().to_string(),
        spill_dir: f.home.join("spill"),
        seed,
    })
    .await
    .unwrap();
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let broken: Vec<String> = entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::CapDelta { id, op: CapDeltaOp::Broken { err }, .. } => Some(format!("{id}: {err}")),
            _ => None,
        })
        .collect();
    assert!(broken.is_empty(), "{broken:?}");
    for tool in
        ["read", "write", "bash", "job_wait", "aho_why", "unlock_request", "subagent", "run_code", "schedule_create", "enter_plan_mode"]
    {
        assert!(s.registry().tool(tool).is_some(), "{tool} registered");
    }
    let hosted = session_drive::host(&drivers, s.clone(), Arc::new(aho_control::SystemClock));
    assert!(hosted.shutdown().await.is_empty());
}
