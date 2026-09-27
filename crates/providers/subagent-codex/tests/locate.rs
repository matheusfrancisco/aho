//! Finding this child's rollout among others: same working directory, written since launch, not
//! a Codex sub-agent thread, not already claimed.
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aho_types::Timestamp;
use foreign_cli::{Cli, LocateCtx};
use subagent_codex::Codex;

fn rollout(dir: &Path, name: &str, cwd: &Path, source: serde_json::Value, age: Duration) -> PathBuf {
    let meta = serde_json::json!({"timestamp": "2026-01-01T00:00:00.000Z", "type": "session_meta", "payload": {"id": name, "cwd": cwd.display().to_string(), "source": source, "cli_version": "0.154.0"}});
    let p = dir.join(format!("rollout-{name}.jsonl"));
    std::fs::write(&p, format!("{meta}\n")).unwrap();
    let f = std::fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(SystemTime::now() - age).unwrap();
    p
}

#[test]
fn the_oldest_fresh_own_rollout_for_this_directory_wins() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let ws = std::fs::canonicalize(dir.path()).unwrap().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let other = dir.path().join("elsewhere");
    let day = home.join("sessions/2026/09/26");
    let older_day = home.join("sessions/2026/09/25");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::create_dir_all(&older_day).unwrap();
    let now = Duration::ZERO;
    let fresh = serde_json::json!("cli");

    rollout(&day, "2026-09-26T10-00-00-stale", &ws, fresh.clone(), Duration::from_secs(3600));
    rollout(&day, "2026-09-26T10-00-01-other-dir", &other, fresh.clone(), now);
    rollout(&day, "2026-09-26T10-00-02-subagent", &ws, serde_json::json!({"subagent": {"other": "guardian"}}), now);
    let claimed = rollout(&day, "2026-09-26T10-00-03-claimed", &ws, fresh.clone(), now);
    let mine = rollout(&day, "2026-09-26T10-00-04-mine", &ws, fresh.clone(), now);
    rollout(&day, "2026-09-26T10-00-05-later", &ws, fresh.clone(), now);
    std::fs::write(day.join("notes.txt"), "not a rollout").unwrap();

    let codex = Codex { binary: "codex".into(), home: home.clone(), export_home: false };
    let launched_ms = Timestamp::now().0 - 60_000;
    let ctx = LocateCtx { cwd: &ws, cli_session: None, launched_ms, claimed: std::slice::from_ref(&claimed) };
    assert_eq!(codex.locate(&ctx), Some(mine.clone()));

    // Nothing written since launch → not found yet.
    let later = LocateCtx { cwd: &ws, cli_session: None, launched_ms: Timestamp::now().0 + 60_000, claimed: &[] };
    assert_eq!(codex.locate(&later), None);
    // A missing sessions directory means "not yet".
    let empty = Codex { binary: "codex".into(), home: dir.path().join("nowhere"), export_home: false };
    assert_eq!(empty.locate(&ctx), None);
}
