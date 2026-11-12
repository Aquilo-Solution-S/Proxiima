use crate::OwnerAccess;

/// What calling a tool, or one action of a dispatcher, does to state.
///
/// THE behaviour declaration: a flat tool declares one
/// ([`crate::Tool::EFFECT`]), a dispatcher one per action
/// (`ActionArgSpec::effect`, `ArgvActionSpec::effect`), a host tool one
/// (`HostTool::effect`). Every other answer is derived from it:
///
/// | Reader | Derived answer |
/// |---|---|
/// | owner-role gate | [`Self::ReadOnly`] needs read access, anything else write |
/// | MCP / REST / `OpenAPI` hints | `ToolAnnotations::registered`, `ToolAnnotations::host` |
/// | REST `QUERY` | admitted for [`Self::ReadOnly`] only |
/// | [`crate::UnitOfWork::erase_own_series`] | admits only a call whose action is [`Self::Destructive`] |
///
/// Ordered by strength, `ReadOnly < Additive < Destructive`; a dispatcher's
/// tool-level effect is the [`Self::join`] of its actions, so a dispatcher
/// with one destructive action is a destructive tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolEffect {
    /// Reads only; idempotent by construction.
    ReadOnly,
    /// Writes; removes nothing it did not create.
    Additive(Replay),
    /// May remove or overwrite what it did not create.
    Destructive(Replay),
}

/// What an identical second call does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Replay {
    /// Nothing further: the second call lands on the first one's result.
    Idempotent,
    /// Acts again.
    NonIdempotent,
}

impl ToolEffect {
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::ReadOnly)
    }

    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Destructive(_))
    }

    /// [`Self::ReadOnly`] replays as [`Replay::Idempotent`].
    #[must_use]
    pub const fn replay(self) -> Replay {
        match self {
            Self::ReadOnly => Replay::Idempotent,
            Self::Additive(replay) | Self::Destructive(replay) => replay,
        }
    }

    const fn strength(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::Additive(_) => 1,
            Self::Destructive(_) => 2,
        }
    }

    /// The effect of a surface that may run either: the stronger kind, and
    /// idempotent only when both are.
    #[must_use]
    pub const fn join(self, other: Self) -> Self {
        let replay = match (self.replay(), other.replay()) {
            (Replay::Idempotent, Replay::Idempotent) => Replay::Idempotent,
            _ => Replay::NonIdempotent,
        };
        let strongest = if self.strength() >= other.strength() {
            self
        } else {
            other
        };
        match strongest {
            Self::ReadOnly => Self::ReadOnly,
            Self::Additive(_) => Self::Additive(replay),
            Self::Destructive(_) => Self::Destructive(replay),
        }
    }

    /// [`Self::join`] over `effects`; `None` when there are none.
    #[must_use]
    pub fn strongest(effects: impl IntoIterator<Item = Self>) -> Option<Self> {
        effects.into_iter().reduce(Self::join)
    }
}

/// Who a tool or action is for.
///
/// Hosts that compute separate tool surfaces per audience partition on this
/// declaration instead of hardcoding tool names: a non-owner caller scope
/// leaves the owner's keys out, including reads. An enum rather than a flag
/// so the declaration site names its
/// meaning, and a future audience is a new variant instead of a second bool
/// that has to be read together with the first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolAudience {
    /// Eligible for every surface a host computes. The default: saying
    /// nothing does not narrow anything.
    #[default]
    Shared,
    /// Belongs to the owner alone; a host computing a surface for any
    /// non-owner audience leaves this key out.
    Owner,
}

/// What a key asks of the owner a call selects, beside what it does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OwnerNeed {
    /// Memory access by the key's effect: read for a read, Fact write for
    /// a write. The default: saying nothing widens nothing.
    #[default]
    Memory,
    /// The key's write is Goal work: every member who
    /// [works Goals](crate::Role::works_goals) runs and sees it, not only a
    /// member who writes Facts. It widens no memory write: what the tool
    /// writes to Facts, Perspectives or host state keeps the caller's
    /// ceiling. A read stays a read.
    GoalWork,
}

