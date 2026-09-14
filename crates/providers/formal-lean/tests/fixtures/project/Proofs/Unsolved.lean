def gauss : Nat → Nat
  | 0 => 0
  | k + 1 => gauss k + (k + 1)

theorem gauss_formula (n : Nat) : 2 * gauss n = n * (n + 1) := by
  induction n with
  | zero => rfl
  | succ k ih =>
    simp only [gauss]

theorem le_of_lt' (x y : Nat) (h : x < y) : x ≤ y := h
