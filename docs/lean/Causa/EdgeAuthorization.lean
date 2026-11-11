/-
Causa — Pin authorization

Writer can read each pinned `t` (UML §2 `read`). No edge row. Source-owned
means the pin lives on the declaring Memory/Goal. Target render is a
separate gate (live / forgotten / unavailable).
-/

import Causa.Edges
import Causa.Authorization
import Causa.Audience

namespace Causa

def MemoryKind.accessKind : MemoryKind → AccessKind
  | .Fact => .fact
  | .Abstraction => .abstraction
  | .Perspective => .perspective

/-- Seeing the declaring row is source-local. -/
def pin_source_read_admitted (requester : User) (source : Memory) : Prop :=
  may_read requester (memory_owner source) (memory_kind source).accessKind

/-- Target projection: the pinned row may be shown only if readable. -/
def pin_target_readable (requester : User) (target : Memory) : Prop :=
  may_read requester (memory_owner target) (memory_kind target).accessKind

/-- Uniform write admission: source write + target read. -/
def pin_write_admitted (requester : User) (source target : Memory) : Prop :=
  may_write requester (memory_owner source) (memory_kind source).accessKind ∧
  may_read requester (memory_owner target) (memory_kind target).accessKind

theorem pin_write_admitted_source_write :
    ∀ requester source target,
      pin_write_admitted requester source target →
      may_write requester (memory_owner source) (memory_kind source).accessKind := by
  intro _ _ _ h
  exact h.1

theorem pin_write_admitted_target_read :
    ∀ requester source target,
      pin_write_admitted requester source target →
      may_read requester (memory_owner target) (memory_kind target).accessKind := by
  intro _ _ _ h
  exact h.2

/-- Cross-owner targets are admitted when readable. -/
theorem cross_owner_target_admitted :
    ∀ requester source target,
      pin_write_admitted requester source target →
      may_read requester (memory_owner target) (memory_kind target).accessKind :=
  pin_write_admitted_target_read

/-- Goal pin: the writer works the Goal owner's Goals or administers that
    owner, whichever the Goal operation needs, and reads the named Memory. -/
def goal_pin_write_admitted (requester : User) (g : Goal) (target : Memory) : Prop :=
  (may_work_goals requester (goal_owner g) ∨ may_administer requester (goal_owner g)) ∧
  may_read requester (memory_owner target) (memory_kind target).accessKind

/- ================================================================
   Goal collaboration authority

   These predicates are deliberately separate from `may_read` and
   `may_write`. A Goal audience is an additional, exact-version route; it is
   never inserted into an `Owner` or used to manufacture owner authority.
   ================================================================ -/

structure GoalControllerEpoch where
  goal_handle : Handle
  owner : Owner
  generation : Nat

structure GoalPublication where
  goal : GoalControllerEpoch
  memory_t : MemoryId
  source_owner : Owner
  source_generation : Nat
  controller_generation : Nat

/-- A grant on one Goal: a `Grant` (a person or a group, and a role) on one
    exact controller epoch. -/
structure GoalAudienceGrant extends Grant where
  goal : GoalControllerEpoch
  controller_generation : Nat

/-- The grant is on this exact Goal and carries the current controller
    generation. A grant past its expiry is not among the Goal's grants; the
    fence and the expiry are both facts checked again where the operation
    commits. -/
def GoalAudienceGrant.inForce (g : GoalAudienceGrant) (goal : GoalControllerEpoch)
    (current_controller_generation : Nat) : Prop :=
  g.goal = goal ∧ g.controller_generation = current_controller_generation

/-- The route every Goal operation shares: the owner's group gives the
    capability, or a grant in force on this exact Goal does. -/
def goal_route (s : OwnerState) (requester : User) (owner : Owner)
    (goal : GoalControllerEpoch) (generation : Nat)
    (grants : List GoalAudienceGrant) (c : Capability) : Prop :=
  owner.gives requester c ∨
    ∃ guest, guest ∈ grants ∧ guest.inForce goal generation ∧
      guest.toGrant.gives s requester c

structure GoalWorkBinding where
  goal : GoalControllerEpoch
  memory_handle : Handle
  source_owner : Owner
  source_generation : Nat
  controller_generation : Nat

