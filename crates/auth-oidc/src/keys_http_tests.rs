use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use axum::{Router, body::Body, http::StatusCode, response::Response, routing::get};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use tokio::sync::Barrier;

use crate::config::{OidcConfigError, validate_https_url};

use super::{
    DEFAULT_HTTP_REQUEST_TIMEOUT, FetchedKeys, HttpJwksResolver, JWKS_MAX_AGE, JWKS_TRUST_BOUND,
    JwksState, KeyError, KeyResolver, MAX_HTTP_REQUEST_TIMEOUT, MAX_RESPONSE_BYTES,
    build_http_client,
};

// Static 2048-bit RSA public key as JWK n/e (base64url). Baked so this
// test needs neither `rsa` nor `rand` (RUSTSEC-2023-0071: the `rsa` crate
// ships an unfixed Marvin timing sidechannel). The test only serves the
// public JWK from a mock IdP and resolves it; nothing signs here.
pub(super) const TEST_JWK_N: &str = "vcvNMtDvpJExXOyytyqUOWhX2sxa-Xtxd4KmfJ05-iPgT_RiyZzx3UoTuJYtvDCCRcXKU13Rn8cIc0ushWlKpLDW08U4r9bBVctcajpnOumCcuIvnM1_HEiM-WuYPRFk0I5h--ueLA0KhIfPs0ORLpqsvF0XIuL6_uZtObrH9wxPMmG4r5Hh7h3Gm5PchY0R8H7VrEOm79fnra7OGg5nh7XkmStnZnwozODW0FFnpW-kMeCK2-2fzmSWg1A_clFdicji1-xIvk7Wog9CVsZZK9iRHgAIxmsU-Iawb_Wwlwuu-_gIZWFkund24iA2qLktFx_39CORZqfFRNiIsHSvIQ";
pub(super) const TEST_JWK_E: &str = "AQAB";

fn test_key() -> Arc<jsonwebtoken::DecodingKey> {
    Arc::new(
        jsonwebtoken::DecodingKey::from_rsa_components(TEST_JWK_N, TEST_JWK_E)
            .expect("valid baked test key"),
    )
}

#[test]
fn default_request_timeout_is_finite_and_explicit() {
    assert_eq!(DEFAULT_HTTP_REQUEST_TIMEOUT, Duration::from_secs(10));
    let resolver = HttpJwksResolver::new("http://127.0.0.1:4180".into(), None)
        .expect("loopback issuer and default timeout are valid");

    assert_eq!(resolver.request_timeout(), DEFAULT_HTTP_REQUEST_TIMEOUT);
    assert!(!resolver.request_timeout().is_zero());
    assert!(resolver.request_timeout() <= MAX_HTTP_REQUEST_TIMEOUT);
}

#[test]
fn explicit_request_timeout_rejects_zero_and_out_of_range_values() {
    for invalid in [
        Duration::ZERO,
        MAX_HTTP_REQUEST_TIMEOUT + Duration::from_nanos(1),
    ] {
        assert!(matches!(
            HttpJwksResolver::with_request_timeout("http://127.0.0.1:4180".into(), None, invalid),
            Err(OidcConfigError::InvalidTimeout { .. })
        ));
    }
}

#[test]
fn constructors_reject_issuer_query_or_fragment_components() {
    let http = reqwest::Client::new();
    for base in [
        "https://issuer.example/tenant",
        "http://127.0.0.1:4180/tenant",
    ] {
        for suffix in ["?region=eu", "?", "#anchor", "#"] {
            let issuer = format!("{base}{suffix}");
            for jwks_uri in [None, Some("https://issuer.example/keys?version=1".into())] {
                for result in [
                    HttpJwksResolver::new(issuer.clone(), jwks_uri.clone()),
                    HttpJwksResolver::with_request_timeout(
                        issuer.clone(),
                        jwks_uri.clone(),
                        DEFAULT_HTTP_REQUEST_TIMEOUT,
                    ),
                    HttpJwksResolver::with_http_client(
                        issuer.clone(),
                        jwks_uri.clone(),
                        http.clone(),
                        DEFAULT_HTTP_REQUEST_TIMEOUT,
                    ),
                ] {
                    assert!(
                        matches!(
                            result,
                            Err(OidcConfigError::InvalidUrl {
                                field: "issuer",
                                ..
                            })
                        ),
                        "issuer component must be rejected: {issuer}"
                    );
                }
            }
        }
    }
}

