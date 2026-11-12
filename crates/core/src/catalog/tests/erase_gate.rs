use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;

use crate::catalog::{ActionArgSpec, OwnerNeed, Replay, ToolAudience, ToolEffect};
use crate::engine::unit_of_work::tests::BeginSpyFactory;
use crate::spiice::contract::{ProjectionDecl, SpiiceContract, ToolContract};
use crate::storage_ports::StoragePorts;
use crate::verbs::own_erase::{EraseMode, SeriesEraseError, SeriesSelection};
use crate::{AuthPath, AuthzContext, Engine, OwnerId, Tool, ToolCtx, ToolError, UserId};

const GATE_SPIICE: &str = "gate_fixture";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GateArgs {
    /// Ask with nothing selected.
    #[serde(default)]
    empty: bool,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct GateOutput {}

/// A non-empty selection: an empty one is answered before storage.
fn some_series() -> SeriesSelection {
    SeriesSelection::Ids(vec![crate::MemoryId::new(uuid::Uuid::now_v7())])
}

/// Both tools call the verb the same way; only their declarations
/// differ.
fn erase_from(ctx: ToolCtx, args: &GateArgs) -> BoxFuture<'static, Result<GateOutput, ToolError>> {
    let selection = if args.empty {
        SeriesSelection::Ids(Vec::new())
    } else {
        some_series()
    };
    Box::pin(async move {
        let engine = ctx
            .engine()
            .ok_or_else(|| ToolError::Other("the gate fixture needs an engine".into()))?;
        let mut unit = engine
            .unit_of_work(ctx.authz())
            .await
            .map_err(ToolError::Protocol)?;
        unit.erase_own_series(GATE_SPIICE, ctx.owner(), selection, EraseMode::DryRun)
            .await
            .map_err(|err: SeriesEraseError| ToolError::Protocol(err.into()))?;
        Ok(GateOutput {})
    })
}

struct WipeTool;

impl Tool for WipeTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = "gate_fixture_wipe";
    const DESCRIPTION: &'static str = "A tool that declares it destroys data.";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::Destructive(Replay::NonIdempotent));
    type Args = GateArgs;
    type Output = GateOutput;

    fn call(ctx: ToolCtx, args: GateArgs) -> BoxFuture<'static, Result<GateOutput, ToolError>> {
        erase_from(ctx, &args)
    }
}

struct TouchTool;

impl Tool for TouchTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = "gate_fixture_touch";
    const DESCRIPTION: &'static str = "A tool that declares it destroys nothing.";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::Additive(Replay::NonIdempotent));
    type Args = GateArgs;
    type Output = GateOutput;

    fn call(ctx: ToolCtx, args: GateArgs) -> BoxFuture<'static, Result<GateOutput, ToolError>> {
        erase_from(ctx, &args)
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum MixedArgs {
    /// The destructive action.
    Wipe {},
    /// The additive action.
    Touch {},
}

/// A dispatcher with one destructive action: its tool-level effect
/// is destructive, and only that action may erase.
struct MixedTool;

impl Tool for MixedTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = "gate_fixture_mixed";
    const DESCRIPTION: &'static str = "A dispatcher with one destructive action.";
    const ACTION_ARG_SPECS: &'static [ActionArgSpec] = &[
        ActionArgSpec {
            action: "wipe",
            allowed_fields: &[],
            required_fields: &[],
            effect: ToolEffect::Destructive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
        ActionArgSpec {
            action: "touch",
            allowed_fields: &[],
            required_fields: &[],
            effect: ToolEffect::Additive(Replay::NonIdempotent),
            audience: ToolAudience::Shared,
            need: OwnerNeed::Memory,
            title: "Test action",
        },
    ];
    type Args = MixedArgs;
    type Output = GateOutput;

    fn call(ctx: ToolCtx, _: MixedArgs) -> BoxFuture<'static, Result<GateOutput, ToolError>> {
        erase_from(ctx, &GateArgs { empty: false })
    }
}

/// Destructive, and registered under the spiice, but not named by
/// its contract.
struct StrayTool;

impl Tool for StrayTool {
    const TITLE: &'static str = "Test tool";
    const NAME: &'static str = "gate_fixture_stray";
    const DESCRIPTION: &'static str = "A destructive tool the contract does not name.";
    const EFFECT: Option<ToolEffect> = Some(ToolEffect::Destructive(Replay::Idempotent));
    type Args = GateArgs;
    type Output = GateOutput;

    fn call(ctx: ToolCtx, args: GateArgs) -> BoxFuture<'static, Result<GateOutput, ToolError>> {
        erase_from(ctx, &args)
    }
}

