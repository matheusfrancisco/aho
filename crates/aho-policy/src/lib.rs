//! Authority flows one way. Three principals as types; the policy is a meet-only
//! lattice with no `widen()`; widening exists only as an `Operator` approval opening an epoch.
//!
//! * [`SandboxPolicy`] / [`Preset`]: the lattice and its named cuts.
//! * [`decide`]: the ordered, bypass-immune decision chain; steps 1–5 are never lifted.
//! * [`shell`]: the blast-radius classifier behind step 5 for shell commands.
//! * [`render`] / [`render_outcome`]: the one renderer, in message voice.

#![forbid(unsafe_code)]

mod chain;
mod glob;
mod paths;
mod render;
mod rules;
pub mod shell;
mod untrusted;

use aho_cap::{BoundWorld, PolicyView};
use aho_types::*;

pub use chain::{CallUnderReview, ChainOutcome, DecisionReason, SafetyKind, decide, meet, safety_check};
pub use render::{ApprovalCard, render, render_outcome, suggested_cut};
pub use rules::{Rule, RuleBehavior, parse_rules};
pub use untrusted::Untrusted;

// ───────────────────────────── principals ─────────────────────────────

/// The human running the harness. Constructible only from an [`OperatorProof`] a surface minted
/// after authenticating; the model can request, it cannot construct this.
pub struct Operator(());
/// Compile time only: `Composition`, bundle defaults.
pub struct Engineer(());
pub struct Agent {
    pub id: AgentId,
    pub turn: TurnId,
}
/// Attributable, never privileged: `"mcp:github"`, `"gc"`, `"compaction"`.
pub struct System(pub &'static str);

/// Opaque proof that a surface authenticated as the operator.
pub struct OperatorProof(());

impl OperatorProof {
    /// The local terminal the CLI runs in: whoever holds the TTY is the operator.
    pub fn local_tty() -> OperatorProof {
        OperatorProof(())
    }
}

impl Operator {
    pub fn authenticate(proof: OperatorProof) -> Operator {
        let OperatorProof(()) = proof;
        Operator(())
    }
    pub fn principal(&self) -> Principal {
        Principal::Operator
    }
}

impl Engineer {
    #[doc(hidden)]
    pub const fn __compile_time() -> Self {
        Engineer(())
    }
    pub fn principal(&self) -> Principal {
        Principal::Engineer
    }
}

impl Agent {
    pub fn principal(&self) -> Principal {
        Principal::Agent { id: self.id, turn: self.turn }
    }
}

impl System {
    pub fn principal(&self) -> Principal {
        Principal::system(self.0)
    }
}

// ───────────────────────────── presets ─────────────────────────────

/// Lattice cuts. `Custom` is derived when the allowed set matches no preset and is never a
/// switch target (`preset("custom")` is `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Preset {
    ReadOnly,
    Workspace,
    Dev,
    Lab,
    Full,
    /// Read-only outside `spec/**` plus checkers.
    Spec,
    Custom,
}

/// Parses a preset name as written in `[policy] preset = "…"` or `aho preset <name>`.
pub fn preset(name: &str) -> Option<Preset> {
    Preset::SWITCHABLE.into_iter().find(|p| p.name() == name.trim())
}

/// Classes a per-call escalation can grant at the point of denial under a confining world.
/// VMs and desktops go through leases; `TalksToHuman` is the approver itself.
pub const ESCALATABLE: ConsequenceSet = ConsequenceSet::EMPTY
    .with(Consequence::WorkspaceWrite)
    .with(Consequence::HostWrite)
    .with(Consequence::NetworkEgress)
    .with(Consequence::SpawnsProcess);

impl Preset {
    /// Every preset a switch may name, narrowest first.
    pub const SWITCHABLE: [Preset; 6] = [Preset::ReadOnly, Preset::Workspace, Preset::Dev, Preset::Lab, Preset::Full, Preset::Spec];
    /// The linear chain `read-only ⊂ workspace ⊂ dev ⊂ lab ⊂ full` used to suggest the smallest cut.
    const CHAIN: [Preset; 5] = [Preset::ReadOnly, Preset::Workspace, Preset::Dev, Preset::Lab, Preset::Full];

