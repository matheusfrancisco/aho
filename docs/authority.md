# Authority

Authority flows one way. The model can request; it cannot construct permission.

## Principals as types

`crates/aho-policy/src/lib.rs`:

- `Operator` — the human. Constructible only from an `OperatorProof` a surface minted after
  authenticating; there is no other constructor.
- `Engineer` — compile time only: the `Composition`, bundle defaults.
- `Agent` — the model. Attributable, never privileged.
- `System(&str)` — attributable subsystems: `"compaction"`, `"gc"`, `"mcp:github"`.

APIs that widen anything take `&Operator`. An agent-authored widening is unrepresentable, not
merely denied.

## The lattice

`SandboxPolicy.allowed` is a set of consequence classes. It only shrinks (`narrow`); it grows
only through `widen_by_approval`, which needs an `Operator` and opens a new policy epoch in the
log. Presets are named cuts: `read-only`, `workspace`, `dev`, `lab`, `full`, `spec`; `custom` is
derived, never a switch target.

Child policy is `parent ⊓ agent declaration` — a subagent can never exceed its parent. An
`async` child cannot reach the operator for approval unless spawned with `bubble`.

## The decision chain

`aho_policy::decide` reviews every tool call, in order; steps 1–5 are never lifted by a later
step:

```
1. deny rules   2. ask rules   3. tool.check   4. guards (deny-only)   5. safety checks
6. preset ceiling   7. allow rules   8. passthrough (the preset is the allow)
```

- **tool.check** (step 3) is the tool's own opinion. For the shell tools it is the blast-radius
  classifier (`aho_policy::shell::assess`): lexical, parser-independent, a floor not a sandbox.
  `Safe`/`Low` pass, `Confirm` asks unless the call carries a real `justification` (≥ 25 chars,
  not a bare affirmation), `Catastrophic` is denied and never lifted.
- **guards** (step 4) are deny-only closures registered by capabilities. Example: with
  `tool-fs` mounted, shell invocations of `rg`/`fd` (and recursive `grep`) are denied with a
  message naming the mounted `grep`/`glob` tool; `… | grep` stream filtering stays allowed.
  Monotone by construction — a guard can refuse, it cannot permit.
- Under a confining world, a denied call can retry once with `escalate: [...]`, which raises an
  approval card; `aho allow <cap>` lifts the preset ceiling for one capability's calls, but
  steps 1–5 still run first.

A per-agent denial counter is the circuit breaker: past its limits the harness stops
auto-denying and asks a human, or aborts headless.

## Leases

VMs, sandboxes, and desktops are never bare handles. A `Lease` names its subject and expiry;
machines are supervised by `aho-vm-supervisor`, so a lease keeps expiring while no aho process
runs, and revocation is proven by the next use being refused.
