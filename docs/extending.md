# Extending aho

You extend aho by implementing `aho_cap::Capability` in a new crate and adding one line to a
`Composition`. You never fork aho itself. A tool, a provider, a world, a surface, a hook, a
panel: all the same contract.

## The shape

```rust
use aho_cap::*;

pub struct MyTool { /* handles from Deps */ }

impl Capability for MyTool {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = MyDeps;      // typed handles; `Deps::REQUIRES` names the seams
    type Config = ();        // or a `Config` impl contributing `my.*` keys
    fn build(deps: MyDeps, cfg: ()) -> Result<Self, BuildError> { … }
    fn register(&self, r: &mut Registrar<'_>) { … }
}

declare_capability!(MyTool, "my-tool", kind = Kind::Tool,
    provides = [], requires = [Requirement::Seam(aho_fs::SEAM)], tools = TOOLS);
```

Compile-time rules, pinned by `trybuild` tests in `aho-cap`: a `ToolDecl` without a consequence
class does not compile; reserved id prefixes (`mcp__`) are rejected.

## What `register` can contribute

Via the `Registrar` (each recorded under the capability's grant):

- `tool(decl, exec)` — a `ToolDecl` (class, schema, concurrency, `check`, presenter, op kind)
  and its executor;
- `guard(f)` — deny-only review of any call (decision-chain step 4). Monotone: it can refuse,
  never permit. `tool-fs`'s shell-search redirect is the worked example;
- `prompt_section(order, name, cache_break, f)` — a system-prompt section rendered from the
  folded `State`;
- `context(f)` — dynamic text logged as a `Snapshot` user message only when it changes;
- `hook(spec)` — lifecycle hooks (pre-step, turn-stopping, …);
- `entry_kind(ns, fold)` — namespaced custom log entries with their own fold;
- `service(name, value)` — a shared object other capabilities may look up;
- `panel(slot, projections)` — a surface panel over fold projections;
- `pre_execute` / `post_execute` — around tool execution. `post_execute` is synchronous and may
  replace a result's content; `tool-lsp`'s diagnostics-on-write ([lsp](lsp.md)) is the worked
  example.

Two phases: `plan(manifests, loadout, env)` is pure — seam bindings, requirement checks,
ceilings, fixes; `Composition::start` builds Active rows in dependency order and diffs what each
registered against its manifest (drift → Broken). `aho plan` renders the first phase; every
Locked row carries at least one `Fix` with the exact command.

### Worked example: a turn-stopping hook (`verify-gate`)

`crates/tools/verify-gate` (in `aho-base`) registers one `HookSpec` on `HookEvent::TurnStopping`.
The payload is `{turn, transition, stop_hook_active}`. A `Deny(reason)` on a `Completed` turn
keeps the turn going: the session logs `StepEnd{StopHookBlocking}`, appends the reason as a
notice, and the model takes another step. It does that once per turn; the next stop arrives with
`stop_hook_active: true` and any denial is ignored, so the gate returns no decision there.

- **Trigger:** the turn has a `ToolCall` classed `WorkspaceWrite`, `HostWrite` or `SpawnsProcess`
  (read from `aho.log`). With the world's `Snapshot` and a checkpoint from before the turn (no
  rewind since), the current tree must also differ from that checkpoint.
- **Command:** `verify.command`, or detected through the world's `FsRead`: `Cargo.toml` →
  `cargo test --quiet`; `package.json` with a real `test` script → `npm test --silent`;
  `pyproject.toml` / `pytest.ini` → `python3 -m pytest -q` (exit 5, no tests, passes); `go.mod`
  → `go test ./...`. No command, no check.
- **Run:** `/bin/sh -c <command>` through the world's `SubprocessHandle` with `verify.timeout_s`
  (default 300). A timeout counts as a failure.
- **Reason:** `verify: <command> exited <code>` (or `timed out after <n>s`), then the last 4 KiB
  of output (`prompts/failed.md`).
- **Off switch:** `verify.enabled = false`.
- **Audit:** each run is a `HookInvoked`/`HookResult` pair under `system:verify-gate`. Its
  decision is `Allow` (passed), `Deny(…)` (failed) or `none` (not checked).

## Ship it

- config keys come from your `Config` impl; `aho set my.key value` and `aho explain my.key`
  work immediately;
- model-facing prose goes in `<your-crate>/prompts/*.md` via `include_str!` — the `prose`
  ratchet counts it, so expect to justify the bytes;
- `scripts/gate.sh` must stay green: fmt, clippy `-D warnings`, ratchets (panic, swallowed,
  long, wildcard, prose), the layer guard. New cross-layer edges need a deliberate baseline
  update.
