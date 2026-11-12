-- Baseline: the core schema without the pgvector part. A store that keeps its
-- vectors in pgvector applies 0070_pgvector_local_vectors.sql after this file;
-- a store whose vectors live in a host index never does, and needs no `vector`
-- extension.
-- Mutable functions, triggers, policies and views live in ../definitions/.
-- The three lexical functions below stay here: generated columns and indexes
-- of this file depend on them, so they exist before any table does.
-- Immutable once released.

-- Migration role and extensions

DO $owner_rls_role$
DECLARE
    role_is_superuser boolean;
    role_bypasses_rls boolean;
BEGIN
    SELECT r.rolsuper, r.rolbypassrls
      INTO role_is_superuser, role_bypasses_rls
      FROM pg_roles AS r
     WHERE r.rolname = current_user;
    IF role_is_superuser OR role_bypasses_rls THEN
        RAISE EXCEPTION 'owner RLS migration role must be NOSUPERUSER NOBYPASSRLS';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM pg_class AS c
          JOIN pg_namespace AS n ON n.oid = c.relnamespace
         WHERE n.nspname = 'proxiima_core'
           AND c.relkind IN ('r', 'p')
           AND NOT pg_has_role(current_user, c.relowner, 'USAGE')
    ) THEN
        RAISE EXCEPTION 'owner RLS migration role does not own every composed table';
    END IF;
END
$owner_rls_role$;

CREATE EXTENSION IF NOT EXISTS btree_gin;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Schema and types

CREATE SCHEMA proxiima_core;

CREATE TYPE proxiima_core.access_ceiling AS ENUM (
    'none',
    'fact',
    'abstraction',
    'perspective',
    'goal'
);

CREATE TYPE proxiima_core.blob_upload_status AS ENUM (
    'pending',
    'completed',
    'aborted',
    'expired'
);

CREATE TYPE proxiima_core.embedding_job_status AS ENUM (
    'pending',
    'processing',
    'failed',
    'failed_permanent'
);

CREATE TYPE proxiima_core.goal_access AS ENUM (
    'none',
    'reads',
    'works'
);

CREATE TYPE proxiima_core.goal_change_kind AS ENUM (
    'set',
    'modify',
    'decompose_child',
    'pause',
    'resume',
    'abandon',
    'achieve'
);

CREATE TYPE proxiima_core.goal_pending_work AS ENUM (
    'keep',
    'discard'
);

CREATE TYPE proxiima_core.goal_publication_use AS ENUM (
    'read',
    'fork'
);

CREATE TYPE proxiima_core.goal_state AS ENUM (
    'Active',
    'Paused',
    'Achieved',
    'Abandoned'
);

CREATE TYPE proxiima_core.interpretation_subject_kind AS ENUM (
    'Fact',
    'Abstraction',
    'Perspective'
);

CREATE TYPE proxiima_core.membership_relation AS ENUM (
    'admin',
    'editor',
    'viewer',
    'ingest'
);

CREATE TYPE proxiima_core.memory_ceiling AS ENUM (
    'none',
    'fact',
    'abstraction',
    'perspective'
);

CREATE TYPE proxiima_core.memory_kind AS ENUM (
    'fact',
    'abstraction',
    'perspective'
);

CREATE TYPE proxiima_core.owner_kind AS ENUM (
    'personal',
    'group'
);

CREATE TYPE proxiima_core.owner_rls_class AS ENUM (
    'owner_id',
    'fk_parent',
    'ownerless',
    'memory_owner'
);

CREATE TYPE proxiima_core.pin_target_kind AS ENUM (
    'fact',
    'abstraction',
    'perspective',
    'goal'
);

CREATE TYPE proxiima_core.publication_record_state AS ENUM (
    'pending',
    'published'
);

CREATE TYPE proxiima_core.publication_state AS ENUM (
    'pending',
    'claimed',
    'published'
);

CREATE TYPE proxiima_core.sketch_kind AS ENUM (
    'fact',
    'abstraction',
    'perspective',
    'goal'
);

CREATE TYPE proxiima_core.task_priority AS ENUM (
    'Low',
    'Medium',
    'High'
);

COMMENT ON TYPE proxiima_core.owner_rls_class IS 'The four owner-RLS policy classes; a fifth is a new migration (docs/09 §Owner RLS).';

-- Lexical metadata and column functions

CREATE TABLE proxiima_core.lexical_default (
    singleton boolean DEFAULT true NOT NULL,
    config regconfig NOT NULL,
    CONSTRAINT lexical_default_singleton_check CHECK (singleton)
);

CREATE TABLE proxiima_core.lexical_languages (
    config regconfig NOT NULL
);

CREATE FUNCTION proxiima_core.lexical_config() RETURNS regconfig
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT config
     FROM proxiima_core.lexical_default
    WHERE singleton $$;

CREATE FUNCTION proxiima_core.lexical_join(VARIADIC parts text[]) RETURNS text
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    AS $$ SELECT NULLIF(concat_ws(' ', VARIADIC parts), '') $$;

CREATE FUNCTION proxiima_core.lexical_text_array(parts text[]) RETURNS text
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    AS $$ SELECT NULLIF(array_to_string(parts, ' '), '') $$;

-- Identity and group authority

CREATE TABLE proxiima_core.delegated_authority_grants (
    delegation_id uuid NOT NULL,
    subject_user_id uuid NOT NULL,
    owner_id uuid NOT NULL,
    tool_name text NOT NULL,
    action_name text,
    read_ceiling proxiima_core.memory_ceiling NOT NULL,
    write_ceiling proxiima_core.memory_ceiling NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    issued_at timestamp with time zone NOT NULL,
    revoked_at timestamp with time zone,
    revoked_by_user_id uuid,
    by uuid NOT NULL,
    via uuid,
    goal_access proxiima_core.goal_access NOT NULL,
    administers boolean NOT NULL,
    CONSTRAINT delegated_authority_grants_role_chk CHECK ((write_ceiling <= read_ceiling))
);

CREATE TABLE proxiima_core.group_edges (
    child_group_id uuid NOT NULL,
    parent_group_id uuid NOT NULL,
    relation proxiima_core.membership_relation DEFAULT 'viewer'::proxiima_core.membership_relation NOT NULL,
    child_group_kind proxiima_core.owner_kind DEFAULT 'group'::proxiima_core.owner_kind NOT NULL,
    parent_group_kind proxiima_core.owner_kind DEFAULT 'group'::proxiima_core.owner_kind NOT NULL,
    CONSTRAINT group_edges_child_kind_chk CHECK ((child_group_kind = 'group'::proxiima_core.owner_kind)),
    CONSTRAINT group_edges_parent_kind_chk CHECK ((parent_group_kind = 'group'::proxiima_core.owner_kind))
);

CREATE TABLE proxiima_core.group_lock_request (
    request_id uuid NOT NULL,
    left_group_id uuid NOT NULL,
    right_group_id uuid,
    member_is_group boolean DEFAULT false NOT NULL
);

CREATE TABLE proxiima_core.group_memberships (
    group_id uuid NOT NULL,
    member_user_id uuid NOT NULL,
    relation proxiima_core.membership_relation NOT NULL,
    group_kind proxiima_core.owner_kind DEFAULT 'group'::proxiima_core.owner_kind NOT NULL,
    CONSTRAINT group_memberships_group_kind_chk CHECK ((group_kind = 'group'::proxiima_core.owner_kind))
);

CREATE TABLE proxiima_core.group_role_projection (
    member_user_id uuid NOT NULL,
    group_id uuid NOT NULL,
    read_ceiling proxiima_core.memory_ceiling NOT NULL,
    write_ceiling proxiima_core.memory_ceiling NOT NULL,
    manage boolean NOT NULL,
    goal_access proxiima_core.goal_access NOT NULL,
    administers boolean NOT NULL,
    CONSTRAINT group_role_projection_role_chk CHECK ((write_ceiling <= read_ceiling))
);

CREATE TABLE proxiima_core.owner_rls_epoch (
    singleton boolean DEFAULT true NOT NULL,
    epoch integer NOT NULL,
    CONSTRAINT owner_rls_epoch_singleton_check CHECK (singleton)
);

CREATE TABLE proxiima_core.owners (
    owner_id uuid NOT NULL,
    kind proxiima_core.owner_kind NOT NULL
);

COMMENT ON TABLE proxiima_core.owner_rls_epoch IS 'Non-optional marker proving that the owner-RLS migration and complete policy census ran.';

