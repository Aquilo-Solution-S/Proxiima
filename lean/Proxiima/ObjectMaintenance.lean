/-! # ObjectMaintenance -/

namespace Proxiima.ObjectMaintenance

/-- Maximum bytes in a stored object-store identity. -/
def storeIdMaxBytes : Nat := 512

/-- Maximum ASCII bytes in an operator-selected object-store name. -/
def operatorIdMaxBytes : Nat := 64

/-- The persisted sentinel for the in-process purge store. -/
def memoryStoreId : String := "memory:"

/-- Maximum UTF-8 bytes in a stored purge diagnostic. -/
def purgeErrorMaxBytes : Nat := 1024

/-- Maximum stuck debts included in a maintenance sample. -/
def stuckSampleMax : Nat := 32

/-- Maximum foreign stores included in a maintenance sample. -/
def foreignStoreSampleMax : Nat := 8

/-- Default maximum due debts selected by one drain batch. -/
def purgeBatchDefault : Nat := 100

/-- Whether a default drain is a preview. -/
def purgeDryRunDefault : Bool := false

/-- Default delay in seconds after the first failed delete. -/
def purgeBackoffBaseSeconds : Nat := 30

/-- Default ceiling in seconds on a failed delete's retry delay. -/
def purgeBackoffMaxSeconds : Nat := 21600

/-- Default failure count at which a debt is reported as stuck. -/
def purgeStuckAfter : Nat := 8

/-- Maximum exponent applied to the initial delay. -/
def purgeBackoffExponentMax : Nat := 40

/-- Default worker cadence in seconds when an object store is configured. -/
def purgeIntervalSeconds : Nat := 60

/-- Interval in seconds that disables the worker. -/
def purgeDisabledSeconds : Nat := 0

/-- Expired uploads one runtime sweep batch can decide. -/
def runtimeSweepBatch : Nat := 500

/-- Maximum sweep batches in one runtime maintenance pass. -/
def runtimeSweepBatches : Nat := 20

/-- Maximum drain batches in one runtime maintenance pass. -/
def runtimePurgeBatches : Nat := 100

/-- Default batch size for the one-shot storage command. -/
def cliBatchDefault : Nat := 1000

/-- Maximum concurrent stages for one store instance and its clones. -/
def stagesPerStore : Nat := 8

/-- Default S3 path style setting. -/
def forcePathStyleDefault : Bool := false

/-- Default upload URL lifetime in seconds. -/
def uploadTtlDefaultSeconds : Nat := 900

/-- Maximum upload URL lifetime in seconds supported by signing. -/
def uploadTtlMaxSeconds : Nat := 604800

/-- Default ceiling in bytes for a cited object. -/
def blobMaxBytesDefault : Nat := 104857600

/-- Maximum examples per reconciliation divergence class. -/
def reconcileSampleMax : Nat := 100

/-- Maximum reconciliation rows fetched per page. -/
def reconcileRowPage : Nat := 1000

/-- Steps of a leased object-maintenance pass. -/
inductive PassStep where
  | sweep
  | drain
  deriving DecidableEq

/-- When a pass does both, expired uploads are swept before due debt is drained. -/
def maintenanceOrder : List PassStep := [.sweep, .drain]

/-- Work selected by the shared storage maintenance lease. -/
inductive LeasedWork where
  | runPass
  | skipPass
  deriving DecidableEq

/-- A holder runs the pass and a contender skips both jobs. -/
def maintenanceLease : (acquired : Bool) → LeasedWork
  | true => .runPass
  | false => .skipPass

/-- Which maintenance command action flags were selected. -/
inductive StorageActions where
  | none
  | retry
  | sweep
  | both
  deriving DecidableEq

/-- Why the selected action flags are refused. -/
inductive ActionRefusal where
  | missingAction
  | sweepDryRun
  deriving DecidableEq

/-- The command requires an action and forbids a dry run of the expiry sweep. -/
def storageActions : StorageActions → (dryRun : Bool) → Except ActionRefusal StorageActions
  | .none, false | .none, true => .error .missingAction
  | .retry, false | .retry, true => .ok .retry
  | .sweep, false => .ok .sweep
  | .both, false => .ok .both
  | .sweep, true | .both, true => .error .sweepDryRun

/-- The state of the configured cadence when S3 is wired. -/
inductive UploadCadence where
  | disabled
  | withinTtl
  | exceedsTtl
  deriving DecidableEq

/-- A configured worker interval at or below the upload TTL is admissible, as is a disabled worker. -/
def cadenceAdmitted : UploadCadence → Bool
  | .disabled | .withinTtl => true
  | .exceedsTtl => false

/-- The ordered boundaries of object debt discharge. -/
inductive PurgeStep where
  | persistUnclaimedDebt
  | commit
  | readCurrentVersion
  | deleteExactKey
  | settleObservedVersion
  deriving DecidableEq

