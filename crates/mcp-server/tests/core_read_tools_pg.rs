mod common;

use std::sync::Arc;

use common::{create_core_db, db_url, drop_db, storage};
use proxiima_core::SpiiceServices;
use proxiima_core::ToolCaller;
use proxiima_core::test_fixtures::authenticated_context;
use proxiima_core::{AuthPath, AuthzContext, Engine, MemoryId, OwnerId, SpiiceRegistry, UserId};
use proxiima_mcp_server::{McpAuthContext, McpToolHost};
use proxiima_storage_pg::PgStorage;
use proxiima_storage_pg::test_fixtures::{TestGoal, TestMemory};

fn owner_authz(owner: OwnerId) -> AuthzContext {
    authenticated_context(
        AuthzContext::single_owner(&owner, AuthPath::HostBearer)
            .narrowed_to_owner(owner)
            .expect("personal owner narrows"),
    )
}

#[derive(Debug)]
struct HidesRemember;

#[async_trait::async_trait]
impl proxiima_core::RequestBehavior for HidesRemember {
    async fn handle(
        &self,
        call: proxiima_core::catalog::ToolCall,
        next: proxiima_core::catalog::Next<'_>,
    ) -> Result<proxiima_core::ToolReply, proxiima_core::ToolError> {
        next.run(call).await
    }

    fn visible(
        &self,
        _ctx: &proxiima_core::ToolCtx,
        tool: &proxiima_core::ToolDescriptorView<'_>,
    ) -> bool {
        tool.name() != proxiima_core::protocol::tool::CORE_REMEMBER
    }
}

