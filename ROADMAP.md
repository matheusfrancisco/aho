# Roadmap

Standing rules, carried from the original phase plan:

- every item ends with something an operator can run and an invariant a test pins; no item
  ships a stub for the next;
- model-facing prose is ratcheted (`scripts/ratchets/prose.json`); a new prose line needs a
  scripted transcript (`llm-scripted`) proving it reaches the request, and judgment prose also a
  recorded real-model eval with and without it (docs/prompts.md).

## Shipped

The phased build-out (contract → loop → policy/worlds → operator surface → web/lab → breadth →
memory/swarm → daemon/self-reload) is implemented and pinned by `cargo test --workspace` plus
`scripts/gate.sh`. Highlights: four worlds with aho-net; the 8-step decision chain with
epochs, leases, and the blast-radius classifier; replay-equal session logs trace-checked
against TLA+ specs; six surfaces over one `Session`; subagents (in-process, ACP, observed
Claude/Codex CLIs); generated TS/Python SDKs with drift checks; provider logins; driver-level
stop/loop/budget guards; gated prompt prose with bundled scout/reviewer roles.

Added since: `aho stats` (local, log-derived latency/token/tool/approval aggregates; a
`sidebar.stats` web panel reads the same projection) and the opt-in `telemetry` capability —
events derive from the closed vocabulary in `aho-inspect` (no `String` field can reach the
wire), `caps.telemetry.enabled = false` until an operator layer flips it, `DO_NOT_TRACK`
refuses the build, a 4xx latches the endpoint off, and `aho telemetry preview` prints exactly
what one flush would send.

Code quality (2026-10-03): `aho run` refuses a session whose world is broken or that lost
its coding tools, showing the world's own error; model limits (`max_tokens`, thinking budget,
context window) come from the catalog unless the operator sets them; `verify-gate` runs the
project's check before a turn that changed the workspace may end; `tool-lsp` appends
diagnostics to write results and sends later ones (on-save `cargo check`) as a late notice;
`context-files` sends AGENTS.md/CLAUDE.md as context; the contract carries test-craft rules;
a built-in `coding` profile (`aho --profile coding`) trims the tool list from 53 to 33; and
`scripts/evals/test-quality` is the operator-invoked eval for judgment prose.

## Next

Ordered by risk retired per unit of work; details in [TODO.md](TODO.md).

1. ~~Real-guest VM validation~~ — done 2026-09-28: Ubuntu 24.04 arm64 through media → recipe
   → lease → guest agent (QEMU 11.1/HVF, macOS); details in TODO.md's closed entry.
2. ~~World-error surfacing~~ — done 2026-10-03: the world's error reaches `plan`/`status`/
   `why`/`shell`, and `aho run` refuses with it (docs/worlds.md).
3. **`swallowed` ratchet paydown** — the gate is red today; either fix the sites or accept
   them deliberately. A red gate that stays red trains people to ignore it.
4. **Gated live acceptance for foreign CLIs and providers** — one operator-invoked run for
   `claude`/`codex` adapters and one per login route; never in unattended CI.
5. **Unprivileged namespace world** — prove `world-ns` on a stock distro without a privileged
   container, or narrow the documented claim.
6. **`aho session import`** — foreign transcripts folded into valid logs; `session verify`
   green on the result.

### The "sessions as branches" arc

One writer per session is now enforced (the 2026-09-28 lock); that unlocks a three-step arc,
each independently shippable:

1. ~~**Writer lock**~~ — done: exclusive OS lock on the jsonl at first mutation; second and
   stale writers refused loudly; 2s patience for a dying holder covers ownership handoff.
2. **Multi-TUI attach** — `aho tui` becomes a client: the first process on a session owns the
   log (or the daemon does) and later TUIs discover the owner socket and attach over rpc
   (`subscribe_from` replay, prompts/steers as queued inputs, approval cards broadcast,
   first answer wins). Invariant: a direct open against a held lock fails with the owner's
   address — never races.
3. **Fork-from-viewer** — any attached TUI splits off at an entry (chat-only) or a checkpoint
   (chat+code): `session.fork { at, mode }` executed by the owner; the fork inherits the log
   prefix, `fold(log[..N]).policy`, and the checkpoint's worktree — never the lease. vm world
   gains the same via qcow2 snapshots per checkpoint, which also closes its time-travel gap.
   Invariant: source and fork both `verify` green, share bytes ≤ N, diverge only after.
   `session diff`/`merge` complete the loop: fork → diverge → diff → merge.

## Later

Direction, not commitment; each enters *Next* only with a runnable result attached.

- **WASM component loading for untrusted tools** — viable for tools, not for worlds; gives
  third-party tools a boundary stronger than the decision chain.
- **`cargo vet` policy for third-party capabilities** — provenance for the extension
  ecosystem the `Capability` contract invites.
- **Richer role library** — further subagent roles only where an output shape earns its
  bytes against the prose ratchet; authority always from presets, never text.
- **Desktop shell and bilingual docs** — after the surfaces underneath stop moving.
