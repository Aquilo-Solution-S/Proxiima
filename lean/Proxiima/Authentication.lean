import Proxiima.OwnerAuthority

/-! # Authentication -/

namespace Proxiima.Authentication

/-- The required checks before returning claims from an OIDC token. -/
inductive TokenCheck where
  | keyFamilySignature | exactIssuer | audienceMembership | expiry | subjectString | presentNotBefore

/-- The OIDC checks required before verified claims are returned. -/
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

/-- URL classes used to validate issuer and key endpoints. -/
inductive Transport where
  | https | loopbackHttp | other

/-- Issuer transport admits HTTPS and loopback HTTP. -/
def issuerTransport : Transport → Bool
  | .https | .loopbackHttp => true
  | .other => false

/-- Raw query and fragment delimiters excluded from issuer URLs. -/
def issuerUrlExcludedDelimiters : List String := ["?", "#"]

/-- Configured, discovered and redirected key endpoints use HTTPS, or loopback HTTP when their issuer is also loopback. -/
def keyTransport : (loopbackIssuer : Bool) → Transport → Bool
  | true, .https | false, .https | true, .loopbackHttp => true
  | false, .loopbackHttp | true, .other | false, .other => false

/-- The URL boundary checked before reading the subject map or opening storage. -/
inductive BootstrapUrl where
  | publicUrl | issuer | explicitKeyEndpoint

/-- The URL checks performed before subject-map and storage intake. -/
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

/-- A configured issuer and subject map to a principal and optional runner, with empty keys, duplicate bindings and malformed records refused. -/
structure SubjectBinding where
  issuer : String
  subject : String
  principal : String
  runner : Option String

/-- The possible admission results for a verified subject. -/
inductive SubjectAdmission where
  | allow | refuse

/-- Which verified subjects are admitted with or without a configured subject allowlist. -/
def subjectAdmission : (allowlistPresent subjectListed : Bool) → SubjectAdmission
  | false, false | false, true | true, true => .allow
  | true, false => .refuse

/-- The configured subject allowlist at settings intake. -/
inductive SubjectAllowlistSetting where
  | omitted | empty | populated

/-- An omitted or populated subject allowlist is accepted, while an explicitly empty list is refused. -/
def admitSubjectAllowlist : SubjectAllowlistSetting → Bool
  | .omitted | .populated => true
  | .empty => false

/-- The presented authorized party after reading the validated token's audience and exact nonempty route allowset. -/
inductive PartyCheck where
  | missingSingleAudience | missingMultipleAudiences | listed | unlisted | malformed

/-- A set route policy accepts a listed party or an omitted party with a single distinct audience. -/
def admitAuthorizedParty : PartyCheck → Bool
  | .listed | .missingSingleAudience => true
  | .unlisted | .missingMultipleAudiences | .malformed => false

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

/-- The context operations that preserve the optional verified payload. -/
def verifiedContext : List ClaimsCarry := [.clone, .narrowToOwner]

/-- Each transport request verifies its own credential and any session must agree with the current principal and owner. -/
inductive SessionAuthorityCheck where
  | authenticateRequest | agreeWithPrincipal | agreeWithOwner

/-- The credential and session agreement checks on each request. -/
def sessionChecks : List SessionAuthorityCheck :=
  [.authenticateRequest, .agreeWithPrincipal, .agreeWithOwner]

/-- The consumers that resolve roles through the runtime's once-bound owner access port. -/
inductive OwnerResolverConsumer where
  | mcpEdge | environmentOidc | delegation | platformFactory

/-- The consumers of the owner access port bound during serving. -/
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

/-- The non-managing role names admitted by forwarding policy. -/
inductive ForwardingRole where
  | viewer | ingest | editor
  deriving DecidableEq

/-- Forwarded roles may read Goals and have neither Goal work, owner administration nor group management. -/
def forwardingCap : Proxiima.OwnerAuthority.RolePreset :=
  ⟨.perspective, .perspective, .reads, false, false, rfl⟩

/-- A forwarded request keeps its subject, verifies owner agreement and resolves only the claim's registered personal or group owner at the configured capped role. -/
inductive ForwardingCheck where
  | verifiedIdentityAndWriter | exactOwner | liveCredential | configuredCeiling | registeredOwner

/-- The identity, owner, time, role and registry checks for a forwarded request. -/
def forwardingChecks : List ForwardingCheck :=
  [.verifiedIdentityAndWriter, .exactOwner, .liveCredential, .configuredCeiling, .registeredOwner]

