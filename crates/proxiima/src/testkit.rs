//! Test support for spiice crates: `proxiima = { features = ["testkit"] }`.
//!
//! Everything a spiice's `tests/support` used to rebuild: the Postgres
//! fixtures (`proxiima-pg-testkit`, re-exported whole), an owner-scoped
//! [`AuthzContext`], split platform/runtime databases ([`SplitRoleDb`]), a
//! migrated split-role database per test ([`HostTemplate`]), and the
//! declaration/presence trigger drift check.

pub use proxiima_pg_testkit::*;

/// A verified owner scope over several named owners (admin on each), for
/// spiice tests that read across owners.
pub use proxiima_core::test_fixtures::owner_scope_for_tests;

/// An engine whose storage refuses every call, for a test that serves tools
/// through a [`crate::ToolHost`] without reaching storage.
pub use proxiima_core::test_fixtures::rejecting_engine;

use proxiima_core::{AuthPath, AuthzContext, OwnerId, Role, SpiiceRegistry, UserId};
use proxiima_storage_pg::{
    PgPoolConfig, PgSidecarRegistry, PgSidecarRegistryFrozen, PgTuning, VectorStorage,
    register_core_pg_sidecars,
};
use sqlx::PgPool;

use proxiima_spiice::{NamedMigrator, SpiiceBundle};

use crate::migrations::{MigrationError, MigrationSource, check_lineup, lineup, run_lineup};

/// The store number a [`HostTemplate`] initializes, as the migrate step does:
/// a test boots its clone with `.store_number(TEMPLATE_STORE_NUMBER)`.
pub const TEMPLATE_STORE_NUMBER: u16 = proxiima_core::ledger!(Proxiima.Repository.templateStore);

/// A migrated, split-role template database a host suite clones once per
/// test.
///
/// `Proxiima.Repository.TemplateLifecycle` and `Proxiima.Repository.CloneRole`.
/// Boot clones with [`TEMPLATE_STORE_NUMBER`]. The caller fingerprint can mark
/// seed inputs the migrators do not carry.
///
/// ```compile_fail,E0308
/// // The family is a validated `TemplateFamily`; a host never passes a raw name.
/// let _ = proxiima::testkit::HostTemplate::new("host_", "seed-v1", Vec::new(), proxiima::PgSidecarRegistryFrozen::default());
/// ```
///
/// A lineup the runner refuses has no template, so none is ever looked up:
///
/// ```compile_fail,E0599
/// fn name(family: proxiima::testkit::TemplateFamily) -> String {
///     proxiima::testkit::HostTemplate::new(family, "seed-v1", Vec::new(), proxiima::PgSidecarRegistryFrozen::default()).template_name()
/// }
/// ```
#[derive(Debug)]
pub struct HostTemplate {
    family: TemplateFamily,
    hash: u64,
    vectors: VectorStorage,
    /// Core first, then the host's migrators: what a build runs and the hash
    /// covers.
    sources: Vec<NamedMigrator>,
    sidecars: PgSidecarRegistryFrozen,
}

impl HostTemplate {
    /// A template of `family` for `migrators`, added to core's. Nothing
    /// touches the database until [`Self::clone_split_role`].
    ///
    /// `migrators` are the spiices' and the host's own, as the host's
    /// `SpiiceBundle::migrators` returns them; a host passes the same list to
    /// its boot, so what the template holds is what the boot expects.
    /// `sidecars` is the same frozen core/spiice composition as that boot;
    /// it supplies the Follows owner-RLS installation and template identity.
    ///
    /// # Errors
    ///
    /// Returns the [`MigrationError`] a run refuses the lineup with before it
    /// connects: a repeated version, a shared ledger. Such a lineup gets no
    /// template name, because the hash cannot tell it from a valid one (it
    /// reads each lane through `MigrationSource::lane`, which lists a repeated
    /// version once) and a name could match a cached template.
    pub fn new(
        family: TemplateFamily,
        fingerprint: &str,
        migrators: Vec<NamedMigrator>,
        sidecars: PgSidecarRegistryFrozen,
    ) -> Result<Self, MigrationError> {
        Self::for_vectors(
            VectorStorage::Local,
            family,
            fingerprint,
            migrators,
            sidecars,
        )
    }

