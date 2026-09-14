//! formal-lean end to end against fake lake/charon/aeneas scripts that print Lean 4 / Lake
//! output for the fixture project (Lean is not installed on this host).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_cap::*;
use aho_config::{ResolveInputs, Schema};
use aho_formal::SpecRef;
use aho_formal::*;
use aho_fs::FsWrite;
use aho_proc::SubprocessHandle;
use formal_lean::{FormalLean, Lean, LeanConfig};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Fixtures and `CARGO_TARGET_TMPDIR` share the aho workspace, which is the test world's root.
fn world() -> FsWrite {
    FsWrite(Arc::new(world_host::HostFs::new(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")).unwrap()))
}

fn lean_in(project: &Path) -> Lean {
    let f = fixtures();
    let proc = SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&f)));
    Lean::new(
        proc,
        world(),
        LeanConfig {
            project: project.to_path_buf(),
            lake: f.join("fake-lake.sh").display().to_string(),
            charon: f.join("fake-charon.sh").display().to_string(),
            aeneas: f.join("fake-aeneas.sh").display().to_string(),
            translations: "Aeneas".into(),
            budget_ms: 30_000,
            max_mutants: 16,
        },
    )
}

fn lean() -> Lean {
    lean_in(&fixtures().join("project"))
}

#[tokio::test]
async fn unsolved_goals_become_named_goals_and_are_not_a_pass() {
    let o = lean().prove("Proofs.Unsolved").await.unwrap();
    let Verdict::Unknown { reason } = &o.verdict else { panic!("{:?}", o.verdict) };
    assert!(reason.starts_with("unproved: 1 open goal(s) in gauss_formula"), "{reason}");
    assert_eq!(o.goals.len(), 1);
    let g = &o.goals[0];
    assert_eq!(g.name, "gauss_formula (Proofs/Unsolved.lean:8:16) case succ");
    assert_eq!(g.hypotheses, ["k : Nat", "ih : 2 * gauss k = k * (k + 1)"]);
    assert_eq!(g.target, "2 * (gauss k + (k + 1)) = (k + 1) * (k + 1 + 1)");
    let errors: Vec<&Diagnostic> = o.diagnostics.iter().filter(|d| d.severity == "error").collect();
    assert_eq!(errors.len(), 2);
    assert_eq!((errors[1].file.as_str(), errors[1].line, errors[1].col), ("Proofs/Unsolved.lean", 11, 54));
    assert!(errors[1].message.starts_with("type mismatch\n  h\nhas type"), "{}", errors[1].message);
    assert_eq!(o.mode, CheckMode::Proof);
}

#[tokio::test]
async fn sorry_builds_but_proves_nothing() {
    let o = lean().prove("Proofs.Sorry").await.unwrap();
    assert!(
        matches!(&o.verdict, Verdict::Unknown { reason } if reason.starts_with("incomplete: `sorry` at Proofs/Sorry.lean:3")),
        "{:?}",
        o.verdict
    );
    assert!(o.goals.is_empty());
}

#[tokio::test]
async fn a_clean_build_passes_and_is_bound_to_the_source() {
    let o = lean().prove("Proofs.Good").await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "{}", o.output);
    let src = std::fs::read_to_string(fixtures().join("project/Proofs/Good.lean")).unwrap();
    assert_eq!(o.spec_hash, aho_types::Hash::of_json(&vec![("Proofs/Good.lean".to_string(), src)]));
    assert!(matches!(lean().prove("not a module!").await, Err(FormalError::Invalid(_))));
}

#[tokio::test]
async fn model_checking_questions_are_unsupported() {
    let spec = SpecRef { kind: FormalKind::Lean, path: fixtures().join("project/Proofs/Good.lean"), module: "Proofs.Good".into() };
    assert!(matches!(lean().check(&spec, &CheckConfig::default()).await, Err(FormalError::Unsupported(_))));
    assert!(matches!(lean().trace_validate(&spec, &[]).await, Err(FormalError::Unsupported(_))));
    let o = lean().check(&spec, &CheckConfig { mode: CheckMode::Proof, ..CheckConfig::default() }).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass);
}

#[tokio::test]
async fn definition_mutants_are_caught_by_the_proofs_they_break() {
    let spec = SpecRef { kind: FormalKind::Lean, path: fixtures().join("project/Proofs/Total.lean"), module: "Proofs.Total".into() };
    let r = lean().mutate(&spec, &CheckConfig::default()).await.unwrap();
    let caught: Vec<(&str, bool)> = r.mutants.iter().map(|m| (m.name.as_str(), m.caught)).collect();
    assert_eq!(caught, [("swap:total#1", true), ("swap:ok#1", false), ("swap:ok#2", false), ("swap:ok#3", true)]);
    assert_eq!(r.mutants[0].violated.as_deref(), Some("total_two: tactic 'decide' proved that the proposition"));
    assert_eq!(r.mutants[1].verdict, Verdict::Pass, "a survivor");
    assert_eq!(r.verdict(), Verdict::Pass);
    let left: Vec<_> = std::fs::read_dir(fixtures().join("project"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".aho-"))
        .collect();
    assert!(left.is_empty(), "scratch copies are removed: {left:?}");
    assert!(std::fs::read_to_string(&spec.path).unwrap().contains("a + b"), "the real file is never touched");
}

