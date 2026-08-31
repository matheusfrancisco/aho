-------------------------------- MODULE Log --------------------------------
(***************************************************************************)
(* The session log (docs 06, 13) and the structural rules its fold         *)
(* enforces (aho_log::step). The store is a tree: every entry gets a fresh *)
(* id and a parent, and a branch is read in id order, so a trace is one    *)
(* branch. The fold state is kept incrementally, as in the Rust fold.      *)
(*                                                                         *)
(*   AppendOnly           ids only grow; every earlier entry is kept as is *)
(*   AtMostOneResult      tool.at_most_one_result                          *)
(*   ApprovalsInsideTurn  approval.inside_turn, decided_matches_requested  *)
(*   ReplaceNeverSplits   replace.never_splits_call_result                 *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Calls,          \* model only: call ids
    MaxLen          \* model only: CONSTRAINT bound on the log length

VARIABLES
    log,            \* entry id -> parent id (0 = none): the store's index
    prev,           \* history: `log` before the last step
    head,           \* id of the newest entry of the branch (0 = empty)
    turn,           \* the open turn [no, at] (`at` = id of its TurnStart), or NoTurn
    ended,          \* history: TurnStart ids of the turns that have ended
    reqTurn,        \* ApprovalRequested id -> `turn.at` when it was appended
    decTurn,        \* ApprovalRequested id -> `turn.at` when it was decided
    calls,          \* call ids that have a Call entry
    res,            \* call id -> ids of its Result entries
    uses,           \* call id -> ids of the Response entries whose ToolUse names it
    replaces        \* history: every Replace as [start, end, at]

vars == <<log, prev, head, turn, ended, reqTurn, decTurn, calls, res, uses, replaces>>

NoTurn == [no |-> 0, at |-> 0]

Range(s) == {s[i] : i \in DOMAIN s}
Get(f, k) == IF k \in DOMAIN f THEN f[k] ELSE {}
Add(f, k, v) == (k :> (Get(f, k) \cup {v})) @@ f
AddAll(f, K, v) == [k \in K |-> Get(f, k) \cup {v}] @@ f

Open == calls \ DOMAIN res
Pending == DOMAIN reqTurn \ DOMAIN decTurn

InRange(x, s, t) == s <= x /\ x <= t

(* The fold's split test: over the Results already folded, a Result in the *)
(* range whose Response is not, or the other way round.                   *)
Splits(s, t) ==
    \E c \in DOMAIN res : \E x \in res[c] :
        InRange(x, s, t) # (\E a \in Get(uses, c) : InRange(a, s, t))

(* The store appends a child of the head under an id past every other.    *)
Appendable(id, parent) == id > head /\ (head = 0 \/ parent = head)
Store(id, parent) == log' = (id :> parent) @@ log /\ prev' = log /\ head' = id

-----------------------------------------------------------------------------

TurnStart(id, parent, n) ==
    /\ Appendable(id, parent)
    /\ turn = NoTurn
    /\ Store(id, parent)
    /\ turn' = [no |-> n, at |-> id]
    /\ UNCHANGED <<ended, reqTurn, decTurn, calls, res, uses, replaces>>

TurnEnd(id, parent, n) ==
    /\ Appendable(id, parent)
    /\ turn.at # 0
    /\ turn.no = n
    /\ Pending = {}
    /\ Store(id, parent)
    /\ turn' = NoTurn
    /\ ended' = ended \cup {turn.at}
    /\ UNCHANGED <<reqTurn, decTurn, calls, res, uses, replaces>>

ApprovalRequested(id, parent) ==
    /\ Appendable(id, parent)
    /\ turn.at # 0
    /\ Store(id, parent)
    /\ reqTurn' = (id :> turn.at) @@ reqTurn
    /\ UNCHANGED <<turn, ended, decTurn, calls, res, uses, replaces>>

ApprovalDecided(id, parent, r) ==
    /\ Appendable(id, parent)
    /\ turn.at # 0
    /\ r \in Pending
    /\ Store(id, parent)
    /\ decTurn' = (r :> turn.at) @@ decTurn
    /\ UNCHANGED <<turn, ended, reqTurn, calls, res, uses, replaces>>

Call(id, parent, c) ==
    /\ Appendable(id, parent)
    /\ c \notin calls
    /\ Store(id, parent)
    /\ calls' = calls \cup {c}
    /\ UNCHANGED <<turn, ended, reqTurn, decTurn, res, uses, replaces>>

Response(id, parent, U) ==
    /\ Appendable(id, parent)
    /\ Store(id, parent)
    /\ uses' = AddAll(uses, U, id)
    /\ UNCHANGED <<turn, ended, reqTurn, decTurn, calls, res, replaces>>

Result(id, parent, c) ==
    /\ Appendable(id, parent)
    /\ c \in Open
    /\ Store(id, parent)
    /\ res' = Add(res, c, id)
    /\ UNCHANGED <<turn, ended, reqTurn, decTurn, calls, uses, replaces>>

Replace(id, parent, s, t) ==
    /\ Appendable(id, parent)
    /\ ~Splits(s, t)
    /\ Store(id, parent)
    /\ replaces' = replaces \cup {[start |-> s, end |-> t, at |-> id]}
    /\ UNCHANGED <<turn, ended, reqTurn, decTurn, calls, res, uses>>

Other(id, parent) ==
    /\ Appendable(id, parent)
    /\ Store(id, parent)
    /\ UNCHANGED <<turn, ended, reqTurn, decTurn, calls, res, uses, replaces>>

-----------------------------------------------------------------------------

Init ==
    /\ log = <<>>
    /\ prev = <<>>
    /\ head = 0
    /\ turn = NoTurn
    /\ ended = {}
    /\ reqTurn = <<>>
    /\ decTurn = <<>>
    /\ calls = {}
    /\ res = <<>>
    /\ uses = <<>>
    /\ replaces = {}

(* Any id may be proposed; `Appendable` admits only the fresh one.        *)
Candidates == 1..(head + 1)

Next ==
    \/ \E id \in Candidates : TurnStart(id, head, Cardinality(ended) + 1)
    \/ \E id \in Candidates : TurnEnd(id, head, turn.no)
    \/ \E id \in Candidates : ApprovalRequested(id, head)
    \/ \E id \in Candidates, r \in DOMAIN reqTurn : ApprovalDecided(id, head, r)
    \/ \E id \in Candidates, c \in Calls : Call(id, head, c)
    \/ \E id \in Candidates, U \in SUBSET Calls : Response(id, head, U)
    \/ \E id \in Candidates, c \in Calls : Result(id, head, c)
    \/ \E id \in Candidates, s \in 1..head : \E t \in s..head : Replace(id, head, s, t)
    \/ \E id \in Candidates : Other(id, head)

Spec == Init /\ [][Next]_vars

Bound == head < MaxLen

-----------------------------------------------------------------------------

TypeOK ==
    /\ head \in 0..MaxLen
    /\ DOMAIN log = 1..head
    /\ \A i \in DOMAIN log : log[i] \in 0..(i - 1)
    /\ turn \in [no : 0..MaxLen, at : 0..MaxLen]
    /\ ended \subseteq 1..MaxLen
    /\ DOMAIN reqTurn \subseteq 1..MaxLen
    /\ DOMAIN decTurn \subseteq DOMAIN reqTurn
    /\ calls \subseteq Calls
    /\ DOMAIN res \subseteq calls
    /\ \A c \in DOMAIN res : res[c] \subseteq 1..MaxLen
    /\ DOMAIN uses \subseteq Calls
    /\ \A c \in DOMAIN uses : uses[c] \subseteq 1..MaxLen
    /\ replaces \subseteq [start : 1..MaxLen, end : 1..MaxLen, at : 1..MaxLen]

(* The last step kept every entry and only added ids past the old ones.   *)
AppendOnly ==
    /\ DOMAIN prev \subseteq DOMAIN log
    /\ \A i \in DOMAIN prev : log[i] = prev[i]
    /\ \A i \in DOMAIN log \ DOMAIN prev : \A j \in DOMAIN prev : j < i

AtMostOneResult == \A c \in DOMAIN res : Cardinality(res[c]) <= 1

(* Requested and Decided inside one open turn, the same one; no turn ends *)
(* with a request left undecided.                                         *)
ApprovalsInsideTurn ==
    /\ \A r \in DOMAIN reqTurn : reqTurn[r] # 0
    /\ \A r \in DOMAIN decTurn : r \in DOMAIN reqTurn /\ decTurn[r] = reqTurn[r]
    /\ \A r \in DOMAIN reqTurn : reqTurn[r] \in ended => r \in DOMAIN decTurn

(* Over the Results that precede it, a Replace holds a call's Response    *)
(* and its Result both or neither.                                        *)
ReplaceNeverSplits ==
    \A p \in replaces : \A c \in DOMAIN res : \A x \in res[c] :
        x < p.at =>
            (InRange(x, p.start, p.end)
                = (\E a \in Get(uses, c) : a < p.at /\ InRange(a, p.start, p.end)))

TraceInv == AppendOnly /\ AtMostOneResult /\ ApprovalsInsideTurn /\ ReplaceNeverSplits

-----------------------------------------------------------------------------

LogKinds == {"TurnStart", "TurnEnd", "ApprovalRequested", "ApprovalDecided",
             "Call", "Response", "Result", "Replace"}

TraceStep(e) ==
    \/ e.action = "TurnStart" /\ TurnStart(e.id, e.parent, e.turn)
    \/ e.action = "TurnEnd" /\ TurnEnd(e.id, e.parent, e.turn)
    \/ e.action = "ApprovalRequested" /\ ApprovalRequested(e.id, e.parent)
    \/ e.action = "ApprovalDecided" /\ ApprovalDecided(e.id, e.parent, e.req)
    \/ e.action = "Call" /\ Call(e.id, e.parent, e.call)
    \/ e.action = "Response" /\ Response(e.id, e.parent, Range(e.uses))
    \/ e.action = "Result" /\ Result(e.id, e.parent, e.call)
    \/ e.action = "Replace" /\ Replace(e.id, e.parent, e.start, e.end)
    \/ e.action \notin LogKinds /\ Other(e.id, e.parent)

=============================================================================
