import Proxiima.Migrations

/-! # Embeddings -/

namespace Proxiima.Embeddings

/-- Request timeout in seconds when the host leaves it unset. -/
def requestSeconds : Nat := 120
/-- Longest request timeout in seconds. -/
def requestSecondsMax : Nat := 3600
/-- Texts per provider batch when the host leaves it unset. -/
def batchSize : Nat := 32
/-- Maximum texts per provider batch. -/
def batchSizeMax : Nat := 1024
/-- Concurrent provider batches when the host leaves it unset. -/
def drainConcurrency : Nat := 1
/-- Maximum concurrent provider batches in one drain. -/
def drainConcurrencyMax : Nat := 64
/-- First idle poll interval in seconds when the host leaves it unset. -/
def idleSeconds : Nat := 5
/-- Largest configured first idle poll interval in seconds. -/
def idleSecondsMax : Nat := 3600
/-- Idle poll backoff ceiling in seconds. -/
def idleBackoffSeconds : Nat := 60
/-- Stale claim timeout in seconds when the host leaves it unset. -/
def staleClaimSeconds : Nat := 900
/-- Largest stale claim timeout in seconds. -/
def staleClaimSecondsMax : Nat := 86400
/-- Divide the stale claim interval by this number for heartbeat and reclaim throttling. -/
def heartbeatDivisor : Nat := 3
/-- Largest number of memory IDs in a diagnostics request. -/
def diagnosticsMax : Nat := 256
/-- Default candidate budget for startup and engine reconciliation. -/
def reconcileDefaultLimit : Nat := 50000
/-- Page cardinality and downstream probes per head relation in one reconcile page. -/
def reconcilePageSize : Nat := 2000
/-- Number of owners routed per reconcile owner page. -/
def reconcileOwnerPageSize : Nat := 1000
/-- Minimum client character cap for chunk rescue. -/
def inputCapMin : Nat := 4095
/-- Character cap for the canonical host when the input ceiling is unset. -/
def inputCapDefault : Nat := 16384
/-- The vector width when the canonical host leaves it unset. -/
def defaultWidth : Nat := 1024
/-- Nested-prefix requests are off by default in the canonical host. -/
def defaultMatryoshka : Bool := false
/-- Default embedding slot. -/
def defaultSlot : String := "default"
/-- Maximum sampled chunks in an owner and embedding-space canary stratum. -/
def recallSampleCap : Nat := 32
/-- Neighbors compared by the recall canary. -/
def recallNeighbors : Nat := 10
/-- Maximum prefix bytes collected for a provider error diagnostic. -/
def errorBodyPrefixBytes : Nat := 512

/-- Minimum pgvector version accepted by the PostgreSQL vector lane. -/
def pgvectorMinimum : List Nat := [0, 8, 0]

/-- The widths the store indexes. -/
inductive Width where
  | d384 | d768 | d1024 | d1536 | d2048 | d3072
  deriving DecidableEq

/-- Component count for the 384-wide lane. -/
def width384 : Nat := 384
/-- Component count for the 768-wide lane. -/
def width768 : Nat := 768
/-- Component count for the 1024-wide lane. -/
def width1024 : Nat := 1024
/-- Component count for the 1536-wide lane. -/
def width1536 : Nat := 1536
/-- Component count for the 2048-wide lane. -/
def width2048 : Nat := 2048
/-- Component count for the 3072-wide lane. -/
def width3072 : Nat := 3072

/-- Widths by component count, in ascending order. -/
def indexedWidths : List Nat := [width384, width768, width1024, width1536, width2048, width3072]

/-- Types of the HNSW expression over the stored vector. -/
inductive IndexEncoding where
  | vector
  | halfvec
  deriving DecidableEq

/-- Wider lanes use half precision for the index, keeping full precision in storage. -/
def indexEncoding : Width → IndexEncoding
  | .d384 | .d768 | .d1024 | .d1536 => .vector
  | .d2048 | .d3072 => .halfvec

/-- Parts of the selected vector storage lane. -/
inductive VectorStoragePart where
  | heads | pgvector | localVectors
  deriving DecidableEq

/-- Both lanes keep heads, and the PostgreSQL lane also installs pgvector and stores vectors locally. -/
def vectorStorageParts : Proxiima.Migrations.VectorLane → List VectorStoragePart
  | .local => [.heads, .pgvector, .localVectors]
  | .hostIndex => [.heads]

/-- The placeholder required and substituted by a query instruction. -/
def queryPlaceholder : String := "{query}"
/-- The environment sequence expanded to a newline in the query template. -/
def queryNewlineEscape : String := "\\n"

/-- Instruction source for a search query. -/
inductive QueryWording where
  | named | default | original
  deriving DecidableEq

/-- A search takes its named instruction, then the default, then the original query. -/
def queryWording : (named : Bool) → (hasDefault : Bool) → QueryWording
  | true, true | true, false => .named
  | false, true => .default
  | false, false => .original

/-- An owner route has zero, one or two bindings with different spaces. -/
inductive Route where
  | absent | current | nextOnly | moving
  deriving DecidableEq

