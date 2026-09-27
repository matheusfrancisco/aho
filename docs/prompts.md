# Prompts

aho tells the model as little as possible and enforces the rest. The rule for any candidate
instruction: if it states a checkable invariant, gate it (policy, guard, driver); if it states a
fact the harness knows, generate it from live state; only judgment nobody can check
mechanically may be prose.

## Inventory

All model-facing prose is a file under a crate's `prompts/` directory, included with
`include_str!` and pinned by the `prose` ratchet:

| file | bytes | job |
|---|---|---|
| `aho-session/prompts/identity.md` | 164 | who the agent is; two sentences |
| `aho-session/prompts/contract.md` | 1339 | the delivery contract: the judgment residue that cannot be gated |
| `aho-session/prompts/async-preamble.md` | 801 | the async tool-call contract (turns, parallelism, late results) |
| `aho-session/prompts/compaction.md` | 329 | the compaction summarizer's instructions |
| `tool-subagent/prompts/scout.md` | 922 | scout role: reporting shape for recon |
| `tool-subagent/prompts/reviewer.md` | 1106 | reviewer role: finding criteria and verdict shape |

Everything else the model reads is generated: the policy line (epoch, allowed classes, world,
workspace, whether approval prompts exist) is computed from live state and re-logged only on
change; capability sections come through `Registrar::prompt_section`; denials explain
themselves at the moment of violation.

## Enforced, not requested

Instructions other harnesses write as prose, and where aho puts them instead:

| elsewhere: prompt prose | here: mechanism |
|---|---|
| "you are read-only" | the child preset's consequence ceiling; the chain re-checks every call |
| "don't use shell grep/rg" | tool-fs guard denies at step 4, naming the mounted tool |
| "never run destructive commands" | blast-radius classifier: Confirm asks, Catastrophic never lifts |
| "keep going / don't stop early" | empty-stop retry with a typed transition and a budget |
| "stop repeating yourself" | loop guard on identical replies |
| "wrap up, you're near the limit" | generated notice on the last budgeted step |
| "you may spawn at most N agents" | the lease and the scheduler; the number is config |

## Roles

`subagent` takes `role: scout | reviewer`. The role text is reporting shape only — sections to
emit, criteria for a finding. Its authority comes from the defaulted `read-only` preset, never
from the text; an explicit `preset` still wins, and the child chain caps it at the parent's.

## The prose ratchet

`scripts/ratchets/prose.json` pins the byte count of every `crates/**/prompts/**/*.md` and the
total. Growth fails `scripts/gate.sh` until the baseline is deliberately rewritten. Accretion is
a reviewed decision, not a default.

Adding a prose line requires the same evidence as a bug fix: a scripted transcript
(`llm-scripted`) that fails without the line and passes with it. No repro, no line.
