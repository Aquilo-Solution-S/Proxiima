import Proxiima.Edges

/-! # Erasure and export -/

namespace Proxiima.ErasureExport

/-- The shortest configurable retention in days. -/
def minKeepDays : Nat := 1

/-- The longest configurable retention in days. -/
def maxKeepDays : Nat := 36500

/-- The series ceiling for an explicit erase or the seed window of an erase page. -/
def maxEraseSeries : Nat := 256

/-- The maximum versions erased in one call. -/
def maxEraseVersions : Nat := 1024

/-- The default export and bulk erase fence wait in seconds. -/
def fenceWaitSeconds : Nat := 5

/-- The route that receives forwarded retention erase requests. -/
def retentionErasePath : String := "/retention/erase"

/-- The signed claim inspected by the receiving retention host. -/
def vouchClaim : String := "vouch"

/-- The vouch value accepted for a verified forwarded retention caller. -/
def retentionVouch : String := "retention"

/-- The evidence that a target owner may be erased. -/
inductive OwnerEraseEvidence where
  | personal (dropVerified : Bool)
  | other (hasMembers : Bool)
  deriving DecidableEq

/-- The preconditions storage distinguishes while holding the membership fence. -/
inductive OwnerEraseAdmission where
  | allow
  | missingDropProof
  | stillLive
  deriving DecidableEq

/-- A registered personal owner needs verified drop proof and any other owner must have no members. -/
def admitOwnerErase : OwnerEraseEvidence → OwnerEraseAdmission
  | .personal true | .other false => .allow
  | .personal false => .missingDropProof
  | .other true => .stillLive

/-- Outcomes of the shared owner-inverse authority route. -/
inductive AuthorityRoute where
  | delegated
  | system
  | host (provider : Bool)
  deriving DecidableEq

/-- The action to take before storage admission. -/
inductive AuthorityAction where
  | refuse
  | grant
  | askHost
  deriving DecidableEq

/-- The system route grants directly, the host route consults its provider when present, and delegated or unconfigured requests are refused. -/
def inverseAuthority : AuthorityRoute → AuthorityAction
  | .delegated | .host false => .refuse
  | .system => .grant
  | .host true => .askHost

/-- How owner erase outcomes reach the host. -/
inductive OperationReceipt where
  | completed
  | notFound
  | unauthorized
  | refused
  deriving DecidableEq

/-- The outcomes carrying the operation ID minted for the attempt. -/
def eraseReceiptOutcomes : List OperationReceipt :=
  [.completed, .notFound, .unauthorized, .refused]

/-- Which authority rows are selected by an erase. -/
inductive AuthorityRows where
  | exactOwner
  | erasedSubject
  deriving DecidableEq

/-- The scope of a bulk inverse. -/
inductive BulkScope where
  | owner
  | source
  deriving DecidableEq

/-- The fields used to scope explicitly attributable bulk rows. -/
inductive SelectionKey where
  | owner
  | source
  deriving DecidableEq

/-- A source narrows the owner selection to rows attributable to it. -/
def bulkSelectionKey : BulkScope → List SelectionKey
  | .owner => [.owner]
  | .source => [.owner, .source]

/-- Source erase leaves owner-level grants in place and owner erase removes exact owner and subject grants. -/
def grantSelection : BulkScope → List AuthorityRows
  | .owner => [.exactOwner, .erasedSubject]
  | .source => []

/-- Whether a blob belongs to the erase scope. -/
def eraseBlob : BulkScope → (selected retainedCitation : Bool) → Bool
  | .owner, true, true | .owner, true, false => true
  | .source, true, false => true
  | .owner, false, true | .owner, false, false
  | .source, true, true | .source, false, true | .source, false, false => false

/-- Stages that turn an authorized physical erase into a completed receipt. -/
inductive CompletionStep where
  | settleHostIndex
  | commitDatabaseAndColdDebt
  | attemptColdPurge
  | reportOutstandingDebt
  deriving DecidableEq

/-- Index settlement is synchronous and cold-object work is attempted after database commit. -/
def completionOrder : List CompletionStep :=
  [.settleHostIndex, .commitDatabaseAndColdDebt, .attemptColdPurge, .reportOutstandingDebt]

/-- The access route of an own-series erase. -/
inductive SeriesAuthority where
  | ownerAdmin
  | system
  | hostState
  | goalWork
  deriving DecidableEq