#[tokio::test]
async fn aeneas_translation_is_bound_to_its_hash() {
    let project = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let krate = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(krate.path().join("Cargo.toml"), "[package]\nname = \"demo-crate\"\nversion = \"0.1.0\"\n").unwrap();
    let krate_dir = krate.path().join("demo-crate");
    std::fs::create_dir(&krate_dir).unwrap();
    std::fs::rename(krate.path().join("Cargo.toml"), krate_dir.join("Cargo.toml")).unwrap();
    let l = lean_in(project.path());
    let t = l.translate(&krate_dir.display().to_string(), &["fold".to_string(), "demo_crate::step".to_string()]).await.unwrap();
    assert_eq!(t.module, "Aeneas.DemoCrate.DemoCrate");
    let text = std::fs::read_to_string(&t.path).unwrap();
    assert!(text.contains("namespace DemoCrate") && text.contains("start-from: demo_crate::fold demo_crate::step"), "{text}");
    let again = l.translate(&krate_dir.display().to_string(), &["fold".to_string(), "demo_crate::step".to_string()]).await.unwrap();
    assert_eq!(again.hash, t.hash, "same code, same translation hash");
    assert!(matches!(l.translate(&project.path().display().to_string(), &[]).await, Err(FormalError::Invalid(_))), "not a crate");
}

#[tokio::test]
async fn missing_lake_is_tool_missing_with_the_install_fix() {
    let mut l = LeanConfig {
        project: fixtures().join("project"),
        lake: fixtures().join("no-such-lake").display().to_string(),
        charon: "charon".into(),
        aeneas: "aeneas".into(),
        translations: "Aeneas".into(),
        budget_ms: 1000,
        max_mutants: 1,
    };
    let proc = SubprocessHandle(Arc::new(world_host::HostSubprocess::new(&fixtures())));
    let e = Lean::new(proc.clone(), world(), l.clone()).prove("").await.unwrap_err();
    assert!(matches!(&e, FormalError::ToolMissing { fix, .. } if fix == "aho doctor formal --install lean"), "{e}");
    l.lake = fixtures().join("fake-lake.sh").display().to_string();
    assert_eq!(Lean::new(proc, world(), l).prove("").await.unwrap().verdict, Verdict::Pass);
}

fn loadout(sets: &[(&str, &str)]) -> aho_config::Loadout {
    let mut schema = Schema::default();
    for k in <LeanConfig as Config>::keys() {
        schema.add(k);
    }
    let inputs = ResolveInputs { sets: sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), ..ResolveInputs::default() };
    aho_config::resolve(&inputs, &schema, false).unwrap()
}

#[test]
fn plans_locked_without_lean_and_active_with_it() {
    let policy = PolicyInput { preset: "dev".into(), allowed: aho_types::ConsequenceSet::ALL };
    let m: &'static Manifest = FormalLean::MANIFEST;
    assert_eq!(m.unlock.map(|f| f.cli), Some("aho doctor formal --install lean"));
    let with_project = loadout(&[("formal.lean.project", "/work/proofs")]);

    let bare = Env::empty(Os::current());
    let p = plan(&[m], &with_project, &bare, &policy, vec![]).unwrap();
    let row = p.row("formal-lean").unwrap();
    assert!(
        matches!(&row.state, aho_log::CapState::Locked { missing } if missing[0].contains("Binary(lake)") && missing[0].contains("Binary(aeneas)")),
        "{:?}",
        row.state
    );

    let mut env = Env::empty(Os::current());
    env.binaries.insert("lake".into());
    let p = plan(&[m], &with_project, &env, &policy, vec![]).unwrap();
    assert!(matches!(p.row("formal-lean").unwrap().state, aho_log::CapState::Active), "{:?}", p.row("formal-lean"));
    assert_eq!(p.seams.get("lean"), Some(&"formal-lean"));

    let p = plan(&[m], &loadout(&[]), &env, &policy, vec![]).unwrap();
    let row = p.row("formal-lean").unwrap();
    assert!(row.missing.iter().any(|x| x.fix.as_deref() == Some("aho set formal.lean.project <value>")), "{:?}", row.missing);
}