#[test]
fn constructors_preserve_path_issuers_and_jwks_queries() {
    for issuer in [
        "https://issuer.example/tenant/",
        "https://issuer.example/tenant%3Fblue%23anchor",
        "http://127.0.0.1:4180/tenant",
    ] {
        let jwks_uri = "https://issuer.example/keys?version=1";
        let resolver = HttpJwksResolver::new(issuer.into(), Some(jwks_uri.into()))
            .expect("path issuer and JWKS query allowed");

        assert_eq!(resolver.issuer, issuer, "issuer identity is not normalized");
        assert_eq!(resolver.jwks_uri.as_deref(), Some(jwks_uri));
    }
}

async fn stalled_body() -> Response {
    let body = Body::from_stream(futures::stream::pending::<Result<&'static [u8], Infallible>>());
    Response::builder()
        .header("content-type", "application/json")
        .body(body)
        .expect("static stalled response")
}

async fn spawn_stalling_idp() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let issuer = format!("http://{addr}");
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(stalled_body))
        .route("/keys", get(stalled_body));
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock idp server failed");
    });
    (issuer, server)
}

#[tokio::test]
async fn stalled_discovery_and_jwks_bodies_time_out() {
    let (issuer, server) = spawn_stalling_idp().await;
    let request_timeout = Duration::from_millis(50);

    for jwks_uri in [None, Some(format!("{issuer}/keys"))] {
        let resolver =
            HttpJwksResolver::with_request_timeout(issuer.clone(), jwks_uri, request_timeout)
                .expect("short explicit timeout is valid");
        let result = tokio::time::timeout(Duration::from_secs(2), resolver.key_for("k1"))
            .await
            .expect("resolver's request timeout must fire before the test guard");
        assert!(
            matches!(result, Err(KeyError::Parse(_))),
            "stalled response body must terminate through the existing parse-error path: {result:?}"
        );
    }

    server.abort();
}

/// Seeds a set holding `kid`, fetched now with no attempt on record.
/// Tests then look it up at a later moment through `key_at`.
async fn seed_key(
    resolver: &HttpJwksResolver,
    kid: &str,
) -> (Arc<jsonwebtoken::DecodingKey>, Instant) {
    let key = test_key();
    let fetched_at = Instant::now();
    *resolver.state.write().await = JwksState {
        set: Some(FetchedKeys {
            keys: HashMap::from([(kid.to_owned(), Arc::clone(&key))]),
            fetched_at,
        }),
        attempted_at: None,
    };
    (key, fetched_at)
}

/// A moment at which a set fetched at `fetched_at` is due for a refresh
/// and still trusted.
fn aging(fetched_at: Instant) -> Instant {
    fetched_at + JWKS_MAX_AGE
}

async fn cached_kids(resolver: &HttpJwksResolver) -> Vec<String> {
    let state = resolver.state.read().await;
    let mut kids: Vec<String> = state
        .set
        .iter()
        .flat_map(|set| set.keys.keys().cloned())
        .collect();
    kids.sort();
    kids
}

#[tokio::test]
async fn discovers_and_loads_jwks() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let issuer = format!("http://{addr}");
    let jwks_uri = format!("{issuer}/keys");
    let openid = serde_json::json!({ "issuer": issuer, "jwks_uri": jwks_uri }).to_string();
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();

    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get({
                let openid = openid.clone();
                move || async move { openid.clone() }
            }),
        )
        .route(
            "/keys",
            get({
                let jwks = jwks.clone();
                move || async move { jwks.clone() }
            }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock idp server failed");
    });

    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback http allowed in tests");
    assert!(resolver.key_for("k1").await.is_ok());
    assert!(matches!(
        resolver.key_for("missing").await,
        Err(KeyError::UnknownKid)
    ));

    server.abort();
}

