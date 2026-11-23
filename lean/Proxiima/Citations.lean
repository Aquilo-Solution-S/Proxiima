import Proxiima.Memory
import Proxiima.ObjectMaintenance

/-! # Citations -/

namespace Proxiima.Citations

/-- Which Memory kinds can carry their one optional cited object. -/
def citationAllowed : Proxiima.Memory.Kind → Bool
  | .Fact | .Abstraction => true
  | .Perspective => false

/-- The two ways a supplied citation resolves an object. -/
inductive CitationSource where
  | reference
  | inlineObject
  deriving DecidableEq

/-- Completeness of the inline schema, version and payload triple. -/
inductive InlineFields where
  | absent
  | incomplete
  | complete
  deriving DecidableEq

/-- Accept an existing object or a complete inline triple, with neither overlap nor incomplete input. -/
def citationInput : (hasReference : Bool) → InlineFields → Option CitationSource
  | true, .absent => some .reference
  | false, .complete => some .inlineObject
  | false, .absent | false, .incomplete | true, .incomplete | true, .complete => none

/-- The two projected fields of a version's citation. -/
structure CitationProjection where
  objectId : String
  schemaId : String

/-- How a referenced object relates to the owner of the writing version. -/
inductive ReferenceResolution where
  | sameOwner
  | otherOwner
  | missing
  deriving DecidableEq

/-- Only the owner's existing cited object satisfies a reference at commit. -/
def referenceAdmitted : ReferenceResolution → Bool
  | .sameOwner => true
  | .otherOwner | .missing => false

/-- The fields that identify an inline cited object. -/
inductive InlineKeyField where
  | owner
  | schema
  | contentHash
  deriving DecidableEq

/-- The owner, schema and content hash deduplicate an inline cited object. -/
def inlineKey : List InlineKeyField := [.owner, .schema, .contentHash]

/-- The descending keyset order of the visible Facts citing an object. -/
inductive CitingFactSortKey where
  | createdAtDescending
  | memoryIdDescending
  deriving DecidableEq

/-- Citing Facts page strictly beyond the last row in this order. -/
def citingFactOrder : List CitingFactSortKey := [.createdAtDescending, .memoryIdDescending]

/-- Storage status of a transfer row. -/
inductive UploadStatus where
  | pending
  | completed
  | aborted
  | expired
  deriving DecidableEq

/-- Whether the content-identity lookup found an upload and its current status. -/
inductive UploadLookup where
  | found (status : UploadStatus)
  | missing
  deriving DecidableEq

/-- Outcomes of looking up an upload by the cited content identity. -/
inductive UploadCitationOutcome where
  | supported
  | terminalConflict
  | missing
  deriving DecidableEq

/-- A live match supports citation, a terminal match conflicts, and an absent match is missing. -/
def uploadSupportsCitation : UploadLookup → UploadCitationOutcome
  | .found .pending | .found .completed => .supported
  | .found .aborted | .found .expired => .terminalConflict
  | .missing => .missing

/-- The upload Fact's receipt payload key, within its owner and schema scope. -/
def uploadReceiptField : String := "content_hash"

/-- What a transfer finds in the completed source upload set. -/
inductive PublicationSet where
  | missing
  | allExact
  | containsInvalid
  deriving DecidableEq

/-- Transfer requires at least one completed upload and refuses any mixed invalid set. -/
def transferablePublication : PublicationSet → Bool
  | .allExact => true
  | .missing | .containsInvalid => false

/-- The metadata fixed for a prepared upload. -/
inductive ExpectationField where
  | contentHash
  | byteLength
  | mediaType
  | filename
  deriving DecidableEq

/-- Completion compares these fields to the staged metadata in order before admitting the Fact. -/
def expectationOrder : List ExpectationField := [.contentHash, .byteLength, .mediaType, .filename]

/-- Largest prepared filename in bytes. -/
def maxFilenameBytes : Nat := 255

/-- Largest prepared media type in bytes. -/
def maxMediaTypeBytes : Nat := 127

/-- Largest initial reservation for a retained upload body in bytes. -/
def maxInitialStageReserve : Nat := 8388608

/-- Largest digest batch for a held-object lookup. -/
def maxHeldDigests : Nat := 1000

/-- Prefix of the canonical key derived from a server-issued upload ID. -/
def canonicalPrefix : String := "objects/"

/-- Prefix of the transfer key a prepared upload signs. -/
def transferPrefix : String := "pending/"

/-- Steps of upload publication from signing to transfer-copy retirement. -/
inductive UploadStep where
  | prepareSignedLength
  | stageVerifiedCanonicalBytes
  | commitCorpusAndUpload
  | attemptPendingCleanup
  deriving DecidableEq

/-- Publication stages verified bytes before the atomic corpus commit and treats cleanup as best effort. -/
def uploadPublicationOrder : List UploadStep :=
  [.prepareSignedLength, .stageVerifiedCanonicalBytes, .commitCorpusAndUpload, .attemptPendingCleanup]

/-- Staging uses the recorded canonical key when present and otherwise uses the transfer key. -/
def stageSource : List Proxiima.ObjectMaintenance.UploadKey := [.canonical, .transfer]

/-- Row count returned by a write guarded by exact upload identity. -/
inductive ChangedRows where
  | none
  | one
  | many
  deriving DecidableEq

/-- Outcomes of classifying a guarded abort. -/
inductive AbortOutcome where
  | wonPending
  | completed
  | alreadyAborted
  | alreadyExpired
  | invalid
  deriving DecidableEq

