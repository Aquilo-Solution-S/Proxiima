//! The projection generator: one contract in, one canonical shape out.
//!
//! This is the whole of it. `projection_artifacts(&SpiiceContract)` is a
//! pure function with no parameters beyond the contract, no per-spiice
//! branch, and no configuration surface. Two spiices' emitted DDL differ in
//! the schema name and the index name and in nothing else — which is what
//! makes "the generator has not grown a hook" a checkable property rather
//! than an intention. A need the canonical shape cannot express becomes a
//! contract-vocabulary extension (`LanguagePolicy::PinnedUnion` is one),
//! never a generator feature.
//!
//! **Every artifact carries its derivable inverse.** [`Artifact`] is a
//! forward statement and its `DROP`, produced by the same call from the
//! same declaration, so they cannot drift. That is what makes provisioning
//! and de-provisioning a spiice in a deployment two readings of one
//! derivation instead of two hand-maintained scripts. A hand-written
//! inverse is the alternative, and it reaches whatever subset of the
//! sidecars its author remembered; a spiice that keeps its own inverse
//! has a test compare it against the contract.
//!
//! Two consumers, deliberately:
//!
//! 1. **The migration author.** The core baseline (`migrations/0069_baseline.sql`)
//!    carries the table and index output; `definitions/triggers.sql` carries
//!    the language trigger, while each spiice baseline carries its own table
//!    and index. A spiice's table names no core table: its three foreign
//!    keys into core are `install_reference` calls after the `CREATE TABLE`,
//!    while core's own table, which that installer does not serve, names
//!    them inline. The generator-output tests pin those contracts so a
//!    change fails the build rather than the next boot.
//! 2. **The boot guardrail** (`ensure_projection_schema`), which re-runs it
//!    and compares against `information_schema` in the same bidirectional
//!    style as `ensure_lexical_language_stamps`.
//!
//! There is deliberately no third consumer that issues this DDL at
//! runtime: boot-time `CREATE TABLE` contradicts the migration ledger and,
//! in a shared multi-owner deployment, runs DDL once per process start.

use proxiima_core::spiice::{
    KeyShape, LanguagePolicy, SchemaContract, SearchProjectionDecl, SpiiceContract,
    TSVECTOR_WEIGHT_CLASSES,
};
use proxiima_core::{SearchProjectionColumnKind, verbs::schema::MemorySearchProjection};
use proxiima_core::{SqlText, StorageError};

use proxiima_core::PgIdent;

/// The snippet bound the search verb applies to sidecar text.
pub(crate) const SNIPPET_BYTES: u64 = 480;

/// One emitted object and the statement that removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub forward: SqlText,
    pub inverse: SqlText,
}

/// Everything one spiice's projection is, as SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionArtifacts {
    pub table: Artifact,
    pub index: Artifact,
    pub tag_index: Artifact,
    pub language_trigger: Artifact,
}

impl ProjectionArtifacts {
    /// Forward statements in dependency order.
    #[must_use]
    pub fn forward(&self) -> Vec<&SqlText> {
        vec![
            &self.table.forward,
            &self.index.forward,
            &self.tag_index.forward,
            &self.language_trigger.forward,
        ]
    }

    /// Inverses in reverse dependency order — the de-provisioning script,
    /// derived rather than written.
    #[must_use]
    pub fn inverse(&self) -> Vec<&SqlText> {
        vec![
            &self.language_trigger.inverse,
            &self.tag_index.inverse,
            &self.index.inverse,
            &self.table.inverse,
        ]
    }

    /// The whole emitted text, forward, as it appears in a baseline.
    #[must_use]
    pub fn render(&self) -> SqlText {
        let mut sql = SqlText::new("");
        for (index, statement) in self.forward().into_iter().enumerate() {
            if index > 0 {
                sql.push("\n\n");
            }
            sql.push_text(statement);
        }
        sql
    }
}

/// The extension the composite `gin (owner_id, search_tsv)` needs.
///
/// Prior art is `btree_gin`'s own documentation: "for queries that test
/// both a GIN-indexable column and a B-tree-indexable column, it might be
/// more efficient to create a multicolumn GIN index that uses one of these
/// operator classes than to create two separate indexes that would have to
/// be combined via bitmap `ANDing`" (`PostgreSQL` *F.9*). `uuid` is in its
/// supported type list.
pub const BTREE_GIN_EXTENSION: &str = "CREATE EXTENSION IF NOT EXISTS btree_gin;";

