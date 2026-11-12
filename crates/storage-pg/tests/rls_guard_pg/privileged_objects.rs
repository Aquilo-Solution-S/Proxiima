//! Catalog refusals for objects that can change whose RLS/SQL authority applies.

use proxiima_core::SqlText;
use proxiima_core::{Credentials, GroupId, OwnerId, OwnerRoles, Role, UserId};
use proxiima_storage_pg::{PgPlatformScope, assert_runtime_rls, begin_owner_transaction};
use sqlx::PgPool;
use uuid::Uuid;

use super::{SyntheticVerifier, cleanup, execute, runtime_pool, setup};

async fn assert_guards_refuse_named(
    runtime: &PgPool,
    platform: &PgPool,
    schema: &str,
    object: &str,
    reason: &str,
) {
    let expected_name = format!("{schema}.{object}");
    let runtime_message = assert_runtime_rls(runtime, &[schema])
        .await
        .expect_err("runtime guard must refuse the object")
        .to_string();
    let platform_message = PgPlatformScope::new(platform.clone(), &[schema])
        .await
        .expect_err("platform guard must refuse the object")
        .to_string();
    for message in [runtime_message, platform_message] {
        assert!(message.contains(&expected_name), "{message}");
        assert!(message.contains(reason), "{message}");
    }
}

async fn assert_guards_accept(runtime: &PgPool, platform: &PgPool, schema: &str) {
    assert_runtime_rls(runtime, &[schema]).await.unwrap();
    PgPlatformScope::new(platform.clone(), &[schema])
        .await
        .unwrap();
}

#[tokio::test]
async fn platform_guard_completes_with_one_connection() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    for row_owner in [Uuid::now_v7(), Uuid::now_v7()] {
        // Raw INSERT: no write path for the memory table reachable from test fixtures; the test creates it
        sqlx::query(ddl!(
            "INSERT INTO ",
            id(&schema),
            ".memory(id, owner_id) VALUES ($1, $1)"
        ))
        .bind(row_owner)
        .execute(&admin)
        .await
        .unwrap();
    }
    let options = proxiima_pg_testkit::db_url(&database)
        .parse::<sqlx::postgres::PgConnectOptions>()
        .unwrap()
        .username(&owner)
        .password(&password);
    let platform = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let scope = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        PgPlatformScope::new(platform.clone(), &[&schema]),
    )
    .await
    .expect("platform validation must complete without acquiring a second connection")
    .unwrap();
    let mut transaction = tokio::time::timeout(std::time::Duration::from_secs(5), scope.begin())
        .await
        .expect("platform validation must release the connection")
        .unwrap();
    let count: i64 = sqlx::query_scalar(ddl!("SELECT count(*) FROM ", id(&schema), ".memory"))
        .fetch_one(&mut *transaction)
        .await
        .unwrap();
    assert_eq!(count, 2, "the validated scope enables the platform policy");
    transaction.commit().await.unwrap();
    let unscoped_count: i64 =
        sqlx::query_scalar(ddl!("SELECT count(*) FROM ", id(&schema), ".memory"))
            .fetch_one(&platform)
            .await
            .unwrap();
    assert_eq!(unscoped_count, 0, "scope must end with the transaction");
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one isolated view leak and repair scenario"
)]
async fn platform_definer_view_leaks_forged_scope_but_invoker_view_is_owner_scoped() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    let platform = runtime_pool(&database, &owner, &password).await;
    let owners = [Uuid::now_v7(), Uuid::now_v7()];
    for row_owner in owners {
        // Raw INSERT: no write path for the memory table reachable from test fixtures; the test creates it
        sqlx::query(ddl!(
            "INSERT INTO ",
            id(&schema),
            ".memory(id, owner_id) VALUES ($1, $2)"
        ))
        .bind(row_owner)
        .bind(row_owner)
        .execute(&admin)
        .await
        .unwrap();
    }
    execute(
        &admin,
        ddl!(
            "CREATE VIEW ",
            id(&schema),
            ".memory_view AS SELECT owner_id FROM ",
            id(&schema),
            ".memory"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "ALTER VIEW ",
            id(&schema),
            ".memory_view OWNER TO ",
            id(&owner)
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "GRANT SELECT ON ",
            id(&schema),
            ".memory_view TO ",
            id(&runtime_role)
        ),
    )
    .await;

    let mut forged = runtime.begin().await.unwrap();
    sqlx::query("SET LOCAL app.proxiima_scope = 'platform'")
        .execute(&mut *forged)
        .await
        .unwrap();
    let direct: i64 = sqlx::query_scalar(ddl!("SELECT count(*) FROM ", id(&schema), ".memory"))
        .fetch_one(&mut *forged)
        .await
        .unwrap();
    let through_view: i64 =
        sqlx::query_scalar(ddl!("SELECT count(*) FROM ", id(&schema), ".memory_view"))
            .fetch_one(&mut *forged)
            .await
            .unwrap();
    assert_eq!(
        direct, 0,
        "the runtime cannot use the platform table policy"
    );
    assert_eq!(
        through_view, 2,
        "the definer view supplies the platform owner's identity"
    );
    forged.rollback().await.unwrap();
    assert_guards_refuse_named(
        &runtime,
        &platform,
        &schema,
        "memory_view",
        "security_invoker=true",
    )
    .await;

    execute(
        &admin,
        ddl!(
            "ALTER VIEW ",
            id(&schema),
            ".memory_view SET (security_invoker = true)"
        ),
    )
    .await;
    assert_guards_accept(&runtime, &platform, &schema).await;
    let verifier = SyntheticVerifier {
        roles: OwnerRoles::for_subject(
            UserId::new(Uuid::now_v7()),
            [(OwnerId::from(GroupId::new(owners[0])), Role::viewer())],
        )
        .unwrap(),
    };
    let authz = proxiima_core::authenticate(&verifier, &Credentials::Bearer("view-test".into()))
        .await
        .unwrap();
    let mut scoped = begin_owner_transaction(&runtime, authz.owner_scope().unwrap())
        .await
        .unwrap();
    let visible: Vec<Uuid> =
        sqlx::query_scalar(ddl!("SELECT owner_id FROM ", id(&schema), ".memory_view"))
            .fetch_all(&mut *scoped)
            .await
            .unwrap();
    assert_eq!(visible, vec![owners[0]]);
    scoped.rollback().await.unwrap();

    execute(
        &admin,
        ddl!(
            "ALTER VIEW ",
            id(&schema),
            ".memory_view OWNER TO CURRENT_USER"
        ),
    )
    .await;
    let refused = PgPlatformScope::new(platform.clone(), &[&schema])
        .await
        .expect_err("an invoker view left with a legacy owner must refuse platform boot")
        .to_string();
    assert!(
        refused.contains(&format!("{schema}.memory_view")),
        "{refused}"
    );
    assert!(refused.contains("platform role does not own"), "{refused}");
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

