use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::{CapDeltaOp, CapState, EntryKind, LogStore, MemoryLog};
use aho_types::{Consequence, ConsequenceSet, Principal};

use crate::{Planned, bootstrap_entries, with_boot, world_refusal};

/// What the guest agent answered in the boot that motivated this (`TODO.md`, 2026-09-28).
const X: &str = "agent workspace is /work, not /workspace";
const CODING: &[&str] = &["tool-fs", "tool-bash"];

#[derive(Clone)]
struct FsH;
impl Handle for FsH {
    const KIND: HandleKind = HandleKind::FsRead;
}
#[derive(Clone)]
struct ProcH;
impl Handle for ProcH {
    const KIND: HandleKind = HandleKind::Subprocess;
}

/// A world whose open always fails with `X`; Locked until `aho-test-sky` is on PATH.
struct Sky;
impl Capability for Sky {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Err(BuildError::Probe(X.into()))
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(FsH);
        r.provide(ProcH);
    }
}
declare_capability!(
    Sky,
    "world-sky",
    kind = Kind::Provider,
    provides = [SeamId("fs"), SeamId("proc"), SeamId("sandbox")],
    requires = [Requirement::Binary("aho-test-sky")],
    tools = &[],
    provides_handles = [HandleKind::FsRead, HandleKind::Subprocess],
    exclusive = true,
    unlock = Fix::cli("aho set sky.recipe <recipe>"),
    enforced_by = "the sky"
);

struct FsDeps(#[allow(dead_code)] FsH);
impl Deps for FsDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::FsRead];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(FsDeps(r.handle()?))
    }
}
struct ToolFs;
impl Capability for ToolFs {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = FsDeps;
    type Config = ();
    fn build(_: FsDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolFs)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(ToolFs, "tool-fs", kind = Kind::Tool, provides = [], requires = [Requirement::Seam(SeamId("fs"))], tools = &[]);

struct ProcDeps(#[allow(dead_code)] ProcH);
impl Deps for ProcDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ProcDeps(r.handle()?))
    }
}
struct ToolBash;
impl Capability for ToolBash {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ProcDeps;
    type Config = ();
    fn build(_: ProcDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolBash)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(ToolBash, "tool-bash", kind = Kind::Tool, provides = [], requires = [Requirement::Seam(SeamId("proc"))], tools = &[]);

fn comp() -> Composition {
    Composition::new("boot-test").provide::<Sky>().consume::<ToolFs>().consume::<ToolBash>()
}

/// The plan with the world's binary present (`installed`) or not, and `disable`d capabilities.
fn planned(installed: bool, disable: &[&str]) -> Planned {
    let c = comp();
    let mut schema = Schema::default();
    for k in c.config_keys() {
        schema.add(k);
    }
    let mut i = ResolveInputs::default();
    i.disable = disable.iter().map(|d| d.to_string()).collect();
    let loadout = resolve(&i, &schema, false).unwrap();
    let mut env = Env::empty(Os::Linux);
    if installed {
        env.binaries.insert("aho-test-sky".into());
    }
    let policy = PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let plan = plan(&c.manifests(), &loadout, &env, &policy, vec![]).unwrap();
    Planned { loadout, plan, env, schema, manifests: c.manifests() }
}

#[test]
fn a_world_that_fails_to_open_names_its_own_error_on_itself_its_dependents_and_the_refusal() {
    let p = planned(true, &[]);
    let (_, broken) = comp().start(&p.plan, &p.loadout);
    let err = |id: &str| broken.iter().find(|(c, _)| c.name == id).map(|(_, e)| e.clone()).unwrap_or_default();
    assert!(err("world-sky").contains(X), "{broken:?}");
    assert!(err("tool-fs").starts_with("missing handle FsRead: world-sky is Broken: ") && err("tool-fs").contains(X), "{broken:?}");
    assert!(err("tool-bash").contains(X), "{broken:?}");

    let r = world_refusal(&p, &broken, CODING).expect("a Broken world refuses the session");
    let text = r.to_string();
    assert!(text.contains(&format!("the world `world-sky` is Broken: probe failed: {X}")), "{text}");
    assert_eq!(r.affected.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["tool-fs", "tool-bash"]);
    assert_eq!(r.affected[0].1, "missing handle FsRead", "the dependent's line does not repeat the world's error");
    assert_eq!(r.fixes, ["aho set sky.recipe <recipe>", "aho why world-sky"]);

    // What `aho plan`/`tree`/`why`/`status` show from the log that boot wrote.
    let log = MemoryLog::new();
    for k in bootstrap_entries(&p) {
        log.append(None, Principal::system("bootstrap"), k).unwrap();
    }
    for (id, err) in &broken {
        let op = CapDeltaOp::Broken { err: err.clone() };
        log.append(None, Principal::system("resolver"), EntryKind::CapDelta { id: id.name.into(), op, reason: "build failed".into() })
            .unwrap();
    }
    let state = aho_log::fold(&log.read_branch(log.head().unwrap()).unwrap(), &Default::default()).unwrap();
    let shown = with_boot(&p, &state);
    for id in ["world-sky", "tool-fs"] {
        assert!(matches!(&shown.plan.row(id).unwrap().state, CapState::Broken { err } if err.contains(X)), "{id}");
    }
    let tree = aho_inspect::render_tree("boot-test", &aho_inspect::tree(&shown.plan), &shown.plan);
    assert!(tree.lines().any(|l| l.contains("world-sky") && l.contains("Broken")) && tree.contains(X), "{tree}");

    // The same log says nothing about a different plan (the operator changed the config since).
    let other = planned(true, &["tool-bash"]);
    assert!(matches!(with_boot(&other, &state).plan.row("tool-fs").unwrap().state, CapState::Active));
}

#[test]
fn a_world_locked_at_plan_refuses_only_while_a_coding_tool_still_wants_it() {
    let p = planned(false, &[]);
    let r = world_refusal(&p, &[], CODING).expect("tool-fs and tool-bash are on and have no world");
    assert_eq!(r.world_error.as_deref(), Some("Locked: Binary(aho-test-sky)"));
    assert_eq!(r.affected.len(), 2, "{r:?}");
    assert_eq!(r.fixes.first().map(String::as_str), Some("./install-system-deps.sh"), "the world's own fix comes first");

    let one = world_refusal(&planned(false, &["tool-fs"]), &[], CODING).unwrap();
    assert_eq!(one.affected.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["tool-bash"], "a disabled tool-fs is not a reason");

    assert_eq!(world_refusal(&planned(false, &["tool-fs", "tool-bash"]), &[], CODING), None, "both switched off on purpose");
    assert_eq!(world_refusal(&planned(true, &[]), &[], CODING), None, "an Active world refuses nothing before it is built");
}
