use crate::{GoalId, MemoryId, ToolCtx, ToolError};

use super::handles::MemoryHandleClass;

#[allow(clippy::unused_self)]
impl ToolCtx {
    /// Parse a bare memory UUID. Kind and visibility are checked by the engine.
    ///
    /// # Errors
    ///
    /// Returns invalid input for any other spelling.
    pub fn resolve_memory(&self, raw: &str) -> Result<MemoryId, ToolError> {
        super::ids::parse_wire_uuid(raw).map(MemoryId::new)
    }

    /// Parse a bare Goal UUID; the Goal operation resolves its stored row.
    ///
    /// # Errors
    ///
    /// Returns invalid input for any other spelling.
    pub fn resolve_goal(&self, raw: &str) -> Result<GoalId, ToolError> {
        super::ids::parse_wire_uuid(raw).map(GoalId::new)
    }

    /// Resolve the layer of a readable stored memory, without trusting the caller.
    ///
    /// # Errors
    ///
    /// Returns invalid input, not found, or an engine read failure.
    pub async fn resolve_memory_with_kind(
        &self,
        raw: &str,
    ) -> Result<(MemoryId, MemoryHandleClass), ToolError> {
        let memory_id = self.resolve_memory(raw)?;
        self.resolve_memory_id_with_kind(memory_id).await
    }

    pub(crate) async fn resolve_memory_id_with_kind(
        &self,
        memory_id: MemoryId,
    ) -> Result<(MemoryId, MemoryHandleClass), ToolError> {
        self.resolve_memory_ids_with_kind(&[memory_id])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                ToolError::NotFound(format!("memory {} not found", memory_id.into_inner()))
            })
    }

    /// [`Self::resolve_memory_id_with_kind`] for each of `ids`, in order, with
    /// one authorization lookup for all of them; the first unreadable id is
    /// the error.
    pub(crate) async fn resolve_memory_ids_with_kind(
        &self,
        ids: &[MemoryId],
    ) -> Result<Vec<(MemoryId, MemoryHandleClass)>, ToolError> {
        self.engine()
            .memory_kinds(self.authz(), ids)
            .await?
            .into_iter()
            .map(|(id, kind)| {
                MemoryHandleClass::from_entity_kind(kind)
                    .map(|class| (id, class))
                    .ok_or_else(|| ToolError::InvalidInput("expected a memory".into()))
            })
            .collect()
    }
}
