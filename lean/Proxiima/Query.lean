/-! # Query -/

namespace Proxiima.Query

/-- A point read selects the named admission or the newest readable admission of its series. -/
inductive ReadAt where
  | version
  | seriesHead
  deriving DecidableEq

/-- An ordered query can select only live heads or also live superseded admissions. -/
inductive History where
  | headsOnly
  | includeSuperseded
  deriving DecidableEq

/-- Select a live row, and in a head-only window require that it is a visible head. -/
def includesVersion : History → (live : Bool) → (visibleHead : Bool) → Bool
  | .headsOnly, true, true | .includeSuperseded, true, true | .includeSuperseded, true, false => true
  | .headsOnly, true, false | .headsOnly, false, true | .headsOnly, false, false
  | .includeSuperseded, false, true | .includeSuperseded, false, false => false

/-- Check explicit space on either route, and default to the current space on the local route. -/
def validateReadSpace : (localStore : Bool) → (explicitSpace : Bool) → Bool
  | true, true | true, false | false, true => true
  | false, false => false

/-- Which stored owner a Memory projection can name in the current session. -/
inductive ReadOwner where
  | currentLocal
  | otherLocal
  | remote
  | unknown
  deriving DecidableEq

/-- The public labels for a known Memory owner. -/
inductive SpaceLabel where
  | current
  | ownerUuid
  deriving DecidableEq

/-- Label the stored owner with its key, or use null when the owner is unknown. -/
def spaceLabel : ReadOwner → Option SpaceLabel
  | .currentLocal => some .current
  | .otherLocal | .remote => some .ownerUuid
  | .unknown => none

/-- The result of looking up a requested version through its issuing store. -/
inductive RemoteOutcome where
  | found
  | absent
  | erased
  | unknownStore
  | unanswered
  deriving DecidableEq

/-- The three outcomes of a UUID in the wire batch. -/
inductive BatchAnswer where
  | found
  | missing
  | unanswered
  deriving DecidableEq

/-- A batch separates answered absence from an unanswered, retryable read. -/
def batchAnswer : RemoteOutcome → BatchAnswer
  | .found => .found
  | .absent | .erased | .unknownStore => .missing
  | .unanswered => .unanswered

/-- How a remote admission compares with the requested admission. -/
inductive RemoteIdentity where
  | sameVersion
  | otherVersionOfStore
  | otherStore
  deriving DecidableEq

/-- Exact replies must echo the requested admission, and head replies must come from the requested store. -/
def acceptsRemoteVersion : ReadAt → RemoteIdentity → Bool
  | .version, .sameVersion | .seriesHead, .sameVersion | .seriesHead, .otherVersionOfStore => true
  | .version, .otherVersionOfStore | .version, .otherStore | .seriesHead, .otherStore => false

/-- The store route for a query without an explicit store selection. -/
inductive ImplicitRoute where
  | localOnly
  | uniqueRemote
  | allReadable
  deriving DecidableEq

/-- How many readable remote stores declare the selected schema. -/
inductive RemoteSchemaMatches where
  | none
  | one
  | multiple
  deriving DecidableEq

/-- A uniquely declared schema routes to its store; otherwise query all readable stores. -/
def implicitRoute : (localMatches : Bool) → RemoteSchemaMatches → ImplicitRoute
  | true, .none => .localOnly
  | false, .one => .uniqueRemote
  | true, .one | true, .multiple | false, .none | false, .multiple => .allReadable

/-- The default per-store request timeout in seconds. -/
def remoteTimeoutSeconds : Nat := 2

/-- The shared deadline in seconds for an origin batch. -/
def remoteBatchDeadlineSeconds : Nat := 10

/-- The maximum concurrent origin reads per store in a batch. -/
def maxRemoteReadsPerStore : Nat := 8

/-- Reject a remote inventory containing the local number or repeated numbers. -/
def acceptsInventory : (includesLocal : Bool) → (duplicates : Bool) → Bool
  | false, false => true
  | true, true | true, false | false, true => false

