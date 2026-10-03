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
| `aho-session/prompts/contract.md` | 1889 | the delivery contract: the judgment residue that cannot be gated, test craft included |
| `aho-session/prompts/async-preamble.md` | 801 | the async tool-call contract (turns, parallelism, late results) |
| `aho-session/prompts/compaction.md` | 329 | the compaction summarizer's instructions |
| `tool-subagent/prompts/scout.md` | 922 | scout role: reporting shape for recon |
| `tool-subagent/prompts/reviewer.md` | 1106 | reviewer role: finding criteria and verdict shape |
| `context-files/prompts/context.md` | 102 | one-line preamble over the repository instruction files |
| `verify-gate/prompts/failed.md` | 35 | template for a failed verify run: command, outcome, output tail |
| `tool-lsp/prompts/write-diagnostics.md` | 198 | wording around diagnostics appended to a write/edit result, and the late notice |

Everything else the model reads is generated: the policy line (epoch, allowed classes, world,
workspace, whether approval prompts exist) is computed from live state and re-logged only on
change; capability sections come through `Registrar::prompt_section`; denials explain
themselves at the moment of violation.

Repository instruction files (AGENTS.md, CLAUDE.md, …) are context, not prose: `context-files`
re-reads them from the workspace each step and sends them as a `<repo-instructions>` Snapshot,
so only its one-line preamble (crates/tools/context-files/prompts/context.md) counts toward the
prose ratchet; see docs/architecture.md#repository-instruction-files.

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

Adding a prose line needs two proofs, because a fake model and a real one prove different things:

| proof | how | shows |
|---|---|---|
| wiring | a scripted transcript (`llm-scripted`, `aho run --script`) | the line reaches the request: right section, right order, right turn |
| behavior | a real-model eval, with and without the line, recorded | the line changes what the model does |

`llm-scripted` is a deterministic fake: it replays its script whatever the prompt says, so it
cannot show that judgment prose changes behavior. The behavioral run is gated and recorded with
the change — exact command, model and thinking settings, per-config results — and the operator
invokes it: it spends tokens and varies between samples, so it never runs in `scripts/gate.sh` or
unattended CI. No wiring transcript and no recorded behavioral result, no line.

`scripts/evals/test-quality/` is the behavioral eval for the contract's test-craft lines. Each
harness config gets a fresh work dir per task (a spec README and a stub Python package) and runs
non-interactively from a template in `harnesses.json` (`aho run -p` with `--project`, `omp -p`;
model and thinking flags per harness). Scores: hidden reference suite against the model's code
(correctness), the model's own tests against hidden mutants of the reference (killed/total), and
the model's tests that also pass against a `NotImplementedError` stub (tautological). Output is
`results.json` plus `summary.md` with one table per config. `--dry-run` checks the tasks
themselves; the `fake-*` harnesses check the scoring without a model:

```
python3 scripts/evals/test-quality/run.py --out <dir> --dry-run
python3 scripts/evals/test-quality/run.py --out <dir> --harness fake-reference --harness fake-tautology
python3 scripts/evals/test-quality/run.py --out <dir> --harness aho-new --harness omp --repeat 3 \
    --var omp=$(command -v omp)
```
