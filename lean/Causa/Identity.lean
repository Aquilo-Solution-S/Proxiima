import Causa.Prelude
import Causa.Owner

/-! Causa — Identity -/

namespace Causa

/-- The kernel's opaque identity token. -/
opaque Id : Type := String

/-- Identifier of one Memory version. -/
abbrev MemoryId          : Type := Id
abbrev GoalId            : Type := Id
/-- Identifier of one series. -/
abbrev Handle            : Type := Id
abbrev BlobId            : Type := Id
/-- Identifier of a typed sidecar payload. -/
abbrev ContentId         : Type := Id
/-- The engine digest of a content payload. -/
opaque ContentHash : Type := String
/-- The version identities describing a retrieval situation. -/
abbrev Cue : Type := Set MemoryId

/-- Authenticated authorship of a write. -/
structure WriterAttribution where
  «by» : User
  via  : Option Id

/-- Stable identifier used for a persisted owner. -/
structure OwnerRef where
  id : Id

/-- The owner membership view used by authorization. -/
abbrev ResolvedOwner : Type := Owner

/-- Trusted owner resolution supplied by the host. -/
structure OwnerState where
  resolve : OwnerRef → ResolvedOwner
  /-- The identifier by which the owner registry knows a user. -/
  own : User → OwnerRef
  /-- The personal-owner resolution law. -/
  personal_resolves : ∀ u : User, resolve (own u) = Owner.ofUser u

/-- Identifier of the operator that produced a derived memory. -/
opaque OperatorId : Type := String

/-- Identifier of a Fact-to-Abstraction input contract. -/
opaque InputContract : Type := String

/-- The opaque tag of a typed schema. -/
opaque SchemaRef : Type := String

/-- Marker for the append-only lifecycle. -/
class AppendOnly (α : Type) : Prop

/-- Marker for immutable content. -/
class Immutable (α : Type) : Prop

end Causa
