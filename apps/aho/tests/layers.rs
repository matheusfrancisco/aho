//! Layer guard (12): `aho-types` and every seam definition crate (`crates/seams/*`) may depend only
//! on `aho-types`, `aho-log`, `aho-config`, `aho-cap`, other seams, and a tiny allowlist of
//! third-party crates. A dependency on a provider, tool, surface, bundle, or app fails. Edges that
//! predate the guard live in `scripts/ratchets/layers.json`; that list may only shrink.
//! `scripts/gate.sh --fix` (UPDATE_RATCHETS=1) rewrites it.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

const CORE_DEFINITIONS: &[&str] = &["aho-types", "aho-log", "aho-config", "aho-cap"];
const THIRD_PARTY: &[&str] = &["serde", "serde_json", "blake3", "tokio", "futures"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn metadata(root: &Path) -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = std::process::Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .expect("cargo metadata runs");
    assert!(out.status.success(), "cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `crate → forbidden dependencies` for every guarded crate.
fn violations(meta: &Value, root: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let seams_dir = root.join("crates/seams");
    let packages = meta["packages"].as_array().unwrap();
    let is_seam = |p: &Value| Path::new(p["manifest_path"].as_str().unwrap()).starts_with(&seams_dir);
    let seams: BTreeSet<&str> = packages.iter().filter(|p| is_seam(p)).map(|p| p["name"].as_str().unwrap()).collect();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for p in packages.iter().filter(|p| is_seam(p) || p["name"] == "aho-types") {
        let name = p["name"].as_str().unwrap();
        for d in p["dependencies"].as_array().unwrap() {
            if d["kind"].as_str() == Some("dev") {
                continue;
            }
            let dep = d["name"].as_str().unwrap();
            let allowed = CORE_DEFINITIONS.contains(&dep) || seams.contains(dep) || THIRD_PARTY.contains(&dep);
            if !allowed {
                out.entry(name.to_string()).or_default().insert(dep.to_string());
            }
        }
    }
    out
}

#[test]
fn seam_definition_crates_depend_only_on_definitions() {
    let root = root();
    let now = violations(&metadata(&root), &root);
    let baseline_path = root.join("scripts/ratchets/layers.json");
    if std::env::var_os("UPDATE_RATCHETS").is_some() {
        let text = serde_json::to_string_pretty(&now).unwrap() + "\n";
        std::fs::write(&baseline_path, text).unwrap();
        return;
    }
    let baseline: BTreeMap<String, BTreeSet<String>> = match std::fs::read_to_string(&baseline_path) {
        Ok(t) => serde_json::from_str(&t).unwrap(),
        Err(_) => BTreeMap::new(),
    };
    let mut new_edges = Vec::new();
    for (krate, deps) in &now {
        for d in deps {
            if !baseline.get(krate).is_some_and(|b| b.contains(d)) {
                new_edges.push(format!("{krate} → {d}"));
            }
        }
    }
    let mut gone = Vec::new();
    for (krate, deps) in &baseline {
        for d in deps {
            if !now.get(krate).is_some_and(|n| n.contains(d)) {
                gone.push(format!("{krate} → {d}"));
            }
        }
    }
    assert!(
        new_edges.is_empty(),
        "seam definition crates may depend only on {CORE_DEFINITIONS:?}, other seams, and {THIRD_PARTY:?}; new edges: {new_edges:?}. \
         Move the shared type into a definition crate instead of depending on an implementation."
    );
    assert!(gone.is_empty(), "these baseline edges are gone; tighten {} (scripts/gate.sh --fix): {gone:?}", baseline_path.display());
}

#[test]
fn the_guard_flags_implementation_edges_and_ignores_dev_dependencies() {
    let root = Path::new("/ws");
    let meta = serde_json::json!({"packages": [
        {"name": "aho-fs", "manifest_path": "/ws/crates/seams/aho-fs/Cargo.toml", "dependencies": [
            {"name": "aho-cap", "kind": null}, {"name": "aho-proc", "kind": null}, {"name": "serde", "kind": null},
            {"name": "world-host", "kind": null}, {"name": "tempfile", "kind": "dev"}
        ]},
        {"name": "aho-proc", "manifest_path": "/ws/crates/seams/aho-proc/Cargo.toml", "dependencies": [{"name": "regex", "kind": "build"}]},
        {"name": "aho-types", "manifest_path": "/ws/crates/aho-types/Cargo.toml", "dependencies": [{"name": "aho-session", "kind": null}]},
        {"name": "tool-fs", "manifest_path": "/ws/crates/tools/tool-fs/Cargo.toml", "dependencies": [{"name": "world-host", "kind": null}]}
    ]});
    let v = violations(&meta, root);
    let edges: Vec<(String, Vec<String>)> = v.into_iter().map(|(k, d)| (k, d.into_iter().collect())).collect();
    assert_eq!(
        edges,
        vec![
            ("aho-fs".to_string(), vec!["world-host".to_string()]),
            ("aho-proc".to_string(), vec!["regex".to_string()]),
            ("aho-types".to_string(), vec!["aho-session".to_string()]),
        ],
        "consumers (tool-fs) are not guarded; seams and aho-types are"
    );
}