    pub fn name(&self) -> &'static str {
        match self {
            Preset::ReadOnly => "read-only",
            Preset::Workspace => "workspace",
            Preset::Dev => "dev",
            Preset::Lab => "lab",
            Preset::Full => "full",
            Preset::Spec => "spec",
            Preset::Custom => "custom",
        }
    }

    /// The allowed set this preset grants in `world`. Under a non-confining world (`host`,
    /// policy only) `dev`/`lab` also allow `HostWrite`: there the tools bind to host
    /// providers and the class is real. `Custom` has no cut of its own and falls to Nothing.
    pub fn cut(&self, world: &BoundWorld) -> ConsequenceSet {
        use Consequence::*;
        let workspace = ConsequenceSet::of(&[ReadOnly, WorkspaceWrite, SpawnsProcess, TalksToHuman]);
        let dev = workspace.with(NetworkEgress);
        let lab = dev.with(ControlsVM).with(ControlsDesktop);
        let host = |s: ConsequenceSet| if world.confines { s } else { s.with(HostWrite) };
        match self {
            Preset::ReadOnly => ConsequenceSet::of(&[ReadOnly]),
            Preset::Workspace => workspace,
            Preset::Dev => host(dev),
            Preset::Lab => host(lab),
            Preset::Full => ConsequenceSet::ALL,
            Preset::Spec => workspace,
            Preset::Custom => ConsequenceSet::EMPTY,
        }
    }

    /// The preset whose cut is exactly `allowed`, else `Custom`. `Spec` is never derived
    /// (its cut equals `workspace`; the difference is its content restriction).
    pub fn derive(allowed: ConsequenceSet, world: &BoundWorld) -> Preset {
        Preset::CHAIN.into_iter().find(|p| p.cut(world) == allowed).unwrap_or(Preset::Custom)
    }

    /// Smallest preset on the chain whose cut contains `need`.
    pub fn smallest_containing(need: ConsequenceSet, world: &BoundWorld) -> Option<Preset> {
        Preset::CHAIN.into_iter().find(|p| need.is_subset(&p.cut(world)))
    }
}

// ───────────────────────────── lattice ─────────────────────────────

/// The folded policy. `allowed` only shrinks through [`SandboxPolicy::narrow`]; it grows only
/// through [`SandboxPolicy::widen_by_approval`], which needs an [`Operator`] and opens an epoch.
#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    allowed: ConsequenceSet,
    world: BoundWorld,
    workspace: String,
    home: String,
    epoch: Epoch,
    preset: Preset,
    /// `WorkspaceWrite` is limited to `spec/**`. Survives narrowing and widening; only a
    /// new policy (leaving spec mode) clears it.
    spec_scope: bool,
    approval: Option<EntryId>,
}

impl SandboxPolicy {
    pub fn new(allowed: ConsequenceSet, world: BoundWorld, workspace: String, home: String, epoch: Epoch) -> Self {
        let preset = Preset::derive(allowed, &world);
        SandboxPolicy { allowed, world, workspace, home, epoch, preset, spec_scope: false, approval: None }
    }

    /// A policy at exactly `preset`'s cut, with the preset's content restriction.
    pub fn from_preset(preset: Preset, world: BoundWorld, workspace: String, home: String, epoch: Epoch) -> Self {
        let allowed = preset.cut(&world);
        SandboxPolicy { allowed, world, workspace, home, epoch, preset, spec_scope: preset == Preset::Spec, approval: None }
    }

    /// Meet with a preset: `allowed ∩ cut`, plus the preset's content restriction and label.
    /// Never widens, so anyone may call it (entering plan or spec mode).
    pub fn under_preset(&self, preset: Preset) -> Self {
        let allowed = self.allowed.intersect(preset.cut(&self.world));
        let spec_scope = self.spec_scope || preset == Preset::Spec;
        let label = if allowed == preset.cut(&self.world) { preset } else { Preset::Custom };
        SandboxPolicy { allowed, preset: label, spec_scope, ..self.clone() }
    }

