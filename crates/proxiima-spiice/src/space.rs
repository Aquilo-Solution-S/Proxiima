//! The owner a tool's optional `space` argument names.

use proxiima_core::catalog::core_tools::memory_spaces::{
    ResolvedMemorySpace, SpaceDefault, resolve_space_owner,
};
use proxiima_core::{ToolCtx, ToolError};

/// Resolve a read tool's optional `space` argument to the owner the scope gate
/// judged the call on.
///
/// The gate resolves the same key with the same resolver and the calling
/// owner as the default, so the tool reads the owner its caller's role was
/// checked against. A spiice has no other default to choose: one that fell
/// back to the caller's own identity would read an owner the gate never
/// judged, so this is the only read resolver the SDK offers.
///
/// ```compile_fail
/// use proxiima_spiice::SpaceDefault;
/// ```
///
/// # Errors
///
/// Returns `InvalidInput` when the key names no space the caller can access,
/// or when the argument is omitted and the calling owner is not accessible.
pub fn resolve_space_for_read(
    ctx: &ToolCtx,
    raw: Option<&str>,
) -> Result<ResolvedMemorySpace, ToolError> {
    resolve_space_owner(ctx, raw, SpaceDefault::Current)
}
