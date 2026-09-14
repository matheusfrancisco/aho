#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use aho_cap::{Capability, Composition, Env, Os, PolicyInput, plan};
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::{CapState, CustomFolds, Entry, EntryKind, LogStore, MemoryLog};
use aho_policy::{Operator, OperatorProof};
use aho_types::{BoxFuture, Hash, Principal, Value};
use selfdev::*;
use serde_json::json;

const GOOD: &str = r#"if [ "$1" = plan ] && [ "$2" = --json ]; then echo '{"rows":[]}'; exit 0; fi
echo "unexpected args: $*" >&2; exit 2"#;

/// Tempdir whose read-only version directories are made writable again before removal.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture { dir: tempfile::tempdir().unwrap() }
    }
    fn builds(&self) -> Builds {
        Builds::new(self.dir.path().join("builds"))
    }
    /// A `#!/bin/sh` fake aho; `tag` makes the bytes (and so the hash) distinct.
    fn bin(&self, tag: &str, body: &str) -> PathBuf {
        let p = self.dir.path().join(format!("aho-{tag}"));
        std::fs::write(&p, format!("#!/bin/sh\n# {tag}\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let versions = self.dir.path().join("builds/versions");
        if let Ok(rd) = std::fs::read_dir(versions) {
            for d in rd.flatten() {
                let _ = std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755));
            }
        }
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

fn operator() -> Operator {
    Operator::authenticate(OperatorProof::local_tty())
}

fn entries(log: &MemoryLog) -> Vec<Entry> {
    log.read_branch(log.head().unwrap()).unwrap()
}

fn selfdev_entries(log: &MemoryLog) -> Vec<(String, Value)> {
    entries(log)
        .into_iter()
        .filter_map(|e| match e.kind {
            EntryKind::Custom { ns, name, body } if ns == NS => Some((name, body)),
            _ => None,
        })
        .collect()
}

fn names(log: &MemoryLog) -> Vec<String> {
    selfdev_entries(log).into_iter().map(|(n, _)| n).collect()
}

fn folded(log: &MemoryLog) -> Value {
    let mut folds = CustomFolds::new();
    folds.insert(NS.to_string(), fold);
    aho_log::fold(&entries(log), &folds).unwrap().custom[NS].clone()
}

fn ok_handoff(called: Arc<AtomicBool>) -> impl FnOnce(PathBuf) -> BoxFuture<'static, Result<(), String>> {
    move |p: PathBuf| -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            called.store(true, Ordering::SeqCst);
            let out = std::process::Command::new(&p).args(["plan", "--json"]).output().map_err(|e| e.to_string())?;
            if out.status.success() { Ok(()) } else { Err(format!("{} exited {}", p.display(), out.status)) }
        })
    }
}

#[test]
fn publish_is_immutable_idempotent_and_detects_tampering() {
    let fx = Fixture::new();
    let b = fx.builds();
    let bin = fx.bin("a", GOOD);
    let p = b.publish(&bin).unwrap();
    assert_eq!(p.hash, Hash::of(&std::fs::read(&bin).unwrap()).to_hex());
    assert_eq!(p.path, b.root().join("versions").join(&p.hash).join("aho"));
    assert_eq!(std::fs::read(&p.path).unwrap(), std::fs::read(&bin).unwrap());
    assert_eq!(mode(&p.path), 0o555);
    assert_eq!(mode(p.path.parent().unwrap()), 0o555);
    assert!(std::fs::write(&p.path, "x").is_err(), "a published build is not writable");

    assert_eq!(b.publish(&bin).unwrap(), p, "same bytes → same build");
    let leftovers: Vec<String> =
        std::fs::read_dir(b.root().join("versions")).unwrap().flatten().map(|d| d.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(leftovers, vec![p.hash.clone()], "no staging directories survive");

    let other = b.publish(&fx.bin("b", GOOD)).unwrap();
    assert_ne!(other.hash, p.hash);

    let dir = p.path.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&p.path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&p.path, "#!/bin/sh\necho evil\n").unwrap();
    let err = b.publish(&bin).unwrap_err();
    assert!(matches!(&err, SelfdevError::Tampered { hash, .. } if *hash == p.hash), "{err:?}");
    assert!(err.to_string().contains("publish again"), "{err}");
}

