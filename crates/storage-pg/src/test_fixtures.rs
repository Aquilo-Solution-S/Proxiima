use proxiima_core::StorageError;
use std::sync::Arc;

use proxiima_storage_pg::{
    PgPlatformScope, PgPoolConfig, PgStorage, PgTuning, VectorStorage, core_migrator_for,
};

use proxiima_core::storage_ports::{FactIngestPort, OwnerWritePermit};
use proxiima_core::verbs::fact_ingest::{FactIngestOutcome, FactWriteCommand};
use proxiima_core::{
    AccessKind, EdgeEndpoint, GroupId, MemoryId, MemoryKind, OwnerId, Relation, SchemaId,
    SchemaVersion, UserId, memory_store_number, new_memory_uuid,
};
use proxiima_pg_testkit::{
    DbGuard, FNV_OFFSET_BASIS, TemplateFamily, create_db_from_template,
    create_named_db_from_template, db_url, drop_db, ensure_template, ensure_template_in,
    fnv1a64_extend, split_role_urls, split_role_urls_without_pgvector,
};
use sqlx::Connection;
use uuid::Uuid;

/// The test roles for `database`; a host-index database gets no `vector`
/// extension.
async fn roles_for(
    database: &str,
    vectors: VectorStorage,
) -> Result<(String, String), sqlx::Error> {
    match vectors {
        VectorStorage::Local => split_role_urls(database).await,
        VectorStorage::HostIndex => split_role_urls_without_pgvector(database).await,
    }
}

/// The host-index kind's templates are a family of their own: a template
/// build drops the idle templates of its family, and the two kinds must not
/// evict each other's.
fn host_index_family() -> TemplateFamily {
    TemplateFamily::new("proxiima_tmpl_corehi_").expect("a valid template family")
}

/// What the template of this store kind is a function of: its migrations, its
/// definitions and the release.
fn core_template_hash(vectors: VectorStorage) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for migration in core_migrator_for(vectors).iter() {
        hash = fnv1a64_extend(hash, &migration.version.to_be_bytes());
        hash = fnv1a64_extend(hash, migration.checksum.as_ref());
    }
    hash = fnv1a64_extend(
        hash,
        proxiima_storage_pg::definitions::checksum_hex(vectors).as_bytes(),
    );
    fnv1a64_extend(hash, proxiima_core::RELEASE_VERSION.as_bytes())
}

#[must_use]
pub fn core_template_name(vectors: VectorStorage) -> String {
    let hash = core_template_hash(vectors);
    match vectors {
        VectorStorage::Local => format!("proxiima_tmpl_core_{hash:016x}"),
        VectorStorage::HostIndex => host_index_family().template_name(hash),
    }
}

/// Build the full core schema template once per migration set.
///
/// # Errors
///
/// Returns admin connection, template build, or migration errors.
pub async fn ensure_core_template(vectors: VectorStorage) -> Result<String, sqlx::Error> {
    let template = core_template_name(vectors);
    let build = |pool: sqlx::PgPool| async move {
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await?;
        let (_runtime_url, platform_url) = roles_for(&database, vectors).await?;
        pool.close().await;
        let mut connection = sqlx::PgConnection::connect(&platform_url).await?;
        let mut transaction = proxiima_storage_pg::begin_migration_transaction(&mut connection)
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        proxiima_storage_pg::run_core_schema(
            &mut transaction,
            proxiima_storage_pg::MissingMigrationPolicy::Reject,
            vectors,
        )
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        transaction.commit().await
    };
    match vectors {
        VectorStorage::Local => ensure_template(&template, build).await?,
        VectorStorage::HostIndex => {
            ensure_template_in(&host_index_family(), core_template_hash(vectors), build)
                .await?
                .release()
                .await?;
        }
    }
    Ok(template)
}

/// Create `name` already migrated through the full core template of a store
/// with pgvector vectors, cloned from that template. Drop it with `drop_db`
/// like any other test database.
///
/// # Errors
///
/// Returns admin connection, template build, or clone errors.
pub async fn create_core_db(name: &str) -> Result<(), sqlx::Error> {
    let template = ensure_core_template(VectorStorage::Local).await?;
    create_named_db_from_template(name, &template).await
}

