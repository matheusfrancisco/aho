-------------------------------- MODULE Turn --------------------------------
(***************************************************************************)
(* The async-first turn loop: aho_session::core::next                      *)
(* and the driver around it (Session::prompt, run_turn, record_result).    *)
(* Every call starts when issued; after a response with tool calls a grace *)
(* window is armed; queued inputs are injected only at the request         *)
(* boundary; the loop is idle exactly when nothing of the turn is pending. *)
(*                                                                         *)
(* Phases: "idle" nothing to do; "prep" the loop decided to call the model *)
(* (inputs injected, StepStart, snapshots); "inflight" model call in       *)
(* flight; "wait" after the response (dispatch, results, inputs, grace,    *)
(* heartbeat); "halted" the turn stopped (max_turns, aborted, ...) with    *)
(* work still pending. Grace, input arrival and grace expiry are never     *)
(* logged; TraceStep folds them into the next logged step.                 *)
(*                                                                         *)
(* Jobs (session-scoped ops): a running op is detached (OpDetached, then   *)
(* its call's Result). It leaves the turn: the turn completes while it     *)
(* runs and a turn abort does not reach it. It ends once: its outcome is   *)
(* delivered (JobDelivered) at a request boundary of a turn, then its      *)
(* OpDone; or its branch ends under it (rewind, fork, restart) and an      *)
(* OpDone closes it undelivered. A job finishing, or being cancelled by    *)
(* abort-all, kill or lease expiry, is not logged; TraceStep folds it into *)
(* the delivery.                                                           *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Calls,          \* model only: call ids
    Ops,            \* model only: op ids
    MaxQueued,      \* model only: CONSTRAINT bound on queued inputs
    MaxSince,       \* model only: CONSTRAINT bound on events since a boundary
    MaxJobs         \* the jobs lease: detached ops that may run at once

VARIABLES
    turnOpen,       \* a TurnStart without its TurnEnd
    phase,          \* see above
    grace,          \* a grace window is armed
    open,           \* calls issued by a response and without a Result
    called,         \* calls that have a Call entry
    ops,            \* the turn's running operations as <<op, call>>
    jobs,           \* detached operations without their OpDone: [op, call, st]
    queued,         \* inputs received and not yet injected
    landed,         \* a Result landed that no request has carried yet
    since,          \* history: <<"call"|"result"|"input"|"deliver", call>> since the last boundary
    fate            \* history: op -> its job events since its OpStarted (absent: none)

vars == <<turnOpen, phase, grace, open, called, ops, jobs, queued, landed, since, fate>>

Phases == {"idle", "prep", "inflight", "wait", "halted"}
Active == {"prep", "inflight", "wait"}

(* "respawned": re-dispatched by a restart, its OpDetached not yet logged; *)
(* "done"/"cancelled": finished or cancelled, its outcome waits in the     *)
(* delivery queue; "delivered": JobDelivered logged, its OpDone owed.      *)
JobStates == {"running", "respawned", "done", "cancelled", "delivered"}

(* "deliver" carries the job's own outcome, "deliver_cancelled" its        *)
(* cancellation; "done" is the OpDone after a delivery, "close" an OpDone  *)
(* without one.                                                            *)
JobEvents == {"detach", "cancel", "deliver", "deliver_cancelled", "done", "close"}
Delivery == {"deliver", "deliver_cancelled"}
Ends == {"done", "close"}

Range(s) == {s[i] : i \in DOMAIN s}

OpIds == {p[1] : p \in ops}
JobIds == {j.op : j \in jobs}
Free(o) == o \notin OpIds \cup JobIds

(* What the lease counts: detached ops still executing.                   *)
Running == {j \in jobs : j.st \in {"running", "respawned"}}

(* Op ids in a trace are the log's, so `fate` grows a key per job.       *)
Fate(o) == IF o \in DOMAIN fate THEN fate[o] ELSE <<>>
Forget(o) == fate' = [x \in DOMAIN fate \ {o} |-> fate[x]]
Note(o, ev) == fate' = (o :> Append(Fate(o), ev)) @@ fate
SetJob(j, s) == jobs' = (jobs \ {j}) \cup {[j EXCEPT !.st = s]}

(* record_result appends a Result and its OpDone together; the loop never *)
(* builds a request in between.                                          *)
Settling == \E p \in ops : p[2] \notin open

(* A model call is in flight, being prepared, or owed for landed results. *)
ModelCall == phase \in {"prep", "inflight"} \/ landed

(* Turn-scoped only: running jobs and undelivered outcomes are not        *)
(* pending, so a turn completes while its detached ops run.               *)
NothingPending == open = {} /\ ops = {} /\ queued = 0 /\ ~ModelCall

(* A halted turn whose last call or op finishes has nothing left.         *)
AfterWork(o, s) == IF phase = "halted" /\ o = {} /\ s = {} /\ queued = 0 THEN "idle" ELSE phase

-----------------------------------------------------------------------------

TurnStart ==
    /\ ~turnOpen
    /\ turnOpen' = TRUE
    /\ phase' = "prep"
    /\ queued' = queued + 1
    /\ since' = <<>>
    /\ UNCHANGED <<grace, open, called, ops, jobs, landed, fate>>

Arrive ==
    /\ phase \in Active
    /\ queued' = queued + 1
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, jobs, landed, since, fate>>

(* n = 1: an input that arrived unlogged is injected in the same step.    *)
Inject(n) ==
    /\ phase = "prep"
    /\ queued + n > 0
    /\ queued' = queued + n - 1
    /\ since' = Append(since, <<"input", "">>)
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, jobs, landed, fate>>

StepStart ==
    /\ phase = "prep"
    /\ UNCHANGED vars

(* d = TRUE: grace expiry is not logged, so a logged Request also ends it. *)
Request(d) ==
    /\ phase = "prep"
    /\ (~grace \/ d)
    /\ ~Settling
    /\ phase' = "inflight"
    /\ grace' = (grace /\ ~d)
    /\ landed' = FALSE
    /\ since' = <<>>
    /\ UNCHANGED <<turnOpen, open, called, ops, jobs, queued, fate>>

(* core::next on Wake::Response: no tool use and nothing pending is idle. *)
Response(U) ==
    /\ phase = "inflight"
    /\ U \cap (open \cup called) = {}
    /\ open' = open \cup U
    /\ grace' = (U # {})
    /\ phase' = IF U = {} /\ open = {} /\ ops = {} /\ queued = 0 /\ ~landed THEN "idle" ELSE "wait"
    /\ UNCHANGED <<turnOpen, called, ops, jobs, queued, landed, since, fate>>

Call(c) ==
    /\ phase = "wait"
    /\ c \in open
    /\ c \notin called
    /\ called' = called \cup {c}
    /\ since' = Append(since, <<"call", c>>)
    /\ UNCHANGED <<turnOpen, phase, grace, open, ops, jobs, queued, landed, fate>>

OpStart(o, c) ==
    /\ c \in open
    /\ c \in called
    /\ Free(o)
    /\ ops' = ops \cup {<<o, c>>}
    /\ Forget(o)
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, jobs, queued, landed, since>>

OpDone(o) ==
    /\ o \in OpIds
    /\ ops' = {p \in ops : p[1] # o}
    /\ phase' = AfterWork(open, ops')
    /\ UNCHANGED <<turnOpen, grace, open, called, jobs, queued, landed, since, fate>>

Result(c) ==
    /\ c \in open
    /\ c \in called
    /\ open' = open \ {c}
    /\ landed' = (landed \/ phase \in {"inflight", "wait"})
    /\ since' = Append(since, <<"result", c>>)
    /\ phase' = AfterWork(open', ops)
    /\ UNCHANGED <<turnOpen, grace, called, ops, jobs, queued, fate>>

(* Expiry, all graced calls done, an input, or the operator's Alt+B.      *)
GraceEnd ==
    /\ grace
    /\ grace' = FALSE
    /\ UNCHANGED <<turnOpen, phase, open, called, ops, jobs, queued, landed, since, fate>>

(* The loop decides to request: after dispatch, for a landed result, an   *)
(* input, or the heartbeat while calls run. A finished job is no reason.  *)
Decide ==
    /\ phase = "wait"
    /\ open \subseteq called
    /\ (landed \/ queued > 0 \/ open # {})
    /\ phase' = "prep"
    /\ since' = <<>>
    /\ UNCHANGED <<turnOpen, grace, open, called, ops, jobs, queued, landed, fate>>

(* The model call failed and is retried (rate limit, reactive compaction). *)
Retry ==
    /\ phase = "inflight"
    /\ phase' = "prep"
    /\ UNCHANGED <<turnOpen, grace, open, called, ops, jobs, queued, landed, since, fate>>

(* The turn stops without going idle: max_turns, aborted, reload, ...     *)
(* A turn abort stops the turn's ops; jobs are not the turn's.            *)
Stop ==
    /\ phase # "halted"
    /\ phase' = IF open = {} /\ ops = {} /\ queued = 0 THEN "idle" ELSE "halted"
    /\ grace' = FALSE
    /\ landed' = FALSE
    /\ UNCHANGED <<turnOpen, open, called, ops, jobs, queued, since, fate>>

Completed ==
    /\ phase = "idle"
    /\ turnOpen
    /\ UNCHANGED vars

TurnEnd ==
    /\ turnOpen
    /\ phase \in {"idle", "halted"}
    /\ turnOpen' = FALSE
    /\ UNCHANGED <<phase, grace, open, called, ops, jobs, queued, landed, since, fate>>

-----------------------------------------------------------------------------
(* Jobs                                                                    *)

(* OpDetached: the op leaves the turn; its call stays open until the      *)
(* Result that follows, so the turn waits on nothing it no longer runs.   *)
DetachOp(o, c) ==
    /\ <<o, c>> \in ops
    /\ c \in open
    /\ ops' = ops \ {<<o, c>>}
    /\ jobs' = jobs \cup {[op |-> o, call |-> c, st |-> "running"]}
    /\ Note(o, "detach")
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, queued, landed, since>>

Detach(o, c) == Cardinality(Running) < MaxJobs /\ DetachOp(o, c)

(* The log takes a restart (Restart below) apart: the OpDone that closes *)
(* the old op, an OpStarted on its closed call, its OpDetached.           *)
RespawnOp(o, c) ==
    /\ c \in called
    /\ c \notin open
    /\ ~\E p \in ops : p[2] = c
    /\ ~\E j \in jobs : j.call = c
    /\ Free(o)
    /\ jobs' = jobs \cup {[op |-> o, call |-> c, st |-> "respawned"]}
    /\ Forget(o)
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

Redetach(o) ==
    /\ \E j \in jobs : j.op = o /\ j.st = "respawned" /\ SetJob(j, "running")
    /\ Note(o, "detach")
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

(* The job's own outcome (completed, failed, uncertain) is queued.        *)
Finish(o) ==
    /\ \E j \in jobs : j.op = o /\ j.st = "running" /\ SetJob(j, "done")
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since, fate>>

(* abort(All), abort(Op)/job_kill, lease expiry: only a running job; a    *)
(* finished one keeps its outcome (the first status wins).                *)
Cancel(o) ==
    /\ \E j \in jobs : j.op = o /\ j.st = "running" /\ SetJob(j, "cancelled")
    /\ Note(o, "cancel")
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

(* JobDelivered, at a request boundary of a turn (its top, or the start   *)
(* of the next turn) and after the Result that detached it. `pre`: job    *)
(* events the log does not record, folded into this step.                 *)
DeliverAs(j, cancelled, pre) ==
    /\ phase = "prep"
    /\ j.call \notin open
    /\ SetJob(j, "delivered")
    /\ fate' = (j.op :> ((Fate(j.op) \o pre) \o <<IF cancelled THEN "deliver_cancelled" ELSE "deliver">>)) @@ fate
    /\ since' = Append(since, <<"deliver", j.call>>)
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed>>

Deliver(o) == \E j \in jobs : j.op = o /\ j.st \in {"done", "cancelled"} /\ DeliverAs(j, j.st = "cancelled", <<>>)

(* The OpDone after a delivery.                                           *)
JobDone(o) ==
    /\ \E j \in jobs : j.op = o /\ j.st = "delivered" /\ jobs' = jobs \ {j}
    /\ Note(o, "done")
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

(* Its branch ends under an undelivered job: rewind (on_rewind = cancel) *)
(* or a fork's child. An OpDone closes it.                                *)
Close(o) ==
    /\ \E j \in jobs : j.op = o /\ j.st \in {"running", "done", "cancelled"} /\ jobs' = jobs \ {j}
    /\ Note(o, "close")
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

(* A restart re-dispatches an undelivered job on its call under a fresh  *)
(* op id: the old op is closed, the new one runs detached.                *)
Restart(o, n) ==
    /\ \E j \in jobs :
        /\ j.op = o
        /\ j.st \in {"running", "done", "cancelled"}
        /\ Cardinality(Running \ {j}) < MaxJobs
        /\ Free(n)
        /\ jobs' = (jobs \ {j}) \cup {[op |-> n, call |-> j.call, st |-> "running"]}
    /\ fate' = (n :> <<"detach">>) @@ ((o :> Append(Fate(o), "close")) @@ fate)
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

-----------------------------------------------------------------------------

Init ==
    /\ turnOpen = FALSE
    /\ phase = "idle"
    /\ grace = FALSE
    /\ open = {}
    /\ called = {}
    /\ ops = {}
    /\ jobs = {}
    /\ queued = 0
    /\ landed = FALSE
    /\ since = <<>>
    /\ fate = <<>>

Next ==
    \/ TurnStart
    \/ Arrive
    \/ Inject(0)
    \/ StepStart
    \/ Request(FALSE)
    \/ \E U \in SUBSET Calls : Response(U)
    \/ \E c \in Calls : Call(c)
    \/ \E o \in Ops, c \in Calls : OpStart(o, c)
    \/ \E o \in Ops : OpDone(o)
    \/ \E c \in Calls : Result(c)
    \/ GraceEnd
    \/ Decide
    \/ Retry
    \/ Stop
    \/ Completed
    \/ TurnEnd
    \/ \E o \in Ops, c \in Calls : Detach(o, c)
    \/ \E o \in Ops : Finish(o) \/ Cancel(o) \/ Deliver(o) \/ JobDone(o) \/ Close(o)
    \/ \E o, n \in Ops : Restart(o, n)

Spec == Init /\ [][Next]_vars

Bound == queued <= MaxQueued /\ Len(since) <= MaxSince

-----------------------------------------------------------------------------

TypeOK ==
    /\ turnOpen \in BOOLEAN
    /\ phase \in Phases
    /\ grace \in BOOLEAN
    /\ open \subseteq Calls
    /\ called \subseteq Calls
    /\ ops \subseteq Ops \X Calls
    /\ jobs \subseteq [op : Ops, call : Calls, st : JobStates]
    /\ \A j, k \in jobs : j.op = k.op => j = k
    /\ OpIds \cap JobIds = {}
    /\ queued \in Nat
    /\ landed \in BOOLEAN
    /\ since \in Seq({"call", "result", "input", "deliver"} \X (Calls \cup {""}))
    /\ DOMAIN fate \subseteq Ops
    /\ \A o \in DOMAIN fate : fate[o] \in Seq(JobEvents)

(* Between a Call and a later injected Input: that call's Result, or a    *)
(* request boundary (`since` restarts at each one).                       *)
NoInjectionBetweenCallAndResult ==
    \A i, j \in DOMAIN since :
        (i < j /\ since[i][1] = "call" /\ since[j][1] = "input")
            => \E k \in (i + 1)..(j - 1) : since[k] = <<"result", since[i][2]>>

IdleIffNothingPending == (phase = "idle") <=> NothingPending

NoRequestDuringGrace == phase = "inflight" => ~grace

Count(f, S) == Cardinality({i \in DOMAIN f : f[i] \in S})

(* Every detachment ends exactly once: one delivery then its OpDone, or   *)
(* an OpDone that closes it undelivered; nothing follows its end.         *)
JobEndsOnce ==
    \A o \in DOMAIN fate :
        LET f == fate[o] IN
        /\ Len(f) > 0 => f[1] = "detach"
        /\ Count(f, {"detach"}) <= 1
        /\ Count(f, Delivery) <= 1
        /\ Count(f, Ends) <= 1
        /\ \A i \in DOMAIN f : f[i] \in Ends => i = Len(f)
        /\ \A i \in DOMAIN f : f[i] = "done" => i > 1 /\ f[i - 1] \in Delivery
        /\ \A i \in DOMAIN f : f[i] = "close" => \A k \in 1..i : f[k] \notin Delivery

(* A job cancelled while running delivers its cancellation, never its own *)
(* outcome; a job closed by a cancel delivers nothing.                    *)
NoDeliveryAfterCancel ==
    \A o \in DOMAIN fate :
        LET f == fate[o] IN
        \A i, k \in DOMAIN f :
            i < k =>
                /\ f[i] = "cancel" => f[k] # "deliver"
                /\ f[i] = "close" => f[k] \notin Delivery

(* Between a Call and a later delivery: that call's Result, or a request  *)
(* boundary; and a job is delivered only after the Result that detached it. *)
NoDeliveryBetweenCallAndResult ==
    /\ \A i, j \in DOMAIN since :
        (i < j /\ since[i][1] = "call" /\ since[j][1] = "deliver")
            => \E k \in (i + 1)..(j - 1) : since[k] = <<"result", since[i][2]>>
    /\ \A j \in jobs : j.st = "delivered" => j.call \notin open

JobWithinLease == Cardinality(Running) <= MaxJobs

(* Grace is not logged and a logged Request ends it, so                   *)
(* NoRequestDuringGrace holds of every trace by construction. The model  *)
(* holds the lease constant; a logged grant with a lower max leaves the  *)
(* jobs already running, so a trace is not checked for JobWithinLease.   *)
TraceInv ==
    /\ NoInjectionBetweenCallAndResult
    /\ IdleIffNothingPending
    /\ JobEndsOnce
    /\ NoDeliveryAfterCancel
    /\ NoDeliveryBetweenCallAndResult

-----------------------------------------------------------------------------

Continue == {"tool_use", "waiting_on_ops", "queued_command",
             "max_output_recovery", "stop_hook_blocking", "budget_continue"}

TraceStepEnd(t) ==
    IF t \in Continue THEN Decide
    ELSE IF t \in {"reactive_compact_retry", "empty_stop_retry"} THEN Retry
    ELSE IF t = "completed" THEN Completed
    ELSE Stop

LoopKinds == {"TurnStart", "TurnEnd", "StepStart", "StepEnd", "Input", "Request",
              "Response", "Call", "Result", "OpStart", "OpDone", "Detach", "Deliver"}

(* A job's finish or cancellation is not logged: its delivery carries it. *)
TraceDeliver(o, s) ==
    \E j \in jobs :
        /\ j.op = o
        /\ j.st = "running"
        /\ DeliverAs(j, s = "cancelled", IF s = "cancelled" THEN <<"cancel">> ELSE <<>>)

(* The OpDone of a job: after its delivery, or closing it undelivered,    *)
(* which a completed op never is.                                         *)
TraceJobDone(o, s) ==
    IF \E j \in jobs : j.op = o /\ j.st = "delivered" THEN JobDone(o) ELSE s # "completed" /\ Close(o)

(* Inputs other than prompts (snapshot, notice, catalog, instructions) are *)
(* the loop's own context, not injections; calls and results the loop did *)
(* not issue (operator commands) leave it unchanged. An op started for a  *)
(* closed call is a job a restart re-dispatched. The trace does not read  *)
(* the lease (session enforces it at detach), so it detaches unbounded.  *)
TraceStep(e) ==
    \/ e.action = "TurnStart" /\ TurnStart
    \/ e.action = "TurnEnd" /\ TurnEnd
    \/ e.action = "StepStart" /\ (IF phase = "inflight" THEN Retry ELSE StepStart)
    \/ e.action = "StepEnd" /\ TraceStepEnd(e.transition)
    \/ e.action = "Input" /\ e.form = "prompt" /\ Inject(IF queued > 0 THEN 0 ELSE 1)
    \/ e.action = "Input" /\ e.form # "prompt" /\ UNCHANGED vars
    \/ e.action = "Request" /\ Request(TRUE)
    \/ e.action = "Response" /\ Response(Range(e.uses))
    \/ e.action = "Call" /\ (IF e.call \in open THEN Call(e.call) ELSE (e.by = "operator" /\ UNCHANGED vars))
    \/ e.action = "Result" /\ (IF e.call \in open THEN Result(e.call) ELSE UNCHANGED vars)
    \/ e.action = "OpStart" /\ (IF e.call \in open THEN OpStart(e.op, e.call) ELSE RespawnOp(e.op, e.call))
    \/ e.action = "OpDone" /\ (IF e.op \in OpIds THEN OpDone(e.op) ELSE TraceJobDone(e.op, e.status))
    \/ e.action = "Detach" /\ (IF e.op \in OpIds THEN DetachOp(e.op, e.call) ELSE Redetach(e.op))
    \/ e.action = "Deliver" /\ TraceDeliver(e.op, e.status)
    \/ e.action \notin LoopKinds /\ UNCHANGED vars

=============================================================================
