use super::*;
use proxiima_core::env_from_pairs as env;

fn bucketed<'a>(extra: &'a [(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut pairs = vec![
        ("PROXIIMA_S3_BUCKET", "b"),
        ("PROXIIMA_S3_REGION", "eu-central-1"),
        ("PROXIIMA_S3_ACCESS_KEY_ID", "test-access-key"),
        ("PROXIIMA_S3_SECRET_ACCESS_KEY", "test-secret-key"),
    ];
    pairs.extend_from_slice(extra);
    pairs
}

/// The S3 lane is optional, so an unset bucket is `Ok(None)` — but a
/// bucket without a region is a half-configured lane, not an absent one.
#[test]
fn from_lookup_requires_a_region_alongside_the_bucket() {
    let err =
        S3RuntimeConfig::from_lookup(&|name| (name == "PROXIIMA_S3_BUCKET").then(|| "b".into()))
            .unwrap_err();
    let BlobError::Config(msg) = err else {
        panic!("wrong variant");
    };
    assert!(msg.contains("PROXIIMA_S3_REGION"), "{msg}");
}

#[test]
fn optional_s3_matches_ledger_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json")).unwrap();
    let rows = vectors["Proxiima.ObjectMaintenance.optionalS3"]
        .as_array()
        .unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let host_policy_valid = row["in"][0].as_bool().unwrap();
        let bucket_set = row["in"][1].as_bool().unwrap();
        let companions_valid = row["in"][2].as_bool().unwrap();
        let pairs = bucketed(&[]);
        let provided = env(&pairs);
        let lookup = |key: &str| match key {
            "PROXIIMA_PLAINTEXT_HOSTS" if !host_policy_valid => Some("invalid/host".into()),
            "PROXIIMA_S3_BUCKET" if !bucket_set => Some("  ".into()),
            "PROXIIMA_S3_REGION" if !companions_valid => None,
            _ => provided(key),
        };
        let actual = match S3RuntimeConfig::from_lookup(&lookup) {
            Ok(Some(_)) => "configured",
            Ok(None) => "disabled",
            Err(BlobError::Config(_)) => "refuse",
            Err(other) => panic!("unexpected S3 admission error: {other}"),
        };
        assert_eq!(row["out"], actual, "{row}");
    }
}

#[test]
fn object_store_source_matches_ledger_vectors() {
    let ledger: serde_json::Value =
        serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json")).unwrap();
    let rows = ledger["Proxiima.Identity.objectStoreSource"]
        .as_array()
        .unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let mut settings = vec![("PROXIIMA_S3_ENDPOINT_URL", "HTTPS://Example.COM:443/")];
        if row["in"][0].as_bool().unwrap() {
            settings.push(("PROXIIMA_S3_STORE_ID", "tenant-a.store_1"));
        }
        let config = S3RuntimeConfig::from_lookup(&env(&bucketed(&settings)))
            .expect("valid config")
            .expect("configured");
        let expected = match row["out"].as_str().unwrap() {
            "operator" => "s3:id/tenant-a.store_1",
            "endpointAndBucket" => "s3:https://example.com/b",
            outcome => panic!("unknown object store source {outcome}"),
        };
        assert_eq!(
            config.object_store_id().expect("store id").as_str(),
            expected
        );
    }
}

#[test]
fn from_lookup_refuses_invalid_store_ids() {
    for bad in ["has space", "slash/inside"] {
        let err = S3RuntimeConfig::from_lookup(&env(&bucketed(&[("PROXIIMA_S3_STORE_ID", bad)])))
            .expect_err("an invalid store id is refused");
        let BlobError::Config(msg) = err else {
            panic!("wrong variant");
        };
        assert!(msg.contains("PROXIIMA_S3_STORE_ID"), "{msg}");
    }
}

