/-! # Schema evolution -/

namespace Proxiima.SchemaEvolution

/-- The inputs checked before ordering the executable steps. -/
inductive ScheduleInput where
  | owningMigrationAnchors
  | explicitDependencies
  | crossSchemaProducers
  | earlierAnchorsInOwningLane
  deriving DecidableEq

/-- These inputs determine the acyclic schedule before writes. -/
def scheduleInputs : List ScheduleInput :=
  [.owningMigrationAnchors, .explicitDependencies, .crossSchemaProducers, .earlierAnchorsInOwningLane]

/-- The inputs of an owner-scoped conversion. -/
inductive ConversionInput where
  | ownedSourceAdmissionsIncludingForgotten
  | earlierMappingsForOwner
  | earlierForeignMappingsForReferencedTargets
  deriving DecidableEq

/-- The transform receives one owner's historical admissions and only the earlier mappings it may inspect. -/
def conversionInputs : List ConversionInput :=
  [.ownedSourceAdmissionsIncludingForgotten, .earlierMappingsForOwner, .earlierForeignMappingsForReferencedTargets]

/-- The obligations of the declared source-to-target relation. -/
inductive MappingCheck where
  | everySourceMapped
  | everyTargetDeclaredAndMapped
  | sameOwner
  | sameAdmissionKind
  | compatibleRetainedState
  deriving DecidableEq

/-- Every source and target participates in an owner-preserving relation, including splits and merges. -/
def mappingChecks : List MappingCheck :=
  [.everySourceMapped, .everyTargetDeclaredAndMapped, .sameOwner, .sameAdmissionKind, .compatibleRetainedState]

/-- Goal fields carrying exact version targets during cutover. -/
inductive GoalPinRole where
  | assignment
  | dependency
  | evidence
  | motivation
  | closeFact
  | writeAct
  deriving DecidableEq

/-- The exact Goal pins checked at cutover. -/
def goalPinRoles : List GoalPinRole :=
  [.assignment, .dependency, .evidence, .motivation, .closeFact, .writeAct]

/-- How retained identities are routed after transformation. -/
inductive IdentityRoute where
  | retainAdmissionAndSeries
  | primaryForHistoricalReplay
  | everySuccessorForCaptureErase
  deriving DecidableEq

/-- A target can retain its admission and series identity, replay follows the primary target, and capture erase follows every successor. -/
def identityRoutes : List IdentityRoute :=
  [.retainAdmissionAndSeries, .primaryForHistoricalReplay, .everySuccessorForCaptureErase]

/-- The Goal collaboration obligations of successor admissions. -/
inductive CollaborationCheck where
  | sameAuthorityOnEveryMergeContributor
  | sameAuthorityOnEachSplitSuccessor
  | compatibleGeneration
  deriving DecidableEq

/-- A cutover carries exact collaboration authority with its generation. -/
def collaborationChecks : List CollaborationCheck :=
  [.sameAuthorityOnEveryMergeContributor, .sameAuthorityOnEachSplitSuccessor, .compatibleGeneration]

/-- The source lock sequence used before transformation. -/
inductive SourceFence where
  | admissionTables
  | sortedSourceSchemaKeys
  deriving DecidableEq

/-- Evolution locks the admission tables before the ordered source schema keys. -/
def sourceFenceOrder : List SourceFence := [.admissionTables, .sortedSourceSchemaKeys]

/-- The fences acquired by erase where it serializes with evolution. -/
inductive EraseFence where
  | evolutionTables
  | ownerFence
  deriving DecidableEq

/-- Erase waits on the evolution tables before taking its owner fence. -/
def eraseFenceOrder : List EraseFence := [.evolutionTables, .ownerFence]

/-- The cutover order from anchored SQL through the remaining lane SQL. -/
inductive CutoverStage where
  | runSqlThroughAnchor
  | transformOwnerInputs
  | checkpointStepConstraints
  | retireSourcesAndFinishClosure
  | runTrailingSql
  deriving DecidableEq

/-- Trailing SQL follows closure and source retirement. -/
def cutoverOrder : List CutoverStage :=
  [.runSqlThroughAnchor, .transformOwnerInputs, .checkpointStepConstraints, .retireSourcesAndFinishClosure, .runTrailingSql]

/-- Final actions of the cutover transaction after all conversions. -/
inductive ClosureStage where
  | rejectUnresolvedIncomingPins
  | validateTargetsAndGraph
  | enforceDeferredDataConstraints
  | removePermits
  deriving DecidableEq

/-- Final validation holds the conversion permits through deferred constraint checks. -/
def closureChecks : List ClosureStage :=
  [.rejectUnresolvedIncomingPins, .validateTargetsAndGraph, .enforceDeferredDataConstraints, .removePermits]

/-- Which state a payload rewrite rebuilds or replaces. -/
inductive RebuildPart where
  | projections
  | sketches
  | embeddingJobsForMappedSpaces
  | discardObsoleteVectorsAndClaims
  deriving DecidableEq

/-- Generated payload state is rebuilt and embedding spaces follow the mapping without carrying old vectors or claims. -/
def rebuildParts : List RebuildPart :=
  [.projections, .sketches, .embeddingJobsForMappedSpaces, .discardObsoleteVectorsAndClaims]

/-- Steps in the boot-time inverse for a retired Fact. -/
inductive RetiredDrainStep where
  | inventoryRawAdmissionsByAscendingOwnerPages
  | commitEachOwnerPage
  | resumeFromRemainingAdmissions
  deriving DecidableEq

/-- Retired Fact drain uses raw admissions and resumes at committed page boundaries. -/
def retiredDrain : List RetiredDrainStep :=
  [.inventoryRawAdmissionsByAscendingOwnerPages, .commitEachOwnerPage, .resumeFromRemainingAdmissions]

end Proxiima.SchemaEvolution
