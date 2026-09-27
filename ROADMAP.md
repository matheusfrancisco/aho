# Roadmap

Standing rules, carried from the original phase plan:

- every item ends with something an operator can run and an invariant a test pins; no item
  ships a stub for the next;
- model-facing prose is ratcheted (`scripts/ratchets/prose.json`); a new prose line needs a
  scripted transcript (`llm-scripted`) that fails without it.

## Shipped

The phased build-out (contract → loop → policy/worlds → operator surface → web/lab → breadth →
memory/swarm → daemon/self-reload) is implemented and pinned by `cargo test --workspace` plus
`scripts/gate.sh`. Highlights: four worlds with aho-net; the 8-step decision chain with
epochs, leases, and the blast-radius classifier; replay-equal session logs trace-checked
against TLA+ specs; six surfaces over one `Session`; subagents (in-process, ACP, observed
Claude/Codex CLIs); generated TS/Python SDKs with drift checks; provider logins; driver-level
stop/loop/budget guards; gated prompt prose with bundled scout/reviewer roles.

## Next

Ordered by risk retired per unit of work; details in [TODO.md](TODO.md).

1. **Real-guest VM validation** — boot a real image through media → recipe → lease → guest
   agent. Retires the largest untested-reality risk in the lab.
2. **`swallowed` ratchet paydown** — the gate is red today; either fix the sites or accept
   them deliberately. A red gate that stays red trains people to ignore it.
3. **Gated live acceptance for foreign CLIs and providers** — one operator-invoked run for
   `claude`/`codex` adapters and one per login route; never in unattended CI.
4. **Unprivileged namespace world** — prove `world-ns` on a stock distro without a privileged
   container, or narrow the documented claim.
5. **`aho session import`** — foreign transcripts folded into valid logs; `session verify`
   green on the result.

## Later

Direction, not commitment; each enters *Next* only with a runnable result attached.

- **WASM component loading for untrusted tools** — viable for tools, not for worlds; gives
  third-party tools a boundary stronger than the decision chain.
- **`cargo vet` policy for third-party capabilities** — provenance for the extension
  ecosystem the `Capability` contract invites.
- **Richer role library** — further subagent roles only where an output shape earns its
  bytes against the prose ratchet; authority always from presets, never text.
- **Telemetry** — operator-visible, log-derived; nothing phones home.
- **Desktop shell and bilingual docs** — after the surfaces underneath stop moving.
