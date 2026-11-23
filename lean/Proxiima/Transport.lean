/-! # Transport -/

namespace Proxiima.Transport

/-- The loopback address supplied by the canonical MCP host. -/
def canonicalBind : String := "127.0.0.1:31415"

/-- Network exposure is opt-in. -/
def defaultExposeNetwork : Bool := false

/-- The REST runtime switch starts off. -/
def defaultRestEnabled : Bool := false

/-- The health route switch starts off. -/
def defaultHealthEndpoints : Bool := false

/-- Requirements for a configured serving address. -/
inductive ListenerRequirement where
  | hostAuthenticator
  | noInsecureSingleOwnerMode
  | loopbackWithoutExposure
  | resolvedHostsWithExposure
  | concreteOriginWithExposure
  | noWildcardHosts
  | noWildcardOriginsWithExposure
  deriving DecidableEq

/-- The requirements for serving on the configured listener. -/
def listenerRequirements : List ListenerRequirement :=
  [.hostAuthenticator, .noInsecureSingleOwnerMode, .loopbackWithoutExposure,
   .resolvedHostsWithExposure, .concreteOriginWithExposure, .noWildcardHosts,
   .noWildcardOriginsWithExposure]

/-- The inbound authority sources in order of precedence. -/
inductive AuthoritySource where
  | host
  | requestAuthority
  deriving DecidableEq

/-- The listener checks Host when present and otherwise checks the request URI authority. -/
def authoritySource : List AuthoritySource := [.host, .requestAuthority]

/-- Hosts present in every listener's authority allowlist. -/
def loopbackHosts : List String := ["localhost", "127.0.0.1", "::1"]

/-- Loopback browser origins used when none are configured for the listener. -/
def loopbackOrigins : List String := ["http://localhost", "http://127.0.0.1", "http://[::1]"]

/-- Desktop webview origins used when none are configured for the listener. -/
def webviewOrigins : List String := ["tauri://localhost", "https://tauri.localhost"]

/-- Inputs used to choose public hosts. -/
inductive PublicHostSource where
  | explicit
  | publicUrl
  | allowedOrigins
  deriving DecidableEq

/-- Public hosts come from the explicit list when present, or are derived from both the public URL and the allowed origins. -/
def publicHostSources : List PublicHostSource := [.explicit, .publicUrl, .allowedOrigins]

/-- Parts of a browser origin matched against a pattern. -/
inductive OriginPart where
  | concreteScheme
  | concreteHost
  | exactOrAnyPort
  deriving DecidableEq

/-- Match a concrete scheme, a concrete host and either an exact port or any port. -/
def originMatch : List OriginPart := [.concreteScheme, .concreteHost, .exactOrAnyPort]

/-- Default port of an HTTP request Origin. -/
def httpPort : Nat := 80

/-- Default port of an HTTPS request Origin. -/
def httpsPort : Nat := 443

/-- Schemes distinguished when a request Origin omits its port. -/
inductive OriginScheme where
  | http
  | https
  | other
  deriving DecidableEq

/-- The implicit port of a request Origin for schemes with an HTTP port convention. -/
def originDefaultPort : OriginScheme → Option Nat
  | .http => some httpPort
  | .https => some httpsPort
  | .other => none

/-- Listener layers, in request execution order. -/
inductive Guard where
  | bodyLimit
  | host
  | origin
  | routeAuthentication
  deriving DecidableEq

/-- The composed router checks the body, Host and Origin before route authentication. -/
def guardOrder : List Guard := [.bodyLimit, .host, .origin, .routeAuthentication]

/-- What the request reveals about its body size before the body is read. -/
inductive DeclaredLength where
  | aboveLimit
  | withinLimit
  | unavailable
  deriving DecidableEq

/-- The action of the outer body guard. -/
inductive BodyAction where
  | refuseEarly
  | limitReads
  deriving DecidableEq

