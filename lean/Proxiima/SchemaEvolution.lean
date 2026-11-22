/-! # Schema evolution -/

namespace Proxiima.SchemaEvolution

/-- The parts of a spiice's executable evolution declaration. -/
inductive DeclarationPart where
  | sourceAndTargetContracts
  | owningMigrationAnchor
  | stepDependencies
  | typedCodecs
  | transform
  | change
  | consumerAction
  deriving DecidableEq

/-- Freeze binds the executable and the catalog to one evolution declaration. -/
def declaration : List DeclarationPart :=
  [.sourceAndTargetContracts, .owningMigrationAnchor, .stepDependencies,
   .typedCodecs, .transform, .change, .consumerAction]

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

/-- The fields projected to consumers of an evolution target. -/
inductive CatalogField where
  | change
  | consumerAction
  deriving DecidableEq

/-- A declared target projects both evolution fields. -/
def catalogFields : List CatalogField := [.change, .consumerAction]

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

/-- Whether the old admission carrying a pin is still present after cutover. -/
inductive PinCarrier where
  | retained
  | removed
  deriving DecidableEq

/-- Every remaining carrier must resolve its pins when the source admissions are removed. -/
def resolveIncomingPins : PinCarrier → Bool
  | .retained => true
  | .removed => false

/-- Goal fields carrying exact version targets during cutover. -/
inductive GoalPinRole where
  | assignment
  | dependency
  | evidence
  | motivation
  | closeFact
  | writeAct
  deriving DecidableEq

/-- The exact Goal pins checked at cutover, independent of the parent series handle. -/
def goalPinRoles : List GoalPinRole :=
  [.assignment, .dependency, .evidence, .motivation, .closeFact, .writeAct]

/-- How retained identities are routed after transformation. -/
inductive IdentityRoute where
  | retainFactAdmissionAndSeries
  | primaryForHistoricalReplay
  | everySuccessorForCaptureErase
  deriving DecidableEq

/-- A Fact can retain its identity; replay follows the primary target and capture erase follows every successor. -/
def identityRoutes : List IdentityRoute :=
  [.retainFactAdmissionAndSeries, .primaryForHistoricalReplay, .everySuccessorForCaptureErase]

/-- The Goal collaboration obligations of successor admissions. -/
inductive CollaborationCheck where
  | sameAuthorityOnEveryMergeContributor
  | sameAuthorityOnEachSplitSuccessor
  | compatibleGeneration
  deriving DecidableEq

/-- A cutover carries exact collaboration authority with its generation. -/
def collaborationChecks : List CollaborationCheck :=
  [.sameAuthorityOnEveryMergeContributor, .sameAuthorityOnEachSplitSuccessor, .compatibleGeneration]

/-- The states of a source schema at ordinary write admission. -/
inductive SourceSchemaState where
  | beforeCompletion
  | completed
  deriving DecidableEq

/-- Ordinary source writes stop when completion is recorded. -/
def sourceWritable : SourceSchemaState → Bool
  | .beforeCompletion => true
  | .completed => false

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

/-- Schema phases that handle generated Follows and search state. -/
inductive GeneratedStatePhase where
  | migration
  | serving
  deriving DecidableEq

/-- How a phase handles generated Follows and search state. -/
inductive GeneratedStateWork where
  | reconcile
  | validate
  deriving DecidableEq

/-- Migration reconciles generated state even with no pending SQL; serving validates it. -/
def generatedStateWork : GeneratedStatePhase → GeneratedStateWork
  | .migration => .reconcile
  | .serving => .validate

/-- Steps in the boot-time inverse for a retired Fact. -/
inductive RetiredDrainStep where
  | inventoryRawAdmissionsByAscendingOwnerPages
  | commitEachOwnerPage
  | resumeFromRemainingAdmissions
  deriving DecidableEq

/-- Retired Fact drain uses raw admissions and resumes at committed page boundaries. -/
def retiredDrain : List RetiredDrainStep :=
  [.inventoryRawAdmissionsByAscendingOwnerPages, .commitEachOwnerPage, .resumeFromRemainingAdmissions]

/-- The steps that prove a retired Fact's tables can be dropped. -/
inductive DropCheck where
  | takeWriterFence
  | proveNoAdmissionNamesSchemaOrSurfaces
  deriving DecidableEq

/-- Destructive DDL checks that no admission names the retired schema or its tables while holding the writer fence. -/
def fencedDropChecks : List DropCheck := [.takeWriterFence, .proveNoAdmissionNamesSchemaOrSurfaces]

/-- The checks on a typed fork copy. -/
inductive ForkCopyCheck where
  | abstractionOrPerspective
  | explicitCopyPolicy
  | unpinnedSidecar
  | sourceUse
  | destinationWrite
  deriving DecidableEq

/-- Typed copies require an opted-in unpinned derived schema and ordinary admission. -/
def forkCopyChecks : List ForkCopyCheck :=
  [.abstractionOrPerspective, .explicitCopyPolicy, .unpinnedSidecar, .sourceUse, .destinationWrite]

end Proxiima.SchemaEvolution