/// Clone a fresh test database from the core migrated template.
///
/// The returned [`DbGuard`] drops the clone when the test passes and keeps
/// it (printing a `psql` URL) when the test panics.
///
/// # Panics
///
/// Panics when the local test Postgres admin connection, template
/// creation, clone creation, or cloned database connection fails.
pub async fn fresh_pg(prefix: &str) -> (PgStorage, DbGuard) {
    fresh_pg_with(prefix, None).await
}

/// [`fresh_pg`] for a store whose vectors live in `index`: its database has
/// no `vector` extension.
///
/// # Panics
///
/// As [`fresh_pg`].
pub async fn fresh_pg_host_index(
    prefix: &str,
    index: Arc<dyn proxiima_core::storage_ports::VectorIndex>,
) -> (PgStorage, DbGuard) {
    fresh_pg_with(prefix, Some(index)).await
}

async fn fresh_pg_with(
    prefix: &str,
    index: Option<Arc<dyn proxiima_core::storage_ports::VectorIndex>>,
) -> (PgStorage, DbGuard) {
    let vectors = match index {
        Some(_) => VectorStorage::HostIndex,
        None => VectorStorage::Local,
    };
    let template = ensure_core_template(vectors)
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but admin connect failed: {e}"));

    let db_name = create_db_from_template(prefix, &template)
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but admin connect failed: {e}"));
    let (runtime_url, platform_url) = roles_for(&db_name, vectors)
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but role provisioning failed: {e}"));
    let migration = proxiima_storage_pg::PgMigrationStorage::connect_with_config(
        &platform_url,
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await
    .unwrap_or_else(|e| panic!("PG required for tests but migration connect failed: {e}"))
    .with_vector_storage(vectors);
    migration
        .run_migrations()
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but migration failed: {e}"));
    migration
        .initialize_store_number(1)
        .await
        .expect("fixture store identity");
    let platform_pool = serving_platform_pool(&platform_url)
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but platform connect failed: {e}"));
    let platform_scope = PgPlatformScope::new(platform_pool, &["proxiima_core"])
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but platform validation failed: {e}"));
    let runtime_url = proxiima_pg_testkit::through_test_pooler(&runtime_url);
    let connected = match index {
        Some(index) => {
            PgStorage::connect_with_vector_index(
                &runtime_url,
                PgPoolConfig::default(),
                PgTuning::default(),
                platform_scope,
                1,
                index,
            )
            .await
        }
        None => {
            PgStorage::connect_with_config(
                &runtime_url,
                PgPoolConfig::default(),
                PgTuning::default(),
                platform_scope,
                1,
            )
            .await
        }
    };
    let storage = match connected {
        Ok(pg) => pg,
        Err(err) => {
            let _ = drop_db(&db_name).await;
            panic!("PG required for tests but unavailable: {err}");
        }
    };
    let admin_pool = sqlx::PgPool::connect(&db_url(&db_name))
        .await
        .unwrap_or_else(|e| panic!("PG required for tests but admin pool failed: {e}"));
    (
        storage.with_admin_pool_for_tests(admin_pool),
        DbGuard::adopt(db_name),
    )
}

/// The fixture's platform pool: direct, or through the transaction-mode
/// pooler named by `PROXIIMA_TEST_PGBOUNCER_URL` with the serving pool policy
/// (a pooler refuses the startup parameters a bare `sqlx` connect sends).
async fn serving_platform_pool(platform_url: &str) -> Result<sqlx::PgPool, StorageError> {
    if proxiima_pg_testkit::test_pooler_url().is_some() {
        return PgPoolConfig::default()
            .connect_lazy(&proxiima_pg_testkit::through_test_pooler(platform_url));
    }
    sqlx::PgPool::connect(platform_url)
        .await
        .map_err(|error| StorageError::Unavailable(error.to_string()))
}

/// Clone a typed Abstraction admission with a citation present at INSERT.
///
/// This fixture exists because production Abstraction authoring currently
/// does not expose citation input, while the append-only memory trigger
/// intentionally rejects retrofitting `blob_id` onto an existing admission.
/// The clone preserves the template's owner, content, pins, and sidecar
/// payload, but receives fresh canonical identities and the supplied blob.
///
/// # Errors
///
/// Returns database errors when the template, citation, or cloned sidecar
/// cannot be admitted atomically.
#[cfg(feature = "testkit")]
pub async fn cited_abstraction_from_template(
    pool: &sqlx::PgPool,
    template: MemoryId,
    blob_id: Uuid,
) -> Result<MemoryId, sqlx::Error> {
    let handle = new_memory_uuid(memory_store_number(template.into_inner()));
    let memory = new_memory_uuid(memory_store_number(template.into_inner()));
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO proxiima_core.memory_head
             (handle, kind, schema_id, owner_id, t, publication_sequence)
         SELECT $1, kind, schema_id, owner_id, $2, 0
           FROM proxiima_core.memory_head
          WHERE handle = (SELECT handle FROM proxiima_core.memory_including_forgotten WHERE t = $3)",
    )
    .bind(handle)
    .bind(memory)
    .bind(template.into_inner())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO proxiima_core.memory
             (handle, t, kind, owner_id, schema_id, source_id, ingest_key,
              blob_id, content_id, origins, refs, sidecar_tables, goal_refs,
              guided_by, reads, occurred_at, by, via, lexical_language, forgotten_at)
         SELECT $1, $2, kind, owner_id, schema_id, NULL, NULL,
                $3, content_id, origins, refs, sidecar_tables, goal_refs,
                guided_by, reads, occurred_at, by, via, lexical_language, forgotten_at
           FROM proxiima_core.memory_including_forgotten
          WHERE t = $4",
    )
    .bind(handle)
    .bind(memory)
    .bind(blob_id)
    .bind(template.into_inner())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO proxiima_core.agent_derivation_v1
             (t, title, body, tags, idempotency_key, source_memory_ids,
              model_id, client_name, client_version)
         SELECT $1, title, body, tags, NULL, source_memory_ids,
                model_id, client_name, client_version
           FROM proxiima_core.agent_derivation_v1
          WHERE t = $2",
    )
    .bind(memory)
    .bind(template.into_inner())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(MemoryId::new(memory))
}

