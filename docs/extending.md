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
- `pre_execute` / `post_execute` — around tool execution.

Two phases: `plan(manifests, loadout, env)` is pure — seam bindings, requirement checks,
ceilings, fixes; `Composition::start` builds Active rows in dependency order and diffs what each
registered against its manifest (drift → Broken). `aho plan` renders the first phase; every
Locked row carries at least one `Fix` with the exact command.

## Ship it

- config keys come from your `Config` impl; `aho set my.key value` and `aho explain my.key`
  work immediately;
- model-facing prose goes in `<your-crate>/prompts/*.md` via `include_str!` — the `prose`
  ratchet counts it, so expect to justify the bytes;
- `scripts/gate.sh` must stay green: fmt, clippy `-D warnings`, ratchets (panic, swallowed,
  long, wildcard, prose), the layer guard. New cross-layer edges need a deliberate baseline
  update.
