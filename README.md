<p align="center">
  <img src="imgs/logo.png" alt="aho" width="640">
</p>

# aho: a modular Rust agent harness

You extend aho by implementing `aho_cap::Capability` in a new crate and adding it to a `Composition`.
You never fork aho itself.

```
cargo build -p aho                  # the CLI, linked against the aho-lab bundle
cargo test --workspace
scripts/gate.sh                     # fmt · clippy -D warnings · ratchets · layer guard
cargo run -p aho-sdk -- gen --check # generated TS/Python SDKs match the rpc protocol
```

## Crate map

```
aho/
├── crates/
│   ├── aho-types        leaf DTOs: ids (u128 as decimal strings on the wire), Message, ContentBlock, ToolSpec,
│   │                    Consequence(Set), Principal, Card, Usage
│   ├── aho-log          Entry/EntryKind (closed core + namespaced Custom), hash chain, fold/step, derive_messages,
│   │                    MemoryLog + JsonlLog (append-only; a fork is a head move)
│   ├── aho-cap          Capability, Deps, handles + ceiling(), Manifest, ToolDecl, Registrar, Discoverer,
│   │                    Composition (builds providers in dependency order), plan() → Plan/PlanRow/Because
│   ├── aho-config       layers/scopes/origins, Sourced<T>, Secret, schema, resolve → Loadout, lint
│   ├── aho-policy       Operator/Engineer/Agent/System authority, presets (per-call ceilings), rules, decide() chain,
│   │                    shell blast-radius classifier, paths, leases, approval cards
│   ├── aho-compose      plan(Composition, inputs) → (Loadout, Plan), plan diff, saved loadouts, lock file
│   ├── aho-session      pure core (Input/Effect/step), async driver, tool dispatch with grace window + late results,
│   │                    abort, compaction, hooks, checkpoints & rewind, resume, PTC nested calls, session ceiling
│   │                    (plan mode), prompt_as (driver attribution), verify_replay, canary
│   ├── aho-inspect      folds over the log: tree, inventory, explain, why, status, orphans, catch-up, invariants
│   │
│   ├── seams/           traits + handle newtypes, no I/O
│   │   aho-llm · aho-fs · aho-proc · aho-sandbox · aho-egress · aho-term · aho-approval · aho-skills · aho-mcp
│   │   aho-acp (ACP v1 schema 1.23.0) · aho-control · aho-goals · aho-subagent · aho-tasks · aho-embed
│   │   aho-memory · aho-formal · aho-net · aho-vm · aho-display
│   │
│   ├── providers/       one crate per backend; each `impl Capability` unless noted
│   │   worlds:    world-host (worktree per session; enforced by: policy only) · world-ssh (remote dir over one
│   │              ControlMaster; enforced by: remote host) · pty-host (PTY terminals under host) · world-ns (Linux
│   │              namespace sandbox + aho-ns-helper) · world-vm (vm-qemu lab + world-vm; aho-vm-supervisor) ·
│   │              vm-guest / vm-guest-agent (host side / guest side of the virtio agent) · vm-rfb (RFB desktop)
│   │   network:   net-userspace (aho-net: userspace switch, DHCP/DHCPv6/RA, DNS, NTP, NAT44/66, port maps, policy)
│   │   models:    llm-openai-completions · llm-anthropic-messages · llm-openai-responses (Codex/Meta/xAI) ·
│   │              llm-pi-messages (Radius gateway) · llm-scripted (deterministic; tests and smoke runs)
│   │   auth:      provider-auth (account login: OAuth browser/device flows, private ~/.aho/auth.json store,
│   │              request-time refresh; each model adapter reads it through `oauth:<store path>` key references)
│   │   knowledge: skills-fsdir · mcp-stdio · mcp-http (Streamable HTTP, 2025-06-18) · embed-local (hash-ngram-384,
│   │              offline) · memory-local · hooks-claude-code (opt-in)
│   │   agents:    subagent-inproc · subagent-acp · subagent-claude · subagent-codex (foreign CLIs observed in tmux via
│   │              their transcripts; shared engine: foreign-cli)
│   │   formal:    formal-tlc (real TLC via tla2tools.jar) · formal-lean (lake/aeneas)
│   │   runtime:   daemon · selfdev · session-drive (schedule/workflow driver host; library) · gc (orphan scan/reclaim; library)
│   │
│   ├── tools/           ToolDecl tables: class, concurrency, interrupt, max_inline, check, present
│   │   tool-fs · tool-bash (bash | pwsh) · tool-jobs · tool-terminal (opt-in) · tool-run-code (PTC) · tool-lsp
│   │   tool-web (web_fetch, web_search) · tool-skills · tool-mcp-resources · tool-memory · tool-session (session query)
│   │   tool-tasks · tool-goals · tool-todo · tool-schedule · tool-workflow (ralph) · tool-plan-mode · tool-subagent
│   │   tool-formal · tool-inspect (aho_why, aho_inspect, unlock_request) · tool-vm (13 vm_*) · tool-display
│   │
│   ├── surfaces/        surface-print · surface-rpc (NDJSON) · surface-tui · surface-web (HTTPS with operator PEM; plain
│   │                    HTTP only on loopback with --insecure-loopback) · surface-webhook (GitHub, HMAC) · surface-acp
│   │
│   └── bundles/         products as Rust values
│       ├── aho-base     spine(): model adapters, worlds (host, namespace, ssh), aho-net, PTY, fs/bash/jobs/terminal/run_code/lsp/web tools,
│       │                skills, MCP (stdio, http), subagents (inproc, acp), tasks/goals/todo/schedule/workflow/plan mode,
│       │                inspect tools, Claude Code hooks (opt-in) + engineer defaults
│       ├── aho-headless spine + print + rpc + webhook + acp
│       ├── aho-web      headless + web + tui
│       ├── aho-lab      web + formal + memory + session query + claude/codex subagents + selfdev + VM lab (vm-qemu,
│       │                world-vm, tool-vm, tool-display)
│       └── aho-sdk      aho-sdk-min: spine + rpc as an embeddable Rust API, plus the TS/Python SDK generator
│
├── apps/aho             the CLI. `aho --help` lists the verbs: plan tree inventory explain why catalog caps
│                        enable disable allow set preset apply loadout lock run attach serve tui web cert webhook acp
│                        session (list show verify diff merge destroy branches) fork checkpoints checkpoint rewind shell
│                        status rpc spec invariants doctor selfdev schedule ralph plan-mode gc credential login logout add
│                        net sandbox export lease media
├── formal/              Log · Policy · Turn · Tasks (.tla/.cfg; TasksVerify.cfg), checked by formal-tlc
├── imgs/                logo
├── sdk/                 schema/aho-rpc.schema.json · ts/ · python/ (aho-sdk generates them and checks for drift)
└── scripts/             gate.sh; ratchets/*.json baselines (panic, swallowed, long, wildcard, layers);
                        models.py regenerates crates/seams/aho-llm/models.tsv (model names, list prices)
```

