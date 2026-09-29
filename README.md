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

## Prompts

aho tells the model as little as possible and enforces the rest: checkable rules live in the
decision chain, guards, and the session driver; facts the harness knows are generated from live
state; only unjudgeable prose ships as text, as files under `crates/**/prompts/` pinned by the
`prose` ratchet (growth fails the gate). Subagents take `role: scout | reviewer` — reporting
shape from bundled prose, authority from a defaulted `read-only` preset. See
[docs/prompts.md](docs/prompts.md).

## Design notes

- The egress allow/deny list lives at `egress.allow` / `egress.deny`, shared by web tools, MCP and the
  sandbox network's seed. Tools have no per-tool list.
- A blocked flow's unlock names what blocked it: `aho net allow <dest>` for the egress list, and
  `aho net allow host-lan|other-sessions|internet` for an isolation rule.

## Gaps and direction

How it works: [docs/](docs/README.md). Unfinished work and known gaps: [TODO.md](TODO.md).
Direction: [ROADMAP.md](ROADMAP.md).
