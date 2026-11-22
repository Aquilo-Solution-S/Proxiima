/-! # Database authority -/

namespace Proxiima.DatabaseAuthority

/-- The platform role owns relations, while the serving role uses owner-scoped access under RLS. -/
inductive DatabaseRole where
  | platform
  | runtime
  deriving DecidableEq

/-- Serving admission checks the role split and the live relation and definer inventory after schema validation. -/
inductive ServingAdmission where
  | validateSchema
  | validatePlatform
  | validateRuntime
  deriving DecidableEq

/-- Validation precedes handing out the serving storage. -/
def servingAdmissionOrder : List ServingAdmission := [.validateSchema, .validatePlatform, .validateRuntime]

/-- Platform transactions must match the validated database and effective role and retain their restricted role attributes. -/
inductive PlatformBindingCheck where
  | database
  | effectiveRole
  | roleAttributes
  deriving DecidableEq

/-- System writes require a witness minted by consuming the engine and bound to that engine's boot. -/
inductive SystemWriteAuthority where
  | consumeEngine
  | matchBootWitness
  deriving DecidableEq

/-- Data access axes in ordinary owner RLS, independent of administration. -/
inductive OwnerDataAccess where
  | memoryRead
  | memoryWrite
  | goalRead
  | goalWork
  deriving DecidableEq

/-- An owner transaction sets its sealed subject's memory ceilings and Goal access for that transaction only. -/
inductive OwnerTransactionScope where
  | data (access : OwnerDataAccess)
  | goalSubject
  | ownerManagement
  deriving DecidableEq

/-- Authority sessions use READ COMMITTED and fence and recheck a declared footprint; newly discovered targets require a new transaction. -/
inductive AuthoritySessionRule where
  | readCommitted
  | declareFootprintBeforeHostLocks
  | fenceAndRecheck
  | retryWithNewTargets
  deriving DecidableEq

/-- Scoped SPIICE reads reuse the caller's transaction; version and receipt locks last through it, and a shared memory lock requires write authority. -/
inductive ScopedRead where
  | readInOwnerTransaction
  | lockReceiptAdmission
  | lockVersionWithWriteAuthority
  deriving DecidableEq

/-- Core views invoke caller RLS and receive SELECT-only serving grants. -/
inductive CoreViewRule where
  | invokerSecurity
  | runtimeSelectOnly
  deriving DecidableEq

/-- Core Memory SQL declares whether it includes forgotten rows; direct reads are reserved for row locks and policy or view definitions. -/
inductive MemorySqlRoute where
  /-- The live route uses proxiima_core.live_memory. -/
  | liveView
  /-- The inclusive route uses proxiima_core.memory_including_forgotten. -/
  | includingForgottenView
  | spineWrite
  | spineRowLock
  | spineDefinition
  deriving DecidableEq

/-- The production Rust SQL guardrail scans outside file-level and inline-module test code. -/
inductive MemoryScanRegion where
  | productionItems
  | excludeTestFiles
  | excludeTestModules
  deriving DecidableEq

/-- The SQL composition boundary uses checked identifiers and program literals, binds request data, and confines raw SQLx composition to one module. -/
inductive SqlComposition where
  | checkedIdentifier
  | programLiteral
  | boundRequestData
  | trustedFragment
  | confinedSqlxEscape
  deriving DecidableEq

/-- Tables take one or two unquoted parts, other unquoted identifiers take one, and quoted catalog names escape quotes and refuse empty, NUL or oversized values. -/
inductive SqlIdentifierForm where
  | unquotedTable
  | unquotedName
  | quotedCatalogName
  deriving DecidableEq

/-- The PostgreSQL byte limit for each identifier part. -/
def maxIdentifierBytes : Nat := 63

/-- Program literals double quotes, use escape strings for backslashes, refuse NUL and nonfinite floats, and preserve backslashes in composed statement literals. -/
inductive SqlLiteralForm where
  | text
  | escapeText
  | finiteFloat
  | composedStatement
  deriving DecidableEq

/-- Template replacement scans the original left to right, prefers the longest matching key at a position, and refuses empty keys. -/
inductive SqlTemplateFill where
  | leftmost
  | longestAtEqualPosition
  | skipReplacementText
  | refuseEmptyKeys
  deriving DecidableEq

