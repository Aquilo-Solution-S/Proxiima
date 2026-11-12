//! `core/list_schemas` — project `SpiiceRegistryFrozen` schemas.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::verbs::schema::{PayloadKind, SchemaInfo, SpiiceRegistryFrozen};
use crate::{SchemaId, SchemaVersion};
use crate::{ToolCtx, ToolError};

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListSchemasArgs {
    /// Optional filter. One of "Fact", "Abstraction", "Perspective",
    /// "Goal", "`CitedObject`" (case-insensitive). Omit to return all kinds.
    #[serde(default)]
    pub kind: Option<PayloadKind>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SchemaItem {
    pub schema_id: String,
    pub schema_version: u32,
    pub kind: PayloadKind,
    /// Whether an admitted Fact of this schema also captures a
    /// `CloudEvents` publication record in the same transaction (issue
    /// #305). Always `false` for the non-Fact kinds.
    pub listenable: bool,
    /// Consumer-facing meaning of this target after an explicit evolution.
    pub change: Option<String>,
    /// Required action for consumers of this target schema.
    pub consumer_action: Option<String>,
}

pub(crate) fn schema_item(registry: &SpiiceRegistryFrozen, info: &SchemaInfo) -> SchemaItem {
    let metadata = registry.evolution_metadata(&info.schema_id, info.schema_version, info.kind);
    SchemaItem {
        schema_id: info.schema_id.as_str().to_string(),
        schema_version: info.schema_version.into_inner(),
        kind: info.kind,
        listenable: info.listenable,
        change: metadata.map(|(change, _)| change.to_owned()),
        consumer_action: metadata.map(|(_, action)| action.to_owned()),
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListSchemasOutput {
    /// Registered payload schemas, filtered by `kind`.
    pub schemas: Vec<SchemaItem>,
    /// The answer schema of every `proxiima://` resource, derived from the
    /// resource's Rust answer type. Not filtered by `kind`.
    pub resources: Vec<ResourceAnswerItem>,
}

/// One resource's answer schema.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ResourceAnswerItem {
    /// The resource, as `resources/templates/list` names it.
    pub uri_template: String,
    /// JSON Schema of the resource's answer, as a tool's `outputSchema` is.
    pub schema: serde_json::Value,
}

#[allow(clippy::unused_async)]
/// # Errors
///
/// Returns invalid kind filters.
pub async fn list_schemas(
    ctx: ToolCtx,
    args: ListSchemasArgs,
) -> Result<ListSchemasOutput, ToolError> {
    let schemas = ctx
        .registry()
        .list()
        .into_iter()
        .filter(|info| args.kind.is_none_or(|k| info.kind == k))
        .map(|info| schema_item(ctx.registry(), &info))
        .collect();
    let resources = crate::all_core_resources()
        .map(|resource| ResourceAnswerItem {
            uri_template: resource.uri_template.to_string(),
            schema: resource.answer.schema(),
        })
        .collect();
    Ok(ListSchemasOutput { schemas, resources })
}

/// One registered payload type's JSON Schema, addressed by the same
/// `{schema_id}/{schema_version}` pair a published event carries in its
/// `CloudEvents` `dataschema`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct GetSchemaOutput {
    pub schema_id: String,
    pub schema_version: u32,
    pub kind: PayloadKind,
    pub listenable: bool,
    pub change: Option<String>,
    pub consumer_action: Option<String>,
    /// The registered `json_schema()` of the payload type.
    pub json_schema: serde_json::Value,
}

/// Read one registered schema, backing `proxiima://schema/{id}/{version}`.
///
/// A registered schema with no `json_schema()` — the opaque
/// cited-object lane — is reported as missing rather
/// than as an empty document: there is nothing to validate against, and a
/// consumer resolving a `dataschema` must not mistake silence for a
/// permissive schema. A listenable schema can never land here, because
/// freeze refuses `LISTENABLE = true` without a schema.
///
/// # Errors
///
/// Returns [`ToolError::NotFound`] when the schema is not registered at
/// that version or carries no JSON Schema.
#[allow(clippy::unused_async)]
pub async fn get_schema(
    ctx: ToolCtx,
    schema_id: &str,
    schema_version: u32,
) -> Result<GetSchemaOutput, ToolError> {
    let id = SchemaId::new(schema_id.to_string());
    let version = SchemaVersion::new(schema_version);
    let missing = || ToolError::NotFound(format!("schema {schema_id}/{schema_version} not found"));
    let info = ctx.registry().lookup(&id, version).ok_or_else(missing)?;
    let json_schema = ctx
        .registry()
        .payload_json_schema(&id, version, info.kind)
        .ok_or_else(missing)?
        .clone();
    let metadata =
        ctx.registry()
            .evolution_metadata(&info.schema_id, info.schema_version, info.kind);
    Ok(GetSchemaOutput {
        schema_id: info.schema_id.as_str().to_string(),
        schema_version: info.schema_version.into_inner(),
        kind: info.kind,
        listenable: info.listenable,
        change: metadata.map(|(change, _)| change.to_owned()),
        consumer_action: metadata.map(|(_, action)| action.to_owned()),
        json_schema,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::get_schema;
    use crate::{ToolCtx, ToolError};

    use crate::{AuthPath, AuthzContext, OwnerId, UserId};

    fn ctx_with_probes() -> ToolCtx {
        let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
        crate::test_fixtures::tool_ctx(
            owner,
            AuthzContext::single_owner(&owner, AuthPath::HostBearer),
            Arc::new(crate::test_fixtures::probe_registry()),
        )
    }

    #[tokio::test]
    async fn an_unregistered_schema_version_is_not_found() {
        let err = get_schema(ctx_with_probes(), "probe/listenable-v1", 7)
            .await
            .expect_err("version 7 is not registered");
        assert!(matches!(err, ToolError::NotFound(_)), "{err}");
    }

    /// #676: the catalog lists every resource's answer schema, derived from
    /// its contract, and a lineage node's `space` is required and nullable.
    #[tokio::test]
    async fn the_catalog_lists_each_resource_answer_schema() {
        let listed = super::list_schemas(ctx_with_probes(), super::ListSchemasArgs::default())
            .await
            .expect("list schemas");
        let listed = serde_json::to_value(listed).expect("serializes");
        let resources = listed["resources"].as_array().expect("resources section");
        for resource in crate::all_core_resources() {
            let entry = resources
                .iter()
                .find(|entry| entry["uri_template"] == resource.uri_template)
                .unwrap_or_else(|| panic!("{} is listed: {listed}", resource.uri_template));
            assert_eq!(entry["schema"], resource.answer.schema());
        }
        assert_eq!(resources.len(), crate::all_core_resources().count());
        let lineage = resources
            .iter()
            .find(|entry| {
                entry["uri_template"]
                    .as_str()
                    .is_some_and(|template| template.contains("/lineage"))
            })
            .expect("the lineage answer is listed");
        let node = &lineage["schema"]["properties"]["nodes"]["items"];
        assert_eq!(
            node["properties"]["space"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert!(
            node["required"]
                .as_array()
                .is_some_and(|required| required.contains(&serde_json::json!("space"))),
            "{node}"
        );
    }

    #[test]
    fn unknown_kind_still_fails_closed() {
        // Case-insensitivity must not soften the typo guard.
        assert!(
            "Facts"
                .parse::<crate::verbs::schema::PayloadKind>()
                .is_err()
        );
        assert!("".parse::<crate::verbs::schema::PayloadKind>().is_err());
        assert_eq!(
            "citedobject"
                .parse::<crate::verbs::schema::PayloadKind>()
                .ok(),
            Some(crate::verbs::schema::PayloadKind::CitedObject)
        );
    }
}
