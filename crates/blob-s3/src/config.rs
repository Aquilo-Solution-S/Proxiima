use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use proxiima_core::{
    EndpointUrlError, EndpointUrlPolicy, PlaintextHosts, Setting, process_env,
    validate_endpoint_url_for_hosts,
};

use crate::BlobError;
use crate::settings::{
    PROXIIMA_S3_ACCESS_KEY_ID, PROXIIMA_S3_BUCKET, PROXIIMA_S3_ENDPOINT_URL,
    PROXIIMA_S3_FORCE_PATH_STYLE, PROXIIMA_S3_MAX_BLOB_BYTES, PROXIIMA_S3_REGION,
    PROXIIMA_S3_SECRET_ACCESS_KEY, PROXIIMA_S3_STORE_ID, PROXIIMA_S3_UPLOAD_TTL_SECONDS,
};

pub(crate) const DEFAULT_UPLOAD_TTL_SECONDS: u64 = 900;
pub(crate) const DEFAULT_MAX_BLOB_BYTES: u64 = 100 * 1024 * 1024;

/// `SigV4` ceiling on a presigned URL (7 days). Bound here: `prepare_upload`
/// computes `now + Duration::seconds(ttl)` before
/// `PresigningConfig::expires_in`, and that add panics on overflow.
pub(crate) const MAX_PRESIGN_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Reject a TTL no presigned URL could carry, naming the variable.
pub(crate) fn validate_presign_ttl(key: &Setting, seconds: u64) -> Result<(), BlobError> {
    if seconds == 0 {
        return Err(BlobError::Config(format!(
            "{key} must be greater than zero"
        )));
    }
    if seconds > MAX_PRESIGN_TTL_SECONDS {
        return Err(BlobError::Config(format!(
            "{key}={seconds} exceeds the maximum presigned-URL lifetime of \
             {MAX_PRESIGN_TTL_SECONDS} seconds (7 days)"
        )));
    }
    Ok(())
}

/// Static S3 key pair; the secret is private and redacted by `Debug`.
///
/// ```
/// proxiima_blob_s3::S3KeyPair::new(String::new(), String::new());
/// ```
///
/// ```compile_fail
/// proxiima_blob_s3::S3KeyPair::new(String::new(), String::new()).secret_access_key;
/// ```
#[derive(Clone)]
pub struct S3KeyPair {
    access_key_id: String,
    secret_access_key: String,
}

impl S3KeyPair {
    /// Construct a static key pair supplied by a host.
    #[must_use]
    pub fn new(access_key_id: String, secret_access_key: String) -> Self {
        Self {
            access_key_id,
            secret_access_key,
        }
    }
}

impl std::fmt::Debug for S3KeyPair {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("S3KeyPair")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[REDACTED]")
            .finish()
    }
}

/// Runtime S3 target and TTL settings for cited-blob uploads.
#[derive(Debug, Clone)]
pub struct S3RuntimeConfig {
    pub bucket: String,
    pub region: String,
    pub key_pair: S3KeyPair,
    /// Endpoint for internal object I/O and for presigned upload links.
    /// `None` uses AWS endpoint resolution.
    pub endpoint_url: Option<String>,
    /// Hosts, besides loopback, that may be named in the endpoint over
    /// plaintext `http://`. Empty by default ([`PlaintextHosts::none`]).
    /// `from_lookup` reads it from `PROXIIMA_PLAINTEXT_HOSTS`.
    pub plaintext_hosts: PlaintextHosts,
    pub force_path_style: bool,
    pub upload_ttl_seconds: u64,
    /// Hard cap on a single blob's byte length. `None` selects the 100 MiB
    /// default when the store is constructed. Enforced at prepare time (declared
    /// `byte_len`) and again while streaming the uploaded object at completion,
    /// so an under-declared object cannot bypass the cap.
    pub max_blob_bytes: Option<u64>,
    /// Operator-chosen identity of this object store (`PROXIIMA_S3_STORE_ID`,
    /// 1 to 64 of `[A-Za-z0-9._-]`). `None` derives it from `endpoint_url`
    /// and `bucket` ([`proxiima_core::ObjectStoreId::derive_s3`]). Upload rows
    /// and purge debt record it, and a store drains only its own debt, so
    /// set it when processes sharing one store reach it through different
    /// endpoint URLs, or when several stores share one endpoint and bucket.
    pub store_id: Option<String>,
}

