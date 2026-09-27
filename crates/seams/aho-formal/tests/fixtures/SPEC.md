# Orders: System Spec

## 1 Purpose

Take orders and charge each at most once. Non-goal: refunds are handled by the bank.

## 2 Vocabulary

- **Order**: a request to buy; `Orders` in TLA+.
- **Charge**: money moved for one order.

## 3 Structure

`api` → `ledger` (crate `orders-ledger`).

## 4 State

- `charged`: set of orders charged (durable).

## 5 Actions

### Charge

Precondition: the order is not charged.

### Refund

Operator only.

## 6 Invariants

### inv.order.no-double-charge

- statement: an order is charged at most once
- kind: safety
- covers: src/ledger.rs, Charge
- mode: bounded(4)
- owner: ana

Why: the bank does not deduplicate.

```tla id=inv.order.no-double-charge
---- MODULE Orders ----
EXTENDS Naturals, FiniteSets
CONSTANT Orders
VARIABLE charged, count
vars == <<charged, count>>
Init == charged = {} /\ count = 0
Charge(o) ==
    /\ o \notin charged
    /\ charged' = charged \cup {o}
    /\ count' = count + 1
Next == \E o \in Orders : Charge(o)
NoDoubleCharge == count = Cardinality(charged)
====
```

```cfg id=inv.order.no-double-charge
CONSTANT Orders = {o1, o2}
INIT Init
NEXT Next
INVARIANT NoDoubleCharge
```

### inv.order.total

- statement: the order total is the sum of its lines
- kind: functional
- mode: proof

```lean id=inv.order.total module=Spec.Order
def total (xs : List Nat) : Nat := xs.foldl (· + ·) 0
theorem total_nil : total [] = 0 := rfl
```

### inv.order.audited

- statement: every charge is visible in the audit log
- kind: safety
- actions: Refund

## 7 Assumptions

- The bank API is idempotent per request id. If false: double charges on retry.

## 8 Refinements

`Charge` is implemented by `ledger::charge` (trace point `charge`).

## 9 Coverage

hand-written text the renderer must replace

## 10 History

old history