/// Contract declaration in → canonical projection shape out.
///
/// Returns `None` for a spiice that declares no projection.
///
/// # Errors
///
/// `StorageError::Internal` when a declared table, index or column name is
/// not a valid `PostgreSQL` identifier.
pub fn projection_artifacts(
    contract: &SpiiceContract,
) -> Result<Option<ProjectionArtifacts>, StorageError> {
    let Some(spec) = contract.projection.spec() else {
        return Ok(None);
    };
    let table = PgIdent::table(spec.table)?;
    let index = PgIdent::column(spec.index)?;
    let tag_index_name = format!("{}_tag", spec.index);
    let tag_index = PgIdent::column(&tag_index_name)?;
    let (schema, relation) = split_qualified(spec.table)?;
    let trigger = format!("{relation}_remember_lang");
    let trigger_ident = PgIdent::column(&trigger)?;
    let schema_ident = PgIdent::name(schema)?;

    // Every spliced name above went through `PgIdent`, which rejects
    // anything that is not a bare (optionally schema-qualified) identifier.
    let create = create_table_sql(&table, schema, relation)?;

    let mut create_index = SqlText::new("CREATE INDEX ");
    create_index
        .push_ident(&index)
        .push(" ON ")
        .push_ident(&table)
        .push(" USING gin (owner_id, search_tsv);");

    let mut create_trigger = SqlText::new("CREATE OR REPLACE TRIGGER ");
    create_trigger
        .push_ident(&trigger_ident)
        .push("\n    BEFORE INSERT ON ")
        .push_ident(&table)
        .push(
            "
    FOR EACH ROW
    EXECUTE FUNCTION proxiima_core.remember_lexical_language();",
        );

    let mut drop_table = SqlText::new("DROP TABLE IF EXISTS ");
    drop_table.push_ident(&table).push(";");
    let mut drop_index = SqlText::new("DROP INDEX IF EXISTS ");
    drop_index
        .push_ident(&schema_ident)
        .push(".")
        .push_ident(&index)
        .push(";");
    let mut create_tag_index = SqlText::new("CREATE INDEX ");
    create_tag_index
        .push_ident(&tag_index)
        .push(" ON ")
        .push_ident(&table)
        .push(" USING gin (owner_id, tag);");
    let mut drop_tag_index = SqlText::new("DROP INDEX IF EXISTS ");
    drop_tag_index
        .push_ident(&schema_ident)
        .push(".")
        .push_ident(&tag_index)
        .push(";");
    let mut drop_trigger = SqlText::new("DROP TRIGGER IF EXISTS ");
    drop_trigger
        .push_ident(&trigger_ident)
        .push(" ON ")
        .push_ident(&table)
        .push(";");

    Ok(Some(ProjectionArtifacts {
        table: Artifact {
            forward: create,
            inverse: drop_table,
        },
        index: Artifact {
            forward: create_index,
            inverse: drop_index,
        },
        tag_index: Artifact {
            forward: create_tag_index,
            inverse: drop_tag_index,
        },
        language_trigger: Artifact {
            forward: create_trigger,
            inverse: drop_trigger,
        },
    }))
}

/// The `CREATE TABLE` of a projection table and, in a spiice's schema, the
/// installer calls for its keys into core.
fn create_table_sql(
    table: &PgIdent,
    schema: &str,
    relation: &str,
) -> Result<SqlText, StorageError> {
    // The table name went through `PgIdent`. The rest of the text is fixed:
    // the column list, the types and the constraints are the same characters
    // for every spiice, which is the slimness property stated as code.
    let keys = if schema == CORE_SCHEMA {
        CoreKeys::Inline
    } else {
        CoreKeys::Installer
    };
    let mut create = SqlText::new("CREATE TABLE ");
    create.push_ident(table).push(" (");
    for (index, column) in PROJECTION_TABLE.iter().enumerate() {
        if index > 0 {
            create.push(",");
        }
        create.push("\n    ").push(column.definition);
        if let (CoreKeys::Inline, Some(key)) = (keys, &column.core_key) {
            create.push("\n                     ").push(key.inline);
        }
    }
    create.push(",\n    PRIMARY KEY (memory_id, schema_id)\n);");
    if keys == CoreKeys::Installer {
        let schema_ident = PgIdent::name(schema)?;
        let relation_ident = PgIdent::name(relation)?;
        for (column, key) in PROJECTION_TABLE
            .iter()
            .filter_map(|column| Some((column.name, column.core_key.as_ref()?)))
        {
            create
                .push("\nSELECT proxiima_core.install_reference('")
                .push_ident(&schema_ident)
                .push("', '")
                .push_ident(&relation_ident)
                .push("', '")
                .push(column)
                .push("', '")
                .push(key.referenced)
                .push("', '")
                .push(key.on_delete)
                .push("');");
        }
    }
    Ok(create)
}

/// The schema `install_reference` does not serve: core's own projection
/// table, released in the core baseline, names the core tables it references.
const CORE_SCHEMA: &str = "proxiima_core";

/// How a projection table's foreign keys into core are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreKeys {
    /// In the column list: core's own table, in core's own schema.
    Inline,
    /// One `install_reference` call each, after the `CREATE TABLE`: a
    /// spiice's table, whose migration names no core table.
    Installer,
}

/// One column of the projection table.
struct ProjectionColumn {
    name: &'static str,
    /// Name, type, nullability and default, as the `CREATE TABLE` spells it.
    definition: &'static str,
    core_key: Option<CoreKey>,
}