#[test]
fn smoke_passes_only_on_exit_zero_with_json() {
    let fx = Fixture::new();
    let b = fx.builds();
    let t = Duration::from_secs(10);

    let good = b.smoke(&b.publish(&fx.bin("good", GOOD)).unwrap(), t);
    assert!(good.ok, "{}", good.detail);

    let fail = b.smoke(&b.publish(&fx.bin("fail", "echo boom >&2; exit 1")).unwrap(), t);
    assert!(!fail.ok);
    assert!(fail.detail.contains("exited") && fail.detail.contains("boom"), "{}", fail.detail);

    let text = b.smoke(&b.publish(&fx.bin("text", "echo 'ready!'")).unwrap(), t);
    assert!(!text.ok);
    assert!(text.detail.contains("not JSON") && text.detail.contains("ready!"), "{}", text.detail);

    let noisy = b.smoke(
        &b.publish(&fx.bin("noisy", "i=0; while [ $i -lt 400 ]; do echo \"line $i of noise on stderr\" >&2; i=$((i+1)); done; exit 1"))
            .unwrap(),
        t,
    );
    assert!(!noisy.ok);
    assert!(noisy.detail.len() <= DETAIL_MAX, "{}", noisy.detail.len());
    assert!(noisy.detail.ends_with("line 399 of noise on stderr"), "the tail is kept: {}", noisy.detail);
}

#[test]
fn smoke_kills_a_build_that_outlives_the_timeout() {
    let fx = Fixture::new();
    let b = fx.builds();
    let p = b.publish(&fx.bin("slow", "sleep 30; echo '{}'")).unwrap();
    let started = Instant::now();
    let s = b.smoke(&p, Duration::from_millis(300));
    assert!(!s.ok);
    assert!(s.detail.contains("did not finish within 300 ms"), "{}", s.detail);
    assert!(started.elapsed() < Duration::from_secs(5), "killed, not waited out: {:?}", started.elapsed());
}

#[test]
fn smoke_boots_in_a_fresh_project_with_a_fresh_home() {
    let fx = Fixture::new();
    let b = fx.builds();
    let real_home = std::env::var("HOME").unwrap_or_default();
    let check = format!(
        r#"[ "$HOME" != "{real_home}" ] || {{ echo "HOME leaked" >&2; exit 1; }}
[ -d "$HOME" ] || {{ echo "no HOME dir" >&2; exit 1; }}
[ -z "$(ls -A)" ] || {{ echo "project not empty" >&2; exit 1; }}
[ "$(cd "$HOME/.." && pwd -P)" = "$(cd .. && pwd -P)" ] || {{ echo "project and home are not siblings" >&2; exit 1; }}
echo '{{"rows":[]}}'"#
    );
    let s = b.smoke(&b.publish(&fx.bin("env", &check)).unwrap(), Duration::from_secs(10));
    assert!(s.ok, "{}", s.detail);
}