#[tokio::test]
async fn catalog_census_refuses_materialized_views_and_foreign_tables_without_runtime_grants() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    let platform = runtime_pool(&database, &owner, &password).await;
    execute(
        &admin,
        ddl!(
            "CREATE MATERIALIZED VIEW ",
            id(&schema),
            ".cached_memory AS SELECT owner_id FROM ",
            id(&schema),
            ".memory WITH NO DATA"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "ALTER MATERIALIZED VIEW ",
            id(&schema),
            ".cached_memory OWNER TO ",
            id(&owner)
        ),
    )
    .await;
    assert_guards_refuse_named(
        &runtime,
        &platform,
        &schema,
        "cached_memory",
        "unsupported materialized view",
    )
    .await;
    execute(
        &admin,
        ddl!("DROP MATERIALIZED VIEW ", id(&schema), ".cached_memory"),
    )
    .await;

    // No handler is needed: this test inspects the catalog and performs no foreign I/O.
    execute(&admin, "CREATE FOREIGN DATA WRAPPER fixture_fdw").await;
    execute(
        &admin,
        "CREATE SERVER fixture_server FOREIGN DATA WRAPPER fixture_fdw",
    )
    .await;
    execute(
        &admin,
        ddl!(
            "CREATE FOREIGN TABLE ",
            id(&schema),
            ".remote_memory(id uuid, owner_id uuid) SERVER fixture_server"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "ALTER FOREIGN TABLE ",
            id(&schema),
            ".remote_memory OWNER TO ",
            id(&owner)
        ),
    )
    .await;
    assert_guards_refuse_named(
        &runtime,
        &platform,
        &schema,
        "remote_memory",
        "unsupported foreign table",
    )
    .await;
    execute(
        &admin,
        ddl!("DROP FOREIGN TABLE ", id(&schema), ".remote_memory"),
    )
    .await;
    assert_guards_accept(&runtime, &platform, &schema).await;
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

