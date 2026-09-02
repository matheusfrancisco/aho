//! aho-cap tests (split out for the file-size ratchet, doc 12).

use crate::*;
use aho_config::{ResolveInputs, Schema, resolve};

#[derive(Clone)]
struct FsH(#[allow(dead_code)] Arc<String>);
impl Handle for FsH {
    const KIND: HandleKind = HandleKind::FsRead;
}

struct Fs;
impl Capability for Fs {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();
    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(Fs)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.provide(FsH(Arc::new("fs".into())));
    }
}
declare_capability!(
    Fs,
    "fs-test",
    kind = Kind::Provider,
    provides = [SeamId("fs")],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::FsRead]
);

struct ReadDeps(#[allow(dead_code)] FsH);
impl Deps for ReadDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::FsRead];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ReadDeps(r.handle()?))
    }
}
static READ_TOOLS: &[ToolDecl] = &[ToolDecl::new("read", "Read a file", Consequence::ReadOnly).safe()];
struct ToolRead;
impl Capability for ToolRead {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ReadDeps;
    type Config = ();
    fn build(_: ReadDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolRead)
    }
    fn register(&self, r: &mut Registrar<'_>) {
        let exec: ToolExec = Arc::new(|_| Box::pin(async { ExecOutcome::text("ok") }));
        r.tool(&READ_TOOLS[0], exec);
    }
}
declare_capability!(
    ToolRead,
    "tool-read",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(SeamId("fs"))],
    tools = READ_TOOLS
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

static VM_TOOLS: &[ToolDecl] = &[ToolDecl::new("vm_start", "Start a VM", Consequence::ControlsVM)];
struct VmDeps;
impl Deps for VmDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::VmControl];
    fn resolve(_: &Resolver) -> Result<Self, MissingDep> {
        Ok(VmDeps)
    }
}
struct ToolVm;
impl Capability for ToolVm {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = VmDeps;
    type Config = ();
    fn build(_: VmDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolVm)
    }
    fn register(&self, _: &mut Registrar<'_>) {}
}
declare_capability!(
    ToolVm,
    "tool-vm",
    kind = Kind::Tool,
    provides = [],
    requires = [
        Requirement::Seam(SeamId("vm")),
        Requirement::Cap("vm-net-relay"),
        Requirement::Binary("qemu-system-aarch64"),
        Requirement::Platform(Os::Linux)
    ],
    tools = VM_TOOLS
);

fn loadout(sets: &[(&str, &str)], disable: &[&str]) -> Loadout {
    let mut s = Schema::default();
    for k in core_keys() {
        s.add(k);
    }
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    i.disable = disable.iter().map(|d| d.to_string()).collect();
    resolve(&i, &s, false).unwrap()
}

fn policy(allowed: &[Consequence]) -> PolicyInput {
    PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(allowed) }
}