#[test]
fn activate_and_rollback_restore_both_links() {
    let fx = Fixture::new();
    let b = fx.builds();
    let a = b.publish(&fx.bin("a", GOOD)).unwrap();
    let c = b.publish(&fx.bin("c", GOOD)).unwrap();
    let both = [Channel::Current, Channel::Canary];

    let first = b.activate(&a.hash, &both).unwrap();
    assert_eq!(first.previous, BTreeMap::from([(Channel::Current, None), (Channel::Canary, None)]));
    assert_eq!(b.channel(Channel::Current).as_deref(), Some(a.hash.as_str()));
    assert_eq!(b.channel(Channel::Canary).as_deref(), Some(a.hash.as_str()));
    assert_eq!(b.channel(Channel::Stable), None);
    assert_eq!(std::fs::read_link(b.root().join("current")).unwrap(), Path::new("versions").join(&a.hash), "links are relative");
    assert_eq!(std::fs::read(b.root().join("current/aho")).unwrap(), std::fs::read(&a.path).unwrap());

    let second = b.activate(&c.hash, &both).unwrap();
    assert_eq!(second.previous, BTreeMap::from([(Channel::Current, Some(a.hash.clone())), (Channel::Canary, Some(a.hash.clone()))]));
    assert_eq!(b.channel(Channel::Current).as_deref(), Some(c.hash.as_str()));

    b.rollback(&second).unwrap();
    assert_eq!(b.channel(Channel::Current).as_deref(), Some(a.hash.as_str()));
    assert_eq!(b.channel(Channel::Canary).as_deref(), Some(a.hash.as_str()));

    b.rollback(&first).unwrap();
    for name in ["current", "canary"] {
        assert!(std::fs::symlink_metadata(b.root().join(name)).is_err(), "`{name}` removed: it had no previous link");
    }

    let promoted = b.promote(&c.hash).unwrap();
    assert_eq!(promoted.previous, BTreeMap::from([(Channel::Stable, None)]));
    assert_eq!(b.channel(Channel::Stable).as_deref(), Some(c.hash.as_str()));
    assert_eq!(b.channel(Channel::Current), None, "promote moves only stable");

    let missing = "0".repeat(64);
    assert!(matches!(b.activate(&missing, &both), Err(SelfdevError::NotPublished { .. })));
    assert!(matches!(b.activate("../../etc", &both), Err(SelfdevError::NotPublished { .. })));
    let stray: Vec<String> = std::fs::read_dir(b.root())
        .unwrap()
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

#[tokio::test]
async fn reload_logs_every_phase_and_the_fold_agrees() {
    let fx = Fixture::new();
    let b = fx.builds();
    let log = MemoryLog::new();
    let called = Arc::new(AtomicBool::new(false));
    let bin = fx.bin("a", GOOD);
    let hash = reload(&operator(), &b, &log, &bin, Duration::from_secs(10), ok_handoff(called.clone())).await.unwrap();

    assert!(called.load(Ordering::SeqCst));
    assert_eq!(hash, Hash::of(&std::fs::read(&bin).unwrap()).to_hex());
    assert_eq!(names(&log), ["Published", "Smoke", "Activated", "Handoff"]);
    assert!(entries(&log).iter().all(|e| e.by == Principal::Operator));
    let es = selfdev_entries(&log);
    assert_eq!(es[1].1["ok"], json!(true));
    assert_eq!(es[2].1["pending"], json!(true));
    assert_eq!(es[2].1["channels"], json!(["current", "canary"]));
    assert_eq!(es[2].1["previous"], json!({"current": null, "canary": null}));
    assert_eq!(es[3].1, json!({"hash": hash, "outcome": "ok"}));
    assert_eq!(b.channel(Channel::Current).as_deref(), Some(hash.as_str()));
    assert_eq!(b.channel(Channel::Canary).as_deref(), Some(hash.as_str()));
    assert_eq!(b.channel(Channel::Stable), None);

    let s = folded(&log);
    assert_eq!(s["published"], json!([hash]));
    assert_eq!(s["smoke"][&hash]["ok"], json!(true));
    assert_eq!(s["pending"], Value::Null);
    assert_eq!(s["running"], json!(hash));
    assert_eq!(s["channels"], json!({"current": hash, "canary": hash}));
    assert_eq!(s["restore"], json!({"current": null, "canary": null}), "a rollback would remove both links");
}

#[tokio::test]
async fn a_failed_handoff_restores_both_links_and_logs_the_rollback() {
    let fx = Fixture::new();
    let b = fx.builds();
    let log = MemoryLog::new();
    let first = reload(&operator(), &b, &log, &fx.bin("a", GOOD), Duration::from_secs(10), ok_handoff(Arc::new(AtomicBool::new(false))))
        .await
        .unwrap();

    let saw = Arc::new(std::sync::OnceLock::<String>::new());
    let saw2 = saw.clone();
    let root = b.root().to_path_buf();
    let failing = move |p: PathBuf| -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let target = std::fs::read_link(root.join("current")).map_err(|e| e.to_string())?;
            let _ = saw2.set(target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            assert_eq!(p, root.join("current/aho"));
            Err("the new daemon exited before it was ready".to_string())
        })
    };
    let err = reload(&operator(), &b, &log, &fx.bin("b", GOOD), Duration::from_secs(10), failing).await.unwrap_err();
    let SelfdevError::HandoffFailed { hash: second, reason, rollback: None } = &err else { panic!("{err:?}") };
    assert_ne!(*second, first);
    assert_eq!(saw.get().map(String::as_str), Some(second.as_str()), "the handoff ran with the new build live");
    assert!(reason.contains("exited before it was ready"));
    assert!(err.to_string().contains("restored the previous"), "{err}");

    assert_eq!(b.channel(Channel::Current).as_deref(), Some(first.as_str()));
    assert_eq!(b.channel(Channel::Canary).as_deref(), Some(first.as_str()));
    assert_eq!(names(&log)[4..], ["Published", "Smoke", "Activated", "Handoff"]);
    let last = selfdev_entries(&log).pop().unwrap().1;
    assert_eq!(last["outcome"], json!("rolled_back"));
    assert_eq!(last["reason"], json!("the new daemon exited before it was ready"));

    let s = folded(&log);
    assert_eq!(s["pending"], Value::Null);
    assert_eq!(s["running"], json!(first), "the failed build never became the running one");
    assert_eq!(s["channels"], json!({"current": first, "canary": first}));
    assert_eq!(s["restored"], json!({"current": first, "canary": first}));
    assert_eq!(s["restore"], Value::Null);
}