/-- The caller's selection when remote inventory fails to answer. -/
inductive StoreSelection where
  | implicit
  | explicitLocal
  | explicitRemote
  deriving DecidableEq

/-- How the query proceeds without a remote inventory answer. -/
inductive InventoryFallback where
  | localWithUnanswered
  | validateLocalSelection
  | retryableRefusal
  deriving DecidableEq

/-- Preserve explicit local selection, flag implicit fallback, and refuse explicit remote selection. -/
def inventoryFallback : StoreSelection → InventoryFallback
  | .implicit => .localWithUnanswered
  | .explicitLocal => .validateLocalSelection
  | .explicitRemote => .retryableRefusal

/-- A continuation reuses the local cursor and therefore only the first page asks remote stores for hits. -/
def asksRemoteStores : (selectedRemote : Bool) → (continuation : Bool) → Bool
  | true, false => true
  | true, true | false, true | false, false => false

/-- A page's ordered keys are compared newest first. -/
inductive DescendingPageKey where
  | seriesHandle
  | createdAt
  | admissionId
  deriving DecidableEq

/-- The Goal keyset puts its stable series handle before the version ID. -/
def goalPageKeys : List DescendingPageKey := [.seriesHandle, .admissionId]

/-- A recency search resumes by creation time and then by admission ID. -/
def searchRecencyKeys : List DescendingPageKey := [.createdAt, .admissionId]

/-- The two wire encodings of a cursor envelope. -/
inductive CursorEncoding where
  | json
  | unpaddedBase64Url
  deriving DecidableEq

/-- Serialize the envelope as JSON and encode its bytes as unpadded base64url. -/
def cursorEncoding : List CursorEncoding := [.json, .unpaddedBase64Url]

/-- Distinguish result shaping, position and presentation when minting a cursor. -/
inductive CursorInput where
  | resultFilter
  | resultOrder
  | resumePosition
  | pageSize
  | presentation
  deriving DecidableEq

/-- Where an input to a paged response belongs in the continuation. -/
inductive CursorBinding where
  | fingerprint
  | keyset
  | excluded
  deriving DecidableEq

/-- Page size and search presentation are excluded, while filters and order bind the keyset. -/
def cursorBinding : CursorInput → CursorBinding
  | .resultFilter | .resultOrder => .fingerprint
  | .resumePosition => .keyset
  | .pageSize | .presentation => .excluded

/-- The fields checked before a cursor exposes its continuation. -/
inductive CursorCheck where
  | formatVersion
  | queryFingerprint
  deriving DecidableEq

/-- Validate a decoded cursor's version and then its query fingerprint. -/
def cursorChecks : List CursorCheck := [.formatVersion, .queryFingerprint]

/-- Initial version of the fingerprinted cursor envelope. -/
def cursorVersion : Nat := 1

/-- The query binding fingerprint width in hexadecimal characters. -/
def cursorFingerprintHexChars : Nat := 16

/-- The two keysets used by ordered queries. -/
inductive CursorKind where
  | memory
  | goal
  deriving DecidableEq

/-- An ordered query over one entity kind or the mixed Memory stream. -/
inductive Stream where
  | mixed
  | fact
  | abstraction
  | perspective
  | goal
  deriving DecidableEq

/-- A continuation requires exactly one stream and its matching keyset kind. -/
def cursorMatchesStream : CursorKind → Stream → Bool
  | .memory, .fact | .memory, .abstraction | .memory, .perspective
  | .goal, .goal => true
  | .memory, .mixed | .memory, .goal | .goal, .mixed
  | .goal, .fact | .goal, .abstraction | .goal, .perspective => false

/-- A .mixedmpty exact filter selects Goal handles, Fact references to Goals, or tags. -/
inductive ExactFilter where
  | goalHandles
  | referencedGoalHandles
  | tags
  deriving DecidableEq

