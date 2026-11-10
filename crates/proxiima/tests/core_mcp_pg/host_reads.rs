//! #677: the host's read set — the memories a session read before it wrote
//! — is kept on every memory the session writes as `read` pins, returned on
//! read, walkable both ways, checked at the write, and covered by erase.
//! It comes from the host's call context, never from a tool argument.

use proxiima::{BuiltProxiima, Proxiima, ReadPin, ReadSet};
use proxiima_core::{AuthzContext, GroupId, MemoryId, OwnerId, Role, UserId};
use uuid::Uuid;

use super::split_core_db::clone_split_core_db;
use super::{
    AgentMemoryApp, call_test_model_tool, read_test_model_resource,
    server_issued_group_space_selector, space_authz,
};

/// `authz` with the host's read set naming each `(memory, space)`.
fn reading(authz: &AuthzContext, reads: &[(&str, OwnerId)]) -> AuthzContext {
    let pins = reads.iter().map(|(memory, space)| ReadPin::Local {
        memory: MemoryId::new(Uuid::parse_str(memory).expect("memory uuid")),
        space: *space,
    });
    authz
        .clone()
        .with_read_set(ReadSet::new(pins).expect("one space per memory"))
        .expect("the read set binds")
}

/// The agent-memory app on the split test databases, bound to `owner`.
async fn boot(
    runtime_url: &str,
    platform_url: &str,
    owner: OwnerId,
) -> Result<BuiltProxiima, proxiima::ProxiimaError> {
    Proxiima::<AgentMemoryApp>::app()
        .database_url(runtime_url.to_owned())
        .store_number(0)
        .platform_database_url(platform_url.to_owned())
        .owner(owner)
        .served_surface(proxiima_core::ServedSurface::All)
        .build()
        .await
}

fn handle(made: &serde_json::Value) -> String {
    made["handle"].as_str().expect("handle").to_owned()
}

/// `(target, kind)` of every edge in `edges` whose source is `source`.
fn edges_from<'a>(edges: &'a serde_json::Value, source: &str) -> Vec<(&'a str, &'a str)> {
    edges
        .as_array()
        .expect("edges")
        .iter()
        .filter(|edge| edge["source"] == source)
        .map(|edge| {
            (
                edge["target"].as_str().expect("target"),
                edge["kind"].as_str().expect("kind"),
            )
        })
        .collect()
}

/// `(source, target, kind)` of every edge in `edges`.
fn edge_triples(edges: &serde_json::Value) -> Vec<(&str, &str, &str)> {
    edges
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| {
            (
                edge["source"].as_str().expect("source"),
                edge["target"].as_str().expect("target"),
                edge["kind"].as_str().expect("kind"),
            )
        })
        .collect()
}

/// `(handle, edge)` of every visit of a `core_think` page.
fn visit_edges(think: &serde_json::Value) -> Vec<(&str, &str)> {
    think["visits"]
        .as_array()
        .expect("visits")
        .iter()
        .filter_map(|visit| Some((visit["handle"].as_str()?, visit["edge"].as_str()?)))
        .collect()
}