/-- Owner administrators and system authority can issue scoped erasure. -/
def seriesAuthority : SeriesAuthority → Bool
  | .ownerAdmin | .system => true
  | .hostState | .goalWork => false

/-- The required parts of scoped erase admission. -/
inductive SeriesScopeCheck where
  | oneOwner
  | nonCoreSpiice
  | ownMemorySchemas
  | invokingActionIsDeclaredDestructive
  deriving DecidableEq

/-- The scoped inverse checks the owner's footprint and the invoking spiice and action. -/
def seriesScope : List SeriesScopeCheck :=
  [.oneOwner, .nonCoreSpiice, .ownMemorySchemas, .invokingActionIsDeclaredDestructive]

/-- How a scoped inverse handles its exclusive transaction. -/
inductive SeriesTransaction where
  | refuse
  | runAndKeepForCommit
  | runAndRollback
  deriving DecidableEq

/-- Scoped erase requires an unused unit, then runs the same inverse for either mode. -/
def seriesTransaction : (unused dryRun : Bool) → SeriesTransaction
  | false, false | false, true => .refuse
  | true, false => .runAndKeepForCommit
  | true, true => .runAndRollback

/-- The version or versions examined by a selector. -/
inductive SelectionVersions where
  | explicit
  | newestIncludingForgotten
  | oldestMatchingSidecar
  | newestWithDeclaredClock
  deriving DecidableEq

/-- The supported scoped erase selectors. -/
inductive Selector where
  | ids
  | admittedBefore
  | occurredBefore
  | sidecarEquals
  | declaredBefore
  deriving DecidableEq

/-- The seed versions considered by each selector before expansion. -/
def seedVersions : Selector → SelectionVersions
  | .ids => .explicit
  | .admittedBefore | .occurredBefore => .newestIncludingForgotten
  | .sidecarEquals => .oldestMatchingSidecar
  | .declaredBefore => .newestWithDeclaredClock

/-- The selected series age clock. -/
inductive AgeClock where
  | admission
  | occurrenceOrAdmission
  | declared
  deriving DecidableEq

/-- The age selections and their effective clocks. -/
inductive AgeSelector where
  | admittedBefore
  | occurredBefore
  | declaredBefore
  deriving DecidableEq

/-- Time-based selectors compare and order by the same effective clock, then by exact version ID. -/
def ageClock : AgeSelector → AgeClock
  | .admittedBefore => .admission
  | .occurredBefore => .occurrenceOrAdmission
  | .declaredBefore => .declared

/-- The steps that close and erase a scoped relational footprint. -/
inductive ClosureStep where
  | expandWholeSeries
  | includeKeyedRestrictReferrers
  | orderDependantsBeforeTargets
  | lockAndRecheck
  | deleteInternalReferenceRows
  | deleteVersions
  deriving DecidableEq

/-- Whole series and keyed restricting referrers form the closure checked before delete. -/
def closureOrder : List ClosureStep :=
  [.expandWholeSeries, .includeKeyedRestrictReferrers, .orderDependantsBeforeTargets,
   .lockAndRecheck, .deleteInternalReferenceRows, .deleteVersions]

/-- The indivisible units of a scoped erase page. -/
inductive PageUnit where
  | series
  | mutuallyReferringSeries
  deriving DecidableEq

/-- A page may split its ordered closure only between these units. -/
def pageUnits : List PageUnit := [.series, .mutuallyReferringSeries]

/-- The actions taken when a seed cannot be included in a page. -/
inductive BlockedSeedAction where
  | refuse
  | returnEarlierPage
  deriving DecidableEq

/-- Explicit selections refuse as a whole, and paged selections stop before a blocked seed if earlier work fits. -/
def blockedSeed : (paged hasEarlierWork : Bool) → BlockedSeedAction
  | true, true => .returnEarlierPage
  | true, false | false, true | false, false => .refuse

/-- The surviving Memory pin fields counted in a scoped erase receipt. -/
def countedInboundFields : List Proxiima.Edges.PinField := [.origins, .refs]

/-- Acquisition order for a Memory erase selection, using sorted distinct identities at each stage. -/
inductive SelectionLock where
  | seriesHandle
  | versionLifecycle
  | admissionRow
  deriving DecidableEq