#[test]
fn active_locked_and_fixes() {
    let ms: Vec<&'static Manifest> = vec![Fs::MANIFEST, ToolRead::MANIFEST, Gh::MANIFEST, ToolVm::MANIFEST];
    let p = plan(&ms, &loadout(&[], &[]), &Env::empty(Os::MacOs), &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    assert!(matches!(p.row("tool-read").unwrap().state, CapState::Active));
    let gh = p.row("mcp.github").unwrap();
    assert_eq!(gh.missing.len(), 1);
    assert_eq!(gh.missing[0].fix.as_deref(), Some("aho credential set github"));
    let vm = p.row("tool-vm").unwrap();
    let reqs: Vec<&str> = vm.missing.iter().map(|m| m.requirement.as_str()).collect();
    assert_eq!(vm.missing.len(), 4, "{reqs:?}");
    assert!(vm.missing.iter().all(|m| m.fix.is_some()), "every Locked reason names a fix");
    assert!(vm.over_preset.contains(Consequence::ControlsVM), "over-preset is flagged, not locked");
}

#[test]
fn disable_always_wins_and_is_attributed() {
    let ms: Vec<&'static Manifest> = vec![Fs::MANIFEST, ToolRead::MANIFEST];
    let p = plan(&ms, &loadout(&[], &["tool-read"]), &Env::empty(Os::Linux), &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    assert!(matches!(&p.row("tool-read").unwrap().state, CapState::Disabled { why } if why.contains("--disable tool-read")));
}

#[test]
fn duplicate_ids_fail_and_start_registers_tools_through_resolved_deps() {
    let ms: Vec<&'static Manifest> = vec![Fs::MANIFEST, Fs::MANIFEST];
    assert!(matches!(plan(&ms, &loadout(&[], &[]), &Env::empty(Os::Linux), &policy(&[]), vec![]), Err(PlanError::DuplicateId("fs-test"))));

    let comp = Composition::new("t").provide::<Fs>().consume::<ToolRead>();
    let l = loadout(&[], &[]);
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::Linux), &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    assert!(reg.tool("read").is_some());
}

#[test]
fn plan_hash_is_stable_and_moves_with_state() {
    let ms: Vec<&'static Manifest> = vec![Fs::MANIFEST, ToolRead::MANIFEST];
    let env = Env::empty(Os::Linux);
    let a = plan(&ms, &loadout(&[], &[]), &env, &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    let b = plan(&ms, &loadout(&[], &[]), &env, &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    let c = plan(&ms, &loadout(&[], &["tool-read"]), &env, &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    assert_eq!(a.hash, b.hash);
    assert_ne!(a.hash, c.hash);
}

#[test]
fn cancel_token_epoch_reset_refuses_newer_fire() {
    let t = CancelToken::new();
    let child = t.child();
    let e = t.cancel();
    assert!(child.is_cancelled());
    t.cancel();
    assert!(!t.reset_if_epoch(e), "a newer fire must survive a stale reset");
}

#[test]
fn ceiling_rejects_overclaiming_tools() {
    static BAD: &[ToolDecl] = &[ToolDecl::new("nuke", "", Consequence::SpawnsProcess)];
    struct Bad;
    impl Capability for Bad {
        const ID: CapabilityId = Self::ID_;
        const MANIFEST: &'static Manifest = &Self::MANIFEST_;
        type Deps = ReadDeps;
        type Config = ();
        fn build(_: ReadDeps, _: ()) -> Result<Self, BuildError> {
            Ok(Bad)
        }
        fn register(&self, _: &mut Registrar<'_>) {}
    }
    declare_capability!(Bad, "tool-bad", kind = Kind::Tool, provides = [], requires = [], tools = BAD);
    let ms: Vec<&'static Manifest> = vec![Fs::MANIFEST, Bad::MANIFEST];
    assert!(matches!(
        plan(&ms, &loadout(&[], &[]), &Env::empty(Os::Linux), &policy(&[]), vec![]),
        Err(PlanError::ClassAboveCeiling { tool: "nuke", .. })
    ));
}

#[test]
fn engineer_opt_in_is_unlocked_while_an_operator_off_is_disabled() {
    let ms: Vec<&'static Manifest> = vec![Fs::MANIFEST, ToolRead::MANIFEST];
    let mut s = Schema::default();
    for k in core_keys() {
        s.add(k);
    }
    let mut i = ResolveInputs::default();
    i.bundle = "product".into();
    i.bundle_defaults = vec![("caps.tool-read.enabled".into(), Value::Bool(false))];
    let engineer = resolve(&i, &s, false).unwrap();
    let p = plan(&ms, &engineer, &Env::empty(Os::Linux), &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    let row = p.row("tool-read").unwrap();
    assert_eq!(row.state, CapState::Unlocked, "05: `○ Unlocked  enable: aho enable tool-read`");
    assert!(matches!(&row.because, Because::OptIn { origin } if origin.contains("product")), "{:?}", row.because);

    i.sets = vec![("caps.tool-read.enabled".into(), "false".into())];
    let operator = resolve(&i, &s, false).unwrap();
    let p = plan(&ms, &operator, &Env::empty(Os::Linux), &policy(&[Consequence::ReadOnly]), vec![]).unwrap();
    assert!(matches!(p.row("tool-read").unwrap().state, CapState::Disabled { .. }), "an operator layer turning it off is Disabled");
}

#[test]
fn an_unchosen_world_names_seams_world_not_a_member_seam() {
    struct WorldA;
    impl Capability for WorldA {
        const ID: CapabilityId = Self::ID_;
        const MANIFEST: &'static Manifest = &Self::MANIFEST_;
        type Deps = ();
        type Config = ();
        fn build(_: (), _: ()) -> Result<Self, BuildError> {
            Ok(WorldA)
        }
        fn register(&self, _: &mut Registrar<'_>) {}
    }
    declare_capability!(
        WorldA,
        "world-host",
        kind = Kind::Provider,
        provides = [SeamId("fs"), SeamId("sandbox")],
        requires = [],
        tools = &[],
        exclusive = true
    );
    struct WorldB;
    impl Capability for WorldB {
        const ID: CapabilityId = Self::ID_;
        const MANIFEST: &'static Manifest = &Self::MANIFEST_;
        type Deps = ();
        type Config = ();
        fn build(_: (), _: ()) -> Result<Self, BuildError> {
            Ok(WorldB)
        }
        fn register(&self, _: &mut Registrar<'_>) {}
    }
    declare_capability!(
        WorldB,
        "world-ssh",
        kind = Kind::Provider,
        provides = [SeamId("fs"), SeamId("sandbox")],
        requires = [],
        tools = &[],
        exclusive = true
    );
    let ms: Vec<&'static Manifest> = vec![WorldA::MANIFEST, WorldB::MANIFEST];
    let p = plan(&ms, &loadout(&[("seams.world", "host")], &[]), &Env::empty(Os::Linux), &policy(&[]), vec![]).unwrap();
    let ssh = p.row("world-ssh").unwrap();
    assert_eq!(ssh.state, CapState::Unlocked);
    assert!(matches!(&ssh.because, Because::NotBound { seam: "world", chosen } if chosen == "host"), "{:?}", ssh.because);
}
