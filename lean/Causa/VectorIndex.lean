import Proxiima.Embeddings
import Proxiima.Search

/-! # Host-index leases and revision-fenced metadata -/

namespace Causa

/-- A publication or drain that misses one lease renewal still renews before its lease lapses. -/
theorem debt_lease_survives_a_missed_renewal :
    2 * Proxiima.Embeddings.debtLeaseRenewSeconds < Proxiima.Embeddings.debtLeaseSeconds := by
  decide

/-- One metadata field of a host-index entry: its value and the revision that wrote it. -/
structure Revised (α : Type) where
  value : α
  revision : Nat

/-- How an incoming revision compares with the stored one. -/
def compared (incoming stored : Nat) : Proxiima.Search.Compared :=
  if incoming < stored then .older else if incoming = stored then .same else .newer

/-- Apply an update aimed at the stored generation under `Proxiima.Search.metadataLands`. -/
def Revised.update (stored incoming : Revised α) : Revised α :=
  if Proxiima.Search.metadataLands .same (compared incoming.revision stored.revision) then
    incoming
  else
    stored

/-- An update lands exactly when its revision is newer than the stored one. -/
theorem lands_iff_newer (incoming stored : Nat) :
    Proxiima.Search.metadataLands .same (compared incoming stored) = true ↔ stored < incoming := by
  unfold compared
  by_cases older : incoming < stored
  · simp [older, Proxiima.Search.metadataLands]; omega
  · by_cases same : incoming = stored
    · simp [same, Proxiima.Search.metadataLands]
    · simp [older, same, Proxiima.Search.metadataLands]; omega

/-- An update replaces the field exactly when its revision is newer. -/
theorem Revised.update_eq (stored incoming : Revised α) :
    stored.update incoming = if stored.revision < incoming.revision then incoming else stored := by
  unfold Revised.update
  by_cases newer : stored.revision < incoming.revision
  · simp [newer, (lands_iff_newer incoming.revision stored.revision).mpr newer]
  · have stays : Proxiima.Search.metadataLands .same
        (compared incoming.revision stored.revision) = false := by
      cases lands : Proxiima.Search.metadataLands .same (compared incoming.revision stored.revision)
      · rfl
      · exact absurd ((lands_iff_newer _ _).mp lands) newer
    simp [newer, stays]

/-- Updates with distinct revisions leave the same field whichever the index receives first. -/
theorem Revised.update_comm (stored a b : Revised α) (distinct : a.revision ≠ b.revision) :
    (stored.update a).update b = (stored.update b).update a := by
  simp only [Revised.update_eq]
  by_cases ha : stored.revision < a.revision <;> by_cases hb : stored.revision < b.revision <;>
    by_cases hab : a.revision < b.revision <;> by_cases hba : b.revision < a.revision <;>
    simp [ha, hb, hab, hba] <;> omega

end Causa
