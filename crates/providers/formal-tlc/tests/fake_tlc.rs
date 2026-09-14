//! The provider end to end against `fixtures/fake-tlc.sh`, which replays real TLC recordings
//! for exactly these inputs (a changed cfg or spec text has no recording and fails loudly).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

mod common;

use aho_formal::*;
use common::*;

fn fake() -> formal_tlc::Tlc {
    // the jar is never opened by the fake; it only has to exist
    tlc(&fixtures().join("fake-tlc.sh"), &fixtures().join("specs/Pass.tla"))
}

#[tokio::test]
async fn provider_against_recorded_tlc_output() {
    run_all(&fake()).await;
}

#[tokio::test]
async fn unsupported_modes_and_missing_tools_are_typed() {
    let t = fake();
    let e =
        t.check(&spec("Pass"), &CheckConfig { mode: CheckMode::Inductive, ..counter(2, false, CheckMode::Inductive) }).await.unwrap_err();
    assert!(matches!(&e, FormalError::Unsupported(m) if m.contains("Apalache")), "{e}");
    assert!(matches!(e.verdict(), Some(Verdict::Unknown { .. })));
    assert!(matches!(t.prove("Aho.Fold").await, Err(FormalError::Unsupported(_))));
    assert!(matches!(t.check(&spec("Nope"), &counter(2, false, CheckMode::Bounded(0))).await, Err(FormalError::Invalid(_))));

    let no_java = tlc(&fixtures().join("no-such-java"), &fixtures().join("specs/Pass.tla"));
    let e = no_java.check(&spec("Pass"), &counter(2, false, CheckMode::Bounded(0))).await.unwrap_err();
    assert!(matches!(&e, FormalError::ToolMissing { fix, .. } if fix == "aho doctor formal --install tla"), "{e}");
    let no_jar = tlc(&fixtures().join("fake-tlc.sh"), &fixtures().join("missing.jar"));
    assert!(matches!(no_jar.check(&spec("Pass"), &counter(2, false, CheckMode::Bounded(0))).await, Err(FormalError::ToolMissing { .. })));
}

#[tokio::test]
async fn trace_validation_needs_a_trace_step() {
    let e = fake().trace_validate(&spec("Pass"), &good_trace()).await.unwrap_err();
    assert!(matches!(&e, FormalError::Invalid(m) if m.contains("TraceStep")), "{e}");
}

#[tokio::test]
async fn scratch_dirs_do_not_outlive_the_run() {
    // a private copy inside the test world, so concurrent tests' scratch dirs are not in view
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::copy(fixtures().join("specs/Pass.tla"), dir.path().join("Pass.tla")).unwrap();
    let o = fake().check(&SpecRef::tla(dir.path().join("Pass.tla")), &counter(2, false, CheckMode::Bounded(0))).await.unwrap();
    assert_eq!(o.verdict, Verdict::Pass, "same inputs, same recording: {}", o.output);
    let left: Vec<String> =
        std::fs::read_dir(dir.path()).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["Pass.tla"]);
}