/-- The audience route reads one exact admission.  A Goal's terminal state is
    intentionally absent: completion does not revoke a publication. -/
def goal_publication_read_admitted
    (s : OwnerState) (requester : User) (publication : GoalPublication)
    (grants : List GoalAudienceGrant) (current_controller_generation : Nat)
    (memory_owner_generation : Nat) (memory : Memory) : Prop :=
  goal_route s requester publication.goal.owner publication.goal
    current_controller_generation grants (.read memory.kind.accessKind) ∧
  publication.memory_t = memory_t memory ∧
  publication.source_owner = memory_owner memory ∧
  publication.goal.generation = current_controller_generation ∧
  publication.controller_generation = current_controller_generation ∧
  publication.source_generation = memory_owner_generation

/-- Reading the Goal itself: a role that reads the owner's Goals, or a current
    grant on this exact Goal whose subject gives the requester a role that
    reads Goals. This is not the uniform audience rule: it waives the grant's
    own Goal ceiling, so every guest reads the Goal it was granted on whatever
    the grant's role says about Goals. -/
def goal_read_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  (may_read_goals requester (goal_owner goal) ∨
    ∃ guest, guest ∈ grants ∧
      guest.inForce controller current_controller_generation ∧
      (s.subject guest.subject).gives requester .readsGoals)

/-- Modifying or transitioning an existing Goal is Goal work, under one rule
    for members and guests: the role works Goals. A guest's grant counts for
    this exact stable Goal and its current controller generation. Achieving
    the Goal is `goal_achieve_admitted`. -/
def goal_edit_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  goal_route s requester (goal_owner goal) controller
    current_controller_generation grants .worksGoals

/-- Whoever edits a Goal reads it. -/
theorem goal_edit_implies_read (s : OwnerState) (requester : User)
    (controller : GoalControllerEpoch) (grants : List GoalAudienceGrant)
    (goal : Goal) (generation : Nat) :
    goal_edit_admitted s requester controller grants goal generation →
      goal_read_admitted s requester controller grants goal generation := by
  rintro ⟨hhandle, howner, hgen, hroute⟩
  refine ⟨hhandle, howner, hgen, ?_⟩
  unfold goal_route at hroute
  cases hroute with
  | inl member =>
      obtain ⟨x, hx, hw⟩ := member
      exact Or.inl ⟨x, hx, x.worksGoals_readsGoals hw⟩
  | inr guest =>
      obtain ⟨g, hmem, hforce, hgives⟩ := guest
      exact Or.inr ⟨g, hmem, hforce,
        Group.gives_mono (c := .worksGoals) (d := .readsGoals)
          (fun r => r.worksGoals_readsGoals)
          ((Grant.gives_iff _ _ _ _).1 hgives).2⟩

theorem goal_edit_requires_exact_goal (s : OwnerState) (requester : User)
    (controller : GoalControllerEpoch) (grants : List GoalAudienceGrant)
    (goal : Goal) (generation : Nat) :
    goal_edit_admitted s requester controller grants goal generation →
      controller.goal_handle = goal_handle goal := by
  intro h
  exact h.1

theorem goal_edit_stale_controller_denied (s : OwnerState) (requester : User)
    (controller : GoalControllerEpoch) (grants : List GoalAudienceGrant)
    (goal : Goal) (generation : Nat)
    (h : controller.generation ≠ generation) :
    ¬ goal_edit_admitted s requester controller grants goal generation := by
  intro admitted
  exact h admitted.2.2.1

/-- Achieving a Goal closes it. Over the owner that is administration; on the
    Goal's own route it belongs to the grant that works this Goal. This is not
    the uniform audience rule: it asks the owner route for administration and
    the guest route for Goal work. A member's Goal work alone achieves
    nothing. -/
def goal_achieve_admitted (s : OwnerState) (requester : User) (controller : GoalControllerEpoch)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation : Nat) : Prop :=
  controller.goal_handle = goal_handle goal ∧
  controller.owner = goal_owner goal ∧
  controller.generation = current_controller_generation ∧
  (may_administer requester (goal_owner goal) ∨
    ∃ guest, guest ∈ grants ∧
      guest.inForce controller current_controller_generation ∧
      guest.toGrant.gives s requester .worksGoals)

