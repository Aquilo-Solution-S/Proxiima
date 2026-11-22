import Proxiima.Memory
import Proxiima.Payloads

/-! # Composition -/

namespace Proxiima.Composition

/-- Which stage of composition owns the registry. -/
inductive RegistryStage where
  | registration
  | serving
  deriving DecidableEq

/-- What supplies each stage's registry. -/
inductive RegistrySource where
  | linkedBundles
  | validatedFreeze
  deriving DecidableEq

/-- Linked code registers at boot and only a validated registry serves calls. -/
def registrySource : RegistryStage → RegistrySource
  | .registration => .linkedBundles
  | .serving => .validatedFreeze

/-- The result of admitting a concrete service type. -/
inductive ServiceAdmission where
  | insert
  | duplicate
  deriving DecidableEq

/-- An already present concrete service type is refused before insertion. -/
def serviceAdmission : (alreadyPresent : Bool) → ServiceAdmission
  | false => .insert
  | true => .duplicate

/-- Which service set application composition exposes. -/
inductive ServicePhase where
  | construction
  | running
  deriving DecidableEq

/-- The visibility of services in application code. -/
inductive ServiceVisibility where
  | hostSupplied
  | composed
  deriving DecidableEq

/-- Service factories see the supplied set and routes, tools and workers see the composed set. -/
def serviceVisibility : ServicePhase → ServiceVisibility
  | .construction => .hostSupplied
  | .running => .composed

/-- The supplier of a request header service. -/
inductive HeaderSupplier where
  | boot
  | requestExtension
  | allowlistProjection
  deriving DecidableEq

/-- Only the transport's per-request allowlist projection may supply request headers. -/
def maySupplyRequestHeaders : HeaderSupplier → Bool
  | .boot | .requestExtension => false
  | .allowlistProjection => true

/-- Operations on the request header service. -/
inductive HeaderUse where
  | storeName
  | lookupName
  | debug
  deriving DecidableEq

/-- Which form of a header name a request header operation uses. -/
inductive HeaderName where
  | lowercase
  | namesOnly
  deriving DecidableEq

/-- Header storage and lookup normalize names and debug shows only names. -/
def requestHeaderNames : HeaderUse → HeaderName
  | .storeName | .lookupName => .lowercase
  | .debug => .namesOnly

/-- Whether the host declaration can supply the requested tool's behavior. -/
def hostToolEligible : (registered : Bool) → (matchingName : Bool) → Bool
  | false, true => true
  | false, false | true, false | true, true => false

/-- The macro registration inputs that name a schema or tool. -/
inductive RegisteredName where
  | typedSchema
  | opaqueCitation
  | tool
  deriving DecidableEq

/-- What a registration accepts after the spiice prefix. -/
inductive PrefixSyntax where
  | slash
  | slashOrUnderscore
  deriving DecidableEq

/-- Declared schema names are slash-prefixed and declared tools also accept underscore. -/
def prefixSyntax : RegisteredName → PrefixSyntax
  | .typedSchema | .opaqueCitation => .slash
  | .tool => .slashOrUnderscore

/-- Where registration checks a declared name's prefix. -/
inductive PrefixCheck where
  | compileTime
  | registration
  deriving DecidableEq

/-- A typed schema or tool name is checked at compile time and opaque expressions at registration. -/
def prefixCheck : RegisteredName → PrefixCheck
  | .typedSchema | .tool => .compileTime
  | .opaqueCitation => .registration

/-- Which identity components distinguish registered schemas and persisted Memory selectors. -/
inductive SchemaIdentityPart where
  | id
  | version
  | kind
  deriving DecidableEq

/-- Which use of a schema identity freeze checks for uniqueness. -/
inductive SchemaIdentityUse where
  | registration
  | persistedMemory
  deriving DecidableEq

/-- Registrations are unique by full identity and persisted Memory selectors omit version. -/
def schemaIdentity : SchemaIdentityUse → List SchemaIdentityPart
  | .registration => [.id, .version, .kind]
  | .persistedMemory => [.kind, .id]

/-- Contract checks whose relative order determines the first refusal. -/
inductive ContractCheck where
  | registrationAgreement
  | projectionUniformity
  | projectionReachability
  | bespokeLegAuthority
  | surfaceReachability
  | lifecycleLegs
  deriving DecidableEq

