/-! # Memory -/

namespace Proxiima.Memory

/-- The three layers of the Memory spine. -/
inductive Kind where
  /-- An admitted observation. -/
  | Fact
  /-- A typed pattern derived from admitted inputs. -/
  | Abstraction
  /-- A derived stance or an interpretation. -/
  | Perspective
  deriving DecidableEq

/-- The layers of Memory and the separate Goal spine. -/
inductive EntityKind where
  | Fact
  | Abstraction
  | Perspective
  | Goal
  deriving DecidableEq

/-- The Memory layer of an entity, with Goals on their own spine. -/
def memoryKind : EntityKind → Option Kind
  | .Fact => some .Fact
  | .Abstraction => some .Abstraction
  | .Perspective => some .Perspective
  | .Goal => none

/-- The lifecycle surfaces that can change a stored admission or its equivalent storage pointers. -/
inductive VersionChange where
  | observationOrRevision
  | forgetOrRestore
  | ownerTransfer
  | capturedSchemaEvolution
  deriving DecidableEq

/-- How each operation acts on the stored admission. -/
inductive VersionAction where
  | appendVersion
  | updateLifecycle
  | updateOwnerAndEquivalentPointers
  | applyCapturedMapping
  deriving DecidableEq

/-- Observations and revisions append, and in-place changes are limited to the lifecycle, transfer and captured-evolution paths. -/
def versionAction : VersionChange → VersionAction
  | .observationOrRevision => .appendVersion
  | .forgetOrRestore => .updateLifecycle
  | .ownerTransfer => .updateOwnerAndEquivalentPointers
  | .capturedSchemaEvolution => .applyCapturedMapping

/-- The action for a deterministic derived-series head probe. -/
inductive DerivedHeadAction where
  | replayStoredVersion
  | refuseReferenceDrift
  | appendVersion
  | probeForgotten
  deriving DecidableEq

/-- Equal origins and references replay the stored version when guidance agrees, while changed origins permit a new revision after the forgotten-version probe. -/
def derivedHeadAction : (originsMatch : Bool) → (referencesMatch : Bool) →
    (guidanceMatches : Bool) → DerivedHeadAction
  | false, false, false | false, false, true | false, true, false | false, true, true =>
    .probeForgotten
  | true, false, false | true, false, true => .refuseReferenceDrift
  | true, true, false => .appendVersion
  | true, true, true => .replayStoredVersion

/-- Where an admission's occurrence time comes from when it has one. -/
inductive OccurrenceSource where
  | supplied
  | admissionTime
  deriving DecidableEq

/-- Facts use the supplied occurrence or the write time encoded by their admission ID, and derived Memory has no occurrence time. -/
def occurrenceSource : Kind → (supplied : Bool) → Option OccurrenceSource
  | .Fact, true => some .supplied
  | .Fact, false => some .admissionTime
  | .Abstraction, true | .Abstraction, false | .Perspective, true | .Perspective, false => none

/-- The greatest allowed future occurrence offset, in minutes relative to the admission clock. -/
def futureOccurrenceMinutes : Nat := 5

/-- The UTC occurrence precision in nanoseconds per stored tick. -/
def occurrenceQuantumNanoseconds : Nat := 1000

/-- Version states in the caller's readable view of a series. -/
inductive StatefulVersion where
  | latestReadable
  | latestReadableTombstone
  | forgotten
  | olderReadable
  deriving DecidableEq

/-- Natural-key prefix reads page the newest readable Fact per series, including a visible Goal publication or spiice tombstone. -/
def statefulHeadVisible : StatefulVersion → Bool
  | .latestReadable | .latestReadableTombstone => true
  | .forgotten | .olderReadable => false

/-- The largest trimmed title in characters for remembered notes and core derivations. -/
def titleMax : Nat := 240

/-- The largest trimmed body in characters for remembered notes, core derivations and recorded utterances. -/
def bodyMax : Nat := 20000

/-- The largest trimmed interpretation claim in characters. -/
def claimMax : Nat := 1000

/-- The largest raw interpretation subject list. -/
def subjectMax : Nat := 64

/-- The highest admitted confidence on the inclusive interpretation scale. -/
def confidenceMax : Nat := 100

/-- The interpretation confidence used when the tool receives no score. -/
def confidenceDefault : Nat := 80

/-- The ordered components of the deterministic interpretation handle. -/
inductive InterpretationKeyField where
  | destinationNamespace
  | operatorLabel
  | claim
  | confidence
  | localSubjectVersions
  | sortedRemoteSubjectHandles
  deriving DecidableEq

/-- The handle hashes the local key and folds in sorted remote handles when present, so reasserting the same interpretation lands on one memory. -/
def interpretationKey : List InterpretationKeyField :=
  [.destinationNamespace, .operatorLabel, .claim, .confidence, .localSubjectVersions,
   .sortedRemoteSubjectHandles]

/-- Stages that derive a Memory or a Goal from admitted inputs. -/
inductive OperatorPhase where
  | ToA
  | AtoP
  | AtoGoal
  deriving DecidableEq

/-- The admissible input layers for an operator invocation. -/
def operatorAdmitsInput : OperatorPhase → EntityKind → Bool
  | .ToA, .Fact | .ToA, .Abstraction | .AtoP, .Abstraction | .AtoGoal, .Abstraction => true
  | .ToA, .Perspective | .ToA, .Goal
  | .AtoP, .Fact | .AtoP, .Perspective | .AtoP, .Goal
  | .AtoGoal, .Fact | .AtoGoal, .Perspective | .AtoGoal, .Goal => false

/-- The layer or spine of each operator phase's output. -/
def operatorOutput : OperatorPhase → EntityKind
  | .ToA => .Abstraction
  | .AtoP => .Perspective
  | .AtoGoal => .Goal

end Proxiima.Memory