/-- Liveness and debt commit together before provider I/O, with settlement limited to the observed debt version. -/
def purgeOrder : List PurgeStep :=
  [.persistUnclaimedDebt, .commit, .readCurrentVersion, .deleteExactKey, .settleObservedVersion]

/-- The outcome of deleting the exact key and its versions. -/
inductive ProviderResult where
  | removed
  | absent
  | failed
  deriving DecidableEq

/-- Whether to retire the observed debt or schedule its retry. -/
inductive Settlement where
  | clear
  | backOff
  deriving DecidableEq

/-- Absence discharges debt like deletion, and failure retains it with backoff. -/
def settlePurge : ProviderResult → Settlement
  | .removed | .absent => .clear
  | .failed => .backOff

/-- Which provider keys an unstaged upload can imply. -/
inductive UploadKey where
  | transfer
  | canonical
  deriving DecidableEq

/-- Abort and expiry owe both implied keys in the status transaction. -/
def terminalUploadDebt : List UploadKey := [.transfer, .canonical]

/-- The phases of checking and deleting a provider page. -/
inductive VersionDeleteStep where
  | checkCursor
  | collectExactVersionIds
  | deleteVersions
  | checkAcknowledgements
  deriving DecidableEq

/-- Each version page must advance, target the exact key and acknowledge every requested version and delete marker. -/
def exactVersionDeletion : List VersionDeleteStep :=
  [.checkCursor, .collectExactVersionIds, .deleteVersions, .checkAcknowledgements]

/-- Whether a listed key is owed, unclaimed or still claimed by a live upload. -/
inductive CanonicalClaim where
  | existingDebt
  | endedOnly
  | unclaimed
  | live
  deriving DecidableEq

/-- How reconciliation classifies a listed canonical key. -/
inductive CanonicalFinding where
  | owed
  | recordDebt
  | orphan
  | retained
  deriving DecidableEq

/-- Only a key implied solely by an ended upload acquires new debt, after a fenced recheck. -/
def reconcileCanonical : CanonicalClaim → CanonicalFinding
  | .existingDebt => .owed
  | .endedOnly => .recordDebt
  | .unclaimed => .orphan
  | .live => .retained

/-- Whether an endpoint was provided by the host. -/
inductive EndpointChoice where
  | configured
  | defaultResolver
  deriving DecidableEq

/-- Object I/O and upload signing choose the configured endpoint or the AWS resolver. -/
def endpointChoice : (configured : Bool) → EndpointChoice
  | true => .configured
  | false => .defaultResolver

/-- Outcomes of comparing stored debt with the wired store. -/
inductive DebtDisposition where
  | attempt
  | retain
  deriving DecidableEq

/-- Provider deletion is authorized only by debt for the exact wired store. -/
def debtStoreMatch : (sameStore : Bool) → DebtDisposition
  | true => .attempt
  | false => .retain

/-- Steps taken by a leased debt drain. -/
inductive DrainStep where
  | renewLease
  | selectDueForStore
  | tryBatch
  | reportCounts
  deriving DecidableEq

/-- Batches renew the lease before trying due rows of the wired store in due-time order. -/
def drainOrder : List DrainStep := [.renewLease, .selectDueForStore, .tryBatch, .reportCounts]

/-- Whether an upload row claims a listed object of each key class. -/
inductive ClaimPhase where
  | inFlight
  | expiredPending
  | completed
  | ended
  deriving DecidableEq

/-- Pending and completed rows claim canonical objects, and only in-flight rows claim transfer objects. -/
def claimsKey : ClaimPhase → UploadKey → Bool
  | .inFlight, .canonical | .inFlight, .transfer
  | .expiredPending, .canonical | .completed, .canonical => true
  | .expiredPending, .transfer | .completed, .transfer
  | .ended, .canonical | .ended, .transfer => false

/-- Reconciliation source visibility. -/
inductive ReconcileScope where
  | global
  | owner
  deriving DecidableEq

/-- Which observations can be returned from the chosen reconciliation scope. -/
inductive ReconcileObservations where
  | bucketAndRows
  | ownLiveRows
  deriving DecidableEq

/-- Boot authority permits bucket-wide findings and locator samples, while Fact-read authority sees its owner's live upload findings without coordinates. -/
def reconcileVisibility : ReconcileScope → ReconcileObservations
  | .global => .bucketAndRows
  | .owner => .ownLiveRows

/-- The outcome of consuming the S3 settings block. -/
inductive S3Admission where
  | disabled
  | configured
  | refuse
  deriving DecidableEq

/-- Plaintext host policy is parsed even without a bucket, which otherwise opts in and requires its companion settings. -/
def optionalS3 : (hostPolicyValid : Bool) → (bucketSet : Bool) → (companionsValid : Bool) → S3Admission
  | false, false, false | false, false, true
  | false, true, false | false, true, true | true, true, false => .refuse
  | true, false, false | true, false, true => .disabled
  | true, true, true => .configured