#[tokio::test]
async fn a_failed_smoke_stops_before_activation() {
    let fx = Fixture::new();
    let b = fx.builds();
    let live = b.publish(&fx.bin("live", GOOD)).unwrap();
    b.activate(&live.hash, &RELOAD_CHANNELS).unwrap();

    for (tag, body) in [("exit1", "exit 1"), ("text", "echo hello"), ("slow", "sleep 30")] {
        let log = MemoryLog::new();
        let called = Arc::new(AtomicBool::new(false));
        let err =
            reload(&operator(), &b, &log, &fx.bin(tag, body), Duration::from_millis(500), ok_handoff(called.clone())).await.unwrap_err();
        assert!(matches!(err, SelfdevError::SmokeFailed { .. }), "{tag}: {err:?}");
        assert!(err.to_string().contains("no channel was changed"), "{err}");
        assert!(!called.load(Ordering::SeqCst), "{tag}: no handoff");
        assert_eq!(names(&log), ["Published", "Smoke"], "{tag}");
        assert_eq!(selfdev_entries(&log)[1].1["ok"], json!(false));
        assert_eq!(b.channel(Channel::Current).as_deref(), Some(live.hash.as_str()), "{tag}");
        assert_eq!(b.channel(Channel::Canary).as_deref(), Some(live.hash.as_str()), "{tag}");
        assert_eq!(folded(&log)["pending"], Value::Null);
    }
}

#[test]
fn fold_rejects_activation_without_smoke_and_handoff_without_pending() {
    let h = "a".repeat(64);
    let other = "b".repeat(64);
    let published = fold(None, "Published", &json!({"hash": h})).unwrap();
    let activated = |hash: &str| json!({"hash": hash, "channels": ["current", "canary"], "previous": {"current": null, "canary": null}, "pending": true});

    assert!(fold(None, "Smoke", &json!({"hash": h, "ok": true})).unwrap_err().contains("never Published"));
    assert!(fold(Some(&published), "Activated", &activated(&h)).unwrap_err().contains("without a successful Smoke"));
    let failed = fold(Some(&published), "Smoke", &json!({"hash": h, "ok": false, "detail": "exit 1"})).unwrap();
    assert!(fold(Some(&failed), "Activated", &activated(&h)).unwrap_err().contains("without a successful Smoke"));
    assert!(fold(Some(&published), "Handoff", &json!({"hash": h, "outcome": "ok"})).unwrap_err().contains("without a pending activation"));

    let smoked = fold(Some(&published), "Smoke", &json!({"hash": h, "ok": true, "detail": ""})).unwrap();
    let pending = fold(Some(&smoked), "Activated", &activated(&h)).unwrap();
    assert!(fold(Some(&pending), "Activated", &activated(&h)).unwrap_err().contains("still pending"));
    assert!(fold(Some(&pending), "Handoff", &json!({"hash": other, "outcome": "ok"})).unwrap_err().contains("is the pending activation"));
    assert!(fold(Some(&pending), "Handoff", &json!({"hash": h, "outcome": "maybe"})).is_err());
    assert!(fold(Some(&pending), "Rebooted", &json!({"hash": h})).unwrap_err().contains("unknown Selfdev entry"));
    let done = fold(Some(&pending), "Handoff", &json!({"hash": h, "outcome": "ok"})).unwrap();
    assert!(fold(Some(&done), "Handoff", &json!({"hash": h, "outcome": "ok"})).is_err(), "a Handoff closes the activation once");

    // Through the session fold: an out-of-order Selfdev entry is an invariant failure.
    let log = MemoryLog::new();
    let by = Principal::Operator;
    let custom = |name: &str, body: Value| EntryKind::Custom { ns: NS.into(), name: name.into(), body };
    log.append(None, by.clone(), custom("Published", json!({"hash": h}))).unwrap();
    log.append(None, by, custom("Activated", activated(&h))).unwrap();
    let mut folds = CustomFolds::new();
    folds.insert(NS.to_string(), fold);
    let err = aho_log::fold(&entries(&log), &folds).unwrap_err();
    assert!(err.to_string().contains("without a successful Smoke"), "{err}");
}

