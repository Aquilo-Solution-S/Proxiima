import Proxiima.Operations

/-! # Tool surface -/

namespace Proxiima.ToolSurface

/-- The shared character limit on tool and action titles. -/
def maxTitleChars : Nat := 60

/-- The longest host tool name in characters. -/
def maxHostToolNameChars : Nat := 128

/-- The longest host instructions contribution in characters. -/
def maxHostInstructionsChars : Nat := 4096

/-- The recorded label when neither the authenticated caller nor the call names a model. -/
def unknownOperator : String := "unknown"

/-- The wire spelling of the memory tool profile. -/
def memoryProfile : String := "memory"

/-- The wire spelling of the full tool profile. -/
def fullProfile : String := "full"

/-- The reserved head of a resource scope key. -/
def resourceKeyPrefix : String := "resource"

/-- Canonical scope key for the synthesized usage guide. -/
def guideScopeKey : String := "resource:how-to"

/-- OpenAPI version with a fixed QUERY operation field. -/
def openApiVersion : String := "3.2.0"

/-- JSON Schema dialect embedded in the REST projection. -/
def openApiSchemaDialect : String := "https://json-schema.org/draft/2020-12/schema"

/-- Cache policy on caller-scoped REST responses and problem documents. -/
def privateCacheControl : String := "private, no-store"

/-- Canonical prefix of a problem type URI. -/
def problemTypePrefix : String := "https://proxiima.dev/errors/"

/-- The vocabulary a registered tool uses to select an effect and scope key. -/
inductive Dispatch where
  | flat | taggedAction | argv
  deriving DecidableEq

/-- Whether an effect belongs to the tool itself or to its selected action. -/
inductive EffectSource where
  | tool | selectedAction
  deriving DecidableEq

/-- Flat tools declare their own effect and dispatchers declare effects per action. -/
def effectSource : Dispatch → EffectSource
  | .flat => .tool
  | .taggedAction | .argv => .selectedAction

/-- Only a flat tool that declares a read may opt into dropping unknown keys. -/
def tolerantFieldsAllowed : Dispatch → (readOnly : Bool) → Bool
  | .flat, true => true
  | .flat, false | .taggedAction, true | .taggedAction, false | .argv, true | .argv, false => false

/-- Operations on flat arguments and their successful result. -/
inductive FlatArgumentStep where
  | coerceSpaceAliases | applyUnknownFieldPolicy | deserialize | reportDroppedKeys
  deriving DecidableEq

/-- Normalize aliases before judging fields; report dropped keys on a successful result. -/
def flatArguments : List FlatArgumentStep :=
  [.coerceSpaceAliases, .applyUnknownFieldPolicy, .deserialize, .reportDroppedKeys]

/-- The contract check on a registered tagged action. -/
inductive TaggedCheck where
  | actionDiscriminator | uniqueExactActionSet | matchingAllowedFields | matchingRequiredFields
  deriving DecidableEq

/-- Freeze requires a unique, exact action set with matching allowed and required fields. -/
def taggedAgreement : List TaggedCheck :=
  [.actionDiscriminator, .uniqueExactActionSet, .matchingAllowedFields, .matchingRequiredFields]

/-- Checks on the presentation title of a tool or action. -/
inductive TitleCheck where
  | nonempty | trimmed | noControlCharacters | withinCharacterLimit
  deriving DecidableEq

/-- A title must be nonempty, trimmed, control-free and within the character limit. -/
def titleChecks : List TitleCheck :=
  [.nonempty, .trimmed, .noControlCharacters, .withinCharacterLimit]

/-- The three canonical key forms that enter a tool scope. -/
inductive KeyForm where
  | tool | actionLeaf | resource
  deriving DecidableEq

/-- A flat tool contributes its tool key and either dispatcher contributes action leaves. -/
def toolKeyForm : Dispatch → KeyForm
  | .flat => .tool
  | .taggedAction | .argv => .actionLeaf

/-- Distinct checks on a canonical key part. -/
inductive KeyPartCheck where
  | nonempty | noSeparator | providerSafe
  deriving DecidableEq

/-- Key parts must be nonempty and preserve their provider-safe spelling without the key separator. -/
def keyPartChecks : List KeyPartCheck := [.nonempty, .noSeparator, .providerSafe]

/-- Tool title lookup outcomes by key relation. -/
inductive TitleKey where
  | ownTool | declaredAction | unknownAction | foreignTool | resource
  deriving DecidableEq

/-- A title is available only for the tool's own key and its declared actions. -/
def titleKeyAdmitted : TitleKey → Bool
  | .ownTool | .declaredAction => true
  | .unknownAction | .foreignTool | .resource => false

/-- Checks a request passes before entering the registered terminal. -/
inductive CallCheck where
  | rejectNul | deriveKey | recordKey | admitCapabilities | admitSelectedOwner
  deriving DecidableEq

/-- The shared request path rejects NUL before the gate and records the judged key before deciding. -/
def callChecks : List CallCheck :=
  [.rejectNul, .deriveKey, .recordKey, .admitCapabilities, .admitSelectedOwner]