/// Connect a disposable database through the real split-role boot sequence.
///
/// Test callers pass the administrator URL used to create the database. The
/// database name is taken from its final path component; role provisioning,
/// migrations, platform validation, and runtime connection then happen in
/// the same order as production boot.
///
/// # Errors
///
/// Refuses an invalid database URL or failed role provisioning, migration,
/// platform validation, or database connection.
#[cfg(feature = "testkit")]
pub async fn connect_for_tests(url: String) -> Result<PgStorage, StorageError> {
    let database = url
        .rsplit('/')
        .next()
        .and_then(|path| path.split('?').next())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            StorageError::Unavailable("test database URL has no database name".into())
        })?;
    let (runtime_url, platform_url) = split_role_urls(database)
        .await
        .map_err(|error| StorageError::Unavailable(error.to_string()))?;
    let migration = proxiima_storage_pg::PgMigrationStorage::connect_with_config(
        &platform_url,
        PgPoolConfig::default(),
        PgTuning::default(),
    )
    .await?;
    migration.run_migrations().await?;
    migration.initialize_store_number(1).await?;
    let platform_pool = serving_platform_pool(&platform_url).await?;
    let platform_scope = PgPlatformScope::new(platform_pool, &["proxiima_core"]).await?;
    let storage = PgStorage::connect_with_config(
        &proxiima_pg_testkit::through_test_pooler(&runtime_url),
        PgPoolConfig::default(),
        PgTuning::default(),
        platform_scope,
        1,
    )
    .await?;
    let admin_pool = sqlx::PgPool::connect(&url)
        .await
        .map_err(|error| StorageError::Unavailable(error.to_string()))?;
    Ok(storage.with_admin_pool_for_tests(admin_pool))
}

impl PgStorage {
    /// Test-only serving connection using the real split-role boot sequence.
    ///
    /// # Errors
    ///
    /// Returns the provisioning, migration, validation, or connection error
    /// from [`connect_for_tests`].
    #[cfg(feature = "testkit")]
    pub async fn connect_for_tests(url: String) -> Result<Self, StorageError> {
        connect_for_tests(url).await
    }
}

/// Register `user` in `proxiima_core.owners` the way the first write under it
/// does ([`crate::access::owner_registry::ensure_owner_row`]). Idempotent.
///
/// # Errors
///
/// Storage errors from the registration.
pub async fn create_user(pool: &sqlx::PgPool, user: UserId) -> Result<(), StorageError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    crate::access::owner_registry::ensure_owner_row(&mut conn, &OwnerId::from(user)).await?;
    Ok(())
}

