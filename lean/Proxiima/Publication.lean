import Proxiima.Operations

/-! # Publication -/

namespace Proxiima.Publication

/-- The default pending-record ceiling. -/
def defaultMaxPending : Nat := 100000

/-- The default serialized envelope ceiling in bytes. -/
def defaultMaxPayloadBytes : Nat := 524288

/-- The default published-body retention in seconds. -/
def defaultRetentionSeconds : Nat := 604800

/-- The minimum positive published-body retention in seconds. -/
def minRetentionSeconds : Nat := 60

/-- The configured retention value that disables pruning. -/
def retentionOffSeconds : Nat := 0

/-- The minimum duration of a publication claim in seconds. -/
def minLeaseSeconds : Nat := 1

/-- The publication backlog estimate freshness window in seconds. -/
def backlogFreshSeconds : Nat := 30

/-- The shortest interval in seconds between backlog estimate refreshes. -/
def backlogRefreshSeconds : Nat := 10

/-- The structured CloudEvents specification version. -/
def cloudEventsSpecVersion : String := "1.0"

/-- The media type of the typed payload. -/
def dataContentType : String := "application/json"

/-- The longest CloudEvents extension name. -/
def maxExtensionNameChars : Nat := 20

/-- The longest string extension value in bytes. -/
def maxExtensionValueBytes : Nat := 256

/-- The initial positive sequence allocated to a series. -/
def firstSequence : Nat := 1

/-- Series positions are padded to this many decimal digits. -/
def sequenceDigits : Nat := 20

/-- The default durable consumer lane seeded by the installation. -/
def defaultConsumer : String := "default"

/-- The longest consumer name in bytes. -/
def maxConsumerNameBytes : Nat := 63

/-- The longest publisher label in bytes. -/
def maxPublisherIdBytes : Nat := 128

/-- The number of records a drain leases at once. -/
def drainBatch : Nat := 64

/-- The default duration of a drain's claim in seconds. -/
def drainLeaseSeconds : Nat := 30

/-- The first poll wait of an idle or failed drain in milliseconds. -/
def pollFloorMillis : Nat := 500

/-- The maximum wait in seconds before polling again. -/
def pollCeilingSeconds : Nat := 30

/-- The deadline in seconds for one intake attempt. -/
def intakeTimeoutSeconds : Nat := 5

/-- Dispatch a pending mark by this divisor of the renewed claim duration. -/
def flushLeaseDivisor : Nat := 2

/-- The maximum records in one retention transaction. -/
def retentionBatch : Nat := 1000

/-- The interval between complete retention passes in seconds. -/
def retentionIntervalSeconds : Nat := 60

/-- The name of the built-in HTTP consumer lane. -/
def httpConsumer : String := "http"

/-- The HTTP request deadline in milliseconds. -/
def httpRequestTimeoutMillis : Nat := 4500

/-- The structured CloudEvents HTTP media type. -/
def httpContentType : String := "application/cloudevents+json"

/-- The HTTP header carrying the count of delivery claims. -/
def httpDeliveryCountHeader : String := "Proxiima-Delivery-Count"

/-- The maximum characters retained from a receiver rejection reason. -/
def httpReasonChars : Nat := 512

/-- Whether a Fact is a new admission and carries a publication plan. -/
inductive CaptureAdmission where
  | replay | quiet | listenable
  deriving DecidableEq

/-- Only a fresh listenable admission captures an outbox event in its transaction. -/
def atomicCapture : CaptureAdmission → Bool
  | .replay | .quiet => false
  | .listenable => true

/-- The possible capture stages before a listenable write can commit. -/
inductive CaptureStage where
  | capacity | seal | insert | commit
  deriving DecidableEq

/-- The capture pipeline checks capacity, seals the envelope, inserts it before committing. -/
def captureOrder : List CaptureStage := [.capacity, .seal, .insert, .commit]

/-- The failures that prevent a Fact from capturing its event. -/
inductive CaptureFailure where
  | payloadTooLarge | capacity | exportFailed | sourceUnbound | untypedWrite
  deriving DecidableEq

/-- The engine's stable capture refusal categories. -/
inductive CaptureRefusal where
  | invalidArgument | backpressure | internal
  deriving DecidableEq

/-- Export and payload defects are invalid input, queue exhaustion is backpressure and missing source is internal. -/
def captureRefusal : CaptureFailure → CaptureRefusal
  | .payloadTooLarge | .exportFailed | .untypedWrite => .invalidArgument
  | .capacity => .backpressure
  | .sourceUnbound => .internal

