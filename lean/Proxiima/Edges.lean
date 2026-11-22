import Proxiima.Memory

/-! # Edges -/

namespace Proxiima.Edges

open Proxiima.Memory (Kind EntityKind)

/-- The kinds of pins, in projection order. -/
inductive PinKind where
  | origin
  | reference
  | read
  deriving DecidableEq

/-- The wire and cross-store storage label of a pin kind. -/
def pinLabel : PinKind → String
  | .origin => "origin"
  | .reference => "reference"
  | .read => "read"

/-- The relationship fields of a Memory admission. -/
inductive PinField where
  | origins
  | refs
  | goalRefs
  | guidedBy
  | reads
  | forkedFrom
  deriving DecidableEq

/-- The role, storage shape and target layers of a pin field. -/
structure PinFieldSpec where
  column : String
  kind : PinKind
  scalar : Bool
  fact : List EntityKind
  abstraction : List EntityKind
  perspective : List EntityKind

/-- A source version declares each pin through the field that fixes its role and target kinds. -/
def pinField : PinField → PinFieldSpec
  | .origins => ⟨"origins", .origin, false, [], [.Fact, .Abstraction], [.Abstraction]⟩
  | .refs => ⟨"refs", .reference, false, [.Fact, .Abstraction, .Perspective],
      [.Fact, .Abstraction, .Perspective], [.Fact, .Abstraction, .Perspective]⟩
  | .goalRefs => ⟨"goal_refs", .reference, false, [.Goal], [.Goal], [.Goal]⟩
  | .guidedBy => ⟨"guided_by", .reference, false, [], [], [.Goal]⟩
  | .reads => ⟨"reads", .read, false, [.Fact, .Abstraction, .Perspective],
      [.Fact, .Abstraction, .Perspective], [.Fact, .Abstraction, .Perspective]⟩
  | .forkedFrom => ⟨"forked_from", .reference, true, [.Fact, .Abstraction, .Perspective],
      [.Fact, .Abstraction, .Perspective], [.Fact, .Abstraction, .Perspective]⟩

/-- An Abstraction requires origins; a Perspective can be grounded through references. -/
def requiresOrigins : Kind → Bool
  | .Fact | .Perspective => false
  | .Abstraction => true

/-- Local storage fields that can ground a derived Memory, per `Causa.GroundsInFact`. -/
def groundingFields : List PinField := [.origins, .refs]

/-- Pin projection groups kinds in this order and preserves field order within each kind. -/
def pinOrder : List PinKind := [.origin, .reference, .read]

/-- The precedence of exact target states when a pin's source is readable. -/
inductive TargetProjection where
  | visible
  | forgotten
  | erased
  | redacted
  deriving DecidableEq

/-- Live wins over readable forgotten or erased witnesses, and other targets are redacted. -/
def projectTarget : (live forgotten erased : Bool) → TargetProjection
  | true, true, true | true, true, false | true, false, true | true, false, false => .visible
  | false, true, true | false, true, false => .forgotten
  | false, false, true => .erased
  | false, false, false => .redacted

/-- A historical witness stores only the exact version identity and its kind. -/
structure ErasedTarget where
  version : String
  kind : EntityKind

/-- The origin or subject declaration a schema contributes to ancestor traversal. -/
inductive Provenance where
  | undeclared
  | originEdges
  | payloadOnly
  | none
  deriving DecidableEq

/-- Local ancestor edge kinds by schema provenance; payload references are limited to declared subject fields. -/
def localAncestorKinds : Provenance → List PinKind
  | .undeclared | .originEdges => [.origin, .read]
  | .payloadOnly => [.reference, .read]
  | .none => [.read]

/-- Memory layering for a declared reference, with Goals on a separate spine. -/
def admitsReferenceLayer : EntityKind → EntityKind → Bool
  | .Fact, .Fact | .Fact, .Goal => true
  | .Fact, .Abstraction | .Fact, .Perspective => false
  | .Abstraction, .Fact | .Abstraction, .Abstraction | .Abstraction, .Goal => true
  | .Abstraction, .Perspective => false
  | .Perspective, .Fact | .Perspective, .Abstraction
  | .Perspective, .Perspective | .Perspective, .Goal => true
  | .Goal, .Fact | .Goal, .Abstraction | .Goal, .Perspective | .Goal, .Goal => true

/-- Local descendant traversal follows origins and read context. -/
def localDescendantKinds : List PinKind := [.origin, .read]

/-- Read pins yield to an origin, reference or fork anchor to the same target. -/
def keepReadPin : (origin reference forkAnchor : Bool) → Bool
  | false, false, false => true
  | true, true, true | true, true, false | true, false, true | true, false, false
  | false, true, true | false, true, false | false, false, true => false

/-- The continuation key for a pin page, compared newest first and exclusive of the cursor. -/
inductive PageKey where
  | sourceTime
  | sourceId
  | targetId
  | kind
  deriving DecidableEq

/-- The pin page key in comparison order. -/
def pageKey : List PageKey := [.sourceTime, .sourceId, .targetId, .kind]

/-- How a pin is presented at admission. -/
inductive Admission where
  | fresh
  | forkRevision
  | schemaEvolution
  deriving DecidableEq

/-- What witnesses an exact pin during admission. -/
inductive AdmissionWitness where
  | liveReadableTarget
  | capturedSourcePin
  deriving DecidableEq

/-- Fresh pins require a live readable target, while a retained pin requires an exact captured source pin. -/
def pinAdmissionWitness : Admission → AdmissionWitness
  | .fresh => .liveReadableTarget
  | .forkRevision | .schemaEvolution => .capturedSourcePin

/-- Whether neighbor edges are expanded when the request omits the flag. -/
def defaultNeighborExpansion : Bool := false

/-- The pin kinds compared for external-edge replay. -/
def externalReplayKinds : List PinKind := [.origin, .reference]

/-- The largest host read set, counted in distinct exact versions. -/
def maxReadPins : Nat := 1024

/-- Default payload references when a schema supplies none. -/
def defaultPayloadReferences : List String := []

/-- Default session observations when no host binds a read set. -/
def defaultReadSet : List String := []

/-- The maximum number of raw sources in one derive declaration. -/
def maxDeriveSources : Nat := 256

/-- The maximum number of seeds in a Think request. -/
def maxThinkSeeds : Nat := 8

/-- The default number of hops in a walk. -/
def defaultWalkDepth : Nat := 3

/-- The maximum number of hops in a walk. -/
def maxWalkDepth : Nat := 8

/-- The maximum number of distinct remote leaves read per lineage page. -/
def maxRemoteLeaves : Nat := 64

/-- The character ceiling for a remote walk sketch. -/
def maxRemoteSketchChars : Nat := 200

/-- The maximum number of hydrated neighbor pins per memory. -/
def maxNeighborPins : Nat := 200

/-- The minimum number of candidate source rows in an inbound page read. -/
def inboundSourceFloor : Nat := 256

/-- The source limit for an inbound probe requesting just one edge. -/
def singlePinProbeRows : Nat := 2

/-- The edge ceiling of a query snapshot, independent of its node window. -/
def maxSnapshotEdges : Nat := 50000

/-- Pin target requests attempted before coarse locking is considered. -/
def finePinLockBudget : Nat := 256

end Proxiima.Edges