/-- Without administration and without a grant on the Goal, nobody achieves
    it, whatever Goal work the role holds. -/
theorem goal_achieve_needs_administration_or_grant (s : OwnerState) (requester : User)
    (controller : GoalControllerEpoch) (goal : Goal) (generation : Nat)
    (h : ¬ may_administer requester (goal_owner goal)) :
    ¬ goal_achieve_admitted s requester controller [] goal generation := by
  rintro ⟨_, _, _, hroute⟩
  cases hroute with
  | inl admin => exact h admin
  | inr guest =>
      obtain ⟨g, hmem, _⟩ := guest
      cases hmem

/-- A work route is same-owner and write-ceiling bounded.  It grants no Goal
    administration, transfer, erase, or owner-membership capability. -/
def goal_work_write_admitted
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation memory_owner_generation : Nat)
    (memory : Memory) : Prop :=
  goal_route s requester (goal_owner goal) binding.goal
    current_controller_generation grants (.write memory.kind.accessKind) ∧
  binding.goal.goal_handle = goal_handle goal ∧
  binding.source_owner = goal_owner goal ∧
  binding.memory_handle = memory_handle memory ∧
  binding.source_owner = memory_owner memory ∧
  binding.goal.generation = current_controller_generation ∧
  binding.controller_generation = current_controller_generation ∧
  binding.source_generation = memory_owner_generation ∧
  goal_state goal = .Active

/-- Working-draft reads use the same exact binding but do not require the Goal
    to remain Active; completing a Goal is not an implicit access revocation. -/
def goal_work_read_admitted
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (current_controller_generation memory_owner_generation : Nat)
    (memory : Memory) : Prop :=
  goal_route s requester (goal_owner goal) binding.goal
    current_controller_generation grants (.read memory.kind.accessKind) ∧
  binding.goal.goal_handle = goal_handle goal ∧
  binding.source_owner = goal_owner goal ∧
  binding.memory_handle = memory_handle memory ∧
  binding.source_owner = memory_owner memory ∧
  binding.goal.generation = current_controller_generation ∧
  binding.controller_generation = current_controller_generation ∧
  binding.source_generation = memory_owner_generation

/-- One rule: for grants all in force, `goal_route` is exactly the flattened
    lookup of the owner joined with those grants. -/
theorem goal_route_is_flattened (s : OwnerState) (owner : Owner)
    (goal : GoalControllerEpoch) (generation : Nat)
    (grants : List GoalAudienceGrant)
    (hforce : ∀ g ∈ grants, g.inForce goal generation)
    (requester : User) (c : Capability) :
    goal_route s requester owner goal generation grants c ↔
      (flattened s owner (grants.map (·.toGrant))).gives requester c := by
  rw [flattened_gives]
  unfold goal_route
  constructor
  · rintro (h | ⟨g, hg, _, hs⟩)
    · exact Or.inl h
    · exact Or.inr ⟨g.toGrant, List.mem_map.2 ⟨g, hg, rfl⟩, hs⟩
  · rintro (h | ⟨_, hmap, hs⟩)
    · exact Or.inl h
    · obtain ⟨g, hg, rfl⟩ := List.mem_map.1 hmap
      exact Or.inr ⟨g, hg, hforce g hg, hs⟩

theorem goal_work_write_implies_read
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (controller_generation memory_generation : Nat) (memory : Memory) :
    goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation memory →
    goal_work_read_admitted s requester binding grants goal
      controller_generation memory_generation memory := by
  intro h
  rcases h with ⟨hroute, hgoal, howner, hhandle, hmemoryowner,
    hgoalgen, hbindctrl, hsource, _active⟩
  have hread :
      goal_route s requester (goal_owner goal) binding.goal
        controller_generation grants (.read memory.kind.accessKind) := by
    unfold goal_route at hroute ⊢
    cases hroute with
    | inl owner => exact Or.inl (may_write_implies_read _ _ _ owner)
    | inr guest =>
        rcases guest with ⟨g, hmem, hforce, hgives⟩
        exact Or.inr ⟨g, hmem, hforce,
          (Grant.gives_iff _ _ _ _).2
            ⟨Role.has_write_read _ _ ((Grant.gives_iff _ _ _ _).1 hgives).1,
              Group.gives_mono (fun r => Role.has_write_read r _)
                ((Grant.gives_iff _ _ _ _).1 hgives).2⟩⟩
  exact ⟨hread, hgoal, howner, hhandle, hmemoryowner,
    hgoalgen, hbindctrl, hsource⟩