    pub fn allowed(&self) -> ConsequenceSet {
        self.allowed
    }
    pub fn world(&self) -> &BoundWorld {
        &self.world
    }
    pub fn workspace(&self) -> &str {
        &self.workspace
    }
    pub fn home(&self) -> &str {
        &self.home
    }
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
    pub fn preset(&self) -> Preset {
        self.preset
    }
    /// True in spec mode: writes are limited to `spec/**` under the workspace.
    pub fn writes_limited_to_spec(&self) -> bool {
        self.spec_scope
    }
    /// The `Approval/Decided` entry that opened this epoch, if an operator widened it.
    pub fn approval_ref(&self) -> Option<EntryId> {
        self.approval
    }

    fn label_for(&self, allowed: ConsequenceSet) -> Preset {
        if allowed == self.allowed {
            self.preset
        } else if self.spec_scope {
            if allowed == Preset::Spec.cut(&self.world) { Preset::Spec } else { Preset::Custom }
        } else {
            Preset::derive(allowed, &self.world)
        }
    }

    /// Meet. Anyone may call. There is no `widen()`.
    pub fn narrow(&self, by: ConsequenceSet) -> Self {
        let allowed = self.allowed.intersect(by);
        SandboxPolicy { allowed, preset: self.label_for(allowed), ..self.clone() }
    }

    /// The only way `allowed` grows: an `Operator` approval reference. Result is
    /// `allowed ∪ to` in a new epoch that records the approval. Content restrictions stay.
    pub fn widen_by_approval(&self, _op: &Operator, approval: EntryId, to: ConsequenceSet) -> Self {
        let allowed = self.allowed.union(to);
        SandboxPolicy {
            allowed,
            preset: self.label_for(allowed),
            epoch: Epoch(self.epoch.0.saturating_add(1)),
            approval: Some(approval),
            ..self.clone()
        }
    }

    pub fn view(&self) -> PolicyView {
        PolicyView {
            epoch: self.epoch,
            allowed: self.allowed,
            world: self.world.clone(),
            workspace: self.workspace.clone(),
            home: self.home.clone(),
        }
    }
}

// ───────────────────────────── children ─────────────────────────────

/// Child policy = parent ⊓ agent declaration. Async agents cannot talk to a human unless bubbled.
pub fn child_policy(parent: &SandboxPolicy, decl_cut: ConsequenceSet, is_async: bool, bubble: bool) -> SandboxPolicy {
    let p = parent.narrow(decl_cut);
    if is_async && !bubble { p.narrow(ConsequenceSet::ALL.without(Consequence::TalksToHuman)) } else { p }
}

/// Denial circuit breaker per agent: past the limits the harness stops auto-denying and
/// asks a human, or aborts headless.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DenialCounter {
    pub consecutive: u32,
    pub total: u32,
}

impl DenialCounter {
    pub const MAX_CONSECUTIVE: u32 = 3;
    pub const MAX_TOTAL: u32 = 20;

    /// A denial bumps both counters; anything that ran resets the consecutive run.
    pub fn record(&mut self, denied: bool) {
        if denied {
            self.consecutive = self.consecutive.saturating_add(1);
            self.total = self.total.saturating_add(1);
        } else {
            self.consecutive = 0;
        }
    }

    pub fn tripped(&self) -> bool {
        self.consecutive >= Self::MAX_CONSECUTIVE || self.total >= Self::MAX_TOTAL
    }
}

// ───────────────────────────── leases ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaseSubject {
    Vm(String),
    Sandbox(SessionId),
    Desktop(SessionId),
    Job(JobId),
    Worktree(String),
    Session(SessionId),
    Swarm(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attestation {
    pub image_hash: Option<Hash>,
    pub recipe: Option<String>,
    pub isolation_rules: Vec<String>,
    pub port_maps: Vec<(u16, u16)>,
}

/// VMs, sandboxes, desktops are never bare handles. Revocation must show the next use was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub id: LeaseId,
    pub subject: LeaseSubject,
    pub attest: Attestation,
    pub to: AgentId,
    pub expires: Timestamp,
    pub max_disk_bytes: u64,
    pub max_cpu_s: u64,
}

#[cfg(test)]
mod tests;