fn assert_missing_key_is_refused(missing: &str) {
    let pairs = bucketed(&[]);
    let lookup = |key: &str| {
        if key == missing {
            None
        } else {
            env(&pairs)(key)
        }
    };
    let err = S3RuntimeConfig::from_lookup(&lookup).unwrap_err();
    let BlobError::Config(msg) = err else {
        panic!("wrong variant");
    };
    assert!(msg.contains(missing), "{msg}");
}

#[test]
fn from_lookup_requires_access_key_id_alongside_the_bucket() {
    assert_missing_key_is_refused("PROXIIMA_S3_ACCESS_KEY_ID");
}

#[test]
fn from_lookup_requires_secret_access_key_alongside_the_bucket() {
    assert_missing_key_is_refused("PROXIIMA_S3_SECRET_ACCESS_KEY");
}

// The SDK's deprecated Config::credentials_provider() always returns None.
// Observe the configured resolver through its supported identity-cache seam.
#[derive(Debug)]
struct CredentialsObserver(std::sync::Arc<std::sync::Mutex<Option<Credentials>>>);

impl aws_sdk_s3::config::ResolveCachedIdentity for CredentialsObserver {
    fn resolve_cached_identity<'a>(
        &'a self,
        resolver: aws_smithy_runtime_api::client::identity::SharedIdentityResolver,
        runtime_components: &'a aws_smithy_runtime_api::client::runtime_components::RuntimeComponents,
        config_bag: &'a aws_sdk_s3::config::ConfigBag,
    ) -> aws_smithy_runtime_api::client::identity::IdentityFuture<'a> {
        use aws_smithy_runtime_api::client::identity::{IdentityFuture, ResolveIdentity};

        IdentityFuture::new(async move {
            let identity = resolver
                .resolve_identity(runtime_components, config_bag)
                .await?;
            *self.0.lock().unwrap() = identity.data::<Credentials>().cloned();
            Ok(identity)
        })
    }
}

