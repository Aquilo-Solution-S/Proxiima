//! The cited-blob store, split by what each piece is responsible for.
//!
//! This file holds only the service handle: its fields, its construction
//! (where the runtime config is validated), and its memoized S3 client.
//! The behaviour lives next door:
//!
//! - [`dto`] — the wire types, the whole public surface
//! - [`upload`] / [`read`] — the presigned request-path verbs
//! - [`verified_read`] — bounded in-process bytes, verified before release
//! - [`port`] — the [`CitedBlobPort`](proxiima_core::storage_ports::CitedBlobPort)
//!   adapter, translation only
//! - [`erase`] — destroy an owner's bytes; the object-store half of an
//!   owner erase
//! - [`rows`] / [`digest`] / [`keys`] / [`guards`] / [`transitions`] — the
//!   support the verbs are assembled from
//!
//! The split is by responsibility, not by line count: each of those files
//! can be read on its own, and the verbs that share an ordering contract
//! stay together.

mod cold;
mod delivery;
mod digest;
mod dto;
mod erase;
mod guards;
mod keys;
mod live_upload;
mod port;
mod read;
mod reconcile;
mod rows;
mod sweep;
mod transitions;
mod upload;
mod verified_read;

#[cfg(test)]
mod testkit;

pub use cold::S3ColdStore;
pub use dto::{
    CitedBlobUploadAbortOutcomeTs, CitedBlobUploadAbortTs, CitedBlobUploadCompleteTs,
    CitedBlobUploadPrepareOutcomeTs, CitedBlobUploadPrepareTs, PresignedHeaderTs,
};
pub use sweep::UploadExpirySweepOutcome;

use crate::config::{
    DEFAULT_MAX_BLOB_BYTES, S3RuntimeConfig, validate_endpoint_url, validate_presign_ttl,
};
use crate::error::BlobError;
use proxiima_core::authz::{SystemAuthority, SystemAuthorityBinding};
use proxiima_storage_pg::PgPlatformScope;

/// Cited-blob transfer, verified-read, and reconcile backend over one
/// Postgres pool and one S3 target. Construct once at boot; capabilities are
/// exposed through separate ports over the same shared instance.
#[derive(Debug, Clone)]
pub struct CitedBlobStore {
    pool: sqlx::PgPool,
    config: S3RuntimeConfig,
    /// The cap on one blob's size, resolved once from `config.max_blob_bytes` and the default.
    max_blob_bytes: u64,
    /// S3 client built from the configured static key pair and target,
    /// memoized once per store.
    client: std::sync::OnceLock<aws_sdk_s3::Client>,
    /// Set once by normal Proxiima boot and shared by every store clone.
    /// A witness minted by an unrelated disposable Engine cannot authorize
    /// this store's global operations.
    system_authority: std::sync::Arc<std::sync::OnceLock<SystemAuthorityBinding>>,
    platform_scope: PgPlatformScope,
    /// Computed once from the config: what upload rows and debt record.
    store_id: proxiima_core::ObjectStoreId,
    /// One slot per stage in flight, shared by every clone. A stage holds
    /// its object in memory until it is published, so the slots bound the
    /// memory staging can hold at [`MAX_CONCURRENT_STAGES`] objects.
    stage_slots: std::sync::Arc<tokio::sync::Semaphore>,
}

/// How many uploads one store stages at once; later stages wait for a slot.
pub(super) const MAX_CONCURRENT_STAGES: usize = 8;

