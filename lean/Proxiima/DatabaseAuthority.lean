/-! # Database authority -/

namespace Proxiima.DatabaseAuthority

/-- Serving admission checks the role split and the live relation and definer inventory after schema validation. -/
inductive ServingAdmission where
  | validateSchema
  | validatePlatform
  | validateRuntime
  deriving DecidableEq

/-- Validation precedes handing out the serving storage. -/
def servingAdmissionOrder : List ServingAdmission := [.validateSchema, .validatePlatform, .validateRuntime]

/-- The PostgreSQL byte limit for each identifier part. -/
def maxIdentifierBytes : Nat := 63

/-- The connection chosen by the database operation. -/
inductive DatabaseOperation where
  | serve
  | operateAsPlatform
  | migrate
  | validateSchema
  deriving DecidableEq

/-- The runtime, platform and migration pools use their resolved per-pool settings at startup. -/
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

/-- The advisory lock key serializing the platform grant phase is ASCII `proxgrnt` interpreted as a big-endian integer. -/
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

end Proxiima.DatabaseAuthority