/-- If no grant is in force and the owner does not read, the publication is not
    admitted. -/
theorem goal_controller_generation_mismatch_denies_publication
    (s : OwnerState) (requester : User) (p : GoalPublication)
    (grants : List GoalAudienceGrant)
    (current_controller_generation memory_generation : Nat) (memory : Memory)
    (howner : ¬ may_read requester p.goal.owner memory.kind.accessKind)
    (h : ∀ g ∈ grants, ¬ g.inForce p.goal current_controller_generation) :
    ¬ goal_publication_read_admitted s requester p grants
      current_controller_generation memory_generation memory := by
  intro admitted
  rcases admitted with ⟨route, _, _, _, _, _⟩
  rcases route with owner | ⟨g, hg, hforce, _⟩
  · exact howner owner
  · exact h g hg hforce

theorem goal_source_generation_mismatch_denies_publication
    (s : OwnerState) (requester : User) (p : GoalPublication)
    (grants : List GoalAudienceGrant)
    (controller_generation memory_generation : Nat) (memory : Memory)
    (h : p.source_generation ≠ memory_generation) :
    ¬ goal_publication_read_admitted s requester p grants
      controller_generation memory_generation memory := by
  intro admitted
  rcases admitted with ⟨_, _, _, _, _, hsource⟩
  exact h hsource

theorem goal_work_requires_same_owner
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (controller_generation memory_generation : Nat) (memory : Memory)
    (h : binding.source_owner ≠ goal_owner goal) :
    ¬ goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation memory := by
  intro admitted
  rcases admitted with ⟨_, _, howner, _, _, _, _, _, _⟩
  exact h howner

theorem goal_work_memory_handle_mismatch_denies
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (controller_generation memory_generation : Nat) (memory : Memory)
    (h : binding.memory_handle ≠ memory_handle memory) :
    ¬ goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation memory := by
  intro admitted
  rcases admitted with ⟨_, _, _, hhandle, _, _, _, _, _⟩
  exact h hhandle

theorem goal_work_memory_owner_mismatch_denies
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (controller_generation memory_generation : Nat) (memory : Memory)
    (h : binding.source_owner ≠ memory_owner memory) :
    ¬ goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation memory := by
  intro admitted
  rcases admitted with ⟨_, _, _, _, howner, _, _, _, _⟩
  exact h howner

theorem goal_work_controller_generation_mismatch_denies
    (s : OwnerState) (requester : User) (binding : GoalWorkBinding)
    (grants : List GoalAudienceGrant) (goal : Goal)
    (controller_generation memory_generation : Nat) (memory : Memory)
    (h : binding.controller_generation ≠ controller_generation) :
    ¬ goal_work_write_admitted s requester binding grants goal
      controller_generation memory_generation memory := by
  intro admitted
  rcases admitted with ⟨_, _, _, _, _, _, hbinding, _, _⟩
  exact h hbinding

structure ForkSourcePermission where
  requester : User
  source : Memory
  destination_owner : Owner
  source_generation : Nat
  destination_generation : Nat
  source_use : User → Prop
  destination_write : User → Prop

/-- An independent adaptation is a fresh A/P series anchored at one exact
    source version. Facts remain immutable admissions; Goals are not Memory
    fork targets. -/
def fork_admitted (permission : ForkSourcePermission)
    (source target : Memory)
    (current_source_generation current_destination_generation : Nat) : Prop :=
  memory_t source = memory_t permission.source ∧
  (memory_kind target = .Abstraction ∨ memory_kind target = .Perspective) ∧
  memory_kind source = memory_kind target ∧
  memory_handle source ≠ memory_handle target ∧
  memory_owner target = permission.destination_owner ∧
  permission.source_generation = current_source_generation ∧
  permission.destination_generation = current_destination_generation ∧
  memory_forked_from target = some (memory_t source) ∧
  memory_origins target = memory_origins source ∧
  memory_refs target = memory_refs source ∧
  memory_goal_refs target = memory_goal_refs source ∧
  memory_guided_by target = [] ∧
  permission.source_use permission.requester ∧
  permission.destination_write permission.requester