/// The stored `(origins, reads)` of `memory`, read as `owner`.
async fn stored_pins(
    pool: &sqlx::PgPool,
    owner: OwnerId,
    memory: &str,
) -> Result<(Vec<Uuid>, Vec<Uuid>), Box<dyn std::error::Error>> {
    let scope = proxiima_core::test_fixtures::owner_scope_for_tests(&[owner]);
    let mut tx = proxiima_storage_pg::begin_owner_transaction(pool, &scope).await?;
    let pins = sqlx::query_as("SELECT origins, reads FROM proxiima_core.memory WHERE t = $1")
        .bind(Uuid::parse_str(memory)?)
        .fetch_one(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(pins)
}

/// Memory rows of `owner`, counted as `owner`.
async fn memory_count(
    pool: &sqlx::PgPool,
    owner: OwnerId,
) -> Result<i64, Box<dyn std::error::Error>> {
    let scope = proxiima_core::test_fixtures::owner_scope_for_tests(&[owner]);
    let mut tx = proxiima_storage_pg::begin_owner_transaction(pool, &scope).await?;
    let count = sqlx::query_scalar("SELECT count(*) FROM proxiima_core.memory WHERE owner_id = $1")
        .bind(owner.into_inner())
        .fetch_one(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(count)
}

/// A Fact and an Abstraction written under a host read set keep it as
/// `read` pins: neighbours name the read memories with kind `read`, a
/// descendants walk from a read memory reaches the writer, and the Fact's
/// origins stay empty.
#[tokio::test]
async fn a_host_read_set_is_kept_returned_and_walked() {
    let split_db = clone_split_core_db("proxiima_host_reads")
        .await
        .expect("PG required for tests");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let built = boot(split_db.runtime_url(), split_db.platform_url(), owner).await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(owner, vec![owner], Role::admin());
        let remember = |authz: AuthzContext, title: &'static str| {
            call_test_model_tool(
                &tools,
                authz,
                owner,
                "core_remember",
                serde_json::json!({"title": title, "body": title, "tags": []}),
            )
        };

        let seen = handle(&remember(authz.clone(), "seen fact").await?);
        let other = handle(&remember(authz.clone(), "other fact").await?);
        let reads = reading(&authz, &[(&seen, owner)]);
        let fact = handle(&remember(reads.clone(), "fact written after reading").await?);
        let derived = handle(
            &call_test_model_tool(
                &tools,
                reading(&authz, &[(&fact, owner)]),
                owner,
                "core_derive",
                serde_json::json!({
                    "kind": "Abstraction",
                    "title": "conclusion",
                    "body": "conclusion",
                    "source_handles": [other],
                    "model_id": "test-model"
                }),
            )
            .await?,
        );

        // (a) neighbours of the writer name what it read, as `read`.
        let read = |uri: String| {
            let (tools, authz) = (&tools, authz.clone());
            async move { read_test_model_resource(tools, authz, owner, &uri).await }
        };
        let fact_read = read(format!("proxiima://memory/{fact}?expand_neighbors=true")).await?;
        assert_eq!(
            edges_from(&fact_read["neighbor_edges"], &fact),
            [(seen.as_str(), "read")],
            "{fact_read}"
        );
        let derived_read =
            read(format!("proxiima://memory/{derived}?expand_neighbors=true")).await?;
        let mut derived_edges = edges_from(&derived_read["neighbor_edges"], &derived);
        derived_edges.sort_unstable();
        let mut expected = vec![(other.as_str(), "origin"), (fact.as_str(), "read")];
        expected.sort_unstable();
        assert_eq!(derived_edges, expected, "{derived_read}");

        // (d) a Fact's read set never becomes an origin.
        let pool = built.host().pool_for_tests();
        let (origins, stored_reads) = stored_pins(pool, owner, &fact).await?;
        assert!(origins.is_empty(), "a Fact has no origins: {origins:?}");
        assert_eq!(stored_reads, [Uuid::parse_str(&seen)?]);

        // (b) descendants of a read memory reach its writer, in the lineage
        // walk and in core_think; ancestors of the writer reach the read.
        let lineage = read(format!(
            "proxiima://memory/{seen}/lineage?direction=descendants&depth=2"
        ))
        .await?;
        let lineage_edges = edge_triples(&lineage["edges"]);
        assert!(
            lineage_edges.contains(&(fact.as_str(), seen.as_str(), "read")),
            "{lineage}"
        );
        assert!(
            lineage_edges.contains(&(derived.as_str(), fact.as_str(), "read")),
            "a second hop over a read pin: {lineage}"
        );
        let think = call_test_model_tool(
            &tools,
            authz.clone(),
            owner,
            "core_think",
            serde_json::json!({"seeds": [seen], "direction": "descendants", "depth": 2}),
        )
        .await?;
        let visits = visit_edges(&think);
        assert!(visits.contains(&(fact.as_str(), "read")), "{think}");
        assert!(visits.contains(&(derived.as_str(), "read")), "{think}");
        let ancestors = read(format!(
            "proxiima://memory/{fact}/lineage?direction=ancestors&depth=1"
        ))
        .await?;
        assert_eq!(
            edges_from(&ancestors["edges"], &fact),
            [(seen.as_str(), "read")],
            "{ancestors}"
        );
        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("host read set");
}

/// A derive from a Fact the session also read stores that Fact once, as
/// the declared origin: ancestors, descendants, think and neighbours all
/// label the edge `origin`, never `read`.
#[tokio::test]
async fn a_declared_origin_wins_over_a_read_of_the_same_memory() {
    let split_db = clone_split_core_db("proxiima_host_reads_origin_wins")
        .await
        .expect("PG required for tests");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let built = boot(split_db.runtime_url(), split_db.platform_url(), owner).await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(owner, vec![owner], Role::admin());
        let source = handle(
            &call_test_model_tool(
                &tools,
                authz.clone(),
                owner,
                "core_remember",
                serde_json::json!({"title": "source", "body": "source", "tags": []}),
            )
            .await?,
        );
        let derived = handle(
            &call_test_model_tool(
                &tools,
                reading(&authz, &[(&source, owner)]),
                owner,
                "core_derive",
                serde_json::json!({
                    "kind": "Abstraction",
                    "title": "conclusion",
                    "body": "conclusion",
                    "source_handles": [source],
                    "model_id": "test-model"
                }),
            )
            .await?,
        );
        let expected = [(derived.as_str(), source.as_str(), "origin")];
        let read = |uri: String| {
            let (tools, authz) = (&tools, authz.clone());
            async move { read_test_model_resource(tools, authz, owner, &uri).await }
        };

        let (origins, reads) = stored_pins(built.host().pool_for_tests(), owner, &derived).await?;
        assert_eq!(origins, [Uuid::parse_str(&source)?], "stored once");
        assert!(reads.is_empty(), "as the origin, not a read: {reads:?}");
        let ancestors = read(format!(
            "proxiima://memory/{derived}/lineage?direction=ancestors&depth=1"
        ))
        .await?;
        assert_eq!(edge_triples(&ancestors["edges"]), expected, "{ancestors}");
        let descendants = read(format!(
            "proxiima://memory/{source}/lineage?direction=descendants&depth=1"
        ))
        .await?;
        assert_eq!(
            edge_triples(&descendants["edges"]),
            expected,
            "{descendants}"
        );
        let neighbours = read(format!("proxiima://memory/{derived}?expand_neighbors=true")).await?;
        assert_eq!(
            edge_triples(&neighbours["neighbor_edges"]),
            expected,
            "{neighbours}"
        );
        let think = call_test_model_tool(
            &tools,
            authz.clone(),
            owner,
            "core_think",
            serde_json::json!({"seeds": [source], "direction": "descendants", "depth": 1}),
        )
        .await?;
        assert_eq!(
            visit_edges(&think),
            [(derived.as_str(), "origin")],
            "{think}"
        );
        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("origin wins over read");
}

/// A read set naming a memory the caller cannot read — one that does not
/// exist, one in another owner's space, or a readable one under the wrong
/// space — refuses the whole write, and nothing is written. A `reads`
/// tool argument is refused: the read set is never a tool argument.
#[tokio::test]
async fn an_unreadable_read_refuses_the_whole_write() {
    let split_db = clone_split_core_db("proxiima_host_reads_refused")
        .await
        .expect("PG required for tests");
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let stranger = OwnerId::from(UserId::new(Uuid::now_v7()));
        let built = boot(split_db.runtime_url(), split_db.platform_url(), owner).await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(owner, vec![owner], Role::admin());
        let stranger_authz = space_authz(stranger, vec![stranger], Role::admin());
        let remember = |authz: AuthzContext, as_owner: OwnerId, args: serde_json::Value| {
            call_test_model_tool(&tools, authz, as_owner, "core_remember", args)
        };
        let fact = |title: &str| serde_json::json!({"title": title, "body": title, "tags": []});

        let mine = handle(&remember(authz.clone(), owner, fact("mine")).await?);
        let theirs = handle(&remember(stranger_authz, stranger, fact("theirs")).await?);
        let missing = proxiima_core::new_memory_uuid(0).to_string();
        let pool = built.host().pool_for_tests();
        let before = memory_count(pool, owner).await?;

        for (label, reads) in [
            ("nonexistent", vec![(missing.as_str(), owner)]),
            ("another owner's", vec![(theirs.as_str(), stranger)]),
            ("wrong space", vec![(mine.as_str(), stranger)]),
            (
                "one bad among good",
                vec![(mine.as_str(), owner), (missing.as_str(), owner)],
            ),
        ] {
            let refused = remember(reading(&authz, &reads), owner, fact("refused")).await;
            let err = refused.expect_err(label);
            assert!(
                err.to_string().contains("not readable in the named space"),
                "{label}: {err}"
            );
            let derive = call_test_model_tool(
                &tools,
                reading(&authz, &reads),
                owner,
                "core_derive",
                serde_json::json!({
                    "kind": "Abstraction",
                    "title": "refused conclusion",
                    "body": "refused conclusion",
                    "source_handles": [mine],
                    "model_id": "test-model"
                }),
            )
            .await;
            assert!(derive.is_err(), "{label}: derive refused too");
        }
        assert_eq!(
            memory_count(pool, owner).await?,
            before,
            "a refused write writes nothing"
        );

        // (f) a read set is host context only: no tool takes it.
        let mut args = fact("smuggled");
        args["reads"] = serde_json::json!([mine]);
        assert!(remember(authz.clone(), owner, args).await.is_err());
        for tool in tools.list_core_tools() {
            assert!(
                !names_reads(&tool.args_schema),
                "{} takes a reads argument",
                tool.name
            );
        }
        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("unreadable read refused");
}

/// Whether a JSON schema declares a `reads` property anywhere.
fn names_reads(schema: &serde_json::Value) -> bool {
    match schema {
        serde_json::Value::Object(map) => {
            map.get("properties")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|properties| properties.contains_key("reads"))
                || map.values().any(names_reads)
        }
        serde_json::Value::Array(items) => items.iter().any(names_reads),
        _ => false,
    }
}

