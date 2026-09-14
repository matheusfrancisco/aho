---- MODULE Broken ----
\* Does not parse.
EXTENDS Naturals
VARIABLE x
Init == x = 0
Next == x' = x + + 1
====
