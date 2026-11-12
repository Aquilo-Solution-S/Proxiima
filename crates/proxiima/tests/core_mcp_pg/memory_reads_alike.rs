//! #479 items 2 and 3: a memory reads the same wherever it is reached from.
//! A by-id or batch read reports the memory's own space, not the caller's,
//! and a search hit and a by-id read of one memory report the same body and
//! tags, both derived from the schema's search declaration.

use super::*;
use proxiima_core::catalog::core_tools::{
    get_memory::GetMemoryOutput, recall::RecallOutput, think::ThinkOutput,
    walk_memory_lineage::WalkMemoryLineageOutput,
};

fn find<'a>(rows: &'a serde_json::Value, list: &str, handle: &str) -> &'a serde_json::Value {
    rows[list]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["memory"] == handle))
        .unwrap_or_else(|| panic!("{handle} in {list}: {rows}"))
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one fixture: spaces, follow-up write, body and tag parity
async fn a_memory_reads_the_same_from_search_and_by_id() {
    let split_db = clone_split_core_db("proxiima_core_memory_reads_alike")
        .await
        .expect("PG required for tests");
    let (runtime_url, platform_url) = (
        split_db.runtime_url().to_owned(),
        split_db.platform_url().to_owned(),
    );

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let personal = OwnerId::from(UserId::new(Uuid::now_v7()));
        let group = OwnerId::from(GroupId::new(Uuid::now_v7()));
        let built = Proxiima::<AgentMemoryApp>::app()
            .configure(|b| {
                b.database_url(runtime_url.clone())
                    .store_number(0)
                    .platform_database_url(platform_url.clone())
                    .owner(personal)
                    .served_surface(proxiima_core::ServedSurface::All)
            })
            .build()
            .await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(personal, vec![personal, group], Role::admin());
        let shared = server_issued_group_space_selector(&tools, authz.clone(), personal).await;
        assert_eq!(shared, group.to_string());
        let call = |name: &'static str, args: serde_json::Value| {
            call_test_model_tool(&tools, authz.clone(), personal, name, args)
        };
        let read = |uri: String| {
            let (tools, authz) = (&tools, authz.clone());
            async move { read_test_model_resource(tools, authz, personal, &uri).await }
        };
        let handle = |value: serde_json::Value| -> Result<String, Box<dyn std::error::Error>> {
            Ok(value["handle"].as_str().ok_or("handle")?.to_owned())
        };

        let m_p = handle(
            call(
                "core_remember",
                serde_json::json!({"space":"current","title":"alike personal",
                    "body":"alikeneedle personal note","tags":["alike"]}),
            )
            .await?,
        )?;
        let m_g = handle(
            call(
                "core_remember",
                serde_json::json!({"space":shared.clone(),"title":"alike group",
                    "body":"alikeneedle group note","tags":["grp"]}),
            )
            .await?,
        )?;

        // Item 2: the reported space is the memory's own.
        let by_id = read(format!("proxiima://memory/{m_g}")).await?;
        assert_eq!(
            by_id["space"], shared,
            "a group memory reports the group: {by_id}"
        );
        assert_eq!(by_id["owner"], group.to_string());
        let batch = read(format!("proxiima://memories?ids={m_p},{m_g}")).await?;
        assert_eq!(
            find(&batch, "memories", &m_p)["space"],
            "current",
            "{batch}"
        );
        assert_eq!(find(&batch, "memories", &m_g)["space"], shared, "{batch}");
        assert_eq!(
            find(&batch, "memories", &m_p)["owner"],
            personal.to_string()
        );
        assert_eq!(find(&batch, "memories", &m_g)["owner"], group.to_string());
        let group_current = read_test_model_resource(
            &tools,
            authz.clone(),
            group,
            &format!("proxiima://memory/{m_g}"),
        )
        .await?;
        assert_eq!(group_current["space"], "current");
        assert_eq!(group_current["owner"], group.to_string());

        // A follow-up write to the reported space lands with the group.
        let reported = by_id["space"].as_str().ok_or("space")?.to_owned();
        let follow_up = handle(
            call(
                "core_remember",
                serde_json::json!({"space":reported,"title":"alike follow-up",
                    "body":"alikeneedle follow-up note","tags":[]}),
            )
            .await?,
        )?;
        let follow_read = read(format!("proxiima://memory/{follow_up}")).await?;
        assert_eq!(follow_read["space"], shared, "{follow_read}");
        let in_group = call(
            "core_search_memories",
            serde_json::json!({"query":"alikeneedle","mode":"lexical","kind":"Fact",
                "spaces":[shared.clone()],"limit":10}),
        )
        .await?;
        let hit = find(&in_group, "memories", &follow_up);
        assert_eq!(
            hit["space"], shared,
            "a search hit reports the searched space"
        );

        // Item 3: an interpretation's body is its claim, in search and by id.
        let claim = "alikeclaim the personal note is a stance";
        let m_i = handle(
            call(
                "core_interpret",
                serde_json::json!({"claim":claim,"confidence":70,"subjects":[m_p.clone()]}),
            )
            .await?,
        )?;
        let search = |query: &'static str, kind: &'static str| {
            call(
                "core_search_memories",
                serde_json::json!({"query":query,"mode":"lexical","kind":kind,
                    "include_body":true,"limit":10}),
            )
        };
        let perspectives = search("alikeclaim", "Perspective").await?;
        let interpretation_hit = find(&perspectives, "memories", &m_i).clone();
        assert_eq!(interpretation_hit["body"], claim, "{perspectives}");
        let interpretation_read = read(format!("proxiima://memory/{m_i}")).await?;
        assert_eq!(interpretation_read["body"], claim, "{interpretation_read}");

        // Parity: the same memory reports the same body and tags either way.
        let facts = search("alikeneedle", "Fact").await?;
        let note_hit = find(&facts, "memories", &m_p).clone();
        let note_read = read(format!("proxiima://memory/{m_p}")).await?;
        assert_eq!(note_hit["tags"], serde_json::json!(["alike"]), "{facts}");
        for (hit, read) in [
            (&note_hit, &note_read),
            (&interpretation_hit, &interpretation_read),
        ] {
            assert_eq!(hit["body"], read["body"], "body parity: {hit} vs {read}");
            assert_eq!(hit["tags"], read["tags"], "tag parity: {hit} vs {read}");
        }

        built.shutdown().await;
        Ok(())
    }
    .await;

    result.expect("memory read parity test failed");
}

