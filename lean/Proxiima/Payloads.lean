/-! # Payloads -/

namespace Proxiima.Payloads

/-- The sources for a projected Memory body. -/
inductive DisplaySource where
  | typedProjection
  | readFallback
  deriving DecidableEq

/-- The sources for a Memory display body in priority order. -/
def displaySources : List DisplaySource := [.typedProjection, .readFallback]

/-- The node kinds a payload reference can target. -/
inductive ReferenceTarget where
  | memoryVersion
  | goalVersion
  deriving DecidableEq

/-- The scope is present only when its kind is declared and its row ID is supplied. -/
def scopeSelection : (declared : Bool) → (hasScopeId : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- The opening marker of a receipt key. -/
def receiptMarker : List Nat := [0x50, 0x4b, 0x45, 0x59, 0x31, 0]

/-- Tag for a UTF-8 string. -/
def receiptStringTag : Nat := 115
/-- Tag for a one-byte Boolean. -/
def receiptBoolTag : Nat := 98
/-- Tag for an unsigned byte. -/
def receiptU8Tag : Nat := 49
/-- Tag for a big-endian unsigned 32-bit integer. -/
def receiptU32Tag : Nat := 52
/-- Tag for a big-endian signed 32-bit integer. -/
def receiptI32Tag : Nat := 73
/-- Tag for a big-endian signed 64-bit integer. -/
def receiptI64Tag : Nat := 76
/-- Tag for a big-endian unsigned 64-bit integer, also used by platform-sized counts. -/
def receiptU64Tag : Nat := 56
/-- Tag for 16 raw UUID bytes. -/
def receiptUuidTag : Nat := 117
/-- Tag for a length-framed byte sequence. -/
def receiptBytesTag : Nat := 66
/-- Tag for signed, big-endian, 128-bit epoch nanoseconds. -/
def receiptTimeTag : Nat := 116
/-- Tag for an optional UTF-8 string. -/
def receiptOptionalStringTag : Nat := 83
/-- Tag for an optional Boolean. -/
def receiptOptionalBoolTag : Nat := 79
/-- Tag for an optional unsigned byte. -/
def receiptOptionalU8Tag : Nat := 50
/-- Tag for an optional unsigned 32-bit integer. -/
def receiptOptionalU32Tag : Nat := 53
/-- Tag for an optional signed 32-bit integer. -/
def receiptOptionalI32Tag : Nat := 105
/-- Tag for an optional signed 64-bit integer. -/
def receiptOptionalI64Tag : Nat := 108
/-- Tag for an optional unsigned 64-bit integer or platform-sized count. -/
def receiptOptionalU64Tag : Nat := 57
/-- Tag for an optional UUID. -/
def receiptOptionalUuidTag : Nat := 85
/-- Tag for an optional byte sequence. -/
def receiptOptionalBytesTag : Nat := 67
/-- Tag for optional epoch nanoseconds. -/
def receiptOptionalTimeTag : Nat := 84
/-- Tag for a list preceded by its element count. -/
def receiptListTag : Nat := 91

/-- The parts of a declared receipt field in byte order. -/
inductive ReceiptFieldPart where
  | nameLength
  | nameBytes
  | tag
  | value
  deriving DecidableEq

/-- A receipt field frames its UTF-8 name before its type tag and value. -/
def receiptFieldOrder : List ReceiptFieldPart := [.nameLength, .nameBytes, .tag, .value]

/-- An optional receipt field distinguishes omission, explicit absence and each present value. -/
inductive ReceiptPresence where
  | omitted
  | absent
  | present
  deriving DecidableEq

/-- The presence byte written after a declared optional field's tag. -/
def receiptPresence : ReceiptPresence → Option Nat
  | .omitted => none
  | .absent => some 0
  | .present => some 1

/-- The byte width of durable and kind-scoped Content digests. -/
def contentHashBytes : Nat := 32

/-- The domain label of a durable Content hash input. -/
def durableHashDomain : String := "proxiima-content-v1"

/-- The components of the durable Content hash input. -/
inductive ContentHashPart where
  | domainLabel
  | utf8SchemaId
  | zeroByte
  | durableLengthU64Le
  | canonicalDurableBytes
  deriving DecidableEq

/-- The opening frame and the frame repeated for each durable payload. -/
structure ContentHashFrame where
  header : List ContentHashPart
  perPayload : List ContentHashPart

/-- The bytes hashed for the header and for each durable payload, in frame order. -/
def durableHashOrder : ContentHashFrame :=
  { header := [.domainLabel, .zeroByte]
    perPayload := [.utf8SchemaId, .zeroByte, .durableLengthU64Le, .canonicalDurableBytes, .zeroByte] }

/-- The kind classes that share or separate Content hashes. -/
inductive ContentHashKind where
  | fact
  | derived
  deriving DecidableEq

/-- A derived admission domain-separates the Content hash by kind. -/
def kindScopedHash : ContentHashKind → Bool
  | .fact => false
  | .derived => true

/-- The text kinds available to a typed natural key. -/
inductive KeyText where
  | text
  | paddedText
  deriving DecidableEq

/-- Only padded text discards trailing ASCII spaces. -/
def trimKeyPadding : KeyText → Bool
  | .text => false
  | .paddedText => true

/-- The registered scalar shapes projected into a natural key in field declaration order. -/
inductive KeyScalar where
  | uuid
  | text (kind : KeyText)
  | i16
  | i32
  | i64
  | bool
  deriving DecidableEq

/-- The scalar kinds available to a stateful Fact natural key. -/
def naturalKeyScalars : List KeyScalar := [.uuid, .text .text, .text .paddedText, .i16, .i32, .i64, .bool]

/-- The registered payload ingress shapes. -/
inductive IngressShape where
  | typed
  | opaqueCitedObject
  deriving DecidableEq

/-- The components of a schema contract's rendered ID. -/
inductive SchemaNamePart where
  | spiice
  | name
  | version
  deriving DecidableEq

/-- A rendered schema ID places the spiice before the schema name and its version. -/
def schemaNameOrder : List SchemaNamePart := [.spiice, .name, .version]

/-- The ordinal reserved for the core contract. -/
def coreOrdinal : Nat := 0

/-- How the schema catalog filters the registered payload inventory. -/
inductive InventoryFilter where
  | allKinds
  | selectedKind
  deriving DecidableEq

/-- Payload and resource sources of the schema inventory. -/
inductive InventorySource where
  | allPayloadKinds
  | selectedPayloadKind
  | resourceAnswers
  deriving DecidableEq

/-- The kind filter controls payload entries while the resource schema inventory is independent. -/
def listSchemaInventory : InventoryFilter → List InventorySource
  | .allKinds => [.allPayloadKinds, .resourceAnswers]
  | .selectedKind => [.selectedPayloadKind, .resourceAnswers]

/-- The schema resource replies only for a registered exact version with published JSON Schema. -/
def versionedSchemaAvailable : (registered : Bool) → (hasJsonSchema : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- Projection relation name. -/
def projectionTable : String := "projection"

/-- Projection's Memory key column. -/
def projectionMemoryColumn : String := "memory_id"

/-- Projection's Memory foreign key name. -/
def projectionMemoryForeignKey : String := "projection_memory_id_fkey"

/-- The largest authorized page of typed sidecar heads. -/
def sidecarHeadPageMax : Nat := 1000

/-- Whether a surface export carries rows, obtains them from a declared source, or excludes them. -/
inductive SurfaceExport where
  | rows
  | allowlist
  | derivedFromCarried
  | unresolvedDerivation
  | excluded
  deriving DecidableEq

/-- The export policy of an erased surface must end at rows or an allowlist. -/
def eraseExportable : SurfaceExport → Bool
  | .rows | .allowlist | .derivedFromCarried => true
  | .unresolvedDerivation | .excluded => false

/-- How a carried surface reaches its owner. -/
inductive SurfaceOwner where
  | ownColumn
  | keyHome
  | unreachable
  deriving DecidableEq

/-- A carried surface serves an owner through its own column or its key's single home. -/
def exportOwner : SurfaceOwner → Bool
  | .ownColumn | .keyHome => true
  | .unreachable => false

/-- The owner relation of a Memory sidecar. -/
inductive SidecarOwnerMode where
  | none
  | pinned
  | follows
  deriving DecidableEq

/-- Pinned and Follows sidecars stamp the row's own owner at insertion. -/
def stampsOwner : SidecarOwnerMode → Bool
  | .none => false
  | .pinned | .follows => true

/-- The inverse scope affecting a pinned Memory sidecar. -/
inductive PinnedTraversal where
  | memoryFollowing
  | wholeOwner
  | sourceErase
  deriving DecidableEq

/-- How an inverse targets a pinned row. -/
inductive PinnedInverseTarget where
  | skip
  | stampedOwner
  | stampedOwnerAndSource
  deriving DecidableEq

/-- Memory-following traversal skips pinned rows; owner inverses select by stamped owner. -/
def pinnedInverse : PinnedTraversal → PinnedInverseTarget
  | .memoryFollowing => .skip
  | .wholeOwner => .stampedOwner
  | .sourceErase => .stampedOwnerAndSource

/-- The timing and direction of the inverse check for a stamped sidecar. -/
inductive StampCheck where
  | sidecarInsert
  | sidecarRepoint
  | sidecarDelete
  | memoryCommit
  deriving DecidableEq

/-- Inserts and repoints require a declaration; reverse checks exempt pinned and retired retain-at-source sidecars. -/
def enforceStamp : SidecarOwnerMode → (retiredRetainAtSource : Bool) → List StampCheck
  | .none, false | .follows, false => [.sidecarInsert, .sidecarRepoint, .sidecarDelete, .memoryCommit]
  | .none, true | .follows, true | .pinned, false | .pinned, true => [.sidecarInsert, .sidecarRepoint]

/-- An emitted reference can ground payload-only provenance when its field and target are eligible. -/
def payloadSubjects : (declaredField : Bool) → (memoryTarget : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- What the installer records for a Memory sidecar. -/
inductive InstallationPart where
  | memoryForeignKey
  | declaredSurface
  | integrityTriggers
  | ownerPolicy
  deriving DecidableEq

/-- The Memory sidecar installer installs the key, declaration, integrity and owner policy together. -/
def installMemorySidecar : List InstallationPart :=
  [.memoryForeignKey, .declaredSurface, .integrityTriggers, .ownerPolicy]

/-- How a reference installation resolves an existing key. -/
inductive ExistingReference where
  | none
  | compatible
  | incompatible
  | secondCascadingMemoryParent
  deriving DecidableEq

/-- The result of installing a declared reference. -/
inductive ReferenceInstallation where
  | add
  | reuse
  | refuse
  deriving DecidableEq

/-- Identical keys are reusable; conflicting keys and a second cascading Memory parent are refused. -/
def installReference : ExistingReference → ReferenceInstallation
  | .none => .add
  | .compatible => .reuse
  | .incompatible | .secondCascadingMemoryParent => .refuse

/-- The ways a sidecar loader can provide a primitive. -/
inductive LoaderPrimitive where
  | batch
  | single
  | neither
  deriving DecidableEq

/-- The three dispatch outcomes for sidecar loads. -/
inductive LoaderDispatch where
  | callBatch
  | fanOutSingle
  | internalError
  deriving DecidableEq

/-- A batch override serves directly; the default calls a single override or reports the missing primitive. -/
def dispatchLoader : LoaderPrimitive → LoaderDispatch
  | .batch => .callBatch
  | .single => .fanOutSingle
  | .neither => .internalError

/-- The source of decoded sidecar key values. -/
inductive StoredKey where
  | supportedScalar
  | paddedText
  | null
  | unsupported
  deriving DecidableEq

/-- How a stored sidecar key is decoded. -/
inductive StoredKeyDecode where
  | preserveType
  | trimPadding
  | refuse
  deriving DecidableEq

/-- PostgreSQL type identity drives decoding and null or unsupported keys are refused. -/
def decodeStoredKey : StoredKey → StoredKeyDecode
  | .supportedScalar => .preserveType
  | .paddedText => .trimPadding
  | .null | .unsupported => .refuse

/-- Text shape checked by the repository schema ID gate. -/
inductive SchemaSpelling where
  | lowercaseWithoutWhitespace
  | notLowercase
  | containsWhitespace
  deriving DecidableEq

/-- A declared schema ID must be lowercase without whitespace. -/
def schemaIdSpelling : SchemaSpelling → Bool
  | .lowercaseWithoutWhitespace => true
  | .notLowercase | .containsWhitespace => false

/-- The checks that resolve a typed sidecar head request before storage. -/
inductive HeadRequestStep where
  | positiveLimit
  | registeredFact
  | declaredNaturalKey
  | declaredMemoryKey
  | typedPrefixInContractOrder
  deriving DecidableEq

/-- A head page resolves SQL identifiers and prefix types from the frozen Fact contract. -/
def resolveHeadRequest : List HeadRequestStep :=
  [.positiveLimit, .registeredFact, .declaredNaturalKey, .declaredMemoryKey, .typedPrefixInContractOrder]

/-- The sources of schema names in the composed database census. -/
inductive StorageSchemaSource where
  | core
  | activeSurface
  | registeredSidecar
  | retiredSurface
  deriving DecidableEq

/-- The sorted storage schema census includes every schema reached from these sources. -/
def storageSchemaCensus : List StorageSchemaSource :=
  [.core, .activeSurface, .registeredSidecar, .retiredSurface]

/-- The search and embedding consumers of a payload column. -/
inductive PayloadColumnConsumer where
  | search
  | embedding
  deriving DecidableEq

/-- The contract checks required before a pending transfer refusal can be used. -/
inductive TransferRefusalPart where
  | reason
  | enforcementSite
  deriving DecidableEq

/-- A schema-declared transfer refusal carries its reason and at least one enforcement site. -/
def transferRefusalParts : List TransferRefusalPart := [.reason, .enforcementSite]

/-- The missing-table and stale-policy states of a Follows sidecar. -/
inductive FollowsTableState where
  | missingTable
  | stalePolicy
  deriving DecidableEq

/-- How boot classifies a Follows table that needs attention. -/
inductive FollowsBootRefusal where
  | internal
  | migrationRequired
  deriving DecidableEq

/-- A missing Follows table yields Internal; stale generated policy requires migration. -/
def followsBootRefusal : FollowsTableState → FollowsBootRefusal
  | .missingTable => .internal
  | .stalePolicy => .migrationRequired

/-- The identifiers and behavior reconciled against core when binding a storage registration. -/
inductive RegistrationCheck where
  | schemaKind
  | typedPayload
  | naturalKey
  | sidecarTable
  | memoryKey
  | ownerMode
  | writeCapabilities
  deriving DecidableEq

/-- PostgreSQL-side registration must agree with the frozen core registration and contract. -/
def sidecarAgreement : List RegistrationCheck :=
  [.schemaKind, .typedPayload, .naturalKey, .sidecarTable, .memoryKey, .ownerMode, .writeCapabilities]

/-- A scoped sidecar session derives the owner from its permit and admits only narrower declared-table predicates. -/
inductive SidecarSessionFilter where
  | ownerFromPermit
  | additionalPredicate
  deriving DecidableEq

/-- The mandatory leading filter and optional narrowing inputs of a session read. -/
def sessionReadFilters : List SidecarSessionFilter := [.ownerFromPermit, .additionalPredicate]

/-- The held prerequisites of a scoped payload admission. -/
inductive ScopeAdmissionStep where
  | canonicalDistinctScopes
  | fenceAndProbeEachScope
  | lockSeries
  deriving DecidableEq

/-- A scoped admission checks liveness under the shared scope fence before the series locks. -/
def scopeAdmissionOrder : List ScopeAdmissionStep :=
  [.canonicalDistinctScopes, .fenceAndProbeEachScope, .lockSeries]

/-- The writes coordinated by a permitted typed sidecar insertion. -/
inductive SidecarWriteLeg where
  | typedRow
  | derivedProjection
  deriving DecidableEq

/-- A sidecar write uses one frozen entry and one transaction for both legs. -/
def sidecarWrite : List SidecarWriteLeg := [.typedRow, .derivedProjection]

/-- The data a surface contributes to an owner export. -/
inductive ExportProjection where
  | allFields
  | declaredFields
  | noRows
  deriving DecidableEq

/-- The direct bundle projection follows the surface's declared export policy. -/
def exportProjection : SurfaceExport → ExportProjection
  | .rows => .allFields
  | .allowlist => .declaredFields
  | .derivedFromCarried | .unresolvedDerivation | .excluded => .noRows

/-- The columns available from the owned current sidecar head. -/
inductive OwnedHeadColumn where
  | handle
  | exactVersion
  deriving DecidableEq

/-- The declared sidecar Memory key joins the current head under the current owner. -/
def ownedHeadColumns : List OwnedHeadColumn := [.handle, .exactVersion]

end Proxiima.Payloads