/-- Refuse an over-cap declared length immediately and limit streaming reads for every other body. -/
def bodyBudget : DeclaredLength → BodyAction
  | .aboveLimit => .refuseEarly
  | .withinLimit | .unavailable => .limitReads

/-- The default listener and MCP request body budget in bytes. -/
def defaultBodyBytes : Nat := 4194304

/-- Smallest configured listener request body budget in bytes. -/
def minBodyBytes : Nat := 1

/-- Browser request classes distinguished by the Origin guard. -/
inductive BrowserRequest where
  | noOrigin
  /-- An Origin outside the allowlist, malformed or repeated. -/
  | invalidOrigin
  | actual
  | validPreflight
  | malformedPreflight
  deriving DecidableEq

/-- Whether to continue to the route or answer at the Origin guard. -/
inductive OriginAction where
  | continueToRoute
  | originForbidden
  | preflightNoContent
  | preflightBadRequest
  deriving DecidableEq

/-- The Origin guard refuses invalid origins, answers preflight, and continues for native and allowed actual requests. -/
def browserAdmission : BrowserRequest → OriginAction
  | .noOrigin | .actual => .continueToRoute
  | .invalidOrigin => .originForbidden
  | .validPreflight => .preflightNoContent
  | .malformedPreflight => .preflightBadRequest

/-- Listener routes grouped by their guard placement. -/
inductive RouteGroup where
  | probe
  | publicRoute
  | protectedRoute
  deriving DecidableEq

/-- Guard placement for probes, public routes and protected routes. -/
def routeGuards : RouteGroup → List Guard
  | .probe => []
  | .publicRoute => [.bodyLimit, .host, .origin]
  | .protectedRoute => guardOrder

/-- The database check's readiness timeout in seconds. -/
def readinessDatabaseSeconds : Nat := 2

/-- What the compile feature and runtime setting can expose. -/
inductive RestExposure where
  | disabled
  | unavailable
  | served
  deriving DecidableEq

/-- REST serves only with both switches on, and an enabled setting without REST support is reported as unavailable. -/
def restExposure : (compiled enabled : Bool) → RestExposure
  | true, true => .served
  | true, false | false, false => .disabled
  | false, true => .unavailable

/-- HTTP methods recognized for actions on the REST tool surface. -/
inductive ActionMethod where
  | post
  | query
  | other
  deriving DecidableEq

/-- Actions accept POST, and QUERY is available for read-only actions. -/
def restActionMethod : ActionMethod → (readOnly : Bool) → Bool
  | .post, true | .post, false | .query, true => true
  | .query, false | .other, true | .other, false => false

/-- Where an action key can be supplied on a narrowed REST route. -/
inductive NarrowedActionCheck where
  | pathSuppliesAction
  | bodyMustOmitAction
  deriving DecidableEq

/-- A narrowed REST route injects the path action and refuses any action field in the body. -/
def narrowedAction : List NarrowedActionCheck := [.pathSuppliesAction, .bodyMustOmitAction]

/-- The owner selector's HTTP header name. -/
def ownerHeader : String := "X-Proxiima-Owner"

/-- The sources of an ordinary request's owner, in selection order. -/
inductive OwnerSource where
  | session
  | header
  | trustedDefault
  deriving DecidableEq

/-- A live binding selects the owner and a supplied header must name the same owner. Without a binding the header takes precedence over a trusted default. -/
def ownerSelection : List OwnerSource := [.session, .header, .trustedDefault]

/-- Transport refusal classes following credential and owner resolution. -/
inductive CredentialFailure where
  | invalidBearer
  | unavailableSession
  | ownerSelectionRefused
  deriving DecidableEq

/-- Invalid bearer is 401, unavailable session is opaque 404, and refused owner selection is 403. -/
def credentialFailureStatus : CredentialFailure → Nat
  | .invalidBearer => 401
  | .unavailableSession => 404
  | .ownerSelectionRefused => 403

