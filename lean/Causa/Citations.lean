/-
Causa — Citations

Citation is `blob_id` 0..1 on the Memory row (UML §4). No mapping table.
Subject is Fact ∪ Abstraction. Perspectives never cite.
-/

import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Causa.Memory

namespace Causa

structure Blob where
  id     : BlobId
  owner  : Owner
  schema : SchemaRef

def blob_id : Blob → BlobId := Blob.id
def blob_owner : Blob → Owner := Blob.owner

instance : Immutable Blob := ⟨⟩
instance : AppendOnly Blob := ⟨⟩

/-- The Memory cites this blob. Multiplicity 0..1 is the Option itself. -/
def memory_cites (m : Memory) (b : Blob) : Prop :=
  memory_blob_id m = some (blob_id b) ∧ memory_owner m = blob_owner b

theorem citation_owner_match :
    ∀ (m : Memory) (b : Blob),
      memory_cites m b → memory_owner m = blob_owner b := by
  intro _ _ h
  exact h.2

end Causa