fn loadout(sets: &[(&str, &str)]) -> aho_config::Loadout {
    let mut s = Schema::default();
    for k in Composition::new("t").provide::<world_host::HostWorld>().provide::<Selfdev>().config_keys() {
        s.add(k);
    }
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    resolve(&i, &s, false).unwrap()
}

#[test]
fn capability_is_active_with_world_host_and_locked_without_it() {
    let fx = Fixture::new();
    let ws = tempfile::tempdir().unwrap();
    let builds = fx.dir.path().join("builds");
    let l = loadout(&[
        ("workspace.root", &ws.path().to_string_lossy()),
        ("workspace.shadow_dir", &fx.dir.path().join("shadow").to_string_lossy()),
        ("selfdev.builds_dir", &builds.to_string_lossy()),
        ("selfdev.smoke_timeout_ms", "1234"),
    ]);
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let policy = PolicyInput { preset: "workspace".into(), allowed: aho_types::ConsequenceSet::ALL };

    let comp = Composition::new("t").provide::<world_host::HostWorld>().provide::<Selfdev>();
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    assert!(matches!(p.row("selfdev").unwrap().state, CapState::Active), "{:?}", p.row("selfdev"));
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    let svc: SelfdevService = reg.service("selfdev").unwrap();
    assert_eq!(svc.builds.root(), builds.as_path());
    assert_eq!(svc.smoke_timeout, Duration::from_millis(1234));
    assert!(reg.entry_kinds.contains_key("Selfdev"));
    assert!(Selfdev::MANIFEST.tools.is_empty(), "never an agent tool");

    let alone = Composition::new("t").provide::<Selfdev>();
    let p = plan(&alone.manifests(), &l, &env, &policy, vec![]).unwrap();
    let row = p.row("selfdev").unwrap();
    assert!(matches!(&row.state, CapState::Locked { missing } if missing.iter().any(|m| m.contains("world-host"))), "{row:?}");
}

#[test]
fn config_defaults_and_home_expansion() {
    let d = <SelfdevConfig as aho_cap::Config>::from_loadout(&loadout(&[])).unwrap();
    assert_eq!(d, SelfdevConfig { builds_dir: "~/.aho/builds".into(), smoke_timeout: Duration::from_secs(30) });
    let home = std::ffi::OsStr::new("/home/op");
    assert_eq!(expand_home("~/.aho/builds", Some(home)).unwrap(), PathBuf::from("/home/op/.aho/builds"));
    assert_eq!(expand_home("~", Some(home)).unwrap(), PathBuf::from("/home/op"));
    assert_eq!(expand_home("/srv/builds", None).unwrap(), PathBuf::from("/srv/builds"));
    assert!(expand_home("~/.aho/builds", None).unwrap_err().contains("aho set selfdev.builds_dir"));
}
