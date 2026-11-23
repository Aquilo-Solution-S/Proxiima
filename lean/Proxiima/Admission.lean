/-! # Admission -/

namespace Proxiima.Admission

/-- Maximum characters in a trimmed explicit request key. -/
def requestKeyMax : Nat := 180

/-- Maximum remembered Facts in one episode. -/
def episodeRememberMax : Nat := 16

/-- Maximum stances in one episode. -/
def episodeStanceMax : Nat := 16

/-- Maximum Goals in one episode. -/
def episodeGoalMax : Nat := 16

/-- Maximum bind keys in one episode. -/
def episodeBindMax : Nat := 32

/-- Default and maximum row limit for a sidecar precondition read. -/
def sidecarReadMax : Nat := 512

/-- The encoded width of the lowercase BLAKE3 digest used as a typed Fact ingest key. -/
def receiptIndexHexChars : Nat := 64

/-- The domain prefix of a length-framed request fingerprint. -/
def requestFingerprintDomain : String := "proxiima-request-v1"

/-- Total begin-body-commit attempts in the storage retry loop. -/
def transactionAttempts : Nat := 3

/-- Outcomes considered by the storage transaction retry loop. -/
inductive TransactionOutcome where
  | transient
  | identityCollision
  | other

/-- The next action after a complete transaction attempt. -/
inductive RetryAction where
  | retry
  | returnOutcome
  | refuseCollision

/-- A collision can use one remaining attempt and every retry consumes the total budget. -/
def retryTransaction : TransactionOutcome → (attemptLeft : Bool) → (collisionRetried : Bool) → RetryAction
  | .transient, true, false | .transient, true, true => .retry
  | .identityCollision, true, false => .retry
  | .identityCollision, true, true | .identityCollision, false, false
  | .identityCollision, false, true => .refuseCollision
  | .transient, false, false | .transient, false, true
  | .other, true, false | .other, true, true
  | .other, false, false | .other, false, true => .returnOutcome

/-- A retained request record is compared against the incoming fingerprint. -/
inductive RequestRecord where
  | absent
  | sameFingerprint
  | differentFingerprint

/-- A request either admits, returns the retained identity, or conflicts with that identity. -/
inductive RequestVerdict where
  | admit
  | replay
  | conflict

/-- Keyed writes arbitrate by fingerprint under the owner and request namespace. -/
def requestVerdict : RequestRecord → RequestVerdict
  | .absent => .admit
  | .sameFingerprint => .replay
  | .differentFingerprint => .conflict

/-- The independent request-key namespaces of Memory writes. -/
inductive MemoryRequestScope where
  | note
  | utterance
  | abstraction
  | perspective
  | goalAbstraction
  | goalPerspective

/-- The request ledger uses Goal and subject qualifiers only for Goal-work writes. -/
def requestScopeUsesGoalAndSubject : MemoryRequestScope → Bool
  | .note | .utterance | .abstraction | .perspective => false
  | .goalAbstraction | .goalPerspective => true

/-- The normalization applied to unordered request fingerprint fields. -/
inductive FingerprintSetStep where
  | sort
  | deduplicate

/-- Request fingerprint UUID sets and normalized tags are sorted and deduplicated. -/
def fingerprintSetOrder : List FingerprintSetStep := [.sort, .deduplicate]

/-- The ordered field encoding of a present fingerprint payload. -/
inductive FingerprintPayloadPart where
  | schemaIdLength
  | schemaId
  | schemaVersion
  | canonicalJson

/-- A payload's schema and version precede its canonical JSON in a fingerprint field. -/
def fingerprintPayload : List FingerprintPayloadPart :=
  [.schemaIdLength, .schemaId, .schemaVersion, .canonicalJson]

/-- Candidate series identities for a Fact write. -/
inductive SeriesSelection where
  | explicitHandle
  | naturalKeyMatch
  | mint

/-- Explicit handle wins over a natural-key match, otherwise the write mints. -/
def factSeries : (explicitHandle : Bool) → (naturalMatch : Bool) → SeriesSelection
  | true, true | true, false => .explicitHandle
  | false, true => .naturalKeyMatch
  | false, false => .mint

/-- A claim uses the explicit request ledger, the source receipt, or no replay key. -/
inductive ReplayAuthority where
  | requestKey
  | sourceReceipt
  | none

