use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ConfigKey, Layer, Loadout, Ty, Where};
use aho_log::{CapState, EntryKind, LogStore, MemoryLog};
use serde_json::json;

use crate::*;

// ───────────────────────────── test capabilities ─────────────────────────────

#[derive(Clone)]
struct FsW;
impl Handle for FsW {
    const KIND: HandleKind = HandleKind::FsWrite;
}

struct World;
impl Capability for World {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(World)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(FsW);
    }
}
declare_capability!(
    World,
    "world-test",
    kind = Kind::Provider,
    provides = [SeamId("fs")],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::FsWrite],
    enforced_by = "policy only"
);

struct EditDeps(#[allow(dead_code)] FsW);
impl Deps for EditDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::FsWrite];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(EditDeps(r.handle()?))
    }
}
struct EditCfg;
impl Config for EditCfg {
    const PREFIX: &'static str = "edit";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new("edit.mode", Ty::String, Some(json!("safe")), "How edits are applied."),
            ConfigKey::new("edit.backup", Ty::Bool, Some(json!(true)), "Keep a backup of every edited file."),
        ]
    }
    fn from_loadout(_: &Loadout) -> Result<Self, ConfigError> {
        Ok(EditCfg)
    }
}
static EDIT_TOOLS: &[ToolDecl] = &[ToolDecl::new("edit", "Edit a file", Consequence::WorkspaceWrite)];
struct ToolEdit;
impl Capability for ToolEdit {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = EditDeps;
    type Config = EditCfg;
    fn build(_: EditDeps, _: EditCfg) -> Result<Self, BuildError> {
        Ok(ToolEdit)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(
    ToolEdit,
    "tool-edit",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(SeamId("fs"))],
    tools = EDIT_TOOLS
);

struct Gh;
impl Capability for Gh {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Gh)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(Gh, "mcp.github", kind = Kind::Tool, provides = [], requires = [Requirement::Credential("github")], tools = &[]);

struct Qemu;
impl Capability for Qemu {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Qemu)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(
    Qemu,
    "tool-qemu",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Binary("aho-test-present-bin"), Requirement::Binary("aho-test-plain-file")],
    tools = &[]
);

struct Skills;
impl Discoverer for Skills {
    fn discover(&self, env: &Env) -> Vec<Discovered> {
        vec![Discovered {
            id: "skill.hello".into(),
            kind: Kind::Template,
            origin: CapOrigin::Workspace,
            source_path: Some(format!("{}/.aho/skills/hello", env.project)),
            classes: ConsequenceSet::of(&[Consequence::ReadOnly]),
            requires: vec![],
            broken: None,
            doc: "Say hello".into(),
        }]
    }
}

fn comp() -> Composition {
    Composition::new("test-bundle").provide::<World>().consume::<ToolEdit>().consume::<Gh>().discoverer(Arc::new(Skills))
}

// ───────────────────────────── fixture ─────────────────────────────

struct Fixture {
    _dir: tempfile::TempDir,
    project: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".aho")).unwrap();
    Fixture { _dir: dir, project, home }
}

impl Fixture {
    fn inputs(&self, env: &[(&str, &str)]) -> Inputs {
        let mut i = Inputs::new(&self.project, &self.home);
        i.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        i.capture_files().unwrap();
        i
    }
    fn project_file(&self, text: &str) -> PathBuf {
        let p = self.project.join("aho.toml");
        std::fs::write(&p, text).unwrap();
        p
    }
    fn home_file(&self, text: &str) -> PathBuf {
        let p = self.home.join(".aho").join("config.toml");
        std::fs::write(&p, text).unwrap();
        p
    }
}

fn load_ok(c: &Composition, i: &Inputs) -> Planned {
    match load(c, i) {
        Ok(p) => p,
        Err(e) => panic!("load failed: {e}"),
    }
}

fn state_of(p: &Planned) -> CapState {
    p.plan.row("tool-edit").unwrap().state.clone()
}

// ───────────────────────────── tests ─────────────────────────────