/-- Handle and lifecycle locks precede row locks on the complete footprint. -/
def selectionLockOrder : List SelectionLock := [.seriesHandle, .versionLifecycle, .admissionRow]

/-- Liveness checks before a cited object becomes cold debt. -/
inductive BlobLivenessCheck where
  | noSurvivingAdmission
  | noSurvivingUploadAlias
  deriving DecidableEq

/-- Blob deletion checks remaining citations and object deletion separately checks remaining aliases. -/
def blobLiveness : List BlobLivenessCheck := [.noSurvivingAdmission, .noSurvivingUploadAlias]

/-- The operations on the owner-and-schema retention row. -/
inductive RetentionOperation where
  | set
  | clear
  | list
  | inventory
  deriving DecidableEq

/-- The witness used by each retention operation. -/
inductive RetentionAuthority where
  | ownerEraseAdmin
  | readableOwners
  | system
  deriving DecidableEq

/-- Retention edits take erase administration, listing takes scoped read authority, and inventory takes system authority. -/
def retentionAuthority : RetentionOperation → RetentionAuthority
  | .set | .clear => .ownerEraseAdmin
  | .list => .readableOwners
  | .inventory => .system

/-- The identity of one retention policy row. -/
inductive RetentionKeyPart where
  | owner
  | schema
  deriving DecidableEq

/-- The retention row key and inventory order. -/
def retentionKey : List RetentionKeyPart := [.owner, .schema]

/-- A remote retention request must establish forwarded authority, a retention vouch and owner binding. -/
inductive RemoteAdmission where
  | forwardedOwner
  | signedRetentionVouch
  | sameRequestOwner
  | cutoffNotFuture
  deriving DecidableEq

/-- The remote retention checks performed before the series selection. -/
def remoteAdmission : List RemoteAdmission :=
  [.forwardedOwner, .signedRetentionVouch, .sameRequestOwner, .cutoffNotFuture]

/-- The loop order for a remotely requested retention erase. -/
inductive RetentionPageStep where
  | eraseEligibleSeries
  | commitPage
  | sumReceipt
  | repeatWhileMore
  deriving DecidableEq

/-- Remote retention commits each page before adding its receipt to the answer. -/
def retentionPageOrder : List RetentionPageStep :=
  [.eraseEligibleSeries, .commitPage, .sumReceipt, .repeatWhileMore]

/-- The two operations sharing the export and erase table fence. -/
inductive FenceUse where
  | export
  | erase
  deriving DecidableEq

/-- The combinations admitted to run concurrently by the fence. -/
def compatibleFence : FenceUse → FenceUse → Bool
  | .export, .export | .erase, .erase => true
  | .export, .erase | .erase, .export => false

/-- The steps that establish an export snapshot. -/
inductive ExportSnapshotStep where
  | takeSharedEraseFence
  | readRepeatableReadOnlySnapshot
  deriving DecidableEq

/-- Export waits for the shared erase fence before taking its repeatable read snapshot. -/
def exportSnapshotOrder : List ExportSnapshotStep :=
  [.takeSharedEraseFence, .readRepeatableReadOnlySnapshot]

/-- The possible repairs to a head after deleting its selected admissions. -/
inductive SelectedHeadDisposition where
  | keepNewestRemaining
  | remove
  deriving DecidableEq

/-- A selected head follows the latest remaining version or disappears when its series is empty. -/
def selectedHeadDispositions : List SelectedHeadDisposition := [.keepNewestRemaining, .remove]

/-- The order used for a generated export surface. -/
inductive ExportOrderPart where
  | declaredKey
  | remainingPrimaryKey
  deriving DecidableEq

/-- The declared key followed by the remaining primary key columns produces a total export order. -/
def exportRowOrder : List ExportOrderPart := [.declaredKey, .remainingPrimaryKey]

/-- The sources of the counts on an owner export bundle. -/
inductive ExportCountSource where
  | tableRows
  | projectedEdges
  deriving DecidableEq

/-- Export counts are computed from the returned rows and edges. -/
def exportCountSources : List ExportCountSource := [.tableRows, .projectedEdges]

/-- Publication copies reached by a bulk erase. -/
inductive PublicationCopySelector where
  | selectedPhysicalFacts
  | originalOwnerAndSource
  deriving DecidableEq