/-- Independent checks for including a key in the caller's listed surface. -/
inductive ListingCheck where
  | callerScope | servedSurface | ownerAccess | requestBehaviors
  deriving DecidableEq

/-- Listings intersect caller scope, deployment policy, owner access and behavior visibility. -/
def listingChecks : List ListingCheck := [.callerScope, .servedSurface, .ownerAccess, .requestBehaviors]

/-- Whether a call reads or may write and whether its write is Goal work. -/
inductive AccessRequirement where
  | read | goalWork | memoryWrite
  deriving DecidableEq

/-- The owner work a tool declares. -/
inductive OwnerNeed where
  | memory | goalWork
  deriving DecidableEq

/-- A read needs read access; other calls require the declared kind of owner work. -/
def ownerRequirement : (readOnly : Bool) → OwnerNeed → AccessRequirement
  | true, .memory | true, .goalWork => .read
  | false, .goalWork => .goalWork
  | false, .memory => .memoryWrite

/-- Replay behavior of an action that may write. -/
inductive Replay where
  | idempotent | nonIdempotent
  deriving DecidableEq

/-- Kinds of tool effect with replay behavior for writes. -/
inductive Effect where
  | readOnly | additive (replay : Replay) | destructive (replay : Replay)
  deriving DecidableEq

/-- Reads replay idempotently; writes use their declared replay behavior. -/
def replayOf : Effect → Replay
  | .readOnly | .additive .idempotent | .destructive .idempotent => .idempotent
  | .additive .nonIdempotent | .destructive .nonIdempotent => .nonIdempotent

/-- The MCP read, destructive and idempotent hints carried for an effect. -/
structure EffectHints where
  readOnly : Bool
  destructive : Option Bool
  idempotent : Option Bool

/-- Read annotations omit both write hints; write annotations carry destructive and replay hints. -/
def effectHints : Effect → EffectHints
  | .readOnly => ⟨true, none, none⟩
  | .additive .idempotent => ⟨false, some false, some true⟩
  | .additive .nonIdempotent => ⟨false, some false, some false⟩
  | .destructive .idempotent => ⟨false, some true, some true⟩
  | .destructive .nonIdempotent => ⟨false, some true, some false⟩

/-- The typed reply forms an entered tool can produce. -/
inductive Reply where
  | structured | content | failure | structuredFailure
  deriving DecidableEq

/-- Only structured success can become a JSON-only answer. -/
def jsonReply : Reply → Bool
  | .structured => true
  | .content | .failure | .structuredFailure => false

/-- Which reply form a tool with an output schema may return successfully. -/
def replyWithOutputSchema : Reply → Bool
  | .structured | .failure => true
  | .content | .structuredFailure => false

/-- Text sources composing MCP instructions, in rendering order. -/
inductive InstructionSource where
  | visibleSurface | boundedHostContribution
  deriving DecidableEq

/-- Generated instructions precede a bounded host contribution. -/
def instructionOrder : List InstructionSource := [.visibleSurface, .boundedHostContribution]

/-- The listener types for tool-list changes. -/
inductive ListChangeListener where
  | initializedSession | selectedSubscription
  deriving DecidableEq

/-- Sessions and selected subscriptions listen for tool-list changes. -/
def listChangeListeners : List ListChangeListener := [.initializedSession, .selectedSubscription]

/-- Owner-channel delivery steps for a tool-list change. -/
inductive ListChangeStep where
  | selectOwner | discardClosed | send | discardFailed
  deriving DecidableEq

/-- A tool-list change addresses only its owner's live listeners and discards failed deliveries. -/
def listChangeDelivery : List ListChangeStep := [.selectOwner, .discardClosed, .send, .discardFailed]

/-- The sources used when composing a host tools listing. -/
inductive CatalogSource where
  | frozenRegistry | hostSource
  deriving DecidableEq

/-- A tools listing draws from the frozen registry and the host tool source. -/
def mcpCatalogSources : List CatalogSource := [.frozenRegistry, .hostSource]

/-- The stages of preparing a derived registered tool schema. -/
inductive SchemaPreparation where
  | deriveInput | inlineReferences | normalizeInputRoot | deriveOutputAsSerialized | validateOutputRoot
  deriving DecidableEq

/-- Registration inlines input schemas, normalizes the MCP root and derives output for serialization. -/
def schemaPreparation : List SchemaPreparation :=
  [.deriveInput, .inlineReferences, .normalizeInputRoot, .deriveOutputAsSerialized, .validateOutputRoot]

/-- The registered vocabulary entries an authorization leaf can resolve. -/
inductive ActionArgumentSource where
  | selectedTaggedFields | argvEnvelope | flatEnvelope
  deriving DecidableEq

/-- Registered argument validation selects fields from the action or the tool envelope. -/
def actionArgumentSource : Dispatch → ActionArgumentSource
  | .taggedAction => .selectedTaggedFields
  | .argv => .argvEnvelope
  | .flat => .flatEnvelope

end Proxiima.ToolSurface