impl CitedBlobStore {
    /// Validate and normalize the S3 configuration at its consuming boundary.
    ///
    /// # Errors
    /// Returns [`BlobError::Config`] when the configured endpoint violates the
    /// HTTPS-or-loopback-or-named-host transport policy, or the store has no
    /// valid identity ([`S3RuntimeConfig::object_store_id`]).
    pub fn new(
        pool: sqlx::PgPool,
        config: S3RuntimeConfig,
        platform_scope: PgPlatformScope,
    ) -> Result<Self, BlobError> {
        if let Some(endpoint_url) = &config.endpoint_url {
            validate_endpoint_url(
                "PROXIIMA_S3_ENDPOINT_URL",
                endpoint_url,
                &config.plaintext_hosts,
            )?;
        }
        // Checked at the consuming boundary too, not only in `from_env`: a
        // host may build this struct directly, and an out-of-range TTL must
        // fail here rather than panic inside a request.
        validate_presign_ttl("PROXIIMA_S3_UPLOAD_TTL_SECONDS", config.upload_ttl_seconds)?;
        let max_blob_bytes = config.max_blob_bytes.unwrap_or(DEFAULT_MAX_BLOB_BYTES);
        let store_id = config.object_store_id()?;

        Ok(Self {
            store_id,
            pool,
            config,
            max_blob_bytes,
            client: std::sync::OnceLock::new(),
            system_authority: std::sync::Arc::new(std::sync::OnceLock::new()),
            platform_scope,
            stage_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_STAGES)),
        })
    }

    /// Internal facade boot seam. Bind host-wide operations to the same boot
    /// instance as the Engine. Binding is shared across clones and cannot be
    /// replaced.
    ///
    /// # Errors
    ///
    /// Returns [`BlobError::State`] when a different authority already bound
    /// this store.
    #[doc(hidden)]
    pub fn bind_system_authority(&self, authority: &SystemAuthority) -> Result<(), BlobError> {
        let binding = authority.binding();
        if let Some(existing) = self.system_authority.get() {
            return if existing == &binding {
                Ok(())
            } else {
                Err(BlobError::State(
                    "cited-blob store is already bound to another system authority".into(),
                ))
            };
        }
        match self.system_authority.set(binding) {
            Ok(()) => Ok(()),
            Err(attempted) if self.system_authority.get() == Some(&attempted) => Ok(()),
            Err(_) => Err(BlobError::State(
                "cited-blob store authority binding raced another boot".into(),
            )),
        }
    }

    /// Refuse a host-wide operation unless `authority` booted this store.
    pub(super) fn require_system_authority(
        &self,
        authority: &SystemAuthority,
        operation: &str,
    ) -> Result<(), proxiima_core::StorageError> {
        let binding = self.system_authority.get().ok_or_else(|| {
            proxiima_core::StorageError::ConstraintViolation(format!(
                "{operation} requires a boot-bound store"
            ))
        })?;
        if !authority.authorizes(binding) {
            return Err(proxiima_core::StorageError::ConstraintViolation(
                "SystemAuthority belongs to a different cited-blob store boot".into(),
            ));
        }
        Ok(())
    }

    /// The memoized S3 client, built on first use from the runtime config.
    pub(super) fn client(&self) -> &aws_sdk_s3::Client {
        self.client.get_or_init(|| self.config.client())
    }

    pub(super) fn bucket(&self) -> &str {
        &self.config.bucket
    }

    /// The identity of the object store this store writes to: what its
    /// upload rows and purge debt record.
    #[must_use]
    pub fn store_id(&self) -> &proxiima_core::ObjectStoreId {
        &self.store_id
    }
}

#[cfg(test)]
mod tests {
    use proxiima_core::{Engine, SpiiceRegistry};

    use super::guards::validate_prepare;
    use super::testkit::{lazy_test_pool, prepare_req, store_config};
    use super::*;