async fn install_definer(admin: &PgPool, schema: &str, owner: &str, runtime_role: &str) {
    execute(
        admin,
        ddl!("CREATE FUNCTION ", id(schema), ".owner_reader() RETURNS integer LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, ", id(schema), ", pg_temp AS 'SELECT 1'"),
    )
    .await;
    execute(
        admin,
        ddl!(
            "ALTER FUNCTION ",
            id(schema),
            ".owner_reader() OWNER TO ",
            id(owner)
        ),
    )
    .await;
    execute(
        admin,
        ddl!(
            "REVOKE ALL ON FUNCTION ",
            id(schema),
            ".owner_reader() FROM PUBLIC, ",
            id(runtime_role)
        ),
    )
    .await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "definer refusal matrix shares an isolated database"
)]
async fn definer_function_guards_check_owner_search_path_and_execute_grants() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    let platform = runtime_pool(&database, &owner, &password).await;
    let function = ddl!("", id(&schema), ".owner_reader()");
    let bypass = format!("rls_definer_bypass_{}", Uuid::now_v7().simple());
    let inherited = format!("rls_definer_exec_{}", Uuid::now_v7().simple());
    let set_only = format!("rls_definer_set_{}", Uuid::now_v7().simple());
    execute(
        &admin,
        ddl!("CREATE ROLE ", id(&bypass), " NOSUPERUSER BYPASSRLS"),
    )
    .await;
    install_definer(&admin, &schema, &owner, &runtime_role).await;
    assert_guards_accept(&runtime, &platform, &schema).await;

    for unsafe_owner in [
        SqlText::new("CURRENT_USER"),
        ddl!("", id(&bypass)),
        ddl!("", id(&runtime_role)),
    ] {
        execute(
            &admin,
            ddl!(
                "ALTER FUNCTION ",
                text(&function),
                " OWNER TO ",
                text(&unsafe_owner)
            ),
        )
        .await;
        assert_guards_refuse_named(
            &runtime,
            &platform,
            &schema,
            "owner_reader",
            "platform role",
        )
        .await;
        execute(
            &admin,
            ddl!("ALTER FUNCTION ", text(&function), " OWNER TO ", id(&owner)),
        )
        .await;
    }

    execute(
        &admin,
        ddl!("ALTER FUNCTION ", text(&function), " RESET search_path"),
    )
    .await;
    assert_guards_refuse_named(&runtime, &platform, &schema, "owner_reader", "search_path").await;
    let writable = format!("writable_{}", Uuid::now_v7().simple());
    execute(
        &admin,
        ddl!(
            "CREATE SCHEMA ",
            id(&writable),
            " AUTHORIZATION ",
            id(&owner)
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "GRANT CREATE ON SCHEMA ",
            id(&writable),
            " TO ",
            id(&runtime_role)
        ),
    )
    .await;
    for unsafe_path in [
        SqlText::new("pg_temp"),
        ddl!("pg_catalog, ", id(&schema)),
        ddl!("pg_temp, pg_catalog, ", id(&schema)),
        ddl!("pg_catalog, ", id(&writable), ", pg_temp"),
    ] {
        execute(
            &admin,
            ddl!(
                "ALTER FUNCTION ",
                text(&function),
                " SET search_path = ",
                text(&unsafe_path)
            ),
        )
        .await;
        assert_guards_refuse_named(&runtime, &platform, &schema, "owner_reader", "search_path")
            .await;
    }
    execute(
        &admin,
        ddl!(
            "ALTER FUNCTION ",
            text(&function),
            " SET search_path = pg_catalog, ",
            id(&schema),
            ", pg_temp"
        ),
    )
    .await;
    assert_guards_accept(&runtime, &platform, &schema).await;

    for grantee in [SqlText::new("PUBLIC"), ddl!("", id(&runtime_role))] {
        execute(
            &admin,
            ddl!(
                "GRANT EXECUTE ON FUNCTION ",
                text(&function),
                " TO ",
                text(&grantee)
            ),
        )
        .await;
        assert_guards_refuse_named(&runtime, &platform, &schema, "owner_reader", "EXECUTE").await;
        execute(
            &admin,
            ddl!(
                "REVOKE EXECUTE ON FUNCTION ",
                text(&function),
                " FROM ",
                text(&grantee)
            ),
        )
        .await;
    }
    for (grantee, inherit) in [(&inherited, true), (&set_only, false)] {
        execute(
            &admin,
            ddl!("CREATE ROLE ", id(grantee), " NOSUPERUSER NOBYPASSRLS"),
        )
        .await;
        let inherit_clause = if inherit { "TRUE" } else { "FALSE" };
        execute(
            &admin,
            ddl!(
                "GRANT ",
                id(grantee),
                " TO ",
                id(&runtime_role),
                " WITH INHERIT ",
                lit(inherit_clause),
                ", SET TRUE"
            ),
        )
        .await;
        execute(
            &admin,
            ddl!(
                "GRANT EXECUTE ON FUNCTION ",
                text(&function),
                " TO ",
                id(grantee)
            ),
        )
        .await;
        let inherited_execute: bool =
            sqlx::query_scalar("SELECT has_function_privilege($1, $2, 'EXECUTE')")
                .bind(&runtime_role)
                .bind(format!("{schema}.owner_reader()"))
                .fetch_one(&admin)
                .await
                .unwrap();
        assert_eq!(inherited_execute, inherit);
        assert_guards_refuse_named(&runtime, &platform, &schema, "owner_reader", "EXECUTE").await;
        execute(
            &admin,
            ddl!(
                "REVOKE EXECUTE ON FUNCTION ",
                text(&function),
                " FROM ",
                id(grantee)
            ),
        )
        .await;
        execute(
            &admin,
            ddl!("REVOKE ", id(grantee), " FROM ", id(&runtime_role)),
        )
        .await;
    }
    assert_guards_accept(&runtime, &platform, &schema).await;
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
    let control = PgPool::connect(&proxiima_pg_testkit::admin_url())
        .await
        .unwrap();
    for role in [&bypass, &inherited, &set_only] {
        execute(&control, ddl!("DROP ROLE ", id(role))).await;
    }
    control.close().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "definer matrix shares an isolated database"
)]
async fn trigger_definer_executes_without_a_direct_grant_and_exempts_only_execute_acl() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    let platform = runtime_pool(&database, &owner, &password).await;
    let function = ddl!("", id(&schema), ".owner_bridge()");
    execute(
        &admin,
        ddl!(
            "CREATE FUNCTION ",
            text(&function),
            " RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, ",
            id(&schema),
            ", pg_temp AS $$ BEGIN RETURN NEW; END $$"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!("ALTER FUNCTION ", text(&function), " OWNER TO ", id(&owner)),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "CREATE TRIGGER owner_bridge BEFORE INSERT ON ",
            id(&schema),
            ".sidecar FOR EACH ROW EXECUTE FUNCTION ",
            text(&function)
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "REVOKE ALL ON FUNCTION ",
            text(&function),
            " FROM PUBLIC, ",
            id(&runtime_role)
        ),
    )
    .await;
    let runtime_execute: bool =
        sqlx::query_scalar("SELECT has_function_privilege($1, $2, 'EXECUTE')")
            .bind(&runtime_role)
            .bind(format!("{schema}.owner_bridge()"))
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(!runtime_execute);
    assert_guards_accept(&runtime, &platform, &schema).await;
    let row_owner = Uuid::now_v7();
    let verifier = SyntheticVerifier {
        roles: OwnerRoles::for_subject(
            UserId::new(Uuid::now_v7()),
            [(OwnerId::from(GroupId::new(row_owner)), Role::editor())],
        )
        .unwrap(),
    };
    let authz = proxiima_core::authenticate(&verifier, &Credentials::Bearer("trigger-test".into()))
        .await
        .unwrap();
    let mut transaction = begin_owner_transaction(&runtime, authz.owner_scope().unwrap())
        .await
        .unwrap();
    // Raw INSERT: no write path for the sidecar table reachable from test fixtures; the test creates it
    sqlx::query(ddl!(
        "INSERT INTO ",
        id(&schema),
        ".sidecar(id, owner_id) VALUES ($1, $1)"
    ))
    .bind(row_owner)
    .execute(&mut *transaction)
    .await
    .expect("a trigger remains callable after direct EXECUTE is revoked");
    transaction.commit().await.unwrap();
    execute(
        &admin,
        ddl!(
            "GRANT EXECUTE ON FUNCTION ",
            text(&function),
            " TO PUBLIC, ",
            id(&runtime_role)
        ),
    )
    .await;
    assert_guards_accept(&runtime, &platform, &schema).await;
    execute(
        &admin,
        ddl!("ALTER FUNCTION ", text(&function), " RESET search_path"),
    )
    .await;
    assert_guards_refuse_named(&runtime, &platform, &schema, "owner_bridge", "search_path").await;
    execute(
        &admin,
        ddl!(
            "ALTER FUNCTION ",
            text(&function),
            " SET search_path = pg_catalog, ",
            id(&schema),
            ", pg_temp"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "ALTER FUNCTION ",
            text(&function),
            " OWNER TO ",
            id(&runtime_role)
        ),
    )
    .await;
    assert_guards_refuse_named(
        &runtime,
        &platform,
        &schema,
        "owner_bridge",
        "platform role",
    )
    .await;
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "definer matrix shares an isolated database"
)]
async fn definer_search_path_refuses_set_only_access_to_a_trusted_schema_owner() {
    let (database, admin, runtime, owner, runtime_role, schema, password) = setup().await;
    let platform = runtime_pool(&database, &owner, &password).await;
    let suffix = Uuid::now_v7().simple().to_string();
    let schema_owner = format!("rls_namespace_owner_{suffix}");
    let external_schema = format!("rls_external_{suffix}");
    let function = ddl!("", id(&schema), ".path_bridge()");
    execute(
        &admin,
        ddl!(
            "CREATE ROLE ",
            id(&schema_owner),
            " NOSUPERUSER NOBYPASSRLS"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "GRANT ",
            id(&schema_owner),
            " TO ",
            id(&owner),
            " WITH INHERIT TRUE, SET TRUE"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "CREATE SCHEMA ",
            id(&external_schema),
            " AUTHORIZATION ",
            id(&schema_owner)
        ),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "CREATE FUNCTION ",
            text(&function),
            " RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, ",
            id(&external_schema),
            ", pg_temp AS $$ BEGIN RETURN NEW; END $$"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!("ALTER FUNCTION ", text(&function), " OWNER TO ", id(&owner)),
    )
    .await;
    execute(
        &admin,
        ddl!(
            "REVOKE ALL ON FUNCTION ",
            text(&function),
            " FROM PUBLIC, ",
            id(&runtime_role)
        ),
    )
    .await;
    assert_guards_accept(&runtime, &platform, &schema).await;
    execute(
        &admin,
        ddl!(
            "GRANT ",
            id(&schema_owner),
            " TO ",
            id(&runtime_role),
            " WITH INHERIT FALSE, SET TRUE"
        ),
    )
    .await;
    let (can_create, can_set_owner): (bool, bool) =
        sqlx::query_as("SELECT has_schema_privilege($1, $2, 'CREATE'), pg_has_role($1, $3, 'SET')")
            .bind(&runtime_role)
            .bind(&external_schema)
            .bind(&schema_owner)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(!can_create, "SET-only membership must not inherit CREATE");
    assert!(
        can_set_owner,
        "the runtime can assume the schema owner's CREATE authority"
    );
    assert_guards_refuse_named(
        &runtime,
        &platform,
        &schema,
        "path_bridge",
        "runtime-writable search_path",
    )
    .await;
    runtime.close().await;
    platform.close().await;
    cleanup(&database, admin, &owner, &runtime_role).await;
    let control = PgPool::connect(&proxiima_pg_testkit::admin_url())
        .await
        .unwrap();
    execute(&control, ddl!("DROP ROLE ", id(&schema_owner))).await;
    control.close().await;
}

