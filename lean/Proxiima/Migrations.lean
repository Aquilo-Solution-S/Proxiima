/-! # Migrations -/

namespace Proxiima.Migrations

/-- Which entry points can apply migration DDL. -/
inductive SchemaEntry where
  | explicitMigrate
  | serving
  | maintenance
  | pendingPlan
  deriving DecidableEq

/-- The schema operation performed by each entry point. -/
inductive SchemaWork where
  | apply
  | validate
  | plan
  deriving DecidableEq

/-- Only the explicit migration entry point applies schema changes. -/
def schemaWork : SchemaEntry → SchemaWork
  | .explicitMigrate => .apply
  | .serving | .maintenance => .validate
  | .pendingPlan => .plan

/-- The checks required before a validating preflight succeeds. -/
inductive PreflightCheck where
  | ledgerIntegrity
  | coreBaselineAndDefinitions
  | allNonCoreMigrationsApplied
  | sidecarOwnerRls
  | candidateDefinitions
  | schemaEvolutions
  deriving DecidableEq

/-- Schema readiness checks under one locked snapshot. -/
def preflightChecks : List PreflightCheck :=
  [.ledgerIntegrity, .coreBaselineAndDefinitions, .allNonCoreMigrationsApplied,
   .sidecarOwnerRls, .candidateDefinitions, .schemaEvolutions]

/-- The possible identities of the core ledger at admission. -/
inductive CoreHistory where
  | blankWithoutSchema
  | baselineWithoutEarlierRows
  | exactPreviousHead
  | unreconciled
  deriving DecidableEq

/-- How core proceeds after classifying its ledger. -/
inductive CoreAdmission where
  | install
  | continueFromBaseline
  | adoptInMigrationTransaction
  | requireReset
  deriving DecidableEq

/-- Only a blank store, the baseline or the exact previous head can enter core migration. -/
def coreAdmission : CoreHistory → CoreAdmission
  | .blankWithoutSchema => .install
  | .baselineWithoutEarlierRows => .continueFromBaseline
  | .exactPreviousHead => .adoptInMigrationTransaction
  | .unreconciled => .requireReset

/-- The largest sequential version attributable to the core lane. -/
def coreVersionCeiling : Nat := 9999

/-- How the host handles a recorded version missing from its composed sources. -/
inductive MissingVersionPolicy where
  | reject
  | allow
  deriving DecidableEq

/-- The declared environment default for missing recorded versions. -/
def missingVersionDefault : String := "reject"

/-- Whether a recorded version is admitted by the source union's missing-version check. -/
def recordedVersionAdmitted : MissingVersionPolicy → (shipped : Bool) → Bool
  | .reject, false => false
  | .reject, true | .allow, false | .allow, true => true

/-- Checks retained under either missing-version policy. -/
inductive LedgerIntegrityCheck where
  | successfulRows
  | shippedChecksums
  | ledgerAssignment
  deriving DecidableEq

/-- Allowing extra recorded versions does not waive ledger integrity. -/
def ledgerIntegrity : List LedgerIntegrityCheck := [.successfulRows, .shippedChecksums, .ledgerAssignment]

/-- The private lane's relationship to unknown history when work is pending. -/
inductive PrivateLaneHistory where
  | pendingRootBesideUnknown
  | laterPendingBeforeUnknown
  | compatible
  deriving DecidableEq

/-- A private lane conflict that admitting unknown versions cannot waive. -/
inductive LineageConflict where
  | replaced
  | diverged
  deriving DecidableEq

/-- A pending root beside unknown history means replacement and later pending work before unknown history means divergence. -/
def privateLineage : PrivateLaneHistory → Option LineageConflict
  | .pendingRootBesideUnknown => some .replaced
  | .laterPendingBeforeUnknown => some .diverged
  | .compatible => none

/-- The relation of the recorded definitions to the running binary. -/
inductive DefinitionRecord where
  | missingOrChangedChecksum
  | matchingChecksum
  | newerRelease
  | malformedRelease
  deriving DecidableEq

/-- How the definitions pass treats its recorded state. -/
inductive DefinitionWork where
  | reinstall
  | keep
  | refuse
  deriving DecidableEq

/-- Newer definitions stay in place only under Allow, and an absent or changed checksum calls for transactional reinstallation. -/
def definitionWork : DefinitionRecord → MissingVersionPolicy → DefinitionWork
  | .missingOrChangedChecksum, .reject | .missingOrChangedChecksum, .allow => .reinstall
  | .matchingChecksum, .reject | .matchingChecksum, .allow | .newerRelease, .allow => .keep
  | .newerRelease, .reject | .malformedRelease, .reject | .malformedRelease, .allow => .refuse

/-- The two core schema lanes selected by where a store keeps its vectors. -/
inductive VectorLane where
  | local
  | hostIndex
  deriving DecidableEq

