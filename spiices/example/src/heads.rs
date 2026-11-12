//! Current note series through the generic, owner-scoped facade.

use proxiima_spiice::{
    MemoryId, SidecarAtom, SidecarHeadsPage, SidecarHeadsScope, ToolCtx, ToolError,
    sidecar_heads_by_key,
};

use crate::NoteV1;

/// List current note heads the caller of `ctx` reads, optionally narrowed by
/// a natural-key tuple prefix.
///
/// An empty prefix lists every visible note; one UUID selects a notebook;
/// that UUID followed by a text key selects one note series. Resume from the
/// returned cursor for the next page, newest admission first.
///
/// # Errors
/// Returns facade authorization, prefix, limit, and storage errors.
pub async fn note_heads(
    ctx: &ToolCtx,
    scope: SidecarHeadsScope,
    key_prefix: &[SidecarAtom],
    after: Option<MemoryId>,
    limit: u32,
) -> Result<SidecarHeadsPage, ToolError> {
    sidecar_heads_by_key::<NoteV1>(ctx, scope, key_prefix, after, limit).await
}
