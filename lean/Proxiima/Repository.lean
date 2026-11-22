/-! # Repository -/

namespace Proxiima.Repository

/-- An outside contribution uses the CLA assistant on a new, reopened or updated main pull request, subject to its configured exemptions. -/
inductive ClaEvent where
  | opened
  | reopened
  | synchronize
  deriving DecidableEq

/-- Evidence used to decide which validation lanes run. -/
inductive ChangedPaths where
  | relevant
  | unrelated
  | unknown
  deriving DecidableEq

/-- Stable required job names are present on main pull requests, and an unknown diff runs the affected lanes. -/
def validateLane : ChangedPaths → Bool
  | .relevant | .unknown => true
  | .unrelated => false

/-- Source of the revision to validate. -/
inductive ValidationRun where
  | pullRequest
  | verify
  deriving DecidableEq

/-- How CI obtains the paths under review. -/
inductive PathSource where
  | pullRequestApi
  | verifiedToCandidate
  deriving DecidableEq

/-- Pull requests use the GitHub API and Verify diffs its tested commit against the last verified commit. -/
def pathSource : ValidationRun → PathSource
  | .pullRequest => .pullRequestApi
  | .verify => .verifiedToCandidate

/-- The validation job's token permissions. -/
inductive ValidationGrant where
  | contentsRead
  | pullRequestsRead
  deriving DecidableEq

/-- Only change detection needs pull-request reads and every validation token has read-only permissions. -/
def validationGrants : (detectChanges : Bool) → List ValidationGrant
  | true => [.contentsRead, .pullRequestsRead]
  | false => [.contentsRead]

/-- A newer PR revision supersedes the previous validation run. -/
def cancelSupersededValidation : ValidationRun → Bool
  | .pullRequest => true
  | .verify => false

/-- Only main verification saves Rust caches. -/
def saveRustCache : ValidationRun → Bool
  | .pullRequest => false
  | .verify => true

/-- Rust validation checks format, both feature lanes for Clippy, warning-free Rustdoc and workspace doctests. -/
inductive RustLane where
  | format
  | allFeaturesClippy
  | defaultClippy
  | rustdoc
  | doctest
  deriving DecidableEq

/-- Rust tests exercise the all-feature workspace and the default substrate host, running both lanes with `--run-ignored all`. -/
inductive TestFeatureLane where
  | allFeaturesWorkspace
  | defaultSubstrateHost
  deriving DecidableEq

/-- The public guide publishes the configured navigation and Mermaid diagrams and excludes local planning pages. -/
inductive PublicDocumentation where
  | configuredNavigation
  | mermaidFences
  | excludePlanningPages
  deriving DecidableEq

/-- The repository offers a blank issue and routes contribution and security reports to the documented entry points. -/
inductive IssueEntryPoint where
  | blankIssue
  | securityPolicy
  | contributionGuide
  deriving DecidableEq

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

/-- A local Markdown target must exist, a heading reference must resolve and a site link must stay within the docs directory. -/
inductive MarkdownLinkRefusal where
  | missingTarget
  | missingHeading
  | escapesDocs
  | siteDirectoryTarget
  deriving DecidableEq

/-- Validation builds the production-shaped image without publishing it and compares both OCI labels with the version and commit it tested. -/
inductive ImageEvidence where
  | build
  | workspaceVersion
  | testedRevision
  deriving DecidableEq

/-- The immutable references for third-party code selected by CI. -/
inductive WorkflowPin where
  | actionCommit
  | serviceImageDigest
  deriving DecidableEq

/-- PostgreSQL tests use a disposable pgvector server with reduced durability and an explicit admin endpoint. -/
inductive PostgresFixture where
  | pgvector
  | explicitAdmin
  | disposableDurability
  deriving DecidableEq

/-- Object integration tests use a versioned fixture bucket and separately probe the HTTP listener. -/
inductive ObjectFixture where
  | httpListenerResponds
  | versionedBucket
  | selectedObjectTests
  deriving DecidableEq

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

/-- The pooler lane checks its package signing key and version floor, exercises SCRAM and startup settings, and requires every selected test binary to log in through the pooler. -/
inductive PoolerVerification where
  | packageKey
  | versionFloor
  | scram
  | startupSettings
  | dedicatedSuites
  | hostBoot
  deriving DecidableEq

