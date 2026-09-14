---- MODULE Calls ----
\* Tool calls and their results: a call gets at most one result. Has a TraceStep so a
\* recorded log can be validated against it, and guards whose loss the invariant notices.
EXTENDS Naturals, Sequences, FiniteSets, TLC
CONSTANT CallIds
VARIABLES open, done, results
vars == <<open, done, results>>

Init ==
    /\ open = {}
    /\ done = {}
    /\ results = <<>>

Call(c) ==
    /\ c \notin open
    /\ c \notin done
    /\ open' = open \cup {c}
    /\ UNCHANGED <<done, results>>

Result(c) ==
    /\ c \in open
    /\ Len(results) < Cardinality(CallIds)
    /\ open' = open \ {c}
    /\ done' = done \cup {c}
    /\ results' = Append(results, c)

Next ==
    \/ \E c \in CallIds : Call(c)
    \/ \E c \in CallIds : Result(c)

TypeOK == open \subseteq CallIds /\ done \subseteq CallIds
AtMostOneResult == \A i, j \in 1..Len(results) : i # j => results[i] # results[j]

TraceStep(e) ==
    \/ /\ e.action = "Call"
       /\ e.call \notin open \cup done
       /\ open' = open \cup {e.call}
       /\ UNCHANGED <<done, results>>
    \/ /\ e.action = "Result"
       /\ e.call \in open
       /\ open' = open \ {e.call}
       /\ done' = done \cup {e.call}
       /\ results' = Append(results, e.call)
    \/ /\ e.action \notin {"Call", "Result"}
       /\ UNCHANGED vars

TraceInv == AtMostOneResult
====
