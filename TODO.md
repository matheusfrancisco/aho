# TODO

Unfinished work and known gaps, with what closing each one looks like. Direction lives in
[ROADMAP.md](ROADMAP.md).

## Not implemented

- `aho session import` — ingest transcripts from other harnesses. Closed when a foreign
  transcript folds into a valid log and `aho session verify` passes on the result.
- `aho craft` — closed when the verb exists or the stub reference is removed from the CLI.

## Known gaps

- **VM world, no PTY**: under `world = vm`, bare `aho shell` opens no PTY into the guest; only
  `aho shell -- <cmd>` works.
- **VM world, no checkpoints**: workspace checkpoints (and therefore file rewind) are
  unavailable; the sandbox VM's disk is the workspace.
- **VM world, one host channel**: the guest agent serves one channel, so `aho export` fails
  while the session holds it. Closed by multiplexing or queueing on the channel.
- **Namespace overlay lifetime**: the recipe root's overlay writes last only while the keeper
  runs; `/workspace` and `/session` persist across keeper restarts. Document-only gap unless a
  persistent overlay is wanted.

## Validation debt (tests pass, reality unproven)

- **No real QEMU guest has booted**: VM tests and smokes run against a stub `qemu-system-*`
  (real QMP, real supervisor, real guest agent, no real boot). Closed by one end-to-end run:
  `aho media import` a real image → `aho add recipe` → `aho lease grant` → sandbox VM answers
  through the guest agent.
- **Foreign CLIs never launched for real**: `subagent-claude` / `subagent-codex` are covered by
  recorded transcript fixtures and fake CLIs in real tmux; the real `claude` and `codex`
  binaries were never launched with a prompt (spends operator credits). Closed by one gated,
  operator-invoked acceptance run.
- **Namespace world ran only in a privileged Linux container**; unprivileged stock-distro runs
  are unproven.
- **Provider logins**: local protocol servers exercise login, refresh, and streamed turns; CI
  does not cover live provider acceptance or billing behavior.

## Engineering debt

- **`swallowed` ratchet is failing** (gate: `total grew: 101 -> 165`): new and grown
  `let _ =` / `.ok();` sites in `net-userspace` (25), `world-ns` (15), `world-vm` (4),
  `vm-guest`/`vm-guest-agent` (8), `mcp-stdio/client` (+4), `surface-tui/runtime` (+4),
  `aho-proc` (+1), `display-xserver` (3). Closed by handling or annotating each site, or by a
  deliberate `scripts/gate.sh --fix` with review — not by letting the baseline drift.
