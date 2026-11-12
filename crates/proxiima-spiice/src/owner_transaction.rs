//! A spiice's own tables, read and written under the caller's owner scope.

use proxiima_core::{ToolCtx, ToolError};
use proxiima_storage_pg::{OwnerTransaction, OwnerTransactions};

/// Begin a transaction on the spiice's own tables for the owner scope of the
/// tool call `ctx` serves.
///
/// Every statement of the operation runs on the returned
/// [`OwnerTransaction`]; it applies owner RLS as well as the statement's own
/// filter, refuses a statement that names the core schema, and rolls back
/// unless committed. Fact and state writes go through the Engine
/// ([`UnitOfWork`](crate::UnitOfWork)), not through it.
///
/// # Errors
///
/// [`ToolError::Unavailable`] when the host serving `ctx` publishes no
/// owner-transaction service, [`ToolError::Other`] when the call carries no
/// authenticated owner scope, and [`ToolError::Storage`] when the
/// transaction cannot begin.
pub async fn owner_transaction(ctx: &ToolCtx) -> Result<OwnerTransaction, ToolError> {
    let transactions = ctx.service::<OwnerTransactions>().ok_or_else(|| {
        ToolError::Unavailable(
            "owner-scoped transactions are not configured: the host serving this tool \
             publishes no OwnerTransactions service"
                .into(),
        )
    })?;
    let scope = ctx
        .authz()
        .owner_scope()
        .ok_or_else(|| ToolError::Other("authenticated owner scope required".into()))?;
    Ok(transactions.begin(scope).await?)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proxiima_core::test_fixtures::{owner_fixture, tool_ctx};
    use proxiima_core::{AuthPath, AuthzContext, SpiiceRegistry, ToolError};

    #[tokio::test]
    async fn a_context_without_the_service_answers_unavailable() {
        let owner = owner_fixture();
        let ctx = tool_ctx(
            owner,
            AuthzContext::single_owner(&owner, AuthPath::System),
            Arc::new(
                SpiiceRegistry::new()
                    .try_freeze()
                    .expect("an empty registry freezes"),
            ),
        );
        let refused = super::owner_transaction(&ctx)
            .await
            .expect_err("no service, no transaction");
        assert!(
            matches!(&refused, ToolError::Unavailable(message) if message.contains("not configured")),
            "{refused:?}"
        );
    }
}