/-- Only the local vector lane installs the pgvector migration and vector candidate functions. -/
def installsPgvector : VectorLane → (pgvectorOnly : Bool) → Bool
  | .local, true | .local, false | .hostIndex, false => true
  | .hostIndex, true => false

/-- The order and direction of a composed migration run. -/
inductive SourceOrder where
  | coreFirst
  | linkedSourcesInCompositionOrder
  | upMigrationsInMigratorOrder
  deriving DecidableEq

/-- Core precedes linked sources and each source applies only up migrations in migrator order. -/
def sourceOrder : List SourceOrder :=
  [.coreFirst, .linkedSourcesInCompositionOrder, .upMigrationsInMigratorOrder]

/-- Conflicts checked before connecting to a composed database. -/
inductive SourceConflict where
  | duplicateUpVersionAcrossComposition
  | duplicateResolvedPrivateLedger
  deriving DecidableEq

/-- Versions are unique across the composition, and sharing a resolved ledger is allowed only for core's ledger. -/
def sourceConflicts : List SourceConflict :=
  [.duplicateUpVersionAcrossComposition, .duplicateResolvedPrivateLedger]

/-- Placement of migration ledgers. -/
inductive LedgerSelection where
  | coreDefault
  | spiiceDefault
  | explicitlyConfigured
  deriving DecidableEq

/-- The table selected by a migration source's configuration. -/
inductive LedgerPlacement where
  | publicCoreLedger
  | publicIdDerivedLedger
  | configuredLedger
  deriving DecidableEq

/-- Spiices derive a durable public ledger unless they configure their own. -/
def ledgerPlacement : LedgerSelection → LedgerPlacement
  | .coreDefault => .publicCoreLedger
  | .spiiceDefault => .publicIdDerivedLedger
  | .explicitlyConfigured => .configuredLedger

/-- Maximum byte length of a spiice id in a derived ledger name. -/
def maxLedgerIdBytes : Nat := 40

/-- Normalization used to compare migration ledger names. -/
inductive IdentifierRule where
  | quotedPartsPreserveCaseAndUnescapeQuotes
  | unquotedPartsFoldAscii
  | truncateOnCharacterBoundary
  | unqualifiedLedgerResolvesToPublic
  deriving DecidableEq

/-- Compare ledger names by their resolved PostgreSQL parts. -/
def ledgerIdentifier : List IdentifierRule :=
  [.quotedPartsPreserveCaseAndUnescapeQuotes, .unquotedPartsFoldAscii,
   .truncateOnCharacterBoundary, .unqualifiedLedgerResolvesToPublic]

/-- Migration work that shares the same advisory lock. -/
inductive LockHolder where
  | fullRunSession
  | planSnapshotSession
  | preflightSnapshotSession
  | sourceTransaction
  deriving DecidableEq

/-- The full run, both read snapshots and source transactions serialize on the same key. -/
def lockHolders : List LockHolder :=
  [.fullRunSession, .planSnapshotSession, .preflightSnapshotSession, .sourceTransaction]

/-- The advisory lock key shared by migration sessions and source transactions. -/
def lockKey : Nat := 0x70726f786d696772

/-- The default wait for migration locks in milliseconds. -/
def lockWaitMillis : Nat := 5000

/-- How migrations dispose of session state on completion or cancellation. -/
inductive SessionDisposal where
  | markCloseOnDropBeforeChangingState
  | closeAndReleaseSessionState
  deriving DecidableEq

/-- Mark the connection for close before it can retain session state, including a lock wait. -/
def sessionDisposal : List SessionDisposal := [.markCloseOnDropBeforeChangingState, .closeAndReleaseSessionState]

/-- The transaction boundaries of source migration and final schema installation. -/
inductive MigrationTransaction where
  | requireTransactionalSource
  | sourceSqlAndLedger
  | finalCandidateDefinitionsAndPolicies
  deriving DecidableEq

/-- Refuse nontransactional source work, and commit the final catalog installation with its policies. -/
def migrationTransactions : List MigrationTransaction :=
  [.requireTransactionalSource, .sourceSqlAndLedger, .finalCandidateDefinitionsAndPolicies]

/-- The maximum attempts for a source transaction encountering shared-catalog contention. -/
def catalogRetryAttempts : Nat := 5

/-- The per-attempt multiplier of the shared-catalog retry delay, in milliseconds. -/
def catalogRetryDelayMillis : Nat := 100

/-- The steps that prepare a private ledger in the migration transaction. -/
inductive LedgerProtection where
  | createFromCoreTrackingShape
  | revokeNonOwnerWrites
  | refuseResidualWriteGrants
  deriving DecidableEq

/-- Private ledgers are prepared under the migration lock with writes confined to their owner. -/
def ledgerProtection : List LedgerProtection :=
  [.createFromCoreTrackingShape, .revokeNonOwnerWrites, .refuseResidualWriteGrants]

