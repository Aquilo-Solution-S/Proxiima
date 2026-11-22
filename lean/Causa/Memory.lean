import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Proxiima.Memory
import Proxiima.ForgetRestore

/-! Causa — Memory -/

namespace Causa

/-- The ledger vocabulary used by Memory proofs. -/
abbrev MemoryKind := Proxiima.Memory.Kind

/-- The ledger vocabulary used by `Proxiima.Edges.ErasedTarget`. -/
abbrev PinTargetKind := Proxiima.Memory.EntityKind

def MemoryKind.pinTargetKind : MemoryKind → PinTargetKind
  | .Fact        => .Fact
  | .Abstraction => .Abstraction
  | .Perspective => .Perspective

/-- The retained identity of a hard-erased pin target. -/
structure ErasedPinTarget where
  t    : MemoryId
  kind : PinTargetKind

def erased_pin_target_t : ErasedPinTarget → MemoryId := ErasedPinTarget.t
def erased_pin_target_kind : ErasedPinTarget → PinTargetKind := ErasedPinTarget.kind

theorem erased_pin_target_ext
    (e1 e2 : ErasedPinTarget)
    (ht : erased_pin_target_t e1 = erased_pin_target_t e2)
    (hk : erased_pin_target_kind e1 = erased_pin_target_kind e2) :
    e1 = e2 := by
  cases e1 with
  | mk t1 k1 =>
      cases e2 with
      | mk t2 k2 =>
          cases ht
          cases hk
          rfl

def ErasedPinTargetIdUnique (targets : Set ErasedPinTarget) : Prop :=
  ∀ e1 e2 : ErasedPinTarget,
    e1 ∈ targets →
    e2 ∈ targets →
    erased_pin_target_t e1 = erased_pin_target_t e2 →
    e1 = e2

def ErasedPinTargetKindCompatible
    (targets : Set ErasedPinTarget) (id : MemoryId) (kind : PinTargetKind) : Prop :=
  ∀ e : ErasedPinTarget, e ∈ targets → erased_pin_target_t e = id →
    erased_pin_target_kind e = kind

def recordedMemoryTargetKind
    (targets : Set ErasedPinTarget) (id : MemoryId) (kind : MemoryKind) : Prop :=
  ∃ e : ErasedPinTarget,
    e ∈ targets ∧
    erased_pin_target_t e = id ∧
    erased_pin_target_kind e = kind.pinTargetKind

def recordedMemoryTargetExists
    (targets : Set ErasedPinTarget) (id : MemoryId) : Prop :=
  ∃ kind : MemoryKind, recordedMemoryTargetKind targets id kind

def recordedGoalTargetExists
    (targets : Set ErasedPinTarget) (id : GoalId) : Prop :=
  ∃ e : ErasedPinTarget,
    e ∈ targets ∧
    erased_pin_target_t e = id ∧
    erased_pin_target_kind e = .Goal

/-- An immutable Content row, with a row ID and the ledger's owner-scoped key. -/
structure Content where
  id : ContentId
  key : Proxiima.Identity.OwnerScopedContent

def content_id : Content → ContentId := Content.id

instance : AppendOnly Content := ⟨⟩
instance : Immutable Content := ⟨⟩

/-- One admission in a Memory series. -/
structure Memory where
  handle     : Handle
  t          : MemoryId
  kind       : MemoryKind
  owner      : Owner
  writer     : WriterAttribution
  origins    : List MemoryId
  /-- Ordinary references to Memory versions. -/
  refs       : List MemoryId
  /-- Ordinary references to Goal versions declared by this Memory admission. -/
  goal_refs  : List GoalId
  /-- Goal versions that guided this Perspective. -/
  guided_by  : List GoalId
  /-- The source version of an independent Memory fork. -/
  forked_from : Option MemoryId
  /-- Read context from the writing session. -/
  reads      : List MemoryId
  blob_id    : Option BlobId
  content_id : Option ContentId
  tick       : Instant
  /-- Whether the admission is forgotten. -/
  forgotten  : Bool
  /-- Restriction of Goal guidance to Perspectives. -/
  guided_by_only_perspective : kind ≠ .Perspective → guided_by = []
  /-- Exclusion of grounding origins from Facts. -/
  fact_origins_empty : kind = .Fact → origins = []
  /-- Exclusion of blob citations from Perspectives. -/
  perspective_never_cites : kind = .Perspective → blob_id = none
  /-- The kinds eligible to cite a blob. -/
  blob_fa_only : blob_id ≠ none → kind = .Fact ∨ kind = .Abstraction

