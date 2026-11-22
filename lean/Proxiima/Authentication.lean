import Proxiima.OwnerAuthority

/-! # Authentication -/

namespace Proxiima.Authentication

/-- The required checks before returning claims from an OIDC token. -/
inductive TokenCheck where
  | keyFamilySignature | exactIssuer | audienceMembership | expiry | subjectString | presentNotBefore

/-- A single verified payload supplies both the registered claims and the host's typed claims. -/
def tokenChecks : List TokenCheck :=
  [.keyFamilySignature, .exactIssuer, .audienceMembership, .expiry, .subjectString, .presentNotBefore]

/-- Verification key families considered by the JWT validator. -/
inductive KeyFamily where
  | rsa | p256 | ed25519 | sharedSecret

/-- The resolved key family determines the accepted signature algorithms. -/
def signatureAlgorithms : KeyFamily → List String
  | .rsa => ["RS256", "RS384", "RS512"]
  | .p256 => ["ES256"]
  | .ed25519 => ["EdDSA"]
  | .sharedSecret => []

/-- The short RFC 9068 access-token media type. -/
def shortAccessTokenType : String := "at+jwt"

/-- The full RFC 9068 access-token media type. -/
def fullAccessTokenType : String := "application/at+jwt"

/-- Access-token media types, matched without ASCII case distinctions. -/
def accessTokenTypes : List String := [shortAccessTokenType, fullAccessTokenType]

/-- The generic JWT media type, matched without ASCII case distinctions. -/
def genericTokenType : String := "JWT"

/-- The token type classified before key resolution. -/
inductive TokenType where
  | absentOrGeneric | access | other

/-- Strict mode accepts access-token types alone, while compatibility mode also accepts absent and generic types. -/
def admitTokenType : (strict : Bool) → TokenType → Bool
  | true, .access | false, .access | false, .absentOrGeneric => true
  | true, .absentOrGeneric | true, .other | false, .other => false

/-- Whether strict access-token type checking is enabled by default. -/
def requireAccessTokenType : Bool := false

/-- The credential refusal exposed by a bearer authenticator. -/
inductive PublicRefusal where
  | invalidCredentials

/-- A token check's detailed refusal is internal to the host. -/
def publicTokenRefusal : PublicRefusal := .invalidCredentials

/-- URL classes used to validate issuer and key endpoints. -/
inductive Transport where
  | https | loopbackHttp | other

/-- Issuer URLs permit paths and exclude query and fragment delimiters. -/
def issuerTransport : Transport → Bool
  | .https | .loopbackHttp => true
  | .other => false

/-- Configured, discovered and redirected key endpoints use HTTPS, or loopback HTTP when their issuer is also loopback. -/
def keyTransport : (loopbackIssuer : Bool) → Transport → Bool
  | true, .https | false, .https | true, .loopbackHttp => true
  | false, .loopbackHttp | true, .other | false, .other => false

/-- The URL boundary checked before reading the subject map or opening storage. -/
inductive BootstrapUrl where
  | publicUrl | issuer | explicitKeyEndpoint

/-- The public HTTP(S) URL requires a host and excludes credentials, query and fragment; URL diagnostics omit its input. -/
def bootstrapUrlChecks : List BootstrapUrl := [.publicUrl, .issuer, .explicitKeyEndpoint]

/-- The setting categories an operator may supply with an issuer. -/
inductive IssuerCompanion where
  | audience | publicUrl | subjectMap | keyUri | inlineKeys | timeout | subjects | strictType

/-- An environment issuer requires a resource audience, a public URL and an issuer-aware subject map. -/
def requiredIssuerCompanions : List IssuerCompanion := [.audience, .publicUrl, .subjectMap]

/-- The remaining issuer settings may be omitted. -/
def optionalIssuerCompanions : List IssuerCompanion := [.keyUri, .inlineKeys, .timeout, .subjects, .strictType]

/-- The source of the public verification keys. -/
inductive KeySource where
  | discovery | endpoint | inlineDocument | conflict

/-- A pinned document requires no fetch; explicit endpoints skip discovery, and the two overrides exclude each other. -/
def keySource : (uri document : Bool) → KeySource
  | false, false => .discovery
  | true, false => .endpoint
  | false, true => .inlineDocument
  | true, true => .conflict

/-- Discovery accepts the key endpoint only if the response names the configured issuer exactly. -/
def discoveredIssuerAgrees : (same : Bool) → Option KeySource
  | true => some .endpoint
  | false => none

/-- How a JWKS document reached the resolver. -/
inductive KeyDocument where
  | pinned | fetched

/-- Handling of an invalid entry in a JWKS document. -/
inductive InvalidKeyEntry where
  | refuseDocument | skipEntry