/-- Migration installs and refreshes classified, forced owner RLS; the serving census rejects missing, unclassified or drifted policy state. -/
inductive OwnerPolicyCheck where
  | relationClassification
  | enabledAndForced
  | recordedPolicyShape
  deriving DecidableEq

/-- Follows sidecars stamp the owning Memory's owner and require a recorded policy, live-memory check and leading owner index. -/
inductive FollowsPolicy where
  | stampOwner
  | checkLiveMemoryAndOwner
  | requirePolicyAndIndex
  deriving DecidableEq

/-- Scoped database reader functions have dedicated read-only owners with execution granted explicitly and withheld from PUBLIC; candidate results are admitted under ordinary visibility. -/
inductive ScopedReader where
  | searchCandidates
  | goalVisibility
  deriving DecidableEq

/-- Serving transactions keep settings and locks transaction-local; connection startup omits the float-format parameter. -/
inductive PoolerCompatibility where
  | localSettings
  | transactionLocks
  | noSessionCommands
  | omitFloatFormat
  deriving DecidableEq

/-- The connection chosen by the database operation. -/
inductive DatabaseOperation where
  | serve
  | operateAsPlatform
  | migrate
  | validateSchema
  deriving DecidableEq

/-- The runtime, platform and migration pools each use the resolved per-pool configuration. -/
inductive ConnectionPurpose where
  | runtime
  | platform
  | migration
  deriving DecidableEq

/-- Migration and schema validation share the direct or session-mode migration connection policy. -/
def connectionPurpose : DatabaseOperation → ConnectionPurpose
  | .serve => .runtime
  | .operateAsPlatform => .platform
  | .migrate | .validateSchema => .migration

/-- Migration, boot and maintenance require the runtime URL, platform URL and store number. -/
inductive RequiredStorageSetting where
  | runtimeUrl
  | platformUrl
  | storeNumber
  deriving DecidableEq

/-- The migration URL defaults to the platform URL; an override must keep its database and role. -/
inductive MigrationConnection where
  | platformDefault
  | sameDatabaseAndRoleOverride
  deriving DecidableEq

/-- Connection diagnostics omit the URL. -/
inductive ConnectionDiagnostic where
  | invalidUrl
  | migrationIdentityMismatch
  | redactedConfig
  deriving DecidableEq

/-- Boot freezes the registry and validates configuration and the embedding route before connecting storage. -/
inductive PreconnectionCheck where
  | configurationAndListenerAllowlists
  | freezeRegistry
  | embeddingRoute
  deriving DecidableEq

/-- The lowest usable pool size. -/
def minPoolConnections : Nat := 1

/-- The default number of connections per pool. -/
def defaultPoolConnections : Nat := 5

/-- The default statement timeout in milliseconds. -/
def defaultStatementTimeoutMs : Nat := 300000

/-- The statement timeout value that omits the pool's startup option. -/
def omitStatementTimeoutMs : Nat := 0

/-- The default acquisition timeout in seconds. -/
def defaultAcquireTimeoutSecs : Nat := 5

/-- The default idle timeout in seconds. -/
def defaultIdleTimeoutSecs : Nat := 600

/-- The default connection lifetime in seconds. -/
def defaultMaxLifetimeSecs : Nat := 1800

/-- The storage block being overlaid. -/
inductive StorageOverlay where
  | pool
  | tuning
  deriving DecidableEq

/-- The source's contribution after parsing, relative to the shipped defaults. -/
inductive OverlayInput where
  | unset
  | explicitDefault
  | nonDefault
  deriving DecidableEq

/-- Pool overrides use setting presence; tuning overrides use a resolved value different from the default. -/
def overlaySupplied : StorageOverlay → OverlayInput → Bool
  | .pool, .unset | .tuning, .unset | .tuning, .explicitDefault => false
  | .pool, .explicitDefault | .pool, .nonDefault | .tuning, .nonDefault => true

/-- The advisory lock key serializing the platform grant phase. -/
def runtimeGrantLockKey : Nat := 0x70726f7867726e74

/-- Runtime grants are opt-in on the migration path. -/
def defaultRuntimeGrants : Bool := false

/-- The opt-in grant phase starts after schema migration and store-number initialization and applies grants in one locked platform transaction. -/
inductive GrantPhaseStep where
  | migrateSchema
  | initializeStore
  | grantInLockedTransaction
  deriving DecidableEq

