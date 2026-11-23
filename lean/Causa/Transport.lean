import Proxiima.Transport

/-! # Transport timing -/

namespace Causa

/-- A positive idle timeout strictly above the progress floor admits a heartbeat before the deadline. -/
theorem heartbeat_before_idle (idle floor cadence : Nat) (aboveFloor : floor < idle) :
    max floor (min cadence (idle / 2)) < idle := by
  have halfBeforeIdle : idle / 2 < idle := by omega
  exact Nat.max_lt.mpr ⟨aboveFloor, Nat.lt_of_le_of_lt (Nat.min_le_right ..) halfBeforeIdle⟩

end Causa
