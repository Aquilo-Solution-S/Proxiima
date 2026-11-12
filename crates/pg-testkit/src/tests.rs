#[test]
fn connection_exhaustion_is_read_from_the_pool_and_server_messages() {
    for message in [
        "runtime connection: Unavailable(\"pool timed out while waiting for an open connection\")",
        "error returned from database: sorry, too many clients already",
        "remaining connection slots are reserved for roles with the SUPERUSER attribute",
        "too many connections for database \"proxiima_test_x\"",
    ] {
        assert!(super::is_connection_exhaustion(message), "{message}");
    }
    for message in [
        "RLS catalog guard refused pool: runtime role proxiima_test_runtime is a superuser",
        "scope not registered",
    ] {
        assert!(!super::is_connection_exhaustion(message), "{message}");
    }
}

use time::OffsetDateTime;

use super::{
    ADMIN_URL_UNSET, ADMIN_URL_VAR, BUILTIN_FAMILIES, MAX_FAMILY_PREFIX_BYTES, RESERVED_PREFIXES,
    SPLIT_ROLE_PASSWORD, TemplateFamily, TemplateFamilyError, connect_admin, create_db, db_url,
    db_url_from_admin, drop_unused_db_on, name_is_stale_clone, redacted_url, role_url_from_admin,
    template_family, trimmed_url, unique_db_name,
};

/// Provisioning twice leaves the fixture roles with one, fixed SCRAM
/// verifier (a re-salted verifier breaks a pooler's cached SCRAM keys),
/// and the fixture password still logs in against it.
#[tokio::test]
async fn split_role_provisioning_keeps_one_scram_verifier() {
    use sqlx::Connection as _;
    if super::admin_url_or_skip().is_none() {
        return;
    }
    let db = unique_db_name("scram_verifier");
    create_db(&db).await.expect("fixture database");
    let verifier = async {
        let mut admin = connect_admin().await.expect("admin");
        sqlx::query_scalar::<_, String>(
            "SELECT rolpassword FROM pg_authid WHERE rolname = 'proxiima_test_runtime'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("runtime role verifier")
    };
    let (runtime_url, _) = super::split_role_urls(&db).await.expect("provisioned");
    let first = verifier.await;
    let verifier = async {
        let mut admin = connect_admin().await.expect("admin");
        sqlx::query_scalar::<_, String>(
            "SELECT rolpassword FROM pg_authid WHERE rolname = 'proxiima_test_runtime'",
        )
        .fetch_one(&mut admin)
        .await
        .expect("runtime role verifier")
    };
    super::split_role_urls(&db)
        .await
        .expect("provisioned again");
    let second = verifier.await;
    assert_eq!(first, super::SPLIT_ROLE_SCRAM_VERIFIER);
    assert_eq!(first, second, "provisioning again does not re-salt");
    sqlx::PgConnection::connect(&runtime_url)
        .await
        .expect("the fixture password matches the verifier")
        .close()
        .await
        .expect("close");
    super::drop_db(&db).await.expect("drop fixture database");
}

#[test]
fn invalid_admin_url_error_contains_no_credentials() {
    let result = db_url_from_admin("postgres://user:private-secret@[broken", "isolated_test");
    let error = result.expect_err("invalid IPv6 address");
    assert!(!error.to_string().contains("private-secret"));
}

#[test]
fn redacted_url_hides_the_password() {
    let parsed =
        url::Url::parse("postgres://user:secret@localhost/isolated_test").expect("test URL");
    let redacted = redacted_url(parsed);
    assert_eq!(redacted.password(), Some("****"));
    assert_eq!(redacted.username(), "user");
    assert!(!redacted.as_str().contains("secret"));
}

#[test]
fn redacted_url_hides_a_password_in_the_query() {
    let parsed = url::Url::parse(
        "postgres://localhost/isolated_test?sslmode=disable&password=private-secret&user=u",
    )
    .expect("test URL");
    let redacted = redacted_url(parsed);
    assert!(!redacted.as_str().contains("private-secret"));
    let pairs: Vec<(String, String)> = redacted
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("sslmode".to_owned(), "disable".to_owned()),
            ("password".to_owned(), "****".to_owned()),
            ("user".to_owned(), "u".to_owned()),
        ],
        "only the password value changes"
    );
}