/-- The outcome of rechecking claims after a stage loses the locator write. -/
inductive LostStageDisposition where
  | preserveBytes
  | recordDebt
  deriving DecidableEq

/-- When the owner row disappears during a stage, a surviving reference preserves the canonical bytes. -/
def lostStageClaims : (stillReferenced : Bool) → LostStageDisposition
  | true => .preserveBytes
  | false => .recordDebt

/-- The finding made when comparing the ordinary read winner with this store. -/
inductive LocatorFinding where
  | foreign
  | present
  | missing
  deriving DecidableEq

/-- A foreign locator is counted separately from a missing local object. -/
def reconcileLocator : (localLocator : Bool) → (present : Bool) → LocatorFinding
  | false, false | false, true => .foreign
  | true, true => .present
  | true, false => .missing

/-- Whether boot and the runtime pass can proceed with the current purge census. -/
inductive CensusAdmission where
  | proceed
  | refuse
  deriving DecidableEq

/-- A queue containing any foreign debt refuses object-store boot or an in-process pass. -/
def foreignDebt : (hasForeignDebt : Bool) → CensusAdmission
  | false => .proceed
  | true => .refuse

/-- The state of one upload at the expiry decision. -/
inductive ExpiryState where
  | pendingUnstaged
  | pendingStaged
  | terminal
  deriving DecidableEq

/-- Only an unstaged pending upload past its TTL may be expired by the sweep. -/
def mayExpire : ExpiryState → (pastTtl : Bool) → Bool
  | .pendingUnstaged, true => true
  | .pendingUnstaged, false | .pendingStaged, false | .pendingStaged, true
  | .terminal, false | .terminal, true => false

/-- How to render a provider diagnostic character. -/
inductive DiagnosticCharacter where
  | preserve
  | space
  deriving DecidableEq

/-- Purge diagnostics preserve printable content and replace control characters with spaces. -/
def purgeDiagnosticCharacter : (control : Bool) → DiagnosticCharacter
  | true => .space
  | false => .preserve

/-- Transfer's disposition of the cited object's metadata. -/
inductive CitedObjectTransfer where
  | reuseDestination
  | moveRows
  | mountSource
  deriving DecidableEq

/-- Reuse identical destination metadata first, move unshared rows in place, or mount shared bytes with independent metadata. -/
def citedObjectTransfer : (destinationHasBytes : Bool) → (shared : Bool) → CitedObjectTransfer
  | true, false | true, true => .reuseDestination
  | false, false => .moveRows
  | false, true => .mountSource

/-- The boundary at which the purge worker checks for quiesce. -/
inductive WorkerStep where
  | checkpoint
  | pass
  | wait
  deriving DecidableEq

/-- The worker starts with a checkpoint and pass, then waits between passes. -/
def workerCycle : List WorkerStep := [.checkpoint, .pass, .wait]

/-- Inputs that determine whether a stuck-debt warning is emitted. -/
def warnStuck : (changed : Bool) → (nonzero : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- Classification of a stored object identity at the read boundary. -/
inductive PersistedStoreForm where
  | knownCanonical
  | knownMalformed
  | externalScheme
  | malformedEnvelope
  deriving DecidableEq

/-- Built-in scheme bodies must reconstruct canonically and external schemes retain their bounded syntax. -/
def persistedStoreAdmitted : PersistedStoreForm → Bool
  | .knownCanonical | .externalScheme => true
  | .knownMalformed | .malformedEnvelope => false

/-- Deterministic ordering keys for samples of missing live uploads. -/
inductive MissingSampleSortKey where
  | completionNewestFirst
  | uploadIdDescending
  deriving DecidableEq

/-- Missing samples are newest first, breaking completion ties by descending upload identity. -/
def missingSampleOrder : List MissingSampleSortKey := [.completionNewestFirst, .uploadIdDescending]

/-- Text substituted for the private S3 secret in debug output. -/
def secretDebugMarker : String := "[REDACTED]"

/-- Transport and host classes distinguished by S3 endpoint validation. -/
inductive EndpointTransport where
  | https
  | loopbackHttp
  | admittedHttp
  | otherHttp
  | invalidOrUnsupported
  deriving DecidableEq

/-- Allow HTTPS and plaintext HTTP to a loopback or explicitly named host. -/
def endpointAdmitted : EndpointTransport → Bool
  | .https | .loopbackHttp | .admittedHttp => true
  | .otherHttp | .invalidOrUnsupported => false

/-- Completion of an operator maintenance command. -/
inductive CommandCompletion where
  | completed
  | leaseContended
  | failed
  deriving DecidableEq

/-- Findings on a completed pass and contention for a lease preserve a successful exit. -/
def commandSucceeded : CommandCompletion → Bool
  | .completed | .leaseContended => true
  | .failed => false

end Proxiima.ObjectMaintenance