async fn presign_config_probe(config: &S3RuntimeConfig, client: &Client) -> reqwest::Url {
    use aws_sdk_s3::presigning::PresigningConfig;
    use std::time::{Duration, SystemTime};

    let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let client = Client::from_conf(
        client
            .config()
            .to_builder()
            .identity_cache(CredentialsObserver(observed.clone()))
            .build(),
    );
    assert_eq!(client.config().region().unwrap().as_ref(), config.region);
    let request = client
        .get_object()
        .bucket(&config.bucket)
        .key("config-probe")
        .presigned(
            PresigningConfig::builder()
                .start_time(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
                .expires_in(Duration::from_mins(1))
                .build()
                .unwrap(),
        )
        .await
        .expect("configuration-only presigning succeeds");
    let credentials = observed
        .lock()
        .unwrap()
        .take()
        .expect("configured resolver ran");
    assert_eq!(credentials.access_key_id(), config.key_pair.access_key_id);
    // Not `assert_eq!`: a failure would print the secret.
    let secret_matches = credentials.secret_access_key() == config.key_pair.secret_access_key;
    assert!(secret_matches);
    assert!(credentials.session_token().is_none());
    assert!(credentials.expiry().is_none());
    reqwest::Url::parse(request.uri()).expect("the SDK emits a valid request URI")
}

fn signed_query_value(url: &reqwest::Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .expect("the SDK includes the signing field")
        .1
        .into_owned()
}

#[tokio::test]
async fn client_uses_only_configured_credentials_region_and_endpoint() {
    let config = S3RuntimeConfig::from_lookup(&env(&bucketed(&[
        (
            "PROXIIMA_S3_ENDPOINT_URL",
            "https://objects.internal.example/internal/",
        ),
        ("PROXIIMA_S3_FORCE_PATH_STYLE", "true"),
    ])))
    .unwrap()
    .expect("bucket is set");
    for (client, host, path) in [(
        config.client(),
        "objects.internal.example",
        "/internal/b/config-probe",
    )] {
        let url = presign_config_probe(&config, &client).await;
        assert_eq!(url.host_str(), Some(host));
        assert_eq!(url.path(), path);
        assert_eq!(
            signed_query_value(&url, "X-Amz-Credential"),
            format!(
                "{}/20231114/{}/s3/aws4_request",
                config.key_pair.access_key_id, config.region
            )
        );
        assert!(
            signed_query_value(&url, "X-Amz-SignedHeaders")
                .split(';')
                .any(|header| header == "host")
        );
    }
}

#[tokio::test]
async fn upload_signing_uses_the_configured_internal_endpoint() {
    let config = S3RuntimeConfig::from_lookup(&env(&bucketed(&[
        ("PROXIIMA_S3_ENDPOINT_URL", "http://127.0.0.1:9000"),
        ("PROXIIMA_S3_FORCE_PATH_STYLE", "true"),
    ])))
    .unwrap()
    .expect("bucket is set");
    let internal = presign_config_probe(&config, &config.client()).await;
    assert_eq!(internal.scheme(), "http");
    assert_eq!(internal.host_str(), Some("127.0.0.1"));
    assert_eq!(internal.port(), Some(9000));
    assert_eq!(internal.path(), "/b/config-probe");
}

#[test]
fn from_lookup_rejects_blank_key_variables() {
    for key in ["PROXIIMA_S3_ACCESS_KEY_ID", "PROXIIMA_S3_SECRET_ACCESS_KEY"] {
        let extra = [(key, " \n")];
        let pairs = bucketed(&extra);
        let error = S3RuntimeConfig::from_lookup(&env(&pairs)).unwrap_err();
        assert!(matches!(error, BlobError::Config(message) if message.contains(key)));
    }
}

#[test]
fn config_debug_redacts_the_secret_and_shows_the_access_key_id() {
    let config = S3RuntimeConfig::from_lookup(&env(&bucketed(&[])))
        .unwrap()
        .expect("bucket is set");
    let debug = format!("{config:?}");
    assert!(!config.key_pair.secret_access_key.is_empty());
    assert!(!debug.contains(&config.key_pair.secret_access_key));
    assert!(debug.contains(&config.key_pair.access_key_id));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn surrounding_whitespace_is_trimmed_from_every_field() {
    let cfg = S3RuntimeConfig::from_lookup(&env(&[
        ("PROXIIMA_S3_BUCKET", " b\n"),
        ("PROXIIMA_S3_REGION", " eu-central-1\n"),
        ("PROXIIMA_S3_UPLOAD_TTL_SECONDS", "900\n"),
        ("PROXIIMA_S3_FORCE_PATH_STYLE", " true "),
        ("PROXIIMA_S3_ACCESS_KEY_ID", "test-access-key"),
        ("PROXIIMA_S3_SECRET_ACCESS_KEY", "test-secret-key"),
    ]))
    .unwrap()
    .expect("bucket is set");
    assert_eq!(cfg.bucket, "b");
    assert_eq!(cfg.region, "eu-central-1");
    assert_eq!(cfg.upload_ttl_seconds, 900);
    assert!(cfg.force_path_style);
}

/// An out-of-range TTL is a startup error on every path into the config,
/// not only the process-environment one.
#[test]
fn from_lookup_rejects_a_ttl_no_presigned_url_could_carry() {
    let err =
        S3RuntimeConfig::from_lookup(&env(&bucketed(&[("PROXIIMA_S3_UPLOAD_TTL_SECONDS", "0")])))
            .unwrap_err();
    let BlobError::Config(msg) = err else {
        panic!("wrong variant");
    };
    assert!(msg.contains("greater than zero"), "{msg}");
}

#[test]
fn endpoint_admission_matches_ledger_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json")).unwrap();
    let rows = vectors["Proxiima.ObjectMaintenance.endpointAdmitted"]
        .as_array()
        .unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let (endpoint, hosts) = match row["in"][0].as_str().unwrap() {
            "https" => ("https://s3.example.com", None),
            "loopbackHttp" => ("http://localhost", None),
            "admittedHttp" => ("http://gateway:9090", Some("gateway")),
            "otherHttp" => ("http://s3.example.com", None),
            "invalidOrUnsupported" => ("s3.example.com", None),
            other => panic!("unknown endpoint transport {other}"),
        };
        assert_eq!(row["out"], with_endpoint(endpoint, hosts).is_ok(), "{row}");
    }
}