## Provider accounts (`aho login`)

`aho login [provider]` signs you into a model provider account without running a vendor CLI, and
`aho logout [provider]` removes the stored login. Providers: `anthropic` (Claude Pro/Max), `openai-codex`
(ChatGPT Plus/Pro, browser or `--device-code`), `github-copilot` (`--enterprise-domain <host>`),
`kimi-coding`, `meta`, `openrouter`, `radius` (`--gateway <url>`, browser or `--device-code`), `xai`.
`aho login --list` prints this table; `--no-browser` prints the URL without launching a browser.

- Tokens live in `~/.aho/auth.json`. aho creates it 0600 in a 0700-owned directory, writes it atomically and
  locks it across processes. Config files hold no tokens: login writes `provider.<id>.key = "oauth:<store path>"`
  into `~/.aho/config.toml`, and adapters resolve and refresh the credential on each model request, so long-lived
  daemons pick up rotated tokens. An explicit API key in a higher layer still wins (`aho credential set <id>`).
- Logout clears the login reference and the stored account. It leaves the grant at the provider and any
  configured API keys in place. Clearing a credential relocks the routes it unlocked.
- Subscription caveat: providers may bill third-party-harness usage as metered or extra usage instead of plan
  quota (pi's Anthropic warning). aho prints a notice at `aho login anthropic` and cannot verify vendor billing.
- Local protocol servers exercise login, refresh/re-mint, streamed tool turns through each route and replayed
  session logs end to end. CI does not cover live provider acceptance or billing.

## Worlds

| world | crate | workspace | enforced by |
|---|---|---|---|
| host | world-host | host dir / git worktree | policy only |
| namespace (Linux) | world-ns (`world-namespace`) | private copy of the project under `<session.dir>/<id>.ns` | user+pid+mount+net namespaces; aho-net only NIC |
| vm | world-vm (`vm-qemu` + `world-vm`, aho-lab) | the sandbox VM's disk, via the guest agent | QEMU guest boundary; aho-net only NIC |
| ssh | world-ssh | remote dir (needs POSIX sh, perl, git ≥ 2.29; curl for egress) | remote host |

Sandbox worlds plug their only NIC into `net-userspace` (aho-net: DHCP/DHCPv6/RA, `<name>.aho` DNS, NTP,
NAT44/NAT66, port maps, isolation, egress policy). Each session gets one network under `<session.dir>/<id>.net`,
started when a sandbox attaches. `aho net status|allow|map` changes it live through its control socket.

- `namespace` needs `world.ns.rootfs` (an unpacked recipe with `/bin/sh`), iproute2, unprivileged user
  namespaces, overlayfs and `/dev/net/tun`, and the helper:
  `cargo install --locked --path crates/providers/world-ns --bin aho-ns-helper`.
- `vm` needs QEMU (`qemu-system-*`, `qemu-img`), `aho-vm-supervisor`
  (`cargo install --locked --path crates/providers/world-vm --bin aho-vm-supervisor`), a guest image that runs
  `vm-guest-agent --workspace <dir>` on virtio port `org.aho.agent`, and a lease: `aho media import`,
  `aho add recipe`, `aho set vm.recipe <name>`, `aho lease grant`. Machines listen on unix sockets under
  `vm.dir` (default `~/.aho/vm`); keep that path short.

## Not implemented

- `aho session import` and `aho craft`.

## Design notes

- The egress allow/deny list lives at `egress.allow` / `egress.deny`, shared by web tools, MCP and the
  sandbox network's seed. Tools have no per-tool list.
- A blocked flow's unlock names what blocked it: `aho net allow <dest>` for the egress list, and
  `aho net allow host-lan|other-sessions|internet` for an isolation rule.

## Known gaps

- Tests cover `subagent-claude` / `subagent-codex` with recorded transcript fixtures and fake CLIs in real
  tmux. We never launched the real `claude` and `codex` binaries with a prompt, since that spends the
  operator's credits.
- Under `world = vm`, `aho shell` without a command opens no PTY into the guest (use `aho shell -- <cmd>`),
  and you get no workspace checkpoints. The guest agent serves one host channel, so `aho export` from the VM
  fails while the session holds it.
- The namespace sandbox's recipe root is an overlay whose writes last while its keeper runs; `/workspace` and
  `/session` persist across keeper restarts.
- The VM tests and smokes run against a stub `qemu-system-*` (real QMP, the real supervisor and guest agent);
  we have not booted a real QEMU guest. The namespace world ran on Linux in a privileged container.