/-- Pinned keys must be named, distinct, supported public signature keys; remote parsing keeps usable siblings and tolerates recoverable key-type spelling. -/
def invalidKeyEntry : KeyDocument → InvalidKeyEntry
  | .pinned => .refuseDocument
  | .fetched => .skipEntry

/-- A resolver's view of a requested key before a refresh attempt. -/
inductive CachedKey where
  | fresh | aging | absent

/-- What a lookup does at the refresh gate. -/
inductive CacheAction where
  | returnCached | tryRefresh | waitForRefresh

/-- Aging hits may use an incumbent during refresh; absent or untrusted keys wait for the gate. -/
def cachedKeyLookup : CachedKey → CacheAction
  | .fresh => .returnCached
  | .aging => .tryRefresh
  | .absent => .waitForRefresh

/-- Minimum seconds between JWKS fetch attempts, including failed attempts. -/
def refreshCooldownSeconds : Nat := 60

/-- Age in seconds of a successful JWKS fetch at which cached keys become refreshable. -/
def freshKeySeconds : Nat := 3600

/-- Age in seconds of a successful JWKS fetch beyond which cached keys are no longer trusted. -/
def trustedKeySeconds : Nat := 14400

/-- Default complete-request timeout in seconds for each discovery or JWKS request. -/
def defaultKeyRequestSeconds : Nat := 10

/-- Maximum complete-request timeout in seconds for each discovery or JWKS request. -/
def maxKeyRequestSeconds : Nat := 300

/-- Maximum bytes read from each discovery or JWKS response. -/
def maxKeyResponseBytes : Nat := 1048576

/-- Byte width of P-256 coordinates and Ed25519 public keys. -/
def keyCoordinateBytes : Nat := 32

/-- The stock validator's JWT clock tolerance in seconds. -/
def clockSkewSeconds : Nat := 60

/-- The key by which verified issuer and subject resolve a principal; empty keys, duplicate bindings and malformed records are refused. -/
structure SubjectBinding where
  issuer : String
  subject : String
  principal : String
  runner : Option String

/-- The allowlist operates on the verified subject before principal mapping, with an omitted list leaving mapping alone. -/
inductive SubjectAdmission where
  | allow | refuse

/-- A configured allowlist that names no subjects is refused at setting intake. -/
def subjectAdmission : (allowlistPresent subjectListed : Bool) → SubjectAdmission
  | false, false | false, true | true, true => .allow
  | true, false => .refuse

/-- The presented authorized party after reading the validated token's audience and exact nonempty route allowset. -/
inductive PartyCheck where
  | missingSingleAudience | missingMultipleAudiences | listed | unlisted | malformed

/-- A set route policy accepts a listed party or an omitted party with a single distinct audience. -/
def admitAuthorizedParty : PartyCheck → Bool
  | .listed | .missingSingleAudience => true
  | .unlisted | .missingMultipleAudiences | .malformed => false

/-- A readable unsigned issuer prunes verification candidates; the route set is nonempty with unique issuer and audience pairs. -/
inductive BindingCandidates where
  | issuerMatches | allRoutes

/-- Which routes to verify before requiring exactly one match. -/
def bindingCandidates : (issuerReadable : Bool) → BindingCandidates
  | true => .issuerMatches
  | false => .allRoutes

/-- What a policy may do to a credential-bound authorization field. -/
inductive ShaperAction where
  | preserve | narrow

/-- Fields a host role shaper receives after verification and initial resolution. -/
inductive ShapedField where
  | identity | writer | path | runner | requiredOwner | roles | tools | expiry

/-- Host shaping preserves attribution and owner confinement, and can only reduce rights, tool scope and time. -/
def hostShape : ShapedField → ShaperAction
  | .identity | .writer | .path | .runner | .requiredOwner => .preserve
  | .roles | .tools | .expiry => .narrow

/-- When optional verified payload context is preserved. -/
inductive ClaimsCarry where
  | clone | narrowToOwner

/-- Verified payload context is immutable and carries neither bearer bytes nor authorization. -/
def verifiedContext : List ClaimsCarry := [.clone, .narrowToOwner]

/-- Header context is constructed at request intake from the host's selected names. -/
inductive HeaderExposure where
  | allowlistedRequestHeaders

/-- A service may read only the header names selected by its host. -/
def requestHeaderExposure : HeaderExposure := .allowlistedRequestHeaders

/-- Each transport request verifies its own credential and any session must agree with the current principal and owner. -/
inductive SessionAuthorityCheck where
  | authenticateRequest | agreeWithPrincipal | agreeWithOwner

/-- A transport session binds selection, while authority comes from each request. -/
def sessionChecks : List SessionAuthorityCheck :=
  [.authenticateRequest, .agreeWithPrincipal, .agreeWithOwner]