/-- A position in the owner route. -/
inductive RoutePosition where
  | current | next
  deriving DecidableEq

/-- Queue each admission in current-then-next order where those spaces exist. -/
def writeSpaces : Route → List RoutePosition
  | .absent => []
  | .current => [.current]
  | .nextOnly => [.next]
  | .moving => [.current, .next]

/-- Search uses the current owner binding while the next space fills. -/
def searchSpace : Route → Option RoutePosition
  | .absent | .nextOnly => none
  | .current | .moving => some .current

/-- Entry points that resolve the data owner's embedding route. -/
inductive RouteUse where
  | write | drain | search | reconcile | purge
  deriving DecidableEq

/-- What the engine does when the owner route lookup refuses. -/
inductive RouteFailure where
  | refuse | release | lexical | skip
  deriving DecidableEq

/-- Route failures refuse writes and purge, yield lexical search, and release or skip maintenance work. -/
def routeFailure : RouteUse → RouteFailure
  | .write | .purge => .refuse
  | .drain => .release
  | .search => .lexical
  | .reconcile => .skip

/-- Embedding configuration outcomes for the canonical host. -/
inductive ProviderConfig where
  | disabled | enabled | refuse
  deriving DecidableEq

/-- Both endpoint and model enable embeddings, and a partial configuration refuses startup. -/
def providerConfig : (endpoint : Bool) → (model : Bool) → (overrideSet : Bool) → ProviderConfig
  | true, true, true | true, true, false => .enabled
  | false, false, false => .disabled
  | false, false, true | true, false, true | true, false, false
  | false, true, true | false, true, false => .refuse

/-- The phases of admitting ordinary embedding work. -/
inductive AdmissionPhase where
  | resolveRoute | enqueueWithWrite | commit | drain
  deriving DecidableEq

/-- Admission queues routed work in its transaction, and provider work follows commit. -/
def admissionOrder : List AdmissionPhase := [.resolveRoute, .enqueueWithWrite, .commit, .drain]

/-- The closed embedding eligibility state of a schema ID. -/
inductive SchemaEligibility where
  | unknown
  | everyLayerDeclines
  | aLayerEmbeds
  deriving DecidableEq

/-- An ID is excluded only when every registered layer declines embeddings. -/
def schemaEmbeddable : SchemaEligibility → Bool
  | .unknown | .aLayerEmbeds => true
  | .everyLayerDeclines => false

/-- Kinds of calls to the embedding provider. -/
inductive ProviderCall where
  | indexing
  | query
  deriving DecidableEq

/-- The client gate state observed at dispatch. -/
inductive ClientState where
  | ready
  | backingOff
  | paused
  deriving DecidableEq

/-- Queries bypass transient indexing backoff, while configuration pauses block both call kinds. -/
def admitCall : ClientState → ProviderCall → Bool
  | .ready, .indexing | .ready, .query | .backingOff, .query => true
  | .backingOff, .indexing | .paused, .indexing | .paused, .query => false

/-- The final provider outcome that the drain may receive. -/
inductive ProviderResult where
  | success
  | inputRefused
  | transient
  | configuration
  deriving DecidableEq

/-- Actions following a provider result at the drain. -/
inductive DrainAction where
  | publish
  | rescue
  | release
  deriving DecidableEq

/-- Publish successful vectors, rescue input refusals, and release on other failures. -/
def drainAction : ProviderResult → DrainAction
  | .success => .publish
  | .inputRefused => .rescue
  | .transient | .configuration => .release

/-- Validation result from a managed client response. -/
inductive VectorValidation where
  | accepted | widthFault | invalidComponent
  deriving DecidableEq

/-- A wrong width pauses the managed binding, and nonfinite components at the expected width are retryable. -/
def validateVector : (widthMatches : Bool) → (finite : Bool) → VectorValidation
  | false, false | false, true => .widthFault
  | true, false => .invalidComponent
  | true, true => .accepted

/-- Outcomes of fenced release after an aborted drain. -/
inductive ClaimRelease where
  | settled
  | alreadyFencedOut
  | storageFailure
  deriving DecidableEq

/-- How outstanding claims cease to belong to a returned drain. -/
inductive ClaimRecovery where
  | settled
  | leaseExpiry
  deriving DecidableEq

/-- An aborted drain attempts fenced release and falls back to lease expiry if release fails. -/
def claimRecovery : ClaimRelease → ClaimRecovery
  | .settled | .alreadyFencedOut => .settled
  | .storageFailure => .leaseExpiry

/-- Stored states of a durable job. -/
inductive JobState where
  | pending
  | processing
  | failed
  | failedPermanent
  deriving DecidableEq

/-- The routing or lock state of a candidate in the claim queue. -/
inductive ClaimAvailability where
  | available | locked | ownerExcluded | spaceBlocked
  deriving DecidableEq