/-- Verify accepts a candidate from main only after checking that it extends the previously verified history without merge commits. -/
def verifyRange : (main : Bool) → (extendsVerified : Bool) → (hasMerge : Bool) → Bool
  | true, true, false => true
  | true, true, true | true, false, true | true, false, false
  | false, true, true | false, true, false | false, false, true | false, false, false => false

/-- Verify uses the selected main event's commit and serializes runs without cancelling work already in flight. -/
inductive VerificationTarget where
  | mainPush
  | mainDispatch
  deriving DecidableEq

/-- Each commit since verified needs a reviewer address outside its Git author and co-author addresses, on that commit or a later one in the candidate range. -/
def reviewCovers : (recordInSuffix : Bool) → (differentAddress : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- Review and co-author records use a final angle-bracket address compared case-insensitively. -/
inductive ReviewAddress where
  | trailingAddress
  | malformed
  deriving DecidableEq

/-- The references that move together to the tested commit after CI and review pass. -/
inductive VerifiedRef where
  | verifiedFastForward
  | nightlyLightweightTag
  deriving DecidableEq

/-- Failed CI or missing review leaves both references in place. -/
def advanceVerified : (ciPassed : Bool) → (reviewCovered : Bool) → List VerifiedRef
  | true, true => [.verifiedFastForward, .nightlyLightweightTag]
  | true, false | false, true | false, false => []

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

/-- Cargo workspace members inherit the workspace version, which the server reports from its build metadata. -/
inductive VersionSource where
  | workspaceManifest
  | cargoPackageVersion
  deriving DecidableEq

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

/-- GitHub publishes v0 tags as pre-releases. -/
def publishPrerelease : (majorZero : Bool) → Bool
  | true => true
  | false => false

/-- Release notes are rendered by git-cliff from Conventional Commits and are previewable offline. -/
inductive ReleaseNotes where
  | groupConventionalCommits
  | omitConfiguredCategories
  | renderOfflineLinks
  | publishGitHubRelease
  deriving DecidableEq

/-- Breaking commits in the candidate range require one nonempty Upgrade footer whose continuation is retained in the rendered notes. -/
def renderUpgrade : (breaking : Bool) → (oneNonemptyFooter : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

/-- Build-time workspace lint policy inherited by its Rust member crates. -/
inductive WorkspaceLint where
  | denyWarnings
  | denyUnsafe
  | denyMissingDebug
  | denyPedantic
  | allowNamedExceptions
  deriving DecidableEq

/-- Test-only schema and manifest helpers are dev dependencies, and the example dependency graph checks the SDK boundary. -/
inductive TestDependency where
  | boon
  | toml
  | exampleSdkBoundary
  deriving DecidableEq

/-- The selected TLS and JWT provider is AWS-LC and the object backend uses regional SigV4. -/
inductive CryptoProvider where
  | awsLcTls
  | awsLcJwt
  | regionalSigV4
  deriving DecidableEq

/-- The release Cargo profile uses thin LTO, one codegen unit, stripping and optimized build scripts. -/
inductive ReleaseProfile where
  | thinLto
  | singleCodegenUnit
  | strip
  | optimizeBuildScripts
  deriving DecidableEq

/-- The image builds the default substrate binary under the workspace compiler and lockfile, using matching Debian libraries and a non-root runtime. -/
inductive ImageBuild where
  | substrateBinary
  | workspaceCompiler
  | lockedDependencies
  | httpsPackageSources
  | matchingDebianLibraries
  | nonrootUser
  deriving DecidableEq

/-- Dependency policies run on dependency changes, with a separate scheduled and manual advisory check. -/
inductive DependencyCheck where
  | advisories
  | licenses
  | bans
  | sources
  deriving DecidableEq

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

/-- The disposable development services expose configurable ports on loopback. -/
inductive LoopbackFixture where
  | postgres
  | objectStore
  deriving DecidableEq

/-- Existing-volume bootstrap refuses while a development serving or platform client is connected to the target database. -/
def bootstrapPreflight : (knownClient : Bool) → Bool
  | false => true
  | true => false

/-- Shared core test fixtures are exported only in tests or with the testkit feature and use the public authentication and tool-context entry points. -/
inductive CoreFixture where
  | sealedAuthentication
  | ordinaryToolContext
  deriving DecidableEq

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

/-- Each clone name carries a UUIDv7, with the prefix truncated to fit PostgreSQL's identifier limit. -/
inductive CloneNamePart where
  | boundedPrefix
  | uuidV7
  deriving DecidableEq

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

/-- Failed registration attempts cleanup and reports a cleanup failure. Fixture-named survivors can be swept without a record. -/
inductive CloneRegistration where
  | track
  | cleanUpAfterFailure
  | reportCleanupFailure
  deriving DecidableEq

/-- The sweeper considers stale tracked and fixture-named untracked clones and lets PostgreSQL recheck for live clients at deletion. -/
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

/-- A custom template uses a valid bounded prefix disjoint from built-in and staging families and owns only names with its exact hexadecimal suffix. -/
inductive TemplateFamilyRule where
  | boundedPrefix
  | noReservedOverlap
  | exactHashSuffix
  deriving DecidableEq

/-- A validated test template is staged under a build lock, published only on success and leased until its clone exists. -/
inductive TemplateLifecycle where
  | lockBuild
  | buildStaging
  | publishSuccess
  | cleanUpFailure
  | leaseThroughClone
  | collectIdlePrevious
  deriving DecidableEq

/-- Template collection takes an exclusive lease and retains a database that has gained a client. -/
def collectTemplate : (leaseAvailable : Bool) → (liveClient : Bool) → Bool
  | true, false => true
  | true, true | false, true | false, false => false

/-- The validated host lineup and its length-framed inputs define its template fingerprint. -/
inductive TemplateFingerprintInput where
  | hostFingerprint
  | vectorStorageKind
  | definitions
  | release
  | storeNumber
  | searchDefinitions
  | migrationSources
  | spiiceSchemas
  | followsTables
  deriving DecidableEq

/-- The store number initialized in a migrated host template. -/
def templateStore : Nat := 0

/-- Serving connections in the example engine fixture. -/
def exampleFixturePoolSize : Nat := 4

/-- A split-role clone runs migrations as platform and serves through a non-owning runtime role subject to owner row security. -/
inductive CloneRole where
  | platformMigration
  | scopedRuntime
  deriving DecidableEq

/-- The owner kind a single-owner test context acts in. -/
inductive FixtureOwner where
  | personal
  | group
  deriving DecidableEq

/-- Who a sealed single-owner test context acts as. -/
inductive FixtureSubject where
  | ownUser
  | freshAdmin
  deriving DecidableEq

/-- A personal fixture acts as its own user; a Group fixture acts as a fresh admin of that Group. -/
def fixtureSubject : FixtureOwner → FixtureSubject
  | .personal => .ownUser
  | .group => .freshAdmin

/-- The spiice trigger test refuses a failed registration, an empty artifact set or a generated artifact absent from its migrations. -/
inductive TriggerFixtureRefusal where
  | invalidRegistration
  | emptyArtifacts
  | missingArtifact
  deriving DecidableEq

/-- Connection exhaustion diagnostics use the admin connection to describe the server's connection budget. -/
inductive ConnectionDiagnostic where
  | exhaustionClassification
  | adminConnectionBudget
  deriving DecidableEq

/-- The local development database URL used by the canonical CLI without explicit configuration. -/
def mcpDefaultDatabaseUrl : String := "postgres://postgres@localhost/proxiima_dev"

/-- The development issuer admits a loopback bind and nonblank normalized subject and audience. -/
inductive DevIssuerInput where
  | loopbackBind
  | fixedPort
  | nonblankSubject
  | nonblankAudience
  | subjectDerivedUser
  deriving DecidableEq

/-- The development issuer loads a persisted signer, links a whole private key into place when the file is missing, reads the winning file after a link conflict, or uses an in-memory signer in ephemeral mode. -/
inductive DevSigner where
  | loadExisting
  | createExclusively
  | loadConcurrentWinner
  | ephemeral
  deriving DecidableEq

/-- The development issuer publishes one RSA JWK and signs timed RS256 claims, refusing clocks before the Unix epoch. -/
inductive DevClaims where
  | rsaJwk
  | rs256
  | issuer
  | audience
  | subject
  | expiry
  | validEpoch
  deriving DecidableEq

end Proxiima.Repository