    /// [`Self::new`] for a store whose vectors live in a host index
    /// ([`VectorStorage::HostIndex`]): the template's database has no
    /// `vector` extension, and a boot with `RuntimeBuilder::vector_index`
    /// finds every migration applied.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn host_index(
        family: TemplateFamily,
        fingerprint: &str,
        migrators: Vec<NamedMigrator>,
        sidecars: PgSidecarRegistryFrozen,
    ) -> Result<Self, MigrationError> {
        Self::for_vectors(
            VectorStorage::HostIndex,
            family,
            fingerprint,
            migrators,
            sidecars,
        )
    }

    fn for_vectors(
        vectors: VectorStorage,
        family: TemplateFamily,
        fingerprint: &str,
        migrators: Vec<NamedMigrator>,
        sidecars: PgSidecarRegistryFrozen,
    ) -> Result<Self, MigrationError> {
        let sources = lineup(migrators, vectors);
        check_lineup(&sources)?;
        Ok(Self {
            hash: fingerprint_hash(fingerprint, vectors, &sources, &sidecars),
            vectors,
            family,
            sources,
            sidecars,
        })
    }

    /// The name of the template database, for diagnostics: the family prefix
    /// and 16 hex digits.
    #[must_use]
    pub fn template_name(&self) -> String {
        self.family.template_name(self.hash)
    }

    /// Clone the template into `<prefix>_<uuid>` and return it as a
    /// [`SplitRoleDb`], building the template first if the server does not
    /// have it.
    ///
    /// The clone is adopted by a [`DbGuard`]: dropping the returned value
    /// after a passing test drops the database; after a panic it keeps it
    /// and prints a redacted `psql` URL. `schemas` is what
    /// [`SplitRoleDb::create`] takes for its spiice schemas. The template is
    /// already prepared, so providing the roles on the clone re-asserts the
    /// two roles and the database grant and returns early.
    ///
    /// Safe to call from many tests at once: the template is built once per
    /// server, under an advisory lock, and the others wait for it.
    ///
    /// # Errors
    ///
    /// Returns [`sqlx::Error::Configuration`] when `PROXIIMA_TEST_PG_URL` is
    /// unset, [`sqlx::Error::Protocol`] carrying the text of a migration
    /// error from the build (a refused ledger, a failing statement), and the
    /// admin, clone or role-provisioning errors of the steps around it. A
    /// failed build leaves no template behind.
    pub async fn clone_split_role(
        &self,
        prefix: &str,
        schemas: &[&str],
    ) -> Result<SplitRoleDb, sqlx::Error> {
        // The lease holds the template against collection until it is cloned.
        let lease = ensure_template_in(&self.family, self.hash, |pool| self.build(pool)).await?;
        let cloned = SplitRoleDb::from_template(prefix, &lease, schemas).await;
        let released = lease.release().await;
        let clone = cloned?;
        released?;
        Ok(clone)
    }

    /// Provision the split roles on the staging database `pool` points at,
    /// then migrate it as the platform role, which owns what it creates.
    async fn build(&self, pool: PgPool) -> Result<(), sqlx::Error> {
        let staging: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await?;
        // Nothing may stay connected to a database that is renamed into place.
        pool.close().await;
        let (_, platform_url) = match self.vectors {
            VectorStorage::Local => split_role_urls(&staging).await,
            VectorStorage::HostIndex => split_role_urls_without_pgvector(&staging).await,
        }?;
        let pg = proxiima_storage_pg::PgMigrationStorage::connect_with_config(
            &platform_url,
            PgPoolConfig::default(),
            PgTuning::default(),
        )
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?
        .with_vector_storage(self.vectors);
        let migrated = match run_lineup(&pg, &self.sources, &self.sidecars).await {
            Ok(_) => pg
                .initialize_store_number(TEMPLATE_STORE_NUMBER)
                .await
                .map_err(|error| sqlx::Error::Protocol(error.to_string())),
            Err(error) => Err(sqlx::Error::Protocol(error.to_string())),
        };
        pg.clone_pool_for_backend().close().await;
        migrated
    }
}

/// `Proxiima.Repository.TemplateFingerprintInput`. Length framing separates successive inputs.
fn fingerprint_hash(
    fingerprint: &str,
    vectors: VectorStorage,
    sources: &[NamedMigrator],
    sidecars: &PgSidecarRegistryFrozen,
) -> u64 {
    fn feed(hash: u64, bytes: &[u8]) -> u64 {
        fnv1a64_extend(
            fnv1a64_extend(hash, &(bytes.len() as u64).to_be_bytes()),
            bytes,
        )
    }
    let mut hash = feed(FNV_OFFSET_BASIS, fingerprint.as_bytes());
    let vector_kind = match vectors {
        VectorStorage::Local => b"local".as_slice(),
        VectorStorage::HostIndex => b"host-index".as_slice(),
    };
    hash = feed(hash, vector_kind);
    hash = feed(
        hash,
        &proxiima_storage_pg::definitions::checksum_bytes(vectors),
    );
    hash = feed(hash, proxiima_core::RELEASE_VERSION.as_bytes());
    hash = feed(hash, &TEMPLATE_STORE_NUMBER.to_be_bytes());
    hash = feed(hash, &sidecars.search_definitions_fingerprint());
    hash = feed(hash, &(sources.len() as u64).to_be_bytes());
    for source in sources {
        hash = feed(hash, source.source().as_bytes());
        hash = feed(hash, source.ledger().as_bytes());
        let lane = source.lane();
        hash = feed(hash, &(lane.len() as u64).to_be_bytes());
        for migration in lane {
            hash = fnv1a64_extend(hash, &migration.version.to_be_bytes());
            hash = feed(hash, migration.checksum.as_ref());
        }
    }
    let mut schemas = sidecars.spiice_schema_names().to_vec();
    schemas.sort_unstable();
    let mut follows = sidecars.owner_follows_memory_sidecar_tables();
    follows.sort_unstable();
    for names in [schemas, follows] {
        hash = feed(hash, &(names.len() as u64).to_be_bytes());
        for name in names {
            hash = feed(hash, name.as_bytes());
        }
    }
    hash
}

