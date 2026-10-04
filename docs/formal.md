# Formal specs

`formal/` holds TLA+ specs; `formal-tlc` runs real TLC (`tla2tools.jar`) as a capability, and
`aho-formal` turns session logs into traces the specs check.

| spec | models |
|---|---|
| `Log.tla` | the append-only log: hash chain, fork-as-head-move |
| `Policy.tla` | the meet-only lattice: narrow always, widen only by operator approval |
| `Turn.tla` (+ `TurnJobs.cfg`) | the loop: phases, grace, injection points, session-scoped jobs, `TraceStepEnd` per transition |
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

## Jobs in `Turn.tla`

A running op is a turn op (`ops`) or a job (`jobs`). `Detach` moves it out of the turn
(`OpDetached`, then its call's `Result`), so `IdleIffNothingPending` counts turn work only: a
turn completes and a turn abort stops while its jobs run. A finished or cancelled job waits
in the delivery queue until `Deliver` (`JobDelivered`), which happens only in `prep` (the top
of a step, or the next turn's start, never between a call and its result), then `JobDone`
(its `OpDone`). `Cancel` (abort-all, kill, lease expiry) hits running jobs only; `Close` is
the `OpDone` of an undelivered job whose branch ended (rewind, a fork's child); `Restart`
re-dispatches an undelivered job under a fresh op id, which the log spells as that `OpDone`,
an `OpStarted` on the closed call (`RespawnOp`) and its `OpDetached` (`Redetach`).

| invariant | holds |
|---|---|
| `JobEndsOnce` | per detachment: one delivery then its `OpDone`, or an undelivered close; nothing after |
| `NoDeliveryAfterCancel` | a job cancelled while running delivers its cancellation, never its own result; a closed job delivers nothing |
| `NoDeliveryBetweenCallAndResult` | a delivery never follows a call without its result since the boundary, nor precedes the result that detached it |
| `JobWithinLease` | running jobs ≤ `MaxJobs` (the jobs lease); model only |

Trace records: `OpDetached` → `Detach{op, call, cause}`, `JobDelivered` →
`Deliver{op, call, tool, status}`. A job finishing or being cancelled is not logged, so
`TraceDeliver` folds it into the delivery (status `cancelled` ⇒ cancelled first); an `OpDone`
closing an undelivered job must not say `completed` (a lost result). The session enforces the
jobs lease (`LeaseGranted` subject `jobs:`) at detach only, and a grant with a lower max leaves
running jobs alone, so a trace detaches unbounded and `TraceInv` leaves `JobWithinLease` out.

`Turn.cfg` checks the loop with jobs off (`MaxJobs = 0`; trace validation reads its
constants); `TurnJobs.cfg` checks jobs on a smaller loop (two calls, two ops, a lease of one).
`formal-tlc/tests/turn_tla.rs` model-checks both, removes each job guard and expects the
invariant it keeps to fail, and validates the logs of scripted sessions run by the real
`Session` (delivery at a boundary and at the next turn's start, abort scopes, restart, rewind).

## Invariants

Capabilities register `Invariant { name, check }` fold-checkers; every test root and
`aho session verify` run them over the whole log. `aho invariants` lists their state.
Lean/Aeneas support rides the same seam (`formal-lean`).
