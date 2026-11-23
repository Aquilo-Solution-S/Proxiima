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

/-- The producer emits structured CloudEvents at this specification version. -/
def cloudEventsSpecVersion : String := "1.0"

/-- The producer serializes the typed payload as JSON. -/
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

/-- The maximum bytes retained from a receiver rejection body. -/
def httpReasonBytes : Nat := 2048

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

/-- A host is the sole source of the configured event URI and trusted run extensions. -/
inductive EnvelopeProvenance where
  | deploymentSource | admittedOwner | authorizedModel | hostBoundExtensions
  deriving DecidableEq

/-- The capture plan stamps source, owner and authorized host provenance. -/
def stampedProvenance : List EnvelopeProvenance :=
  [.deploymentSource, .admittedOwner, .authorizedModel, .hostBoundExtensions]

/-- The envelope writes substrate attributes, sorted extensions, then data. -/
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

/-- An outbox mutation that can settle or remove lanes and bodies. -/
inductive LaneEvent where
  | forget | retire | retain | hardErase
  deriving DecidableEq

/-- Which lanes a lifecycle operation removes from the captured event. -/
inductive LaneRemoval where
  | undelivered | retiredConsumer | prunedRecord | erasedRecord
  deriving DecidableEq

/-- What the lifecycle operation does with the aggregate record. -/
inductive RecordEffect where
  | deleteIfNoLanesOtherwiseSettle | settle | deleteIfPublishedAndOld | delete
  deriving DecidableEq

/-- The lane removal, aggregate effect and origin preservation of a lifecycle operation. -/
structure LaneEffect where
  lanes : LaneRemoval
  record : RecordEffect
  preservesOrigin : Bool
  deriving DecidableEq

/-- Forget cancels pending work, retirement removes a consumer, retention reclaims published bodies and erase removes origins. -/
def laneLifecycle : LaneEvent → LaneEffect
  | .forget => ⟨.undelivered, .deleteIfNoLanesOtherwiseSettle, true⟩
  | .retire => ⟨.retiredConsumer, .settle, true⟩
  | .retain => ⟨.prunedRecord, .deleteIfPublishedAndOld, true⟩
  | .hardErase => ⟨.erasedRecord, .delete, false⟩

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

/-- Cancel all unstarted work when quiescing or stopping, and interrupt in-flight intake only on shutdown. -/
def handBack : (quiescing : Bool) → (stopping : Bool) → (inFlight : Bool) → Bool
  | true, true, true | false, true, true | true, true, false | false, true, false => true
  | true, false, false => true
  | true, false, true | false, false, true | false, false, false => false

/-- Whether a configured HTTP receiver has an explicit trust bundle. -/
inductive TrustSource where
  | system | configured | malformed
  deriving DecidableEq

/-- Refuse a malformed trust bundle, otherwise use its roots or the system roots. -/
def httpTrustAdmitted : TrustSource → Bool
  | .system | .configured => true
  | .malformed => false

/-- The relevant classes of HTTP response status. -/
inductive HttpStatusClass where
  | success | badRequest | unprocessable | other
  deriving DecidableEq

/-- Success and 400 or 422 settle a lane, while other HTTP statuses are retryable. -/
def httpAcknowledgement : HttpStatusClass → IntakeOutcome
  | .success => .accepted
  | .badRequest | .unprocessable => .rejected
  | .other => .intakeError

/-- The parser's classification of a configured producer identifier. -/
inductive SourceSpelling where
  | empty | forbiddenCharacters | relative | invalidUri | absoluteUri
  deriving DecidableEq

/-- Preserve the original spelling of a valid absolute URI and refuse other source strings. -/
def absoluteProducerSource : SourceSpelling → Bool
  | .absoluteUri => true
  | .empty | .forbiddenCharacters | .relative | .invalidUri => false

/-- The remaining delivery debt of a captured record. -/
inductive LaneDebt where
  | none | pending | leased
  deriving DecidableEq

/-- A record is settled precisely when no delivery debt remains. -/
def aggregateSettled : LaneDebt → Bool
  | .none => true
  | .pending | .leased => false

/-- The targets at capture are the consumers registered at that instant. -/
inductive FanOut where
  | noConsumers | consumersRegistered | consumersAddedLater
  deriving DecidableEq

/-- Capture creates lanes for the registrations it sees and registration never backfills old records. -/
def fanOut : FanOut → Bool
  | .consumersRegistered => true
  | .noConsumers | .consumersAddedLater => false

/-- The number of matching typed payloads on a listenable write. -/
inductive MatchingPayloads where
  | none | exactlyOne | multiple
  deriving DecidableEq

/-- Capture requires exactly one typed Fact payload of the schema and version being admitted. -/
def typedCaptureAdmitted : MatchingPayloads → Bool
  | .exactlyOne => true
  | .none | .multiple => false

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

/-- The scope that can check the original physical event. -/
inductive OriginBinding where
  | sameInstallation | differentInstallation | unreadableIdentity
  deriving DecidableEq

/-- Only the stamping installation supplies an authority for the origin. -/
def originAuthority : OriginBinding → Bool
  | .sameInstallation => true
  | .differentInstallation | .unreadableIdentity => false

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

/-- The inputs to binding a single host extension. -/
inductive ExtensionBinding where
  | valid | invalidName | invalidValue | reservedName | duplicateName | tooMany
  deriving DecidableEq

/-- A host extension must obey the name, value and capacity checks and add a new attribute. -/
def extensionAdmitted : ExtensionBinding → Bool
  | .valid => true
  | .invalidName | .invalidValue | .reservedName | .duplicateName | .tooMany => false

/-- The recorded attribution and bytes reuse frozen at capture. -/
inductive ImmutablePublicationField where
  | physicalFact | originalOwner | originalSource | eventId | envelope | digest
  deriving DecidableEq

/-- Publication identity, attribution and envelope bytes survive transfer unchanged. -/
def immutablePublication : List ImmutablePublicationField :=
  [.physicalFact, .originalOwner, .originalSource, .eventId, .envelope, .digest]

/-- Whether intake has received a captured ID in its canonical Fact UUID spelling. -/
inductive EventIdSpelling where
  | canonical | otherUuidSpelling | opaque
  deriving DecidableEq

/-- An intake classifies canonical UUIDs as Fact IDs and preserves other text verbatim. -/
def identifiesFact : EventIdSpelling → Bool
  | .canonical => true
  | .otherUuidSpelling | .opaque => false

/-- The durable evidence a publisher could observe before stopping. -/
inductive DeliveryBoundary where
  | beforeAcknowledgement | acknowledgedUnmarked | marked
  deriving DecidableEq

/-- A process exit before a fenced mark leaves the lane available to redeliver after expiry. -/
def canRedeliverAfterExpiry : DeliveryBoundary → Bool
  | .beforeAcknowledgement | .acknowledgedUnmarked => true
  | .marked => false

/-- The operation attempting to bind a configured consumer name. -/
inductive ConsumerBinding where
  | first | subsequentHostBinding | httpConflictsWithHost
  deriving DecidableEq

/-- The effect of binding a name in the runtime configuration. -/
inductive ConsumerBindingEffect where
  | insert | replace | refuse
  deriving DecidableEq

/-- A host can replace its intake; the HTTP registration refuses a collision. -/
def consumerBinding : ConsumerBinding → ConsumerBindingEffect
  | .first => .insert
  | .subsequentHostBinding => .replace
  | .httpConflictsWithHost => .refuse

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