/-- Session binding operations at admission and observation. -/
inductive BindingOperation where
  | pruneIdle
  | evictOldestOfPrincipal
  | refuseRatherThanEvictAnotherPrincipal
  | closeRemovedOrRefusedTransport
  | refreshLiveBinding
  deriving DecidableEq

/-- Binding admission prunes idle entries and evicts only its principal's oldest entries; observation refreshes a live binding. -/
def sessionBindingOperations : List BindingOperation :=
  [.pruneIdle, .evictOldestOfPrincipal, .refuseRatherThanEvictAnotherPrincipal,
   .closeRemovedOrRefusedTransport, .refreshLiveBinding]

/-- Idle lifetime in seconds of an in-process owner binding. -/
def bindingIdleSeconds : Nat := 3600

/-- Largest number of local owner bindings. -/
def maxBindings : Nat := 4096

/-- Largest number of local owner bindings for one principal. -/
def maxPrincipalBindings : Nat := 64

/-- Protocol entry modes that distinguish session support. -/
inductive McpEntry where
  | initialize
  | perRequest
  deriving DecidableEq

/-- Only legacy initialize can use a server-side session, and only when enabled. -/
def mcpSessions : McpEntry → (legacyEnabled : Bool) → Bool
  | .initialize, true => true
  | .initialize, false | .perRequest, true | .perRequest, false => false

/-- Legacy MCP sessions are enabled by default. -/
def defaultLegacySessions : Bool := true

/-- Stateless MCP calls do not prefer JSON by default. -/
def defaultJsonResponse : Bool := false

/-- The default SSE ping interval in seconds. -/
def defaultSsePingSeconds : Nat := 15

/-- The default transport session idle timeout in seconds. -/
def defaultSessionIdleSeconds : Nat := 300

/-- The default SSE retry hint in seconds. -/
def defaultSseRetrySeconds : Nat := 3

/-- A zero in any of the three transport duration settings disables its timer. -/
def disableTimerSeconds : Nat := 0

/-- The normal progress notification interval in seconds. -/
def defaultHeartbeatSeconds : Nat := 30

/-- Minimum progress notification interval in milliseconds. -/
def minHeartbeatMilliseconds : Nat := 100

/-- Newest MCP revision the handler implements. -/
def maxMcpRevision : String := "2026-07-28"

/-- Freshness of MCP list results in milliseconds. -/
def listTtlMilliseconds : Nat := 0

/-- Headers excluded from forwarding by exact name and by prefix. -/
def credentialHeaders : List String := ["authorization", "proxy-authorization", "cookie"]

/-- Admission requirements at request-header projection. -/
inductive HeaderRequirement where
  | allowlistedNameOrPrefix
  | singleValue
  | visibleAscii
  deriving DecidableEq

/-- A projected header must match the allowlist, have one value and be visible ASCII. -/
def projectedHeader : List HeaderRequirement := [.allowlistedNameOrPrefix, .singleValue, .visibleAscii]

/-- Transport categories distinguished by outbound endpoint validation. -/
inductive OutboundEndpoint where
  | https
  | plaintextEligibleHost
  | plaintextOtherHost
  | unsupportedScheme
  deriving DecidableEq

/-- HTTPS is admitted; HTTP requires both a permissive policy and a loopback or explicitly named host. -/
def outboundTransport : OutboundEndpoint → (plaintextEnabled : Bool) → Bool
  | .https, true | .https, false | .plaintextEligibleHost, true => true
  | .plaintextEligibleHost, false | .plaintextOtherHost, true | .plaintextOtherHost, false
  | .unsupportedScheme, true | .unsupportedScheme, false => false

/-- Components excluded from an exact plaintext host name. -/
inductive PlaintextHostExclusion where
  | wildcards
  | addresses
  | ranges
  | ports
  | schemes
  deriving DecidableEq

/-- The exclusions for exact plaintext host names. -/
def plaintextHostExclusions : List PlaintextHostExclusion := [.wildcards, .addresses, .ranges, .ports, .schemes]

end Proxiima.Transport