#[test]
fn env_beats_file_and_explain_shows_every_layer_it_beat() {
    let f = fixture();
    let file = f.project_file("# project settings\n[edit]\nmode = \"careful\"\n");
    let p = load_ok(&comp(), &f.inputs(&[("AHO_EDIT_MODE", "fast")]));

    let e = aho_inspect::explain(&p.loadout, "edit.mode");
    assert_eq!(e.value, Some(json!("fast")));
    assert_eq!(e.winner.as_ref().unwrap().at, Where::Env { var: "AHO_EDIT_MODE".into() });
    assert_eq!(e.shadowed.len(), 2);
    assert_eq!(e.shadowed[0].origin, format!("File {}:3", file.display()));
    assert_eq!(e.shadowed[0].value, json!("careful"));
    assert!(e.shadowed[0].broader_scope_wins, "a user-wide env var beat the project file");
    assert_eq!(e.shadowed[1].origin, "Default");
    let out = aho_inspect::render_explain(&e);
    assert!(out.starts_with("edit.mode = \"fast\"\n  winner   Env AHO_EDIT_MODE"), "{out}");
    assert!(out.lines().nth(2).unwrap().ends_with("⚠ broader-scope-wins"), "{out}");

    // the same answer from the log the session would write
    let log = MemoryLog::new();
    for k in bootstrap_entries(&p) {
        log.append(None, Principal::system("compose"), k).unwrap();
    }
    let entries = log.read_branch(log.head().unwrap()).unwrap();
    let from_log = aho_inspect::explain(&entries[..], "edit.mode");
    assert_eq!(from_log.shadowed, e.shadowed);
    assert!(from_log.history.len() == 1 && from_log.history[0].bootstrap);
}

#[test]
fn disable_renders_disabled_with_the_flag_or_env_that_did_it() {
    let f = fixture();
    let mut i = f.inputs(&[]);
    i.disable = vec!["tool-edit".into()];
    let p = load_ok(&comp(), &i);
    let out = aho_inspect::render_tree(comp().name, &aho_inspect::tree(&p.plan), &p.plan);
    let line = out.lines().find(|l| l.contains("tool-edit")).unwrap();
    assert!(line.starts_with("⊘ tool-edit") && line.contains("Disabled") && line.contains("Arg --disable tool-edit"), "{out}");

    let p = load_ok(&comp(), &f.inputs(&[("AHO_DISABLE", "tool-edit, mcp.github")]));
    assert!(matches!(p.plan.row("mcp.github").unwrap().state, CapState::Disabled { ref why } if why == "Env AHO_DISABLE"));
    assert!(matches!(state_of(&p), CapState::Disabled { .. }));
}

#[test]
fn plan_diff_names_the_classes_a_flip_unlocks_and_the_winning_origin() {
    let f = fixture();
    let mut narrow = f.inputs(&[]);
    narrow.sets = vec![("policy.preset".into(), "read-only".into())];
    let before = load_ok(&comp(), &narrow);
    let after = load_ok(&comp(), &f.inputs(&[]));
    assert_eq!(state_of(&before), CapState::Active, "a preset is a per-call ceiling; it never locks a capability");
    assert!(before.plan.row("tool-edit").unwrap().over_preset.contains(Consequence::WorkspaceWrite));
    assert_eq!(state_of(&after), CapState::Active);

    let d = plan_diff(&before, &after);
    let flip = d.caps.iter().find(|c| c.id == "tool-edit").unwrap();
    assert_eq!(flip.unlocks, ConsequenceSet::of(&[Consequence::WorkspaceWrite]));
    assert_eq!(flip.revokes, ConsequenceSet::EMPTY);
    assert_eq!(d.preset, Some(("read-only".into(), "workspace".into())));
    let change = d.config.iter().find(|c| c.key == "policy.preset").unwrap();
    assert_eq!((change.before.clone(), change.after.clone()), (Some(json!("read-only")), Some(json!("workspace"))));
    assert_eq!(change.winner.as_deref(), Some("Default"));
    assert_eq!((change.beats.as_deref(), change.replaced.as_deref()), (None, Some("Arg --set policy.preset")));
    let out = render_plan_diff(&d);
    assert!(out.contains("  ~Allows  tool-edit           unlocks: WorkspaceWrite\n"), "{out}");
    assert!(out.contains("~ policy.preset  read-only -> workspace   (Default; was Arg --set policy.preset)"), "{out}");
    assert!(out.contains("~ allowed +WorkspaceWrite, SpawnsProcess, TalksToHuman"), "{out}");

    let back = render_plan_diff(&plan_diff(&after, &before));
    assert!(back.contains("  ~Allows  tool-edit           revokes: WorkspaceWrite\n"), "{back}");
    assert!(back.contains("~ policy.preset  workspace -> read-only   (Arg --set policy.preset beats Default)"), "{back}");
    assert!(plan_diff(&after, &after).is_empty());
}