impl S3RuntimeConfig {
    /// Read S3 runtime configuration from the process environment.
    ///
    /// Thin adapter over [`Self::from_lookup`]: this crate holds the single
    /// `PROXIIMA_S3_*` parser, so an embedding host that injects its own
    /// environment cannot end up with different answers than a host that uses
    /// the process environment.
    ///
    /// # Errors
    /// Returns `BlobError::Config` when a required variable is missing
    /// or a typed setting cannot be parsed.
    pub fn from_env() -> Result<Self, BlobError> {
        Self::from_lookup(&process_env)?
            .ok_or_else(|| BlobError::Config("missing PROXIIMA_S3_BUCKET".to_string()))
    }

    /// Read S3 runtime configuration from an injected environment.
    ///
    /// Returns `Ok(None)` when `PROXIIMA_S3_BUCKET` is unset — the S3 lane is
    /// optional, and a host that configures no bucket is not misconfigured.
    /// [`Self::from_env`] turns that into an error because its caller has
    /// already decided S3 is required.
    ///
    /// Every value is read through [`proxiima_core::Setting::read`], so a variable
    /// set to the empty string (or to whitespace) reads as unset rather than
    /// as a value that fails to parse.
    ///
    /// # Errors
    /// Returns `BlobError::Config` when a variable required alongside the
    /// bucket is missing, or a typed setting cannot be parsed.
    pub fn from_lookup(
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, BlobError> {
        // Parsed before the bucket check so a malformed setting fails boot
        // whether or not this lane is configured.
        let plaintext_hosts = PlaintextHosts::from_lookup(lookup)
            .map_err(|error| BlobError::Config(error.to_string()))?;
        let Some(bucket) = PROXIIMA_S3_BUCKET.read(lookup) else {
            return Ok(None);
        };
        let region = PROXIIMA_S3_REGION.read(lookup).ok_or_else(|| {
            BlobError::Config(
                "missing PROXIIMA_S3_REGION (required with PROXIIMA_S3_BUCKET)".to_string(),
            )
        })?;
        let key_pair = S3KeyPair::new(
            required_key(lookup, &PROXIIMA_S3_ACCESS_KEY_ID)?,
            required_key(lookup, &PROXIIMA_S3_SECRET_ACCESS_KEY)?,
        );
        let endpoint_url = PROXIIMA_S3_ENDPOINT_URL.read(lookup);
        if let Some(endpoint_url) = &endpoint_url {
            validate_endpoint_url(&PROXIIMA_S3_ENDPOINT_URL, endpoint_url, &plaintext_hosts)?;
        }
        let store_id = PROXIIMA_S3_STORE_ID.read(lookup);
        if let Some(store_id) = &store_id {
            proxiima_core::ObjectStoreId::operator(store_id)
                .map_err(|error| BlobError::Config(format!("PROXIIMA_S3_STORE_ID: {error}")))?;
        }
        Ok(Some(Self {
            bucket,
            region,
            key_pair,
            endpoint_url,
            plaintext_hosts,
            force_path_style: parse_bool_env(lookup, &PROXIIMA_S3_FORCE_PATH_STYLE)?,
            upload_ttl_seconds: parse_presign_ttl_env(
                lookup,
                &PROXIIMA_S3_UPLOAD_TTL_SECONDS,
                DEFAULT_UPLOAD_TTL_SECONDS,
            )?,
            // Left `None` when unset rather than defaulted here: the field's
            // own contract is that `None` selects the default at store
            // construction, and `CitedBlobStore::new` applies it for direct
            // constructors too. Defaulting in the parser as well was the
            // second of two application points for one value.
            max_blob_bytes: parse_optional_u64_env(lookup, &PROXIIMA_S3_MAX_BLOB_BYTES)?,
            store_id,
        }))
    }

    /// True when `PROXIIMA_S3_BUCKET` is configured.
    ///
    /// Uses the same empty-is-unset rule as the parser, so this cannot report
    /// "configured" for a bucket [`Self::from_env`] then rejects as missing.
    #[must_use]
    pub fn present_in_env() -> bool {
        PROXIIMA_S3_BUCKET.read(&process_env).is_some()
    }

    /// The identity upload rows and purge debt record for this store.
    ///
    /// # Errors
    /// Returns `BlobError::Config` when `store_id` is not a valid operator
    /// identity, or the endpoint URL or bucket cannot name a store.
    pub fn object_store_id(&self) -> Result<proxiima_core::ObjectStoreId, BlobError> {
        match &self.store_id {
            Some(id) => proxiima_core::ObjectStoreId::operator(id)
                .map_err(|error| BlobError::Config(format!("PROXIIMA_S3_STORE_ID: {error}"))),
            None => {
                proxiima_core::ObjectStoreId::derive_s3(self.endpoint_url.as_deref(), &self.bucket)
                    .map_err(|error| {
                        BlobError::Config(format!("S3 object store identity: {error}"))
                    })
            }
        }
    }

    /// Build the internal object-I/O client solely from configured values.
    #[must_use]
    pub fn client(&self) -> Client {
        self.client_for_endpoint(self.endpoint_url.as_deref())
    }

    fn client_for_endpoint(&self, endpoint_url: Option<&str>) -> Client {
        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(self.region.clone()))
            .force_path_style(self.force_path_style)
            .credentials_provider(Credentials::new(
                self.key_pair.access_key_id.clone(),
                self.key_pair.secret_access_key.clone(),
                None,
                None,
                "proxiima-s3-config",
            ));
        if let Some(endpoint_url) = endpoint_url {
            builder = builder.endpoint_url(endpoint_url);
        }
        Client::from_conf(builder.build())
    }
}