/-- Claim only pending rows whose locks and owner-space bindings permit work. -/
def claimEligible : JobState → ClaimAvailability → Bool
  | .pending, .available => true
  | .pending, .locked | .pending, .ownerExcluded | .pending, .spaceBlocked
  | .processing, .available | .processing, .locked | .processing, .ownerExcluded | .processing, .spaceBlocked
  | .failed, .available | .failed, .locked | .failed, .ownerExcluded | .failed, .spaceBlocked
  | .failedPermanent, .available | .failedPermanent, .locked
  | .failedPermanent, .ownerExcluded | .failedPermanent, .spaceBlocked => false

/-- The two status groups in an owner's backlog read. -/
inductive BacklogGroup where
  | queued
  | failed
  deriving DecidableEq

/-- The backlog groups pending and processing, and both kinds of failure. -/
def backlogGroup : JobState → BacklogGroup
  | .pending | .processing => .queued
  | .failed | .failedPermanent => .failed

/-- Only processing diagnostics expose claim age. -/
def diagnosticClaimAge : JobState → Bool
  | .processing => true
  | .pending | .failed | .failedPermanent => false

/-- Events on a token-fenced processing claim. -/
inductive ClaimEvent where
  | renew | finish | retryableFailure | permanentFailure | release | expire
  deriving DecidableEq

/-- When its fence matches, a claim can renew, disappear, fail or return to pending. -/
def settleClaim : ClaimEvent → Option JobState
  | .renew => some .processing
  | .finish => none
  | .retryableFailure => some .failed
  | .permanentFailure => some .failedPermanent
  | .release | .expire => some .pending

/-- Whether a reconcile target already has a job. -/
inductive ReconcileJob where
  | absent | pending | processing | failed | failedPermanent
  deriving DecidableEq

/-- Missing coverage can open a job or reopen a retryable failure. -/
def reconcileJob : ReconcileJob → Bool
  | .absent | .failed => true
  | .pending | .processing | .failedPermanent => false

/-- Metadata examined by the missing-embedding read and reconcile before text resolution. -/
inductive CandidateMetadata where
  | liveHead | allowedSchema | requestedSpace
  deriving DecidableEq

/-- Metadata used to select candidate Memory heads. -/
def candidateMetadata : List CandidateMetadata := [.liveHead, .allowedSchema, .requestedSpace]

/-- The durable write steps for a completed embedding version. -/
inductive ChunkWrite where
  | settleFencedClaim
  | lockHead
  | appendOrderedChunks
  | advanceHead
  deriving DecidableEq

/-- Publish ordered chunks and advance the head in the same transaction as fenced completion. -/
def publishChunks : List ChunkWrite := [.settleFencedClaim, .lockHead, .appendOrderedChunks, .advanceHead]

/-- The order of host-index publication for an embedding key. -/
inductive HostWrite where
  | settleDebt
  | pushVersion
  | commit
  deriving DecidableEq

/-- Old key debt is settled before the new version is pushed and accepted before completion. -/
def hostPublication : List HostWrite := [.settleDebt, .pushVersion, .commit]

/-- Stages of repairing a host-index key during reconciliation. -/
inductive IndexRepair where
  | comparePages | lockKey | compareCurrentHead | repair
  deriving DecidableEq

/-- A page difference authorizes repair only after the key lock and a fresh head comparison. -/
def indexRepair : List IndexRepair := [.comparePages, .lockKey, .compareCurrentHead, .repair]

/-- The presence of a stored space on the current owner route. -/
inductive SpaceRole where
  | current | next | unrouted
  deriving DecidableEq

/-- Purge removes only unrouted residue and leaves processing jobs to settle. -/
def purgeJob : SpaceRole → JobState → Bool
  | .unrouted, .pending | .unrouted, .failed | .unrouted, .failedPermanent => true
  | .unrouted, .processing
  | .current, .pending | .current, .processing | .current, .failed | .current, .failedPermanent
  | .next, .pending | .next, .processing | .next, .failed | .next, .failedPermanent => false

/-- Transfer drops non-destination coverage before queueing for the destination live head. -/
inductive TransferStep where
  | dropOutsideDestination
  | queueDestinationHead
  deriving DecidableEq

/-- The in-transaction steps that move embedding coverage with a series. -/
def transferCoverage : List TransferStep := [.dropOutsideDestination, .queueDestinationHead]

/-- The two methods compared by the owner-and-space stratified recall canary. -/
inductive RecallLeg where
  | exact
  | approximate
  deriving DecidableEq

/-- Compare exact and approximate chunk neighbors. -/
def recallComparison : List RecallLeg := [.exact, .approximate]

/-- Startup passes that precede the idle/poll cycle. -/
inductive WorkerPass where
  | reconcile
  | gatedDrain
  deriving DecidableEq

/-- Startup attempts reconcile once before the first gated claim. -/
def workerStart : List WorkerPass := [.reconcile, .gatedDrain]

/-- The provider responses that distinguish input and configuration failures. -/
inductive FailureStatus where
  | http400 | http401 | http403 | http404 | http413 | http422 | other
  deriving DecidableEq

/-- Classify the HTTP status independently of its body. -/
def providerFailure : FailureStatus → ProviderResult
  | .http400 | .http401 | .http403 | .http404 => .configuration
  | .http413 | .http422 => .inputRefused
  | .other => .transient

end Proxiima.Embeddings