struct DiscoveryServer {
    issuer: String,
    jwks_uri: String,
    discovery_fetches: Arc<AtomicUsize>,
    jwks_fetches: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DiscoveryServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn spawn_discovery_idp(
    metadata_issuer: impl FnOnce(&str) -> Option<serde_json::Value>,
) -> DiscoveryServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener address");
    let issuer = format!("http://{addr}/tenant/");
    let jwks_uri = format!("http://{addr}/keys?version=1");
    let mut metadata = serde_json::json!({ "jwks_uri": jwks_uri });
    if let Some(value) = metadata_issuer(&issuer) {
        metadata["issuer"] = value;
    }
    let metadata = metadata.to_string();
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let discovery_fetches = Arc::new(AtomicUsize::new(0));
    let jwks_fetches = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/tenant/.well-known/openid-configuration",
            get({
                let fetches = Arc::clone(&discovery_fetches);
                move || {
                    let fetches = Arc::clone(&fetches);
                    let metadata = metadata.clone();
                    async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        metadata
                    }
                }
            }),
        )
        .route(
            "/keys",
            get({
                let fetches = Arc::clone(&jwks_fetches);
                move |query: axum::extract::RawQuery| {
                    let fetches = Arc::clone(&fetches);
                    let jwks = jwks.clone();
                    async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(query.0.as_deref(), Some("version=1"));
                        jwks
                    }
                }
            }),
        );
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock discovery server");
    });
    DiscoveryServer {
        issuer,
        jwks_uri,
        discovery_fetches,
        jwks_fetches,
        task,
    }
}

#[tokio::test]
async fn discovery_issuer_must_match_exactly_before_jwks_fetch() {
    let mismatches: [fn(&str) -> String; 3] = [
        |_| "http://another.example".into(),
        |issuer| issuer.trim_end_matches('/').into(),
        |issuer| issuer.replace("/tenant/", "/TENANT/"),
    ];
    for mismatch in mismatches {
        let server = spawn_discovery_idp(|issuer| Some(serde_json::json!(mismatch(issuer)))).await;
        let resolver = HttpJwksResolver::new(server.issuer.clone(), None).expect("issuer");

        assert!(matches!(
            resolver.key_for("k1").await,
            Err(KeyError::Config(_))
        ));
        assert_eq!(server.discovery_fetches.load(Ordering::SeqCst), 1);
        assert_eq!(server.jwks_fetches.load(Ordering::SeqCst), 0);
        assert!(cached_kids(&resolver).await.is_empty());
    }
}

#[tokio::test]
async fn discovery_issuer_is_a_required_string() {
    for issuer in [
        None,
        Some(serde_json::Value::Null),
        Some(serde_json::json!(7)),
    ] {
        let server = spawn_discovery_idp(|_| issuer).await;
        let resolver = HttpJwksResolver::new(server.issuer.clone(), None).expect("issuer");

        assert!(matches!(
            resolver.key_for("k1").await,
            Err(KeyError::Parse(_))
        ));
        assert_eq!(server.discovery_fetches.load(Ordering::SeqCst), 1);
        assert_eq!(server.jwks_fetches.load(Ordering::SeqCst), 0);
        assert!(cached_kids(&resolver).await.is_empty());
    }
}

