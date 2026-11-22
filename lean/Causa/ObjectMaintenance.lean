import Proxiima.ObjectMaintenance

/-! # Object maintenance retry arithmetic -/

namespace Causa

/-- Retry delay in whole units with a capped exponent and a configured ceiling. -/
def purgeRetryDelay (attempts base ceiling : Nat) : Nat :=
  min (base * 2 ^ min attempts Proxiima.ObjectMaintenance.purgeBackoffExponentMax) ceiling

/-- Additional failures past the exponent cap do not increase the delay. -/
theorem purgeRetryDelay_saturates (attempts base ceiling : Nat)
    (pastCap : Proxiima.ObjectMaintenance.purgeBackoffExponentMax ≤ attempts) :
    purgeRetryDelay attempts base ceiling =
      purgeRetryDelay Proxiima.ObjectMaintenance.purgeBackoffExponentMax base ceiling := by
  simp [purgeRetryDelay, Nat.min_eq_right pastCap]

end Causa
