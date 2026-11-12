//! `proxiima-example_list_notebooks`: the spiice's one tool, read-only and
//! owner-scoped. A test and tutorial fixture, not a product.

use std::future::Future;
use std::pin::Pin;

use proxiima_spiice::{Tool, ToolCtx, ToolEffect, ToolError, owner_transaction};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::contract::LIST_NOTEBOOKS_TOOL;

/// Static SQL: the registry rows of one owner, read on an owner-scoped
/// transaction so owner RLS applies as well as the filter.
const LIST_NOTEBOOKS_SQL: &str = "SELECT notebook_id, name FROM proxiima_example.notebooks \
     WHERE owner_id = $1 ORDER BY name, notebook_id";

/// The caller's notebooks, `(notebook_id, name)`, ordered by name, read on
/// the caller's owner-scoped transaction.
async fn list_notebooks(ctx: &ToolCtx) -> Result<Vec<(Uuid, String)>, ToolError> {
    let mut tx = owner_transaction(ctx).await?;
    let rows = tx
        .fetch_all(
            sqlx::query_as::<_, (Uuid, String)>(LIST_NOTEBOOKS_SQL).bind(ctx.owner().into_inner()),
        )
        .await?;
    tx.commit().await?;
    Ok(rows)
}

/// No arguments: the tool lists the calling owner's notebooks.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListNotebooksArgs {}

/// One notebook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct NotebookItem {
    /// The notebook id (the scope id its notes carry).
    pub notebook_id: Uuid,
    /// The notebook's name.
    pub name: String,
}

/// The caller's notebooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ListNotebooksOutput {
    /// Ordered by name.
    pub notebooks: Vec<NotebookItem>,
}

/// `proxiima-example_list_notebooks`.
#[derive(Debug)]
pub struct ListNotebooksTool;

impl Tool for ListNotebooksTool {
    const NAME: &'static str = LIST_NOTEBOOKS_TOOL;
    const TITLE: &'static str = "List notebooks";
    const DESCRIPTION: &'static str =
        "List the current owner's notebooks (example spiice; a test fixture).";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::ReadOnly);

    type Args = ListNotebooksArgs;
    type Output = ListNotebooksOutput;

    fn call(
        ctx: ToolCtx,
        _args: ListNotebooksArgs,
    ) -> Pin<Box<dyn Future<Output = Result<ListNotebooksOutput, ToolError>> + Send>> {
        Box::pin(async move {
            let notebooks = list_notebooks(&ctx)
                .await?
                .into_iter()
                .map(|(notebook_id, name)| NotebookItem { notebook_id, name })
                .collect();
            Ok(ListNotebooksOutput { notebooks })
        })
    }
}