/// Register `group` as a group in `proxiima_core.owners`, the way a host
/// creates one before writing under it
/// ([`crate::access::owner_registry::register_group`]). Idempotent; an id
/// already registered as a user is a `Conflict`.
///
/// # Errors
///
/// Storage errors from the registration.
pub async fn create_group(pool: &sqlx::PgPool, group: GroupId) -> Result<(), StorageError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    crate::access::owner_registry::register_group(&mut conn, group).await
}

/// Grant `member` the `relation` in `group` the way the engine's
/// `add_member` does
/// ([`crate::access::owner_columns::add_group_member_on_connection`]): an
/// unseen group is registered first, a user member adds a person relation,
/// and a group member attaches that group as a child. Idempotent.
///
/// # Errors
///
/// `Conflict` when `group` is registered as a user or the child group is
/// already attached elsewhere; other storage errors from the write.
pub async fn add_member(
    pool: &sqlx::PgPool,
    group: GroupId,
    member: impl Into<OwnerId>,
    relation: Relation,
) -> Result<(), StorageError> {
    let mut conn = pool
        .acquire()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    crate::access::owner_columns::add_group_member_on_connection(
        &mut conn,
        group,
        member.into(),
        relation,
    )
    .await?;
    Ok(())
}

/// A memory with defaults, written through the port the product uses
/// ([`FactIngestPort::ingest_fact_atomic`]): name only the fields a test
/// asserts on.
#[derive(Debug, Clone)]
pub struct TestMemory {
    command: FactWriteCommand,
}

impl TestMemory {
    /// A payload-less `core/test-fact-v1` Fact with no source, receipt,
    /// references or citation.
    #[must_use]
    pub fn fact() -> Self {
        Self::from_command(FactWriteCommand {
            schema_id: SchemaId::new("core/test-fact-v1".to_owned()),
            schema_version: SchemaVersion::new(1),
            handle: None,
            source_id: None,
            ingest_key: None,
            payload: Vec::new(),
            rendered_text: None,
            lexical_language: None,
            receipt: None,
            occurred_at: None,
            citation: None,
            additional_references: Vec::new(),
            refs: Vec::new(),
            blob_id: None,
            kind: MemoryKind::Fact,
            request: None,
        })
    }

    /// Write a command the test built itself, such as a typed spiice payload.
    #[must_use]
    pub fn from_command(command: FactWriteCommand) -> Self {
        Self { command }
    }

    /// The memory kind (Fact, Abstraction or Perspective).
    #[must_use]
    pub fn kind(mut self, kind: MemoryKind) -> Self {
        self.command.kind = kind;
        self
    }

    /// Append a version to this series instead of starting a new one.
    #[must_use]
    pub fn handle(mut self, handle: Uuid) -> Self {
        self.command.handle = Some(handle);
        self
    }

    /// The memories this one references.
    #[must_use]
    pub fn references(mut self, references: Vec<EdgeEndpoint>) -> Self {
        self.command.additional_references = references;
        self
    }

    /// Observation-neutral pins (visit, write-act).
    #[must_use]
    pub fn refs(mut self, refs: Vec<Uuid>) -> Self {
        self.command.refs = refs;
        self
    }

    /// Write the memory under `owner`.
    ///
    /// # Errors
    ///
    /// The storage error the write refuses with.
    pub async fn create(
        self,
        pg: &PgStorage,
        owner: OwnerId,
    ) -> Result<FactIngestOutcome, StorageError> {
        let permit = OwnerWritePermit::new_for_tests(owner, AccessKind::Fact);
        pg.ingest_fact_atomic(&permit, &self.command, None).await
    }
}

/// Stable writer for synthetic storage rows with no authenticated request.
#[must_use]
pub const fn writer() -> proxiima_core::WriterAttribution {
    proxiima_core::WriterAttribution {
        by: uuid::Uuid::from_u128(0x509),
        via: None,
    }
}

/// A Content row for `(owner, schema_id, content_hash)`, written the way an
/// admission writes it (`crate::verbs::content::ensure_content`); an
/// existing row for the same triple is reused. Returns its `content_id`.
///
/// # Errors
///
/// Storage errors from the write.
pub async fn create_content(
    pool: &sqlx::PgPool,
    owner: OwnerId,
    schema_id: &str,
    content_hash: [u8; 32],
) -> Result<Uuid, StorageError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    let content_id = crate::verbs::content::ensure_content(
        &mut tx,
        owner.into_inner(),
        schema_id,
        &content_hash,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    Ok(content_id)
}

fn unit_vector(width: usize) -> Vec<f32> {
    let mut values = vec![0.0; width];
    values[0] = 1.0;
    values
}