/// One foreign key into core, in both of its spellings.
struct CoreKey {
    inline: &'static str,
    /// `install_reference`'s closed vocabularies.
    referenced: &'static str,
    on_delete: &'static str,
}

/// The projection table's columns, in `CREATE TABLE` order. Erase reaches a
/// projection row through the memory key's cascade
/// ([`proxiima_core::spiice::PROJECTION_MEMORY_FK`]).
const PROJECTION_TABLE: [ProjectionColumn; 6] = [
    ProjectionColumn {
        name: "memory_id",
        definition: "memory_id        uuid      NOT NULL",
        core_key: Some(CoreKey {
            inline: "REFERENCES proxiima_core.memory (t) ON DELETE CASCADE",
            referenced: "memory",
            on_delete: "cascade",
        }),
    },
    ProjectionColumn {
        name: "schema_id",
        definition: "schema_id        text      NOT NULL",
        core_key: None,
    },
    ProjectionColumn {
        name: "owner_id",
        definition: "owner_id         uuid      NOT NULL",
        core_key: Some(CoreKey {
            inline: "REFERENCES proxiima_core.owners (owner_id)",
            referenced: "owner",
            on_delete: "no_action",
        }),
    },
    ProjectionColumn {
        name: "search_tsv",
        definition: "search_tsv       tsvector  NOT NULL",
        core_key: None,
    },
    ProjectionColumn {
        name: "tag",
        definition: "tag              text[]    NOT NULL DEFAULT '{}'",
        core_key: None,
    },
    ProjectionColumn {
        name: "lexical_language",
        definition: "lexical_language regconfig NOT NULL DEFAULT proxiima_core.lexical_config()",
        core_key: Some(CoreKey {
            inline: "REFERENCES proxiima_core.lexical_languages (config)",
            referenced: "lexical_language",
            on_delete: "no_action",
        }),
    },
];

fn split_qualified(table: &str) -> Result<(&str, &str), StorageError> {
    table.split_once('.').ok_or_else(|| {
        StorageError::Internal(format!(
            "projection table must be schema-qualified: {table:?}"
        ))
    })
}

/// The text expression the vector is built from, over sidecar alias `c`.
///
/// This is the same shape the generated columns carry —
/// `lexical_join(VARIADIC ARRAY[NULLIF(col,''), lexical_text_array(arr)])`
/// — which is what makes the identity re-proof provable rather than
/// hopeful, not a re-derivation that happens to agree.
fn search_text_expr(
    fields: &[(&str, SearchProjectionColumnKind)],
) -> Result<SqlText, StorageError> {
    if fields.is_empty() {
        return Err(StorageError::Internal(
            "a projected schema declares no fields".into(),
        ));
    }
    let mut sql = SqlText::new("proxiima_core.lexical_join(VARIADIC ARRAY[");
    for (index, (column, kind)) in fields.iter().enumerate() {
        let column = PgIdent::column(column)?;
        if index > 0 {
            sql.push(", ");
        }
        match kind {
            SearchProjectionColumnKind::Text => {
                sql.push("NULLIF(c.").push_ident(&column).push(", '')");
            }
            SearchProjectionColumnKind::TextArray => {
                sql.push("proxiima_core.lexical_text_array(c.")
                    .push_ident(&column)
                    .push(")");
            }
        }
    }
    sql.push("])");
    Ok(sql)
}

/// `lexical_tsv(<config>, <text>)`, unioned across a `PinnedUnion` in the
/// declared order. Order is load-bearing: tsvector concatenation offsets
/// the right operand's positions and `ts_rank_cd` is position-sensitive.
fn vector_expr(language: LanguagePolicy, text: &SqlText) -> Result<SqlText, StorageError> {
    let pinned = |config: &str| -> Result<SqlText, StorageError> {
        let config = PgIdent::column(config)?;
        let mut sql = SqlText::new("proxiima_core.lexical_tsv('");
        sql.push_ident(&config)
            .push("'::regconfig, ")
            .push_text(text)
            .push(")");
        Ok(sql)
    };
    match language {
        LanguagePolicy::PerRow { .. } => {
            let mut sql = SqlText::new("proxiima_core.lexical_tsv(");
            sql.push(LANGUAGE_BIND).push(", ").push_text(text).push(")");
            Ok(sql)
        }
        LanguagePolicy::Pinned(config) => pinned(config),
        LanguagePolicy::PinnedUnion(configs) => {
            if configs.is_empty() {
                return Err(StorageError::Internal(
                    "PinnedUnion declares no configuration".into(),
                ));
            }
            let mut sql = SqlText::new("");
            for (index, config) in configs.iter().enumerate() {
                if index > 0 {
                    sql.push("\n            || ");
                }
                sql.push_text(&pinned(config)?);
            }
            Ok(sql)
        }
    }
}

