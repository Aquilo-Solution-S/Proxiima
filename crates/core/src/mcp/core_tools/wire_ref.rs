//! Graph endpoints carry bare UUIDs; redaction retains its explicit sentinels.

use crate::ToolCtx;
use crate::{EdgeEndpoint, EdgeTargetProjection, EntityRef};

pub(crate) const REDACTED_TARGET: &str = "redacted target";
pub(crate) const FORGOTTEN_TARGET: &str = "forgotten target";
pub(crate) const ERASED_TARGET: &str = "erased target";

pub(crate) fn format_endpoint(_ctx: &ToolCtx, endpoint: EdgeEndpoint) -> String {
    match endpoint.entity {
        EntityRef::Memory(id) => id.into_inner().to_string(),
        EntityRef::Goal(id) => id.into_inner().to_string(),
    }
}

pub(crate) fn format_target_projection(ctx: &ToolCtx, target: EdgeTargetProjection) -> String {
    match target {
        EdgeTargetProjection::Visible { target } => format_endpoint(ctx, target),
        EdgeTargetProjection::Redacted => REDACTED_TARGET.into(),
        EdgeTargetProjection::Forgotten => FORGOTTEN_TARGET.into(),
        EdgeTargetProjection::Erased => ERASED_TARGET.into(),
    }
}

/// Another store's edge target, in the words a local target is given.
pub(crate) fn format_remote_target(
    target: crate::storage_ports::other_stores::RemoteEdgeTarget,
) -> String {
    use crate::storage_ports::other_stores::RemoteEdgeTarget;
    match target {
        RemoteEdgeTarget::Visible(id) => id.to_string(),
        RemoteEdgeTarget::Redacted => REDACTED_TARGET.into(),
        RemoteEdgeTarget::Forgotten => FORGOTTEN_TARGET.into(),
        RemoteEdgeTarget::Erased => ERASED_TARGET.into(),
    }
}
