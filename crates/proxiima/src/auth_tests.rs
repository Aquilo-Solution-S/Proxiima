use std::collections::HashMap;
use std::sync::Arc;

use super::{
    allowed_subjects, oidc_from_lookup, oidc_http_timeout, oidc_require_access_token_typ,
    public_url,
};
use crate::ProxiimaError;

/// A lookup over a fixed map, so none of this touches the process
/// environment — which is what lets these run in parallel with every
/// other test in the crate.
fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    move |key: &str| map.get(key).cloned()
}

/// Lazy, so it never opens a connection — but constructing a lazy
/// sqlx pool still requires a Tokio context, which is why these are
/// `#[tokio::test]` and not `#[test]`.
fn owner_access() -> Arc<dyn crate::OwnerAccessPort> {
    let pool = sqlx::PgPool::connect_lazy("postgres://invalid/proxiima")
        .expect("test pool URL is syntactically valid");
    Arc::new(proxiima_storage_pg::PgOwnerAccessResolver::new(
        pool,
        proxiima_storage_pg::PgPlatformScope::for_tests(),
    ))
}

/// No issuer is not a misconfiguration. An embedded host using the Host
/// API serves no MCP and needs no issuer, and must still boot.
#[tokio::test]
async fn no_issuer_means_no_authenticator_and_no_error() {
    let resolved = oidc_from_lookup(&env(&[]), owner_access()).expect("absence is not an error");
    assert!(resolved.is_none());
}

/// The runtime environment layer snapshots `OIDC_ENV_KEYS` and replays
/// them at resolve; a key read here but missing there would silently
/// resolve as unset.
#[tokio::test]
async fn every_variable_read_is_captured_by_the_runtime_env_layer() {
    let asked = std::sync::Mutex::new(Vec::new());
    let full = env(&[
        ("PROXIIMA_OIDC_ISSUER", "https://issuer.test"),
        ("PROXIIMA_OIDC_AUDIENCE", "proxiima"),
        ("PROXIIMA_PUBLIC_URL", "https://mcp.test"),
        ("PROXIIMA_OIDC_JWKS_URI", "https://issuer.test/jwks"),
        ("PROXIIMA_OIDC_ALLOWED_SUBJECTS", "a,b"),
        ("PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS", "5"),
        (
            "PROXIIMA_OIDC_SUBJECT_MAP_JSON",
            r#"[{"iss":"https://issuer.test","sub":"a","user_id":"00000000-0000-7000-8000-000000000001"}]"#,
        ),
    ]);
    let recording = |key: &str| {
        asked.lock().unwrap().push(key.to_owned());
        full(key)
    };
    oidc_from_lookup(&recording, owner_access()).expect("valid env");
    for key in asked.into_inner().unwrap() {
        assert!(
            super::OIDC_ENV_KEYS
                .iter()
                .any(|setting| setting.name() == key),
            "{key} is read but not in OIDC_ENV_KEYS"
        );
    }
}

#[test]
fn a_public_url_that_is_not_a_base_url_is_refused_without_echoing_it() {
    for bad in [
        "proxiima.example.com",
        "ftp://proxiima.example.com",
        "https://user:secret@proxiima.example.com",
        "https://proxiima.example.com/?a=1",
        "https://proxiima.example.com/#top",
        "https://",
    ] {
        let Err(ProxiimaError::Config(message)) = public_url(bad) else {
            panic!("{bad} must be refused");
        };
        assert!(message.contains("PROXIIMA_PUBLIC_URL"), "{message}");
        assert!(!message.contains("secret"), "{message}");
    }
    for good in [
        "https://mcp.test",
        "https://mcp.test/",
        "http://127.0.0.1:31415",
        "https://mcp.test/proxiima",
    ] {
        assert_eq!(public_url(good).expect(good), good);
    }
}

#[test]
fn an_allowed_subjects_value_that_names_no_subject_is_refused() {
    let lookup = |value: &'static str| env(&[("PROXIIMA_OIDC_ALLOWED_SUBJECTS", value)]);
    assert_eq!(allowed_subjects(&lookup("")).expect("blank is unset"), None);
    for bad in [",", ", ,"] {
        let Err(ProxiimaError::Config(message)) = allowed_subjects(&lookup(bad)) else {
            panic!("{bad:?} must be refused");
        };
        assert!(
            message.contains("PROXIIMA_OIDC_ALLOWED_SUBJECTS"),
            "{message}"
        );
    }
    let kept = allowed_subjects(&lookup(" a, b ,")).expect("two subjects");
    assert_eq!(
        kept,
        Some(std::collections::HashSet::from([
            "a".to_owned(),
            "b".to_owned()
        ]))
    );
}

