use super::*;
use proxiima_core::verbs::query::SearchCursor;

#[tokio::test]
async fn listing_pages_heads_newest_first_with_owner_isolation()
-> Result<(), Box<dyn std::error::Error>> {
    let (pg, _guard) = proxiima_storage_pg::test_fixtures::fresh_pg("listing_heads").await;
    let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
    let other = OwnerId::from(UserId::new(Uuid::now_v7()));
    let old = TestNote::new("Old version", "old listing text")
        .create(&pg, owner)
        .await?;
    let head = TestNote::new("Current version", "current listing text")
        .handle(old.handle)
        .create(&pg, owner)
        .await?;
    let mut expected = vec![head.memory_id.into_inner()];
    for title in ["Second", "Third", "Fourth"] {
        expected.push(seed_note(&pg, owner, title, "listing body").await?);
    }
    let foreign = seed_note(&pg, other, "Foreign", "listing body").await?;
    expected.sort_unstable_by(|a, b| b.cmp(a));
    let scope = owner_scope_for_tests(&[owner]);
    let mut req = search_req(owner, "unused");
    req.query = None;
    req.order = SearchOrder::Recency;
    req.limit = 2;
    // A broader selector still cannot exceed the serving role's witness.
    req.read_owners.push(other);
    let mut actual = Vec::new();
    let mut continued = Vec::new();
    for _ in 0..expected.len() {
        let page = pg
            .search_memories(Some(&scope), &req, &[note_projection()])
            .await?;
        for hit in &page.results {
            assert_eq!(
                (hit.score, hit.lexical_score, hit.similarity_score),
                (0.0, 0.0, 0.0)
            );
            assert!(!hit.snippet.is_empty(), "listing hydrates snippets");
            actual.push(hit.memory_id.into_inner());
        }
        continued.push(page.has_more);
        if !page.has_more {
            break;
        }
        let last = page
            .results
            .last()
            .expect("a continued page contains a row");
        req.after = Some(SearchCursor::Recency {
            created_at: last.created_at,
            memory_id: last.memory_id,
            seen: u32::try_from(actual.len())?,
        });
    }
    assert_eq!(actual, expected, "keysets neither repeat nor skip heads");
    assert_eq!(continued, [true, false], "four heads fill two pages of two");
    assert!(!actual.contains(&old.memory_id.into_inner()));
    assert!(!actual.contains(&foreign));
    req.after = None;
    req.limit = 8;
    req.supersession = SupersessionStatus::IncludeSuperseded;
    let all = pg
        .search_memories(Some(&scope), &req, &[note_projection()])
        .await?;
    assert_eq!(all.results.len(), expected.len() + 1);
    assert!(all.results.iter().any(|hit| hit.memory_id == old.memory_id));
    Ok(())
}

#[tokio::test]
async fn listing_and_query_exclude_schemas_before_limit_and_share_tag_filters()
-> Result<(), Box<dyn std::error::Error>> {
    let db_name = format!("proxiima_test_{}", Uuid::now_v7().simple());
    create_core_db(&db_name).await?;
    let result: Result<(), Box<dyn std::error::Error>> = async {
        split_role_urls_for(&db_name, &["proxiima_docs"]).await?;
        let pg = PgStorage::connect_for_tests(db_url(&db_name)).await?;
        let pool = pg.pool_for_tests();
        let owner = OwnerId::from(UserId::new(Uuid::now_v7()));
        let other = OwnerId::from(UserId::new(Uuid::now_v7()));
        create_docs_search_surface(pool).await?;
        let first =
            seed_note_lang(&pg, owner, "First", "quartz listing", None, &["shared"]).await?;
        let second = seed_note_lang(
            &pg,
            owner,
            "Second",
            "quartz listing",
            None,
            &["shared", "selected"],
        )
        .await?;
        let mut allowed = vec![first, second];
        allowed.sort_unstable_by(|a, b| b.cmp(a));
        let mut docs = Vec::new();
        for _ in 0..4 {
            docs.push(
                seed_docs_note(pool, owner, first, "quartz docs", &["shared", "selected"]).await?,
            );
        }
        let foreign = seed_note(&pg, other, "Foreign", "quartz listing").await?;
        let projections = [note_projection(), docs_projection()];
        let scope = owner_scope_for_tests(&[owner]);
        let mut req = search_req(owner, "quartz");
        req.kind = None;
        req.order = SearchOrder::Recency;
        req.limit = 2;
        req.query = None;
        let newest = pg.search_memories(Some(&scope), &req, &projections).await?;
        assert_eq!(newest.results.len(), 2);
        assert!(
            newest
                .results
                .iter()
                .all(|hit| docs.contains(&hit.memory_id.into_inner())),
            "newest rows have the schema to exclude"
        );
        req.exclude_schema_ids = vec!["proxiima-docs/section-text-v1".into()];
        for query in [None, Some("quartz".into())] {
            req.query = query;
            let page = pg.search_memories(Some(&scope), &req, &projections).await?;
            assert_eq!(
                page.results
                    .iter()
                    .map(|hit| hit.memory_id.into_inner())
                    .collect::<Vec<_>>(),
                allowed,
                "exclusions are applied before the limit"
            );
            assert!(!page.has_more);
            assert!(
                !page
                    .results
                    .iter()
                    .any(|hit| hit.memory_id.into_inner() == foreign)
            );
        }
        for id in allowed.iter().chain(&docs) {
            seed_embedding(&pg, owner, *id, &embed_vector()).await?;
        }
        req.query = Some("quartz".into());
        req.mode = SearchMode::Semantic;
        req.semantic = Some(test_semantic(embed_vector().to_vec()));
        let semantic = pg.search_memories(Some(&scope), &req, &projections).await?;
        assert_eq!(
            semantic
                .results
                .iter()
                .map(|hit| hit.memory_id.into_inner())
                .collect::<Vec<_>>(),
            allowed,
            "semantic admission uses the excluded projection set"
        );
        req.mode = SearchMode::Lexical;
        req.semantic = None;
        req.query = None;
        for (tag_match, tags, expected) in [
            (TagMatch::Any, vec!["selected", "absent"], vec![second]),
            (TagMatch::All, vec!["shared", "selected"], vec![second]),
            (TagMatch::All, vec!["shared", "absent"], Vec::new()),
        ] {
            req.tag_match = tag_match;
            req.tags = tags.into_iter().map(str::to_owned).collect();
            let page = pg.search_memories(Some(&scope), &req, &projections).await?;
            assert_eq!(
                page.results
                    .iter()
                    .map(|hit| hit.memory_id.into_inner())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        // Every optional narrowing at once: time bounds, the keyset and tags.
        req.tag_match = TagMatch::Any;
        req.tags = vec!["shared".into()];
        let page = pg.search_memories(Some(&scope), &req, &projections).await?;
        let newer = &page.results[0];
        assert_eq!(newer.memory_id.into_inner(), second);
        req.since = Some(newer.created_at - time::Duration::hours(1));
        req.until = Some(newer.created_at + time::Duration::hours(1));
        req.after = Some(SearchCursor::Recency {
            created_at: newer.created_at,
            memory_id: newer.memory_id,
            seen: 1,
        });
        let page = pg.search_memories(Some(&scope), &req, &projections).await?;
        assert_eq!(
            page.results
                .iter()
                .map(|hit| hit.memory_id.into_inner())
                .collect::<Vec<_>>(),
            vec![first]
        );
        Ok(())
    }
    .await;
    drop_db(&db_name).await?;
    result
}