/-- Check registration before projection and bespoke-leg authority before lifecycle coverage. -/
def contractCheckOrder : List ContractCheck :=
  [.registrationAgreement, .projectionUniformity, .projectionReachability, .bespokeLegAuthority, .surfaceReachability, .lifecycleLegs]

/-- Refusals for substrate-only declarations, identified by `Proxiima.Payloads.coreOrdinal`. -/
inductive CoreDeclarationRefusal where
  | resources
  | kernelSurfaces
  deriving DecidableEq

/-- Only core admits resource and kernel-surface declarations, with resource refusal taking precedence. -/
def coreDeclarations :
    (core : Bool) → (resources : Bool) → (kernelSurfaces : Bool) → Except CoreDeclarationRefusal Bool
  | true, true, true | true, true, false | true, false, true | true, false, false => .ok true
  | false, true, true | false, true, false => .error .resources
  | false, false, true => .error .kernelSurfaces
  | false, false, false => .ok true

/-- The two character positions of a capability tag. -/
inductive TagPosition where
  | first
  | subsequent
  deriving DecidableEq

/-- Character classes distinguished by the capability tag grammar. -/
inductive TagCharacter where
  | asciiLowercase
  | asciiDigit
  | hyphen
  | other
  deriving DecidableEq

/-- A tag starts with a lowercase ASCII letter and may continue with those letters, digits or hyphens. -/
def capabilityTagCharacter : TagPosition → TagCharacter → Bool
  | .first, .asciiLowercase | .subsequent, .asciiLowercase | .subsequent, .asciiDigit | .subsequent, .hyphen => true
  | .first, .asciiDigit | .first, .hyphen | .first, .other | .subsequent, .other => false

/-- The source of an application version when using the app macro. -/
inductive AppVersionSource where
  | invokingPackage
  | explicit
  deriving DecidableEq

/-- The app macro uses the invoking Cargo package version when the caller omits an override. -/
def appVersionSource : (overrideSupplied : Bool) → AppVersionSource
  | true => .explicit
  | false => .invokingPackage

/-- Ways the canonical app composes the core. -/
inductive CoreCommand where
  | serve
  | maintenance
  | migrate
  deriving DecidableEq

/-- How a core command reaches storage. -/
inductive CoreStartup where
  | listenerWithValidation
  | headlessWithValidation
  | headlessMigration
  deriving DecidableEq

/-- The canonical core app starts a listener only for serving and migrates only on explicit request. -/
def coreStartup : CoreCommand → CoreStartup
  | .serve => .listenerWithValidation
  | .maintenance => .headlessWithValidation
  | .migrate => .headlessMigration

/-- The caller may deny a registered command while mandatory reads and unknown keys are refused. -/
def profileDenial : (registered : Bool) → (mandatory : Bool) → Bool
  | true, false => true
  | true, true | false, true | false, false => false

/-- The agreement checks between a linked registry and a declared contract. -/
inductive AgreementCheck where
  | declaredSchemaHasRegistration
  | registeredSchemaHasDeclaration
  | declaredToolHasRegistration
  deriving DecidableEq

/-- Each contract covers its schema registrations in both directions and every declared tool is registered. -/
def contractAgreement : List AgreementCheck :=
  [.declaredSchemaHasRegistration, .registeredSchemaHasDeclaration, .declaredToolHasRegistration]

/-- Conditions that match a natural key declaration to its registration. -/
inductive NaturalKeyCheck where
  | matchingSchemaIdWhenPresent
  | uniqueColumns
  | matchesTypedRegistration
  deriving DecidableEq

/-- Natural key agreement requires unique columns and the registered typed declaration. -/
def naturalKeyAgreement : List NaturalKeyCheck :=
  [.matchingSchemaIdWhenPresent, .uniqueColumns, .matchesTypedRegistration]

/-- A typed schema has one matching protocol parser and an opaque citation has none. -/
def ingressParserCount : Proxiima.Payloads.IngressShape → Nat
  | .typed => 1
  | .opaqueCitedObject => 0

/-- Schema declarations the bundle macro may carry. -/
inductive BundleSchema where
  | entity (kind : Proxiima.Memory.EntityKind)
  | typedCitation
  | opaqueCitation
  deriving DecidableEq

/-- The bundle macro generates sidecar registrations from the entity schema lists. -/
def registersBundleSidecar : BundleSchema → Bool
  | .entity .Fact | .entity .Abstraction | .entity .Perspective | .entity .Goal => true
  | .typedCitation | .opaqueCitation => false

end Proxiima.Composition