/// The write's language, or the deployment default. `$2`, and it appears
/// ONLY in a `LanguagePolicy::PerRow` statement — a pinned policy renders
/// its configuration as a literal, so its statement has no language bind
/// and no caller can influence what it stamps.
///
/// The `COALESCE` arm is reached by two writers only: a write that asked
/// for the deployment configuration by name, and a projection rebuild that
/// was handed no language. A write that named no language at all
/// never reaches this statement — see
/// `PgSidecarWriter::insert_memory_sidecar`.
const LANGUAGE_BIND: &str = "COALESCE($2::regconfig, proxiima_core.lexical_config())";

/// The row's `lexical_language` value.
fn language_value_expr(language: LanguagePolicy) -> Result<SqlText, StorageError> {
    match language {
        LanguagePolicy::PerRow { .. } => Ok(SqlText::new(LANGUAGE_BIND)),
        LanguagePolicy::Pinned(_) | LanguagePolicy::PinnedUnion(_) => {
            let config = language.pinned_config().ok_or_else(|| {
                StorageError::Internal("a pinned language policy names no configuration".into())
            })?;
            let config = PgIdent::column(config)?;
            let mut sql = SqlText::new("'");
            sql.push_ident(&config).push("'::regconfig");
            Ok(sql)
        }
    }
}

/// The whole vector expression for one schema, `setweight` included.
///
/// One declared level ⇒ ONE `to_tsvector` call over the joined text, which
/// is textually what the generated column emits and therefore
/// position-identical. Two or more ⇒ one call per class, concatenated,
/// because `setweight` marks a whole vector and per-field marking is the
/// only way to mark fields differently.
///
/// # Errors
///
/// `StorageError::Internal` for an invalid identifier or a unit whose
/// weight levels exceed `PostgreSQL`'s four classes (the freeze rejects
/// that first; this is the backstop).
pub fn projection_vector_sql(search: &SearchProjectionDecl) -> Result<SqlText, StorageError> {
    let SearchProjectionDecl::Projected {
        fields, language, ..
    } = search
    else {
        return Err(StorageError::Internal(
            "a declared non-surface has no vector".into(),
        ));
    };
    let levels = search.weight_levels().map_err(|levels| {
        StorageError::Internal(format!(
            "{levels} distinct weight levels exceed the {} PostgreSQL tsvector classes",
            TSVECTOR_WEIGHT_CLASSES.len()
        ))
    })?;
    if levels.len() < 2 {
        let all = fields
            .iter()
            .map(|field| (field.column, field.kind))
            .collect::<Vec<_>>();
        let vector = vector_expr(*language, &search_text_expr(&all)?)?;
        let mut sql = SqlText::new("COALESCE(");
        sql.push_text(&vector).push(", ''::tsvector)");
        return Ok(sql);
    }
    let mut sql = SqlText::new("");
    for (index, level) in levels.iter().enumerate() {
        let class = TSVECTOR_WEIGHT_CLASSES[index];
        let in_class = fields
            .iter()
            .filter(|field| field.weight.total_cmp(level).is_eq())
            .map(|field| (field.column, field.kind))
            .collect::<Vec<_>>();
        if in_class.is_empty() {
            continue;
        }
        let vector = vector_expr(*language, &search_text_expr(&in_class)?)?;
        if !sql.as_str().is_empty() {
            sql.push("\n            || ");
        }
        sql.push("setweight(COALESCE(")
            .push_text(&vector)
            .push(", ''::tsvector), '")
            .push(class)
            .push("')");
    }
    Ok(sql)
}

