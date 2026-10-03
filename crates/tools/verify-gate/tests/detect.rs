//! Which check runs when `verify.command` is unset: the workspace's own markers, read through the
//! world's Fs, decide; a workspace with nothing to run has no check.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use verify_gate::{Check, detect};

async fn detected(files: &[(&str, &str)]) -> Option<Check> {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let fs = world_host::HostFs::new(dir.path()).unwrap();
    detect(&fs).await
}

async fn command(files: &[(&str, &str)]) -> Option<String> {
    detected(files).await.map(|c| c.command)
}

#[tokio::test]
async fn each_project_marker_selects_its_runner() {
    assert_eq!(command(&[("Cargo.toml", "[package]\n")]).await.as_deref(), Some("cargo test --quiet"));
    assert_eq!(command(&[("package.json", r#"{"scripts":{"test":"vitest run"}}"#)]).await.as_deref(), Some("npm test --silent"));
    assert_eq!(command(&[("pyproject.toml", "[project]\n")]).await.as_deref(), Some("python3 -m pytest -q"));
    assert_eq!(command(&[("pytest.ini", "[pytest]\n")]).await.as_deref(), Some("python3 -m pytest -q"));
    assert_eq!(command(&[("go.mod", "module x\n")]).await.as_deref(), Some("go test ./..."));
}

#[tokio::test]
async fn a_workspace_without_a_runnable_test_has_no_check() {
    assert_eq!(command(&[]).await, None);
    assert_eq!(command(&[("README.md", "hi\n")]).await, None);
    assert_eq!(command(&[("package.json", r#"{"scripts":{"build":"tsc"}}"#)]).await, None, "no test script");
    let npm_init = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
    assert_eq!(command(&[("package.json", npm_init)]).await, None, "npm init's placeholder always fails");
    assert_eq!(command(&[("package.json", "{not json")]).await, None);
}

#[tokio::test]
async fn the_first_marker_in_order_wins() {
    let both = [("package.json", r#"{"scripts":{"test":"jest"}}"#), ("Cargo.toml", "[package]\n")];
    assert_eq!(command(&both).await.as_deref(), Some("cargo test --quiet"));
    let py_go = [("go.mod", "module x\n"), ("pyproject.toml", "[project]\n")];
    assert_eq!(command(&py_go).await.as_deref(), Some("python3 -m pytest -q"));
}

#[tokio::test]
async fn pytest_collecting_no_tests_is_not_a_failure() {
    let c = detected(&[("pyproject.toml", "[project]\n")]).await.unwrap();
    assert!(c.passes(0) && c.passes(5));
    assert!(!c.passes(1) && !c.passes(2));
    let cargo = detected(&[("Cargo.toml", "[package]\n")]).await.unwrap();
    assert!(!cargo.passes(5));
}