/-- A frozen registry needs a producer source exactly when it has at least one listenable schema. -/
def compositionAdmitted : (hasListenable : Bool) → (sourceBound : Bool) → Bool
  | true, false => false
  | true, true | false, true | false, false => true

/-- A listenable schema must resolve to a typed JSON schema before freeze. -/
def schemaFreezeAdmitted : (listenable : Bool) → (hasJsonSchema : Bool) → Bool
  | true, false => false
  | true, true | false, true | false, false => true

/-- The provenance fields of a captured envelope. -/
inductive EnvelopeProvenance where
  | deploymentSource | admittedOwner | authorizedModel | hostBoundExtensions
  deriving DecidableEq

/-- The capture plan stamps source, owner and authorized host provenance. -/
def stampedProvenance : List EnvelopeProvenance :=
  [.deploymentSource, .admittedOwner, .authorizedModel, .hostBoundExtensions]

/-- The sections of a structured envelope. -/
inductive EnvelopeSection where
  | substrate | extensions | data
  deriving DecidableEq

/-- The serialization order of structured event sections. -/
def envelopeSectionOrder : List EnvelopeSection := [.substrate, .extensions, .data]

/-- The spelling or presence of the subject and position attributes. -/
inductive PositionAttributes where
  | neither | subjectOnly | sequenceOnly | bothCanonical | bothMalformed
  deriving DecidableEq

/-- A stored event must carry a canonical pair of subject and position attributes. -/
def subjectPositionAdmitted : PositionAttributes → Bool
  | .bothCanonical => true
  | .neither | .subjectOnly | .sequenceOnly | .bothMalformed => false

/-- The delivery evidence on a record being forgotten. -/
inductive ForgetDelivery where
  | noDeliveredLane | deliveredWithDebt | deliveredWithoutDebt
  deriving DecidableEq

/-- The debts on a record when removing a consumer. -/
inductive RetirementDebt where
  | lastDebt | otherDebt | noDebtToConsumer
  deriving DecidableEq

/-- The state of a retained record relative to the pruning horizon. -/
inductive RetentionState where
  | pending | publishedRecent | publishedOld
  deriving DecidableEq

/-- A lifecycle operation with the record state relevant to its decision. -/
inductive LaneEvent where
  | forget (delivery : ForgetDelivery)
  | retire (debt : RetirementDebt)
  | retain (state : RetentionState)
  | hardErase
  deriving DecidableEq

/-- The lifecycle effect on the aggregate record. -/
inductive RecordEffect where
  | keep | settle | delete
  deriving DecidableEq

/-- The aggregate record effect and whether its origin survives. -/
structure LaneEffect where
  record : RecordEffect
  preservesOrigin : Bool
  deriving DecidableEq

/-- Decide the aggregate record and origin effects of a lifecycle operation. -/
def laneLifecycle : LaneEvent → LaneEffect
  | .forget .noDeliveredLane => ⟨.delete, true⟩
  | .forget .deliveredWithDebt | .retire .lastDebt => ⟨.settle, true⟩
  | .forget .deliveredWithoutDebt | .retire .otherDebt | .retire .noDebtToConsumer
  | .retain .pending | .retain .publishedRecent => ⟨.keep, true⟩
  | .retain .publishedOld => ⟨.delete, true⟩
  | .hardErase => ⟨.delete, false⟩

/-- A consumer's current lease and delivery status. -/
inductive LaneState where
  | pending | claimLive | claimExpired | published
  deriving DecidableEq

/-- Claim only pending work and expired claims of the selected consumer. -/
def laneClaimable : LaneState → Bool
  | .pending | .claimExpired => true
  | .claimLive | .published => false