/// The maintenance statement: one row of `<spiice>.projection` from one row
/// of the schema's sidecar, in the writing transaction.
///
/// `$1` is the memory id; `$2` is the caller's lexical language, or NULL
/// for the deployment default. `owner_id` comes from the joined `memory`
/// row rather than from a bind, so the projection's owner is the memory's
/// owner by construction and no caller can supply a different one.
///
/// The sidecar side of the join and of the `WHERE` reads the column the
/// contract declares for that table (`KeyShape::MemoryT { column }`), which
/// is what keeps the statement a function of the contract alone. `m.t` is
/// the kernel memory table's own key and is fixed.
///
/// The whole contract is the input rather than the projection spec alone,
/// because a sidecar's `Surface` may be declared on a SIBLING registration
/// of the same table — see [`SpiiceContract::surface_for`].
///
/// # Errors
///
/// `StorageError::Internal` for an invalid identifier, a spiice that
/// declares no projection, a schema that is not a search surface, or a
/// sidecar that declares no memory key.
pub fn projection_insert_sql(
    contract: &SpiiceContract,
    schema: &SchemaContract,
) -> Result<SqlText, StorageError> {
    let spec = contract.projection.spec().ok_or_else(|| {
        StorageError::Internal(format!(
            "spiice {} declares no projection table, so its schemas write no projection row",
            contract.spiice_id
        ))
    })?;
    let table = PgIdent::table(spec.table)?;
    let sidecar_table = schema.sidecar_table.ok_or_else(|| {
        StorageError::Internal("a projected schema declares no sidecar table".into())
    })?;
    let sidecar = PgIdent::table(sidecar_table)?;
    let key = PgIdent::column(sidecar_memory_key(contract, schema, sidecar_table)?)?;
    let SearchProjectionDecl::Projected {
        tag_column,
        language,
        ..
    } = &schema.search
    else {
        return Err(StorageError::Internal(
            "a declared non-surface writes no projection row".into(),
        ));
    };
    let vector = projection_vector_sql(&schema.search)?;
    let mut tag = SqlText::new("");
    match tag_column {
        Some(column) => {
            tag.push("c.").push_ident(&PgIdent::column(column)?);
        }
        None => {
            tag.push("'{}'::text[]");
        }
    }
    let language_value = language_value_expr(*language)?;

    // The projection table, the sidecar table, its declared memory-key
    // column, the tag column and every configuration name are validated
    // identifiers; the schema id is a bind ($3) and the memory id is a
    // bind ($1).
    //
    // `c.{key}` is the SIDECAR's key column as the contract declares it and
    // `m.t` is the kernel memory table's own, which is fixed. Spelling the
    // sidecar side `t` would make this statement a function of the contract
    // plus a naming convention no declaration states, and a sidecar keyed
    // on any other column would get no projection row.
    //
    // `m.schema_id = $3` makes `projection.schema_id = memory.schema_id` a
    // property of the only statement that writes a projection row, rather
    // than a convention every caller has to remember. Search relies on it:
    // the ranked arm narrows on `p.schema_id` where `admit_hits` narrows on
    // `m.schema_id`, and those two agreeing is what lets one stand in for
    // the other. Nothing in the schema enforces it, so a mismatched write
    // inserts nothing rather than writing a row search would spend a window
    // slot on and admission would drop.
    let mut sql = SqlText::new("INSERT INTO ");
    sql.push_ident(&table)
        .push(
            "
       (memory_id, schema_id, owner_id, search_tsv, tag, lexical_language)
SELECT c.",
        )
        .push_ident(&key)
        .push(
            ",
       $3,
       m.owner_id,
       ",
        )
        .push_text(&vector)
        .push(",\n       ")
        .push_text(&tag)
        .push(",\n       ")
        .push_text(&language_value)
        .push("\n  FROM ")
        .push_ident(&sidecar)
        .push(" c\n  JOIN proxiima_core.memory_including_forgotten m ON m.t = c.")
        .push_ident(&key)
        .push("\n WHERE c.")
        .push_ident(&key)
        .push(" = $1\n   AND m.schema_id = $3");
    Ok(sql)
}

/// The sidecar's memory-key column, as the contract's [`Surface`] for that
/// table declares it, or the refusal.
///
/// There is no `t` fallback on either arm, deliberately. A default here
/// reads as "the contract, unless it says something this generator does not
/// expect", which is the shape of defect that produces a spiice whose
/// projection is silently empty. The error names the declaration to fix.
///
/// [`Surface`]: proxiima_core::spiice::Surface
fn sidecar_memory_key(
    contract: &SpiiceContract,
    schema: &SchemaContract,
    sidecar_table: &str,
) -> Result<&'static str, StorageError> {
    let Some(surface) = contract.surface_for(sidecar_table) else {
        return Err(StorageError::Internal(format!(
            "projected schema {} declares sidecar table {sidecar_table}, which no Surface of \
             spiice {} declares; add that Surface so the projection can read the declared \
             memory-key column",
            schema.schema_id().as_str(),
            contract.spiice_id
        )));
    };
    match surface.key {
        KeyShape::MemoryT { column } => Ok(column),
        other => Err(StorageError::Internal(format!(
            "projected schema {} keys sidecar {sidecar_table} as {other:?}; a projection row is \
             one per memory, so the sidecar's memory column has to be declared as \
             KeyShape::MemoryT {{ column }}",
            schema.schema_id().as_str()
        ))),
    }
}