/-- Nonempty exact filters are accepted only on a stream that implements them. -/
def acceptsExactFilter : ExactFilter → Stream → Bool
  | .goalHandles, .goal | .referencedGoalHandles, .fact
  | .tags, .fact | .tags, .abstraction | .tags, .perspective => true
  | .goalHandles, .fact | .goalHandles, .abstraction | .goalHandles, .perspective
  | .referencedGoalHandles, .goal | .referencedGoalHandles, .abstraction
  | .referencedGoalHandles, .perspective | .tags, .goal
  | .goalHandles, .mixed | .referencedGoalHandles, .mixed | .tags, .mixed => false

/-- Nonempty exact tag predicates check overlap or containment on one declared tag column. -/
inductive TagMatch where
  | any
  | all
  deriving DecidableEq

/-- Any requires at least one matching tag, and All requires every requested tag on that row. -/
def matchesTags : TagMatch → (anyMatch : Bool) → (allMatch : Bool) → Bool
  | .any, true, true | .any, true, false | .all, true, true | .all, false, true => true
  | .any, false, true | .any, false, false | .all, false, false | .all, true, false => false

/-- The stages of an ordered page assembled from indexed owner streams. -/
inductive OwnerPageStage where
  | filterVisibleRows
  | limitPerOwner
  | mergeAndLimit
  deriving DecidableEq

/-- Apply visibility and query predicates before each owner limit, then merge and bound the page. -/
def ownerPageStages : List OwnerPageStage := [.filterVisibleRows, .limitPerOwner, .mergeAndLimit]

/-- Fetch one extra row for an exhaustion witness in single-stream ordered reads. -/
def memoryPageExtraRows : Stream → Nat
  | .fact | .abstraction | .perspective => 1
  | .goal | .mixed => 0

/-- An unrestricted motivation filter, no motivations, or a set of matching Perspectives. -/
inductive Motivation where
  | any
  | unmotivated
  | matchingPerspectives
  deriving DecidableEq

/-- Match the Goal's motivation against the selected filter and Perspective intersection. -/
def matchesMotivation : Motivation → (hasMotivation : Bool) → (intersects : Bool) → Bool
  | .any, true, true | .any, true, false | .any, false, true | .any, false, false
  | .unmotivated, false, true | .unmotivated, false, false
  | .matchingPerspectives, true, true => true
  | .unmotivated, true, true | .unmotivated, true, false
  | .matchingPerspectives, true, false | .matchingPerspectives, false, true
  | .matchingPerspectives, false, false => false

/-- Derive a Goal version's changed time from its admission and its set time from the earliest admission. -/
inductive GoalTimeSource where
  | earliestAdmission
  | returnedAdmission
  deriving DecidableEq

/-- The admission sources of Goal set and change timestamps, in that order. -/
def goalTimeSources : List GoalTimeSource := [.earliestAdmission, .returnedAdmission]

/-- The two authority gates used by row reads and the graph overview. -/
inductive ReadSurface where
  | rows
  | graphOverview
  deriving DecidableEq

/-- Row reads use row visibility and the graph overview requires owner administration. -/
def permitsReadSurface : ReadSurface → (rowsReadable : Bool) → (ownerAdmin : Bool) → Bool
  | .rows, true, true | .rows, true, false
  | .graphOverview, true, true | .graphOverview, false, true => true
  | .rows, false, true | .rows, false, false
  | .graphOverview, true, false | .graphOverview, false, false => false

/-- Steps for authorizing a caller's explicit list of entry reads. -/
inductive EntryAuthorizationStep where
  | loadVisibleEntriesTogether
  | authorizeInRequestOrder
  deriving DecidableEq

/-- Fetch visibility in one batch and then authorize each entry in request order. -/
def entryAuthorization : List EntryAuthorizationStep :=
  [.loadVisibleEntriesTogether, .authorizeInRequestOrder]

/-- Graph readiness reports current client routing separately from retryable and terminal embedding work. -/
structure GraphReadiness where
  embeddingsClientConfigured : Bool
  pendingEmbeddingJobs : Nat
  failedEmbeddingJobs : Nat