static GATE_CONTRACT: SpiiceContract = SpiiceContract {
    retired_facts: &[],
    spiice_id: GATE_SPIICE,
    ordinal: 96,
    schemas: &[],
    state_surfaces: &[],
    scopes: &[],
    kernel_surfaces: &[],
    tools: &[
        ToolContract {
            wire_name: "gate_fixture_wipe",
            actions: &[],
        },
        ToolContract {
            wire_name: "gate_fixture_touch",
            actions: &[],
        },
        ToolContract {
            wire_name: "gate_fixture_mixed",
            actions: &["wipe", "touch"],
        },
    ],
    resources: &[],
    projection: ProjectionDecl::None {
        why: "the erase-gate fixture has no search surface",
    },
    bespoke_erase_legs: &[],
    bespoke_transfer_legs: &[],
    evolutions: &[],
};

fn engine(begin_calls: Arc<AtomicUsize>) -> Arc<Engine> {
    let factory = Arc::new(BeginSpyFactory { begin_calls });
    Arc::new(
        Engine::try_compose(
            StoragePorts::rejecting_with_write_session(factory),
            |registry| {
                registry.try_add_contract(&GATE_CONTRACT)?;
                registry.try_add_tool::<WipeTool>(GATE_SPIICE)?;
                registry.try_add_tool::<TouchTool>(GATE_SPIICE)?;
                registry.try_add_tool::<MixedTool>(GATE_SPIICE)?;
                registry.try_add_tool::<StrayTool>(GATE_SPIICE)?;
                Ok(())
            },
        )
        .expect("the erase-gate registry freezes"),
    )
}

async fn call(engine: &Arc<Engine>, tool: &str) -> Result<serde_json::Value, String> {
    call_with(engine, tool, serde_json::json!({})).await
}

async fn call_with(
    engine: &Arc<Engine>,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let descriptor = engine
        .registry()
        .mcp_tool(tool)
        .expect("the fixture tool is registered");
    let ctx = crate::test_fixtures::tool_ctx_with(
        owner,
        AuthzContext::single_owner(&owner, AuthPath::HostBearer),
        Arc::new(engine.registry().clone()),
        crate::SpiiceServices::default(),
        crate::ToolCaller::new("gate/model", "gate", "1"),
    )
    .with_engine(Some(Arc::clone(engine)));
    (descriptor.call)(ctx, args)
        .await
        .map_err(|err| err.to_string())
}

/// The declaration is the gate: the tool that declares itself
/// destructive reaches storage, the one that does not is refused
/// before a session is begun, and the handlers are identical.
#[tokio::test]
async fn only_a_tool_declared_destructive_reaches_the_erase() {
    let begins = Arc::new(AtomicUsize::new(0));
    let engine = engine(begins.clone());

    let refused = call(&engine, "gate_fixture_touch")
        .await
        .expect_err("a tool not declared destructive cannot erase");
    assert!(
        refused.contains("not a destructive tool"),
        "the refusal names the missing declaration: {refused}"
    );
    assert_eq!(begins.load(Ordering::SeqCst), 0, "refused before storage");

    let reached = call(&engine, "gate_fixture_wipe")
        .await
        .expect_err("the spy backend fails the begin");
    assert!(
        !reached.contains("not a destructive tool"),
        "a destructive tool passes the gate: {reached}"
    );
    assert_eq!(
        begins.load(Ordering::SeqCst),
        1,
        "the erase reached storage"
    );
}

/// The gate reads the dispatched action's own effect, not the
/// dispatcher's join: one destructive action lends nothing to its
/// sibling, though the tool as a whole is destructive.
#[tokio::test]
async fn a_dispatcher_erases_only_from_its_destructive_action() {
    let begins = Arc::new(AtomicUsize::new(0));
    let engine = engine(begins.clone());
    let mixed = engine
        .registry()
        .mcp_tool("gate_fixture_mixed")
        .expect("the dispatcher is registered");
    assert_eq!(
        mixed.effect(),
        Some(ToolEffect::Destructive(Replay::NonIdempotent)),
        "the tool-level effect is the strongest action's"
    );

    let refused = call_with(
        &engine,
        "gate_fixture_mixed",
        serde_json::json!({ "action": "touch" }),
    )
    .await
    .expect_err("the additive action cannot erase");
    assert!(refused.contains("not a destructive tool"), "{refused}");
    assert_eq!(begins.load(Ordering::SeqCst), 0, "refused before storage");

    let reached = call_with(
        &engine,
        "gate_fixture_mixed",
        serde_json::json!({ "action": "wipe" }),
    )
    .await
    .expect_err("the spy backend fails the begin");
    assert!(!reached.contains("not a destructive tool"), "{reached}");
    assert_eq!(
        begins.load(Ordering::SeqCst),
        1,
        "the erase reached storage"
    );
}

