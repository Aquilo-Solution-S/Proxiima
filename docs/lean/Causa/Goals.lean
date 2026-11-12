/-
Causa — Goals

Goal is its own timeseries (UML §3). Not Memory.

  series  = handle
  version = t

The runtime orders the versions of a handle: a later `t` is the new head, and
a terminal state admits no later `t`. The kernel states neither.

No `supersedes`. No authorship blob. Title stays. `text` is on the row: the
version's prompt, `none` when unknown, never blank (`goal_text_nonblank_chk`,
migration 0025); the rest of the body is the sidecar. A version that changes
the state keeps its predecessor's text (the Goal write verbs carry it).
`close_fact_t` pins an existing Fact `t`. `assignment_t` is the historical
single intended Perspective target on old versions; the causal
`motivated_by` list is distinct and does not reinterpret it.

The goal tree (#514): an open goal (Active or Paused) never sits under a
closed one (Achieved or Abandoned). Each write checks one level: decompose
needs an Active parent, achieve and abandon need no open child (abandon
takes an Active or Paused goal, #515), and pause, resume and modify need
nothing. `goalStep_preserves_inv` shows that keeps the
rule, and `closed_ancestor_closes_descendant` that it holds at any depth.
-/

import Causa.Prelude
import Causa.Owner
import Causa.Identity
import Causa.Memory

namespace Causa

-- ============================================================
-- States and lifecycle
-- ============================================================

inductive GoalState where
  | Active
  | Paused
  | Achieved
  | Abandoned
  deriving DecidableEq, Repr

def GoalState.terminal : GoalState → Bool
  | .Achieved | .Abandoned => true
  | _ => false

-- ============================================================
-- The Goal entity
-- ============================================================

/-- The characters `goal_text_nonblank_chk` trims: ASCII whitespace (space,
tab, line feed, carriage return, vertical tab, form feed). -/
def goalTextBlankChar (c : Char) : Bool :=
  c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\x0b' || c == '\x0c'

/-- `goal_text_nonblank_chk`: a stored text keeps a character the check does
not trim. -/
def GoalTextNonblank (s : Text) : Prop :=
  s.any (fun c => !goalTextBlankChar c) = true

structure Goal where
  handle        : Handle
  t             : GoalId
  owner         : Owner
  writer        : WriterAttribution
  title         : Text
  text          : Option Text
  state         : GoalState
  request_id    : Text
  close_fact_t  : Option MemoryId
  assignment_t  : Option MemoryId
  dependency_t  : List GoalId
  evidence_t    : List MemoryId
  /-- Exact Perspective versions that motivated this Goal version. -/
  motivated_by  : List MemoryId
  write_act_t   : Option MemoryId
  /-- The parent's series (#514), set at decompose and fixed for the series. -/
  parent_handle : Option Handle
  tick          : Instant
  terminal_close_fact :
    state.terminal = true → close_fact_t.isSome = true
  text_nonblank :
    ∀ s, text = some s → GoalTextNonblank s

def goal_handle : Goal → Handle := Goal.handle
def goal_t : Goal → GoalId := Goal.t
def goal_id : Goal → GoalId := Goal.t
def goal_owner : Goal → Owner := Goal.owner
def goal_state : Goal → GoalState := Goal.state
def goal_close_fact_t : Goal → Option MemoryId := Goal.close_fact_t
def goal_assignment : Goal → Option MemoryId := Goal.assignment_t
def goal_dependencies : Goal → List GoalId := Goal.dependency_t
def goal_evidence : Goal → List MemoryId := Goal.evidence_t
def goal_motivated_by : Goal → List MemoryId := Goal.motivated_by
def goal_write_act_t : Goal → Option MemoryId := Goal.write_act_t

/- The new causal relations are source-owned, exact-version references. -/
def goal_motivations_valid
    (memories : Set Memory) (goal : Goal) : Prop :=
  ∀ id : MemoryId, id ∈ goal_motivated_by goal →
    ∃ p : Memory,
      p ∈ memories ∧ memory_t p = id ∧
      memory_kind p = .Perspective ∧
      memory_owner p = goal_owner goal

def perspective_guidance_valid
    (_memories : Set Memory) (goals : Set Goal) (p : Memory) : Prop :=
  memory_kind p = .Perspective →
    ∀ id : GoalId, id ∈ memory_guided_by p →
      ∃ g : Goal,
        g ∈ goals ∧ goal_t g = id ∧
        goal_owner g = memory_owner p

def goal_motivations_retained_valid
    (memories : Set Memory) (_goals : Set Goal)
    (targets : Set ErasedPinTarget) (goal : Goal) : Prop :=
  ∀ id : MemoryId, id ∈ goal_motivated_by goal →
    (∃ p : Memory,
      p ∈ memories ∧ memory_t p = id ∧
      memory_kind p = .Perspective ∧
      memory_owner p = goal_owner goal) ∨
    (∃ e : ErasedPinTarget,
      e ∈ targets ∧ erased_pin_target_t e = id ∧
      erased_pin_target_kind e = .Perspective)

def perspective_guidance_retained_valid
    (_memories : Set Memory) (goals : Set Goal)
    (targets : Set ErasedPinTarget) (p : Memory) : Prop :=
  memory_kind p = .Perspective →
    ∀ id : GoalId, id ∈ memory_guided_by p →
      (∃ g : Goal,
        g ∈ goals ∧ goal_t g = id ∧
        goal_owner g = memory_owner p) ∨
      (∃ e : ErasedPinTarget,
        e ∈ targets ∧ erased_pin_target_t e = id ∧
        erased_pin_target_kind e = .Goal)

def ErasedTargetsDisjointGoals
    (targets : Set ErasedPinTarget) (goals : Set Goal) : Prop :=
  ∀ e : ErasedPinTarget, e ∈ targets →
    ∀ g : Goal, g ∈ goals → erased_pin_target_t e ≠ goal_t g

/-- Declared pins to other nodes (ids only). No edge table. -/
def goalDeclaredTargetIds (g : Goal) : List Id :=
  (goal_assignment g).toList ++ goal_dependencies g ++ goal_evidence g ++
    goal_motivated_by g ++
    (goal_close_fact_t g).toList ++ (goal_write_act_t g).toList

def GoalIdUnique (goals : Set Goal) : Prop :=
  ∀ g1 g2 : Goal,
    g1 ∈ goals →
    g2 ∈ goals →
    goal_t g1 = goal_t g2 →
    g1 = g2

instance : AppendOnly Goal := ⟨⟩

-- ============================================================
-- No open goal under a closed one: the step model (refs #514)
-- ============================================================

/-- Active and Paused are open; Achieved and Abandoned are closed. -/
def GoalState.isOpen : GoalState → Bool
  | .Active | .Paused => true
  | .Achieved | .Abandoned => false

/-- The goal tree at one moment: one entry per series, holding its head
    state. `parent` is a function of the handle:
    every version of a series names the same parent. `born` orders series by creation; `clock` is the next birth. -/
structure GoalForest where
  state  : Handle → Option GoalState
  parent : Handle → Option Handle
  born   : Handle → Nat
  clock  : Nat

def forestChildOf (F : GoalForest) (c p : Handle) : Prop :=
  (F.state c).isSome = true ∧ F.parent c = some p

/-- An open goal never sits under a closed one. -/
def NoOpenUnderClosed (F : GoalForest) : Prop :=
  ∀ (c p : Handle) (sc sp : GoalState),
    F.parent c = some p → F.state c = some sc → F.state p = some sp →
    sc.isOpen = true → sp.isOpen = true

/-- A live child's parent is live. -/
def ParentsExist (F : GoalForest) : Prop :=
  ∀ c p : Handle, (F.state c).isSome = true → F.parent c = some p →
    (F.state p).isSome = true

/-- A parent was born before its child (the parent exists first). -/
def ParentOlder (F : GoalForest) : Prop :=
  ∀ c p : Handle, forestChildOf F c p → F.born p < F.born c

/-- Every live series was born before the clock. -/
def BornBelowClock (F : GoalForest) : Prop :=
  ∀ h : Handle, (F.state h).isSome = true → F.born h < F.clock

structure GoalForestInv (F : GoalForest) : Prop where
  noOpenUnderClosed : NoOpenUnderClosed F
  parentsExist      : ParentsExist F
  parentOlder       : ParentOlder F
  bornBelowClock    : BornBelowClock F

/-- No child head of `x` is open: the precondition of achieve and abandon. -/
def NoOpenChild (F : GoalForest) (x : Handle) : Prop :=
  ∀ (c : Handle) (s : GoalState), F.parent c = some x → F.state c = some s →
    s.isOpen = false

/-- A new series `x` is born Active under `parent`; nothing else moves. -/
structure ForestBirth (F F' : GoalForest) (x : Handle) (parent : Option Handle) : Prop where
  fresh     : F.state x = none
  state_x   : F'.state x = some .Active
  parent_x  : F'.parent x = parent
  born_x    : F'.born x = F.clock
  clock     : F'.clock = F.clock + 1
  state_eq  : ∀ h, h ≠ x → F'.state h = F.state h
  parent_eq : ∀ h, h ≠ x → F'.parent h = F.parent h
  born_eq   : ∀ h, h ≠ x → F'.born h = F.born h

/-- Series `x` moves from `old` to `new`; parents, births, the clock and
    every other series stay. -/
structure ForestRelabel (F F' : GoalForest) (x : Handle) (old new : GoalState) : Prop where
  state_old : F.state x = some old
  state_x   : F'.state x = some new
  state_eq  : ∀ h, h ≠ x → F'.state h = F.state h
  parent    : F'.parent = F.parent
  born      : F'.born = F.born
  clock     : F'.clock = F.clock

/-- One goal write. Each step states only its local precondition: decompose
    needs an Active parent, achieve and abandon need no open child (abandon
    takes an Active or Paused goal), and pause, resume and modify need
    nothing new. -/
inductive GoalStep : GoalForest → GoalForest → Prop where
  | set {F F' : GoalForest} (x : Handle) :
      ForestBirth F F' x none → GoalStep F F'
  | decompose {F F' : GoalForest} (p x : Handle) :
      F.state p = some .Active → ForestBirth F F' x (some p) → GoalStep F F'
  | pause {F F' : GoalForest} (x : Handle) :
      ForestRelabel F F' x .Active .Paused → GoalStep F F'
  | resume {F F' : GoalForest} (x : Handle) :
      ForestRelabel F F' x .Paused .Active → GoalStep F F'
  | modify {F F' : GoalForest} (x : Handle) :
      ForestRelabel F F' x .Active .Active → GoalStep F F'
  | achieve {F F' : GoalForest} (x : Handle) :
      NoOpenChild F x → ForestRelabel F F' x .Active .Achieved → GoalStep F F'
  | abandon {F F' : GoalForest} (x : Handle) {old : GoalState} :
      old.isOpen = true → NoOpenChild F x → ForestRelabel F F' x old .Abandoned →
      GoalStep F F'

theorem relabel_isSome {F F' : GoalForest} {x : Handle} {old new : GoalState}
    (r : ForestRelabel F F' x old new) (h : Handle) :
    (F'.state h).isSome = (F.state h).isSome := by
  cases Classical.em (h = x) with
  | inl hx => subst hx; rw [r.state_x, r.state_old]; rfl
  | inr hx => rw [r.state_eq h hx]

/-- The parts of the invariant a relabel keeps whatever the new state. -/
theorem relabel_keeps_shape {F F' : GoalForest} {x : Handle} {old new : GoalState}
    (inv : GoalForestInv F) (r : ForestRelabel F F' x old new) :
    ParentsExist F' ∧ ParentOlder F' ∧ BornBelowClock F' := by
  refine ⟨?_, ?_, ?_⟩
  · intro c p hc hp
    rw [relabel_isSome r] at hc ⊢
    rw [r.parent] at hp
    exact inv.parentsExist c p hc hp
  · intro c p ⟨hc, hp⟩
    rw [relabel_isSome r] at hc
    rw [r.parent] at hp
    rw [r.born]
    exact inv.parentOlder c p ⟨hc, hp⟩
  · intro h hh
    rw [relabel_isSome r] at hh
    rw [r.born, r.clock]
    exact inv.bornBelowClock h hh

/-- A relabel from an open state to an open state keeps the invariant. -/
theorem open_relabel_preserves {F F' : GoalForest} {x : Handle} {old new : GoalState}
    (inv : GoalForestInv F) (r : ForestRelabel F F' x old new)
    (hold : old.isOpen = true) (hnew : new.isOpen = true) : GoalForestInv F' := by
  obtain ⟨hpe, hpo, hbc⟩ := relabel_keeps_shape inv r
  refine ⟨?_, hpe, hpo, hbc⟩
  intro c p sc sp hp hc hsp hopen
  rw [r.parent] at hp
  cases Classical.em (p = x) with
  | inl hpx =>
    subst hpx
    rw [r.state_x] at hsp
    cases hsp
    exact hnew
  | inr hpx =>
    rw [r.state_eq p hpx] at hsp
    cases Classical.em (c = x) with
    | inl hcx =>
      subst hcx
      exact inv.noOpenUnderClosed c p old sp hp r.state_old hsp hold
    | inr hcx =>
      rw [r.state_eq c hcx] at hc
      exact inv.noOpenUnderClosed c p sc sp hp hc hsp hopen

/-- A relabel to a closed state under `NoOpenChild` keeps the invariant. -/
theorem closing_relabel_preserves {F F' : GoalForest} {x : Handle} {old new : GoalState}
    (inv : GoalForestInv F) (hchild : NoOpenChild F x) (r : ForestRelabel F F' x old new)
    (hnew : new.isOpen = false) : GoalForestInv F' := by
  obtain ⟨hpe, hpo, hbc⟩ := relabel_keeps_shape inv r
  refine ⟨?_, hpe, hpo, hbc⟩
  intro c p sc sp hp hc hsp hopen
  rw [r.parent] at hp
  cases Classical.em (c = x) with
  | inl hcx =>
    subst hcx
    rw [r.state_x] at hc
    cases hc
    rw [hnew] at hopen
    cases hopen
  | inr hcx =>
    rw [r.state_eq c hcx] at hc
    cases Classical.em (p = x) with
    | inl hpx =>
      subst hpx
      rw [hchild c sc hp hc] at hopen
      cases hopen
    | inr hpx =>
      rw [r.state_eq p hpx] at hsp
      exact inv.noOpenUnderClosed c p sc sp hp hc hsp hopen

/-- A birth under no parent or under an Active parent keeps the invariant. -/
theorem birth_preserves {F F' : GoalForest} {x : Handle} {parent : Option Handle}
    (inv : GoalForestInv F) (b : ForestBirth F F' x parent)
    (hparent : ∀ p, parent = some p → F.state p = some .Active) : GoalForestInv F' := by
  -- A live series of `F` is never `x`, and no live series of `F` has `x` as parent.
  have live_ne : ∀ h, (F.state h).isSome = true → h ≠ x := by
    intro h hh hx
    subst hx
    rw [b.fresh] at hh
    cases hh
  have parent_ne : ∀ c p, (F.state c).isSome = true → F.parent c = some p → p ≠ x := by
    intro c p hc hp
    exact live_ne p (inv.parentsExist c p hc hp)
  have isSome' : ∀ h, h ≠ x → (F'.state h).isSome = (F.state h).isSome := by
    intro h hx
    rw [b.state_eq h hx]
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro c p sc sp hp hc hsp hopen
    cases Classical.em (c = x) with
    | inl hcx =>
      subst hcx
      rw [b.parent_x] at hp
      have hpa := hparent p hp
      have hpx : p ≠ c := live_ne p (by rw [hpa]; rfl)
      rw [b.state_eq p hpx, hpa] at hsp
      cases hsp
      rfl
    | inr hcx =>
      rw [b.parent_eq c hcx] at hp
      rw [b.state_eq c hcx] at hc
      have hpx := parent_ne c p (by rw [hc]; rfl) hp
      rw [b.state_eq p hpx] at hsp
      exact inv.noOpenUnderClosed c p sc sp hp hc hsp hopen
  · intro c p hc hp
    cases Classical.em (c = x) with
    | inl hcx =>
      subst hcx
      rw [b.parent_x] at hp
      have hpa := hparent p hp
      have hpx : p ≠ c := live_ne p (by rw [hpa]; rfl)
      rw [isSome' p hpx, hpa]
      rfl
    | inr hcx =>
      rw [isSome' c hcx] at hc
      rw [b.parent_eq c hcx] at hp
      have hpx := parent_ne c p hc hp
      rw [isSome' p hpx]
      exact inv.parentsExist c p hc hp
  · intro c p ⟨hc, hp⟩
    cases Classical.em (c = x) with
    | inl hcx =>
      subst hcx
      rw [b.parent_x] at hp
      have hpa := hparent p hp
      have hlive : (F.state p).isSome = true := by rw [hpa]; rfl
      have hpx : p ≠ c := live_ne p hlive
      rw [b.born_eq p hpx, b.born_x]
      exact inv.bornBelowClock p hlive
    | inr hcx =>
      rw [isSome' c hcx] at hc
      rw [b.parent_eq c hcx] at hp
      have hpx := parent_ne c p hc hp
      rw [b.born_eq c hcx, b.born_eq p hpx]
      exact inv.parentOlder c p ⟨hc, hp⟩
  · intro h hh
    rw [b.clock]
    cases Classical.em (h = x) with
    | inl hx =>
      subst hx
      rw [b.born_x]
      exact Nat.lt_succ_self _
    | inr hx =>
      rw [isSome' h hx] at hh
      rw [b.born_eq h hx]
      exact Nat.lt_succ_of_lt (inv.bornBelowClock h hh)

theorem set_preserves {F F' : GoalForest} {x : Handle}
    (inv : GoalForestInv F) (b : ForestBirth F F' x none) : GoalForestInv F' :=
  birth_preserves inv b (fun _ h => by cases h)

theorem decompose_preserves {F F' : GoalForest} {p x : Handle}
    (inv : GoalForestInv F) (hp : F.state p = some .Active)
    (b : ForestBirth F F' x (some p)) : GoalForestInv F' :=
  birth_preserves inv b (fun q h => by cases h; exact hp)

theorem pause_preserves {F F' : GoalForest} {x : Handle}
    (inv : GoalForestInv F) (r : ForestRelabel F F' x .Active .Paused) : GoalForestInv F' :=
  open_relabel_preserves inv r rfl rfl

theorem resume_preserves {F F' : GoalForest} {x : Handle}
    (inv : GoalForestInv F) (r : ForestRelabel F F' x .Paused .Active) : GoalForestInv F' :=
  open_relabel_preserves inv r rfl rfl

theorem modify_preserves {F F' : GoalForest} {x : Handle}
    (inv : GoalForestInv F) (r : ForestRelabel F F' x .Active .Active) : GoalForestInv F' :=
  open_relabel_preserves inv r rfl rfl

theorem achieve_preserves {F F' : GoalForest} {x : Handle}
    (inv : GoalForestInv F) (hchild : NoOpenChild F x)
    (r : ForestRelabel F F' x .Active .Achieved) : GoalForestInv F' :=
  closing_relabel_preserves inv hchild r rfl

theorem abandon_preserves {F F' : GoalForest} {x : Handle} {old : GoalState}
    (inv : GoalForestInv F) (hchild : NoOpenChild F x)
    (r : ForestRelabel F F' x old .Abandoned) : GoalForestInv F' :=
  closing_relabel_preserves inv hchild r rfl

theorem goalStep_preserves_inv :
    ∀ {F F' : GoalForest}, GoalForestInv F → GoalStep F F' → GoalForestInv F' := by
  intro F F' inv step
  cases step with
  | set _ b => exact set_preserves inv b
  | decompose _ _ hp b => exact decompose_preserves inv hp b
  | pause _ r => exact pause_preserves inv r
  | resume _ r => exact resume_preserves inv r
  | modify _ r => exact modify_preserves inv r
  | achieve _ hchild r => exact achieve_preserves inv hchild r
  | abandon _ _ hchild r => exact abandon_preserves inv hchild r

def emptyForest : GoalForest where
  state  := fun _ => none
  parent := fun _ => none
  born   := fun _ => 0
  clock  := 0

inductive GoalForestReachable : GoalForest → Prop where
  | empty : GoalForestReachable emptyForest
  | step {F F' : GoalForest} : GoalForestReachable F → GoalStep F F' → GoalForestReachable F'

theorem emptyForest_inv : GoalForestInv emptyForest := by
  refine ⟨?_, ?_, ?_, ?_⟩
  · intro _ _ _ _ _ hc
    cases hc
  · intro _ _ hc
    cases hc
  · intro _ _ ⟨hc, _⟩
    cases hc
  · intro _ hh
    cases hh

theorem reachable_inv : ∀ {F : GoalForest}, GoalForestReachable F → GoalForestInv F := by
  intro F h
  induction h with
  | empty => exact emptyForest_inv
  | step _ step ih => exact goalStep_preserves_inv ih step

/-- `d` lies below `a`: the transitive closure of `forestChildOf`. -/
inductive GoalDescends (F : GoalForest) : Handle → Handle → Prop where
  | child {d a : Handle} : forestChildOf F d a → GoalDescends F d a
  | trans {d m a : Handle} : forestChildOf F d m → GoalDescends F m a → GoalDescends F d a

/-- Every goal below a closed goal is closed, at any depth. -/
theorem closed_ancestor_closes_descendant :
    ∀ {F : GoalForest} {d a : Handle} {sa : GoalState},
      GoalForestInv F → GoalDescends F d a →
      F.state a = some sa → sa.isOpen = false →
      ∃ sd, F.state d = some sd ∧ sd.isOpen = false := by
  intro F d a sa inv hdesc ha hclosed
  -- One level: a live child of a closed goal is closed.
  have step : ∀ c p sp, forestChildOf F c p → F.state p = some sp → sp.isOpen = false →
      ∃ sc, F.state c = some sc ∧ sc.isOpen = false := by
    intro c p sp ⟨hc, hp⟩ hsp hpc
    cases hstate : F.state c with
    | none => rw [hstate] at hc; cases hc
    | some sc =>
      refine ⟨sc, rfl, ?_⟩
      cases ho : sc.isOpen with
      | false => rfl
      | true =>
        have := inv.noOpenUnderClosed c p sc sp hp hstate hsp ho
        rw [hpc] at this
        cases this
  induction hdesc with
  | child h => exact step _ _ sa h ha hclosed
  | trans h _ ih =>
    obtain ⟨sm, hm, hmc⟩ := ih ha
    exact step _ _ sm h hm hmc

theorem descends_born_lt :
    ∀ {F : GoalForest} {d a : Handle},
      GoalForestInv F → GoalDescends F d a → F.born a < F.born d := by
  intro F d a inv h
  induction h with
  | child hc => exact inv.parentOlder _ _ hc
  | trans hc _ ih => exact Nat.lt_trans ih (inv.parentOlder _ _ hc)

/-- No goal lies below itself. -/
theorem goal_tree_acyclic :
    ∀ {F : GoalForest} {h : Handle}, GoalForestInv F → ¬ GoalDescends F h h := by
  intro F h inv hdesc
  exact Nat.lt_irrefl _ (descends_born_lt inv hdesc)

-- ============================================================
-- Self as query, not entity (S3: cue-indexed, not parameterless)
-- ============================================================

/-- Candidate pool — owner's Perspectives. Not Self. -/
def ownerPerspectives (memories : Set Memory) (o : Owner) : Set Memory :=
  fun m => m ∈ memories ∧ memory_owner m = o ∧ memory_kind m = .Perspective

/-- Situation touches this admission: the `t` is in the cue, or a pin is. -/
def cueTouches (m : Memory) (cue : Cue) : Prop :=
  memory_t m ∈ cue ∨
  (∃ id : MemoryId, id ∈ cue ∧ (id ∈ memory_origins m ∨ id ∈ memory_refs m))

/-- Self for a situation. Different cues, different Self. Question text is
    protocol; the kernel face of situation is `Cue`. -/
def situatedSelf
    (memories : Set Memory) (heads : Set MemoryHead)
    (o : Owner) (cue : Cue) : Set Memory :=
  fun m =>
    m ∈ ownerPerspectives memories o ∧
    memoryIsHead memories heads m ∧
    cueTouches m cue

-- ============================================================
-- Goal assignment and evidence
-- ============================================================

def RetainedGoalAssignmentValid
    (goals : Set Goal) (memories : Set Memory)
  (targets : Set ErasedPinTarget) : Prop :=
  ∀ g : Goal, g ∈ goals →
    goal_assignment g = none ∨
    (∃ m : Memory,
      m ∈ memories ∧
      goal_assignment g = some (memory_t m) ∧
      memory_kind m = .Perspective ∧
      memory_owner m = goal_owner g) ∨
    (∃ e : ErasedPinTarget,
      e ∈ targets ∧
      ∃ id : MemoryId,
        goal_assignment g = some id ∧
        erased_pin_target_t e = id ∧
      erased_pin_target_kind e = .Perspective)

def RetainedGoalEvidenceValid
    (goals : Set Goal) (memories : Set Memory)
    (targets : Set ErasedPinTarget) : Prop :=
  ∀ g : Goal, g ∈ goals → ∀ i : MemoryId, i ∈ goal_evidence g →
    (∃ m : Memory, m ∈ memories ∧ memory_t m = i ∧
      (memory_kind m = .Fact ∨ memory_kind m = .Abstraction)) ∨
    (∃ e : ErasedPinTarget, e ∈ targets ∧ erased_pin_target_t e = i ∧
      (erased_pin_target_kind e = .Fact ∨ erased_pin_target_kind e = .Abstraction))

def RetainedGoalTerminalCloseFactValid
    (goals : Set Goal) (memories : Set Memory)
    (targets : Set ErasedPinTarget) : Prop :=
  ∀ g : Goal, g ∈ goals → (goal_state g).terminal = true →
    (∃ m : Memory,
      m ∈ memories ∧
      goal_close_fact_t g = some (memory_t m) ∧
      memory_kind m = .Fact ∧
      memory_owner m = goal_owner g) ∨
    (∃ e : ErasedPinTarget,
      e ∈ targets ∧
      goal_close_fact_t g = some (erased_pin_target_t e) ∧
      erased_pin_target_kind e = .Fact)

end Causa
