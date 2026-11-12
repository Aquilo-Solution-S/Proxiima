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
def blob_schema : Blob → SchemaRef := Blob.schema

def BlobIdUnique (objects : Set Blob) : Prop :=
  ∀ b1 b2 : Blob,
    b1 ∈ objects →
    b2 ∈ objects →
    blob_id b1 = blob_id b2 →
    b1 = b2

instance : Immutable Blob := ⟨⟩
instance : AppendOnly Blob := ⟨⟩

def Citable : Type := { m : Memory // memory_kind m = .Fact ∨ memory_kind m = .Abstraction }

def Citable.memory (c : Citable) : Memory := c.val

def Fact.citable (f : Fact) : Citable := ⟨f.val, Or.inl f.property⟩

/-- The Memory cites this blob. Multiplicity 0..1 is the Option itself. -/
def memory_cites (m : Memory) (b : Blob) : Prop :=
  memory_blob_id m = some (blob_id b) ∧ memory_owner m = blob_owner b

theorem citation_owner_match :
    ∀ (m : Memory) (b : Blob),
      memory_cites m b → memory_owner m = blob_owner b := by
  intro _ _ h
  exact h.2

end Causa
