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
- `delivery(f)` — sees each finished job before its `JobDelivered` is logged and may replace,
  suppress or defer it (the powers of a `HookEvent::JobDone` hook, typed and synchronous).
  `jobs-digest` is the worked example;
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

### Worked example: a delivery hook (`jobs-digest`)

A job's outcome reaches the model as a `JobDelivered` entry at a request boundary
([architecture](architecture.md), Jobs). Before it is logged, the delivery chain sees it:

```rust
// typed and synchronous, like post_execute
r.delivery(Arc::new(move |v: &JobView| digest.decide(v, Instant::now())));
// or a hook: the same decisions from a JSON payload, async, under a timeout
r.hook(HookSpec { on: &[HookEvent::JobDone], if_tool: Some("bash"), on_error: OnError::Deny, /* … */
    run: Arc::new(|_, _p| Box::pin(async { HookOutput { delivery: Delivery::Suppress, ..HookOutput::default() } })) });
```

- **View:** `JobView {op, call, tool, status, cause, content, deferred, with}`. `content` is what
  the model will see after earlier replacements (a hook gets it as text cut to 4096 chars, with
  `content_truncated`); `deferred` counts earlier holds; `with` lists every op offered at this
  delivery point.
- **Decisions (`Delivery`):** `Keep`; `Replace(content)`, which later steps see; `Suppress`: the
  entry is still logged, its content `Its output was suppressed by <cap>.`
  (`aho-session/prompts/jobs.md`); `Defer { until }`: held, then offered to the whole chain
  again at the first delivery point after `until` (while idle the session announces it again
  with `JobFinished`). After 16 holds a `Defer` counts as `Keep`.
- **Order:** every `delivery` callback, then every `JobDone` hook, each in registration order:
  the embedder's seed first, then the order capabilities start (providers, then by dependency,
  ties by id), call order within one. `Suppress` and `Defer` end the chain. `if_tool` limits a
  hook to one tool; a timed-out hook suppresses (`on_error: Deny`) or keeps (`Allow`). A
  `JobDone` hook's `context`, `stop` and `decision` are ignored.
- **When:** at the request boundary that delivers the job, or before a turn opens when a driver
  asks what is due; only the audit pairs are logged there.
- **Audit and replay:** each run is a `HookInvoked{point: "JobDone"}`/`HookResult` pair under
  `system:<cap>` (`matcher: "delivery"` for callbacks), decision `keep`, `replace`, `suppress`
  or `defer <n>ms`. `JobDelivered` holds exactly the decided content, so `verify_replay` sees
  what the model saw.
- **`jobs-digest`** (in `aho-base`, opt-in: `aho enable jobs-digest`; `jobs_digest.window_ms`,
  default 2000): the first delivery offered while no batch is open starts one and is deferred
  until `window_ms` later; deliveries offered before then join and are deferred to the same
  instant, so they come back together. There the first member offered (the lead) is replaced by
  the digest (a header, then each outcome under `## Job <op> (<tool>) <status>`) and every other
  member by `Its outcome is in the digest delivered with job <lead>.` Every op keeps its own
  `JobDelivered` (its status, then its `OpDone`: the fold closes each job once), the model reads
  the batch once, and the log holds exactly that. A batch of one is delivered unchanged; a
  member another capability held back from its lead's delivery point is batched again.

## Ship it

- config keys come from your `Config` impl; `aho set my.key value` and `aho explain my.key`
  work immediately;
- model-facing prose goes in `<your-crate>/prompts/*.md` via `include_str!` — the `prose`
  ratchet counts it, so expect to justify the bytes;
- `scripts/gate.sh` must stay green: fmt, clippy `-D warnings`, ratchets (panic, swallowed,
  long, wildcard, prose), the layer guard. New cross-layer edges need a deliberate baseline
  update.
