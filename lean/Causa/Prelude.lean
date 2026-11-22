/-! Causa — Prelude -/

namespace Causa

def Set (α : Type) : Type := α → Prop

instance {α : Type} : Membership α (Set α) := ⟨fun s a => s a⟩

/-- The logical insertion-time coordinate. -/
abbrev Instant : Type := Nat

/-- Text carried by Memory and Goal admissions. -/
abbrev Text : Type := String

end Causa
