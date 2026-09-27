-------------------------------- MODULE Turn --------------------------------
(***************************************************************************)
(* The async-first turn loop: aho_session::core::next                      *)
(* and the driver around it (Session::prompt, run_turn, record_result).    *)
(* Every call starts when issued; after a response with tool calls a grace *)
(* window is armed; queued inputs are injected only at the request         *)
(* boundary; the loop is idle exactly when nothing is pending.             *)
(*                                                                         *)
(* Phases: "idle" nothing to do; "prep" the loop decided to call the model *)
(* (inputs injected, StepStart, snapshots); "inflight" model call in       *)
(* flight; "wait" after the response (dispatch, results, inputs, grace,    *)
(* heartbeat); "halted" the turn stopped (max_turns, aborted, ...) with    *)
(* work still pending. Grace, input arrival and grace expiry are never     *)
(* logged; TraceStep folds them into the next logged step.                 *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Calls,          \* model only: call ids
    Ops,            \* model only: op ids
    MaxQueued,      \* model only: CONSTRAINT bound on queued inputs
    MaxSince        \* model only: CONSTRAINT bound on events since a boundary

VARIABLES
    turnOpen,       \* a TurnStart without its TurnEnd
    phase,          \* see above
    grace,          \* a grace window is armed
    open,           \* calls issued by a response and without a Result
    called,         \* calls that have a Call entry
    ops,            \* running operations as <<op, call>>
    queued,         \* inputs received and not yet injected
    landed,         \* a Result landed that no request has carried yet
    since           \* history: <<"call"|"result"|"input", call>> since the last boundary

vars == <<turnOpen, phase, grace, open, called, ops, queued, landed, since>>

Phases == {"idle", "prep", "inflight", "wait", "halted"}
Active == {"prep", "inflight", "wait"}

Range(s) == {s[i] : i \in DOMAIN s}

OpIds == {p[1] : p \in ops}

(* record_result appends a Result and its OpDone together; the loop never *)
(* builds a request in between.                                          *)
Settling == \E p \in ops : p[2] \notin open

(* A model call is in flight, being prepared, or owed for landed results. *)
ModelCall == phase \in {"prep", "inflight"} \/ landed

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
    /\ UNCHANGED <<grace, open, called, ops, landed>>

Arrive ==
    /\ phase \in Active
    /\ queued' = queued + 1
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, landed, since>>

(* n = 1: an input that arrived unlogged is injected in the same step.    *)
Inject(n) ==
    /\ phase = "prep"
    /\ queued + n > 0
    /\ queued' = queued + n - 1
    /\ since' = Append(since, <<"input", "">>)
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, ops, landed>>

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
    /\ UNCHANGED <<turnOpen, open, called, ops, queued>>

(* core::next on Wake::Response: no tool use and nothing pending is idle. *)
Response(U) ==
    /\ phase = "inflight"
    /\ U \cap (open \cup called) = {}
    /\ open' = open \cup U
    /\ grace' = (U # {})
    /\ phase' = IF U = {} /\ open = {} /\ ops = {} /\ queued = 0 /\ ~landed THEN "idle" ELSE "wait"
    /\ UNCHANGED <<turnOpen, called, ops, queued, landed, since>>

Call(c) ==
    /\ phase = "wait"
    /\ c \in open
    /\ c \notin called
    /\ called' = called \cup {c}
    /\ since' = Append(since, <<"call", c>>)
    /\ UNCHANGED <<turnOpen, phase, grace, open, ops, queued, landed>>

OpStart(o, c) ==
    /\ c \in open
    /\ c \in called
    /\ o \notin OpIds
    /\ ops' = ops \cup {<<o, c>>}
    /\ UNCHANGED <<turnOpen, phase, grace, open, called, queued, landed, since>>

OpDone(o) ==
    /\ o \in OpIds
    /\ ops' = {p \in ops : p[1] # o}
    /\ phase' = AfterWork(open, ops')
    /\ UNCHANGED <<turnOpen, grace, open, called, queued, landed, since>>