#[test]
fn a_role_url_drops_the_admin_credentials_wherever_they_are() {
    let admin = "postgres://admin:authority-secret@localhost:5433/postgres\
                     ?sslmode=disable&user=query-admin&password=private-secret&dbname=other";
    let url = role_url_from_admin(admin, "target", "proxiima_test_runtime").expect("role URL");
    assert_eq!(url.username(), "proxiima_test_runtime");
    assert_eq!(url.password(), Some(SPLIT_ROLE_PASSWORD));
    assert_eq!(
        (url.host_str(), url.port()),
        (Some("localhost"), Some(5433))
    );
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("sslmode".to_owned(), "disable".to_owned()),
            ("dbname".to_owned(), "target".to_owned()),
        ],
        "no user or password override survives; the target database does"
    );
    let text = url.to_string();
    // The failure names the credential, never its value or the URL.
    for (credential, value) in [
        ("the admin password", "authority-secret"),
        ("the query password", "private-secret"),
        ("the query user", "query-admin"),
    ] {
        assert!(
            !text.contains(value),
            "{credential} leaked into the role URL"
        );
    }
}

/// The window a lease does not cover is a session that connects after the
/// collector looked: `DROP DATABASE` without `FORCE` refuses it, and the
/// refusal is a skip.
#[tokio::test]
async fn dropping_a_database_somebody_is_connected_to_is_a_skip() {
    use sqlx::Connection;

    let name = unique_db_name("tfbusy_unit");
    create_db(&name).await.expect("create");
    let session = sqlx::PgConnection::connect(&db_url(&name))
        .await
        .expect("a session on the database");
    let mut admin = connect_admin().await.expect("admin");
    let busy = drop_unused_db_on(&mut admin, &name).await;
    session.close().await.expect("close the session");
    let idle = drop_unused_db_on(&mut admin, &name).await;
    admin.close().await.expect("close admin");

    assert!(!busy.expect("a busy drop is not an error"), "kept");
    assert!(idle.expect("an idle drop"), "dropped once nobody is on it");
}

/// A role without statistics privileges reads no backend type of another
/// role's session; the session still occupies its database.
#[tokio::test]
async fn a_session_whose_backend_type_is_hidden_occupies_its_database() {
    use sqlx::Connection;

    let name = unique_db_name("tfhidden_unit");
    create_db(&name).await.expect("create");
    let session = sqlx::PgConnection::connect(&db_url(&name))
        .await
        .expect("a session on the database");
    let mut observer = connect_admin().await.expect("admin");
    sqlx::raw_sql("SET ROLE proxiima_search")
        .execute(&mut observer)
        .await
        .expect("look as a role that reads no statistics");
    let hidden: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
              WHERE datname = $1 AND backend_type IS NULL)",
    )
    .bind(&name)
    .fetch_one(&mut observer)
    .await
    .expect("read activity");
    let in_use: bool = sqlx::query_scalar(concat!("SELECT ", database_in_use!("$1")))
        .bind(&name)
        .fetch_one(&mut observer)
        .await
        .expect("ask whether somebody is connected");
    observer.close().await.expect("close the observer");
    session.close().await.expect("close the session");
    super::drop_db(&name).await.expect("clean up");

    assert!(hidden, "the observer must not read the session's type");
    assert!(in_use, "a backend of unknown type is somebody");
}

/// A database that was created and cannot be recorded (here: it accepts no
/// connection, so the grant on it fails) is dropped again.
#[tokio::test]
async fn a_created_database_that_cannot_be_recorded_is_dropped() {
    use sqlx::Connection;

    let name = unique_db_name("record_fails");
    let mut admin = connect_admin().await.expect("admin");
    let mut create = super::SqlText::new("CREATE DATABASE ");
    create
        .push_ident(&super::ident(&name).expect("name"))
        .push(" ALLOW_CONNECTIONS false");
    sqlx::raw_sql(create)
        .execute(&mut admin)
        .await
        .expect("create");

    let recorded = super::record_new_db(&mut admin, &name).await;
    let left = super::database_exists_on(&mut admin, &name)
        .await
        .expect("exists");
    admin.close().await.expect("close admin");

    recorded.expect_err("a database that accepts no connection cannot be recorded");
    assert!(!left, "the unrecorded database was dropped");
}