#[test]
fn governing_caps_settings_show_their_values_and_origins() {
    let f = fixture();
    let mut i = f.inputs(&[]);
    i.sets = vec![("policy.preset".into(), "read-only".into()), ("caps.tool-edit.allow".into(), "true".into())];
    let p = load_ok(&comp(), &i);
    assert_eq!(state_of(&p), CapState::Active, "caps.<id>.allow admits a class the preset lacks");

    let mut nodes = aho_inspect::tree(&p.plan);
    aho_inspect::govern(&mut nodes, &p.loadout.config);
    let why = aho_inspect::why_in(&nodes, &p.plan, "tool-edit").unwrap();
    let allow = aho_inspect::Setting {
        key: "caps.tool-edit.allow".into(),
        value: Some(json!(true)),
        origin: "Arg --set caps.tool-edit.allow".into(),
    };
    assert_eq!(why.governed_by, vec![allow]);
    assert!(aho_inspect::why(&p.plan, "tool-edit").unwrap().governed_by.is_empty(), "the plan alone does not carry values");
    let out = aho_inspect::render_tree(comp().name, &nodes, &p.plan);
    let line = out.lines().find(|l| l.contains("tool-edit")).unwrap();
    assert!(line.ends_with("[caps.tool-edit.allow = true ← Arg --set caps.tool-edit.allow]"), "{line}");
}

