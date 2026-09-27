# The session log

Append-only, hash-chained JSONL (`MemoryLog` for tests). Everything visible is an entry; every
view — status, tree, inventory, the request itself — is a fold. A fork is a head move, not a
copy.

## Entries

`EntryKind` is a closed core (turn/step boundaries, user and assistant messages, tool calls and
results, request headers, policy epochs, approvals, checkpoints, subagent lifecycle, …) plus
namespaced `Custom` kinds that capabilities register with their own fold
(`Registrar::entry_kind`). Every entry records its principal — who spoke, including
`System("loop")`, `System("hooks")`, `System("delegation")`.

## Requests are folds

`build_request` derives the message list from `fold(log)`; nothing is cached between steps. The
logged `RequestHeader` carries the model, visible tools, policy epoch, plan hash, system-section
hashes, open calls, and `request_hash`. `verify_replay` re-derives every header's hash from the
log prefix before it — a session file proves what the model was shown.

System sections (`identity`, `contract`, `async-tools`, plus capability sections) are logged as
a `SystemMessage` of `(name, hash)` pairs only when they change; `cache_break_at` marks the end
of the cache-stable prefix.

Dynamic context (the policy line: epoch, allowed classes, world, workspace, whether approval
prompts exist) is appended as a `Snapshot` user message only when its text changes.

## Transitions

Steps and turns end with a closed `Transition`: `completed`, `tool_use`, `waiting_on_ops`,
`reactive_compact_retry`, `empty_stop_retry`, `max_output_recovery`, `stop_hook_blocking`,
`queued_command`, `budget_continue`, `prompt_too_long`, `max_turns`, `reload`,
`aborted(reason)`. The same tags appear in the SDK protocol table and in `formal/Turn.tla`;
adding one fails compilation of the exhaustive sample in `aho-sdk/tests/schema.rs` until all
three agree.

## Compaction

Past `compaction.threshold` of the context window (or reactively, on a context-window error)
the driver summarizes the older range with a one-paragraph prompt
(`crates/aho-session/prompts/compaction.md`) and replaces it; `compaction_range` picks the cut.

## Checkpoints and rewind

Workspace checkpoints are tied to log entries (per call or per turn,
`checkpoint.granularity`). `rewind` takes a checkpoint and a `RewindMode`: `both`, `chat_only`,
or `code_only`. Not available under the vm world.

## Inspection

`aho-inspect` is folds only: `tree`, `inventory`, `explain`, `why`, `status`, `orphans`,
`catch-up`, `invariants`. `aho session verify` runs replay equality plus every registered
invariant over the file.
