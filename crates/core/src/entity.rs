//! Shared entity-kind and endpoint types used by graph and embedding queries.

use uuid::Uuid;

use crate::{GoalId, MemoryId};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum EntityKind {
    Fact,
    Abstraction,
    Perspective,
    Goal,
}

/// The memory layer of a node: a Fact, an Abstraction or a Perspective.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
pub enum MemoryKind {
    #[default]
    Fact,
    Abstraction,
    Perspective,
}

impl MemoryKind {
    pub const ALL: [Self; 3] = [Self::Fact, Self::Abstraction, Self::Perspective];

    #[must_use]
    pub const fn entity_kind(self) -> EntityKind {
        match self {
            Self::Fact => EntityKind::Fact,
            Self::Abstraction => EntityKind::Abstraction,
            Self::Perspective => EntityKind::Perspective,
        }
    }

    /// The memory kind of an entity kind; a Goal is not a memory.
    #[must_use]
    pub const fn of(kind: EntityKind) -> Option<Self> {
        match kind {
            EntityKind::Fact => Some(Self::Fact),
            EntityKind::Abstraction => Some(Self::Abstraction),
            EntityKind::Perspective => Some(Self::Perspective),
            EntityKind::Goal => None,
        }
    }
}

impl From<MemoryKind> for EntityKind {
    fn from(kind: MemoryKind) -> Self {
        kind.entity_kind()
    }
}

impl EntityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "Fact",
            Self::Abstraction => "Abstraction",
            Self::Perspective => "Perspective",
            Self::Goal => "Goal",
        }
    }
}

/// Entity kinds admitted to the vector infrastructure.
///
/// Fact entities and edges are not embeddable. Memory embeddings keep the
/// memory layer explicit; Goal embeddings use the Goal id directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EmbeddableEntityRef {
    Memory {
        kind: EntityKind,
        memory_id: MemoryId,
    },
    Goal(GoalId),
}

impl EmbeddableEntityRef {
    #[must_use]
    pub const fn entity_kind(self) -> EntityKind {
        match self {
            Self::Memory { kind, .. } => kind,
            Self::Goal(_) => EntityKind::Goal,
        }
    }

    #[must_use]
    pub const fn entity_id(self) -> Uuid {
        match self {
            Self::Memory { memory_id, .. } => memory_id.into_inner(),
            Self::Goal(goal_id) => goal_id.into_inner(),
        }
    }
}

/// Endpoint of a pin: a memory `t` or a Goal `t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum EntityRef {
    Memory(MemoryId),
    Goal(GoalId),
}

#[cfg(test)]
mod memory_kind_tests {
    use super::*;

    #[test]
    fn memory_kind_speaks_the_entity_kind_words() {
        for kind in MemoryKind::ALL {
            let json = serde_json::to_string(&kind).expect("serializes");
            assert_eq!(json, format!("\"{}\"", kind.entity_kind().as_str()));
            assert_eq!(serde_json::from_str::<MemoryKind>(&json).ok(), Some(kind));
            assert_eq!(MemoryKind::of(kind.entity_kind()), Some(kind));
        }
        assert_eq!(MemoryKind::default(), MemoryKind::Fact);
        assert_eq!(MemoryKind::of(EntityKind::Goal), None);
    }

    #[test]
    fn memory_kind_refuses_a_word_outside_the_wire_spelling() {
        for bad in [
            "\"Goal\"",
            "\"fact\"",
            "\"FACT\"",
            "\"perspective\"",
            "\"Note\"",
        ] {
            assert!(
                serde_json::from_str::<MemoryKind>(bad).is_err(),
                "{bad} must be refused"
            );
        }
    }
}