/-- A live exact claim may mark only if the retained origin remains eligible. -/
def canMark : (exactLiveClaim : Bool) → (originEligible : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- The mark result observed after a fenced batch update. -/
inductive MarkResult where
  | published | alreadyPublished | staleClaim
  deriving DecidableEq

/-- Report a newly updated lane first, then an already published lane, then a stale claim. -/
def markVerdict : (changed : Bool) → (published : Bool) → MarkResult
  | true, true | true, false => .published
  | false, true => .alreadyPublished
  | false, false => .staleClaim

/-- The storage evidence examined for an exact physical publication origin. -/
inductive OriginEvidence where
  | retained | erased | missing
  deriving DecidableEq

/-- Hand over and mark only when the publication origin is retained and eligible. -/
def originEligible : OriginEvidence → Bool
  | .retained => true
  | .erased | .missing => false

/-- The outcome of attempting intake for one claimed event. -/
inductive IntakeOutcome where
  | accepted | rejected | malformed | intakeError | panic | timeout
  deriving DecidableEq

/-- The runtime owes a mark after either durable decision and retries failures. -/
def owesMark : IntakeOutcome → Bool
  | .accepted | .rejected => true
  | .malformed | .intakeError | .panic | .timeout => false

/-- Cancellation hands back work in flight. -/
def handBack : Proxiima.Operations.ClaimBoundary → Bool
  | .cancelled => true
  | .opened | .closed => false

/-- The relevant classes of HTTP response status. -/
inductive HttpStatusClass where
  | success | badRequest | unprocessable | other
  deriving DecidableEq

/-- Success and 400 or 422 settle a lane, while other HTTP statuses are retryable. -/
def httpAcknowledgement : HttpStatusClass → IntakeOutcome
  | .success => .accepted
  | .badRequest | .unprocessable => .rejected
  | .other => .intakeError

/-- The remaining delivery debt of a captured record. -/
inductive LaneDebt where
  | none | pending | leased
  deriving DecidableEq

/-- A record is settled precisely when no delivery debt remains. -/
def aggregateSettled : LaneDebt → Bool
  | .none => true
  | .pending | .leased => false

/-- The fixed metadata and bytes after the envelope is sealed. -/
inductive SealedPart where
  | physicalFactId | unversionedSchema | versionedSchemaUri | occurrenceTime | bytes | blake3Digest
  deriving DecidableEq

/-- The envelope identity binds the physical Fact, schema, occurrence, bytes and digest. -/
def sealedEventIdentity : List SealedPart :=
  [.physicalFactId, .unversionedSchema, .versionedSchemaUri, .occurrenceTime, .bytes, .blake3Digest]

/-- Which path proves full-queue backpressure. -/
inductive QueueEvidence where
  | zeroLimit | freshBelowLimit | probedBelowLimit | probedAtLeastLimit
  deriving DecidableEq

/-- A low fresh estimate or low exact probe permits capture; a zero or saturated ceiling refuses it. -/
def withinQueueCeiling : QueueEvidence → Bool
  | .freshBelowLimit | .probedBelowLimit => true
  | .zeroLimit | .probedAtLeastLimit => false

/-- When captured records become due for a mark batch. -/
inductive MarkTiming where
  | continuing | leaseDeadline | passEnds | handBack
  deriving DecidableEq

/-- Send outstanding marks when the lease-relative deadline arrives or the pass hands back or ends. -/
def flushAccepted : MarkTiming → Bool
  | .continuing => false
  | .leaseDeadline | .passEnds | .handBack => true

/-- The independent capabilities held by the host runtime. -/
inductive OutboxCapability where
  | delivery | registration | retention | exactOriginCheck
  deriving DecidableEq

/-- Which publication calls a host-held capability grants. -/
inductive OutboxOperation where
  | claim | renew | mark | release | register | list | retire | prune | checkOrigin
  deriving DecidableEq

/-- Delivery, administration, retention and origin eligibility use disjoint ports. -/
def hostOutboxOperations : OutboxCapability → List OutboxOperation
  | .delivery => [.claim, .renew, .mark, .release]
  | .registration => [.register, .list, .retire]
  | .retention => [.prune]
  | .exactOriginCheck => [.checkOrigin]

/-- The recorded attribution and bytes reuse frozen at capture. -/
inductive ImmutablePublicationField where
  | physicalFact | originalOwner | originalSource | eventId | envelope | digest
  deriving DecidableEq

/-- Publication identity, attribution and envelope bytes survive transfer unchanged. -/
def immutablePublication : List ImmutablePublicationField :=
  [.physicalFact, .originalOwner, .originalSource, .eventId, .envelope, .digest]

/-- The remaining work after a bounded retention delete. -/
inductive RetentionBatchResult where
  | full | short | failed | cancelled
  deriving DecidableEq

/-- Only a full retention batch calls for another transaction in the same pass. -/
def pruneAgain : RetentionBatchResult → Bool
  | .full => true
  | .short | .failed | .cancelled => false

/-- The visible result of the records attempted in a pass. -/
inductive DeliveryPassEvidence where
  | progress | noProgressWithFailure | noFailure
  deriving DecidableEq

/-- Only a pass with a failure and no successful mark reports its first failure. -/
def deliveryPassFailed : DeliveryPassEvidence → Bool
  | .noProgressWithFailure => true
  | .progress | .noFailure => false

end Proxiima.Publication