#[tokio::test]
async fn runtime_guard_refuses_a_writable_core_view_until_the_grants_are_restricted() {
    let (database, admin, runtime, platform, platform_role, runtime_role, _password) =
        super::setup_full_schema().await;
    let schemas = ["proxiima_core", "proxiima_example"];
    assert_runtime_rls(&runtime, &schemas).await.unwrap();

    // One grant names its view; a blanket grant makes every core view writable,
    // and the guard names one of them.
    for (grant, view) in [
        (
            "GRANT INSERT ON proxiima_core.memory_version TO",
            "core view proxiima_core.memory_version",
        ),
        (
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA proxiima_core TO",
            "core view proxiima_core.",
        ),
    ] {
        execute(&admin, ddl!("", lit(grant), " ", id(&runtime_role))).await;
        let refused = assert_runtime_rls(&runtime, &schemas)
            .await
            .expect_err("a core view the runtime role may write must refuse boot")
            .to_string();
        assert!(refused.contains(view), "{refused}");
        assert!(refused.contains("restrict_view_grants"), "{refused}");

        execute(&admin, "SELECT proxiima_core.restrict_view_grants()").await;
        assert_runtime_rls(&runtime, &schemas).await.unwrap();
    }
    platform.close().await;
    runtime.close().await;
    cleanup(&database, admin, &platform_role, &runtime_role).await;
}