-- Memory, content and provenance

CREATE TABLE proxiima_core.blob (
    blob_id uuid DEFAULT uuidv7() NOT NULL,
    owner_id uuid NOT NULL,
    schema_id text NOT NULL,
    content_hash bytea NOT NULL,
    CONSTRAINT blob_hash_len_chk CHECK ((octet_length(content_hash) = 32))
);

CREATE TABLE proxiima_core.blob_uploads (
    upload_id uuid DEFAULT uuidv7() NOT NULL,
    owner_id uuid NOT NULL,
    bucket text NOT NULL,
    object_key text NOT NULL,
    filename text NOT NULL,
    mime text NOT NULL,
    expected_byte_len bigint NOT NULL,
    status proxiima_core.blob_upload_status DEFAULT 'pending'::proxiima_core.blob_upload_status NOT NULL,
    blob_id uuid,
    sha256 bytea,
    etag text,
    error_message text,
    expires_at timestamp with time zone NOT NULL,
    completed_at timestamp with time zone,
    aborted_at timestamp with time zone,
    mounted_from_upload_id uuid,
    content_hash bytea,
    object_store text NOT NULL,
    CONSTRAINT blob_uploads_content_hash_chk CHECK (((content_hash IS NULL) OR (octet_length(content_hash) = 32))),
    CONSTRAINT blob_uploads_mount_not_self_chk CHECK ((mounted_from_upload_id IS DISTINCT FROM upload_id)),
    CONSTRAINT blob_uploads_object_store_chk CHECK (((object_store ~ '^[a-z][a-z0-9]*:'::text) AND (octet_length(object_store) <= 512)))
);