/// Every projection table a composed binary declares, in name order.
#[must_use]
pub fn projection_tables(contracts: &[&'static SpiiceContract]) -> Vec<String> {
    let mut tables = contracts
        .iter()
        .filter_map(|contract| contract.projection.table())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    tables.sort();
    tables.dedup();
    tables
}

/// The snippet expression the search verb joins back to the sidecar for.
///
/// The projection carries no snippet. Search ranks on the projection
/// alone and joins the owning sidecar for the surviving top-k rows only, so
/// the text lives in exactly one place and the hot path still scans one
/// indexed table.
///
/// # Errors
///
/// `StorageError::Internal` for an invalid column identifier.
pub(crate) fn snippet_sql(projection: &MemorySearchProjection) -> Result<SqlText, StorageError> {
    let mut sql = SqlText::new("left(");
    sql.push_text(&sidecar_text_sql(projection)?)
        .push(", ")
        .push_u64(SNIPPET_BYTES)
        .push(")");
    Ok(sql)
}

/// The joined sidecar text for one projected schema, over alias `c`.
///
/// The same expression the vector is built from, so the snippet, the
/// substring arm and the stored vector cannot disagree about what a row's
/// searchable text is.
///
/// # Errors
///
/// `StorageError::Internal` for an invalid column identifier.
pub(crate) fn sidecar_text_sql(
    projection: &MemorySearchProjection,
) -> Result<SqlText, StorageError> {
    let fields = projection
        .fields
        .iter()
        .map(|field| (field.column.as_str(), field.kind))
        .collect::<Vec<_>>();
    search_text_expr(&fields)
}

/// The sidecar key column of a runtime search projection, validated.
///
/// The read-side twin of [`sidecar_memory_key`]. Freeze carried the
/// declared column onto [`MemorySearchProjection`]; every read statement
/// that joins or filters the sidecar spells it from here, so the substring
/// arm, the snippet lookup and the projection `INSERT` all key the sidecar
/// on the one column its `Surface` declares. Same rule as the write side:
/// no `t` fallback, because the fallback is the defect.
///
/// There is no "absent column" arm: freeze refuses a projected schema whose
/// sidecar is not `KeyShape::MemoryT`-keyed, so the field is a `String` and
/// the only thing left to check here is that it is a legal identifier.
///
/// # Errors
///
/// `StorageError::Internal` when the declared memory key column is not a
/// valid identifier.
pub(crate) fn projection_key_ident(
    projection: &MemorySearchProjection,
) -> Result<PgIdent, StorageError> {
    PgIdent::column(&projection.sidecar_key_column)
}

/// The boot half of the pin: what the generator says, and what the catalog
/// has, must be the same set of projection tables — in both directions.
///
/// A spiice linked without its baseline is a search that silently returns
/// nothing; a projection table left behind by a spiice that was unlinked is
/// rows nobody maintains. Both are the same kind of drift as a missing
/// migration, so they fail the same way: at boot, before a write.
///
/// # Errors
///
/// [`StorageError::Internal`] when a declared projection table, its index or
/// one of its columns is missing, or when a `<schema>.projection` table
/// exists that no linked spiice declares.
pub async fn ensure_projection_schema(
    pool: &sqlx::PgPool,
    contracts: &[&'static proxiima_core::spiice::SpiiceContract],
) -> Result<(), StorageError> {
    let declared = projection_tables(contracts);
    let found: Vec<String> = sqlx::query_scalar(
        "SELECT (table_schema || '.' || table_name)::text
           FROM information_schema.tables
          WHERE table_name = $1
          ORDER BY 1",
    )
    .bind(proxiima_core::spiice::PROJECTION_TABLE_NAME)
    .fetch_all(pool)
    .await
    .map_err(crate::error::map_err)?;
    if found != declared {
        return Err(StorageError::Internal(format!(
            "projection tables in the database do not match the linked spiices: \
             declared {declared:?}, found {found:?}; apply migrations before boot"
        )));
    }

    for contract in contracts {
        let Some(spec) = contract.projection.spec() else {
            continue;
        };
        let (schema, _) = split_qualified(spec.table)?;
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text
               FROM information_schema.columns
              WHERE table_schema = $1
                AND table_name = $2
              ORDER BY 1",
        )
        .bind(schema)
        .bind(proxiima_core::spiice::PROJECTION_TABLE_NAME)
        .fetch_all(pool)
        .await
        .map_err(crate::error::map_err)?;
        let emitted = projection_columns();
        if columns != emitted {
            return Err(StorageError::Internal(format!(
                "{} has columns {columns:?}, the generator emits {emitted:?}; \
                 the baseline was hand-edited away from the generator",
                spec.table
            )));
        }
        let index_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_indexes
                  WHERE schemaname = $1 AND indexname = $2
             )",
        )
        .bind(schema)
        .bind(spec.index)
        .fetch_one(pool)
        .await
        .map_err(crate::error::map_err)?;
        if !index_exists {
            return Err(StorageError::Internal(format!(
                "missing index {}.{} on {}; apply migrations before boot",
                schema, spec.index, spec.table
            )));
        }
    }
    Ok(())
}

