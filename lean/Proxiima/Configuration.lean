/-! # Configuration -/

namespace Proxiima.Configuration

/-- Whether a configuration layer supplies the slot being resolved. -/
inductive Presence where
  | omitted
  | supplied
  deriving DecidableEq

/-- Which layer provides a resolved configuration slot. -/
inductive Layer where
  | builtIn
  | application
  | environment
  | host
  deriving DecidableEq

/-- In host, environment, application order, the first supplied value wins, then built-in defaults; authenticators supplied in code take precedence over environment OIDC. -/
def layerPrecedence : Presence → Presence → Presence → Layer
  | .supplied, .supplied, .supplied | .supplied, .supplied, .omitted
  | .supplied, .omitted, .supplied | .supplied, .omitted, .omitted => .host
  | .omitted, .supplied, .supplied | .omitted, .supplied, .omitted => .environment
  | .omitted, .omitted, .supplied => .application
  | .omitted, .omitted, .omitted => .builtIn

/-- An authenticator supplied by either the host or application prevents using environment OIDC. -/
def useEnvironmentAuthenticator : Presence → Bool
  | .omitted => true
  | .supplied => false

/-- Only a slot or block left unset by the host is looked up and parsed from the environment. -/
def readEnvironment : Presence → Bool
  | .omitted => true
  | .supplied => false

/-- The available configuration lookup profiles. -/
inductive LookupProfile where
  | serving
  | storageOnly
  deriving DecidableEq

/-- The groups of settings used by the facade's lookup. -/
inductive SettingGroup where
  | storage
  | embeddingRuntime
  | serving
  | objectStore
  deriving DecidableEq

/-- Storage commands read storage and its embedding runtime policy, with object configuration acquired separately by commands that need it. -/
def readsGroup : LookupProfile → SettingGroup → Bool
  | .serving, .storage | .serving, .embeddingRuntime | .serving, .serving
  | .serving, .objectStore => true
  | .storageOnly, .storage | .storageOnly, .embeddingRuntime => true
  | .storageOnly, .serving | .storageOnly, .objectStore => false

/-- The lexical outcomes of trimming a setting's lookup result. -/
inductive LookupText where
  | missing
  | blank
  | nonblank
  deriving DecidableEq

/-- A missing or blank setting is omitted, and nonblank text is passed to the reader trimmed. -/
def lookupPresence : LookupText → Presence
  | .missing | .blank => .omitted
  | .nonblank => .supplied

/-- The process reader accepts a setting value rather than a bare name. -/
inductive ReadSetting where
  | declaredSetting
  | bareName
  deriving DecidableEq

/-- By the `settings!` convention, process settings are read through declared settings. -/
def mayReadProcessSetting : ReadSetting → Bool
  | .declaredSetting => true
  | .bareName => false

/-- Configured names distinguished by the unknown-name diagnostic. -/
inductive EnvironmentName where
  | declared
  | unknownProductName
  | unrelated
  deriving DecidableEq

/-- Warn once per unknown product name without exposing its value or refusing startup. -/
def warnForName : EnvironmentName → Bool
  | .declared | .unrelated => false
  | .unknownProductName => true

/-- The two positions of repeated entries in an injected environment. -/
inductive PairPosition where
  | earlier
  | later
  deriving DecidableEq

/-- The last supplied pair wins when injected configuration repeats a name. -/
def winsRepeatedPair : PairPosition → Bool
  | .earlier => false
  | .later => true

/-- Given the earlier and later values in that order, tuple entries and repeated host configuration calls retain the last supplied slot value. -/
def lastConfiguredSlot : Presence → Presence → Option PairPosition
  | .omitted, .omitted => none
  | .supplied, .omitted => some .earlier
  | .omitted, .supplied | .supplied, .supplied => some .later

/-- The possible sources of storage policy during facade boot. -/
inductive StoragePolicySource where
  | resolvedConfiguration
  | processEnvironment
  deriving DecidableEq

/-- Facade storage boot consumes its resolved pool and query policy without a second environment lookup. -/
def storagePolicySource : StoragePolicySource := .resolvedConfiguration

/-- The case-insensitive boolean words that read as true after trimming. -/
def trueWords : List String := ["1", "true", "yes", "on"]

/-- The case-insensitive boolean words that read as false after trimming. -/
def falseWords : List String := ["0", "false", "no", "off"]

/-- The registered sources that supply operator documentation and host help. -/
inductive SettingSource where
  | facade
  | core
  | postgres
  | objectStore
  | host
  deriving DecidableEq

/-- The composition order for settings documentation and help, with development tools outside the composition. -/
def documentationSources : List SettingSource := [.facade, .core, .postgres, .objectStore, .host]

/-- The inputs compared against each declared default's effective reader. -/
inductive DefaultInput where
  | omitted
  | statedDefault
  | differentValue
  deriving DecidableEq

/-- The stated default must behave like omission, and the different value must reach the reader. -/
def matchesOmission : DefaultInput → Bool
  | .omitted | .statedDefault => true
  | .differentValue => false

end Proxiima.Configuration
