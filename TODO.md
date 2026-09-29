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

## Enforcement gaps

Places where the kernel or aho-net could enforce what the decision chain only reviews today.
Order: the first two come first; credential binding and method/path rules share TLS
termination in aho-net; the widening diff reports what those add; resource ceilings stand
alone. Prior art: NVIDIA OpenShell (supervisor, egress proxy, prover, sandbox limits).

- **Host world has no kernel boundary**: `world = host` is policy only (`docs/worlds.md`), the
  `aho-egress` crate doc says `bash` can `curl` anything, and no crate applies Landlock,
  seccomp, or Seatbelt. `world-ns` is Linux-only, so on macOS the only local enforced world is
  a full VM. Closed by a confined world (Linux: Landlock with the workspace read-write, the
  toolchain read-only, and TCP connect only to a local egress proxy, plus a seccomp filter;
  macOS: a Seatbelt profile) when tests on both OSes pin a kernel-refused write outside the
  workspace and a kernel-refused direct connect with the preset at `full`, so the kernel is the
  only thing saying no. A kernel without the needed Landlock ABI refuses the world with its own
  error; it never falls back to `host`.
- **Agent processes inherit the operator's secrets**: `HostSubprocess::start`
  (`crates/providers/world-host/src/proc.rs`) hands every child the harness's whole environment
  (only `AHO_READY_FD` is removed), so `env` in agent `bash` shows any exported token. OAuth
  tokens sit in `~/.aho/auth.json` (`apps/aho/src/auth.rs`), 0600 under the uid agent `bash`
  runs as, and `~/.aho` is not in `CREDENTIAL_DIRS` (`crates/aho-policy/src/paths.rs`). Closed
  when children start from a cleared environment plus an operator allowlist, a test pins that
  an exported `*_TOKEN` never reaches agent `bash` in any world, and a test pins the store
  unreadable from every enforced world. Under `host` the store stays readable by construction;
  `docs/worlds.md` says so.
- **No endpoint-bound credentials for agent processes**: `secret` headers (`aho_egress::Header`)
  guard aho's own tool requests (`web_fetch`, MCP); a process the agent runs (`git push`, `gh`,
  `curl`) can use a credential only by holding it. Closed when `ns`/`vm` processes get a
  placeholder (e.g. `AHO_SECRET_GITHUB`) that aho-net replaces with the real value only on
  flows to that credential's allowed hosts, with tests pinning that the real value never
  appears inside the guest or in the session log, and that the placeholder reaches any other
  host unchanged. Needs TLS termination in aho-net, with a per-session CA the guest trusts.
- **Egress rules cannot name methods, paths, or processes**: `Policy::check`
  (`crates/providers/net-userspace/src/policy.rs`) decides on guest, destination address, and
  DNS-validated domain; it cannot say "GET `api.github.com` yes, POST no" or tell `git` from
  `python3`. Closed when `egress.allow` entries can carry methods and path globs, enforced on
  the TLS stream terminated for the item above, and, in the `ns` world, the executable that owns
  the socket, resolved from host-side procfs and never self-reported by the guest. The `vm`
  world cannot attribute flows to processes; `docs/worlds.md` says so.
- **Approval cards don't show what a widening opens**: `aho_policy::ApprovalCard` carries the
  call and its `suggested_cut`, not what that cut grants beyond this call. Closed when every
  widening (approval card, `aho allow`, `aho net allow`) shows the reach it adds over the
  current epoch, computed from the two policies rather than prose: new consequence classes
  now, new credentialed destinations and methods once the two items above land. A test per
  widening kind pins the diff.
- **No resource ceilings**: nothing in `crates/` touches cgroups or rlimits, so a fork bomb or a
  memory balloon from agent `bash` in the `host` or `ns` world lands on the host. Closed when
  `ns` and the confined world run agent processes under a cgroup v2 child with configured
  `pids.max`, `memory.max`, and `cpu.max`, a host without a delegated subtree refuses the
  ceiling with its own error, and a test pins a fork bomb and a balloon stopped at the ceiling
  while the session keeps serving.

## Validation debt (tests pass, reality unproven)

- ~~No real QEMU guest has booted~~ **closed 2026-09-28**: Ubuntu 24.04.5 arm64 booted under
  QEMU 11.1.1/HVF on macOS via media → recipe → lease → sandbox VM; `aho shell -- uname -a`,
  a file round-trip, `aho export`, and revoke-then-refuse all exercised through the guest
  agent. Requires QEMU ≥ 9.2 (`reconnect-ms`); found and fixed `-netdev …,reconnect=1`,
  which QEMU 10 removed.
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
- **World build errors are flattened**: a failing `world = vm` boot surfaces to `aho shell`
  only as `no Subprocess provider is bound`; the actual reason (observed: `agent workspace is
  /work, not /workspace`) is swallowed per-attempt in `VmWorld::open` (`let _ = e`) and lost
  again when the Broken capability is reported by handle. Closed when the operator sees the
  world's own error text without patching in an `eprintln`.
- **CLI panics on closed stdout**: `aho sandbox status | head -2` → `failed printing to
  stdout: Broken pipe` panic. Closed by tolerating EPIPE process-wide.
- ~~Cross-process append race can brick a session log~~ **fixed 2026-09-28** (it struck three
  times that day: stale `turn_start` 290, then 388/389, then 433×2, each repaired by hand):
  `JsonlLog` now takes an exclusive OS lock on the file at its first mutation and holds it for
  the log's lifetime; a second writer is refused with the owner named, a stale writer whose
  file advanced is refused with the byte delta, and acquisition waits up to 2s for a dying
  holder (daemon detach→open handoff). Regression tests in `aho-log`, an SDK close-releases
  probe in `aho-sdk/tests/facade.rs`, and a live CLI smoke (concurrent `shell` + `lease grant`
  → clean refusal, `session verify` green). Fixing it surfaced and fixed two retention leaks:
  the rpc approval pump held `Arc<Session>` strongly (now `Weak`), and the daemon mutation
  test's host retained closed sessions (now `Weak`). Still open: a `session repair` verb for
  logs damaged before the lock existed — cut the stale suffix, prove it with `session verify`.