/-- What the repository migration check requires of a lane. -/
inductive HistoryCheck where
  | preserveReleasedBlobsInActiveLanes
  | retireOnlyEmptySpiiceLanes
  | requireVersionedSqlFilenames
  | newVersionsAboveReleasedLaneHead
  | distinctVersionsWithinLane
  | excludeMutableDefinitions
  | pendingEpochOnlyForNextRelease
  deriving DecidableEq

/-- Released migration history, filenames and numbering are checked per lane. -/
def historyChecks : List HistoryCheck :=
  [.preserveReleasedBlobsInActiveLanes, .retireOnlyEmptySpiiceLanes, .requireVersionedSqlFilenames,
   .newVersionsAboveReleasedLaneHead, .distinctVersionsWithinLane, .excludeMutableDefinitions,
   .pendingEpochOnlyForNextRelease]

/-- What the schema inspection tool does with its temporary database and output. -/
inductive InspectionStep where
  | createDisposableDatabase
  | replayEmbeddedSources
  | dumpSeparatedSchemasWithoutLedgers
  | normalizeDumpPreamble
  | dropDisposableDatabaseOnExit
  deriving DecidableEq

/-- Inspection replays into an isolated database and drops that database when finished. -/
def inspectionSteps : List InspectionStep :=
  [.createDisposableDatabase, .replayEmbeddedSources, .dumpSeparatedSchemasWithoutLedgers,
   .normalizeDumpPreamble, .dropDisposableDatabaseOnExit]

/-- Destinations considered before writing a schema inspection dump. -/
inductive InspectionDestination where
  | ignoredFileInCheckout
  | other
  deriving DecidableEq

/-- Only ignored output files in the checkout may receive schema dumps. -/
def inspectionDestinationAdmitted : InspectionDestination → Bool
  | .ignoredFileInCheckout => true
  | .other => false

/-- Sources in order for the development CLI's database URL. -/
inductive DevDatabaseSource where
  | databaseUrlFlag
  | databaseUrlSetting
  deriving DecidableEq

/-- The development migration CLI prefers its URL flag over the environment. -/
def devDatabaseSources : List DevDatabaseSource := [.databaseUrlFlag, .databaseUrlSetting]

/-- Required checks for the development database reset. -/
inductive ResetCheck where
  | exactConfirmation
  | localConnection
  | unprotectedDatabase
  | censusForeignDependents
  | deletePermissionForDependentLedgersOnOptIn
  deriving DecidableEq

/-- The development reset inspects dependencies and requires opt-in before clearing other lanes. -/
def resetChecks : List ResetCheck :=
  [.exactConfirmation, .localConnection, .unprotectedDatabase, .censusForeignDependents,
   .deletePermissionForDependentLedgersOnOptIn]

/-- Work committed together by the development database reset. -/
inductive ResetWork where
  | dropProxiimaSchemas
  | dropCompiledSpiiceLedgers
  | deleteAttributableSharedVersions
  | deleteUnattributedAndDependentRowsOnOptIn
  deriving DecidableEq

/-- The development reset commits schema drops and their ledger changes together. -/
def resetCommit : List ResetWork :=
  [.dropProxiimaSchemas, .dropCompiledSpiiceLedgers, .deleteAttributableSharedVersions,
   .deleteUnattributedAndDependentRowsOnOptIn]

/-- Database and registry requirements of a retired Fact schema. -/
inductive RetirementCheck where
  | inactiveSchemaWithByKeyInverse
  | unrestorable
  | drainBeforeDestructiveDdl
  deriving DecidableEq

/-- Retired Facts keep their erase inverse, and destructive DDL requires the retired storage to be drained. -/
def retirementChecks : List RetirementCheck := [.inactiveSchemaWithByKeyInverse, .unrestorable, .drainBeforeDestructiveDdl]

/-- Decisions on preflight failures requiring operator action. -/
inductive ReadinessFailure where
  | unreconciledCoreHistory
  | pendingCoreOrLinkedMigration
  | missingRequiredSidecarState
  deriving DecidableEq

/-- Admission errors that distinguish reset from migration. -/
inductive ReadinessRefusal where
  | requireReset
  | requireMigration
  deriving DecidableEq

/-- Boot preserves typed migration and reset refusals. -/
def readinessRefusal : ReadinessFailure → ReadinessRefusal
  | .unreconciledCoreHistory => .requireReset
  | .pendingCoreOrLinkedMigration | .missingRequiredSidecarState => .requireMigration

/-- Shared-catalog retry classifications. -/
inductive CatalogFailure where
  | concurrentTupleChange
  | uniqueViolationOnRoleCatalog
  | other
  deriving DecidableEq

/-- Only shared-catalog contention is eligible for the migration retry budget. -/
def retryCatalog : CatalogFailure → Bool
  | .concurrentTupleChange | .uniqueViolationOnRoleCatalog => true
  | .other => false

end Proxiima.Migrations