Result(c) ==
    /\ c \in open
    /\ c \in called
    /\ open' = open \ {c}
    /\ landed' = (landed \/ phase \in {"inflight", "wait"})
    /\ since' = Append(since, <<"result", c>>)
    /\ phase' = AfterWork(open', ops)
    /\ UNCHANGED <<turnOpen, grace, called, ops, queued>>

(* Expiry, all graced calls done, an input, or the operator's Alt+B.      *)
GraceEnd ==
    /\ grace
    /\ grace' = FALSE
    /\ UNCHANGED <<turnOpen, phase, open, called, ops, queued, landed, since>>

(* The loop decides to request: after dispatch, for a landed result, an   *)
(* input, or the heartbeat while calls run.                               *)
Decide ==
    /\ phase = "wait"
    /\ open \subseteq called
    /\ (landed \/ queued > 0 \/ open # {})
    /\ phase' = "prep"
    /\ since' = <<>>
    /\ UNCHANGED <<turnOpen, grace, open, called, ops, queued, landed>>

(* The model call failed and is retried (rate limit, reactive compaction). *)
Retry ==
    /\ phase = "inflight"
    /\ phase' = "prep"
    /\ UNCHANGED <<turnOpen, grace, open, called, ops, queued, landed, since>>

(* The turn stops without going idle: max_turns, aborted, reload, ...     *)
Stop ==
    /\ phase # "halted"
    /\ phase' = IF open = {} /\ ops = {} /\ queued = 0 THEN "idle" ELSE "halted"
    /\ grace' = FALSE
    /\ landed' = FALSE
    /\ UNCHANGED <<turnOpen, open, called, ops, queued, since>>

Completed ==
    /\ phase = "idle"
    /\ turnOpen
    /\ UNCHANGED vars

TurnEnd ==
    /\ turnOpen
    /\ phase \in {"idle", "halted"}
    /\ turnOpen' = FALSE
    /\ UNCHANGED <<phase, grace, open, called, ops, queued, landed, since>>

-----------------------------------------------------------------------------

Init ==
    /\ turnOpen = FALSE
    /\ phase = "idle"
    /\ grace = FALSE
    /\ open = {}
    /\ called = {}
    /\ ops = {}
    /\ queued = 0
    /\ landed = FALSE
    /\ since = <<>>

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
    /\ queued \in Nat
    /\ landed \in BOOLEAN
    /\ since \in Seq({"call", "result", "input"} \X (Calls \cup {""}))

(* Between a Call and a later injected Input: that call's Result, or a    *)
(* request boundary (`since` restarts at each one).                       *)
NoInjectionBetweenCallAndResult ==
    \A i, j \in DOMAIN since :
        (i < j /\ since[i][1] = "call" /\ since[j][1] = "input")
            => \E k \in (i + 1)..(j - 1) : since[k] = <<"result", since[i][2]>>

IdleIffNothingPending == (phase = "idle") <=> NothingPending

NoRequestDuringGrace == phase = "inflight" => ~grace

(* Grace is not logged and a logged Request ends it, so                   *)
(* NoRequestDuringGrace holds of every trace by construction.             *)
TraceInv == NoInjectionBetweenCallAndResult /\ IdleIffNothingPending

-----------------------------------------------------------------------------

Continue == {"tool_use", "waiting_on_ops", "queued_command",
             "max_output_recovery", "stop_hook_blocking", "budget_continue"}

TraceStepEnd(t) ==
    IF t \in Continue THEN Decide
    ELSE IF t \in {"reactive_compact_retry", "empty_stop_retry"} THEN Retry
    ELSE IF t = "completed" THEN Completed
    ELSE Stop

LoopKinds == {"TurnStart", "TurnEnd", "StepStart", "StepEnd", "Input", "Request",
              "Response", "Call", "Result", "OpStart", "OpDone"}

(* Inputs other than prompts (snapshot, notice, catalog, instructions) are *)
(* the loop's own context, not injections; calls and results the loop did *)
(* not issue (operator commands) leave it unchanged.                       *)
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
    \/ e.action = "OpStart" /\ OpStart(e.op, e.call)
    \/ e.action = "OpDone" /\ OpDone(e.op)
    \/ e.action \notin LoopKinds /\ UNCHANGED vars

=============================================================================
