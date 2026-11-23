import Proxiima.Memory

/-! # Identity -/

namespace Proxiima.Identity

/-- An embedding uses a Memory ID with its layer, or a Goal ID. -/
inductive EmbeddableEntity where
  | memory (kind : Proxiima.Memory.Kind)
  | goal
  deriving DecidableEq

/-- An admission ID is available only when absent from both spines and the retained erase witnesses. -/
def admissionIdAvailable : (memory : Bool) → (goal : Bool) → (erased : Bool) → Bool
  | false, false, false => true
  | false, false, true | false, true, false | false, true, true
  | true, false, false | true, false, true | true, true, false | true, true, true => false

/-- The kind of series whose versions align with its head. -/
inductive SeriesKind where
  | memory
  | goal
  deriving DecidableEq

/-- The catalog fields shared by all versions of a series. -/
inductive HeadField where
  | handle
  | owner
  | kind
  | schema
  deriving DecidableEq

/-- The fields shared by the versions and head of each series kind. -/
def seriesContract : SeriesKind → List HeadField
  | .memory => [.handle, .owner, .schema, .kind]
  | .goal => [.handle, .owner, .schema]

/-- Records whose stored schema selects the typed shape. -/
inductive SchemaRecord where
  | memoryVersion
  | goalVersion
  | blob
  deriving DecidableEq

/-- The source of a record's schema tag. -/
inductive SchemaAuthority where
  | memoryCatalog
  | goalCatalog
  | blob
  deriving DecidableEq

/-- Memory validates its version's schema against the catalog, Goal stamps or validates it there, and Blob carries its own schema. -/
def schemaAuthority : SchemaRecord → SchemaAuthority
  | .memoryVersion => .memoryCatalog
  | .goalVersion => .goalCatalog
  | .blob => .blob

/-- An admission stores the attested writer separately from its owner and credential subject. -/
structure WriterAttribution where
  /-- The attested principal UUID. -/
  «by» : String
  /-- An optional attested caller UUID. -/
  via : Option String

/-- Events that capture or retain writer attribution. -/
inductive WriterEvent where
  | admission
  | revision
  | replay
  | transfer
  | forget
  | restore
  deriving DecidableEq

/-- Whether an event takes its writer from the request or the version. -/
inductive WriterSource where
  | attested
  | stored
  deriving DecidableEq

/-- Each new version captures its own writer, and replay, transfer, forget and restore keep stored authorship. -/
def versionWriter : WriterEvent → WriterSource
  | .admission | .revision => .attested
  | .replay | .transfer | .forget | .restore => .stored

/-- The owner, schema and hash that key Content. -/
structure OwnerScopedContent where
  owner : String
  schema : String
  hash : String

/-- Which admissions still name a Content row. -/
inductive ContentReference where
  | live
  | forgottenOnly
  | absent
  deriving DecidableEq

/-- Collect Content only once no live or forgotten admission names it. -/
def collectContent : ContentReference → Bool
  | .live | .forgottenOnly => false
  | .absent => true

/-- The ordered components of a Fact receipt digest. -/
inductive ReceiptField where
  | source
  | owner
  | payload
  deriving DecidableEq

/-- The ordered components of a Fact receipt digest. -/
def receiptFields : List ReceiptField := [.source, .owner, .payload]

/-- Timestamp width in the time-ordered UUIDv8 layout, in milliseconds since the Unix epoch. -/
def uuidTimestampBits : Nat := 48

/-- Counter width in the time-ordered UUIDv8 layout. -/
def uuidCounterBits : Nat := 12

/-- The clock relative to the greater of the last mint and the supplied revision floor. -/
inductive MintClock where
  | aheadOrUnbound
  | atOrBehind
  deriving DecidableEq

/-- Whether another counter fits in the chosen logical millisecond. -/
inductive CounterSpace where
  | available
  | exhausted
  deriving DecidableEq

/-- Where the shared generator mints the next admission. -/
inductive MintStep where
  | reseedAtClock
  | incrementCounter
  | reseedNextMillisecond
  deriving DecidableEq

/-- Range of the clock or the logical millisecond selected for admission. -/
inductive MintRange where
  | fits
  | overflow
  deriving DecidableEq

/-- The refusal of a timestamp outside the admission ID range. -/
inductive MintRefusal where
  | timestampOverflow
  deriving DecidableEq

/-- A mint advances past the revision floor and the last mint, even when the clock stalls. -/
def monotonicMint : MintClock → CounterSpace → MintRange → Except MintRefusal MintStep
  | .aheadOrUnbound, .available, .overflow | .aheadOrUnbound, .exhausted, .overflow
  | .atOrBehind, .available, .overflow | .atOrBehind, .exhausted, .overflow =>
    .error .timestampOverflow
  | .aheadOrUnbound, .available, .fits | .aheadOrUnbound, .exhausted, .fits => .ok .reseedAtClock
  | .atOrBehind, .available, .fits => .ok .incrementCounter
  | .atOrBehind, .exhausted, .fits => .ok .reseedNextMillisecond

/-- The persisted store number relative to the configured number. -/
inductive StoreNumberState where
  | absent
  | matching
  | different
  deriving DecidableEq

/-- Operations that open a numbered store. -/
inductive StoreNumberEvent where
  | migrate
  | validate
  deriving DecidableEq

/-- The result of applying the configured number. -/
inductive StoreNumberAction where
  | persist
  | useExisting
  | requireMigration
  | refuseMismatch
  deriving DecidableEq

/-- The first migration persists the number and subsequent migrations, serving and maintenance require agreement. -/
def storeNumberLifecycle : StoreNumberState → StoreNumberEvent → StoreNumberAction
  | .absent, .migrate => .persist
  | .absent, .validate => .requireMigration
  | .matching, .migrate | .matching, .validate => .useExisting
  | .different, .migrate | .different, .validate => .refuseMismatch

/-- Publication counters retain their series handle and never regress. -/
def publicationCounterUpdate : (sameHandle : Bool) → (nondecreasing : Bool) → Bool
  | true, true => true
  | true, false | false, true | false, false => false

end Proxiima.Identity