#[tokio::test]
async fn runtime_guard_accepts_a_writable_view_outside_the_core_schema() {
    let (database, admin, runtime, platform, platform_role, runtime_role, _password) =
        super::setup_full_schema().await;
    execute(
        &admin,
        "CREATE VIEW proxiima_example.writable_probe WITH (security_invoker = true) \
         AS SELECT owner_id FROM proxiima_core.memory",
    )
    .await;
    execute(
        &admin,
        ddl!(
            "GRANT ALL ON proxiima_example.writable_probe TO ",
            id(&runtime_role)
        ),
    )
    .await;
    execute(&admin, "SELECT proxiima_core.restrict_view_grants()").await;
    assert_runtime_rls(&runtime, &["proxiima_core", "proxiima_example"])
        .await
        .unwrap();
    platform.close().await;
    runtime.close().await;
    cleanup(&database, admin, &platform_role, &runtime_role).await;
}

/// (grantee, privilege) rows of the ACL of `view`, PUBLIC spelled out, sorted.
async fn acl(admin: &PgPool, view: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT COALESCE(r.rolname::text, 'PUBLIC'), a.privilege_type
           FROM pg_class AS c
           CROSS JOIN LATERAL aclexplode(c.relacl) AS a
           LEFT JOIN pg_roles AS r ON r.oid = a.grantee
          WHERE c.oid = $1::regclass
          ORDER BY 1, 2",
    )
    .bind(view)
    .fetch_all(admin)
    .await
    .unwrap()
}