fn required_key(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
) -> Result<String, BlobError> {
    key.read(lookup).ok_or_else(|| {
        BlobError::Config(format!("missing {key} (required with PROXIIMA_S3_BUCKET)"))
    })
}

fn parse_bool_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
) -> Result<bool, BlobError> {
    let Some(value) = key.read(lookup) else {
        return Ok(false);
    };
    proxiima_core::parse_bool_word(&value)
        .ok_or_else(|| BlobError::Config(format!("invalid boolean {key}={value}")))
}

fn parse_u64_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
    default: u64,
) -> Result<u64, BlobError> {
    let Some(value) = key.read(lookup) else {
        return Ok(default);
    };
    value
        .parse::<u64>()
        .map_err(|_| BlobError::Config(format!("invalid integer {key}={value}")))
}

fn parse_presign_ttl_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
    default: u64,
) -> Result<u64, BlobError> {
    let seconds = parse_u64_env(lookup, key, default)?;
    validate_presign_ttl(key, seconds)?;
    Ok(seconds)
}

fn parse_optional_u64_env(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &Setting,
) -> Result<Option<u64>, BlobError> {
    let Some(value) = key.read(lookup) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| BlobError::Config(format!("invalid integer {key}={value}")))
}

/// Reject an S3 endpoint that would carry presigned URLs (and the credentials
/// signed into them) over plaintext HTTP. HTTPS is always accepted; plaintext
/// HTTP is accepted only for a loopback host or a host named in
/// `plaintext_hosts` (local object store/dev, or a sidecar on a network with
/// no route out), unlike production S3 endpoints which must be TLS.
pub(crate) fn validate_endpoint_url(
    key: &Setting,
    raw: &str,
    plaintext_hosts: &PlaintextHosts,
) -> Result<(), BlobError> {
    validate_endpoint_url_for_hosts(raw, EndpointUrlPolicy::AllowLoopbackHttp, plaintext_hosts)
        .map_err(|error| match error {
            EndpointUrlError::InvalidUrl(reason) => BlobError::Config(format!(
                "{key} is not a valid URL: {reason}"
            )),
            // With no host named the message is the one this lane always gave.
            EndpointUrlError::InsecureTransport if plaintext_hosts.is_empty() => {
                BlobError::Config(format!(
                    "{key} must use https (plaintext http allowed only for loopback): {raw}"
                ))
            }
            EndpointUrlError::InsecureTransport => BlobError::Config(format!(
                "{key} must use https (plaintext http allowed only for loopback or a host named in PROXIIMA_PLAINTEXT_HOSTS): {raw}"
            )),
        })
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