#[tokio::test]
async fn matching_discovery_issuer_preserves_path_and_jwks_query() {
    let server = spawn_discovery_idp(|issuer| Some(serde_json::json!(issuer))).await;
    let resolver = HttpJwksResolver::new(server.issuer.clone(), None).expect("issuer");

    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(server.discovery_fetches.load(Ordering::SeqCst), 1);
    assert_eq!(server.jwks_fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_jwks_endpoint_skips_discovery_metadata() {
    let server = spawn_discovery_idp(|_| Some(serde_json::json!("http://another.example"))).await;
    let resolver = HttpJwksResolver::new(server.issuer.clone(), Some(server.jwks_uri.clone()))
        .expect("explicit JWKS");

    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(server.discovery_fetches.load(Ordering::SeqCst), 0);
    assert_eq!(server.jwks_fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn discovery_issuer_mismatch_preserves_only_previously_cached_keys() {
    let server = spawn_discovery_idp(|_| Some(serde_json::json!("http://another.example"))).await;
    let resolver = HttpJwksResolver::new(server.issuer.clone(), None).expect("issuer");
    let (cached, fetched_at) = seed_key(&resolver, "old").await;
    let now = aging(fetched_at);

    let returned = resolver
        .key_at("old", now)
        .await
        .expect("trusted cached key");

    assert!(Arc::ptr_eq(&returned, &cached));
    assert_eq!(server.discovery_fetches.load(Ordering::SeqCst), 1);
    assert_eq!(server.jwks_fetches.load(Ordering::SeqCst), 0);
    assert!(matches!(
        resolver.key_at("k1", now).await,
        Err(KeyError::UnknownKid)
    ));
    assert_eq!(cached_kids(&resolver).await, ["old"]);
}

#[tokio::test]
async fn plaintext_nonloopback_redirect_is_rejected_before_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let issuer = format!("http://{addr}");
    let destination = format!("http://redirect.example:{}/redirect-target", addr.port());
    let fetches = Arc::new(AtomicUsize::new(0));
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let redirect = move || async move { axum::response::Redirect::temporary(&destination) };
    let intermediate = || async { axum::response::Redirect::temporary("/intermediate") };
    let app = Router::new()
        .route("/keys", get(intermediate))
        .route("/.well-known/openid-configuration", get(intermediate))
        .route("/intermediate", get(redirect))
        .route(
            "/redirect-target",
            get({
                let fetches = Arc::clone(&fetches);
                move || {
                    let fetches = Arc::clone(&fetches);
                    let jwks = jwks.clone();
                    async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        jwks
                    }
                }
            }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock idp server");
    });
    let http = build_http_client(
        &issuer,
        reqwest::Client::builder()
            .no_proxy()
            .resolve("redirect.example", addr),
    );
    for jwks_uri in [None, Some(format!("{issuer}/keys"))] {
        let resolver = HttpJwksResolver::with_http_client(
            issuer.clone(),
            jwks_uri,
            http.clone(),
            DEFAULT_HTTP_REQUEST_TIMEOUT,
        )
        .expect("loopback issuer");

        let result = resolver.key_for("k1").await;

        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        assert!(matches!(result, Err(KeyError::Fetch(_))));
    }
    server.abort();
}

/// Serves `jwks` from a loopback mock `IdP`, counting `/keys` fetches.
/// Returns `(issuer, fetch_counter, server_handle)`.
async fn spawn_mock_idp(jwks: String) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let issuer = format!("http://{addr}");
    let openid = serde_json::json!({
        "issuer": issuer,
        "jwks_uri": format!("{issuer}/keys")
    })
    .to_string();
    let fetches = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { openid.clone() }),
        )
        .route(
            "/keys",
            get({
                let fetches = Arc::clone(&fetches);
                move || {
                    let jwks = jwks.clone();
                    let fetches = Arc::clone(&fetches);
                    async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        jwks
                    }
                }
            }),
        )
        .route(
            "/redirect/keys",
            get(|| async { axum::response::Redirect::temporary("/keys") }),
        )
        .route(
            "/redirect/loop",
            get({
                let fetches = Arc::clone(&fetches);
                move || {
                    let fetches = Arc::clone(&fetches);
                    async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        axum::response::Redirect::temporary("/redirect/loop")
                    }
                }
            }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock idp server failed");
    });
    (issuer, fetches, server)
}