#[test]
fn bootstrap_entries_fold_back_into_the_planned_state() {
    let f = fixture();
    f.project_file("typo_key = 1\n");
    let p = load_ok(&comp(), &f.inputs(&[("GITHUB_TOKEN", "ghp_live")]));
    assert!(p.plan.discovered.iter().any(|d| d.id == "skill.hello"), "discoverers ran against the probed env");

    let kinds = bootstrap_entries(&p);
    assert!(matches!(kinds.last(), Some(EntryKind::PlanSealed { plan_hash, .. }) if *plan_hash == p.plan.hash));
    let diag = kinds.iter().find_map(|k| match k {
        EntryKind::LoadoutDiagnostic { lint, key, .. } if key == "typo_key" => Some(lint.clone()),
        _ => None,
    });
    assert_eq!(diag.as_deref(), Some("unknown-key"));
    let edit = kinds
        .iter()
        .find_map(|k| match k {
            EntryKind::CapActivated { id, deps, config_hash, manifest_hash } if id == "tool-edit" => {
                Some((deps.clone(), *config_hash, *manifest_hash))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(edit.0, vec!["FsWrite".to_string()]);
    assert_eq!(edit.1, cap_config_hash(ToolEdit::MANIFEST, &p.loadout));
    assert_eq!(edit.2, ToolEdit::MANIFEST.hash());

    let log = MemoryLog::new();
    for k in kinds {
        log.append(None, Principal::system("compose"), k).unwrap();
    }
    let state = aho_log::fold(&log.read_branch(log.head().unwrap()).unwrap(), &Default::default()).unwrap();
    assert_eq!(state.config, p.loadout.config);
    let planned_active: BTreeSet<&str> = p.plan.active().map(|r| r.id.name).collect();
    let folded_active: BTreeSet<&str> = state.active_caps().map(String::as_str).collect();
    assert_eq!(folded_active, planned_active);
    assert!(folded_active.contains("mcp.github"), "GITHUB_TOKEN satisfied Credential(github)");
    assert_eq!(state.plan_hash, Some(p.plan.hash));
    assert_eq!((state.policy.epoch, state.policy.allowed, state.policy.preset.as_str()), (Epoch(1), p.plan.allowed, "workspace"));
}

#[test]
fn edit_config_hash_moves_only_with_that_capabilitys_keys() {
    let f = fixture();
    let base = load_ok(&comp(), &f.inputs(&[]));
    let other = load_ok(&comp(), &f.inputs(&[("AHO_MODEL_NAME", "other-model")]));
    let own = load_ok(&comp(), &f.inputs(&[("AHO_EDIT_BACKUP", "false")]));
    let h = |p: &Planned| cap_config_hash(ToolEdit::MANIFEST, &p.loadout);
    assert_eq!(h(&base), h(&other));
    assert_ne!(h(&base), h(&own));
}

#[test]
fn lock_refuses_a_plan_whose_inputs_drifted_and_names_the_file() {
    let f = fixture();
    let file = f.project_file("[edit]\nmode = \"careful\"\n");
    let p = load_ok(&comp(), &f.inputs(&[]));
    let lock = LockFile::of(&p);
    let written = lock.write(&f.project).unwrap();
    assert_eq!(written, f.project.join(LOCK_FILE));
    assert_eq!(LockFile::read(&f.project).unwrap(), lock);
    assert!(lock.verify(&p).is_ok());
    assert!(verify_against(p.plan.hash, &p).is_ok());

    // a comment is not drift: same values, same plan
    f.project_file("# tuned\n[edit]\nmode = \"careful\"\n");
    assert!(lock.verify(&load_ok(&comp(), &f.inputs(&[]))).is_ok());

    f.project_file("[edit]\nmode = \"fast\"\n");
    let drifted = load_ok(&comp(), &f.inputs(&[]));
    let err = LockFile::read(&f.project).unwrap().verify(&drifted).unwrap_err();
    match &err {
        ComposeError::Drift { expected, actual, causes } => {
            assert_eq!((*expected, *actual), (p.plan.hash, drifted.plan.hash));
            assert_eq!(causes, &vec![format!("{} changed", file.display())]);
        }
        other => panic!("expected drift, got {other:?}"),
    }
    assert!(err.to_string().contains("run `aho plan --lock` to accept the new plan"), "{err}");
    assert!(matches!(verify_against(p.plan.hash, &drifted), Err(ComposeError::Drift { .. })));
    assert!(matches!(LockFile::read(&f.home), Err(ComposeError::Lock { .. })));
}

#[test]
fn credentials_unlock_requirements_and_never_reach_the_log() {
    let f = fixture();
    f.project_file("[provider.anthropic]\nkey = \"sk-ant-file-secret\"\nbase_url = \"http://proxy.local\"\n");
    let i = f.inputs(&[("GITHUB_TOKEN", "ghp_live_token"), ("AHO_CRED_GITLAB", "glpat-secret"), ("OPENAI_API_KEY", "")]);
    let p = load_ok(&comp(), &i);

    let present: Vec<&str> = p.env.credentials.iter().map(String::as_str).collect();
    assert_eq!(present, vec!["anthropic", "github", "gitlab"], "empty OPENAI_API_KEY does not count");
    assert_eq!(p.plan.row("mcp.github").unwrap().state, CapState::Active);
    assert_eq!(p.loadout.secrets["provider.anthropic.key"].expose(), "sk-ant-file-secret");
    assert_eq!(p.loadout.get("provider.anthropic.base_url"), Some(&json!("http://proxy.local")));
    assert!(!p.loadout.diagnostics.iter().any(|d| d.key.starts_with("AHO_CRED_")), "{:?}", p.loadout.diagnostics);

    let logged = serde_json::to_string(&bootstrap_entries(&p)).unwrap();
    for secret in ["sk-ant-file-secret", "ghp_live_token", "glpat-secret"] {
        assert!(!logged.contains(secret), "{secret} leaked into the bootstrap entries");
    }
    let origins = credentials(&i, &p.loadout);
    assert_eq!(origins["github"], vec![CredentialOrigin::Env { var: "GITHUB_TOKEN".into() }]);
    assert!(matches!(&origins["anthropic"][0], CredentialOrigin::AppConfigFile { key, origin }
        if key == "provider.anthropic.key" && origin.ends_with("aho.toml:2")));
}

#[test]
fn clearing_a_saved_credential_relocks_its_capability() {
    let f = fixture();
    f.project_file("[credentials]\ngithub = \"saved-token\"\n");
    let before = load_ok(&comp(), &f.inputs(&[]));
    assert_eq!(before.plan.row("mcp.github").unwrap().state, CapState::Active);
    f.project_file("[credentials]\ngithub = \"\"\n");
    let after = load_ok(&comp(), &f.inputs(&[]));
    assert!(!after.env.credentials.contains("github"));
    assert!(matches!(after.plan.row("mcp.github").unwrap().state, CapState::Locked { .. }));
}

#[cfg(unix)]
#[test]
fn binaries_are_probed_on_path_by_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let bin = f.project.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for (name, mode) in [("aho-test-present-bin", 0o755), ("aho-test-plain-file", 0o644)] {
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let path = format!("/nonexistent-aho-dir::{}", bin.display());
    let c = comp().consume::<Qemu>();
    let p = load_ok(&c, &f.inputs(&[("PATH", path.as_str())]));
    assert_eq!(p.env.binaries.iter().map(String::as_str).collect::<Vec<_>>(), vec!["aho-test-present-bin"]);
    let row = p.plan.row("tool-qemu").unwrap();
    assert_eq!(row.missing.iter().map(|m| m.requirement.as_str()).collect::<Vec<_>>(), vec!["Binary(aho-test-plain-file)"]);
    assert!(load_ok(&c, &f.inputs(&[])).env.binaries.is_empty(), "no PATH, nothing found");
}

#[test]
fn apply_writes_into_the_most_specific_existing_file_keeping_comments() {
    let f = fixture();
    let home_cfg = f.home.join(".aho").join("config.toml");
    std::fs::remove_dir_all(f.home.join(".aho")).unwrap();
    assert_eq!(apply_target(&f.project, &f.home), home_cfg);
    let (old, new) = apply(&home_cfg, "edit.mode", json!("home-mode")).unwrap();
    assert_eq!(old, "");
    assert!(new.contains("mode = \"home-mode\""), "{new}");
    assert_eq!(load_ok(&comp(), &f.inputs(&[])).loadout.get("edit.mode"), Some(&json!("home-mode")));

    let project_cfg = f.project_file("# keep me\n[edit]\nbackup = false # inline\n");
    assert_eq!(apply_target(&f.project, &f.home), project_cfg);
    let (old, new) = apply(&project_cfg, "edit.mode", json!("project-mode")).unwrap();
    assert_eq!(old, "# keep me\n[edit]\nbackup = false # inline\n");
    assert!(new.starts_with("# keep me\n") && new.contains("backup = false # inline"), "{new}");
    let p = load_ok(&comp(), &f.inputs(&[]));
    let s = &p.loadout.config["edit.mode"];
    assert_eq!(s.value, json!("project-mode"));
    assert!(s.shadowed.iter().any(|(o, v)| o.layer == Layer::FileHome && v == &json!("home-mode")));
    assert!(matches!(apply(&project_cfg, "edit.backup", json!(null)), Err(ComposeError::Apply { .. })));
    assert_eq!(std::fs::read_to_string(&project_cfg).unwrap(), new, "a refused apply leaves the file alone");
}

#[test]
fn a_saved_loadout_replays_as_an_arg_layer_with_the_same_plan() {
    let f = fixture();
    f.project_file("[edit]\nmode = \"careful\"\n[provider.anthropic]\nkey = \"sk-never-frozen\"\n");
    let mut i = f.inputs(&[]);
    i.sets = vec![("model.name".into(), "frozen-model".into())];
    let original = load_ok(&comp(), &i);
    let file = loadouts::save(&f.home, "frozen", &original.loadout).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("\"edit.mode\" = \"careful\"") && !text.contains("sk-never-frozen"), "{text}");
    assert!(text.contains("never frozen; set them again: provider.anthropic.key"), "{text}");
    assert_eq!(loadouts::list(&f.home).unwrap(), vec!["frozen".to_string()]);

    // replay on a clean slate: no project file, no flags, only the frozen layer
    std::fs::remove_file(f.project.join("aho.toml")).unwrap();
    let replay = f.inputs(&[]).with_layer(loadouts::load(&f.home, "frozen").unwrap());
    let again = load_ok(&comp(), &replay);
    let winner = &again.loadout.config["edit.mode"].winner;
    assert_eq!(winner.layer, Layer::Arg);
    assert!(matches!(&winner.at, Where::File { path, .. } if Path::new(path) == file));
    let keys = |l: &Loadout| {
        l.config.iter().filter(|(k, _)| !l.secrets.contains_key(*k)).map(|(k, s)| (k.clone(), s.value.clone())).collect::<Vec<_>>()
    };
    assert_eq!(keys(&again.loadout), keys(&original.loadout));

    // without secrets in play the frozen table reproduces the plan exactly
    let mut plain = f.inputs(&[]);
    plain.disable = vec!["mcp.github".into()];
    let before = load_ok(&comp(), &plain);
    loadouts::save(&f.home, "plain", &before.loadout).unwrap();
    let replayed = load_ok(&comp(), &f.inputs(&[]).with_layer(loadouts::load(&f.home, "plain").unwrap()));
    assert_eq!(replayed.loadout.hash, before.loadout.hash);
    assert!(replayed.loadout.is_disabled("mcp.github"));

    let d = loadouts::diff(&loadouts::read(&f.home, "plain").unwrap(), &loadouts::read(&f.home, "frozen").unwrap());
    assert!(d.contains(&("model.name".into(), Some(json!("gpt-4.1-mini")), Some(json!("frozen-model")))), "{d:?}");
    assert!(d.iter().any(|(k, a, b)| k == "disable" && a == &Some(json!(["mcp.github"])) && b == &Some(json!([]))), "{d:?}");
    assert!(matches!(loadouts::save(&f.home, "../escape", &before.loadout), Err(ComposeError::BadName { .. })));
    assert!(matches!(loadouts::load(&f.home, "missing"), Err(ComposeError::Loadout { .. })));
}

#[test]
fn unknown_preset_is_refused_naming_where_it_was_set() {
    let f = fixture();
    let mut i = f.inputs(&[]);
    i.sets = vec![("policy.preset".into(), "yolo".into())];
    let err = load(&comp(), &i).unwrap_err();
    assert!(matches!(&err, ComposeError::UnknownPreset { name, origin } if name == "yolo" && origin == "Arg --set policy.preset"));
    assert!(err.to_string().contains("names no preset"), "{err}");
}

#[test]
fn profile_ranks_between_bundle_defaults_and_home_files() {
    let f = fixture();
    let c = comp().default("edit.mode", json!("bundle"));
    std::fs::create_dir_all(f.home.join(".aho/profiles")).unwrap();
    std::fs::write(f.home.join(".aho/profiles/web.toml"), "[edit]\nmode = \"profile\"\n").unwrap();
    let mut i = Inputs::new(&f.project, &f.home);
    i.profile = Some("web".into());
    i.capture_files().unwrap();
    let s = load_ok(&c, &i).loadout.config["edit.mode"].clone();
    assert_eq!((s.value, s.winner.layer), (json!("profile"), Layer::Profile("web".into())));
    assert_eq!(s.shadowed[0].0.layer, Layer::Bundle("test-bundle".into()));

    f.home_file("[edit]\nmode = \"home\"\n");
    i.capture_files().unwrap();
    let s = load_ok(&c, &i).loadout.config["edit.mode"].clone();
    assert_eq!((s.value, s.winner.layer), (json!("home"), Layer::FileHome));
    assert_eq!(s.shadowed[0].0.layer, Layer::Profile("web".into()));

    i.profile = Some("missing".into());
    i.capture_files().unwrap();
    let err = load(&c, &i).unwrap_err();
    assert!(matches!(&err, ComposeError::ProfileNotFound { name, builtin, .. } if name == "missing" && builtin.is_empty()), "{err}");
    i.profile = Some("../web".into());
    assert!(matches!(i.capture_files(), Err(ComposeError::BadName { .. })));
}

#[test]
fn builtin_profile_layers_under_the_operator_file_of_the_same_name() {
    let f = fixture();
    let c = Composition::new("product")
        .include(comp().profile("web", "[edit]\nmode = \"base\"\nbackup = false\n"))
        .profile("web", "[edit]\nmode = \"product\"\n");
    let at = |s: &aho_config::Sourced<serde_json::Value>| match &s.winner.at {
        Where::File { path, .. } => path.clone(),
        other => format!("{other:?}"),
    };
    let mut i = Inputs::new(&f.project, &f.home);
    let s = load_ok(&c, &i).loadout.config["edit.mode"].clone();
    assert_eq!(s.value, json!("safe"), "built-in fragments apply only when the profile is named");

    // no file: the fragments alone, the including product's after the included bundle's
    i.profile = Some("web".into());
    i.capture_files().unwrap();
    let l = load_ok(&c, &i).loadout;
    let (mode, backup) = (&l.config["edit.mode"], &l.config["edit.backup"]);
    assert_eq!((mode.value.clone(), mode.winner.layer.clone()), (json!("product"), Layer::Profile("web".into())));
    assert_eq!(at(mode), "builtin:product/profiles/web.toml");
    assert_eq!((backup.value.clone(), at(backup)), (json!(false), "builtin:test-bundle/profiles/web.toml".to_string()));

    // the operator's file of the same name wins key by key and inherits the rest
    std::fs::create_dir_all(f.home.join(".aho/profiles")).unwrap();
    let file = f.home.join(".aho/profiles/web.toml");
    std::fs::write(&file, "[edit]\nmode = \"mine\"\n").unwrap();
    i.capture_files().unwrap();
    let l = load_ok(&c, &i).loadout;
    let mode = &l.config["edit.mode"];
    assert_eq!((mode.value.clone(), at(mode)), (json!("mine"), file.display().to_string()));
    assert!(
        mode.shadowed
            .iter()
            .any(|(o, v)| matches!(&o.at, Where::File { path, .. } if path.starts_with("builtin:")) && *v == json!("product"))
    );
    assert_eq!(l.config["edit.backup"].value, json!(false));

    // ~/.aho/config.toml still outranks every profile layer
    f.home_file("[edit]\nmode = \"home\"\n");
    i.capture_files().unwrap();
    assert_eq!(load_ok(&c, &i).loadout.config["edit.mode"].value, json!("home"));

    i.profile = Some("nope".into());
    i.capture_files().unwrap();
    let err = load(&c, &i).unwrap_err();
    assert!(err.to_string().contains("(built-in: web)"), "{err}");
}

#[test]
fn strict_refuses_unknown_keys_and_broken_files_are_named() {
    let f = fixture();
    f.project_file("typo_key = 1\n");
    let mut i = f.inputs(&[]);
    i.strict = true;
    let err = load(&comp(), &i).unwrap_err();
    assert!(err.to_string().starts_with("--strict refused 1 diagnostic(s)"), "{err}");

    let file = f.project_file("[edit\nmode = ");
    let err = load(&comp(), &f.inputs(&[])).unwrap_err();
    assert!(err.to_string().starts_with(&format!("could not parse {}", file.display())), "{err}");
}