/-- The consumers that resolve roles through the runtime's once-bound owner access port. -/
inductive OwnerResolverConsumer where
  | mcpEdge | environmentOidc | delegation | platformFactory

/-- Serving binds one owner access port before any consumer uses it. -/
def ownerResolverConsumers : List OwnerResolverConsumer :=
  [.mcpEdge, .environmentOidc, .delegation, .platformFactory]

/-- A layer may set neither, either or both authenticator forms. -/
inductive AuthenticatorForms where
  | neither | direct | factory | both

/-- Resolution keeps each layer's supplied form, rejects a selected layer with both forms and runs a selected factory after platform admission. -/
inductive AuthenticatorSlot where
  | inherit | useDirect | buildWithPlatform | conflict

/-- How the selected layer supplies authentication. -/
def authenticatorSlot : AuthenticatorForms → AuthenticatorSlot
  | .neither => .inherit
  | .direct => .useDirect
  | .factory => .buildWithPlatform
  | .both => .conflict

/-- Credential routes available to a verified request. -/
inductive CredentialRoute where
  | membership | forwarded

/-- Ordinary requests take only membership; a configured forwarder uses a verified exact owner and writer claim before any owner role is resolved. -/
def requestRoleSource : (forwarder : Bool) → CredentialRoute
  | false => .membership
  | true => .forwarded

/-- The non-managing role names admitted with a nonempty set of forwarder subjects. -/
inductive ForwardingRole where
  | viewer | ingest | editor
  deriving DecidableEq

/-- Forwarded roles may read Goals and have neither Goal work, owner administration nor group management. -/
def forwardingCap : Proxiima.OwnerAuthority.RolePreset :=
  ⟨.perspective, .perspective, .reads, false, false, rfl⟩

/-- A forwarded request keeps its subject, verifies owner agreement and resolves only the claim's registered personal or group owner at the configured capped role. -/
inductive ForwardingCheck where
  | verifiedIdentityAndWriter | exactOwner | liveCredential | configuredCeiling | registeredOwner

/-- Membership roles are never unioned with a forwarded role. -/
def forwardingChecks : List ForwardingCheck :=
  [.verifiedIdentityAndWriter, .exactOwner, .liveCredential, .configuredCeiling, .registeredOwner]

/-- The credential-free boot witness confines a synthetic host bearer to its personal owner. -/
inductive ScopeSource where
  | verifiedCredential | singleOwnerBoot

/-- Verified credential admission seals owner roles, deadline and writer for engine operations; ordinary contexts carry no witness. -/
def scopeSources : List ScopeSource := [.verifiedCredential, .singleOwnerBoot]

/-- Only engine gates construct read and write permits from their checked authority. -/
inductive EnginePermit where
  | entryRead | write

/-- The engine permit families subject to `Proxiima.OwnerAuthority.ScopeRestriction.expiry`. -/
def enginePermits : List EnginePermit := [.entryRead, .write]

/-- Largest credential-attested model label in Unicode characters after trimming; blank labels are refused. -/
def maxTrustedModelChars : Nat := 120

/-- The caller's normalized model label relative to any attestation. -/
inductive ModelClaim where
  | absent | matching | supplied

/-- The effective provenance label or a refusal. -/
inductive ModelLabelSource where
  | credential | caller | unknown | conflict

/-- Verified model attestation survives narrowing; a different explicit label is refused and a blank label is omitted. -/
def modelLabelSource : (attested : Bool) → ModelClaim → ModelLabelSource
  | true, .absent | true, .matching => .credential
  | true, .supplied => .conflict
  | false, .absent => .unknown
  | false, .matching | false, .supplied => .caller

/-- Maximum validated host run attributes carried with Fact capture; binds merge without clearing and refuse duplicate names. -/
def maxHostRunExtensions : Nat := 8

/-- Default maximum lifetime in seconds of authenticated streams. -/
def defaultStreamLifetimeSeconds : Nat := 3600

/-- Conditions observed by the generic wrapper, which has no live revocation probe. -/
inductive StreamObservation where
  | beforeDeadlines | credentialExpired | lifetimeElapsed | sourceEnded

/-- A wrapped stream's observable state. -/
inductive StreamState where
  | open | ended

/-- The first observed expiry or positive configured lifetime deadline ends the stream silently, including while its source is ready. -/
def streamLifetime : StreamState → StreamObservation → StreamState
  | .open, .beforeDeadlines => .open
  | .open, .credentialExpired | .open, .lifetimeElapsed | .open, .sourceEnded
  | .ended, .beforeDeadlines | .ended, .credentialExpired | .ended, .lifetimeElapsed | .ended, .sourceEnded => .ended

