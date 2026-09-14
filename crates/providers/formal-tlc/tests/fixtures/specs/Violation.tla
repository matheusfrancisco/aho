---- MODULE Violation ----
\* A log that outgrows its invariant after two calls: a multi-variable counterexample.
EXTENDS Naturals, Sequences, TLC
VARIABLES log, open, seen
vars == <<log, open, seen>>
Init == log = <<>> /\ open = {} /\ seen = [c \in {} |-> 0]
Call(c) ==
    /\ c \notin open
    /\ log' = Append(log, [kind |-> "call", call |-> c])
    /\ open' = open \cup {c}
    /\ seen' = seen @@ (c :> 1)
Next == \E c \in {"c1", "c2"} : Call(c)
Small == Len(log) < 2
====