/-- An ordered owner read needs read authority, and a row-locking read also needs write authority. -/
def permitsOwnerVersionRead : (readable : Bool) → (lock : Bool) → (writable : Bool) → Bool
  | true, false, true | true, false, false | true, true, true => true
  | true, true, false | false, false, true | false, false, false
  | false, true, true | false, true, false => false

/-- A requested sketch needs either a readable live Memory or a readable Goal parent. -/
def readableSketch : (liveMemoryParent : Bool) → (goalParent : Bool) → Bool
  | true, false | true, true | false, true => true
  | false, false => false

/-- A page limit before defaulting or clamping. -/
inductive LimitInput where
  | omitted
  | zero
  | withinBound
  | aboveBound
  deriving DecidableEq

/-- The action for an ordered page limit. -/
inductive LimitAction where
  | useDefault
  | refuse
  | keep
  | clamp
  deriving DecidableEq

/-- Default an omitted limit, refuse zero, keep a positive limit up to the ceiling, and clamp above it. -/
def pageLimit : LimitInput → LimitAction
  | .omitted => .useDefault
  | .zero => .refuse
  | .withinBound => .keep
  | .aboveBound => .clamp

/-- The lengths distinguished after trimming whitespace and counting characters. -/
inductive TrimmedLength where
  | blank
  | withinBound
  | oversized
  deriving DecidableEq

/-- The field can be accepted, refused as blank, or refused with an exceeded bound. -/
inductive TextAdmission where
  | accept
  | blank
  | tooLong
  deriving DecidableEq

/-- Reject blank or oversized trimmed text with distinct reasons. -/
def admitTrimmedText : TrimmedLength → TextAdmission
  | .blank => .blank
  | .withinBound => .accept
  | .oversized => .tooLong

/-- Default size of a common ordered page when the caller omits its limit. -/
def defaultPageLimit : Nat := 50

/-- Ceiling for the common ordered-page limit. -/
def maxPageLimit : Nat := 200

/-- Maximum input UUIDs in one batch read, before deduplication. -/
def maxBatchMemoryIds : Nat := 100

/-- Maximum accepted query length in characters after trimming. -/
def maxQueryChars : Nat := 512

/-- Maximum text cap a caller may request in a search result, in characters. -/
def maxTextCapChars : Nat := 8000

/-- Default number of sketches returned by recall. -/
def defaultRecallLimit : Nat := 16

/-- Maximum number of sketches returned by recall. -/
def maxRecallLimit : Nat := 32

/-- Maximum number of subjects in a recall cue. -/
def maxRecallSubjects : Nat := 16

/-- Maximum characters in a local recall sketch. -/
def sketchChars : Nat := 160

/-- Recall requires a nonblank question or at least one subject. -/
def recallHasCue : (hasQuestion : Bool) → (hasSubjects : Bool) → Bool
  | true, true | true, false | false, true => true
  | false, false => false

/-- A count reply has a separate field for each entity kind. -/
structure HeadCounts where
  facts : Nat
  abstractions : Nat
  perspectives : Nat
  goals : Nat

/-- A count row refers to the latest live readable admission per series. -/
def countedVersions : History := .headsOnly

/-- The literal placeholder replaced with the search text in a configured embedding instruction. -/
def queryPlaceholder : String := "{query}"

/-- An optional instruction supplies a template when it is configured and leaves a bare query otherwise. -/
inductive QueryInstruction where
  | unchanged
  | substituteQuery
  | refuseMissingPlaceholder
  deriving DecidableEq

/-- Refuse a configured instruction without the query placeholder. -/
def queryInstruction : (configured : Bool) → (hasPlaceholder : Bool) → QueryInstruction
  | false, true | false, false => .unchanged
  | true, true => .substituteQuery
  | true, false => .refuseMissingPlaceholder

end Proxiima.Query