def memory_handle : Memory → Handle := Memory.handle
def memory_t : Memory → MemoryId := Memory.t
def memory_kind : Memory → MemoryKind := Memory.kind
def memory_owner : Memory → Owner := Memory.owner
def memory_origins : Memory → List MemoryId := Memory.origins
def memory_refs : Memory → List MemoryId := Memory.refs
def memory_goal_refs : Memory → List GoalId := Memory.goal_refs
def memory_guided_by : Memory → List GoalId := Memory.guided_by
def memory_forked_from : Memory → Option MemoryId := Memory.forked_from
def memory_reads : Memory → List MemoryId := Memory.reads
/-- Memory references including the fork anchor. -/
def memory_reference_ids (m : Memory) : List MemoryId :=
  memory_refs m ++ (memory_forked_from m).toList
def memory_blob_id : Memory → Option BlobId := Memory.blob_id
def memory_tick : Memory → Instant := Memory.tick
def memory_forgotten : Memory → Bool := Memory.forgotten

/-- Uniqueness of Memory version identities in a store. -/
def MemoryIdUnique (memories : Set Memory) : Prop :=
  ∀ m1 m2 : Memory,
    m1 ∈ memories →
    m2 ∈ memories →
    memory_t m1 = memory_t m2 →
    m1 = m2

def ForkAnchorAligned (memories : Set Memory) : Prop :=
  ∀ m1 m2 : Memory,
    m1 ∈ memories → m2 ∈ memories →
    memory_handle m1 = memory_handle m2 →
    memory_forked_from m1 = memory_forked_from m2

instance : AppendOnly Memory := ⟨⟩