/// (e) Erasing a group that holds a read target leaves the personal
/// writer's pin naming an `erased target`; erasing a group that holds the
/// writer leaves no inbound read pin on the personal memory it read.
#[tokio::test]
async fn erase_of_a_read_target_or_its_writer_leaves_no_dangling_pin() {
    let split_db = clone_split_core_db("proxiima_host_reads_erase")
        .await
        .expect("PG required for tests");
    let (runtime_url, platform_url) = (
        split_db.runtime_url().to_owned(),
        split_db.platform_url().to_owned(),
    );
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let personal = OwnerId::from(UserId::new(Uuid::now_v7()));
        let group_id = GroupId::new(Uuid::now_v7());
        let group = OwnerId::from(group_id);
        let built = boot(&runtime_url, &platform_url, personal).await?;
        super::erased_targets::register_abandoned_group(&platform_url, group_id).await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(personal, vec![personal, group], Role::admin());
        let shared = server_issued_group_space_selector(&tools, authz.clone(), personal).await;
        let remember = |authz: AuthzContext, space: Option<&str>, title: &str| {
            let mut args = serde_json::json!({"title": title, "body": title, "tags": []});
            if let Some(space) = space {
                args["space"] = serde_json::json!(space);
            }
            call_test_model_tool(&tools, authz, personal, "core_remember", args)
        };

        let personal_seen = handle(&remember(authz.clone(), None, "personal seen").await?);
        let group_seen = handle(&remember(authz.clone(), Some(&shared), "group seen").await?);
        // A personal writer reads the group memory; a group writer reads the
        // personal memory.
        let personal_writer = handle(
            &remember(
                reading(&authz, &[(&group_seen, group)]),
                None,
                "personal writer",
            )
            .await?,
        );
        let group_writer = handle(
            &remember(
                reading(&authz, &[(&personal_seen, personal)]),
                Some(&shared),
                "group writer",
            )
            .await?,
        );
        let read = |uri: String| {
            let (tools, authz) = (&tools, authz.clone());
            async move { read_test_model_resource(tools, authz, personal, &uri).await }
        };
        let before = read(format!(
            "proxiima://memory/{personal_seen}?expand_neighbors=true"
        ))
        .await?;
        assert_eq!(
            edges_from(&before["neighbor_edges"], &group_writer),
            [(personal_seen.as_str(), "read")],
            "control: the group writer's read pin is visible before the erase: {before}"
        );

        super::erased_targets::erase_group(built.host().engine(), group_id).await?;

        let writer = read(format!(
            "proxiima://memory/{personal_writer}?expand_neighbors=true"
        ))
        .await?;
        assert_eq!(
            edges_from(&writer["neighbor_edges"], &personal_writer),
            [("erased target", "read")],
            "{writer}"
        );
        let seen = read(format!(
            "proxiima://memory/{personal_seen}?expand_neighbors=true"
        ))
        .await?;
        assert!(
            edges_from(&seen["neighbor_edges"], &group_writer).is_empty(),
            "the erased writer's read pin is gone: {seen}"
        );
        let pool = built.host().pool_for_tests();
        let scope = proxiima_core::test_fixtures::owner_scope_for_tests(&[personal, group]);
        let mut tx = proxiima_storage_pg::begin_owner_transaction(pool, &scope).await?;
        let pinning: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM proxiima_core.memory WHERE reads @> ARRAY[$1::uuid]",
        )
        .bind(Uuid::parse_str(&personal_seen)?)
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        assert_eq!(pinning, 0, "no memory still pins the personal read target");
        built.shutdown().await;
        Ok(())
    }
    .await;
    result.expect("erase covers read pins");
}