#[test]
fn non_loopback_http_endpoint_is_rejected() {
    let err = validate_endpoint_url(
        &PROXIIMA_S3_ENDPOINT_URL,
        "http://s3.example.com",
        &PlaintextHosts::none(),
    )
    .unwrap_err();
    let BlobError::Config(msg) = err else {
        panic!("wrong variant");
    };
    assert!(msg.contains("must use https"));
}

#[test]
fn schemeless_endpoint_is_rejected() {
    let err = validate_endpoint_url(
        &PROXIIMA_S3_ENDPOINT_URL,
        "s3.example.com",
        &PlaintextHosts::none(),
    )
    .unwrap_err();
    assert!(matches!(err, BlobError::Config(_)));
}

fn with_endpoint<'a>(
    endpoint: &'a str,
    hosts: Option<&'a str>,
) -> Result<Option<S3RuntimeConfig>, BlobError> {
    let mut extra = vec![("PROXIIMA_S3_ENDPOINT_URL", endpoint)];
    if let Some(hosts) = hosts {
        extra.push(("PROXIIMA_PLAINTEXT_HOSTS", hosts));
    }
    let pairs = bucketed(&extra);
    S3RuntimeConfig::from_lookup(&env(&pairs))
}

#[test]
fn a_named_host_may_be_the_plaintext_s3_endpoint() {
    let config = with_endpoint("http://gateway:9090", Some("gateway"))
        .unwrap()
        .expect("bucket is set");
    assert_eq!(config.endpoint_url.as_deref(), Some("http://gateway:9090"));
    assert!(config.plaintext_hosts.contains("gateway"));
}

#[test]
fn an_unnamed_host_stays_refused_as_the_s3_endpoint() {
    for (endpoint, hosts) in [
        ("http://gateway:9090", None),
        ("http://gateway:9090", Some("   ")),
        ("http://other:9090", Some("gateway")),
    ] {
        let error = with_endpoint(endpoint, hosts).unwrap_err();
        assert!(
            matches!(&error, BlobError::Config(message) if message.contains("must use https")),
            "{endpoint} with {hosts:?}: {error}",
        );
    }
}

#[test]
fn a_wildcard_or_range_in_the_setting_fails_boot_even_without_a_bucket() {
    for hosts in ["*", "*.internal", "10.0.0.0/8", "10.0.0.1"] {
        let error = with_endpoint("https://s3.example.com", Some(hosts)).unwrap_err();
        assert!(
            matches!(&error, BlobError::Config(message) if message.contains("PROXIIMA_PLAINTEXT_HOSTS")),
            "{hosts}: {error}",
        );
        let no_bucket = [("PROXIIMA_PLAINTEXT_HOSTS", hosts)];
        assert!(
            S3RuntimeConfig::from_lookup(&env(&no_bucket)).is_err(),
            "{hosts}"
        );
    }
}

#[test]
fn unset_setting_leaves_s3_config_as_before() {
    let config = S3RuntimeConfig::from_lookup(&env(&bucketed(&[])))
        .unwrap()
        .expect("bucket is set");
    assert!(config.plaintext_hosts.is_empty());
    assert!(S3RuntimeConfig::from_lookup(&env(&[])).unwrap().is_none());
    assert!(with_endpoint("http://127.0.0.1:9000", None).is_ok());
    let BlobError::Config(message) = with_endpoint("http://s3.example.com", None).unwrap_err()
    else {
        panic!("wrong variant");
    };
    assert_eq!(
        message,
        "PROXIIMA_S3_ENDPOINT_URL must use https (plaintext http allowed only for loopback): http://s3.example.com"
    );
}
