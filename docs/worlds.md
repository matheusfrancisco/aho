# Execution worlds

A world is where the session's files, processes, and terminals live. The column that matters is
`enforced by`.

| world | crate | workspace | enforced by |
|---|---|---|---|
| host | world-host | host dir / git worktree | policy only |
| namespace (Linux) | world-ns | private copy under `<session.dir>/<id>.ns` | user+pid+mount+net namespaces; aho-net only NIC |
| vm | vm-qemu + world-vm (aho-lab) | the sandbox VM's disk, via the guest agent | QEMU guest boundary; aho-net only NIC |
| ssh | world-ssh | remote dir over one ControlMaster | remote host |

"Policy only" means the decision chain is the last line of defense — a floor, not a sandbox.
The sandbox worlds hold even against a maximally wrong agent.

## aho-net

Sandbox worlds plug their only NIC into `net-userspace`: a userspace switch with DHCP/DHCPv6/RA,
`<name>.aho` DNS, NTP, NAT44/66, port maps, session isolation, and the shared egress policy
(`egress.allow` / `egress.deny` — the same list web tools and MCP use; there is no per-tool
list). One network per session under `<session.dir>/<id>.net`, started when a sandbox attaches.
`aho net status|allow|map` changes it live; a blocked flow's message names the exact unlock.

## VM lab vs vm world

One provider crate, two capabilities, two relationships between agent and machine:

- **VM lab** (`vm-qemu` + `tool-vm`): the VM is an *object* the agent works **on**. The session
  stays in its own world; the agent creates machines from operator recipes and drives them
  through the 13 `vm_*` tools (create/start/stop/status/list, disks, console, screenshot,
  keyboard/mouse/pointer), capped by the operator lease. Isolates the host from the guests'
  contents — not from the agent.
- **vm world** (`world-vm`, `seams.world = vm` + `vm.recipe`): the VM is the *place* the agent
  works **in**. The sandbox VM is created from `vm.recipe` on first run and persists; every
  `read`/`write`/`bash`/terminal crosses the guest agent on virtio-serial, the guest's screen is
  the session desktop (RFB), and host exposure narrows to the agent channel, QMP, and the
  supervisor.

Shared substrate for both: immutable media (`aho media import`), typed recipes
(`aho add recipe` — no raw QEMU arguments; a recipe cannot add devices, host mounts, or a second
NIC), an operator lease (`aho lease grant`, machines-at-once including the sandbox VM), and
`aho-vm-supervisor` owning every QEMU process.

| | VM lab | vm world |
|---|---|---|
| machines | up to the lease, agent-created | one sandbox VM, harness-created |
| `fs`/`bash` target | host workspace | guest, via agent |
| checkpoints | host world's | none |
| `aho shell` | normal | `aho shell -- <cmd>` only |

## Requirements

- namespace: `world.ns.rootfs` (unpacked recipe with `/bin/sh`), iproute2, unprivileged user
  namespaces, overlayfs, `/dev/net/tun`, and `aho-ns-helper`
  (`cargo install --locked --path crates/providers/world-ns --bin aho-ns-helper`).
- vm: QEMU ≥ 9.2 (`qemu-system-*`, `qemu-img`; the netdev uses `reconnect-ms`),
  `aho-vm-supervisor`
  (`cargo install --locked --path crates/providers/world-vm --bin aho-vm-supervisor`), a guest
  image running `vm-guest-agent --workspace <dir>` on virtio port `org.aho.agent` — the
  `--workspace` must equal the recipe's `workspace` (default `/workspace`) or the world
  refuses the agent — and a lease. Keep `vm.dir` short: machines listen on unix sockets
  under it.

`scripts/vm-setup.sh --image <cloud-image> [--smoke]` does all of the vm setup from a stock
cloud-init image: prerequisite checks with exact fixes, agent cross-compile, seed build,
content-detected media import, recipe, config, and (with `--smoke`) a lease plus a proven
`aho shell -- uname -a` boot.

Validated end to end 2026-09-28: Ubuntu 24.04.5 arm64 cloud image with a
cloud-init-installed agent, QEMU 11.1.1/HVF on macOS; shell, file round-trip, `aho export`,
and revoke-then-refuse all through the guest agent.
