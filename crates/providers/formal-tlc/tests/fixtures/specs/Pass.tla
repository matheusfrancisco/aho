---- MODULE Pass ----
\* A counter that stays inside its bound: TLC finds no error.
EXTENDS Naturals
CONSTANT Max
VARIABLE x
vars == <<x>>
Init == x = 0
Next ==
    /\ x < Max
    /\ x' = x + 1
Small == x <= Max
====