#[tokio::test]
async fn candidate_ownership_and_execution_survive_split_role_provisioning() {
    use sqlx::Connection;

    let Some(_) = super::admin_url_or_skip() else {
        return;
    };
    let name = unique_db_name("proxiima_search_role");
    create_db(&name).await.expect("create");
    let guard = super::DbGuard::adopt(name);
    let (runtime_url, _) = super::split_role_urls(guard.name()).await.expect("roles");
    let mut admin = sqlx::PgConnection::connect(&db_url(guard.name()))
        .await
        .expect("admin");
    sqlx::raw_sql(
        "CREATE SCHEMA proxiima_core;
             GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
             SET ROLE proxiima_search;
             CREATE FUNCTION proxiima_core.candidate_probe() RETURNS integer
               LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog AS 'SELECT 7';
             REVOKE ALL ON FUNCTION proxiima_core.candidate_probe() FROM PUBLIC;
             RESET ROLE;
             REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search",
    )
    .execute(&mut admin)
    .await
    .expect("candidate creation");
    let default_execute: bool = sqlx::query_scalar(
        "SELECT has_function_privilege($1, 'proxiima_core.candidate_probe()', 'EXECUTE')",
    )
    .bind(super::SPLIT_RUNTIME_ROLE)
    .fetch_one(&mut admin)
    .await
    .expect("default execution grant");
    assert!(default_execute);
    sqlx::raw_sql(
        "REVOKE EXECUTE ON FUNCTION proxiima_core.candidate_probe() FROM proxiima_test_runtime;
             DROP TABLE _proxiima_test.split_roles_ready",
    )
    .execute(&mut admin)
    .await
    .expect("replay preparation");
    super::split_role_urls(guard.name()).await.expect("replay");
    let owner: String = sqlx::query_scalar(
        "SELECT owner_role.rolname FROM pg_proc p
               JOIN pg_roles owner_role ON owner_role.oid = p.proowner
              WHERE p.oid = 'proxiima_core.candidate_probe()'::regprocedure",
    )
    .fetch_one(&mut admin)
    .await
    .expect("owner");
    assert_eq!(owner, "proxiima_search");
    let attributes: (bool, bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT rolcanlogin, rolsuper, rolbypassrls, rolcreatedb, rolcreaterole, rolinherit
               FROM pg_roles WHERE rolname = 'proxiima_search'",
    )
    .fetch_one(&mut admin)
    .await
    .expect("search attributes");
    assert_eq!(attributes, (false, false, false, false, false, false));
    let membership: (bool, bool, bool) = sqlx::query_as(
        "SELECT m.admin_option, m.inherit_option, m.set_option
               FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid
               JOIN pg_roles member_role ON member_role.oid = m.member
              WHERE r.rolname = 'proxiima_search' AND member_role.rolname = $1",
    )
    .bind(super::SPLIT_PLATFORM_ROLE)
    .fetch_one(&mut admin)
    .await
    .expect("platform membership");
    assert_eq!(membership, (false, false, true));
    let runtime_member: bool =
        sqlx::query_scalar("SELECT pg_has_role($1, 'proxiima_search', 'MEMBER')")
            .bind(super::SPLIT_RUNTIME_ROLE)
            .fetch_one(&mut admin)
            .await
            .expect("runtime membership");
    assert!(!runtime_member);
    let mut runtime = sqlx::PgConnection::connect(&runtime_url)
        .await
        .expect("runtime");
    let result: i32 = sqlx::query_scalar("SELECT proxiima_core.candidate_probe()")
        .fetch_one(&mut runtime)
        .await
        .expect("explicit execution grant");
    assert_eq!(result, 7);
    runtime.close().await.expect("close runtime");
    admin.close().await.expect("close admin");
}

#[test]
fn a_role_url_from_an_unparsable_admin_url_is_an_error_without_credentials() {
    let error = role_url_from_admin("postgres://u:private-secret@[broken", "t", "r")
        .expect_err("invalid IPv6 address");
    assert!(!error.to_string().contains("private-secret"));
}

#[test]
fn template_family_only_core_and_split() {
    assert_eq!(
        template_family("proxiima_tmpl_core_3f94e256ef4f396f"),
        Some("proxiima_tmpl_core_")
    );
    assert_eq!(
        template_family("proxiima_tmpl_other_2d01c98992e2f0a0"),
        None
    );
    assert_eq!(
        template_family("proxiima_tmpl_split_9a41c0d2e6b37f58"),
        Some("proxiima_tmpl_split_")
    );
    assert_eq!(template_family("proxiima_tmpl_build_abc"), None);
    assert_eq!(template_family("proxiima_test_abc"), None);
}

#[test]
fn an_unset_empty_or_blank_admin_url_is_unconfigured() {
    assert_eq!(trimmed_url(None), None);
    assert_eq!(trimmed_url(Some(String::new())), None);
    assert_eq!(trimmed_url(Some(" \t\n ".into())), None);
    assert_eq!(
        trimmed_url(Some("  postgres://u:p@h/db \n".into())).as_deref(),
        Some("postgres://u:p@h/db")
    );
}

#[test]
fn the_unconfigured_message_names_the_variable_and_carries_no_url() {
    assert!(ADMIN_URL_UNSET.contains(ADMIN_URL_VAR));
    assert!(ADMIN_URL_UNSET.contains("no default"));
}

fn refusal(prefix: &str) -> TemplateFamilyError {
    TemplateFamily::new(prefix).expect_err("prefix must be refused")
}