/// The generator's column names, in the boot check's `ORDER BY 1` order.
fn projection_columns() -> Vec<&'static str> {
    let mut names = PROJECTION_TABLE
        .iter()
        .map(|column| column.name)
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::{
        projection_artifacts, projection_columns, projection_insert_sql, projection_vector_sql,
    };
    use proxiima_core::SPIICE_0;
    use proxiima_core::spiice::{SchemaContract, SearchProjectionDecl};

    /// `PROJECTION_MEMORY_COLUMN` names one thing, and everything that
    /// spells that thing agrees with it.
    ///
    /// Being a constant buys no agreement here: it is read in exactly one
    /// place (`ProjectionSpec::surface`). The FK name is an independent literal,
    /// and the generator writes `memory_id` inline in the `CREATE TABLE`
    /// column's name and definition, in its `PRIMARY KEY` and in the
    /// projection `INSERT`. Interpolating the constant into fixed SQL text
    /// would trade four spellings for four `format!` holes and buy nothing,
    /// so the agreement is asserted instead of engineered.
    ///
    /// The DDL is read out of the generator's own emitted artifact rather
    /// than restated here, so this cannot pass against text nobody emits.
    #[test]
    fn the_projection_column_name_has_one_spelling() {
        use proxiima_core::spiice::{
            PROJECTION_MEMORY_COLUMN, PROJECTION_MEMORY_FK, PROJECTION_TABLE_NAME,
        };

        assert_eq!(
            PROJECTION_MEMORY_FK,
            format!("{PROJECTION_TABLE_NAME}_{PROJECTION_MEMORY_COLUMN}_fkey"),
            "PostgreSQL mints an FK name as <table>_<column>_fkey, so these two \
             constants are one fact; they are separate literals, which is why this \
             is checked"
        );
        assert!(
            projection_columns().contains(&PROJECTION_MEMORY_COLUMN),
            "the catalog boot check compares against projection_columns(), so a \
             renamed column that missed that list would pass boot and fail the \
             surface"
        );

        let artifacts = projection_artifacts(&SPIICE_0)
            .expect("core artifacts")
            .expect("core declares a projection");
        let ddl = artifacts.table.forward.as_str();
        assert!(
            ddl.contains(&format!("{PROJECTION_MEMORY_COLUMN} ")),
            "the emitted CREATE TABLE declares the column this constant names: {ddl}"
        );
        assert!(
            ddl.contains(&format!(
                "PRIMARY KEY ({PROJECTION_MEMORY_COLUMN}, schema_id)"
            )),
            "and keys on it: {ddl}"
        );

        let schema = SPIICE_0
            .schemas
            .iter()
            .find(|schema| schema.search.is_projected())
            .expect("core projects at least one schema");
        let spec = SPIICE_0.projection.spec().expect("core declares a spec");
        let insert = projection_insert_sql(&SPIICE_0, schema).expect("the insert generates");
        let insert = insert.as_str();
        assert!(
            insert.contains(&format!("({PROJECTION_MEMORY_COLUMN}, schema_id,")),
            "and the projection INSERT writes it: {insert}"
        );

        assert_eq!(
            spec.surface().key,
            proxiima_core::spiice::KeyShape::MemoryT {
                column: PROJECTION_MEMORY_COLUMN,
            },
            "the one reader agrees with all of the above"
        );

        // Two surfaces, two key columns. The projection's own is this
        // constant and is fixed for every spiice; the SIDECAR's is whatever
        // that schema's `Surface` declares, and the `INSERT` reads it from
        // there. Asserting them together is what keeps a reader from
        // treating one as the other — the confusion that put a literal `t`
        // on the sidecar side of this statement.
        let sidecar_table = schema
            .sidecar_table
            .expect("a projected schema has a sidecar");
        let sidecar_key = SPIICE_0
            .sidecar_memory_key_column(sidecar_table)
            .expect("a projected sidecar declares a memory key");
        assert!(
            insert.contains(&format!("FROM {sidecar_table} c")),
            "the sidecar is the FROM: {insert}"
        );
        assert!(
            insert.contains(&format!(
                "JOIN proxiima_core.memory_including_forgotten m ON m.t = c.{sidecar_key}"
            )),
            "and the join reads the DECLARED sidecar column against the kernel's fixed \
             `t`: {insert}"
        );
    }

    const NOTE_SIDECAR: &str = "proxiima_core.agent_note_v1";

    /// Spiice #0's note declaration with its sidecar re-keyed, and nothing
    /// else changed — so an assertion that fails below is about the key
    /// column and nothing else.
    ///
    /// The contract is narrowed to the one schema so `surface_for` cannot
    /// answer from a sibling's declaration of the same table.
    fn a_contract_keying_the_note_on(
        key: proxiima_core::spiice::KeyShape,
    ) -> (
        proxiima_core::spiice::SpiiceContract,
        &'static SchemaContract,
    ) {
        let mut surface = SPIICE_0
            .surface_for(NOTE_SIDECAR)
            .expect("the note sidecar is declared");
        surface.key = key;
        let mut schema = *SPIICE_0
            .schemas
            .iter()
            .find(|schema| schema.sidecar_table == Some(NOTE_SIDECAR))
            .expect("agent_note_v1");
        schema.surfaces = Box::leak(Box::new([surface]));
        let schema: &'static SchemaContract = Box::leak(Box::new(schema));
        let mut contract = SPIICE_0;
        contract.schemas = std::slice::from_ref(schema);
        (contract, schema)
    }

    /// A sidecar the generator cannot key on a memory is a refusal that
    /// names the declaration to fix — never a fall back to `t`.
    ///
    /// The fallback is the defect: it writes a statement the contract did
    /// not ask for, which either files nothing or errors in the writing
    /// transaction, and in both cases the spiice's search is empty with
    /// nothing saying why.
    #[test]
    fn a_sidecar_that_is_not_memory_keyed_is_refused_by_name() {
        let (contract, schema) =
            a_contract_keying_the_note_on(proxiima_core::spiice::KeyShape::Custom(&["note_id"]));
        let err = projection_insert_sql(&contract, schema).expect_err("the generator refuses");
        let message = err.to_string();
        assert!(
            message.contains("KeyShape::MemoryT"),
            "the refusal names the declaration to fix: {message}"
        );
        assert!(
            message.contains("proxiima_core.agent_note_v1"),
            "and the surface it is about: {message}"
        );
    }

    /// Every artifact's inverse is derived from the same declaration, so a
    /// de-provisioning script is a reading of the generator rather than a
    /// second hand-maintained file.
    #[test]
    fn every_artifact_carries_its_inverse() {
        let artifacts = projection_artifacts(&SPIICE_0)
            .expect("artifacts")
            .expect("core declares a projection");
        assert_eq!(
            artifacts
                .inverse()
                .into_iter()
                .map(proxiima_core::SqlText::as_str)
                .collect::<Vec<_>>(),
            vec![
                "DROP TRIGGER IF EXISTS projection_remember_lang ON proxiima_core.projection;",
                "DROP INDEX IF EXISTS proxiima_core.core_projection_owner_tsv_gin_tag;",
                "DROP INDEX IF EXISTS proxiima_core.core_projection_owner_tsv_gin;",
                "DROP TABLE IF EXISTS proxiima_core.projection;",
            ]
        );
    }

    /// Spiice #0's contract with its projection table moved into a spiice's
    /// own schema, and nothing else changed.
    fn a_spiice_projection_contract() -> proxiima_core::spiice::SpiiceContract {
        let spec = *SPIICE_0.projection.spec().expect("core declares a spec");
        let mut contract = SPIICE_0;
        contract.projection =
            proxiima_core::spiice::ProjectionDecl::Table(proxiima_core::spiice::ProjectionSpec {
                table: "some_spiice.projection",
                index: "some_spiice_projection_owner_tsv_gin",
                ..spec
            });
        contract
    }

    /// A spiice's table names no core table: its keys into core go through
    /// `install_reference`, with the delete actions core's inline keys have.
    #[test]
    fn a_spiice_table_gets_its_core_keys_from_the_installer() {
        let artifacts = projection_artifacts(&a_spiice_projection_contract())
            .expect("artifacts")
            .expect("the contract declares a projection");
        assert_eq!(
            artifacts.table.forward.as_str(),
            "CREATE TABLE some_spiice.projection (
    memory_id        uuid      NOT NULL,
    schema_id        text      NOT NULL,
    owner_id         uuid      NOT NULL,
    search_tsv       tsvector  NOT NULL,
    tag              text[]    NOT NULL DEFAULT '{}',
    lexical_language regconfig NOT NULL DEFAULT proxiima_core.lexical_config(),
    PRIMARY KEY (memory_id, schema_id)
);
SELECT proxiima_core.install_reference('some_spiice', 'projection', 'memory_id', 'memory', 'cascade');
SELECT proxiima_core.install_reference('some_spiice', 'projection', 'owner_id', 'owner', 'no_action');
SELECT proxiima_core.install_reference('some_spiice', 'projection', 'lexical_language', 'lexical_language', 'no_action');"
        );
        // Everything else in the block that names core is a function the SQL
        // contract provides.
        let rendered = artifacts.render();
        let rest = [
            "proxiima_core.install_reference(",
            "proxiima_core.lexical_config()",
            "proxiima_core.remember_lexical_language()",
        ]
        .iter()
        .fold(rendered.as_str().to_owned(), |text, call| {
            text.replace(call, "")
        });
        assert!(
            !rest.contains("proxiima_core."),
            "the block names a core table: {}",
            rendered.as_str()
        );
    }

    #[test]
    fn the_maintenance_statement_takes_its_owner_from_the_memory_row() {
        let note = SPIICE_0
            .schemas
            .iter()
            .find(|schema| schema.sidecar_table == Some(NOTE_SIDECAR))
            .expect("agent_note_v1");
        let sql = projection_insert_sql(&SPIICE_0, note).expect("insert");
        let sql = sql.as_str();
        assert!(sql.contains("m.owner_id"), "owner comes from memory");
        assert!(
            sql.contains("JOIN proxiima_core.memory_including_forgotten m ON m.t = c.t"),
            "the owner is joined, never bound"
        );
        assert!(sql.contains("c.tags"), "the tag column is copied");
    }

    #[test]
    fn a_declared_non_surface_has_no_vector() {
        let decl = SearchProjectionDecl::None {
            why: "a fixture",
            tag_column: None,
        };
        assert!(projection_vector_sql(&decl).is_err());
    }
}