/-- Explicit request claims take priority over the source receipt. -/
def replayAuthority : (keyed : Bool) → (sourced : Bool) → ReplayAuthority
  | true, true | true, false => .requestKey
  | false, true => .sourceReceipt
  | false, false => .none

/-- The most recent observation's admission status within its series. -/
inductive ObservationStatus where
  | head
  | displacedLive
  | forgotten

/-- Re-observation can append only a displaced live observation and only when enabled. -/
def appendObservation : ObservationStatus → (enabled : Bool) → Bool
  | .displacedLive, true => true
  | .displacedLive, false | .head, true | .head, false
  | .forgotten, true | .forgotten, false => false

/-- Whether a forgotten admission is sought for a public read or exact ingest replay. -/
inductive ReceiptUse where
  /-- An owner-scoped lookup holds a share lock on any visible admission. -/
  | lookup
  /-- Replay answers using the original identity and does not restore the admission. -/
  | replay

/-- Receipt lookup hides forgotten admissions while receipt replay can return them. -/
def receiptAnswers : ReceiptUse → (forgotten : Bool) → Bool
  | .lookup, true => false
  | .lookup, false | .replay, true | .replay, false => true

/-- The three payload legs that require typed Fact persistence. -/
inductive FactWriteLeg where
  | sidecar
  | citation
  | publication

/-- The typed route admits sidecars, citations and publication; the receipt-only route refuses all three. -/
def typedFactLegs : List FactWriteLeg := [.sidecar, .citation, .publication]

/-- The order of the episode's writes followed by their shared commit. -/
inductive EpisodeStep where
  | writeAct
  | remember
  | derive
  | stance
  | goal
  | commit

/-- An episode writes the act and all declared items in one owner unit. -/
def episodeOrder : List EpisodeStep := [.writeAct, .remember, .derive, .stance, .goal, .commit]

/-- The produced slots an episode may name in its bind list. -/
inductive EpisodeSlot where
  | remember
  | derive
  | stance
  | goal

/-- Only an already produced Memory slot can serve as a local Memory reference. -/
def localMemorySlot : EpisodeSlot → (written : Bool) → Bool
  | .remember, true | .derive, true | .stance, true => true
  | .remember, false | .derive, false | .stance, false
  | .goal, true | .goal, false => false

/-- A bound item requires a fresh admission that can reference the episode's write-act. -/
def acceptEpisodeItem : (bound : Bool) → (replayed : Bool) → Bool
  | true, true => false
  | false, false | false, true | true, false => true

/-- An episode compares explicit keys after normalization within one item family. -/
inductive EpisodeKeyMatch where
  | omitted
  | firstExplicit
  | repeatedExplicit

/-- A repeated normalized explicit key is refused during preflight. -/
def uniqueEpisodeKeys : EpisodeKeyMatch → Bool
  | .omitted | .firstExplicit => true
  | .repeatedExplicit => false

/-- The session steps of a sidecar precondition read. -/
inductive SidecarPreconditionStep where
  | takeExclusiveLock
  | readOwnerRows

/-- The sidecar precondition read takes the exclusive lock on its own transaction before reading. -/
def sidecarPreconditionOrder : List SidecarPreconditionStep :=
  [.takeExclusiveLock, .readOwnerRows]

/-- A projector cursor's owner-scoped operation. -/
inductive CursorOperation where
  | load
  | store
  | age

/-- Load and store require Fact write authority, while age only requires Fact read authority. -/
def cursorRequiresFactWrite : CursorOperation → Bool
  | .load | .store => true
  | .age => false

/-- The owner and source fields of an opaque projector cursor. -/
inductive SourceCursorKey where
  | owner
  | source

/-- Cursor reads, writes and erasure key the stored bytes by the resolved owner and exact source. -/
def sourceCursorKey : List SourceCursorKey := [.owner, .source]

/-- Ordered phases of deadline-bound detached ingestion. -/
inductive DetachedStep where
  | spawnOnFirstPoll
  | runWithOriginalAuthority
  | settleWithinDeadline

/-- The spawned Fact ingest can finish after request cancellation, within its own deadline. -/
def detachedIngestOrder : List DetachedStep :=
  [.spawnOnFirstPoll, .runWithOriginalAuthority, .settleWithinDeadline]

/-- How write-session changes are finalized when the unit terminates. -/
inductive UnitDisposition where
  | commit
  | drop
  | poisonedCommit

/-- Only an explicit commit of a healthy unit makes its transaction durable. -/
def commitsUnit : UnitDisposition → Bool
  | .commit => true
  | .drop | .poisonedCommit => false

