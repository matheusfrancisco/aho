# Formal specs

`formal/` holds TLA+ specs; `formal-tlc` runs real TLC (`tla2tools.jar`) as a capability, and
`aho-formal` turns session logs into traces the specs check.

| spec | models |
|---|---|
| `Log.tla` | the append-only log: hash chain, fork-as-head-move |
| `Policy.tla` | the meet-only lattice: narrow always, widen only by operator approval |
| `Turn.tla` | the loop: phases, grace, injection points, `TraceStepEnd` per transition |
| `Tasks.tla` (+ `TasksVerify.cfg`) | the task graph and its verify-gate rule |

## Trace validation

`aho-formal/src/trace.rs` maps log entries to spec actions (`TurnStart`, `StepEnd` with the
transition's serde tag, `Input` with its form, `Request`, `Response`, `Call`, `Result`, …). A
recorded session is replayed against `Turn.tla`: every step transition must land in the spec's
transition function —

```tla
Continue == {"tool_use", "waiting_on_ops", "queued_command",
             "max_output_recovery", "stop_hook_blocking", "budget_continue"}

TraceStepEnd(t) ==
    IF t \in Continue THEN Decide
    ELSE IF t \in {"reactive_compact_retry", "empty_stop_retry"} THEN Retry
    ELSE IF t = "completed" THEN Completed
    ELSE Stop
```

Adding a `Transition` variant therefore touches three places or fails: the Rust enum
(`aho-log`), this spec, and the SDK protocol table (whose exhaustive sample in
`aho-sdk/tests/schema.rs` is a compile error until updated).

## Invariants

Capabilities register `Invariant { name, check }` fold-checkers; every test root and
`aho session verify` run them over the whole log. `aho invariants` lists their state.
Lean/Aeneas support rides the same seam (`formal-lean`).
