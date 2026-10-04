# Configuration and profiles

Every setting is a dotted key (`model.name`, `caps.tool-bash.shell`). `aho_compose::load` hands the
layer stack to `aho_config::resolve`, which keeps every value with its origin: `aho plan` prints the
winner per key, `aho explain <key>` prints every layer it beat.

## Layers

Lowest to highest; within one layer, the input read later wins.

| layer | source |
|---|---|
| Default | the `ConfigKey` default a capability declares |
| Bundle | `Composition::default` (engineer defaults, e.g. `policy.preset = "workspace"`) |
| Catalog | values derived from the model catalog for `model.provider` / `model.name` |
| Profile | `--profile <name>`: built-in fragments, then `~/.aho/profiles/<name>.toml` |
| FileHome | `~/.aho/config.toml` |
| FileProject | `./aho.toml` |
| Env | `AHO_<KEY>` for declared keys (`sandbox.mode` → `AHO_SANDBOX_MODE`); `AHO_DISABLE=a,b` |
| Arg | `--set key=value`; a `--loadout <name>` table |
| Runtime | changes applied inside a running session (e.g. an approved `unlock_request`) |

`disable = ["id", …]` (any file), `AHO_DISABLE`, and `--disable <id>` are not keys: their ids add up
across layers and always win, so nothing below can undo them. To make a switch that a higher layer
can flip back, use `caps.<id>.enabled = false` instead.

## Turning capabilities off and on

- `caps.<id>.enabled = false` at Default/Bundle: opt-in (`○ Unlocked`, `aho enable <id>`).
- `caps.<id>.enabled = false` at Profile or above: `Disabled`, naming the layer
  (`aho why <id>`; the model's `aho_why` shows the same).
- `caps.<id>.enabled = true` at a higher layer undoes either.

`caps` is a table key, so a `caps.<id>` switch for a capability this binary does not link resolves
silently, even under `--strict`; it simply has no row to act on.

## Profiles

`--profile <name>` adds a Profile layer. It is refused (nothing loads) only when the name has
neither a built-in fragment nor a `~/.aho/profiles/<name>.toml`; the error lists the built-in names.

A bundle ships a built-in fragment with `Composition::profile(name, toml)`; `include` keeps the
fragment and its bundle. With `--profile <name>`, the layer order is:

1. built-in fragments of `<name>`, in include order (`builtin:aho-base/profiles/coding.toml`, then
   `builtin:aho-lab/profiles/coding.toml`); a later fragment wins a key both set;
2. `~/.aho/profiles/<name>.toml`, if present: it overrides those keys it names and inherits the rest
   (same rank, read later);
3. then `~/.aho/config.toml`, `./aho.toml`, env, flags as above.

So `~/.aho/profiles/coding.toml` with `[caps.tool-goals] enabled = true` brings goals back for coding
sessions only, and `[caps.tool-lsp] enabled = false` drops LSP from them.

## Built-in `coding`

`aho --profile coding run` (any verb takes it: `aho --profile coding plan` shows the result). The
fragments live in `crates/bundles/aho-base/profiles/coding.toml` and
`crates/bundles/aho-lab/profiles/coding.toml`.

| off | why |
|---|---|
| tool-goals | multi-round goal tracking; the todo list covers a coding turn |
| tool-schedule | timed prompts belong to the daemon, not an edit loop |
| tool-workflow | the `ralph` loop is operator-driven (`aho ralph`) |
| tool-tasks | the task-graph swarm; `subagent` (scout/reviewer) covers delegation |
| tool-mcp-resources | generic MCP resource browsing; servers' own tools stay |
| tool-session | session/event log queries inspect aho, not the code |
| tool-memory, memory-local | memory tools, plus the per-turn recall snapshot and extraction calls |
| tool-vm, tool-display | VM lab and desktop control (`seams.world` is untouched) |
| tool-formal | TLA+/Lean checkers belong to spec mode (`aho spec`) |

Kept as shipped: tool-fs, tool-bash, tool-jobs, tool-lsp, tool-run-code, tool-todo, tool-subagent,
tool-skills, tool-web-fetch / tool-web-search, tool-plan-mode, tool-inspect (`aho_why`,
`unlock_request`, so the model can learn why a tool is missing and ask), MCP servers' tools.
Switched on: `verify-gate` and `context-files`. Opt-in capabilities (tool-terminal,
hooks-claude-code, telemetry) stay opt-in.

`coding` is not the default; `aho run` without `--profile` resolves as before.

## Jobs

Session jobs (background operations; [architecture](architecture.md#jobs)) read these core keys
at boot; a value of the wrong shape refuses the boot and names the key.

| key | default | |
|---|---|---|
| `jobs.detach.<tool>` | the tool's decl (aho-base: `subagent*`, `send_message`, `run_code` `on_request`; `bash` `after:60s`) | `never` \| `on_request` \| `always` \| `after:<n>ms\|s\|m\|h` |
| `jobs.on_rewind` | `cancel` | `keep`: jobs the rewind target holds keep running |
| `jobs.deliver` | `boundary` | `on_wait`: mid-turn, deliver only around a `job_wait` call; the rest at the next turn's start |
| `jobs.wake` | `idle` | a job finished while idle: `idle` starts a wake turn (`System(jobs)`), `operator` only tells the operator, `never` waits for the next prompt |
| `jobs.wake_budget` | `10` | wake turns since the operator's last prompt |
| `jobs.max` | `8` | jobs running at once; a live `aho lease grant --jobs <n>` replaces it |
| `jobs.settle_timeout_s` | `1800` | how long `aho run -p` waits for its jobs before cancelling them |