/-- Data admitted together by one typed Fact write. -/
inductive TypedFactCommitMember where
  | fact
  | registeredSidecars
  | admittedCitation

/-- A typed Fact and its supplied sidecars and citation persist in one transaction. -/
def typedFactCommit : List TypedFactCommitMember := [.fact, .registeredSidecars, .admittedCitation]

/-- Steps that turn an ownerless Fact request into an owner-stamped write. -/
inductive FactOwnerStep where
  | resolveSingleWritableOwner
  | authorizeFactWrite
  | stampResolvedOwner

/-- Fact authorization resolves one writable owner and storage stamps the permit's owner. -/
def factOwnerOrder : List FactOwnerStep :=
  [.resolveSingleWritableOwner, .authorizeFactWrite, .stampResolvedOwner]

/-- The verification performed at a Fact write's typed and external ingress boundaries. -/
inductive IngressValidation where
  | factSchemaRegistration
  | externalCitationContract

/-- A typed Fact requires registration and an inline external citation also requires payload validation. -/
def factIngressValidation : (inlineCitation : Bool) → List IngressValidation
  | false => [.factSchemaRegistration]
  | true => [.factSchemaRegistration, .externalCitationContract]

/-- The phases shared by governed Fact and derived Memory writes. -/
inductive MemoryWriteStep where
  | prepare
  | lock
  | claim
  | materialize

/-- Governed Memory writes prepare the candidate, lock its targets, arbitrate its claim and materialize on one transaction. -/
def memoryWriteOrder : List MemoryWriteStep := [.prepare, .lock, .claim, .materialize]

/-- The locks acquired by a new Memory admission before its durable writes. -/
inductive MemoryLock where
  | ownerFence
  | sourceFence
  | declaredScopeFences
  | seriesHandles
  | lifecycleVersions

/-- Admission takes its owner, optional source and declared-scope fences before handle and version locks. -/
def memoryLockOrder : List MemoryLock :=
  [.ownerFence, .sourceFence, .declaredScopeFences, .seriesHandles, .lifecycleVersions]

/-- The durable identity of a sourced receipt lookup or replay. -/
inductive IngestKeyPart where
  | owner
  | source
  | ingestKey

/-- A sourced receipt keys its original admission by owner, source and ingest key. -/
def ingestKeyParts : List IngestKeyPart := [.owner, .source, .ingestKey]

/-- The index prefix for the request identity digest. -/
def requestRowPrefix : String := "r:"

/-- The index prefix for the digest of the length-framed key and fingerprint. -/
def fingerprintRowPrefix : String := "f:"

/-- Whether the authoring request named an owner space or a working Goal. -/
inductive WriteDestination where
  | ownerSpace
  | workingGoal

/-- Refusal of more than one destination in an authoring request. -/
inductive DestinationRefusal where
  | bothDestinations

/-- Default to the owner space and refuse simultaneous space and working-Goal selectors. -/
def selectDestination : (space : Bool) → (goal : Bool) → Except DestinationRefusal WriteDestination
  | false, false | true, false => .ok .ownerSpace
  | false, true => .ok .workingGoal
  | true, true => .error .bothDestinations

/-- The direct observation source family for a recorded conversation turn. -/
def utteranceSource : String := "core/conversation"

/-- The distilled Fact source for remembered notes and episode notes. -/
def rememberedNoteSource : String := "core/agent"

/-- Inputs to the decision to preserve or reselect a natural-key series. -/
inductive NaturalSeriesProbe where
  | liveMatch
  | forgottenMatch
  | noMatch

/-- An existing natural-key series can take a new version even if its predecessor is forgotten. -/
def reuseNaturalSeries : NaturalSeriesProbe → Bool
  | .liveMatch | .forgottenMatch => true
  | .noMatch => false

/-- Where a tool's derived request gets its key. -/
inductive DerivationRequestKey where
  | typedExplicit
  | contentFingerprint

/-- How earlier episode endpoints can participate in later writes. -/
inductive UnitEndpoint where
  | newMemory
  | newGoal
  | forgottenReplay

/-- Written Memory and Goals supply same-unit typed endpoints; a forgotten replay supplies no live endpoint. -/
def unitEndpointLive : UnitEndpoint → Bool
  | .newMemory | .newGoal => true
  | .forgottenReplay => false

end Proxiima.Admission