#[tokio::test]
async fn graph_and_tool_catalog_share_behavior_visibility() -> Result<(), Box<dyn std::error::Error>>
{
    use proxiima_core::protocol::tool::CORE_REMEMBER;

    let db_name = create_core_db().await?;
    let pg = storage(&db_url(&db_name)).await?;
    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    TestMemory::fact().create(&pg, owner).await?;
    for hidden in [false, true] {
        let mut registry = SpiiceRegistry::new();
        if hidden {
            registry.add_request_behavior(HidesRemember);
        }
        let engine = Arc::new(Engine::new(
            registry.freeze_or_panic_for_tests(),
            Arc::new(pg.clone()).storage_ports(),
        ));
        let server = McpToolHost::from_engine(engine, SpiiceServices::default());
        let auth = McpAuthContext {
            owner,
            authz: owner_authz(owner),
        };
        let tools = server
            .read_resource("proxiima://tools", author_ctx(), Some(auth.clone()))
            .await?;
        let graph = server
            .read_resource("proxiima://graph", author_ctx(), Some(auth))
            .await?;
        assert_eq!(graph["substrate_tools"], tools["tools"]);
        assert_eq!(
            graph["substrate_tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["tool_id"] == CORE_REMEMBER),
            !hidden,
        );
    }
    drop(pg);
    drop_db(&db_name).await?;
    Ok(())
}

#[tokio::test]
async fn core_read_resources_return_bare_uuids_and_author() -> Result<(), Box<dyn std::error::Error>>
{
    let db_name = create_core_db().await?;
    let database_url = db_url(&db_name);
    let pg = storage(&database_url).await?;

    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
    let engine = Arc::new(Engine::new(
        registry.clone(),
        Arc::new(pg.clone()).storage_ports(),
    ));
    let server = McpToolHost::from_engine(engine, SpiiceServices::default());
    // The host is now the authoritative scope chokepoint, so reads need an
    // authenticated full-scope context (production always passes Some(auth);
    // a None context is unauthenticated and correctly denied).
    let auth = McpAuthContext {
        owner,
        authz: owner_authz(owner),
    };
    let source = insert_memory(&server, &pg, &auth, "source lineage memory", &[]).await?;
    let derived = insert_memory(&server, &pg, &auth, "derived lineage memory", &[source]).await?;

    let fetched = server
        .read_resource(
            &format!("proxiima://memory/{derived}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(fetched["memory"], format!("{derived}"));
    assert_eq!(fetched["kind"], "Abstraction");
    assert_eq!(fetched["handle"], format!("{derived}"));
    assert_eq!(fetched["body"], "derived lineage memory");
    assert!(
        fetched.get("neighbor_edges").is_none(),
        "neighbor_edges should be omitted unless expand_neighbors is true"
    );

    let prefixed_uuid_err = server
        .read_resource(
            &format!("proxiima://memory/A:{derived}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect_err("prefixed UUID must be rejected");
    assert!(
        prefixed_uuid_err.to_string().contains("hyphenated UUID"),
        "a prefixed resource must report the bare UUID grammar"
    );

    let expanded = server
        .read_resource(
            &format!("proxiima://memory/{derived}?expand_neighbors=true"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(expanded["memory"], format!("{derived}"));
    // Four fields is the whole edge: no handle, no relation, no payload.
    assert_eq!(expanded["neighbor_edges"][0]["kind"], "origin");
    assert_eq!(
        expanded["neighbor_edges"][0]["source"],
        format!("{derived}")
    );
    assert_eq!(expanded["neighbor_edges"][0]["target"], format!("{source}"));
    assert!(
        expanded["neighbor_edges"][0].get("edge").is_none(),
        "an edge has no id to hand back"
    );

    let lineage = server
        .read_resource(
            &format!("proxiima://memory/{derived}/lineage?direction=ancestors&depth=1"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(lineage["start"], format!("{derived}"));
    assert!(
        lineage["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .any(|node| node["memory"] == format!("{source}"))
    );
    assert_eq!(lineage["edges"][0]["kind"], "origin");
    assert_eq!(lineage["edges"][0]["source"], format!("{derived}"));
    assert_eq!(lineage["edges"][0]["target"], format!("{source}"));

    drop(server);
    drop(pg);
    drop_db(&db_name).await?;
    Ok(())
}

#[tokio::test]
async fn search_results_expose_structured_memory_id() -> Result<(), Box<dyn std::error::Error>> {
    let db_name = create_core_db().await?;
    let database_url = db_url(&db_name);
    let pg = storage(&database_url).await?;

    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
    let engine = Arc::new(Engine::new(
        registry.clone(),
        Arc::new(pg.clone()).storage_ports(),
    ));
    let server = McpToolHost::from_engine(engine, SpiiceServices::default());
    let auth = McpAuthContext {
        owner,
        authz: owner_authz(owner),
    };
    let memory_id = insert_memory(&server, &pg, &auth, "structured search identity", &[]).await?;

    let output = server
        .call_tool(
            "core_search_memories",
            serde_json::json!({
                "query": "structured search identity",
                "mode": "lexical",
                "limit": 1,
            }),
            author_ctx(),
            Some(auth),
        )
        .await?;
    let result = output["memories"]
        .as_array()
        .and_then(|memories| memories.first())
        .expect("search returns the inserted memory");
    let structured_id = MemoryId::new(serde_json::from_value(result["memory_id"].clone())?);
    assert_eq!(structured_id.into_inner(), memory_id);
    assert_eq!(result["memory"], format!("{memory_id}"));

    drop(server);
    drop(pg);
    drop_db(&db_name).await?;
    Ok(())
}

async fn assert_goals_do_not_transfer(
    server: &McpToolHost,
    auth: &McpAuthContext,
    references: &[uuid::Uuid],
) {
    for goal_reference in references {
        let transfer = server
            .call_tool(
                "core_transfer",
                serde_json::json!({
                    "action": "transfer_to_owner",
                    "entity": goal_reference.to_string(),
                    "to_owner": auth.owner.to_string(),
                }),
                author_ctx(),
                Some(auth.clone()),
            )
            .await
            .expect_err("a stored Goal UUID retains the typed transfer refusal");
        assert!(
            transfer.to_string().contains("goals do not transfer"),
            "{transfer}"
        );
    }
}

#[tokio::test]
async fn goal_resources_list_and_paginate() -> Result<(), Box<dyn std::error::Error>> {
    let db_name = create_core_db().await?;
    let database_url = db_url(&db_name);
    let pg = storage(&database_url).await?;

    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let (goal_handles, goal_ids): (Vec<_>, Vec<_>) = [
        insert_active_goal(&pg, &owner).await?,
        insert_active_goal(&pg, &owner).await?,
        insert_active_goal(&pg, &owner).await?,
    ]
    .into_iter()
    .unzip();
    let foreign_owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let (foreign_handle, _) = insert_active_goal(&pg, &foreign_owner).await?;

    let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
    let engine = Arc::new(Engine::new(
        registry.clone(),
        Arc::new(pg.clone()).storage_ports(),
    ));
    let server = McpToolHost::from_engine(engine, SpiiceServices::default());
    let auth = McpAuthContext {
        owner,
        authz: owner_authz(owner),
    };

    assert_goals_do_not_transfer(&server, &auth, &[goal_ids[0], goal_handles[0]]).await;

    // Paginate Active goals two-at-a-time: disjoint, exhaustive, terminated.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let uri = match &cursor {
            Some(token) => format!("proxiima://goals?state=Active&limit=2&cursor={token}"),
            None => "proxiima://goals?state=Active&limit=2".to_string(),
        };
        let page = server
            .read_resource(&uri, author_ctx(), Some(auth.clone()))
            .await?;
        for goal in page["goals"].as_array().expect("goals array") {
            seen.push(goal["goal"].as_str().expect("goal ref").to_string());
        }
        if page["has_more"] == serde_json::json!(true) {
            cursor = Some(
                page["next_cursor"]
                    .as_str()
                    .expect("has_more implies next_cursor")
                    .to_string(),
            );
            assert!(seen.len() <= 3, "pagination must terminate");
        } else {
            assert_eq!(page["next_cursor"], serde_json::Value::Null);
            break;
        }
    }
    let mut deduped = seen.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), 3, "pages disjoint and exhaustive: {seen:?}");
    for goal_id in &goal_ids {
        assert!(seen.contains(&format!("{goal_id}")));
    }

    let paused_head = proxiima_core::new_memory_uuid(0);
    insert_goal_version(
        &pg,
        &owner,
        Some(goal_handles[1]),
        paused_head,
        "Paused",
        None,
    )
    .await?;
    let union = server
        .read_resource(
            "proxiima://goals?state=paused,ACTIVE,paused",
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(union["goals"].as_array().expect("goals array").len(), 3);

    let goal = server
        .read_resource(
            &format!("proxiima://goal/{}", goal_handles[1]),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(goal["goal"], format!("{paused_head}"));

    for handle in [foreign_handle, proxiima_core::new_memory_uuid(0)] {
        let error = server
            .read_resource(
                &format!("proxiima://goal/{handle}"),
                author_ctx(),
                Some(auth.clone()),
            )
            .await
            .expect_err("unreadable and nonexistent series are both absent");
        assert!(error.to_string().contains("not found"), "{error}");
    }

    assert_goal_reference_and_state_rejections(&server, &auth, goal_ids[0]).await;

    drop(server);
    drop(pg);
    drop_db(&db_name).await?;
    Ok(())
}

/// A Goal read returns the prompt text, when the series was set and when this
/// version changed, over the wire a client sees: `text` is a string when the
/// version knows it and JSON `null` (the key present) when it does not, and
/// both times are RFC 3339.
#[tokio::test]
async fn goal_resources_carry_text_set_at_and_changed_at() -> Result<(), Box<dyn std::error::Error>>
{
    let db_name = create_core_db().await?;
    let pg = storage(&db_url(&db_name)).await?;

    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    // Unknown text: a version no declaration can speak for.
    let (unknown_handle, unknown) = insert_active_goal(&pg, &owner).await?;
    // A series set at 22:13:20Z and paused a minute later; the head is the
    // second version, which keeps the first one's text.
    let set = goal_t_at(1_700_000_000_000);
    let paused = goal_t_at(1_700_000_060_000);
    let handle =
        insert_goal_version(&pg, &owner, None, set, "Active", Some("write the report")).await?;
    insert_goal_version(
        &pg,
        &owner,
        Some(handle),
        paused,
        "Paused",
        Some("write the report"),
    )
    .await?;

    let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
    let engine = Arc::new(Engine::new(
        registry.clone(),
        Arc::new(pg.clone()).storage_ports(),
    ));
    let server = McpToolHost::from_engine(engine, SpiiceServices::default());
    let auth = McpAuthContext {
        owner,
        authz: owner_authz(owner),
    };

    let listing = server
        .read_resource("proxiima://goals", author_ctx(), Some(auth.clone()))
        .await?;
    let listed = |goal: uuid::Uuid| {
        listing["goals"]
            .as_array()
            .expect("goals array")
            .iter()
            .find(|row| row["goal"] == format!("{goal}"))
            .unwrap_or_else(|| panic!("goal {goal} is listed: {listing}"))
            .clone()
    };

    // The listing returns the full text and both times.
    let series = listed(paused);
    assert_eq!(series["handle"], format!("{handle}"));
    assert_eq!(series["state"], "Paused");
    assert_eq!(series["text"], "write the report");
    assert_eq!(series["set_at"], "2023-11-14T22:13:20Z");
    assert_eq!(series["changed_at"], "2023-11-14T22:14:20Z");

    // Unknown is explicit: the key is there and it is JSON null.
    let unlisted = listed(unknown);
    assert_eq!(
        unlisted.get("text"),
        Some(&serde_json::Value::Null),
        "an unknown text serialises as null, not as an absent key: {unlisted}"
    );
    assert_eq!(unlisted["set_at"], unlisted["changed_at"]);
    let set_at = unlisted["set_at"].as_str().expect("set_at is a string");
    time::OffsetDateTime::parse(set_at, &time::format_description::well_known::Rfc3339)
        .expect("set_at is RFC 3339");

    // A handle reads the current head, and so does any version id a client
    // was given, here the superseded first version. The current head is the
    // paused version at this point.
    let single = server
        .read_resource(
            &format!("proxiima://goal/{handle}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(single, series, "the single read and the listing agree");
    assert_version_id_reads_series_head(&server, &auth, set, &series).await;
    let single_unknown = server
        .read_resource(
            &format!("proxiima://goal/{unknown_handle}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(single_unknown.get("text"), Some(&serde_json::Value::Null));

    // The stable handle reads the current head rather than a version id.
    let current: uuid::Uuid =
        sqlx::query_scalar("SELECT t FROM proxiima_core.goal_head WHERE handle = $1")
            .bind(handle)
            .fetch_one(pg.pool_for_tests())
            .await?;
    let current_read = server
        .read_resource(
            &format!("proxiima://goal/{handle}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(current_read["goal"], format!("{current}"));
    assert_eq!(current_read["handle"], format!("{handle}"));
    assert_eq!(current_read["state"], "Paused");

    drop(server);
    drop(pg);
    drop_db(&db_name).await?;
    Ok(())
}

/// An interpretation Perspective's implied connections show up as
/// `reference` `neighbor_edges` on `proxiima://memory`. There is no
/// `proxiima://edges` resource.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn edge_resources_read_back_interpretation_references()
-> Result<(), Box<dyn std::error::Error>> {
    let db_name = create_core_db().await?;
    let database_url = db_url(&db_name);
    let pg = storage(&database_url).await?;

    let owner = OwnerId::from(UserId::new(uuid::Uuid::now_v7()));
    let registry = SpiiceRegistry::new().freeze_or_panic_for_tests();
    let engine = Arc::new(Engine::new(
        registry.clone(),
        Arc::new(pg.clone()).storage_ports(),
    ));
    let server = McpToolHost::from_engine(engine, SpiiceServices::default());
    let auth = McpAuthContext {
        owner,
        authz: owner_authz(owner),
    };
    let hub = insert_memory(&server, &pg, &auth, "hub abstraction", &[]).await?;
    let spoke = insert_memory(&server, &pg, &auth, "spoke abstraction", &[]).await?;

    let interpreted = server
        .call_tool(
            "core_interpret",
            serde_json::json!({
                "claim": "the hub summarizes the spoke",
                "confidence": 61,
                "subjects": [format!("{hub}"), format!("{spoke}")],
            }),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    let handle = interpreted["handle"]
        .as_str()
        .expect("interpretation handle")
        .to_string();
    assert!(
        handle.parse::<uuid::Uuid>().is_ok(),
        "an interpretation is a Perspective"
    );
    assert_eq!(
        interpreted["edge_count"],
        serde_json::json!(2),
        "a count, not handles: {interpreted}"
    );

    let listed = server
        .read_resource(
            &format!("proxiima://memory/{handle}?expand_neighbors=true"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    let edges = listed["neighbor_edges"]
        .as_array()
        .expect("neighbor_edges array");
    assert_eq!(edges.len(), 2);
    let targets: Vec<&str> = edges
        .iter()
        .map(|edge| edge["target"].as_str().expect("target handle"))
        .collect();
    assert!(targets.contains(&format!("{hub}").as_str()), "{targets:?}");
    assert!(
        targets.contains(&format!("{spoke}").as_str()),
        "{targets:?}"
    );
    for edge in edges {
        assert_eq!(edge["source"], handle);
        assert_eq!(edge["kind"], "reference");
        assert!(
            edge.get("payload").is_none() && edge.get("relation").is_none(),
            "an edge carries no content: {edge}"
        );
        assert!(
            edge.get("edge").is_none(),
            "an edge has no id to hand back: {edge}"
        );
    }

    // Re-asserting the same judgment is one memory and the same two rows:
    // structural idempotency, with no id scheme to keep honest.
    let replay = server
        .call_tool(
            "core_interpret",
            serde_json::json!({
                "claim": "the hub summarizes the spoke",
                "confidence": 61,
                "subjects": [format!("{hub}"), format!("{spoke}")],
            }),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    assert_eq!(replay["handle"], serde_json::json!(handle));
    assert_eq!(replay["idempotent_replay"], serde_json::json!(true));

    let retired = server
        .read_resource(
            &format!("proxiima://edges?kind=reference&source={handle}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect_err("proxiima://edges is not a resource");
    assert!(
        matches!(
            &retired,
            proxiima_mcp_server::ToolInvocationError::ToolNotFound(_)
        ),
        "retired edges catalog is resource-not-found: {retired}"
    );

    drop(server);
    drop(pg);
    drop_db(&db_name).await?;
    Ok(())
}

/// A version UUID resolves to its series' current head.
async fn assert_version_id_reads_series_head(
    server: &McpToolHost,
    auth: &McpAuthContext,
    version: uuid::Uuid,
    head: &serde_json::Value,
) {
    let from_version = server
        .read_resource(
            &format!("proxiima://goal/{version}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect("a version id reads its series");
    assert_eq!(
        &from_version, head,
        "a superseded version id reads its series' current head"
    );
    let missing = server
        .read_resource(
            &format!("proxiima://goal/{}", proxiima_core::new_memory_uuid(0)),
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect_err("an id naming no version and no series is not found");
    assert!(missing.to_string().contains("not found"), "{missing}");
}

/// Goal UUIDs read successfully; prefixes and unknown states fail closed.
async fn assert_goal_reference_and_state_rejections(
    server: &McpToolHost,
    auth: &McpAuthContext,
    goal_id: uuid::Uuid,
) {
    let bare = server
        .read_resource(
            &format!("proxiima://goal/{goal_id}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect("bare UUID goal reference reads the current head");
    assert_eq!(bare["goal"], goal_id.to_string());
    let prefixed = server
        .read_resource(
            &format!("proxiima://goal/G:{goal_id}"),
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect_err("prefixed goal references are refused");
    assert!(
        prefixed.to_string().contains("hyphenated UUID"),
        "{prefixed}"
    );
    let bad_state = server
        .read_resource(
            "proxiima://goals?state=Everything",
            author_ctx(),
            Some(auth.clone()),
        )
        .await
        .expect_err("unknown state must be rejected");
    assert!(
        bad_state.to_string().contains("must be one of"),
        "{bad_state}"
    );
    for state in ["", " ", "Active,", "Active,,Paused"] {
        let error = server
            .read_resource(
                &format!("proxiima://goals?state={state}"),
                author_ctx(),
                Some(auth.clone()),
            )
            .await
            .expect_err("empty state members must be rejected");
        assert!(error.to_string().contains("empty"), "{state:?}: {error}");
    }
}

async fn insert_active_goal(
    pg: &PgStorage,
    owner: &OwnerId,
) -> Result<(uuid::Uuid, uuid::Uuid), Box<dyn std::error::Error>> {
    let request_id = format!("read-e2e:{}", uuid::Uuid::now_v7());
    let written = TestGoal::new(&request_id).create(pg, *owner).await?;
    Ok((written.handle, written.t))
}

/// A `UUIDv7` minted at an exact millisecond.
fn goal_t_at(millis: u64) -> uuid::Uuid {
    uuid::Uuid::new_v7(uuid::Timestamp::from_unix(
        uuid::NoContext,
        millis / 1000,
        u32::try_from(millis % 1000).expect("below 1000") * 1_000_000,
    ))
}

/// One version of a Goal series with its text, written straight to the
/// tables: `handle` `None` starts a new series. Returns the series handle.
async fn insert_goal_version(
    pg: &PgStorage,
    owner: &OwnerId,
    handle: Option<uuid::Uuid>,
    t: uuid::Uuid,
    state: &str,
    text: Option<&str>,
) -> Result<uuid::Uuid, Box<dyn std::error::Error>> {
    let owner_id = owner.into_inner();
    // Raw INSERT on purpose: a Goal version at an exact past `t`; `write_goal` mints `t`
    // from the clock, so no writer takes one.
    let handle = if let Some(handle) = handle {
        sqlx::query("UPDATE proxiima_core.goal_head SET t = $2 WHERE handle = $1")
            .bind(handle)
            .bind(t)
            .execute(pg.pool_for_tests())
            .await?;
        handle
    } else {
        let handle = proxiima_core::new_memory_uuid(0);
        sqlx::query(
            "INSERT INTO proxiima_core.goal_head (handle, schema_id, owner_id, t)
             VALUES ($1, 'core/simple-text-v1', $2, $3)",
        )
        .bind(handle)
        .bind(owner_id)
        .bind(t)
        .execute(pg.pool_for_tests())
        .await?;
        handle
    };
    sqlx::query(
        "INSERT INTO proxiima_core.goal (handle, t, owner_id, title, text, state, request_id, \"by\", via)
         VALUES ($1, $2, $3, 'report', $4, $5::proxiima_core.goal_state, $6, $3, NULL)",
    )
    .bind(handle)
    .bind(t)
    .bind(owner_id)
    .bind(text)
    .bind(state)
    .bind(format!("text-read:{t}"))
    .execute(pg.pool_for_tests())
    .await?;
    Ok(handle)
}

/// An Abstraction written through `core_derive`: an Abstraction with no
/// origins is derived from a fresh Fact. Returns its memory id.
async fn insert_memory(
    server: &McpToolHost,
    pg: &PgStorage,
    auth: &McpAuthContext,
    text: &str,
    origins: &[uuid::Uuid],
) -> Result<uuid::Uuid, Box<dyn std::error::Error>> {
    let sources = if origins.is_empty() {
        vec![
            TestMemory::fact()
                .create(pg, auth.owner)
                .await?
                .memory_id
                .into_inner(),
        ]
    } else {
        origins.to_vec()
    };
    let derived = server
        .call_tool(
            "core_derive",
            serde_json::json!({
                "kind": "Abstraction",
                "title": text,
                "body": text,
                "tags": [],
                "source_handles": sources.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "model_id": "test-model",
            }),
            author_ctx(),
            Some(auth.clone()),
        )
        .await?;
    Ok(derived["handle"]
        .as_str()
        .ok_or("derived handle")?
        .parse()?)
}

fn author_ctx() -> ToolCaller {
    ToolCaller::new("codex-test", "codex", "1")
}
