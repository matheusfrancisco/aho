------------------------------- MODULE Policy -------------------------------
(***************************************************************************)
(* The authority lattice across epochs. `narrow` is a meet                 *)
(* (SandboxPolicy::narrow); `allowed` grows only through                   *)
(* widen_by_approval, an Operator citing an AllowedOnce decision, which    *)
(* opens the next epoch. The fold enforces the log side                    *)
(* (policy.widen_requires_operator_approval, policy.epoch_monotone); the   *)
(* first epoch of a session (from epoch 0) is the bootstrap and may set    *)
(* any allowed set.                                                        *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Classes,        \* the Consequence names
    Reqs,           \* model only: approval request ids
    MaxEpoch        \* model only: CONSTRAINT bound on the epoch

VARIABLES
    epoch,          \* current epoch; 0 = before the bootstrap epoch
    allowed,        \* current allowed classes
    requested,      \* ids of ApprovalRequested entries
    granted,        \* the ones decided allowed_once
    last            \* history: the last epoch step (see Step)

vars == <<epoch, allowed, requested, granted, last>>

Principals == {"operator", "engineer", "agent", "system"}

Set(s) == {s[i] : i \in DOMAIN s}

(* An epoch step: who, which approval it cites, the requested set, the    *)
(* allowed set and epoch before (old, from) and after (new, to).          *)
Step(k, p, r, q, s, n) ==
    [kind |-> k, by |-> p, approval |-> r, req |-> q,
     old |-> allowed, new |-> s, from |-> epoch, to |-> n]

NoStep == [kind |-> "none", by |-> "system", approval |-> 0, req |-> {},
           old |-> {}, new |-> {}, from |-> 0, to |-> 0]

-----------------------------------------------------------------------------

Bootstrap(p, n, S) ==
    /\ epoch = 0
    /\ epoch' = n
    /\ allowed' = S
    /\ last' = Step("bootstrap", p, 0, S, S, n)
    /\ UNCHANGED <<requested, granted>>

(* The result S is characterised by the meet's laws: below the old set,  *)
(* below the requested one, above every common lower bound.              *)
Narrow(p, n, req, S) ==
    /\ epoch > 0
    /\ n >= epoch
    /\ S \subseteq allowed
    /\ S \subseteq req
    /\ allowed \cap req \subseteq S
    /\ epoch' = n
    /\ allowed' = S
    /\ last' = Step("narrow", p, 0, req, S, n)
    /\ UNCHANGED <<requested, granted>>

Widen(p, n, r, to) ==
    /\ epoch > 0
    /\ n >= epoch
    /\ p = "operator"
    /\ r \in granted
    /\ epoch' = n
    /\ allowed' = allowed \cup to
    /\ last' = Step("widen", p, r, to, allowed \cup to, n)
    /\ UNCHANGED <<requested, granted>>

ApprovalRequested(r) ==
    /\ r \notin requested
    /\ requested' = requested \cup {r}
    /\ UNCHANGED <<epoch, allowed, granted, last>>

ApprovalDecided(r, o) ==
    /\ r \in requested
    /\ granted' = IF o = "allowed_once" THEN granted \cup {r} ELSE granted
    /\ UNCHANGED <<epoch, allowed, requested, last>>

-----------------------------------------------------------------------------

Init ==
    /\ epoch = 0
    /\ allowed = {}
    /\ requested = {}
    /\ granted = {}
    /\ last = NoStep

(* Narrowing requests drop one class (a preset cut, a child's decl cut);  *)
(* widening adds the class of the approved call, as the session does.     *)
Next ==
    \/ \E c \in Classes : Bootstrap("system", 1, {c})
    \/ \E c \in Classes, S \in SUBSET allowed : Narrow("agent", epoch, Classes \ {c}, S)
    \/ \E p \in Principals, r \in Reqs \cup {0}, c \in Classes : Widen(p, epoch + 1, r, {c})
    \/ \E r \in Reqs : ApprovalRequested(r)
    \/ \E r \in Reqs, o \in {"allowed_once", "rejected"} : ApprovalDecided(r, o)

Spec == Init /\ [][Next]_vars

Bound == epoch < MaxEpoch

-----------------------------------------------------------------------------

TypeOK ==
    /\ epoch \in 0..MaxEpoch
    /\ allowed \subseteq Classes
    /\ requested \subseteq Reqs
    /\ granted \subseteq requested
    /\ last.kind \in {"none", "bootstrap", "narrow", "widen"}
    /\ last.by \in Principals
    /\ last.approval \in Reqs \cup {0}
    /\ last.req \subseteq Classes
    /\ last.old \subseteq Classes
    /\ last.new = allowed
    /\ last.from \in 0..MaxEpoch
    /\ last.to = epoch

NarrowIsMeet == last.kind = "narrow" => last.new = last.old \cap last.req

(* After the bootstrap, an epoch that is not a subset of the one before   *)
(* was opened by the operator citing an AllowedOnce decision.            *)
WidenNeedsOperatorApproval ==
    (last.from > 0 /\ ~(last.new \subseteq last.old))
        => (last.by = "operator" /\ last.approval \in granted)

EpochMonotone == last.from <= last.to

(* A log does not record the requested set of a narrowing, so NarrowIsMeet *)
(* holds of every trace by construction and is left out.                   *)
TraceInv == WidenNeedsOperatorApproval /\ EpochMonotone

-----------------------------------------------------------------------------

(* The log records the resulting set: epoch 0 -> bootstrap; a subset ->   *)
(* a narrowing that asked for exactly it; otherwise a widening, whose      *)
(* union must be what was logged.                                          *)
TraceEpoch(e) ==
    LET S == Set(e.allowed) IN
    /\ IF epoch = 0 THEN Bootstrap(e.by, e.epoch, S)
       ELSE IF S \subseteq allowed THEN Narrow(e.by, e.epoch, S, S)
       ELSE Widen(e.by, e.epoch, e.approval, S)
    /\ allowed' = S

TraceStep(e) ==
    \/ e.action = "Epoch" /\ TraceEpoch(e)
    \/ e.action = "ApprovalRequested" /\ ApprovalRequested(e.req)
    \/ e.action = "ApprovalDecided" /\ ApprovalDecided(e.req, e.outcome)
    \/ e.action \notin {"Epoch", "ApprovalRequested", "ApprovalDecided"} /\ UNCHANGED vars

=============================================================================
