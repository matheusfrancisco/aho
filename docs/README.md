# aho docs

How the harness works, as built. Design history and the wide ideation trail live outside this
repository; everything here is checkable against the code it names.

| doc | question it answers |
|---|---|
| [architecture](architecture.md) | Crate layers, the turn loop, why a request is a fold over the log |
| [config](config.md) | Config layers and precedence, `caps.<id>.enabled`, profiles, the built-in `coding` profile |
| [authority](authority.md) | Principals as types, the meet-only lattice, the 8-step decision chain, leases |
| [session-log](session-log.md) | Entry kinds, hash chain, replay equality, checkpoints and rewind |
| [surfaces](surfaces.md) | Abort scopes and session jobs per surface: rpc/daemon methods, TUI keys and jobs panel, web, `aho jobs` |
| [worlds](worlds.md) | host / namespace / vm / ssh, the VM lab vs the vm world, aho-net |
| [lsp](lsp.md) | Language servers in the world, `lsp_*` tools, diagnostics appended to file writes, `cargo check` on save |
| [prompts](prompts.md) | What the model is told vs what is enforced; the prose ratchet |
| [extending](extending.md) | Adding a capability: tool, guard, world, section, hook — no fork |
| [formal](formal.md) | The TLA+ specs and how sessions are trace-checked against them |

Three rules to hold in your head:

1. Everything is a `Capability`.
2. Everything visible is a log entry; inspection is a fold.
3. Authority flows one way; the agent can only request.
