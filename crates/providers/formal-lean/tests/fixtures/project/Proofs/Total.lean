-- total of the lines of an order
def total (xs : List Nat) : Nat :=
  xs.foldl (fun a b => a + b) 0

def ok (n : Nat) : Bool := n < 10 && true

theorem total_two : total [1, 2] = 3 := by decide

theorem ok_small : ok 3 = true := by decide