#[test]
fn a_family_prefix_is_validated_at_construction() {
    for accepted in ["host_", "a_", "my_host2_tmpl_", "proxiima_split_core_"] {
        let family = TemplateFamily::new(accepted).expect(accepted);
        assert_eq!(family.prefix(), accepted);
    }
    let longest = format!("{}_", "a".repeat(MAX_FAMILY_PREFIX_BYTES - 1));
    assert_eq!(longest.len(), 47);
    TemplateFamily::new(&longest).expect("47 bytes fit beside 16 digits in 63");

    assert_eq!(refusal(""), TemplateFamilyError::Empty);
    assert_eq!(
        refusal(&format!("a{longest}")),
        TemplateFamilyError::TooLong { len: 48 }
    );
    assert_eq!(
        refusal("Host_"),
        TemplateFamilyError::InvalidCharacter { found: 'H' }
    );
    assert_eq!(
        refusal("ho-st_"),
        TemplateFamilyError::InvalidCharacter { found: '-' }
    );
    assert_eq!(
        refusal("h\u{00f6}st_"),
        TemplateFamilyError::InvalidCharacter { found: '\u{00f6}' }
    );
    assert_eq!(refusal("1host_"), TemplateFamilyError::MustStartWithLetter);
    assert_eq!(refusal("_host_"), TemplateFamilyError::MustStartWithLetter);
    assert_eq!(refusal("host"), TemplateFamilyError::MustEndWithUnderscore);
    assert_eq!(
        refusal("host_x"),
        TemplateFamilyError::MustEndWithUnderscore
    );
}

#[test]
fn a_family_prefix_never_overlaps_a_reserved_one() {
    for reserved in RESERVED_PREFIXES {
        // Every `_`-terminated prefix of the reserved one (itself last),
        // and an extension of it: equal, a prefix of, prefixed by.
        let extended = format!("{reserved}host_");
        let prefixes = reserved
            .match_indices('_')
            .map(|(at, _)| &reserved[..=at])
            .chain([extended.as_str()]);
        for prefix in prefixes {
            assert!(
                matches!(
                    TemplateFamily::new(prefix),
                    Err(TemplateFamilyError::ReservedOverlap { .. })
                ),
                "{prefix:?} overlaps {reserved:?}"
            );
        }
    }
    // Near misses are other people's families.
    for prefix in [
        "proxiima_tmpl_cor_",
        "proxiima_tmplx_",
        "proxiima_tmpl_coree_",
    ] {
        TemplateFamily::new(prefix).expect(prefix);
    }
}

#[test]
fn a_family_owns_its_own_templates_and_no_database_it_could_confuse_them_with() {
    let family = TemplateFamily::new("host_").expect("family");
    let nested = TemplateFamily::new("host_tmpl_").expect("family");
    let own = family.template_name(0xabc);
    assert_eq!(own, "host_0000000000000abc");
    assert!(family.owns(&own));
    assert!(family.owns(&family.template_name(u64::MAX)));
    // A family whose prefix extends this one is another family, even
    // though `host_%` matches its templates.
    assert!(!family.owns(&nested.template_name(1)));
    assert!(nested.owns(&nested.template_name(1)));
    for other in [
        "host_",
        "host_0000000000000ABC",
        "host_000000000000abc",
        "host_00000000000000abc",
        "host_0000000000000abg",
        "other_0000000000000abc",
        "host_00000000-0000-7000-8000-000000000000",
    ] {
        assert!(!family.owns(other), "{other:?}");
    }
    for builtin in BUILTIN_FAMILIES {
        assert!(!family.owns(&format!("{builtin}0000000000000abc")));
    }
}

#[test]
fn a_template_name_never_parses_as_a_clone_name() {
    // The sweep drops any `proxiima_*` database whose last segment is a
    // UUID v7 older than its grace. A template's last segment is 16 hex
    // digits, whatever the hash.
    let far_future = OffsetDateTime::now_utc() + time::Duration::days(3650);
    let hashes = [0, 1, 0xdead_beef_dead_beef, u64::MAX, 0x0192_8f5a_0000_0000];
    let host_families = ["host_", "proxiima_split_core_", "a_b_"]
        .map(|prefix| TemplateFamily::new(prefix).expect("family"));
    let names = host_families
        .iter()
        .flat_map(|family| hashes.map(|hash| family.template_name(hash)))
        .chain(
            BUILTIN_FAMILIES
                .into_iter()
                .flat_map(|prefix| hashes.map(move |hash| format!("{prefix}{hash:016x}"))),
        );
    for name in names {
        assert!(!name_is_stale_clone(&name, far_future), "{name}");
        assert!(name.len() <= 63, "{name}");
    }
}