/// A destructive tool of the spiice's registration that its contract
/// does not name is not the spiice's tool for this purpose.
#[tokio::test]
async fn a_destructive_tool_its_contract_does_not_name_cannot_erase() {
    let begins = Arc::new(AtomicUsize::new(0));
    let engine = engine(begins.clone());
    let refused = call(&engine, "gate_fixture_stray")
        .await
        .expect_err("an unnamed tool cannot erase");
    assert!(refused.contains("not a destructive tool"), "{refused}");
    assert_eq!(begins.load(Ordering::SeqCst), 0);
}

/// Outside a tool nothing is stamped, so the gate does not apply:
/// the spiice's own code and the host reach the same verb.
#[tokio::test]
async fn a_unit_outside_any_tool_is_not_gated_by_a_tool_declaration() {
    let begins = Arc::new(AtomicUsize::new(0));
    let engine = engine(begins.clone());
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    assert_eq!(authz.invoking_tool(), None);
    let mut unit = engine.unit_of_work(&authz).await.expect("a unit opens");
    let _ = unit
        .erase_own_series(GATE_SPIICE, owner, some_series(), EraseMode::DryRun)
        .await;
    assert_eq!(begins.load(Ordering::SeqCst), 1);
}

/// A sidecar selection names its schema, and a schema the spiice does
/// not declare is refused by name before a session is begun.
#[tokio::test]
async fn a_sidecar_selection_of_a_foreign_schema_is_refused_before_storage() {
    let begins = Arc::new(AtomicUsize::new(0));
    let engine = engine(begins.clone());
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    let schema = || crate::SchemaId::new("another_spiice/thing-v1".to_owned());
    for selection in [
        SeriesSelection::SidecarEquals {
            schema: schema(),
            predicates: vec![(
                "kind".to_owned(),
                crate::verbs::query::SidecarAtom::Bool(true),
            )],
        },
        SeriesSelection::DeclaredBefore {
            schema: schema(),
            column: "happened_at".to_owned(),
            cutoff: time::OffsetDateTime::now_utc(),
        },
    ] {
        let mut unit = engine.unit_of_work(&authz).await.expect("a unit opens");
        match unit
            .erase_own_series(GATE_SPIICE, owner, selection, EraseMode::DryRun)
            .await
        {
            Err(SeriesEraseError::Refused(refusal)) => {
                assert_eq!(
                    refusal.kind,
                    crate::verbs::own_erase::SeriesEraseRefusalKind::ForeignSchema
                );
                assert_eq!(refusal.offending, vec!["schema=another_spiice/thing-v1"]);
            }
            other => panic!("a foreign schema must be refused: {other:?}"),
        }
    }
    assert_eq!(begins.load(Ordering::SeqCst), 0, "refused before storage");
}

/// An empty selection is the cheap way to ask "may I erase here?": it
/// passes every gate a real erase passes and opens no session. A
/// caller paging a scope relies on the first half, the lock table on
/// the second.
#[tokio::test]
async fn an_empty_selection_is_authorized_without_a_session() {
    let begins = Arc::new(AtomicUsize::new(0));
    let engine = engine(begins.clone());
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let authz = AuthzContext::single_owner(&owner, AuthPath::HostBearer);
    let mut unit = engine.unit_of_work(&authz).await.expect("a unit opens");
    let receipt = unit
        .erase_own_series(
            GATE_SPIICE,
            owner,
            SeriesSelection::Ids(Vec::new()),
            EraseMode::Erase,
        )
        .await
        .expect("Admin may ask with nothing selected");
    assert_eq!(receipt.series_erased, 0);
    assert!(receipt.versions.is_empty());
    assert_eq!(begins.load(Ordering::SeqCst), 0, "no session was begun");
    unit.commit()
        .await
        .expect("an erase unit with no session commits");

    let group = OwnerId::from(crate::GroupId::new(uuid::Uuid::now_v7()));
    let ingest = AuthzContext::for_subject_with_role(
        UserId::new(uuid::Uuid::now_v7()),
        [(group, crate::Role::ingest())],
        AuthPath::HostBearer,
    );
    let mut unit = engine.unit_of_work(&ingest).await.expect("a unit opens");
    let err = unit
        .erase_own_series(
            GATE_SPIICE,
            group,
            SeriesSelection::Ids(Vec::new()),
            EraseMode::Erase,
        )
        .await
        .expect_err("an empty selection is still gated on Admin");
    assert!(
        matches!(err, SeriesEraseError::Protocol(ref err) if err.code == crate::ErrorCode::Forbidden),
        "below Admin is forbidden, empty selection or not: {err:?}"
    );

    let refused = call_with(
        &engine,
        "gate_fixture_touch",
        serde_json::json!({ "empty": true }),
    )
    .await
    .expect_err("and on the tool's declaration");
    assert!(refused.contains("not a destructive tool"), "{refused}");
    assert_eq!(begins.load(Ordering::SeqCst), 0);
}
