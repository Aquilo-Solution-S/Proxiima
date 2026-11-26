/-! # Repository -/

namespace Proxiima.Repository

/-- Evidence used to decide which validation lanes run. -/
inductive ChangedPaths where
  | relevant
  | unrelated
  | unknown
  deriving DecidableEq

/-- Relevant and unknown diffs run the lane. -/
def validateLane : ChangedPaths → Bool
  | .relevant | .unknown => true
  | .unrelated => false

/-- The validation job's token permissions. -/
inductive ValidationGrant where
  | contentsRead
  | pullRequestsRead
  deriving DecidableEq

/-- Only change detection needs pull-request reads and every validation token has read-only permissions. -/
def validationGrants : (detectChanges : Bool) → List ValidationGrant
  | true => [.contentsRead, .pullRequestsRead]
  | false => [.contentsRead]

/-- The docs jobs and when they run. -/
inductive DocumentationLane where
  | markdownLinks
  | mkdocs
  | leanLedger
  deriving DecidableEq

/-- Every uncancelled validation checks Markdown links and audits the Lean ledger, while MkDocs builds on relevant changes or an unknown diff. -/
def validateDocs : DocumentationLane → ChangedPaths → Bool
  | .markdownLinks, .relevant | .markdownLinks, .unrelated | .markdownLinks, .unknown
  | .leanLedger, .relevant | .leanLedger, .unrelated | .leanLedger, .unknown => true
  | .mkdocs, .relevant | .mkdocs, .unknown => true
  | .mkdocs, .unrelated => false

/-- What uses the test pooler when it is configured. -/
inductive PostgresFixtureClient where
  | serving
  | migration
  | admin
  deriving DecidableEq

/-- Only serving clients are retargeted to the transaction pooler, preserving database and role. -/
def usesTestPooler : PostgresFixtureClient → Bool
  | .serving => true
  | .migration | .admin => false

/-- Verify accepts a candidate from main only after checking that it extends the previously verified history without merge commits. -/
def verifyRange : (main : Bool) → (extendsVerified : Bool) → (hasMerge : Bool) → Bool
  | true, true, false => true
  | true, true, true | true, false, true | true, false, false
  | false, true, true | false, true, false | false, false, true | false, false, false => false

/-- The references that move together to the tested commit after CI passes. -/
inductive VerifiedRef where
  | verifiedFastForward
  | nightlyLightweightTag
  deriving DecidableEq

/-- Failed CI leaves both references in place. -/
def advanceVerified : (ciPassed : Bool) → List VerifiedRef
  | true => [.verifiedFastForward, .nightlyLightweightTag]
  | false => []

/-- The numbered-release action after a candidate reaches verified. -/
inductive ReleaseAction where
  | refuseUnverified
  | skip
  | tagAndPublish
  deriving DecidableEq

/-- A new workspace version cuts one immutable matching tag and GitHub Release when Verify reaches that candidate. -/
def numberedRelease : (verified : Bool) → (tagExists : Bool) → ReleaseAction
  | true, false => .tagAndPublish
  | true, true => .skip
  | false, true | false, false => .refuseUnverified

/-- A matching verified tag can publish its notes through the same workflow for backfill or recovery. -/
def recoverRelease : (verified : Bool) → (tagMatchesWorkspace : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- The candidate's stable SemVer relationship to the canonical released tags fetched by CI. -/
inductive VersionStep where
  | initial
  | unchanged
  | nextPatch
  | nextMinor
  | nextMajor
  | invalid
  deriving DecidableEq

/-- Stable versions use canonical ASCII components and stay at the latest release or take one valid SemVer step. -/
def versionAccepted : VersionStep → Bool
  | .initial | .unchanged | .nextPatch | .nextMinor | .nextMajor => true
  | .invalid => false

/-- Breaking commits in the candidate range require one nonempty Upgrade footer whose continuation is retained in the rendered notes. -/
def renderUpgrade : (breaking : Bool) → (oneNonemptyFooter : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- Dependency input selected for the repository's allow or refuse decision. -/
inductive DependencyInput where
  | vulnerability
  | yank
  | unmaintainedWorkspaceDependency
  | disallowedLicense
  | unknownLicense
  | registryWildcard
  | repeatedVersion
  | workspacePath
  | cratesIo
  | undeclaredRegistry
  | undeclaredGit
  deriving DecidableEq

/-- Cargo-deny rejects forbidden inputs subject to its reviewed exceptions and warns about repeated versions. -/
inductive DependencyDisposition where
  | allow
  | warn
  | deny
  deriving DecidableEq

/-- Cargo-deny's configured enforcement for dependency inputs. -/
def dependencyPolicy : DependencyInput → DependencyDisposition
  | .workspacePath | .cratesIo => .allow
  | .repeatedVersion => .warn
  | .vulnerability | .yank | .unmaintainedWorkspaceDependency | .disallowedLicense |
    .unknownLicense | .registryWildcard | .undeclaredRegistry | .undeclaredGit => .deny

/-- What the test database harness does when its only admin URL setting is present or absent. -/
inductive AdminConfiguration where
  | connect
  | skip
  | refuse
  deriving DecidableEq

/-- A blank or absent admin URL is a visible skip only in a local test that opts into skipping. -/
def adminConfiguration : (configured : Bool) → (skippable : Bool) → (ci : Bool) → AdminConfiguration
  | true, true, true | true, true, false | true, false, true | true, false, false => .connect
  | false, true, false => .skip
  | false, true, true | false, false, true | false, false, false => .refuse

/-- The guard's decision on an isolated tracked test clone. -/
inductive CloneDisposition where
  | drop
  | keep
  | keepWithRedactedDiagnostic
  deriving DecidableEq

/-- Explicit retention preserves a clone, as does panic, which otherwise reports its redacted URL. -/
def cloneGuard : (keep : Bool) → (panics : Bool) → CloneDisposition
  | true, true | true, false => .keep
  | false, true => .keepWithRedactedDiagnostic
  | false, false => .drop

/-- Only stale clones without live clients are collectible. -/
def staleCloneCollectible : (stale : Bool) → (liveClient : Bool) → Bool
  | true, false => true
  | true, true | false, true | false, false => false

/-- Minimum age in seconds for stale test clones considered by the sweeper. -/
def cloneGraceSeconds : Nat := 300

/-- Attempt budget for test database operations retried because of connected clients. -/
def databaseAttempts : Nat := 25

/-- Delay in milliseconds between retryable test database attempts. -/
def databaseRetryMillis : Nat := 200

/-- Hex digits in a test template's fingerprint suffix. -/
def templateHashDigits : Nat := 16

/-- Template collection takes an exclusive lease and retains a database that has gained a client. -/
def collectTemplate : (leaseAvailable : Bool) → (liveClient : Bool) → Bool
  | true, false => true
  | true, true | false, true | false, false => false

/-- The store number initialized in a migrated host template. -/
def templateStore : Nat := 0

/-- Serving connections in the example engine fixture. -/
def exampleFixturePoolSize : Nat := 4

/-- The local development database URL used by the canonical CLI without explicit configuration. -/
def mcpDefaultDatabaseUrl : String := "postgres://postgres@localhost/proxiima_dev"

end Proxiima.Repository