/-- The OAuth metadata selected by a request path, with the resource URL independent of token audience. -/
inductive ProtectedResource where
  | origin | mcp

/-- The resource metadata URL used by an origin or MCP authentication challenge. -/
def metadataPath : ProtectedResource → String
  | .origin => "/.well-known/oauth-protected-resource"
  | .mcp => "/.well-known/oauth-protected-resource/mcp"

/-- Inclusive ASCII byte bounds for each byte of a nonempty OAuth scope token. -/
def scopeTokenByteBounds : List Nat := [33, 126]

/-- The quote and backslash bytes excluded from an OAuth scope token. -/
def scopeTokenExcludedBytes : List Nat := [34, 92]

/-- Advertise the built-in sign-in and refresh scopes first, then unique validated extra scopes in order. -/
def defaultScopes : List String := ["openid", "offline_access"]

/-- Flows advertised for the stateless public client, which uses the upstream sign-in and token endpoints. -/
def publicGrantTypes : List String := ["authorization_code", "refresh_token"]

/-- Requests published only in public-client facade deployments. -/
inductive FacadeRoute where
  | authorizationMetadata | registration

/-- Facade metadata and registration return the same configured public client; other deployments publish neither. -/
def publicClientRoutes : List FacadeRoute := [.authorizationMetadata, .registration]

/-- Registration takes at least one absolute redirect URL and public token endpoint authentication. The provider admits redirects. -/
inductive RegistrationCheck where
  | nonemptyAbsoluteRedirects | publicAuthentication

/-- An omitted registration auth method defaults to public authentication. -/
def registrationChecks : List RegistrationCheck := [.nonemptyAbsoluteRedirects, .publicAuthentication]

/-- A registered delegated command is a flat tool or a dispatcher leaf, with serialized scope derived from the typed command. -/
inductive DelegatedCommand where
  | flat | dispatcherLeaf

/-- The two command kinds admitted for delegation, excluding resources and bare dispatchers. -/
def delegatedCommands : List DelegatedCommand := [.flat, .dispatcherLeaf]

/-- Maximum delegated tool identifier length in characters. -/
def maxDelegatedCommandChars : Nat := 200

/-- The service a runtime can publish for queued delegated commands. -/
inductive AuthService where
  | delegation

/-- Authentication is required to publish the service for issuing and redeeming delegation grants. -/
def publishDelegationService : (authenticatorPresent : Bool) → Option AuthService
  | true => some .delegation
  | false => none

/-- A queued job stores an opaque redacted UUIDv7 grant handle and no bearer. -/
structure GrantHandle where
  id : String

/-- Issuance checks recorded before minting a grant for a finite verified host bearer. -/
inductive GrantIssueCheck where
  | finiteVerifiedBearer | currentMembership | sourceRole | nonManagingCeiling | registeredCommand
  | callerScope | deployment | sourceExpiry | attestedWriter

/-- Issuance bounds the exact command and selected owner by the bearer, current membership and deployment policy. -/
def grantIssueChecks : List GrantIssueCheck :=
  [.finiteVerifiedBearer, .sourceRole, .sourceExpiry, .nonManagingCeiling, .currentMembership,
   .registeredCommand, .callerScope, .deployment, .attestedWriter]

/-- Admission checks performed anew at each delegated phase. -/
inductive GrantRedemptionCheck where
  | exactOwnerAndCommand | expiry | revocation | callerProfile | currentMembership

/-- Each new phase retains the recorded role ceiling and exact tool scope after fresh redemption. -/
def grantRedemptionChecks : List GrantRedemptionCheck :=
  [.exactOwnerAndCommand, .revocation, .expiry, .callerProfile, .currentMembership]

/-- Who may revoke a grant after proving current bearer membership in its owner. -/
inductive GrantRevoker where
  | issuer | ownerManager

/-- Revocation is idempotent and blocks future redemption; an admitted phase retains its finite expiry. -/
def grantRevokers : List GrantRevoker := [.issuer, .ownerManager]

/-- A noncloneable phase is checked at the engine against its originating runtime, finite expiry, exact purpose and target. -/
inductive PhaseCheck where
  | runtime | expiry | purpose | target

/-- Checks on each use of sealed delegated authority before a backend operation. -/
def phaseChecks : List PhaseCheck := [.runtime, .expiry, .purpose, .target]

/-- Admission of a cited-blob maintenance operation under delegation. -/
inductive BlobOperation where
  | delivery | verifiedCollection | ownerReconciliation

/-- Verified collection needs a positive bound and owner reconciliation requires owner authority. -/
def delegateBlobOperation : BlobOperation → Bool
  | .delivery | .verifiedCollection => true
  | .ownerReconciliation => false

end Proxiima.Authentication