/-- The sources that may seal owner authority. -/
inductive ScopeSource where
  | verifiedCredential | singleOwnerBoot

/-- The verified-credential and personal-owner boot paths that seal owner authority. -/
def scopeSources : List ScopeSource := [.verifiedCredential, .singleOwnerBoot]

/-- Only engine gates construct read and write permits from their checked authority. -/
inductive EnginePermit where
  | entryRead | write

/-- The engine permit families subject to `Proxiima.OwnerAuthority.ScopeRestriction.expiry`. -/
def enginePermits : List EnginePermit := [.entryRead, .write]

/-- The Unicode character limit for credential-attested and caller-supplied model labels and for the retained portion of an echoed label. -/
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

/-- Maximum validated host run attributes carried with Fact capture. -/
def maxHostRunExtensions : Nat := 8

/-- Default seconds before the lifetime limit starts the final drain of an authenticated stream. -/
def defaultStreamLifetimeSeconds : Nat := 3600

/-- Maximum frames delivered without waiting after the first authorization deadline. -/
def finalReadyFrames : Nat := 16

/-- Observations of the first deadline and source polls, with a ready item counting toward the closing budget if its poll reaches the deadline. -/
inductive StreamObservation where
  | firstDeadline | ready | finalReady | sourcePending | sourceEnded

/-- Stream states before the first deadline, while draining up to finalReadyFrames, and after termination. -/
inductive StreamState where
  | open | closing | ended

/-- The first deadline starts closing, every pulled item is delivered, and closing ends at the budget, a pending poll or source EOF. -/
def streamLifetime : StreamState → StreamObservation → StreamState
  | .open, .ready | .open, .finalReady | .open, .sourcePending => .open
  | .open, .firstDeadline | .closing, .firstDeadline | .closing, .ready => .closing
  | .open, .sourceEnded | .closing, .finalReady | .closing, .sourcePending | .closing, .sourceEnded
  | .ended, .firstDeadline | .ended, .ready | .ended, .finalReady | .ended, .sourcePending | .ended, .sourceEnded => .ended

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

/-- The built-in sign-in and refresh scopes, in advertisement order. -/
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

/-- The redirect and token endpoint authentication checks on public-client registration. -/
def registrationChecks : List RegistrationCheck := [.nonemptyAbsoluteRedirects, .publicAuthentication]

/-- The public token endpoint authentication method used when registration omits the method and required when it supplies one. -/
def publicRegistrationAuthMethod : String := "none"

/-- A registered delegated command is a flat tool or a dispatcher leaf, with serialized scope derived from the typed command. -/
inductive DelegatedCommand where
  | flat | dispatcherLeaf

/-- The two command kinds admitted for delegation, excluding resources and bare dispatchers. -/
def delegatedCommands : List DelegatedCommand := [.flat, .dispatcherLeaf]

/-- Maximum delegated tool identifier length in characters. -/
def maxDelegatedCommandChars : Nat := 200

/-- A queued job stores an opaque redacted UUIDv7 grant handle and no bearer. -/
structure GrantHandle where
  id : String

/-- Issuance checks recorded before minting a grant for a finite verified host bearer. -/
inductive GrantIssueCheck where
  | finiteVerifiedBearer | currentMembership | sourceRole | nonManagingCeiling | registeredCommand
  | callerScope | deployment | sourceExpiry | attestedWriter

/-- The credential, membership, command and policy checks before issuing a delegation grant. -/
def grantIssueChecks : List GrantIssueCheck :=
  [.finiteVerifiedBearer, .sourceRole, .sourceExpiry, .nonManagingCeiling, .currentMembership,
   .registeredCommand, .callerScope, .deployment, .attestedWriter]

/-- Admission checks performed anew at each delegated phase. -/
inductive GrantRedemptionCheck where
  | exactOwnerAndCommand | expiry | revocation | callerProfile | currentMembership

/-- The owner, command, validity and membership checks before admitting each delegated phase. -/
def grantRedemptionChecks : List GrantRedemptionCheck :=
  [.exactOwnerAndCommand, .revocation, .expiry, .callerProfile, .currentMembership]

/-- Who may revoke a grant after proving current bearer membership in its owner. -/
inductive GrantRevoker where
  | issuer | ownerManager

/-- The issuer and current owner managers who may revoke a grant. -/
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