/-- Both physical identity and original attribution select delivery copies, including copies that followed a transfer. -/
def publicationCopySelectors : List PublicationCopySelector :=
  [.selectedPhysicalFacts, .originalOwnerAndSource]

/-- The erase declarations and evidence used to derive a physical leg. -/
inductive EraseDeclaration where
  | keyed (hasSingleHome bespoke : Bool)
  | owned (sourceAttributed bespoke : Bool)
  | cascade
  | hostState
  | retain
  deriving DecidableEq

/-- The one physical leg or deliberate retention for a declared surface. -/
inductive EraseLeg where
  | keyed
  | ownerOnly
  | ownerAndSource
  | bespoke
  | cascade
  | hostState
  | retained
  | unreachable
  deriving DecidableEq

/-- A keyed erase needs a single selected home or a bespoke leg, while generated owner erase narrows by source only for source-attributable surfaces. -/
def classifyErase : EraseDeclaration → EraseLeg
  | .keyed true true | .keyed false true | .owned true true | .owned false true => .bespoke
  | .keyed true false => .keyed
  | .keyed false false => .unreachable
  | .owned true false => .ownerAndSource
  | .owned false false => .ownerOnly
  | .cascade => .cascade
  | .hostState => .hostState
  | .retain => .retained

/-- The policy and key evidence used to derive a transfer leg. -/
inductive TransferDeclaration where
  | follow (hasVersionKeyAndOwner bespoke : Bool)
  | drop (hasVersionKey bespoke : Bool)
  | dedupe (bespoke : Bool)
  | stayOnKey
  | retainAtSource
  | refuseTransfer
  deriving DecidableEq

/-- The one leg selected for a surface during a Memory series transfer. -/
inductive TransferLeg where
  | follow
  | drop
  | dedupe
  | bespoke
  | stayOnKey
  | retained
  | refused
  | unreachable
  deriving DecidableEq

/-- Generated follow and drop require a version key, follow also needs an owner column, and dedupe needs a bespoke orchestrator. -/
def classifyTransfer : TransferDeclaration → TransferLeg
  | .follow true true | .follow false true | .drop true true | .drop false true => .bespoke
  | .follow true false => .follow
  | .drop true false => .drop
  | .dedupe true => .dedupe
  | .stayOnKey => .stayOnKey
  | .retainAtSource => .retained
  | .refuseTransfer => .refused
  | .follow false false | .drop false false | .dedupe false => .unreachable

/-- Each live core base relation belongs to exactly one lifecycle partition. -/
def coreRelationClassified : (ownerSurface storeWide : Bool) → Bool
  | true, false | false, true => true
  | true, true | false, false => false

/-- The bulk content inverse begins with selected admissions and checks what survives. -/
inductive ContentCollectionStep where
  | collectContentIds
  | deleteSelectedAdmissions
  | collectContentWithoutSurvivingAdmission
  deriving DecidableEq

/-- Shared content is collected after the selected admissions are gone. -/
def contentCollectionOrder : List ContentCollectionStep :=
  [.collectContentIds, .deleteSelectedAdmissions, .collectContentWithoutSurvivingAdmission]

/-- Stages of applying and accounting for a sealed bulk erase. -/
inductive BulkInverseStep where
  | seedDeclaredCounters
  | applyDeclaredKeyedAndOwnedLegs
  | verifySelectedRowsDeleted
  | readDeclaredCounts
  deriving DecidableEq

/-- Bulk erase uses registry-selected inverse legs and returns a complete tally only after the sealed rows are gone. -/
def bulkInverse : List BulkInverseStep :=
  [.seedDeclaredCounters, .applyDeclaredKeyedAndOwnedLegs, .verifySelectedRowsDeleted, .readDeclaredCounts]

/-- The row accepted from a declared timestamp selection. -/
inductive DeclaredClockCandidate where
  | newestWithTimestamp
  | newestWithoutTimestamp
  | historicalWithTimestamp
  deriving DecidableEq

/-- A declared clock admits only the newest version with a timestamp before the cutoff. -/
def declaredClockCandidate : DeclaredClockCandidate → (beforeCutoff : Bool) → Bool
  | .newestWithTimestamp, true => true
  | .newestWithTimestamp, false | .newestWithoutTimestamp, true | .newestWithoutTimestamp, false
  | .historicalWithTimestamp, true | .historicalWithTimestamp, false => false

end Proxiima.ErasureExport