/// What one scope key declares: what it does, who it is for and what it
/// asks of the owner. The scope gate, the owner-role gate and every listing
/// judge a key by this one value ([`CapabilitySet::admits`](crate::CapabilitySet::admits),
/// [`Self::owner_access`], `ToolDescriptor::declaration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDeclaration {
    /// `None` for a name nothing declares, which every gate reads as a write.
    pub effect: Option<ToolEffect>,
    pub audience: ToolAudience,
    pub need: OwnerNeed,
}

impl KeyDeclaration {
    /// A name nothing declares: a write.
    pub const UNDECLARED: Self = Self::shared(None);

    /// A declaration for every audience: a core resource's, a host tool's.
    #[must_use]
    pub const fn shared(effect: Option<ToolEffect>) -> Self {
        Self {
            effect,
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
        }
    }

    /// Whether the key declares itself a read. Silence is a write.
    #[must_use]
    pub const fn is_read(self) -> bool {
        matches!(self.effect, Some(effect) if effect.is_read_only())
    }

    /// What a call of this key asks of the owner it selects: the one rule
    /// the owner-role gate and every listing share.
    #[must_use]
    pub const fn owner_access(self) -> OwnerAccess {
        match (self.is_read(), self.need) {
            (true, OwnerNeed::Memory | OwnerNeed::GoalWork) => OwnerAccess::Read,
            (false, OwnerNeed::Memory) => OwnerAccess::Write,
            (false, OwnerNeed::GoalWork) => OwnerAccess::GoalWork,
        }
    }

    /// Whether this read is declared for every audience. Caller scope
    /// membership is still required for both shared and owner reads.
    #[must_use]
    pub fn is_open_read(self) -> bool {
        self.is_read() && self.audience == ToolAudience::Shared
    }
}

#[cfg(test)]
pub(crate) const ALL_EFFECTS: [ToolEffect; 5] = [
    ToolEffect::ReadOnly,
    ToolEffect::Additive(Replay::Idempotent),
    ToolEffect::Additive(Replay::NonIdempotent),
    ToolEffect::Destructive(Replay::Idempotent),
    ToolEffect::Destructive(Replay::NonIdempotent),
];

#[cfg(test)]
mod tests {
    use super::{ALL_EFFECTS, Replay, ToolEffect};

    /// A lattice join: commutative, idempotent, associative, with
    /// `ReadOnly` as its bottom — so a dispatcher's effect does not depend
    /// on the order its actions are declared in.
    #[test]
    fn join_is_a_lattice_join_with_read_only_at_the_bottom() {
        for a in ALL_EFFECTS {
            assert_eq!(a.join(a), a);
            assert_eq!(a.join(ToolEffect::ReadOnly), a);
            for b in ALL_EFFECTS {
                assert_eq!(a.join(b), b.join(a));
                for c in ALL_EFFECTS {
                    assert_eq!(a.join(b).join(c), a.join(b.join(c)));
                }
            }
        }
    }

    #[test]
    fn join_takes_the_stronger_kind_and_idempotence_only_from_both() {
        assert_eq!(
            ToolEffect::Additive(Replay::Idempotent)
                .join(ToolEffect::Destructive(Replay::Idempotent)),
            ToolEffect::Destructive(Replay::Idempotent)
        );
        assert_eq!(
            ToolEffect::Destructive(Replay::Idempotent)
                .join(ToolEffect::Additive(Replay::NonIdempotent)),
            ToolEffect::Destructive(Replay::NonIdempotent)
        );
        assert_eq!(
            ToolEffect::ReadOnly.join(ToolEffect::Additive(Replay::Idempotent)),
            ToolEffect::Additive(Replay::Idempotent),
            "a read replays idempotently, so it does not spoil a sibling's idempotence"
        );
        assert_eq!(ToolEffect::strongest([]), None);
    }
}