#[tokio::test]
async fn restricting_view_grants_leaves_select_for_everyone_and_the_owner_untouched() {
    let (database, admin, runtime, platform, platform_role, runtime_role, _password) =
        super::setup_full_schema().await;
    let view = "proxiima_core.restrict_probe";
    execute(
        &admin,
        "CREATE VIEW proxiima_core.restrict_probe AS SELECT 1 AS one",
    )
    .await;
    for grant in [
        ddl!(
            "GRANT ALL ON ",
            lit(view),
            " TO PUBLIC, ",
            id(&runtime_role)
        ),
        ddl!("GRANT INSERT ON ", lit(view), " TO ", id(&platform_role)),
    ] {
        execute(&admin, grant).await;
    }
    let owner: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&admin)
        .await
        .unwrap();
    let before = acl(&admin, view).await;
    let owner_rows = |rows: &[(String, String)]| -> Vec<(String, String)> {
        rows.iter().filter(|row| row.0 == owner).cloned().collect()
    };
    assert!(!owner_rows(&before).is_empty());

    execute(&admin, "SELECT proxiima_core.restrict_view_grants()").await;
    let after = acl(&admin, view).await;
    let others: Vec<_> = after.iter().filter(|row| row.0 != owner).cloned().collect();
    let mut expected = vec![
        ("PUBLIC".to_owned(), "SELECT".to_owned()),
        (runtime_role.clone(), "SELECT".to_owned()),
    ];
    expected.sort();
    assert_eq!(
        others, expected,
        "SELECT only, and the INSERT-only grantee ends with nothing"
    );
    assert_eq!(owner_rows(&after), owner_rows(&before));

    execute(&admin, "SELECT proxiima_core.restrict_view_grants()").await;
    assert_eq!(
        acl(&admin, view).await,
        after,
        "running it twice changes nothing"
    );
    platform.close().await;
    runtime.close().await;
    cleanup(&database, admin, &platform_role, &runtime_role).await;
}