/-- The platform applies opt-in grants after both database preparation steps succeed. -/
def grantPhaseOrder : List GrantPhaseStep := [.migrateSchema, .initializeStore, .grantInLockedTransaction]

/-- Runtime grants target the runtime URL role only after confirming it differs from the platform URL role. -/
inductive GrantRoleCheck where
  | runtimeUrlRole
  | distinctPlatformUrlRole
  deriving DecidableEq

/-- Existing composed schemas grant usage, table DML and sequence access with schema-scoped defaults; the phase grants neither ownership nor role membership and revokes runtime and public CREATE. -/
inductive SchemaGrant where
  | schemaUsage
  /-- SELECT, INSERT, UPDATE and DELETE. -/
  | tableDml
  /-- USAGE, SELECT and UPDATE. -/
  | sequenceAccess
  | scopedDefaults
  | revokeCreate
  deriving DecidableEq

/-- Core views end SELECT-only after broad table grants, and migration ledgers grant runtime SELECT with schema usage only as needed. -/
inductive ReadOnlyGrant where
  | coreViews
  | migrationLedgers
  deriving DecidableEq

/-- Reader grants cover the composed schemas and switch to the function owner only for the grant before restoring the platform role. -/
inductive ScopedReaderGrantStep where
  | selectComposedReader
  | assumeOwnerLocally
  | grantExecution
  | restorePlatformRole
  deriving DecidableEq

/-- The order of one isolated reader grant. -/
def scopedReaderGrantOrder : List ScopedReaderGrantStep :=
  [.selectComposedReader, .assumeOwnerLocally, .grantExecution, .restorePlatformRole]

/-- Bootstrap transfers table ownership before dependent sequence ownership. -/
inductive BootstrapOwnershipStep where
  | relations
  | sequences
  deriving DecidableEq

/-- The PostgreSQL object-ownership replay order. -/
def bootstrapOwnershipOrder : List BootstrapOwnershipStep := [.relations, .sequences]

/-- Engine storage is required at construction; the shipped backend is PostgreSQL, and refuse-all storage is test-only. -/
inductive EngineStorage where
  | requiredPorts
  | postgresBackend
  | testOnlyRefusal
  deriving DecidableEq

/-- Storage assembly reports all missing required ports together; erase authority and personal-drop proof are optional. -/
inductive StoragePortRequirement where
  | collectMissingRequired
  | optionalEraseAuthority
  | optionalPersonalDropProof
  deriving DecidableEq

/-- PostgreSQL bindings distinguish catalog enums even where their underlying domain types share cases. -/
inductive PostgresEnumBinding where
  | memoryKind
  | interpretationSubjectKind
  | sketchKind
  | pinTargetKind
  | publicationDeliveryState
  | publicationRecordState
  deriving DecidableEq

/-- The external storage outcomes whose protocol category differs by operation. -/
inductive StorageOutcome where
  | conflictOrConstraint
  | exhaustedTransient
  deriving DecidableEq

/-- A read or write returning a storage failure. -/
inductive StorageVerb where
  | read
  | write
  deriving DecidableEq

/-- Public categories for the classified storage failures. -/
inductive ProtocolFailure where
  | invalidArgument
  | internal
  deriving DecidableEq

/-- Reads collapse storage failures to internal; writes distinguish caller-fixable conflicts and constraints from exhausted retries. -/
def storageErrorCategory : StorageVerb → StorageOutcome → ProtocolFailure
  | .write, .conflictOrConstraint => .invalidArgument
  | .read, .conflictOrConstraint | .read, .exhaustedTransient | .write, .exhaustedTransient => .internal

/-- A SQLSTATE reporting a caller-facing storage refusal with a rule in the constraint field. -/
def ruleSqlstate : String := "PX001"

/-- A SQLSTATE reporting an internal storage backstop without a caller-facing rule. -/
def backstopSqlstate : String := "PX002"

/-- PostgreSQL SQLSTATEs that allow the transaction to be replayed after rollback. -/
def transientSqlstates : List String := ["40P01", "40001", "55P03"]

/-- Database fault classes have different storage routes. -/
inductive SqlFailureRoute where
  | typedRule
  | retryWholeTransaction
  | internalBackstop
  deriving DecidableEq

end Proxiima.DatabaseAuthority