/// An entity an embedding is written for: a Fact by its [`MemoryId`], a
/// Goal by its [`GoalId`](proxiima_core::GoalId), or any
/// [`EmbeddableEntityRef`](proxiima_core::EmbeddableEntityRef) (an
/// Abstraction or a Perspective).
pub trait EmbeddingTarget {
    /// The embeddable entity this names.
    fn entity(self) -> proxiima_core::EmbeddableEntityRef;
}

impl EmbeddingTarget for MemoryId {
    fn entity(self) -> proxiima_core::EmbeddableEntityRef {
        proxiima_core::EmbeddableEntityRef::Memory {
            kind: proxiima_core::EntityKind::Fact,
            memory_id: self,
        }
    }
}

impl EmbeddingTarget for proxiima_core::GoalId {
    fn entity(self) -> proxiima_core::EmbeddableEntityRef {
        proxiima_core::EmbeddableEntityRef::Goal(self)
    }
}

impl EmbeddingTarget for proxiima_core::EmbeddableEntityRef {
    fn entity(self) -> proxiima_core::EmbeddableEntityRef {
        self
    }
}

/// Where a test writes an embedding: a [`PgStorage`], or the admin pool of a
/// database a host under test serves, when the test holds no storage handle.
/// The pool write is the local vector write in a plain transaction, so it is
/// only right for a role that bypasses row security and a store whose vectors
/// live in the database; any other role is refused.
#[async_trait::async_trait]
pub trait EmbeddingHome: Sync {
    /// Write one new embedding version of `entity` the way the embedding port
    /// does once it has checked the job claim; returns the version written.
    ///
    /// # Errors
    ///
    /// Storage errors from the write.
    async fn write_embedding(
        &self,
        owner: &OwnerId,
        entity: proxiima_core::EmbeddableEntityRef,
        vectors: &[proxiima_core::SpaceVector],
    ) -> Result<i32, StorageError>;
}

#[async_trait::async_trait]
impl EmbeddingHome for PgStorage {
    async fn write_embedding(
        &self,
        owner: &OwnerId,
        entity: proxiima_core::EmbeddableEntityRef,
        vectors: &[proxiima_core::SpaceVector],
    ) -> Result<i32, StorageError> {
        let mut tx = self.platform_transaction().await?;
        let outcome = self
            .insert_embedding_version(&mut tx, owner, entity, vectors)
            .await?;
        tx.commit()
            .await
            .map_err(|err| StorageError::Unavailable(err.to_string()))?;
        Ok(outcome.embedding_version)
    }
}

#[async_trait::async_trait]
impl<T: EmbeddingHome + Send> EmbeddingHome for Arc<T> {
    async fn write_embedding(
        &self,
        owner: &OwnerId,
        entity: proxiima_core::EmbeddableEntityRef,
        vectors: &[proxiima_core::SpaceVector],
    ) -> Result<i32, StorageError> {
        self.as_ref().write_embedding(owner, entity, vectors).await
    }
}

#[async_trait::async_trait]
impl EmbeddingHome for sqlx::PgPool {
    async fn write_embedding(
        &self,
        owner: &OwnerId,
        entity: proxiima_core::EmbeddableEntityRef,
        vectors: &[proxiima_core::SpaceVector],
    ) -> Result<i32, StorageError> {
        let mut tx = self
            .begin()
            .await
            .map_err(|err| StorageError::Unavailable(err.to_string()))?;
        // Under row security the write would see no entity and report "nothing
        // written", which a test would take for a seeded row.
        let sees_every_row: bool = sqlx::query_scalar(
            "SELECT rolsuper OR rolbypassrls FROM pg_catalog.pg_roles WHERE rolname = current_user",
        )
        .fetch_one(tx.as_mut())
        .await
        .map_err(crate::error::map_err)?;
        if !sees_every_row {
            return Err(StorageError::Internal(
                "an embedding is seeded through a pool only as a role that bypasses row security"
                    .into(),
            ));
        }
        let outcome =
            crate::verbs::fact_embeddings::insert_embedding(&mut tx, owner, entity, vectors)
                .await?;
        tx.commit()
            .await
            .map_err(|err| StorageError::Unavailable(err.to_string()))?;
        Ok(outcome.embedding_version)
    }
}