#[tokio::test]
async fn runtime_guard_refuses_a_column_write_grant_on_a_core_view_until_the_grants_are_restricted()
{
    let (database, admin, runtime, platform, platform_role, runtime_role, _password) =
        super::setup_full_schema().await;
    let schemas = ["proxiima_core", "proxiima_example"];
    let view = "proxiima_core.column_grant_probe";
    execute(
        &admin,
        "CREATE VIEW proxiima_core.column_grant_probe WITH (security_invoker = true) \
         AS SELECT t, schema_id FROM proxiima_core.memory",
    )
    .await;
    execute(
        &admin,
        ddl!("GRANT SELECT ON ", lit(view), " TO ", id(&runtime_role)),
    )
    .await;
    assert_runtime_rls(&runtime, &schemas).await.unwrap();

    for (privilege, grant) in [
        ("UPDATE", "GRANT UPDATE (schema_id) ON"),
        ("INSERT", "GRANT INSERT (schema_id) ON"),
    ] {
        execute(
            &admin,
            ddl!("", lit(grant), " ", lit(view), " TO ", id(&runtime_role)),
        )
        .await;
        let refused = assert_runtime_rls(&runtime, &schemas)
            .await
            .expect_err("a column write grant on a core view must refuse boot")
            .to_string();
        assert!(refused.contains(view), "{refused}");
        assert!(refused.contains("restrict_view_grants"), "{refused}");

        execute(&admin, "SELECT proxiima_core.restrict_view_grants()").await;
        assert_runtime_rls(&runtime, &schemas).await.unwrap();
        let (may_write, may_select): (bool, bool) = sqlx::query_as(
            "SELECT has_any_column_privilege($1, $2::regclass, $3),
                    has_table_privilege($1, $2::regclass, 'SELECT')",
        )
        .bind(&runtime_role)
        .bind(view)
        .bind(privilege)
        .fetch_one(&admin)
        .await
        .unwrap();
        assert!(!may_write, "{privilege} column grant must be gone");
        assert!(may_select, "the SELECT grant must stay");
    }
    platform.close().await;
    runtime.close().await;
    cleanup(&database, admin, &platform_role, &runtime_role).await;
}

#[tokio::test]
async fn restricting_view_grants_takes_a_passed_on_grant_with_it_when_run_by_the_view_owner() {
    let (database, admin, runtime, platform, platform_role, runtime_role, _password) =
        super::setup_full_schema().await;
    let view = "proxiima_core.passed_on_probe";
    let middle = format!("rls_middle_{}", Uuid::now_v7().simple());

    execute(
        &admin,
        ddl!(
            "GRANT CREATE ON SCHEMA proxiima_core TO ",
            id(&platform_role)
        ),
    )
    .await;
    execute(
        &platform,
        "CREATE VIEW proxiima_core.passed_on_probe WITH (security_invoker = true) \
         AS SELECT t, schema_id FROM proxiima_core.memory",
    )
    .await;
    execute(&admin, ddl!("CREATE ROLE ", id(&middle), " NOLOGIN")).await;
    execute(
        &platform,
        ddl!("GRANT SELECT ON ", lit(view), " TO ", id(&runtime_role)),
    )
    .await;
    execute(
        &platform,
        ddl!(
            "GRANT UPDATE ON ",
            lit(view),
            " TO ",
            id(&middle),
            " WITH GRANT OPTION"
        ),
    )
    .await;
    execute(
        &admin,
        ddl!("GRANT USAGE ON SCHEMA proxiima_core TO ", id(&middle)),
    )
    .await;
    let mut connection = admin.acquire().await.unwrap();
    for statement in [
        ddl!("SET ROLE ", id(&middle)),
        ddl!("GRANT UPDATE ON ", lit(view), " TO ", id(&runtime_role)),
        SqlText::new("RESET ROLE"),
    ] {
        sqlx::query(statement)
            .execute(&mut *connection)
            .await
            .unwrap();
    }
    drop(connection);
    let may_update = |role: String| {
        let admin = admin.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT has_table_privilege($1, $2::regclass, 'UPDATE')")
                .bind(role)
                .bind(view)
                .fetch_one(&admin)
                .await
                .unwrap()
        }
    };
    assert!(may_update(runtime_role.clone()).await, "fixture: passed on");

    execute(&platform, "SELECT proxiima_core.restrict_view_grants()").await;
    assert!(!may_update(middle.clone()).await);
    assert!(!may_update(runtime_role.clone()).await);
    let may_select: bool =
        sqlx::query_scalar("SELECT has_table_privilege($1, $2::regclass, 'SELECT')")
            .bind(&runtime_role)
            .bind(view)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(may_select, "the SELECT grant must stay");
    platform.close().await;
    runtime.close().await;
    cleanup(&database, admin, &platform_role, &runtime_role).await;
    let control = PgPool::connect(&proxiima_pg_testkit::admin_url())
        .await
        .unwrap();
    execute(&control, ddl!("DROP ROLE ", id(&middle))).await;
    control.close().await;
}