/// A verified caller acting in its own personal owner, sealed to it.
/// `Proxiima.Repository.fixtureSubject`.
///
/// # Panics
///
/// Never: a personal owner narrows to its own user by construction.
#[must_use]
pub fn personal_authz(user: UserId) -> AuthzContext {
    proxiima_core::test_fixtures::authenticated_context(
        AuthzContext::for_subject(user, AuthPath::HostBearer)
            .narrowed_to_owner(user.into())
            .expect("a personal owner narrows to its own user"),
    )
}

/// A fresh verified caller with the admin role on the Group `owner`, sealed to it.
/// `Proxiima.Repository.fixtureSubject`.
///
/// # Panics
///
/// Never: an admin on exactly `owner` narrows to it by construction.
#[must_use]
pub fn group_authz(owner: OwnerId) -> AuthzContext {
    let context = AuthzContext::for_subject_with_role(
        UserId::new(uuid::Uuid::now_v7()),
        [(owner, Role::admin())],
        AuthPath::HostBearer,
    )
    .narrowed_to_owner(owner)
    .expect("an admin on exactly this owner narrows to it");
    proxiima_core::test_fixtures::authenticated_context(context)
}

/// A verified caller that is admin on each of `owners`: its owner scope
/// reads all of them, for a test that shows a tool answers for the calling
/// owner only.
#[must_use]
pub fn shared_authz(owners: &[OwnerId]) -> AuthzContext {
    proxiima_core::test_fixtures::authenticated_context(AuthzContext::for_subject_with_role(
        UserId::new(uuid::Uuid::now_v7()),
        owners.iter().map(|owner| (*owner, Role::admin())),
        AuthPath::HostBearer,
    ))
}

/// Compare generated sidecar triggers with migration SQL. `Proxiima.Repository.TriggerFixtureRefusal`.
///
/// Boot compares the same statements with the live database.
///
/// # Panics
///
/// When `F` does not register or freeze, when it registers no memory
/// sidecar under `spiice_id` (nothing to check usually means a wrong id),
/// or when a generated statement is missing, listing each one.
pub fn assert_trigger_migrations<F: SpiiceBundle>(spiice_id: &str, migrations: &[&str]) {
    let mut registry = SpiiceRegistry::new();
    F::register(&mut registry)
        .unwrap_or_else(|error| panic!("spiice {spiice_id} does not register: {error}"));
    let registry = registry
        .try_freeze()
        .unwrap_or_else(|error| panic!("spiice {spiice_id} does not freeze: {error}"));
    let mut sidecars = PgSidecarRegistry::new();
    register_core_pg_sidecars(&mut sidecars);
    F::register_pg_sidecars(&mut sidecars);
    let sidecars = sidecars
        .freeze_against(&registry)
        .unwrap_or_else(|error| panic!("spiice {spiice_id} PG sidecars do not freeze: {error}"));
    let mut artifacts = sidecars
        .declaration_trigger_artifacts(spiice_id)
        .unwrap_or_else(|error| panic!("declaration triggers for {spiice_id}: {error}"));
    artifacts.extend(
        sidecars
            .presence_trigger_artifacts(spiice_id)
            .unwrap_or_else(|error| panic!("presence triggers for {spiice_id}: {error}")),
    );
    assert!(
        !artifacts.is_empty(),
        "spiice {spiice_id} registers no memory sidecar under that id, so there is no trigger to check"
    );
    let missing: Vec<&str> = artifacts
        .iter()
        .map(|artifact| artifact.forward.as_str())
        .filter(|forward| !migrations.iter().any(|sql| sql.contains(forward)))
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {} generated trigger statements for {spiice_id} appear in no migration \
         (ship them verbatim in a new migration):\n\n{}",
        missing.len(),
        artifacts.len(),
        missing.join("\n\n")
    );
}