theorem fork_never_admits_fact_target
    (permission : ForkSourcePermission) (source target : Memory)
    (h : memory_kind target = .Fact) :
    ∀ source_generation destination_generation,
      ¬ fork_admitted permission source target source_generation destination_generation := by
  intro source_generation destination_generation
  intro admitted
  rcases admitted with ⟨_, htarget, _, _, _, _, _, _, _, _, _, _, _⟩
  cases htarget with
  | inl h_abs => simp_all
  | inr h_persp => simp_all

theorem fork_requires_fresh_series
    (permission : ForkSourcePermission) (source target : Memory)
    (h : memory_handle source = memory_handle target) :
    ∀ source_generation destination_generation,
      ¬ fork_admitted permission source target source_generation destination_generation := by
  intro source_generation destination_generation
  intro admitted
  rcases admitted with ⟨_, _, _, hfresh, _, _, _, _, _, _, _, _, _⟩
  exact hfresh h

theorem fork_requires_exact_anchor_and_copied_pins
    (permission : ForkSourcePermission) (source target : Memory)
    (source_generation destination_generation : Nat)
    (hadmitted : fork_admitted permission source target
      source_generation destination_generation) :
    memory_forked_from target = some (memory_t source) ∧
      memory_origins target = memory_origins source ∧
      memory_refs target = memory_refs source ∧
      memory_goal_refs target = memory_goal_refs source := by
  rcases hadmitted with
    ⟨_, _, _, _, _, _, _, hanchor, horigins, hrefs, hgoalrefs, _, _, _⟩
  exact ⟨hanchor, horigins, hrefs, hgoalrefs⟩

def fork_source_ancestor_read_admitted (requester : User) (source : Memory) : Prop :=
  may_read requester (memory_owner source) source.kind.accessKind

theorem fork_admission_preserves_separate_ancestor_read_gate
    (permission : ForkSourcePermission) (source target : Memory)
    (source_generation destination_generation : Nat)
    (hadmitted : fork_admitted permission source target
      source_generation destination_generation)
    (hancestor_unreadable :
      ¬ fork_source_ancestor_read_admitted permission.requester source) :
    fork_admitted permission source target source_generation destination_generation ∧
      ¬ fork_source_ancestor_read_admitted permission.requester source :=
  ⟨hadmitted, hancestor_unreadable⟩

/-- A forgotten pin target keeps its owner and kind, so the write admission
    onto it is the same rule as onto a live one (#488). -/
theorem pin_write_admitted_forgotten_target
    (requester : User) (source target : Memory) :
    pin_write_admitted requester source target.forget ↔
      pin_write_admitted requester source target :=
  Iff.rfl

/-- A requester dropped from every group a grant names is not admitted through
    any of them: the grants are unchanged, only the owner state differs, and
    only the owner's own route remains. -/
theorem goal_guest_group_drop_denies
    (s : OwnerState) (requester : User) (p : GoalPublication)
    (grants : List GoalAudienceGrant)
    (current_controller_generation memory_generation : Nat) (memory : Memory)
    (hsubject : ∀ g ∈ grants, ∃ o, g.subject = .group o ∧
      ∃ team : Group, s.resolve o = team.drop requester)
    (howner : ¬ may_read requester p.goal.owner memory.kind.accessKind) :
    ¬ goal_publication_read_admitted s requester p grants
      current_controller_generation memory_generation memory := by
  rintro ⟨route, _⟩
  rcases route with owner | ⟨g, hg, _, hgives⟩
  · exact howner owner
  · obtain ⟨o, ho, team, hteam⟩ := hsubject g hg
    have e : s.subject g.toGrant.subject = team.drop requester := by
      show s.subject g.subject = _
      rw [ho]; exact hteam
    exact Group.drop_gives_self _ _ _ (e ▸ ((Grant.gives_iff _ _ _ _).1 hgives).2)

end Causa