/// Write one new embedding version of `target` (a Fact, a Goal or a derived
/// memory) in the 1024-wide space of `model_id`, `chunks` unit vectors long,
/// through the write the embedding port makes
/// (`PgStorage::insert_embedding_version`): the head advances to the new
/// version, and an ineligible entity (forgotten, or not `owner`'s) writes
/// nothing. Returns the version written, `0` when nothing was.
///
/// # Errors
///
/// Storage errors from the write.
pub async fn create_embedding(
    home: &impl EmbeddingHome,
    owner: OwnerId,
    target: impl EmbeddingTarget,
    model_id: &str,
    chunks: usize,
) -> Result<i32, StorageError> {
    let width = proxiima_core::EmbeddingDim::D1024.width();
    create_embedding_of(
        home,
        owner,
        target,
        model_id,
        vec![unit_vector(width); chunks],
    )
    .await
}

/// [`create_embedding`] with the chunk vectors named by the test. Every
/// vector has the same width, one the product supports
/// ([`EmbeddingDim`](proxiima_core::EmbeddingDim)). The write is the one the
/// embedding write port makes once it has checked the job claim.
///
/// # Errors
///
/// `Internal` when no vector is given or the width is not a supported
/// dimension; otherwise the storage errors from the write.
pub async fn create_embedding_of(
    home: &impl EmbeddingHome,
    owner: OwnerId,
    target: impl EmbeddingTarget,
    model_id: &str,
    chunks: Vec<Vec<f32>>,
) -> Result<i32, StorageError> {
    let width = chunks.first().map_or(0, Vec::len);
    let dim = proxiima_core::EmbeddingDim::from_width(width).ok_or_else(|| {
        StorageError::Internal(format!("{width} is not a supported embedding width"))
    })?;
    let space = proxiima_core::EmbeddingSpace::new(model_id, dim);
    let vectors = chunks
        .into_iter()
        .map(|values| {
            proxiima_core::SpaceVector::new(space.clone(), values)
                .map_err(|err| StorageError::Internal(err.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    home.write_embedding(&owner, target.entity(), &vectors)
        .await
}

/// Queue `entity_id` for embedding in the 1024-wide space of `model_id`, the
/// way a write that makes it embeddable does
/// (`crate::verbs::fact_embeddings::enqueue_embedding_jobs_in_tx`).
///
/// # Errors
///
/// Storage errors from the write.
pub async fn enqueue_embedding_job(
    pg: &PgStorage,
    owner: OwnerId,
    entity_id: Uuid,
    model_id: &str,
) -> Result<(), StorageError> {
    let space = proxiima_core::EmbeddingSpace::new(model_id, proxiima_core::EmbeddingDim::D1024);
    let mut tx = pg.platform_transaction().await?;
    crate::verbs::fact_embeddings::enqueue_embedding_jobs_in_tx(
        &mut tx,
        owner.into_inner(),
        entity_id,
        [&space],
    )
    .await?;
    tx.commit()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))
}

/// A `core/agent-note-v1` Fact with defaults, written the way production
/// writes a typed Fact: the payload through the ordinary ingest, so the
/// admission stamps `sidecar_tables` and fills the sidecar and projection
/// rows. Name only what a test asserts on.
#[derive(Debug, Clone)]
pub struct TestNote {
    note: proxiima_core::AgentNoteV1,
    language: String,
    handle: Option<Uuid>,
}

impl TestNote {
    /// A note with no tags.
    #[must_use]
    pub fn new(title: &str, body: &str) -> Self {
        Self {
            note: proxiima_core::AgentNoteV1 {
                note_id: Uuid::now_v7(),
                title: title.to_owned(),
                body: body.to_owned(),
                tags: Vec::new(),
                idempotency_key: None,
            },
            language: proxiima_core::lexical_language::LEXICAL_LANGUAGE_DEPLOYMENT_DEFAULT
                .to_owned(),
            handle: None,
        }
    }

    /// Append a version to this series instead of starting a new one.
    #[must_use]
    pub fn handle(mut self, handle: Uuid) -> Self {
        self.handle = Some(handle);
        self
    }

    /// The note's tags.
    #[must_use]
    pub fn tags(mut self, tags: &[&str]) -> Self {
        self.note.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        self
    }

    /// The lexical language the write names for its search row.
    #[must_use]
    pub fn language(mut self, language: &str) -> Self {
        language.clone_into(&mut self.language);
        self
    }

    /// Write the note under `owner`.
    ///
    /// # Errors
    ///
    /// The storage error the write refuses with.
    pub async fn create(
        self,
        pg: &PgStorage,
        owner: OwnerId,
    ) -> Result<FactIngestOutcome, StorageError> {
        use proxiima_core::FactPayload as _;
        let mut command = FactWriteCommand::from_payload("test-note", &self.note);
        // `agent-note-v1` is a per-row language policy: the write names one.
        command.lexical_language = Some(self.language);
        command.handle = self.handle;
        let authorized = proxiima_core::verbs::fact_ingest::AuthorizedFactWrite::new_for_tests(
            OwnerWritePermit::new_for_tests(owner, AccessKind::Fact),
            command,
            proxiima_core::AgentNoteV1::sidecar_table().map(str::to_owned),
            Vec::new(),
        )
        .with_sidecar_payloads_for_tests(vec![proxiima_core::SidecarPayload::fact(self.note)]);
        pg.ingest_fact_with_typed_sidecar(&authorized, &[]).await
    }
}

impl TestMemory {
    /// Cite an object of `schema_id` with `content_hash`: the admission
    /// creates the blob, and [`FactIngestOutcome::cited_object_id`] names it.
    #[must_use]
    pub fn cites(mut self, schema_id: &str, content_hash: [u8; 32]) -> Self {
        self.command.citation = Some(
            proxiima_core::verbs::fact_ingest::CitationSpec::v1(schema_id, content_hash).into(),
        );
        self
    }
}

/// An Active `core/task-goal-v1` Goal with defaults, written through the
/// Goal writer ([`crate::verbs::goal_timeseries::write_goal`]): the head and
/// the first version land together.
#[derive(Debug, Clone)]
pub struct TestGoal {
    command: crate::verbs::goal_timeseries::GoalWriteCommand,
}

impl TestGoal {
    /// A goal named by its idempotency `request_id`, with no write-act.
    #[must_use]
    pub fn new(request_id: &str) -> Self {
        Self {
            command: crate::verbs::goal_timeseries::GoalWriteCommand {
                handle: None,
                schema_id: "core/task-goal-v1".to_owned(),
                title: "test goal".to_owned(),
                text: None,
                state: proxiima_core::verbs::goal_write::GoalState::Active,
                request_id: request_id.to_owned(),
                close_fact_t: None,
                motivated_by: Vec::new(),
                dependency_t: Vec::new(),
                evidence_t: Vec::new(),
                mint_write_act: false,
                write_act_t: None,
                parent_handle: None,
            },
        }
    }

    /// The goal's title.
    #[must_use]
    pub fn title(mut self, title: &str) -> Self {
        title.clone_into(&mut self.command.title);
        self
    }

    /// Write the goal under `owner`.
    ///
    /// # Errors
    ///
    /// The storage error the write refuses with.
    pub async fn create(
        self,
        pg: &PgStorage,
        owner: OwnerId,
    ) -> Result<crate::verbs::goal_timeseries::GoalWriteOutcome, StorageError> {
        let mut tx = pg.platform_transaction().await?;
        let outcome =
            crate::verbs::goal_timeseries::write_goal(&mut tx, &owner, writer(), &self.command, 1)
                .await?;
        tx.commit()
            .await
            .map_err(|err| StorageError::Unavailable(err.to_string()))?;
        Ok(outcome)
    }
}

/// A `core/agent-derivation-v1` Abstraction or Perspective with defaults,
/// written the way production writes a typed derived memory: the payload
/// through the typed ingest, so the admission stamps `sidecar_tables`, fills
/// the sidecar and projection rows, and records the origin edge. Name only
/// what a test asserts on.
#[derive(Debug, Clone)]
pub struct TestDerived {
    kind: MemoryKind,
    origin: EdgeEndpoint,
    derivation: proxiima_core::AgentDerivationV1,
}

impl TestDerived {
    /// An Abstraction grounded in the Fact `origin`.
    #[must_use]
    pub fn abstraction(origin: MemoryId) -> Self {
        Self::new(
            MemoryKind::Abstraction,
            EdgeEndpoint::memory(proxiima_core::EntityKind::Fact, origin),
        )
    }

    /// A Perspective over the Abstraction `origin`.
    #[must_use]
    pub fn perspective(origin: MemoryId) -> Self {
        Self::new(
            MemoryKind::Perspective,
            EdgeEndpoint::memory(proxiima_core::EntityKind::Abstraction, origin),
        )
    }

    fn new(kind: MemoryKind, origin: EdgeEndpoint) -> Self {
        Self {
            kind,
            origin,
            derivation: proxiima_core::AgentDerivationV1 {
                title: "test derivation".to_owned(),
                body: "test derivation".to_owned(),
                tags: Vec::new(),
                idempotency_key: None,
                source_memory_ids: Vec::new(),
                model_id: "test".to_owned(),
                client_name: "test".to_owned(),
                client_version: "1".to_owned(),
            },
        }
    }

    /// The derivation's tags.
    #[must_use]
    pub fn tags(mut self, tags: &[&str]) -> Self {
        self.derivation.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        self
    }

    /// Write the derived memory under `owner`.
    ///
    /// # Errors
    ///
    /// The storage error the write refuses with.
    pub async fn create(
        self,
        pg: &PgStorage,
        owner: OwnerId,
    ) -> Result<FactIngestOutcome, StorageError> {
        use proxiima_core::AbstractionPayload as _;
        let mut command = TestMemory::fact().command;
        command.schema_id = proxiima_core::AgentDerivationV1::schema_id();
        command.lexical_language =
            Some(proxiima_core::lexical_language::LEXICAL_LANGUAGE_DEPLOYMENT_DEFAULT.to_owned());
        command.kind = self.kind;
        command.additional_references = vec![self.origin];
        let payload = match self.kind {
            MemoryKind::Perspective => proxiima_core::SidecarPayload::perspective(self.derivation),
            _ => proxiima_core::SidecarPayload::abstraction(self.derivation),
        };
        let authorized = proxiima_core::verbs::fact_ingest::AuthorizedFactWrite::new_for_tests(
            OwnerWritePermit::new_for_tests(owner, AccessKind::Fact),
            command,
            None,
            Vec::new(),
        )
        .with_sidecar_payloads_for_tests(vec![payload]);
        pg.ingest_fact_with_typed_sidecar(&authorized, &[]).await
    }
}

/// Store `cursor` as `owner`'s cursor for `source`, the way a host checkpoint
/// does (`crate::verbs::source_cursors::store_source_cursor`). An existing
/// cursor for the same source is replaced.
///
/// # Errors
///
/// Storage errors from the write.
pub async fn create_source_cursor(
    pool: &sqlx::PgPool,
    owner: OwnerId,
    source: &str,
    cursor: Vec<u8>,
) -> Result<(), StorageError> {
    let permit = OwnerWritePermit::new_for_tests(owner, AccessKind::Fact);
    crate::verbs::source_cursors::store_source_cursor(
        pool,
        &permit,
        source,
        &proxiima_core::Cursor::from_bytes(cursor),
    )
    .await
}

/// Record `remote`, a memory of another store, as an origin of `memory` the
/// way a derive that names one does
/// (`crate::verbs::derive_append::record_external_edges`).
///
/// # Errors
///
/// Storage errors from the write.
pub async fn create_external_origin(
    pool: &sqlx::PgPool,
    memory: MemoryId,
    remote: Uuid,
) -> Result<(), StorageError> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    let outcome = crate::verbs::derive_append::DerivedOutcome {
        memory_id: memory,
        idempotent_replay: false,
        forgotten: false,
    };
    crate::verbs::derive_append::record_external_edges(&mut tx, &outcome, &[remote], &[]).await?;
    tx.commit()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))
}

/// Install `schema.table` as a memory sidecar of `spiice_id`, keyed to the
/// memory by `memory_key`, the way a spiice migration does
/// (`proxiima_core.install_sidecar`): the foreign key, the surface row, the
/// integrity triggers and owner row security. `platform` is a pool of the
/// platform role, which owns the table.
///
/// # Errors
///
/// Storage errors from the migration transaction or the installer.
pub async fn install_sidecar(
    platform: &sqlx::PgPool,
    schema: &str,
    table: &str,
    spiice_id: &str,
    memory_key: &str,
) -> Result<(), StorageError> {
    let mut conn = platform
        .acquire()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))?;
    let mut tx = crate::begin_migration_transaction(&mut conn).await?;
    sqlx::query("SELECT proxiima_core.install_sidecar($1, $2, $3, memory_key => $4)")
        .bind(schema)
        .bind(table)
        .bind(spiice_id)
        .bind(memory_key)
        .execute(tx.as_mut())
        .await
        .map_err(crate::error::map_err)?;
    tx.commit()
        .await
        .map_err(|err| StorageError::Unavailable(err.to_string()))
}