#[tokio::test]
async fn default_client_follows_valid_loopback_redirect() {
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/redirect/keys")))
        .expect("loopback issuer");

    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn default_client_limits_redirect_loops() {
    let (issuer, fetches, server) = spawn_mock_idp(String::new()).await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/redirect/loop")))
        .expect("loopback issuer");

    assert!(matches!(
        resolver.key_for("k1").await,
        Err(KeyError::Fetch(_))
    ));
    assert_eq!(fetches.load(Ordering::SeqCst), 11);
    server.abort();
}

/// Serves a failing explicit JWKS endpoint and counts fetch attempts.
async fn spawn_failing_jwks() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("read listener addr");
    let issuer = format!("http://{addr}");
    let fetches = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route(
        "/keys",
        get({
            let fetches = Arc::clone(&fetches);
            move || {
                let fetches = Arc::clone(&fetches);
                async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock idp server failed");
    });
    (issuer, fetches, server)
}

#[tokio::test]
async fn non_rsa_entry_does_not_break_rsa_resolution() {
    // A mixed set: an EC key and an OKP key (each lacking n/e) alongside
    // the RSA key. The tolerant parse must skip the non-RSA entries and
    // still resolve the RSA kid rather than erroring the whole set.
    let jwks = serde_json::json!({
        "keys": [
            { "kid": "ec1", "kty": "EC", "crv": "P-256", "x": "AQ", "y": "AQ" },
            { "kid": "okp1", "kty": "OKP", "crv": "Ed25519", "x": "AQ" },
            { "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }
        ]
    })
    .to_string();
    let (issuer, _fetches, server) = spawn_mock_idp(jwks).await;

    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback http allowed in tests");
    assert!(resolver.key_for("k1").await.is_ok());
    assert!(matches!(
        resolver.key_for("ec1").await,
        Err(KeyError::UnknownKid)
    ));

    server.abort();
}

#[tokio::test]
async fn well_formed_ec_and_okp_entries_are_materialized_by_family() {
    use jsonwebtoken::AlgorithmFamily;

    use super::fixtures::{ec_key, okp_key, without};

    let jwks = serde_json::json!({
        "keys": [
            { "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E },
            ec_key("ec"),
            okp_key("okp"),
            without(without(ec_key("ec-bare"), "alg"), "use"),
            without(without(okp_key("okp-bare"), "alg"), "use"),
        ]
    })
    .to_string();
    let (issuer, _fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

    for (kid, family) in [
        ("k1", AlgorithmFamily::Rsa),
        ("ec", AlgorithmFamily::Ec),
        ("okp", AlgorithmFamily::Ed),
        ("ec-bare", AlgorithmFamily::Ec),
        ("okp-bare", AlgorithmFamily::Ed),
    ] {
        let key = resolver.key_for(kid).await.expect("resolves");
        assert_eq!(key.family(), family, "{kid}");
    }
    server.abort();
}

/// One fault in an EC or OKP entry skips that entry and nothing else: the
/// good keys beside it still resolve.
#[tokio::test]
async fn faulty_ec_and_okp_entries_are_skipped_and_the_rest_resolve() {
    use super::fixtures::{ec_key, faulty_ec_and_okp_entries, okp_key};

    let mut keys = vec![
        serde_json::json!({ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }),
        ec_key("ec"),
        okp_key("okp"),
    ];
    let faulty = faulty_ec_and_okp_entries();
    assert!(!faulty.is_empty());
    keys.extend(faulty);
    let jwks = serde_json::json!({ "keys": keys }).to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

    for kid in ["k1", "ec", "okp"] {
        assert!(resolver.key_for(kid).await.is_ok(), "{kid}");
    }
    let cached = cached_kids(&resolver).await;
    assert_eq!(cached, ["ec", "k1", "okp"], "only the well-formed keys");
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn a_set_whose_only_ec_and_okp_entries_are_faulty_is_still_an_error() {
    use super::fixtures::faulty_ec_and_okp_entries;

    let jwks = serde_json::json!({ "keys": faulty_ec_and_okp_entries() }).to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

    assert!(matches!(
        resolver.key_for("ec-p384").await,
        Err(KeyError::Parse(_))
    ));
    assert!(cached_kids(&resolver).await.is_empty());
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

/// A wrong-typed member sinks only its own entry: the RSA key beside it
/// still resolves, whichever member is mistyped.
#[tokio::test]
async fn a_mistyped_entry_does_not_fail_the_set() {
    use super::fixtures::mistyped_entries;

    let mut keys = vec![serde_json::json!({
        "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E
    })];
    keys.extend(mistyped_entries());
    let jwks = serde_json::json!({ "keys": keys }).to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(cached_kids(&resolver).await.len(), 1, "only the RSA key");
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

/// An RSA entry whose modulus does not decode is skipped like any other
/// unusable entry; the keys beside it still resolve.
#[tokio::test]
async fn a_fetched_rsa_entry_with_an_undecodable_modulus_is_skipped() {
    let jwks = serde_json::json!({
        "keys": [
            { "kid": "bad", "kty": "RSA", "n": "!!not base64url!!", "e": TEST_JWK_E },
            { "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }
        ]
    })
    .to_string();
    let (issuer, _fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(cached_kids(&resolver).await.len(), 1, "only the good key");
    server.abort();
}

#[tokio::test]
async fn a_response_body_past_the_byte_cap_is_refused() {
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }],
        "padding": "a".repeat(MAX_RESPONSE_BYTES),
    })
    .to_string();
    let (issuer, _fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

    assert!(matches!(
        resolver.key_for("k1").await,
        Err(KeyError::Fetch(_))
    ));
    server.abort();
}

/// RSA entries with incompatible signing metadata are skipped, while a
/// valid RSA key beside them still resolves.
#[tokio::test]
async fn fetched_rsa_entries_require_signing_metadata() {
    let signing = RsaKeyPair::generate(KeySize::Rsa2048).expect("generate signing key");
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(signing.public_key());
    let n = URL_SAFE_NO_PAD.encode(public.n);
    let e = URL_SAFE_NO_PAD.encode(public.e);
    let jwks = serde_json::json!({
        "keys": [
            { "kid": "k1", "kty": "RSA", "n": n, "e": e,
              "use": "enc" },
            { "kid": "k2", "kty": "RSA", "n": n, "e": e,
              "alg": "ES256" },
            { "kid": "good", "kty": "RSA", "n": n, "e": e,
              "alg": "RS256", "use": "sig" },
        ]
    })
    .to_string();
    let (issuer, _fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer.clone(), None).expect("loopback issuer");

    let validator = crate::authenticator::OidcTokenValidator::new(
        crate::config::OidcAuthConfig {
            issuer: issuer.clone(),
            jwks_uri: None,
            audience: "proxiima-api".into(),
            allowed_subjects: None,
            leeway_secs: 0,
        },
        Arc::new(resolver),
    )
    .expect("validator");
    let mut verified = Vec::new();
    for kid in ["k1", "k2", "good"] {
        let header = serde_json::json!({"alg": "RS256", "kid": kid});
        let claims = serde_json::json!({
            "sub": "subject-1", "iss": issuer, "aud": "proxiima-api", "exp": 4_102_444_800_u64
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string()),
        );
        let mut signature = vec![0; signing.public_modulus_len()];
        signing
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .expect("sign token");
        let token = format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature));
        verified.push(validator.validate(&token).await.is_ok());
    }
    assert_eq!(
        verified,
        [false, false, true],
        "enc, ES256, valid RSA sibling"
    );
    server.abort();
}

/// The document itself must still be `{ "keys": [..] }`.
#[tokio::test]
async fn a_document_that_is_not_a_jwks_is_a_parse_error() {
    for body in ["[]", r#"{"keys":{}}"#, r#"{"keys":"k1"}"#, "{}", "not json"] {
        let (issuer, _fetches, server) = spawn_mock_idp(body.to_owned()).await;
        let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");

        assert!(
            matches!(resolver.key_for("k1").await, Err(KeyError::Parse(_))),
            "{body}"
        );
        server.abort();
    }
}

#[tokio::test]
async fn jwks_entries_without_kid_do_not_break_named_rsa_resolution() {
    // Public example keys from RFC 7517 section 3 and RFC 8037 A.2.
    // RFC 7517 section 4.5 makes kid optional; these unsupported keys
    // must not prevent the named RSA signing key from being loaded.
    let unnamed_keys = [
        serde_json::json!({
            "kty": "EC", "crv": "P-256",
            "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
            "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"
        }),
        serde_json::json!({
            "kty": "OKP", "crv": "Ed25519",
            "x": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
        }),
        serde_json::json!({ "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }),
    ];
    for unnamed in unnamed_keys {
        let jwks = serde_json::json!({
            "keys": [unnamed, {
                "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E
            }]
        })
        .to_string();
        let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
        let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");
        let named = resolver.key_for("k1").await;
        let absent = resolver.key_for("").await;
        server.abort();

        assert!(named.is_ok(), "named RSA key must load: {:?}", named.err());
        assert!(matches!(absent, Err(KeyError::UnknownKid)));
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(cached_kids(&resolver).await.len(), 1);
    }
}

#[tokio::test]
async fn jwks_without_addressable_rsa_keys_is_rejected() {
    for keys in [
        serde_json::json!([]),
        serde_json::json!([{ "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]),
    ] {
        let (issuer, fetches, server) =
            spawn_mock_idp(serde_json::json!({ "keys": keys }).to_string()).await;
        let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");
        let result = resolver.key_for("k1").await;
        let absent = resolver.key_for("").await;
        server.abort();

        assert!(matches!(result, Err(KeyError::Parse(_))));
        assert!(matches!(absent, Err(KeyError::UnknownKid)));
        assert!(cached_kids(&resolver).await.is_empty());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rsa_jwk_without_kty_is_materialized() {
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let (issuer, _fetches, server) = spawn_mock_idp(jwks).await;

    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback http allowed in tests");
    assert!(resolver.key_for("k1").await.is_ok());

    server.abort();
}

#[tokio::test]
async fn stale_cache_past_max_age_refetches_on_hit() {
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;

    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback http allowed in tests");
    // Prime the cache: one fetch, then a plain hit does NOT refetch.
    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    assert!(resolver.key_for("k1").await.is_ok());
    assert_eq!(fetches.load(Ordering::SeqCst), 1);

    // A hit once the set is past JWKS_MAX_AGE triggers exactly one refetch
    // and still resolves; the set is fresh again afterwards.
    let later = aging(Instant::now());
    assert!(resolver.key_at("k1", later).await.is_ok());
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    assert!(resolver.key_at("k1", later).await.is_ok());
    assert_eq!(fetches.load(Ordering::SeqCst), 2);

    server.abort();
}

#[tokio::test]
async fn stale_cache_uses_cached_key_when_refresh_fails() {
    let (issuer, fetches, server) = spawn_failing_jwks().await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
        .expect("loopback http allowed in tests");
    let (cached, fetched_at) = seed_key(&resolver, "k1").await;

    let returned_key = resolver
        .key_at("k1", aging(fetched_at))
        .await
        .expect("an aging cached key must survive an IdP outage");

    assert!(Arc::ptr_eq(&returned_key, &cached));
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn stale_cache_uses_cached_key_when_refresh_has_no_rsa_keys() {
    let jwks = serde_json::json!({
        "keys": [{ "kid": "ec1", "kty": "EC", "crv": "P-256", "x": "AQ", "y": "AQ" }]
    })
    .to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
        .expect("loopback http allowed in tests");
    let (cached, fetched_at) = seed_key(&resolver, "k1").await;

    let returned_key = resolver
        .key_at("k1", aging(fetched_at))
        .await
        .expect("an aging cached key must survive an empty RSA refresh");

    assert!(Arc::ptr_eq(&returned_key, &cached));
    assert_eq!(cached_kids(&resolver).await, ["k1"]);
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn stale_cache_uses_cached_key_when_refresh_is_throttled() {
    let jwks = serde_json::json!({ "keys": [] }).to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
        .expect("loopback http allowed in tests");
    let (cached, fetched_at) = seed_key(&resolver, "k1").await;
    let now = aging(fetched_at);
    resolver.state.write().await.attempted_at = Some(now);

    let returned_key = resolver
        .key_at("k1", now)
        .await
        .expect("throttling must not discard an aging cached key");

    assert!(Arc::ptr_eq(&returned_key, &cached));
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    server.abort();
}

/// Past the trust bound a cached key resolves only through a successful
/// fetch: a failed one refuses it, and so does the cooldown after it.
#[tokio::test]
async fn a_set_past_its_trust_bound_refuses_its_keys_until_a_fetch_succeeds() {
    let (issuer, fetches, server) = spawn_failing_jwks().await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
        .expect("loopback http allowed in tests");
    let (cached, fetched_at) = seed_key(&resolver, "k1").await;

    let returned_key = resolver
        .key_at("k1", fetched_at + JWKS_MAX_AGE * 3)
        .await
        .expect("inside the bound a failed refresh serves the cached key");
    assert!(Arc::ptr_eq(&returned_key, &cached));
    assert_eq!(fetches.load(Ordering::SeqCst), 1);

    let past_bound = fetched_at + JWKS_TRUST_BOUND;
    assert!(matches!(
        resolver.key_at("k1", past_bound).await,
        Err(KeyError::Fetch(_))
    ));
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    assert!(matches!(
        resolver.key_at("k1", past_bound).await,
        Err(KeyError::UnknownKid)
    ));
    assert_eq!(
        fetches.load(Ordering::SeqCst),
        2,
        "the cooldown spaces attempts"
    );
    server.abort();
}

/// The bound holds when the attempt ends, not when the lookup began.
#[tokio::test]
async fn a_failed_fetch_that_crosses_the_trust_bound_refuses_the_key() {
    let (issuer, server) = spawn_stalling_idp().await;
    let request_timeout = Duration::from_millis(50);
    let resolver = HttpJwksResolver::with_request_timeout(
        issuer.clone(),
        Some(format!("{issuer}/keys")),
        request_timeout,
    )
    .expect("short explicit timeout is valid");
    let (_, fetched_at) = seed_key(&resolver, "k1").await;
    let just_inside = JWKS_TRUST_BOUND
        .checked_sub(request_timeout / 2)
        .expect("the bound is longer than the timeout");

    assert!(matches!(
        resolver.key_at("k1", fetched_at + just_inside).await,
        Err(KeyError::Parse(_))
    ));
    server.abort();
}

#[tokio::test]
async fn a_set_past_its_trust_bound_resolves_again_after_a_successful_fetch() {
    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = HttpJwksResolver::new(issuer, None).expect("loopback issuer");
    let (_, fetched_at) = seed_key(&resolver, "k1").await;

    assert!(
        resolver
            .key_at("k1", fetched_at + JWKS_TRUST_BOUND)
            .await
            .is_ok()
    );
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

/// While one request refreshes an aging set, the others resolve from the
/// cache without waiting for it.
#[tokio::test]
async fn hits_on_an_aging_set_do_not_wait_for_a_refresh_in_flight() {
    let (issuer, server) = spawn_stalling_idp().await;
    let resolver = Arc::new(
        HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
            .expect("loopback http allowed in tests"),
    );
    let (cached, fetched_at) = seed_key(&resolver, "k1").await;
    let now = aging(fetched_at);

    let refreshing = tokio::spawn({
        let resolver = Arc::clone(&resolver);
        async move { resolver.key_at("k1", now).await }
    });
    while resolver.state.read().await.attempted_at.is_none() {
        tokio::task::yield_now().await;
    }

    let returned_key = tokio::time::timeout(Duration::from_secs(2), resolver.key_at("k1", now))
        .await
        .expect("a hit must not wait for the refresh in flight")
        .expect("the aging key is still trusted");
    assert!(Arc::ptr_eq(&returned_key, &cached));
    assert!(!refreshing.is_finished(), "the refresh is still in flight");

    refreshing.abort();
    server.abort();
}

#[tokio::test]
async fn cache_miss_returns_fetch_error_when_refresh_fails() {
    let (issuer, fetches, server) = spawn_failing_jwks().await;
    let resolver = HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
        .expect("loopback http allowed in tests");

    assert!(matches!(
        resolver.key_for("missing").await,
        Err(KeyError::Fetch(_))
    ));
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn concurrent_cache_misses_share_one_refresh_attempt() {
    const CALLERS: usize = 16;

    let jwks = serde_json::json!({
        "keys": [{ "kid": "k1", "kty": "RSA", "n": TEST_JWK_N, "e": TEST_JWK_E }]
    })
    .to_string();
    let (issuer, fetches, server) = spawn_mock_idp(jwks).await;
    let resolver = Arc::new(
        HttpJwksResolver::new(issuer.clone(), Some(format!("{issuer}/keys")))
            .expect("loopback http allowed in tests"),
    );
    let barrier = Arc::new(Barrier::new(CALLERS + 1));
    let mut tasks = Vec::with_capacity(CALLERS);
    for _ in 0..CALLERS {
        let resolver = Arc::clone(&resolver);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            resolver.key_for("missing").await
        }));
    }

    barrier.wait().await;
    for task in tasks {
        assert!(matches!(
            task.await.expect("key lookup task completed"),
            Err(KeyError::UnknownKid)
        ));
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    server.abort();
}

#[test]
fn rejects_discovered_http_jwks_uri() {
    assert!(matches!(
        validate_https_url("discovered jwks_uri", "http://issuer.example/keys"),
        Err(OidcConfigError::InsecureUrl {
            field: "discovered jwks_uri",
            ..
        })
    ));
}