/-- Abort preserves completed bytes and classifies a missed update against the locked status. -/
def abortOutcome : UploadStatus → ChangedRows → AbortOutcome
  | .pending, .one | .completed, .one | .aborted, .one | .expired, .one => .wonPending
  | .completed, .none => .completed
  | .aborted, .none => .alreadyAborted
  | .expired, .none => .alreadyExpired
  | .pending, .none | .pending, .many | .completed, .many | .aborted, .many
  | .expired, .many => .invalid

/-- Outcomes of classifying a guarded completion. -/
inductive CompletionOutcome where
  | wonPending
  | sameObjectReplay
  | aborted
  | expired
  | invalid
  deriving DecidableEq

/-- Completion replays the same finished object and refuses terminal failure or a contradictory write. -/
def completionOutcome : UploadStatus → ChangedRows → (sameObject : Bool) → CompletionOutcome
  | .pending, .one, true | .pending, .one, false => .wonPending
  | .completed, .none, true => .sameObjectReplay
  | .aborted, .none, true | .aborted, .none, false => .aborted
  | .expired, .none, true | .expired, .none, false => .expired
  | .completed, .none, false | .pending, .none, true | .pending, .none, false
  | .completed, .one, true | .completed, .one, false | .aborted, .one, true | .aborted, .one, false
  | .expired, .one, true | .expired, .one, false | .pending, .many, true | .pending, .many, false
  | .completed, .many, true | .completed, .many, false | .aborted, .many, true | .aborted, .many, false
  | .expired, .many, true | .expired, .many, false => .invalid

/-- Outcomes of classifying a guarded canonical locator write. -/
inductive StageOutcome where
  | staged
  | replay
  | repairAborted
  | repairExpired
  | invalid
  deriving DecidableEq

/-- A lost stage replays a completed upload with an object or repairs the terminal canonical copy. -/
def stageOutcome : UploadStatus → ChangedRows → (hasObject : Bool) → StageOutcome
  | .pending, .one, true | .pending, .one, false => .staged
  | .completed, .none, true => .replay
  | .aborted, .none, true | .aborted, .none, false => .repairAborted
  | .expired, .none, true | .expired, .none, false => .repairExpired
  | .completed, .none, false | .pending, .none, true | .pending, .none, false
  | .completed, .one, true | .completed, .one, false | .aborted, .one, true | .aborted, .one, false
  | .expired, .one, true | .expired, .one, false | .pending, .many, true | .pending, .many, false
  | .completed, .many, true | .completed, .many, false | .aborted, .many, true | .aborted, .many, false
  | .expired, .many, true | .expired, .many, false => .invalid

/-- Locks acquired to settle an upload. -/
inductive UploadLock where
  | owner
  | canonicalKey
  | uploadRow
  deriving DecidableEq

/-- Upload settlements take the owner fence, canonical key fence and any needed upload row lock in that order. -/
def settlementLockOrder : List UploadLock := [.owner, .canonicalKey, .uploadRow]

/-- Delivery binds exactly one registered read operation to its runtime. -/
inductive BindingOutcome where
  | runtimeMismatch
  | operationNotRegisteredRead
  | bound
  deriving DecidableEq

/-- A foreign runtime is refused first; the bound operation must be an exact registered read. -/
def bindDelivery : (runtimeMatches registeredRead : Bool) → BindingOutcome
  | false, false | false, true => .runtimeMismatch
  | true, false => .operationNotRegisteredRead
  | true, true => .bound

/-- The common authority routes of verified collection and host delivery. -/
inductive ReadRoute where
  | owner
  | goalPublication
  deriving DecidableEq

/-- The selected upload identity for one authorized read. -/
inductive ReadSelection where
  | latestCompleted
  | exactCaptured
  deriving DecidableEq

/-- Owner reads choose the latest completed upload; Goal reads use the captured upload. -/
def selectUpload : ReadRoute → UploadStatus → Option ReadSelection
  | .owner, .completed => some .latestCompleted
  | .goalPublication, .completed => some .exactCaptured
  | .owner, .pending | .owner, .aborted | .owner, .expired
  | .goalPublication, .pending | .goalPublication, .aborted | .goalPublication, .expired => none

/-- Ordering keys for completed uploads on the owner route. -/
inductive CompletedUploadSortKey where
  | completionDescendingNullsLast
  | uploadIdDescending
  deriving DecidableEq

/-- Ordinary reads break completion-time ties by descending upload identity. -/
def completedUploadOrder : List CompletedUploadSortKey :=
  [.completionDescendingNullsLast, .uploadIdDescending]

/-- The integrity checks for stored read and staged metadata. -/
inductive MetadataCheck where
  | nonnegativeLength
  | fullDigests
  | storedTimestampWhenRequired
  deriving DecidableEq

/-- Reads require stored length, full digests and the timestamp required by the row's state. -/
def storedMetadataChecks : List MetadataCheck :=
  [.nonnegativeLength, .fullDigests, .storedTimestampWhenRequired]

/-- Byte disclosure steps for an in-process read. -/
inductive VerifiedReadStep where
  | authorizeAndResolveExactRecord
  | checkDeclaredLengthAgainstCeiling
  | enforceStreamingBound
  | verifyLengthAndDigests
  | releaseBytes
  deriving DecidableEq

/-- Verified reads admit before provider access and release only complete, digest-verified bytes. -/
def verifiedReadOrder : List VerifiedReadStep :=
  [.authorizeAndResolveExactRecord, .checkDeclaredLengthAgainstCeiling, .enforceStreamingBound,
   .verifyLengthAndDigests, .releaseBytes]

/-- Exact locator checks required before honoring a stored record. -/
inductive LocatorField where
  | store
  | bucket
  | canonicalMintingKey
  deriving DecidableEq

/-- Require the configured store and bucket and the canonical key of the upload that minted the bytes. -/
def locatorFields : List LocatorField := [.store, .bucket, .canonicalMintingKey]

end Proxiima.Citations