/// #676: every memory item a walk or recall names carries its own space, the
/// key a by-id read reports, whichever space the walk started from.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one fixture: two spaces, three walks, one recall
async fn lineage_think_and_recall_name_each_memorys_own_space() {
    let split_db = clone_split_core_db("proxiima_core_walk_spaces")
        .await
        .expect("PG required for tests");
    let (runtime_url, platform_url) = (
        split_db.runtime_url().to_owned(),
        split_db.platform_url().to_owned(),
    );

    let result: Result<(), Box<dyn std::error::Error>> = async {
        let personal = OwnerId::from(UserId::new(Uuid::now_v7()));
        let group = OwnerId::from(GroupId::new(Uuid::now_v7()));
        let built = Proxiima::<AgentMemoryApp>::app()
            .configure(|b| {
                b.database_url(runtime_url.clone())
                    .store_number(0)
                    .platform_database_url(platform_url.clone())
                    .owner(personal)
                    .served_surface(proxiima_core::ServedSurface::All)
            })
            .build()
            .await?;
        let tools = built.host().core_mcp_tools();
        let authz = space_authz(personal, vec![personal, group], Role::admin());
        let shared = server_issued_group_space_selector(&tools, authz.clone(), personal).await;
        let call = |name: &'static str, args: serde_json::Value| {
            call_test_model_tool(&tools, authz.clone(), personal, name, args)
        };
        let read = |uri: String| {
            let (tools, authz) = (&tools, authz.clone());
            async move { read_test_model_resource(tools, authz, personal, &uri).await }
        };
        let handle = |value: serde_json::Value| -> Result<String, Box<dyn std::error::Error>> {
            Ok(value["handle"].as_str().ok_or("handle")?.to_owned())
        };

        let m_p = handle(
            call(
                "core_remember",
                serde_json::json!({"space":"current","title":"walk personal",
                    "body":"walkspace personal note","tags":[]}),
            )
            .await?,
        )?;
        let m_g = handle(
            call(
                "core_remember",
                serde_json::json!({"space":shared.clone(),"title":"walk group",
                    "body":"walkspace group note","tags":[]}),
            )
            .await?,
        )?;
        // Written to the group, grounded in a memory of each space.
        let derived = handle(
            call(
                "core_derive",
                serde_json::json!({"space":shared.clone(),"kind":"Abstraction",
                    "title":"walk pattern","body":"walkspace pattern","tags":[],
                    "source_handles":[m_p.clone(), m_g.clone()],"model_id":"test-model"}),
            )
            .await?,
        )?;
        let expected = [
            (m_p.clone(), serde_json::json!("current")),
            (m_g.clone(), serde_json::json!(shared.clone())),
            (derived.clone(), serde_json::json!(shared.clone())),
        ];
        for (memory, space) in &expected {
            // The anchor: a by-id read says the same.
            let by_id = read(format!("proxiima://memory/{memory}")).await?;
            assert_eq!(&by_id["space"], space, "{by_id}");
        }
        let space_of = |rows: &serde_json::Value, list: &str, key: &str, memory: &str| {
            rows[list]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row[key] == memory))
                .map_or_else(
                    || panic!("{memory} in {list}: {rows}"),
                    |row| row["space"].clone(),
                )
        };

        let ancestors = read(format!("proxiima://memory/{derived}/lineage")).await?;
        for (memory, space) in &expected {
            assert_eq!(
                &space_of(&ancestors, "nodes", "memory", memory),
                space,
                "ancestor lineage node {memory}: {ancestors}"
            );
        }
        let descendants = read(format!(
            "proxiima://memory/{m_p}/lineage?direction=descendants"
        ))
        .await?;
        for (memory, space) in expected.iter().filter(|(memory, _)| *memory != m_g) {
            assert_eq!(
                &space_of(&descendants, "nodes", "memory", memory),
                space,
                "descendant lineage node {memory}: {descendants}"
            );
        }

        let think = call(
            "core_think",
            serde_json::json!({"seeds":[derived.clone()],"direction":"ancestors","depth":2}),
        )
        .await?;
        for (memory, space) in &expected {
            assert_eq!(
                &space_of(&think, "visits", "handle", memory),
                space,
                "think visit {memory}: {think}"
            );
        }
        let below = call(
            "core_think",
            serde_json::json!({"seeds":[m_p.clone()],"direction":"descendants","depth":1}),
        )
        .await?;
        assert_eq!(
            space_of(&below, "visits", "handle", &derived),
            serde_json::json!(shared),
            "{below}"
        );

        let recall = call(
            "core_recall",
            serde_json::json!({"subjects":[m_p.clone(), m_g.clone(), derived.clone()]}),
        )
        .await?;
        for (memory, space) in &expected {
            assert_eq!(
                &space_of(&recall, "sketches", "handle", memory),
                space,
                "recall sketch {memory}: {recall}"
            );
        }

        // An embedder parses each of these answers into its published type.
        serde_json::from_value::<WalkMemoryLineageOutput>(ancestors)?;
        serde_json::from_value::<WalkMemoryLineageOutput>(descendants)?;
        serde_json::from_value::<ThinkOutput>(think)?;
        serde_json::from_value::<ThinkOutput>(below)?;
        serde_json::from_value::<RecallOutput>(recall)?;
        serde_json::from_value::<GetMemoryOutput>(
            read(format!("proxiima://memory/{derived}")).await?,
        )?;

        built.shutdown().await;
        Ok(())
    }
    .await;

    result.expect("walk and recall space test failed");
}