#[test]
fn oidc_http_timeout_has_a_finite_default_and_parses_an_override() {
    assert_eq!(
        oidc_http_timeout(&env(&[])).expect("default timeout"),
        proxiima_auth_oidc::DEFAULT_HTTP_REQUEST_TIMEOUT
    );
    assert_eq!(
        oidc_http_timeout(&env(&[("PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS", "17")]))
            .expect("valid override"),
        std::time::Duration::from_secs(17)
    );
}

#[test]
fn access_token_typ_setting_uses_shared_boolean_parser_and_defaults_off() {
    assert!(!oidc_require_access_token_typ(&env(&[])).expect("default off"));
    for value in ["", " \t\n "] {
        assert!(
            !oidc_require_access_token_typ(&env(&[(
                "PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP",
                value,
            )]))
            .expect("blank is unset")
        );
    }
    for value in ["1", "true", "TRUE", "yes", "On", " true \n"] {
        assert!(
            oidc_require_access_token_typ(&env(&[(
                "PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP",
                value,
            )]))
            .expect("true value")
        );
    }
    for value in ["0", "false", "FALSE", "no", "OFF", " off \t"] {
        assert!(
            !oidc_require_access_token_typ(&env(&[(
                "PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP",
                value,
            )]))
            .expect("false value")
        );
    }
    for invalid in ["maybe", "truthy"] {
        assert!(
            oidc_require_access_token_typ(&env(&[(
                "PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP",
                invalid,
            )]))
            .is_err(),
            "{invalid:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn oidc_http_timeout_override_is_wired_and_invalid_values_are_refused() {
    let configured = [
        ("PROXIIMA_OIDC_ISSUER", "https://idp.test"),
        ("PROXIIMA_OIDC_AUDIENCE", "proxiima-mcp"),
        ("PROXIIMA_PUBLIC_URL", "https://mcp.test"),
        (
            "PROXIIMA_OIDC_SUBJECT_MAP_JSON",
            r#"[{"iss":"https://idp.test","sub":"sub","user_id":"0195a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}]"#,
        ),
        ("PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS", "17"),
    ];
    oidc_from_lookup(&env(&configured), owner_access())
        .expect("valid timeout reaches resolver construction")
        .expect("issuer config produces a bundle");

    for invalid in ["0", "301", "not-a-number"] {
        let invalid_config = [
            ("PROXIIMA_OIDC_ISSUER", "https://idp.test"),
            ("PROXIIMA_OIDC_AUDIENCE", "proxiima-mcp"),
            ("PROXIIMA_PUBLIC_URL", "https://mcp.test"),
            (
                "PROXIIMA_OIDC_SUBJECT_MAP_JSON",
                r#"[{"iss":"https://idp.test","sub":"sub","user_id":"0195a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}]"#,
            ),
            ("PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS", invalid),
        ];
        let Err(err) = oidc_from_lookup(&env(&invalid_config), owner_access()) else {
            panic!("configured invalid timeout must fail boot");
        };
        assert!(
            err.to_string()
                .contains("PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS"),
            "message: {err}"
        );
    }
}

/// Once an issuer IS set every companion becomes required. A silent
/// downgrade here would serve MCP with less authentication than the
/// operator asked for, which is the one outcome worth failing to boot
/// over.
#[tokio::test]
async fn an_issuer_without_its_companions_is_refused() {
    for partial in [
        vec![("PROXIIMA_OIDC_ISSUER", "https://idp.test")],
        vec![
            ("PROXIIMA_OIDC_ISSUER", "https://idp.test"),
            ("PROXIIMA_OIDC_AUDIENCE", "proxiima-mcp"),
        ],
        vec![
            ("PROXIIMA_OIDC_ISSUER", "https://idp.test"),
            ("PROXIIMA_OIDC_AUDIENCE", "proxiima-mcp"),
            ("PROXIIMA_PUBLIC_URL", "https://mcp.test"),
        ],
    ] {
        assert!(
            oidc_from_lookup(&env(&partial), owner_access()).is_err(),
            "an issuer with {} companion(s) must not resolve",
            partial.len() - 1
        );
    }
}

/// A plaintext issuer is refused before anything else is parsed, so the
/// error names the insecure URL rather than whatever came after it.
#[tokio::test]
async fn a_plaintext_issuer_is_refused() {
    let insecure = env(&[
        ("PROXIIMA_OIDC_ISSUER", "http://idp.test"),
        ("PROXIIMA_OIDC_AUDIENCE", "proxiima-mcp"),
        ("PROXIIMA_PUBLIC_URL", "https://mcp.test"),
        (
            "PROXIIMA_OIDC_SUBJECT_MAP_JSON",
            r#"[{"iss":"https://idp.test","sub":"sub","user_id":"0195a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}]"#,
        ),
    ]);
    assert!(oidc_from_lookup(&insecure, owner_access()).is_err());
}

/// The spiice-call shape: tokens minted by an issuer the host has no
/// network path to, verified against a key set shipped in its config.
mod pinned_jwks {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair};
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use proxiima_core::{
        AccessError, AuthError, Authenticator, Credentials, OwnerAccessPort, OwnerRoles, UserId,
    };
    use proxiima_mcp_server::AuthorizationServers;
    use uuid::Uuid;

    use super::env;
    use crate::auth::oidc_from_lookup;
    use crate::runtime_config::ProxiimaError;

    const JWKS_JSON: &str = crate::auth::JWKS_JSON.name();
    const JWKS_URI: &str = crate::auth::JWKS_URI.name();

    const KID: &str = "spiice-call-1";
    const FLEET: &str = "spiice-fleet";
    const CALLER: &str = "centauri";

    /// An issuer that never answers. Plaintext loopback passes the
    /// issuer URL policy, and any discovery or JWKS fetch aimed at it
    /// completes its handshake into this listener's backlog, where
    /// [`Self::assert_never_contacted`] finds it.
    struct SilentIssuer {
        listener: TcpListener,
        url: String,
    }

    impl SilentIssuer {
        fn bind() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback issuer");
            listener.set_nonblocking(true).expect("non-blocking accept");
            let url = format!("http://{}", listener.local_addr().expect("bound address"));
            Self { listener, url }
        }

        fn assert_never_contacted(&self) {
            match self.listener.accept() {
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                other => panic!("the static path contacted the issuer: {other:?}"),
            }
        }
    }

    struct OneSubject(UserId);

    #[async_trait]
    impl OwnerAccessPort for OneSubject {
        async fn resolve_roles_for_subject(
            &self,
            subject: UserId,
        ) -> Result<OwnerRoles, AccessError> {
            if subject != self.0 {
                return Err(AccessError::Resolution("unknown subject".into()));
            }
            OwnerRoles::for_subject(subject, [])
        }
    }

    fn signing_key() -> RsaKeyPair {
        RsaKeyPair::generate(KeySize::Rsa2048).expect("generate test RSA key")
    }

    fn jwks(kid: &str, key: &RsaKeyPair) -> String {
        let public = key.public_key();
        serde_json::json!({ "keys": [{
            "kty": "RSA",
            "kid": kid,
            "alg": "RS256",
            "use": "sig",
            "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
            "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
        }]})
        .to_string()
    }

    fn token(key: &RsaKeyPair, kid: &str, issuer: &str) -> Credentials {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs();
        let header = serde_json::json!({ "alg": "RS256", "kid": kid, "typ": "JWT" });
        let claims = serde_json::json!({
            "iss": issuer,
            "aud": FLEET,
            "sub": CALLER,
            "exp": now + 60,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; key.public_modulus_len()];
        key.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .expect("sign jwt");
        Credentials::Bearer(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }

    fn pinned(
        issuer: &str,
        jwks: &str,
        user: Uuid,
    ) -> Result<Option<crate::auth::OidcBundle>, ProxiimaError> {
        let subject_map = format!(r#"[{{"iss":"{issuer}","sub":"{CALLER}","user_id":"{user}"}}]"#);
        oidc_from_lookup(
            &env(&[
                ("PROXIIMA_OIDC_ISSUER", issuer),
                ("PROXIIMA_OIDC_AUDIENCE", FLEET),
                ("PROXIIMA_PUBLIC_URL", "https://spiice.test"),
                ("PROXIIMA_OIDC_SUBJECT_MAP_JSON", &subject_map),
                (JWKS_JSON, jwks),
                // A regression to the HTTP resolver stalls on the silent
                // issuer; make it fail in one second rather than ten.
                ("PROXIIMA_OIDC_HTTP_TIMEOUT_SECONDS", "1"),
            ]),
            Arc::new(OneSubject(UserId::new(user))),
        )
    }

    fn authenticator(issuer: &str, jwks: &str, user: Uuid) -> Arc<dyn Authenticator> {
        let (authenticator, metadata) = pinned(issuer, jwks, user)
            .expect("pinned jwks config")
            .expect("an issuer yields a bundle");
        assert_eq!(
            metadata.authorization_servers,
            AuthorizationServers::Issuers(vec![issuer.to_string()])
        );
        authenticator
    }

    #[tokio::test]
    async fn access_token_typ_setting_reaches_the_authenticator() {
        let issuer = SilentIssuer::bind();
        let key = signing_key();
        let keys = jwks(KID, &key);
        let user = Uuid::now_v7();
        let subject_map = format!(
            r#"[{{"iss":"{}","sub":"{CALLER}","user_id":"{user}"}}]"#,
            issuer.url
        );
        for (setting, accepted) in [("true", false), ("false", true)] {
            let (auth, _) = oidc_from_lookup(
                &env(&[
                    ("PROXIIMA_OIDC_ISSUER", &issuer.url),
                    ("PROXIIMA_OIDC_AUDIENCE", FLEET),
                    ("PROXIIMA_PUBLIC_URL", "https://spiice.test"),
                    ("PROXIIMA_OIDC_SUBJECT_MAP_JSON", &subject_map),
                    (JWKS_JSON, &keys),
                    ("PROXIIMA_OIDC_REQUIRE_ACCESS_TOKEN_TYP", setting),
                ]),
                Arc::new(OneSubject(UserId::new(user))),
            )
            .expect("valid pinned configuration")
            .expect("configured authenticator");
            let credentials = token(&key, KID, &issuer.url);
            assert_eq!(auth.authenticate(&credentials).await.is_ok(), accepted);
        }
        issuer.assert_never_contacted();
    }

    #[tokio::test]
    async fn a_token_signed_by_a_pinned_key_authenticates_without_contacting_the_issuer() {
        let issuer = SilentIssuer::bind();
        let key = signing_key();
        let user = Uuid::now_v7();
        let authenticator = authenticator(&issuer.url, &jwks(KID, &key), user);

        let ctx = authenticator
            .authenticate(&token(&key, KID, &issuer.url))
            .await
            .expect("pinned key verifies its token");

        assert_eq!(ctx.subject(), Some(UserId::new(user)));
        issuer.assert_never_contacted();
    }

    /// An unknown kid is where the HTTP resolver refetches, so it is the
    /// case that proves the pinned set is the whole set.
    #[tokio::test]
    async fn a_token_with_an_unknown_kid_is_refused_without_contacting_the_issuer() {
        let issuer = SilentIssuer::bind();
        let pinned_key = signing_key();
        let other_key = signing_key();
        let user = Uuid::now_v7();
        let authenticator = authenticator(&issuer.url, &jwks(KID, &pinned_key), user);

        for credentials in [
            token(&other_key, "rotated-in", &issuer.url),
            token(&pinned_key, "rotated-in", &issuer.url),
        ] {
            assert_eq!(
                authenticator.authenticate(&credentials).await.err(),
                Some(AuthError::InvalidCredentials)
            );
        }
        issuer.assert_never_contacted();
    }

    /// One source says fetch keys from the issuer, the other says never
    /// contact it; neither is allowed to win silently.
    #[tokio::test]
    async fn both_jwks_sources_at_once_is_refused() {
        let user = Uuid::now_v7();
        let both = env(&[
            ("PROXIIMA_OIDC_ISSUER", "https://centauri.test"),
            ("PROXIIMA_OIDC_AUDIENCE", FLEET),
            ("PROXIIMA_PUBLIC_URL", "https://spiice.test"),
            (
                "PROXIIMA_OIDC_SUBJECT_MAP_JSON",
                &format!(
                    r#"[{{"iss":"https://centauri.test","sub":"{CALLER}","user_id":"{user}"}}]"#
                ),
            ),
            (JWKS_URI, "https://centauri.test/keys"),
            (JWKS_JSON, &jwks(KID, &signing_key())),
        ]);
        let Err(ProxiimaError::Config(message)) =
            oidc_from_lookup(&both, Arc::new(OneSubject(UserId::new(user))))
        else {
            panic!("both JWKS sources must be a config error");
        };
        assert!(
            message.contains(JWKS_JSON) && message.contains(JWKS_URI),
            "message: {message}"
        );
    }

    #[tokio::test]
    async fn a_malformed_pinned_jwks_is_refused_at_boot() {
        let key = signing_key();
        let mut no_kid: serde_json::Value =
            serde_json::from_str(&jwks(KID, &key)).expect("jwks json");
        no_kid["keys"][0]
            .as_object_mut()
            .expect("jwk object")
            .remove("kid");
        let mut ec: serde_json::Value = serde_json::from_str(&jwks(KID, &key)).expect("jwks json");
        ec["keys"][0]["kty"] = "EC".into();

        for (case, raw) in [
            ("malformed JSON", "{\"keys\": [".to_owned()),
            ("no keys", r#"{"keys":[]}"#.to_owned()),
            ("no kid", no_kid.to_string()),
            ("non-RSA key", ec.to_string()),
        ] {
            let Err(ProxiimaError::Config(message)) =
                pinned("https://centauri.test", &raw, Uuid::now_v7())
            else {
                panic!("{case} must be a config error");
            };
            assert!(message.contains(JWKS_JSON), "{case}: {message}");
        }
    }
}