CREATE TABLE proxiima_core.closed_handle (
    handle uuid NOT NULL,
    closed_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE proxiima_core.cold_purge_pending (
    object_key text NOT NULL,
    owner_id uuid,
    enqueued_at timestamp with time zone DEFAULT now() NOT NULL,
    object_store text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    next_attempt_at timestamp with time zone DEFAULT now() NOT NULL,
    last_attempt_at timestamp with time zone,
    last_error text,
    CONSTRAINT cold_purge_pending_attempts_chk CHECK ((attempts >= 0)),
    CONSTRAINT cold_purge_pending_last_error_chk CHECK (((last_error IS NULL) OR (octet_length(last_error) <= 1024))),
    CONSTRAINT cold_purge_pending_object_store_chk CHECK (((object_store ~ '^[a-z][a-z0-9]*:'::text) AND (octet_length(object_store) <= 512)))
);

CREATE TABLE proxiima_core.content (
    content_id uuid DEFAULT uuidv7() NOT NULL,
    owner_id uuid NOT NULL,
    schema_id text NOT NULL,
    content_hash bytea NOT NULL,
    CONSTRAINT content_hash_len_chk CHECK ((octet_length(content_hash) = 32))
);

CREATE TABLE proxiima_core.erased_pin_target (
    t uuid NOT NULL,
    kind proxiima_core.pin_target_kind NOT NULL
);

CREATE TABLE proxiima_core.ingest_keys (
    owner_id uuid NOT NULL,
    source_id text NOT NULL,
    ingest_key text NOT NULL,
    t uuid NOT NULL
);

CREATE TABLE proxiima_core.memory (
    handle uuid NOT NULL,
    t uuid NOT NULL,
    kind proxiima_core.memory_kind NOT NULL,
    owner_id uuid NOT NULL,
    schema_id text NOT NULL,
    source_id text,
    ingest_key text,
    blob_id uuid,
    content_id uuid,
    origins uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    refs uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    sidecar_tables text[] DEFAULT '{}'::text[] NOT NULL,
    goal_refs uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    occurred_at timestamp with time zone,
    by uuid NOT NULL,
    via uuid,
    lexical_language regconfig DEFAULT proxiima_core.lexical_config() NOT NULL,
    forgotten_at timestamp with time zone,
    guided_by uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    forked_from uuid,
    reads uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    CONSTRAINT memory_ap_content_chk CHECK (((kind = 'fact'::proxiima_core.memory_kind) OR (content_id IS NOT NULL))),
    CONSTRAINT memory_blob_fa_chk CHECK (((blob_id IS NULL) OR (kind = ANY (ARRAY['fact'::proxiima_core.memory_kind, 'abstraction'::proxiima_core.memory_kind])))),
    CONSTRAINT memory_fact_occurred_at_chk CHECK (((kind = 'fact'::proxiima_core.memory_kind) = (occurred_at IS NOT NULL))),
    CONSTRAINT memory_fact_origins_chk CHECK (((kind <> 'fact'::proxiima_core.memory_kind) OR (origins = '{}'::uuid[]))),
    CONSTRAINT memory_fact_source_chk CHECK ((((kind = 'fact'::proxiima_core.memory_kind) AND ((source_id IS NULL) = (ingest_key IS NULL))) OR ((kind <> 'fact'::proxiima_core.memory_kind) AND (source_id IS NULL) AND (ingest_key IS NULL)))),
    CONSTRAINT memory_goal_refs_no_null_chk CHECK ((array_position(goal_refs, NULL::uuid) IS NULL)),
    CONSTRAINT memory_guided_by_no_null_chk CHECK ((array_position(guided_by, NULL::uuid) IS NULL)),
    CONSTRAINT memory_origins_no_null_chk CHECK ((array_position(origins, NULL::uuid) IS NULL)),
    CONSTRAINT memory_reads_no_null_chk CHECK ((array_position(reads, NULL::uuid) IS NULL)),
    CONSTRAINT memory_refs_no_null_chk CHECK ((array_position(refs, NULL::uuid) IS NULL)),
    CONSTRAINT memory_sidecar_tables_no_null_chk CHECK ((array_position(sidecar_tables, NULL::text) IS NULL))
);

CREATE TABLE proxiima_core.memory_external_origin (
    t uuid NOT NULL,
    memory uuid NOT NULL,
    edge_kind text NOT NULL,
    CONSTRAINT memory_external_origin_edge_kind_chk CHECK ((edge_kind = ANY (ARRAY['origin'::text, 'reference'::text, 'read'::text])))
);

CREATE TABLE proxiima_core.memory_head (
    handle uuid NOT NULL,
    kind proxiima_core.memory_kind NOT NULL,
    schema_id text NOT NULL,
    owner_id uuid NOT NULL,
    t uuid NOT NULL,
    publication_sequence bigint DEFAULT 0 NOT NULL,
    authority_generation bigint DEFAULT 0 NOT NULL,
    CONSTRAINT memory_head_authority_generation_chk CHECK ((authority_generation >= 0)),
    CONSTRAINT memory_head_publication_sequence_chk CHECK ((publication_sequence >= 0))
);

CREATE TABLE proxiima_core.source_cursors (
    owner_id uuid NOT NULL,
    source text NOT NULL,
    cursor bytea NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

COMMENT ON TABLE proxiima_core.erased_pin_target IS 'Internal historical identity witness for a hard-erased pin target. It carries no owner or readable payload and is excluded from export.';

-- Core sidecars and search

CREATE TABLE proxiima_core.agent_derivation_v1 (
    t uuid NOT NULL,
    title text NOT NULL,
    body text NOT NULL,
    tags text[] DEFAULT '{}'::text[] NOT NULL,
    idempotency_key text,
    source_memory_ids uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    model_id text NOT NULL,
    client_name text NOT NULL,
    client_version text NOT NULL,
    embed_text text GENERATED ALWAYS AS (proxiima_core.lexical_join(VARIADIC ARRAY[NULLIF(title, ''::text), NULLIF(body, ''::text), proxiima_core.lexical_text_array(tags)])) STORED
);

CREATE TABLE proxiima_core.agent_note_v1 (
    t uuid NOT NULL,
    note_id uuid NOT NULL,
    title text NOT NULL,
    body text NOT NULL,
    tags text[] DEFAULT '{}'::text[] NOT NULL,
    idempotency_key text,
    embed_text text GENERATED ALWAYS AS (proxiima_core.lexical_join(VARIADIC ARRAY[NULLIF(title, ''::text), NULLIF(body, ''::text), proxiima_core.lexical_text_array(tags)])) STORED
);

CREATE TABLE proxiima_core.spiice_surface (
    table_name text NOT NULL,
    spiice_id text NOT NULL,
    CONSTRAINT spiice_surface_spiice_id_chk CHECK ((spiice_id <> ''::text)),
    CONSTRAINT spiice_surface_qualified_chk CHECK (((table_name ~~ '%.%'::text) AND (table_name = lower(table_name))))
);

CREATE TABLE proxiima_core.interpretation_v1 (
    t uuid NOT NULL,
    claim text NOT NULL,
    confidence smallint NOT NULL,
    subject_memory_ids uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    subject_kinds proxiima_core.interpretation_subject_kind[] DEFAULT '{}'::proxiima_core.interpretation_subject_kind[] NOT NULL,
    model_id text NOT NULL,
    client_name text NOT NULL,
    client_version text NOT NULL,
    embed_text text GENERATED ALWAYS AS (NULLIF(claim, ''::text)) STORED
);

CREATE TABLE proxiima_core.sketch (
    t uuid NOT NULL,
    owner_id uuid NOT NULL,
    kind proxiima_core.sketch_kind NOT NULL,
    text text NOT NULL,
    CONSTRAINT sketch_text_nonblank_chk CHECK ((length(btrim(text)) > 0))
);

CREATE TABLE proxiima_core.task_goal_v1 (
    t uuid NOT NULL,
    due_at timestamp with time zone,
    priority proxiima_core.task_priority
);

CREATE TABLE proxiima_core.utterance_v1 (
    t uuid NOT NULL,
    speaker text NOT NULL,
    conversation_id text NOT NULL,
    text text NOT NULL,
    embed_text text GENERATED ALWAYS AS (NULLIF(text, ''::text)) STORED
);

CREATE TABLE proxiima_core.write_act_v1 (
    t uuid NOT NULL,
    episode_id uuid NOT NULL
);

COMMENT ON TABLE proxiima_core.spiice_surface IS 'Declared sidecar surfaces, one row per (table, spiice). Populated by each spiice migration; memory.sidecar_tables is constrained to be a subset. Spiice #0 (core) owns the proxiima_core.* rows and is non-removable.';

-- Embedding jobs and vectors

CREATE TABLE proxiima_core.embedding_heads (
    entity_id uuid NOT NULL,
    model_id text NOT NULL,
    embedding_version integer NOT NULL,
    owner_id uuid NOT NULL,
    dim smallint NOT NULL,
    CONSTRAINT embedding_heads_dim_lane_chk CHECK ((dim = ANY (ARRAY[384, 768, 1024, 1536, 2048, 3072])))
);

CREATE TABLE proxiima_core.embedding_jobs (
    job_id uuid DEFAULT uuidv7() NOT NULL,
    entity_id uuid NOT NULL,
    model_id text NOT NULL,
    owner_id uuid NOT NULL,
    status proxiima_core.embedding_job_status DEFAULT 'pending'::proxiima_core.embedding_job_status NOT NULL,
    claimed_at timestamp with time zone,
    claim_token uuid,
    last_error text,
    dim smallint NOT NULL,
    CONSTRAINT embedding_job_processing_claim_chk CHECK (((status = 'processing'::proxiima_core.embedding_job_status) = ((claimed_at IS NOT NULL) AND (claim_token IS NOT NULL)))),
    CONSTRAINT embedding_jobs_dim_lane_chk CHECK ((dim = ANY (ARRAY[384, 768, 1024, 1536, 2048, 3072])))
);

CREATE TABLE proxiima_core.embeddings (
    entity_id uuid NOT NULL,
    model_id text NOT NULL,
    embedding_version integer DEFAULT 1 NOT NULL,
    owner_id uuid NOT NULL,
    dim smallint NOT NULL,
    chunk_ordinal integer DEFAULT 0 NOT NULL,
    CONSTRAINT embeddings_chunk_ordinal_chk CHECK ((chunk_ordinal >= 0)),
    CONSTRAINT embeddings_dim_lane_chk CHECK ((dim = ANY (ARRAY[384, 768, 1024, 1536, 2048, 3072])))
);

-- Goals and replay declarations

CREATE TABLE proxiima_core.goal (
    handle uuid NOT NULL,
    t uuid NOT NULL,
    owner_id uuid NOT NULL,
    title text NOT NULL,
    state proxiima_core.goal_state NOT NULL,
    request_id text NOT NULL,
    close_fact_t uuid,
    assignment_t uuid,
    dependency_t uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    evidence_t uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    write_act_t uuid,
    text text,
    by uuid NOT NULL,
    via uuid,
    parent_handle uuid,
    motivated_by uuid[] DEFAULT '{}'::uuid[] NOT NULL,
    schema_id text NOT NULL,
    CONSTRAINT goal_arrays_no_null_chk CHECK (((array_position(dependency_t, NULL::uuid) IS NULL) AND (array_position(evidence_t, NULL::uuid) IS NULL))),
    CONSTRAINT goal_motivated_by_no_null_chk CHECK ((array_position(motivated_by, NULL::uuid) IS NULL)),
    CONSTRAINT goal_parent_not_self_chk CHECK (((parent_handle IS NULL) OR (parent_handle <> handle))),
    CONSTRAINT goal_terminal_close_chk CHECK (((state = ANY (ARRAY['Achieved'::proxiima_core.goal_state, 'Abandoned'::proxiima_core.goal_state])) = (close_fact_t IS NOT NULL))),
    CONSTRAINT goal_text_nonblank_chk CHECK (((text IS NULL) OR (length(btrim(text, E' \t\n\r\013\f'::text)) > 0))),
    CONSTRAINT goal_title_nonblank_chk CHECK ((length(btrim(title)) > 0))
);

CREATE TABLE proxiima_core.goal_change_v1 (
    t uuid NOT NULL,
    series uuid NOT NULL,
    goal_t uuid NOT NULL,
    prior_t uuid,
    revision integer NOT NULL,
    state proxiima_core.goal_state NOT NULL,
    change_kind proxiima_core.goal_change_kind NOT NULL,
    parent_t uuid,
    pending proxiima_core.goal_pending_work,
    CONSTRAINT goal_change_v1_parent_chk CHECK (((change_kind = 'decompose_child'::proxiima_core.goal_change_kind) = (parent_t IS NOT NULL))),
    CONSTRAINT goal_change_v1_pending_chk CHECK (((change_kind = 'pause'::proxiima_core.goal_change_kind) = (pending IS NOT NULL))),
    CONSTRAINT goal_change_v1_revision_chk CHECK ((revision >= 1))
);

CREATE TABLE proxiima_core.goal_head (
    handle uuid NOT NULL,
    schema_id text NOT NULL,
    owner_id uuid NOT NULL,
    t uuid NOT NULL,
    authority_generation bigint DEFAULT 0 NOT NULL,
    CONSTRAINT goal_head_authority_generation_chk CHECK ((authority_generation >= 0))
);

CREATE TABLE proxiima_core.goal_replay_declaration (
    goal_t uuid NOT NULL,
    declaration jsonb NOT NULL,
    edge_count integer NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    owner_id uuid NOT NULL,
    request_id text NOT NULL,
    CONSTRAINT goal_replay_declaration_object_chk CHECK ((jsonb_typeof(declaration) = 'object'::text)),
    CONSTRAINT goal_replay_edge_count_chk CHECK ((edge_count >= 0))
);

-- Publication and installation

CREATE TABLE proxiima_core.installation (
    singleton boolean DEFAULT true NOT NULL,
    installation_id uuid DEFAULT gen_random_uuid() NOT NULL,
    CONSTRAINT installation_singleton_check CHECK (singleton)
);

CREATE TABLE proxiima_core.publication_consumer (
    name text NOT NULL,
    registered_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT publication_consumer_name_chk CHECK ((name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'::text))
);

CREATE TABLE proxiima_core.publication_delivery (
    t uuid NOT NULL,
    consumer text NOT NULL,
    state proxiima_core.publication_state DEFAULT 'pending'::proxiima_core.publication_state NOT NULL,
    claim_token uuid,
    claimed_by text,
    lease_expires_at timestamp with time zone,
    attempts integer DEFAULT 0 NOT NULL,
    published_at timestamp with time zone,
    CONSTRAINT publication_delivery_attempts_chk CHECK ((attempts >= 0)),
    CONSTRAINT publication_delivery_claim_chk CHECK (((state = 'claimed'::proxiima_core.publication_state) = ((claim_token IS NOT NULL) AND (lease_expires_at IS NOT NULL) AND (claimed_by IS NOT NULL)))),
    CONSTRAINT publication_delivery_published_chk CHECK (((state = 'published'::proxiima_core.publication_state) = (published_at IS NOT NULL)))
);

CREATE TABLE proxiima_core.publication_origin (
    t uuid NOT NULL,
    original_owner_id uuid NOT NULL,
    source_id text
);

CREATE TABLE proxiima_core.publication_outbox (
    t uuid NOT NULL,
    owner_id uuid NOT NULL,
    schema_id text NOT NULL,
    schema_version integer NOT NULL,
    event_type text NOT NULL,
    event_id text NOT NULL,
    envelope bytea NOT NULL,
    envelope_digest bytea NOT NULL,
    state proxiima_core.publication_record_state DEFAULT 'pending'::proxiima_core.publication_record_state NOT NULL,
    published_at timestamp with time zone,
    published_stream text,
    published_seq bigint,
    captured_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT publication_outbox_digest_len_chk CHECK ((octet_length(envelope_digest) = 32)),
    CONSTRAINT publication_outbox_published_chk CHECK (((state = 'published'::proxiima_core.publication_record_state) = (published_at IS NOT NULL)))
);

COMMENT ON TABLE proxiima_core.installation IS 'Payload-free deployment identity, minted at install and never updated. Stamped on published broker messages so retained-copy cleanup can prove a message is this installation''s before reading an absent origin row as a revocation.';

COMMENT ON TABLE proxiima_core.publication_origin IS 'Payload-free immutable original publication identity. Lifecycle erasure may delete the row; payload and broker delivery state remain exclusively in publication_outbox.';

-- Export fence, lexical configuration and store-wide records

CREATE TABLE proxiima_core.export_erase_fence (
    singleton boolean DEFAULT true NOT NULL,
    CONSTRAINT export_erase_fence_empty_chk CHECK (false)
);

COMMENT ON TABLE proxiima_core.export_erase_fence IS 'Never holds a row (CHECK false). Owner export locks it IN SHARE MODE before its snapshot and erase IN ROW EXCLUSIVE MODE, so exports and erases exclude each other without a session lock.';

CREATE UNLOGGED TABLE proxiima_core.maintenance_lease (
    name text NOT NULL,
    holder uuid NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    CONSTRAINT maintenance_lease_name_chk CHECK ((name = ANY (ARRAY['storage'::text, 'embedding'::text])))
);

COMMENT ON TABLE proxiima_core.maintenance_lease IS 'The holder of each maintenance singleton (storage, embedding) and when its lease expires. A pass takes a missing or expired lease, renews its own, and deletes it when done; an unrenewed lease expires.';

CREATE TABLE proxiima_core.owner_follows (
    table_name text NOT NULL,
    CONSTRAINT owner_follows_qualified_chk CHECK (((table_name ~~ '%.%'::text) AND (table_name = lower(table_name))))
);

COMMENT ON TABLE proxiima_core.owner_follows IS 'Follows sidecar tables, one row per schema-qualified table. Written by the host from pg_sidecar!(owner: follows) through sync_owner_follows; spiices never write it. A listed table gets the host follows owner RLS from install_owner_rls whatever class it is listed in.';

CREATE TABLE proxiima_core.publication_backlog (
    singleton boolean DEFAULT true NOT NULL,
    pending bigint NOT NULL,
    measured_at timestamp with time zone NOT NULL,
    CONSTRAINT publication_backlog_pending_chk CHECK ((pending >= 0)),
    CONSTRAINT publication_backlog_singleton_chk CHECK (singleton)
);

COMMENT ON TABLE proxiima_core.publication_backlog IS 'An estimate of how many outbox records are not yet published, and when the publisher measured it. A listenable write trusts a fresh estimate below the pending ceiling instead of probing the backlog; an old one sends it to the exact probe.';

CREATE TABLE proxiima_core.publication_counter (
    handle uuid NOT NULL,
    owner_id uuid NOT NULL,
    sequence bigint NOT NULL,
    CONSTRAINT publication_counter_sequence_chk CHECK ((sequence >= 0))
);

CREATE TABLE proxiima_core.store_identity (
    singleton boolean DEFAULT true NOT NULL,
    store_number integer NOT NULL,
    CONSTRAINT store_identity_singleton_check CHECK (singleton),
    CONSTRAINT store_identity_store_number_check CHECK (((store_number >= 0) AND (store_number <= 65535)))
);

CREATE TABLE proxiima_core.vector_index_debt (
    entity_id uuid NOT NULL,
    model_id text NOT NULL,
    dim smallint NOT NULL,
    owner_id uuid NOT NULL,
    op text NOT NULL,
    enqueued_at timestamp with time zone DEFAULT now() NOT NULL,
    xact xid8 DEFAULT pg_current_xact_id() NOT NULL,
    CONSTRAINT vector_index_debt_dim_lane_chk CHECK ((dim = ANY (ARRAY[384, 768, 1024, 1536, 2048, 3072]))),
    CONSTRAINT vector_index_debt_op_chk CHECK ((op = ANY (ARRAY['delete'::text, 'owner'::text])))
);

COMMENT ON TABLE proxiima_core.vector_index_debt IS 'What a store owes its host vector index: delete the key''s chunks, or record owner_id as their owner. Written by the embedding_heads trigger in the head change''s transaction; deleted once applied.';

-- Primary, unique and check constraints

ALTER TABLE ONLY proxiima_core.agent_derivation_v1
    ADD CONSTRAINT agent_derivation_v1_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.agent_note_v1
    ADD CONSTRAINT agent_note_v1_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.blob
    ADD CONSTRAINT blob_owner_id_schema_id_content_hash_key UNIQUE (owner_id, schema_id, content_hash);

ALTER TABLE ONLY proxiima_core.blob
    ADD CONSTRAINT blob_pkey PRIMARY KEY (blob_id);

ALTER TABLE ONLY proxiima_core.blob_uploads
    ADD CONSTRAINT blob_uploads_pkey PRIMARY KEY (upload_id);

ALTER TABLE ONLY proxiima_core.closed_handle
    ADD CONSTRAINT closed_handle_pkey PRIMARY KEY (handle);

ALTER TABLE ONLY proxiima_core.cold_purge_pending
    ADD CONSTRAINT cold_purge_pending_pkey PRIMARY KEY (object_store, object_key);

ALTER TABLE ONLY proxiima_core.content
    ADD CONSTRAINT content_owner_id_schema_id_content_hash_key UNIQUE (owner_id, schema_id, content_hash);

ALTER TABLE ONLY proxiima_core.content
    ADD CONSTRAINT content_pkey PRIMARY KEY (content_id);

ALTER TABLE ONLY proxiima_core.delegated_authority_grants
    ADD CONSTRAINT delegated_authority_grants_pkey PRIMARY KEY (delegation_id);

ALTER TABLE ONLY proxiima_core.embedding_heads
    ADD CONSTRAINT embedding_heads_pkey PRIMARY KEY (entity_id, model_id, dim);

ALTER TABLE ONLY proxiima_core.embedding_jobs
    ADD CONSTRAINT embedding_jobs_owner_id_entity_id_model_id_dim_key UNIQUE (owner_id, entity_id, model_id, dim);

ALTER TABLE ONLY proxiima_core.embedding_jobs
    ADD CONSTRAINT embedding_jobs_pkey PRIMARY KEY (job_id);

ALTER TABLE ONLY proxiima_core.embeddings
    ADD CONSTRAINT embeddings_pkey PRIMARY KEY (entity_id, model_id, dim, embedding_version, chunk_ordinal);

ALTER TABLE ONLY proxiima_core.erased_pin_target
    ADD CONSTRAINT erased_pin_target_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.export_erase_fence
    ADD CONSTRAINT export_erase_fence_pkey PRIMARY KEY (singleton);

ALTER TABLE ONLY proxiima_core.spiice_surface
    ADD CONSTRAINT spiice_surface_pkey PRIMARY KEY (table_name);

ALTER TABLE ONLY proxiima_core.goal_change_v1
    ADD CONSTRAINT goal_change_v1_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.goal_head
    ADD CONSTRAINT goal_head_handle_owner_key UNIQUE (handle, owner_id);

ALTER TABLE ONLY proxiima_core.goal_head
    ADD CONSTRAINT goal_head_pkey PRIMARY KEY (handle);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_owner_id_request_id_key UNIQUE (owner_id, request_id);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_pkey PRIMARY KEY (handle, t);

ALTER TABLE ONLY proxiima_core.goal_replay_declaration
    ADD CONSTRAINT goal_replay_declaration_pkey PRIMARY KEY (owner_id, request_id);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_t_key UNIQUE (t);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_t_owner_key UNIQUE (t, owner_id);

ALTER TABLE ONLY proxiima_core.group_edges
    ADD CONSTRAINT group_edges_pkey PRIMARY KEY (child_group_id);

ALTER TABLE ONLY proxiima_core.group_lock_request
    ADD CONSTRAINT group_lock_request_pkey PRIMARY KEY (request_id);

ALTER TABLE ONLY proxiima_core.group_memberships
    ADD CONSTRAINT group_memberships_pkey PRIMARY KEY (group_id, member_user_id, relation);

ALTER TABLE ONLY proxiima_core.group_role_projection
    ADD CONSTRAINT group_role_projection_pkey PRIMARY KEY (member_user_id, group_id);

ALTER TABLE ONLY proxiima_core.ingest_keys
    ADD CONSTRAINT ingest_keys_pkey PRIMARY KEY (owner_id, source_id, ingest_key);

ALTER TABLE ONLY proxiima_core.installation
    ADD CONSTRAINT installation_pkey PRIMARY KEY (singleton);

ALTER TABLE ONLY proxiima_core.interpretation_v1
    ADD CONSTRAINT interpretation_v1_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.lexical_default
    ADD CONSTRAINT lexical_default_pkey PRIMARY KEY (singleton);

ALTER TABLE ONLY proxiima_core.lexical_languages
    ADD CONSTRAINT lexical_languages_pkey PRIMARY KEY (config);

ALTER TABLE ONLY proxiima_core.maintenance_lease
    ADD CONSTRAINT maintenance_lease_pkey PRIMARY KEY (name);

ALTER TABLE ONLY proxiima_core.memory_external_origin
    ADD CONSTRAINT memory_external_origin_pkey PRIMARY KEY (t, memory);

ALTER TABLE ONLY proxiima_core.memory_head
    ADD CONSTRAINT memory_head_pkey PRIMARY KEY (handle);

ALTER TABLE ONLY proxiima_core.memory
    ADD CONSTRAINT memory_pkey PRIMARY KEY (handle, t);

ALTER TABLE ONLY proxiima_core.memory
    ADD CONSTRAINT memory_t_key UNIQUE (t);

ALTER TABLE ONLY proxiima_core.owner_follows
    ADD CONSTRAINT owner_follows_pkey PRIMARY KEY (table_name);

ALTER TABLE ONLY proxiima_core.owner_rls_epoch
    ADD CONSTRAINT owner_rls_epoch_pkey PRIMARY KEY (singleton);

ALTER TABLE ONLY proxiima_core.owners
    ADD CONSTRAINT owners_owner_id_kind_key UNIQUE (owner_id, kind);

ALTER TABLE ONLY proxiima_core.owners
    ADD CONSTRAINT owners_pkey PRIMARY KEY (owner_id);

ALTER TABLE ONLY proxiima_core.publication_backlog
    ADD CONSTRAINT publication_backlog_pkey PRIMARY KEY (singleton);

ALTER TABLE ONLY proxiima_core.publication_consumer
    ADD CONSTRAINT publication_consumer_pkey PRIMARY KEY (name);

ALTER TABLE ONLY proxiima_core.publication_counter
    ADD CONSTRAINT publication_counter_pkey PRIMARY KEY (handle);

ALTER TABLE ONLY proxiima_core.publication_delivery
    ADD CONSTRAINT publication_delivery_pkey PRIMARY KEY (t, consumer);

ALTER TABLE ONLY proxiima_core.publication_origin
    ADD CONSTRAINT publication_origin_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.publication_outbox
    ADD CONSTRAINT publication_outbox_event_id_key UNIQUE (event_id);

ALTER TABLE ONLY proxiima_core.publication_outbox
    ADD CONSTRAINT publication_outbox_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.sketch
    ADD CONSTRAINT sketch_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.source_cursors
    ADD CONSTRAINT source_cursors_pkey PRIMARY KEY (owner_id, source);

ALTER TABLE ONLY proxiima_core.store_identity
    ADD CONSTRAINT store_identity_pkey PRIMARY KEY (singleton);

ALTER TABLE ONLY proxiima_core.task_goal_v1
    ADD CONSTRAINT task_goal_v1_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.utterance_v1
    ADD CONSTRAINT utterance_v1_pkey PRIMARY KEY (t);

ALTER TABLE ONLY proxiima_core.vector_index_debt
    ADD CONSTRAINT vector_index_debt_pkey PRIMARY KEY (entity_id, model_id, dim);

ALTER TABLE ONLY proxiima_core.write_act_v1
    ADD CONSTRAINT write_act_v1_pkey PRIMARY KEY (t);

-- Generated search projection (projection.rs)

CREATE TABLE proxiima_core.projection (
    memory_id        uuid      NOT NULL
                     REFERENCES proxiima_core.memory (t) ON DELETE CASCADE,
    schema_id        text      NOT NULL,
    owner_id         uuid      NOT NULL
                     REFERENCES proxiima_core.owners (owner_id),
    search_tsv       tsvector  NOT NULL,
    tag              text[]    NOT NULL DEFAULT '{}',
    lexical_language regconfig NOT NULL DEFAULT proxiima_core.lexical_config()
                     REFERENCES proxiima_core.lexical_languages (config),
    PRIMARY KEY (memory_id, schema_id)
);

-- Retention: how long one owner keeps one schema's memory, one row per owner
-- and schema (docs/data-and-erasure.md). An absent row keeps for good.
-- Owner export carries the rows and owner erase takes them, as declared on
-- the kernel surface.

CREATE TABLE proxiima_core.retention (
    owner_id uuid NOT NULL REFERENCES proxiima_core.owners(owner_id),
    schema_id text NOT NULL,
    keep_days integer NOT NULL,
    CONSTRAINT retention_pkey PRIMARY KEY (owner_id, schema_id),
    CONSTRAINT retention_keep_days_chk CHECK (keep_days BETWEEN 1 AND 36500)
);

-- Goal motivation. The source-owned array is `goal.motivated_by`; this scalar
-- projection is maintained by triggers solely for selective reverse reads;
-- callers never write it directly.

CREATE TABLE proxiima_core.goal_motivation_index (
    goal_t uuid NOT NULL REFERENCES proxiima_core.goal(t) ON DELETE CASCADE,
    perspective_t uuid NOT NULL,
    owner_id uuid NOT NULL REFERENCES proxiima_core.owners(owner_id),
    PRIMARY KEY (goal_t, perspective_t)
);

-- Goal collaboration: bounded audience routes over one durable owner. A role
-- is a read and a write ceiling over the memory ladder, what it may do with
-- Goals, whether it administers the owner and whether it manages the group.

CREATE TABLE proxiima_core.goal_access_grant (
    goal_handle uuid NOT NULL REFERENCES proxiima_core.goal_head(handle) ON DELETE CASCADE,
    subject_id uuid NOT NULL REFERENCES proxiima_core.owners(owner_id) ON DELETE CASCADE,
    read_ceiling proxiima_core.memory_ceiling NOT NULL,
    write_ceiling proxiima_core.memory_ceiling NOT NULL,
    manage boolean NOT NULL,
    controller_generation bigint NOT NULL CHECK (controller_generation >= 0),
    expires_at timestamptz,
    granted_by uuid NOT NULL,
    goal_access proxiima_core.goal_access NOT NULL,
    administers boolean NOT NULL,
    PRIMARY KEY (goal_handle, subject_id),
    CONSTRAINT goal_access_grant_role_chk CHECK (write_ceiling <= read_ceiling),
    CONSTRAINT goal_access_grant_no_administers_chk CHECK (NOT administers)
);

CREATE TABLE proxiima_core.goal_memory_publication (
    goal_handle uuid NOT NULL REFERENCES proxiima_core.goal_head(handle) ON DELETE CASCADE,
    memory_t uuid NOT NULL REFERENCES proxiima_core.memory(t) ON DELETE CASCADE,
    source_owner uuid NOT NULL REFERENCES proxiima_core.owners(owner_id) ON DELETE CASCADE,
    source_generation bigint NOT NULL CHECK (source_generation >= 0),
    controller_generation bigint NOT NULL CHECK (controller_generation >= 0),
    permitted_use proxiima_core.goal_publication_use NOT NULL,
    cited_upload_id uuid REFERENCES proxiima_core.blob_uploads(upload_id) ON DELETE SET NULL,
    published_by uuid NOT NULL,
    PRIMARY KEY (goal_handle, memory_t)
);

CREATE TABLE proxiima_core.goal_work_binding (
    goal_handle uuid NOT NULL REFERENCES proxiima_core.goal_head(handle) ON DELETE CASCADE,
    memory_handle uuid NOT NULL REFERENCES proxiima_core.memory_head(handle) ON DELETE CASCADE,
    source_owner uuid NOT NULL REFERENCES proxiima_core.owners(owner_id) ON DELETE CASCADE,
    source_generation bigint NOT NULL CHECK (source_generation >= 0),
    controller_generation bigint NOT NULL CHECK (controller_generation >= 0),
    bound_by uuid NOT NULL,
    PRIMARY KEY (goal_handle, memory_handle)
);

-- Schema evolution completion and serving write fence. A completed source
-- schema is no longer writable. The migration executor takes the exclusive
-- transaction advisory lock for the same schema before it transforms rows and
-- records completion. Ordinary writers take the shared lock in the generated
-- trigger and refuse the source after completion.

CREATE TABLE proxiima_core.schema_evolution (
    source_kind text NOT NULL CHECK (source_kind IN ('fact', 'abstraction', 'perspective', 'goal', 'cited_object')),
    source_schema_id text NOT NULL,
    evolution_id text NOT NULL,
    spiice_id text NOT NULL,
    target_schema_ids text[] NOT NULL,
    target_kinds text[] NOT NULL,
    completed_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT schema_evolution_source_nonblank_chk
        CHECK (length(btrim(source_schema_id)) > 0),
    CONSTRAINT schema_evolution_evolution_nonblank_chk
        CHECK (length(btrim(evolution_id)) > 0),
    CONSTRAINT schema_evolution_spiice_nonblank_chk
        CHECK (length(btrim(spiice_id)) > 0),
    CONSTRAINT schema_evolution_targets_nonempty_chk
        CHECK (cardinality(target_schema_ids) > 0 AND cardinality(target_kinds) = cardinality(target_schema_ids)),
    CONSTRAINT schema_evolution_targets_no_null_chk
        CHECK (array_position(target_schema_ids, NULL::text) IS NULL AND array_position(target_kinds, NULL::text) IS NULL),
    CONSTRAINT schema_evolution_targets_nonblank_chk
        CHECK (array_position(target_schema_ids, '') IS NULL AND array_position(target_kinds, '') IS NULL),
    CONSTRAINT schema_evolution_target_kinds_chk
        CHECK (target_kinds <@ ARRAY['fact', 'abstraction', 'perspective', 'goal', 'cited_object']::text[]),
    PRIMARY KEY (source_kind, source_schema_id)
);

CREATE TABLE proxiima_core.schema_evolution_authority (
    xact xid8 NOT NULL DEFAULT pg_current_xact_id(),
    record_kind text NOT NULL CHECK (record_kind IN ('memory', 'goal', 'blob')),
    source_id uuid NOT NULL,
    target_id uuid NOT NULL,
    source_schema text NOT NULL,
    target_schema text NOT NULL,
    source_handle uuid,
    target_handle uuid,
    record_owner uuid NOT NULL REFERENCES proxiima_core.owners(owner_id),
    primary_target boolean NOT NULL,
    source_pins uuid[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (xact, record_kind, source_id, source_schema, target_id)
);

CREATE TABLE proxiima_core.schema_evolution_publication (
    publication_t uuid NOT NULL REFERENCES proxiima_core.publication_origin(t) ON DELETE CASCADE,
    memory_t uuid NOT NULL REFERENCES proxiima_core.memory(t) ON DELETE CASCADE,
    PRIMARY KEY (publication_t, memory_t)
);

-- Owner-RLS records

CREATE TABLE proxiima_core.owner_rls_table (
    schema_name text NOT NULL,
    table_name text NOT NULL,
    class proxiima_core.owner_rls_class NOT NULL,
    owner_column text NOT NULL,
    PRIMARY KEY (schema_name, table_name),
    CONSTRAINT owner_rls_table_schema_chk CHECK (schema_name <> 'proxiima_core')
);

COMMENT ON TABLE proxiima_core.owner_rls_table IS 'The owner-RLS class and owner column of each table outside proxiima_core that install_owner_rls or install_owner_rls_table last installed. Written by the installers only, in platform scope; every migration run reinstalls the generated policies from it at its end. A Follows table is governed by owner_follows and has no row here.';

CREATE TABLE proxiima_core.owner_rls_installed (
    schema_name text NOT NULL,
    table_name text NOT NULL,
    policies text NOT NULL,
    PRIMARY KEY (schema_name, table_name)
);

COMMENT ON TABLE proxiima_core.owner_rls_installed IS 'The text of the three owner policies of each table at the moment owner_rls_apply_policies created them, as owner_rls_policy_text renders it from the catalog. Written by that function only, in platform scope; boot refuses a table of a composed schema whose policies differ from it.';

-- Indexes

CREATE INDEX agent_derivation_v1_substring_trgm ON proxiima_core.agent_derivation_v1 USING gin (lower(proxiima_core.lexical_join(VARIADIC ARRAY[NULLIF(title, ''::text), NULLIF(body, ''::text), proxiima_core.lexical_text_array(tags)])) public.gin_trgm_ops);

CREATE INDEX agent_note_v1_substring_trgm ON proxiima_core.agent_note_v1 USING gin (lower(proxiima_core.lexical_join(VARIADIC ARRAY[NULLIF(title, ''::text), NULLIF(body, ''::text), proxiima_core.lexical_text_array(tags)])) public.gin_trgm_ops);

CREATE INDEX blob_uploads_blob_id_idx ON proxiima_core.blob_uploads USING btree (blob_id) WHERE (blob_id IS NOT NULL);

CREATE INDEX blob_uploads_mounted_from_idx ON proxiima_core.blob_uploads USING btree (mounted_from_upload_id) WHERE (mounted_from_upload_id IS NOT NULL);

CREATE INDEX blob_uploads_object_key_idx ON proxiima_core.blob_uploads USING btree (object_key);

CREATE INDEX blob_uploads_owner_status_idx ON proxiima_core.blob_uploads USING btree (owner_id, status);

CREATE INDEX blob_uploads_pending_expiry_idx ON proxiima_core.blob_uploads USING btree (expires_at) WHERE (status = 'pending'::proxiima_core.blob_upload_status);

CREATE INDEX blob_uploads_terminal_content_idx ON proxiima_core.blob_uploads USING btree (owner_id, content_hash) WHERE (status = ANY (ARRAY['aborted'::proxiima_core.blob_upload_status, 'expired'::proxiima_core.blob_upload_status]));

CREATE INDEX cold_purge_pending_due_idx ON proxiima_core.cold_purge_pending USING btree (object_store, next_attempt_at, enqueued_at, object_key);

CREATE INDEX cold_purge_pending_owner_idx ON proxiima_core.cold_purge_pending USING btree (owner_id, enqueued_at);

CREATE INDEX content_owner_schema_idx ON proxiima_core.content USING btree (owner_id, schema_id);

CREATE INDEX core_projection_owner_tsv_gin ON proxiima_core.projection USING gin (owner_id, search_tsv);

CREATE INDEX core_projection_owner_tsv_gin_tag ON proxiima_core.projection USING gin (owner_id, tag);

CREATE INDEX embedding_jobs_entity_id_idx ON proxiima_core.embedding_jobs USING btree (entity_id);

CREATE INDEX embedding_jobs_pending_claim_idx ON proxiima_core.embedding_jobs USING btree (job_id) WHERE (status = 'pending'::proxiima_core.embedding_job_status);

CREATE INDEX embedding_jobs_processing_idx ON proxiima_core.embedding_jobs USING btree (claimed_at) WHERE (status = 'processing'::proxiima_core.embedding_job_status);

CREATE INDEX embeddings_owner_model_idx ON proxiima_core.embeddings USING btree (owner_id, model_id, dim);

CREATE INDEX goal_dependency_gin ON proxiima_core.goal USING gin (dependency_t);

CREATE INDEX goal_evidence_gin ON proxiima_core.goal USING gin (evidence_t);

CREATE INDEX goal_access_grant_subject_idx ON proxiima_core.goal_access_grant USING btree (subject_id, goal_handle);

CREATE INDEX goal_head_owner_handle_idx ON proxiima_core.goal_head USING btree (owner_id, handle);

CREATE INDEX goal_head_owner_schema_idx ON proxiima_core.goal_head USING btree (owner_id, schema_id, handle);

CREATE INDEX goal_memory_publication_memory_idx ON proxiima_core.goal_memory_publication USING btree (memory_t, goal_handle);

CREATE INDEX goal_memory_publication_source_idx ON proxiima_core.goal_memory_publication USING btree (source_owner, goal_handle, memory_t);

CREATE INDEX goal_memory_publication_upload_idx ON proxiima_core.goal_memory_publication USING btree (cited_upload_id) WHERE (cited_upload_id IS NOT NULL);

CREATE INDEX goal_motivated_by_gin ON proxiima_core.goal USING gin (motivated_by);

CREATE INDEX goal_motivation_index_owner_perspective ON proxiima_core.goal_motivation_index USING btree (owner_id, perspective_t, goal_t);

CREATE INDEX goal_owner_handle_t_idx ON proxiima_core.goal USING btree (owner_id, handle, t DESC);

CREATE INDEX goal_owner_state_t_idx ON proxiima_core.goal USING btree (owner_id, state, t DESC);

CREATE INDEX goal_parent_handle_idx ON proxiima_core.goal USING btree (owner_id, parent_handle) WHERE (parent_handle IS NOT NULL);

CREATE INDEX goal_replay_declaration_goal_t_idx ON proxiima_core.goal_replay_declaration USING btree (goal_t);

CREATE INDEX goal_unmotivated_owner_state_idx ON proxiima_core.goal USING btree (owner_id, state, handle DESC, t DESC) WHERE (motivated_by = '{}'::uuid[]);

CREATE INDEX goal_work_binding_memory_idx ON proxiima_core.goal_work_binding USING btree (memory_handle, goal_handle);

CREATE INDEX goal_work_binding_source_idx ON proxiima_core.goal_work_binding USING btree (source_owner, goal_handle, memory_handle);

CREATE INDEX group_edges_parent_idx ON proxiima_core.group_edges USING btree (parent_group_id);

CREATE INDEX group_memberships_member_user_id_idx ON proxiima_core.group_memberships USING btree (member_user_id, group_id, relation);

CREATE INDEX group_role_projection_group_idx ON proxiima_core.group_role_projection USING btree (group_id, member_user_id);

CREATE INDEX idx_agent_note_v1_nk ON proxiima_core.agent_note_v1 USING btree (note_id);

CREATE INDEX interpretation_v1_substring_trgm ON proxiima_core.interpretation_v1 USING gin (lower(proxiima_core.lexical_join(VARIADIC ARRAY[NULLIF(claim, ''::text)])) public.gin_trgm_ops);

CREATE INDEX memory_blob_id_idx ON proxiima_core.memory USING btree (blob_id) WHERE (blob_id IS NOT NULL);

CREATE INDEX memory_content_id_idx ON proxiima_core.memory USING btree (content_id) WHERE (content_id IS NOT NULL);

CREATE INDEX memory_forked_from_idx ON proxiima_core.memory USING btree (forked_from) WHERE (forked_from IS NOT NULL);

CREATE INDEX memory_goal_refs_gin ON proxiima_core.memory USING gin (goal_refs);

CREATE INDEX memory_guided_by_gin ON proxiima_core.memory USING gin (guided_by);

CREATE INDEX memory_head_owner_abstraction_t_idx ON proxiima_core.memory_head USING btree (owner_id, t DESC) INCLUDE (handle, kind) WHERE (kind = 'abstraction'::proxiima_core.memory_kind);

CREATE INDEX memory_head_owner_fact_t_idx ON proxiima_core.memory_head USING btree (owner_id, t DESC) INCLUDE (handle, kind) WHERE (kind = 'fact'::proxiima_core.memory_kind);

CREATE INDEX memory_head_owner_kind_idx ON proxiima_core.memory_head USING btree (owner_id, kind, handle);

CREATE INDEX memory_head_owner_perspective_t_idx ON proxiima_core.memory_head USING btree (owner_id, t DESC) INCLUDE (handle, kind) WHERE (kind = 'perspective'::proxiima_core.memory_kind);

CREATE INDEX memory_head_owner_schema_idx ON proxiima_core.memory_head USING btree (owner_id, schema_id, handle);

CREATE INDEX memory_head_owner_schema_t_idx ON proxiima_core.memory_head USING btree (owner_id, schema_id, t DESC) INCLUDE (handle, kind);

CREATE INDEX memory_head_owner_t_idx ON proxiima_core.memory_head USING btree (owner_id, t DESC) INCLUDE (handle, kind);

CREATE INDEX memory_origins_gin ON proxiima_core.memory USING gin (origins);

CREATE INDEX memory_owner_handle_t_idx ON proxiima_core.memory USING btree (owner_id, handle, t DESC);

CREATE INDEX memory_owner_schema_t_idx ON proxiima_core.memory USING btree (owner_id, schema_id, t DESC);

CREATE INDEX memory_owner_t_handle_idx ON proxiima_core.memory USING btree (owner_id, t, handle);

CREATE INDEX memory_reads_gin ON proxiima_core.memory USING gin (reads);

CREATE INDEX memory_refs_gin ON proxiima_core.memory USING gin (refs);

CREATE INDEX owners_kind_idx ON proxiima_core.owners USING btree (kind, owner_id);

CREATE INDEX publication_counter_owner_idx ON proxiima_core.publication_counter USING btree (owner_id, handle);

CREATE INDEX publication_delivery_consumer_idx ON proxiima_core.publication_delivery USING btree (consumer);

CREATE INDEX publication_delivery_pending_idx ON proxiima_core.publication_delivery USING btree (consumer, attempts, t) WHERE (state <> 'published'::proxiima_core.publication_state);

CREATE INDEX publication_origin_owner_source_idx ON proxiima_core.publication_origin USING btree (original_owner_id, source_id, t);

CREATE INDEX publication_outbox_owner_idx ON proxiima_core.publication_outbox USING btree (owner_id);

CREATE INDEX publication_outbox_pending_idx ON proxiima_core.publication_outbox USING btree (t) WHERE (state <> 'published'::proxiima_core.publication_record_state);

CREATE INDEX publication_outbox_published_idx ON proxiima_core.publication_outbox USING btree (published_at) WHERE (state = 'published'::proxiima_core.publication_record_state);

CREATE INDEX schema_evolution_publication_memory_idx ON proxiima_core.schema_evolution_publication USING btree (memory_t, publication_t);

CREATE INDEX schema_evolution_spiice_idx ON proxiima_core.schema_evolution USING btree (spiice_id, source_schema_id);

CREATE INDEX sketch_owner_t_idx ON proxiima_core.sketch USING btree (owner_id, t DESC);

CREATE INDEX utterance_v1_substring_trgm ON proxiima_core.utterance_v1 USING gin (lower(proxiima_core.lexical_join(VARIADIC ARRAY[NULLIF(text, ''::text)])) public.gin_trgm_ops);

-- Foreign keys

ALTER TABLE ONLY proxiima_core.agent_derivation_v1
    ADD CONSTRAINT agent_derivation_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.memory(t);

ALTER TABLE ONLY proxiima_core.agent_note_v1
    ADD CONSTRAINT agent_note_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.memory(t);

ALTER TABLE ONLY proxiima_core.blob
    ADD CONSTRAINT blob_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.blob_uploads
    ADD CONSTRAINT blob_uploads_blob_id_fkey FOREIGN KEY (blob_id) REFERENCES proxiima_core.blob(blob_id);

ALTER TABLE ONLY proxiima_core.blob_uploads
    ADD CONSTRAINT blob_uploads_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.cold_purge_pending
    ADD CONSTRAINT cold_purge_pending_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.content
    ADD CONSTRAINT content_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.embedding_heads
    ADD CONSTRAINT embedding_heads_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.embedding_jobs
    ADD CONSTRAINT embedding_jobs_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.embeddings
    ADD CONSTRAINT embeddings_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.goal_change_v1
    ADD CONSTRAINT goal_change_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.memory(t);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_handle_fkey FOREIGN KEY (handle) REFERENCES proxiima_core.goal_head(handle);

ALTER TABLE ONLY proxiima_core.goal_head
    ADD CONSTRAINT goal_head_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.goal
    ADD CONSTRAINT goal_owner_matches_head_fk FOREIGN KEY (handle, owner_id) REFERENCES proxiima_core.goal_head(handle, owner_id) NOT VALID;

ALTER TABLE ONLY proxiima_core.goal_replay_declaration
    ADD CONSTRAINT goal_replay_declaration_goal_t_fkey FOREIGN KEY (goal_t) REFERENCES proxiima_core.goal(t) ON DELETE CASCADE;

ALTER TABLE ONLY proxiima_core.goal_replay_declaration
    ADD CONSTRAINT goal_replay_declaration_goal_owner_fkey FOREIGN KEY (goal_t, owner_id) REFERENCES proxiima_core.goal(t, owner_id) ON DELETE CASCADE;

ALTER TABLE ONLY proxiima_core.group_edges
    ADD CONSTRAINT group_edges_child_group_fk FOREIGN KEY (child_group_id, child_group_kind) REFERENCES proxiima_core.owners(owner_id, kind) DEFERRABLE;

ALTER TABLE ONLY proxiima_core.group_edges
    ADD CONSTRAINT group_edges_parent_group_fk FOREIGN KEY (parent_group_id, parent_group_kind) REFERENCES proxiima_core.owners(owner_id, kind) DEFERRABLE;

ALTER TABLE ONLY proxiima_core.group_memberships
    ADD CONSTRAINT group_memberships_group_registered_fk FOREIGN KEY (group_id, group_kind) REFERENCES proxiima_core.owners(owner_id, kind);

ALTER TABLE ONLY proxiima_core.group_role_projection
    ADD CONSTRAINT group_role_projection_group_id_fkey FOREIGN KEY (group_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.ingest_keys
    ADD CONSTRAINT ingest_keys_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.interpretation_v1
    ADD CONSTRAINT interpretation_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.memory(t);

ALTER TABLE ONLY proxiima_core.lexical_default
    ADD CONSTRAINT lexical_default_config_fkey FOREIGN KEY (config) REFERENCES proxiima_core.lexical_languages(config);

ALTER TABLE ONLY proxiima_core.memory
    ADD CONSTRAINT memory_blob_id_fkey FOREIGN KEY (blob_id) REFERENCES proxiima_core.blob(blob_id);

ALTER TABLE ONLY proxiima_core.memory
    ADD CONSTRAINT memory_content_id_fkey FOREIGN KEY (content_id) REFERENCES proxiima_core.content(content_id);

ALTER TABLE ONLY proxiima_core.memory
    ADD CONSTRAINT memory_handle_fkey FOREIGN KEY (handle) REFERENCES proxiima_core.memory_head(handle);

ALTER TABLE ONLY proxiima_core.memory_head
    ADD CONSTRAINT memory_head_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.memory
    ADD CONSTRAINT memory_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.publication_counter
    ADD CONSTRAINT publication_counter_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.publication_delivery
    ADD CONSTRAINT publication_delivery_consumer_fkey FOREIGN KEY (consumer) REFERENCES proxiima_core.publication_consumer(name) ON DELETE CASCADE;

ALTER TABLE ONLY proxiima_core.publication_delivery
    ADD CONSTRAINT publication_delivery_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.publication_outbox(t) ON DELETE CASCADE;

ALTER TABLE ONLY proxiima_core.publication_origin
    ADD CONSTRAINT publication_origin_original_owner_id_fkey FOREIGN KEY (original_owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.publication_outbox
    ADD CONSTRAINT publication_outbox_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.sketch
    ADD CONSTRAINT sketch_owner_id_fkey FOREIGN KEY (owner_id) REFERENCES proxiima_core.owners(owner_id);

ALTER TABLE ONLY proxiima_core.task_goal_v1
    ADD CONSTRAINT task_goal_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.goal(t);

ALTER TABLE ONLY proxiima_core.utterance_v1
    ADD CONSTRAINT utterance_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.memory(t);

ALTER TABLE ONLY proxiima_core.write_act_v1
    ADD CONSTRAINT write_act_v1_t_fkey FOREIGN KEY (t) REFERENCES proxiima_core.memory(t);

-- Core seed rows

-- The insertion order is fixed: owner-erase goldens compare physical row order.
INSERT INTO proxiima_core.spiice_surface (table_name, spiice_id) VALUES
    ('proxiima_core.agent_derivation_v1', 'core'),
    ('proxiima_core.agent_note_v1', 'core'),
    ('proxiima_core.interpretation_v1', 'core'),
    ('proxiima_core.task_goal_v1', 'core'),
    ('proxiima_core.utterance_v1', 'core'),
    ('proxiima_core.write_act_v1', 'core'),
    ('proxiima_core.goal_change_v1', 'core');

INSERT INTO proxiima_core.lexical_languages (config)
VALUES ('english'::regconfig);

INSERT INTO proxiima_core.lexical_default (singleton, config)
VALUES (true, 'english'::regconfig);

INSERT INTO proxiima_core.installation (singleton) VALUES (true);

INSERT INTO proxiima_core.owner_rls_epoch (singleton, epoch)
VALUES (true, 1);

INSERT INTO proxiima_core.publication_consumer (name) VALUES ('default');

INSERT INTO proxiima_core.publication_backlog (singleton, pending, measured_at)
VALUES (true, 0, '-infinity');

-- Row-level security enablement

ALTER TABLE proxiima_core.agent_derivation_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.agent_note_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.blob ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.blob_uploads ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.closed_handle ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.cold_purge_pending ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.content ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.delegated_authority_grants ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.embedding_heads ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.embedding_jobs ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.embeddings ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.erased_pin_target ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.export_erase_fence ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.spiice_surface ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_access_grant ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_change_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_head ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_memory_publication ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_motivation_index ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_replay_declaration ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.goal_work_binding ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.group_edges ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.group_lock_request ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.group_memberships ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.group_role_projection ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.ingest_keys ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.installation ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.interpretation_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.lexical_default ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.lexical_languages ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.maintenance_lease ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.memory ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.memory_external_origin ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.memory_head ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.owner_follows ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.owner_rls_epoch ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.owner_rls_installed ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.owner_rls_table ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.owners ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.projection ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.publication_backlog ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.publication_consumer ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.publication_counter ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.publication_delivery ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.publication_origin ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.publication_outbox ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.retention ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.schema_evolution ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.schema_evolution_authority ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.schema_evolution_publication ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.sketch ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.source_cursors ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.store_identity ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.task_goal_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.utterance_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.vector_index_debt ENABLE ROW LEVEL SECURITY;

ALTER TABLE proxiima_core.write_act_v1 ENABLE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.agent_derivation_v1 FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.agent_note_v1 FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.blob FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.blob_uploads FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.closed_handle FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.cold_purge_pending FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.content FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.delegated_authority_grants FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.embedding_heads FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.embedding_jobs FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.embeddings FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.erased_pin_target FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.export_erase_fence FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.spiice_surface FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_access_grant FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_change_v1 FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_head FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_memory_publication FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_motivation_index FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_replay_declaration FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.goal_work_binding FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.group_edges FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.group_lock_request FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.group_memberships FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.group_role_projection FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.ingest_keys FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.installation FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.interpretation_v1 FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.lexical_default FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.lexical_languages FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.maintenance_lease FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.memory FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.memory_external_origin FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.memory_head FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.owner_follows FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.owner_rls_epoch FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.owner_rls_installed FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.owner_rls_table FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.owners FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.projection FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.publication_backlog FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.publication_consumer FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.publication_counter FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.publication_delivery FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.publication_origin FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.publication_outbox FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.retention FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.schema_evolution FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.schema_evolution_authority FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.schema_evolution_publication FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.sketch FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.source_cursors FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.store_identity FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.task_goal_v1 FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.utterance_v1 FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.vector_index_debt FORCE ROW LEVEL SECURITY;

ALTER TABLE ONLY proxiima_core.write_act_v1 FORCE ROW LEVEL SECURITY;

-- Privileges

GRANT SELECT ON TABLE proxiima_core.group_edges TO PUBLIC;

GRANT SELECT ON TABLE proxiima_core.group_lock_request TO PUBLIC;

GRANT SELECT ON TABLE proxiima_core.group_role_projection TO PUBLIC;