    #[tokio::test]
    async fn store_debug_redacts_secret_before_and_after_client_initialization() {
        let secret = "test-secret-key";
        let store = CitedBlobStore::new(
            lazy_test_pool(),
            store_config(None, None),
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .unwrap();
        assert!(!format!("{store:?}").contains(secret));
        store.client();
        assert!(!format!("{store:?}").contains(secret));
    }

    #[tokio::test]
    async fn store_client_uses_the_internal_endpoint() {
        use aws_sdk_s3::presigning::PresigningConfig;
        let mut config = store_config(Some("https://objects.internal.example/internal/"), None);
        config.force_path_style = true;
        let store = CitedBlobStore::new(
            lazy_test_pool(),
            config,
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .unwrap();
        let signed = store
            .client()
            .get_object()
            .bucket(store.bucket())
            .key("config-probe")
            .presigned(PresigningConfig::expires_in(std::time::Duration::from_mins(1)).unwrap())
            .await
            .expect("configuration-only presigning succeeds");
        assert!(
            signed
                .uri()
                .starts_with("https://objects.internal.example/internal/test-bucket/config-probe?")
        );
    }

    #[tokio::test]
    async fn store_rejects_plaintext_non_loopback_endpoint() {
        let error = CitedBlobStore::new(
            lazy_test_pool(),
            store_config(Some("http://s3.internal:9000"), Some(1_024)),
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .expect_err("plaintext remote S3 endpoint rejected");

        assert!(matches!(error, BlobError::Config(_)));
        assert!(error.to_string().contains("must use https"));
    }

    #[tokio::test]
    async fn store_accepts_a_plaintext_endpoint_only_on_a_named_host() {
        let named = |hosts: &str| {
            let mut config = store_config(Some("http://gateway:9090"), Some(1_024));
            config.plaintext_hosts = proxiima_core::PlaintextHosts::parse(hosts).unwrap();
            CitedBlobStore::new(
                lazy_test_pool(),
                config,
                proxiima_storage_pg::PgPlatformScope::for_tests(),
            )
        };
        named("gateway").expect("named host accepted");
        let error = named("other").expect_err("unnamed host refused");
        assert!(error.to_string().contains("must use https"));
        let error = CitedBlobStore::new(
            lazy_test_pool(),
            store_config(Some("http://gateway:9090"), Some(1_024)),
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .expect_err("nothing named: refused");
        assert!(error.to_string().contains("must use https"));
    }

    #[tokio::test]
    async fn store_applies_default_cap_when_config_omits_it() {
        let store = CitedBlobStore::new(
            lazy_test_pool(),
            store_config(None, None),
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .expect("default-capped store builds");

        assert_eq!(store.max_blob_bytes, DEFAULT_MAX_BLOB_BYTES);
        let error = validate_prepare(
            &prepare_req(DEFAULT_MAX_BLOB_BYTES + 1),
            store.max_blob_bytes,
        )
        .expect_err("default cap rejects an oversized prepare request");
        assert!(error.to_string().contains("exceeds max blob size"));
    }

    #[tokio::test]
    async fn store_authority_binding_is_same_boot_idempotent_and_foreign_boot_rejected() {
        let store = CitedBlobStore::new(
            lazy_test_pool(),
            store_config(None, None),
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .expect("test store builds");
        let (_, authority) =
            Engine::new(SpiiceRegistry::new().freeze_or_panic_for_tests()).into_system_authority();
        let (_, foreign) =
            Engine::new(SpiiceRegistry::new().freeze_or_panic_for_tests()).into_system_authority();

        store.bind_system_authority(&authority).expect("first bind");
        store
            .bind_system_authority(&authority)
            .expect("same-boot bind is idempotent");
        let error = store
            .bind_system_authority(&foreign)
            .expect_err("a different boot cannot replace the binding");

        assert!(matches!(error, BlobError::State(_)));
    }

    /// An out-of-range upload TTL is a startup error, not a request-path panic.
    #[tokio::test]
    async fn store_rejects_a_ttl_no_presigned_url_could_carry() {
        let mut config = store_config(None, None);
        config.upload_ttl_seconds = 0;
        let error = CitedBlobStore::new(
            lazy_test_pool(),
            config,
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .expect_err("a zero upload TTL is refused");
        assert!(
            error.to_string().contains("PROXIIMA_S3_UPLOAD_TTL_SECONDS"),
            "the refusal names the variable to fix: {error}"
        );

        // The boundary itself is usable, so the bound is not merely small.
        let mut config = store_config(None, None);
        config.upload_ttl_seconds = crate::config::MAX_PRESIGN_TTL_SECONDS;
        CitedBlobStore::new(
            lazy_test_pool(),
            config,
            proxiima_storage_pg::PgPlatformScope::for_tests(),
        )
        .expect("7 days is allowed");
    }
}