def Fact : Type := { m : Memory // memory_kind m = .Fact }

/-- The catalog entry for a Memory series. -/
structure MemoryHead where
  handle : Handle
  kind   : MemoryKind
  schema : SchemaRef
  owner  : Owner
  t      : MemoryId

def memory_head_handle : MemoryHead → Handle := MemoryHead.handle
def memory_head_kind : MemoryHead → MemoryKind := MemoryHead.kind
def memory_head_owner : MemoryHead → Owner := MemoryHead.owner
def memory_head_t : MemoryHead → MemoryId := MemoryHead.t

/-- Agreement of Memory versions with their series catalog. -/
def MemoryHeadAligned (memories : Set Memory) (heads : Set MemoryHead) : Prop :=
  ∀ (m : Memory) (h : MemoryHead),
    m ∈ memories →
    h ∈ heads →
    memory_handle m = memory_head_handle h →
      memory_kind m = memory_head_kind h ∧
      memory_owner m = memory_head_owner h

/-- Whether the series catalog names this admission as its head. -/
def memoryIsHead (memories : Set Memory) (heads : Set MemoryHead) (m : Memory) : Prop :=
  m ∈ memories ∧
  ∃ h : MemoryHead, h ∈ heads ∧
    memory_head_handle h = memory_handle m ∧
    memory_head_t h = memory_t m

/-- Setting the forgotten mark on the Memory row. -/
def Memory.forget (m : Memory) : Memory :=
  { m with forgotten := true }

/-- Clearing the mark of an admitted restore. -/
def Memory.restore (m : Memory) : Memory :=
  { m with forgotten := false }

/-- Forget followed by an admitted restore recovers the original live Memory row. -/
theorem restore_forget (m : Memory) (live : m.forgotten = false) (retired : Bool)
    (_admitted : Proxiima.ForgetRestore.restoreAdmission retired m.forget.forgotten = .ok .rebuild) :
    m.forget.restore = m := by
  cases m
  simp_all [Memory.forget, Memory.restore]

/-- The Memory store after forgetting one admission. -/
def forgetIn (memories : Set Memory) (id : MemoryId) : Set Memory :=
  fun m' => ∃ m : Memory, m ∈ memories ∧
    ((memory_t m = id ∧ m' = m.forget) ∨ (memory_t m ≠ id ∧ m' = m))

/-- Preservation of the admission's identity, kind and owner through forgetting. -/
theorem forgetIn_keeps_row
    (memories : Set Memory) (id : MemoryId) (m : Memory) (hm : m ∈ memories) :
    ∃ m' : Memory, m' ∈ forgetIn memories id ∧ memory_t m' = memory_t m ∧
      memory_kind m' = memory_kind m ∧ memory_owner m' = memory_owner m := by
  by_cases h : memory_t m = id
  · exact ⟨m.forget, ⟨m, hm, Or.inl ⟨h, rfl⟩⟩, rfl, rfl, rfl⟩
  · exact ⟨m, ⟨m, hm, Or.inr ⟨h, rfl⟩⟩, rfl, rfl, rfl⟩

/-- Existence of a stored Memory pin target. -/
def pinExists (memories : Set Memory) (id : MemoryId) : Prop :=
  ∃ m : Memory, m ∈ memories ∧ memory_t m = id

/-- Kind lookup for a stored Memory pin target. -/
def pinKindIs
    (memories : Set Memory)
    (id : MemoryId) (k : MemoryKind) : Prop :=
  ∃ tgt : Memory, tgt ∈ memories ∧ memory_t tgt = id ∧ memory_kind tgt = k

/-- Admissible origin target kinds for an Abstraction. -/
def pinKindFactOrAbstraction
    (memories : Set Memory) (id : MemoryId) : Prop :=
  pinKindIs memories id .Fact ∨ pinKindIs memories id .Abstraction

/-- Preservation of target kind across forgetting. -/
theorem pinKindIs_forgetIn
    (memories : Set Memory) (forgotten id : MemoryId) (k : MemoryKind)
    (h : pinKindIs memories id k) : pinKindIs (forgetIn memories forgotten) id k := by
  obtain ⟨m, hm, ht, hk⟩ := h
  obtain ⟨m', hm', ht', hk', _⟩ := forgetIn_keeps_row memories forgotten m hm
  exact ⟨m', hm', ht'.trans ht, hk'.trans hk⟩

def ErasedTargetsDisjointMemories
    (targets : Set ErasedPinTarget) (memories : Set Memory) : Prop :=
  ∀ e : ErasedPinTarget, e ∈ targets →
    ∀ m : Memory, m ∈ memories → erased_pin_target_t e ≠ memory_t m

def recordedPinKindIs
    (memories : Set Memory)
    (targets : Set ErasedPinTarget)
    (id : MemoryId) (kind : MemoryKind) : Prop :=
  pinKindIs memories id kind ∨ recordedMemoryTargetKind targets id kind

def recordedPinKindFactOrAbstraction
    (memories : Set Memory)
    (targets : Set ErasedPinTarget) (id : MemoryId) : Prop :=
  recordedPinKindIs memories targets id .Fact ∨
  recordedPinKindIs memories targets id .Abstraction

structure RetainedOriginKindValid
    (memories : Set Memory)
    (targets : Set ErasedPinTarget) (m : Memory) : Prop where
  factEmpty : memory_kind m = .Fact → memory_origins m = []
  absFactOrAbs : memory_kind m = .Abstraction →
    ∀ id : MemoryId, id ∈ memory_origins m →
      recordedPinKindFactOrAbstraction memories targets id
  perspAbsOrEmpty : memory_kind m = .Perspective →
    memory_origins m = [] ∨
    ∀ id : MemoryId, id ∈ memory_origins m →
      recordedPinKindIs memories targets id .Abstraction

structure OriginKindValid
    (memories : Set Memory) (m : Memory) : Prop where
  factEmpty : memory_kind m = .Fact → memory_origins m = []
  absFactOrAbs : memory_kind m = .Abstraction →
    ∀ id : MemoryId, id ∈ memory_origins m →
      pinKindFactOrAbstraction memories id
  perspAbsOrEmpty : memory_kind m = .Perspective →
    memory_origins m = [] ∨
    ∀ id : MemoryId, id ∈ memory_origins m →
      pinKindIs memories id .Abstraction

end Causa
