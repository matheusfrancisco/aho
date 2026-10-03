//! Which command verifies this workspace: the configured one, else the first project marker
//! found at the world's workspace root (read through the world's `Fs`, never the host).

use aho_fs::Fs;
use serde_json::Value;

/// A check command, run as `/bin/sh -c <command>` in the bound world's workspace root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub command: String,
    /// Exit codes besides 0 that mean "nothing is broken" for this runner.
    pub also_passes: &'static [i32],
}

impl Check {
    pub fn new(command: impl Into<String>) -> Check {
        Check { command: command.into(), also_passes: &[] }
    }

    pub fn passes(&self, code: i32) -> bool {
        code == 0 || self.also_passes.contains(&code)
    }
}

/// pytest exits 5 when it collected no tests: a project without tests is not a failure.
const PYTEST_NO_TESTS: &[i32] = &[5];

/// `Cargo.toml` → `cargo test --quiet`; `package.json` with a real `test` script →
/// `npm test --silent`; `pyproject.toml` / `pytest.ini` → `python3 -m pytest -q`; `go.mod` →
/// `go test ./...`; first match wins, none → `None` (nothing to verify with).
pub async fn detect(fs: &dyn Fs) -> Option<Check> {
    if exists(fs, "Cargo.toml").await {
        return Some(Check::new("cargo test --quiet"));
    }
    if npm_test_script(fs).await {
        return Some(Check::new("npm test --silent"));
    }
    for marker in ["pyproject.toml", "pytest.ini"] {
        if exists(fs, marker).await {
            return Some(Check { command: "python3 -m pytest -q".into(), also_passes: PYTEST_NO_TESTS });
        }
    }
    if exists(fs, "go.mod").await {
        return Some(Check::new("go test ./..."));
    }
    None
}

async fn exists(fs: &dyn Fs, path: &str) -> bool {
    fs.stat(path).await.is_ok()
}

/// A `scripts.test` other than the one `npm init` writes (which always fails).
async fn npm_test_script(fs: &dyn Fs) -> bool {
    let Ok(bytes) = fs.read_bytes("package.json").await else {
        return false;
    };
    let Ok(pkg) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    pkg["scripts"]["test"].as_str().is_some_and(|s| !s.trim().is_empty() && !s.contains("no test specified"))
}
