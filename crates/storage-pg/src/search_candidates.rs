//! Fixed, contract-derived candidate functions carried by migrations.
//!
//! A function accepts search values and narrowing filters, never SQL or a
//! relation name. Its private execution role can scan the derived projection;
//! ordinary memory RLS authorizes every id before the function returns it.

use proxiima_core::spiice::SpiiceContract;
use proxiima_core::{PgIdent, SqlText, StorageError, verbs::query::SearchOrder};

use crate::projection::Artifact;
use crate::verbs::query::{
    LexicalCandidates, RankedCandidates, StagedCandidates, SubstringCandidates,
};

/// The fixed `PostgreSQL` identity arguments of both candidate functions.
pub const ARGUMENT_TYPES: &str = "text, bigint, text[], timestamp with time zone, timestamp with time zone, uuid, uuid[], text[], boolean, boolean, boolean, text, boolean";

const PARAMETERS: &str = "query_text text, candidate_limit bigint, requested_tags text[], since_time timestamptz, until_time timestamptz, before_id uuid, requested_owners uuid[], requested_schemas text[], rescue boolean, recency boolean, heads_only boolean, requested_kind text, tag_all boolean";

/// One generated function and the exact SQL body installed for it.
///
/// The body is produced alongside the installation artifact so catalog
/// validation does not need to parse generated SQL or maintain a second
/// definition source.
#[derive(Debug, Clone)]
pub(crate) struct CandidateDefinition {
    pub(crate) signature: SqlText,
    pub(crate) body: SqlText,
    pub(crate) kind: CandidateKind,
    pub(crate) artifact: Artifact,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CandidateKind {
    Lexical,
    Substring,
}

impl CandidateKind {
    pub(crate) fn result_columns(self) -> &'static str {
        match self {
            Self::Lexical => "t uuid, schema_id text, lexical_score real",
            Self::Substring => "t uuid, lexical_score real",
        }
    }

    /// The function's volatility: its `CREATE FUNCTION` keyword and the
    /// `pg_proc.provolatile` code that keyword stores. Every statement of a
    /// `STABLE` function reads its caller's snapshot, so under `READ
    /// COMMITTED` a commit between two lexical stages cannot add, drop or move
    /// a row between them.
    pub(crate) fn volatility(self) -> (&'static str, &'static str) {
        match self {
            Self::Lexical => (" STABLE", "s"),
            Self::Substring => ("", "v"),
        }
    }
}

/// Qualified function name derived from a projection's schema.
///
/// # Errors
/// Refuses a projection table that is not a qualified `PostgreSQL` identifier.
pub fn function_name(table: &str, substring: bool) -> Result<PgIdent, StorageError> {
    PgIdent::table(table)?;
    let (schema, _) = table.split_once('.').ok_or_else(|| {
        StorageError::Internal("candidate projection must be schema-qualified".into())
    })?;
    let name = if substring {
        "search_substring_candidates"
    } else {
        "search_lexical_candidates"
    };
    PgIdent::table(&format!("{schema}.{name}"))
}

/// The statement that calls the candidate function for `table`.
///
/// # Errors
/// As [`function_name`].
pub(crate) fn call_sql(table: &str, substring: bool) -> Result<SqlText, StorageError> {
    let name = function_name(table, substring)?;
    let mut sql = SqlText::new("SELECT ");
    sql.push(if substring {
        "t, lexical_score"
    } else {
        "t, schema_id, lexical_score"
    });
    sql.push(" FROM ").push_ident(&name).push("(");
    for (index, argument) in ARGUMENT_TYPES.split(", ").enumerate() {
        if index > 0 {
            sql.push(", ");
        }
        let cast = match argument {
            "timestamp with time zone" => "timestamptz",
            other => other,
        };
        sql.push("$")
            .push_u64(index as u64 + 1)
            .push("::")
            .push(cast);
    }
    sql.push(")");
    Ok(sql)
}

/// Generate the lexical and declared substring functions for one spiice.
///
/// Migration files carry these artifacts. Serving boot only verifies them;
/// this function never accesses a database or installs objects at runtime.
///
/// # Errors
/// Refuses invalid declared identifiers or inconsistent search projections.
pub fn candidate_artifacts(
    contract: &'static SpiiceContract,
) -> Result<Vec<Artifact>, StorageError> {
    let Some(spec) = contract.projection.spec() else {
        return Ok(Vec::new());
    };
    let projections = proxiima_core::verbs::schema::contract_search_projections(&[contract])
        .map_err(|error| StorageError::Internal(error.to_string()))?;
    candidate_artifacts_for_projection(spec.table, spec.overfetch_k, &projections)
}

/// Generate current candidate definitions for catalog application and checks.
pub(crate) fn candidate_definitions(
    contract: &'static SpiiceContract,
) -> Result<Vec<CandidateDefinition>, StorageError> {
    let Some(spec) = contract.projection.spec() else {
        return Ok(Vec::new());
    };
    let projections = proxiima_core::verbs::schema::contract_search_projections(&[contract])
        .map_err(|error| StorageError::Internal(error.to_string()))?;
    candidate_definitions_for_projection(spec.table, spec.overfetch_k, &projections)
}

/// Candidate artifacts for one synthetic projection used by database fixtures.
///
/// # Errors
/// Refuses invalid identifiers or incomplete fixture metadata.
#[cfg(any(test, feature = "testkit"))]
#[doc(hidden)]
pub fn candidate_artifacts_for_tests(
    projections: &[proxiima_core::verbs::schema::MemorySearchProjection],
) -> Result<Vec<Artifact>, StorageError> {
    let Some(head) = projections.first() else {
        return Ok(Vec::new());
    };
    candidate_artifacts_for_projection(&head.projection_table, head.overfetch_k, projections)
}

fn candidate_artifacts_for_projection(
    table: &str,
    cap: u32,
    projections: &[proxiima_core::verbs::schema::MemorySearchProjection],
) -> Result<Vec<Artifact>, StorageError> {
    Ok(
        candidate_definitions_for_projection(table, cap, projections)?
            .into_iter()
            .map(|definition| definition.artifact)
            .collect(),
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the statement text is the length of the function"
)]
fn candidate_definitions_for_projection(
    table: &str,
    cap: u32,
    projections: &[proxiima_core::verbs::schema::MemorySearchProjection],
) -> Result<Vec<CandidateDefinition>, StorageError> {
    let mut definitions = Vec::new();
    for substring in [false, true] {
        let (template, candidate_tables) = if substring {
            let Some(template) = crate::verbs::query::projection_substring_candidates(projections)?
            else {
                continue;
            };
            // The candidate role reads the core sidecars through their
            // trigram indexes, which row security would otherwise keep out of
            // reach. A spiice's sidecar has no such index (see `SubstringArm`)
            // and stays under owner row security.
            let mut tables = vec![table.to_owned()];
            tables.extend(
                template
                    .legs
                    .iter()
                    .filter(|leg| leg.table.starts_with("proxiima_core."))
                    .map(|leg| leg.table.clone()),
            );
            (substring_execution(&template)?, tables)
        } else {
            let Some(template) = crate::verbs::query::projection_ranked_candidates(projections)?
            else {
                continue;
            };
            (execution(&template), vec![table.to_owned()])
        };
        let name = function_name(table, substring)?;
        let kind = if substring {
            CandidateKind::Substring
        } else {
            CandidateKind::Lexical
        };
        let returns = kind.result_columns();
        let mut cap_sql = SqlText::new("");
        cap_sql.push_u64(u64::from(cap));
        let cap = cap_sql;
        // The name and all body identifiers are checked by the renderer.
        let mut body = SqlText::new(
            "\nBEGIN\n\
                 PERFORM proxiima_core.assert_search_scope(requested_owners);\n\
                 IF candidate_limit IS NULL OR candidate_limit < 1 OR candidate_limit > ",
        );
        body.push_text(&cap)
            .push(
                " THEN\n\
                     RAISE EXCEPTION 'candidate limit must be between 1 and ",
            )
            .push_text(&cap)
            .push(
                "' USING ERRCODE = 'PX002';\n\
                 END IF;\n\
                 ",
            )
            .push_text(&template)
            .push(
                "\n\
             END\n",
            );
        let mut function_ddl = SqlText::new("CREATE OR REPLACE FUNCTION ");
        function_ddl
            .push_ident(&name)
            .push("(")
            .push(PARAMETERS)
            .push(
                ")\n\
             RETURNS TABLE(",
            )
            .push(returns)
            .push(")\nLANGUAGE plpgsql")
            .push(kind.volatility().0)
            .push(
                " SECURITY DEFINER\n\
             SET search_path TO pg_catalog, pg_temp\n\
             AS $candidate_function$",
            )
            .push_text(&body)
            .push("$candidate_function$;");
        let mut signature = SqlText::new("");
        signature
            .push_ident(&name)
            .push("(")
            .push(ARGUMENT_TYPES)
            .push(")");
        let mut read_tables = vec![
            "proxiima_core.memory",
            "proxiima_core.memory_head",
            "proxiima_core.lexical_languages",
            "proxiima_core.lexical_default",
            table,
        ];
        read_tables.extend(
            projections
                .iter()
                .map(|projection| projection.sidecar_table.as_str()),
        );
        read_tables.sort_unstable();
        read_tables.dedup();
        let schema = table
            .split_once('.')
            .ok_or_else(|| StorageError::Internal("unqualified projection".into()))?
            .0;
        let installed = installation_sql(
            schema,
            &function_ddl,
            &signature,
            &read_tables,
            &candidate_tables
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )?;
        let mut drop_function = SqlText::new("DROP FUNCTION IF EXISTS ");
        drop_function
            .push_ident(&name)
            .push("(")
            .push(ARGUMENT_TYPES)
            .push(");");
        let artifact = Artifact {
            forward: installed,
            inverse: drop_function,
        };
        definitions.push(CandidateDefinition {
            signature,
            body,
            kind,
            artifact,
        });
    }
    Ok(definitions)
}

/// The `USING` clause of one candidate statement: `$1` the query, `$2` the
/// limit, `$3`..`$12` the request's filters in parameter order, then `extra`.
fn using(query: &'static str, limit: &'static str, extra: &'static str) -> SqlText {
    SqlText::fill(
        "USING {query}, {limit}, requested_tags, since_time, until_time, before_id, \
         requested_owners, requested_schemas, rescue, heads_only, requested_kind, \
         tag_all{extra};",
        &[
            ("{query}", &SqlText::new(query)),
            ("{limit}", &SqlText::new(limit)),
            ("{extra}", &SqlText::new(extra)),
        ],
    )
}

/// EXECUTE plans each search using its actual values. A generic plan cannot
/// hide the tag/text condition behind optional-filter switches.
fn execution(candidates: &LexicalCandidates) -> SqlText {
    match candidates {
        LexicalCandidates::SinglePass(query) => single_pass_execution(query),
        LexicalCandidates::Staged(stages) => staged_execution(stages),
    }
}

fn single_pass_execution(query: &RankedCandidates) -> SqlText {
    let mut sql = SqlText::new("RETURN QUERY EXECUTE ");
    sql.push_sql_literal(&query.body)
        .push(" || CASE WHEN recency THEN ' ORDER BY ")
        .push(RankedCandidates::order_sql(SearchOrder::Recency))
        .push("' ELSE ' ORDER BY ")
        .push(RankedCandidates::order_sql(SearchOrder::Relevance))
        .push("' END || ' LIMIT $2' ")
        .push_text(&using("query_text", "candidate_limit", ""));
    sql
}

/// The staged lexical search: the query analyzed once, then only the
/// statements the request needs.
///
/// In relevance order each exact match ranks above each rescue match, so the
/// exact stage runs first and the rescue stage runs only for the rows left
/// in the window, over the rows the exact query does not match. A rescue
/// query that adds no row to the exact one (a single word, or only stop
/// words) runs no rescue stage; equality compares `tsquerysend(...)` because
/// `PostgreSQL`'s `tsquery` `=` ignores prefix (`:*`) and weight modifiers.
/// Recency order interleaves the two kinds, so one statement over both serves
/// it.
fn staged_execution(stages: &StagedCandidates) -> SqlText {
    let bound = ", exact_tsq, rescue_tsq";
    SqlText::fill(
        "DECLARE
    analyzed text := {analyzed};
    exact_tsq tsquery := {exact_tsq};
    rescue_tsq tsquery;
    exact_count bigint := 0;
    do_rescue boolean := COALESCE(rescue, false);
    do_recency boolean := COALESCE(recency, false);
BEGIN
IF do_recency AND do_rescue THEN
    rescue_tsq := {rescue_tsq};
    IF rescue_tsq IS NOT NULL AND tsquerysend(rescue_tsq) IS DISTINCT FROM tsquerysend(exact_tsq) THEN
        RETURN QUERY EXECUTE {merged} || ' ORDER BY {recency} LIMIT $2' {using}
        RETURN;
    END IF;
END IF;
IF exact_tsq IS NOT NULL AND exact_tsq <> ''::tsquery THEN
    RETURN QUERY EXECUTE {exact} || CASE WHEN do_recency THEN ' ORDER BY {recency}' ELSE ' ORDER BY {relevance}' END || ' LIMIT $2' {using}
    GET DIAGNOSTICS exact_count = ROW_COUNT;
END IF;
IF do_recency OR NOT do_rescue OR exact_count >= candidate_limit THEN
    RETURN;
END IF;
rescue_tsq := {rescue_tsq};
IF rescue_tsq IS NULL OR tsquerysend(rescue_tsq) IS NOT DISTINCT FROM tsquerysend(exact_tsq) THEN
    RETURN;
END IF;
RETURN QUERY EXECUTE {rescue} || ' ORDER BY {relevance} LIMIT $2' {rescue_using}
END;",
        &[
            ("{analyzed}", &stages.analyzed_sql("query_text")),
            ("{exact_tsq}", &stages.exact_tsquery_sql("analyzed")),
            ("{rescue_tsq}", &stages.rescue_tsquery_sql("analyzed")),
            ("{merged}", &literal(&stages.merged.body)),
            ("{exact}", &literal(&stages.exact.body)),
            ("{rescue}", &literal(&stages.rescue.body)),
            (
                "{recency}",
                &SqlText::new(RankedCandidates::order_sql(SearchOrder::Recency)),
            ),
            (
                "{relevance}",
                &SqlText::new(RankedCandidates::order_sql(SearchOrder::Relevance)),
            ),
            ("{using}", &using("analyzed", "candidate_limit", bound)),
            (
                "{rescue_using}",
                &using("analyzed", "candidate_limit - exact_count", bound),
            ),
        ],
    )
}

/// A statement as the quoted text an `EXECUTE` runs.
fn literal(statement: &SqlText) -> SqlText {
    let mut sql = SqlText::new("");
    sql.push_sql_literal(statement);
    sql
}

/// The substring arm plans one statement per request over only the legs whose
/// schemas the request names: each leg is `m.schema_id = ANY($8)` and
/// `= ANY(<its own schemas>)`, so a leg the request does not name matches no
/// row, and planning it (about 2 ms of row-security expansion) buys nothing.
fn substring_execution(candidates: &SubstringCandidates) -> Result<SqlText, StorageError> {
    let (open, between, close) = SubstringCandidates::frame();
    let mut sql = SqlText::new(
        "DECLARE legs text[] := ARRAY[]::text[];
BEGIN
",
    );
    for (index, leg) in candidates.legs.iter().enumerate() {
        if index > 0 {
            sql.push("\n");
        }
        sql.push("IF requested_schemas && ARRAY[");
        for (n, schema) in leg.schemas.iter().enumerate() {
            if n > 0 {
                sql.push(", ");
            }
            sql.push_literal(schema)?;
        }
        let mut wrapped = SqlText::new("(");
        wrapped.push_text(&leg.sql).push(")");
        sql.push("]::text[] THEN legs := legs || ")
            .push_sql_literal(&wrapped)
            .push("::text; END IF;");
    }
    sql.push(
        "
IF cardinality(legs) = 0 THEN RETURN; END IF;
RETURN QUERY EXECUTE ",
    )
    .push_sql_literal(&SqlText::new(open))
    .push(" || array_to_string(legs, ")
    .push_sql_literal(&SqlText::new(between))
    .push(") || ")
    .push_sql_literal(&SqlText::new(close))
    .push(" || CASE WHEN recency THEN ' ORDER BY ")
    .push(SubstringCandidates::order_sql(SearchOrder::Recency))
    .push("' ELSE ' ORDER BY ")
    .push(SubstringCandidates::order_sql(SearchOrder::Relevance))
    .push("' END || ' LIMIT $2' ")
    .push_text(&using("query_text", "candidate_limit", ""))
    .push("\nEND;");
    Ok(sql)
}

/// Install only generated functions while assuming the non-inheritable owner.
/// CREATE is temporary; subsequent definition passes use the same path.
pub(crate) fn installation_sql(
    schema: &str,
    function_ddl: &SqlText,
    signature: &SqlText,
    read_tables: &[&str],
    candidate_tables: &[&str],
) -> Result<SqlText, StorageError> {
    let schema = PgIdent::name(schema)?;
    let mut grants = SqlText::new("");
    for (index, table) in read_tables.iter().enumerate() {
        if index > 0 {
            grants.push(", ");
        }
        grants.push_ident(&PgIdent::table(table)?);
    }
    let mut policies = SqlText::new("");
    for (index, table) in candidate_tables.iter().enumerate() {
        let table = PgIdent::table(table)?;
        if index > 0 {
            policies.push("\n");
        }
        // The candidate table is contract-derived.
        policies
            .push("DROP POLICY IF EXISTS proxiima_search_candidates ON ")
            .push_ident(&table)
            .push(";\nCREATE POLICY proxiima_search_candidates ON ")
            .push_ident(&table)
            .push(" FOR SELECT TO proxiima_search USING (true);");
    }
    let mut sql = SqlText::new(
        "DO $install_candidate$\n\
         DECLARE original_role name := current_user;\n\
         BEGIN\n\
             GRANT USAGE, CREATE ON SCHEMA ",
    );
    sql.push_ident(&schema)
        .push(
            " TO proxiima_search;\n\
             GRANT SELECT ON ",
        )
        .push_text(&grants)
        .push(
            " TO proxiima_search;\n\
             ",
        )
        .push_text(&policies)
        .push(
            "\n\
             SET LOCAL ROLE proxiima_search;\n\
             EXECUTE $generated_function$",
        )
        .push_text(function_ddl)
        .push(
            "$generated_function$;\n\
             REVOKE ALL ON FUNCTION ",
        )
        .push_text(signature)
        .push(
            " FROM PUBLIC;\n\
             EXECUTE format('SET LOCAL ROLE %I', original_role);\n\
             REVOKE CREATE ON SCHEMA ",
        )
        .push_ident(&schema)
        .push(
            " FROM proxiima_search;\n\
         END\n\
         $install_candidate$;",
        );
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::call_sql;

    /// The call text is the statement the plan pins were written against: the
    /// checked function name is the only part that is not a literal.
    #[test]
    fn the_call_names_the_checked_candidate_function() {
        assert_eq!(
            call_sql("proxiima_fixture.note_projection", false)
                .unwrap()
                .as_str(),
            "SELECT t, schema_id, lexical_score FROM proxiima_fixture.search_lexical_candidates($1::text, $2::bigint, $3::text[], $4::timestamptz, $5::timestamptz, $6::uuid, $7::uuid[], $8::text[], $9::boolean, $10::boolean, $11::boolean, $12::text, $13::boolean)"
        );
        assert_eq!(
            call_sql("proxiima_fixture.note_projection", true)
                .unwrap()
                .as_str(),
            "SELECT t, lexical_score FROM proxiima_fixture.search_substring_candidates($1::text, $2::bigint, $3::text[], $4::timestamptz, $5::timestamptz, $6::uuid, $7::uuid[], $8::text[], $9::boolean, $10::boolean, $11::boolean, $12::text, $13::boolean)"
        );
        assert!(call_sql("unqualified", false).is_err());
        assert!(call_sql("a.b; DROP TABLE x", false).is_err());
    }

    /// The lexical function stages only when its exact band's rendered floor
    /// lies above everything its rescue band renders; at a shared edge or an
    /// overlap it runs the single pass, which interleaves the two kinds.
    #[test]
    fn only_disjoint_bands_stage_the_lexical_search() {
        use crate::verbs::query::{LexicalCandidates, projection_ranked_candidates};

        let note = proxiima_core::SpiiceRegistry::new()
            .freeze_or_panic_for_tests()
            .search_projections()
            .iter()
            .find(|projection| projection.schema_id.as_str() == "core/agent-note-v1")
            .expect("core/agent-note-v1 is a search surface")
            .clone();
        let shipped = note.render_bands.expect("core declares render bands");
        let staged = |exact_floor: f32, rescue_ceiling: f32| {
            let mut bands = shipped;
            bands.exact.floor = exact_floor;
            bands.rescue.ceiling = rescue_ceiling;
            let mut projection = note.clone();
            projection.render_bands = Some(bands);
            match projection_ranked_candidates(&[projection]).expect("candidates") {
                Some(LexicalCandidates::Staged(_)) => true,
                Some(LexicalCandidates::SinglePass(_)) => false,
                None => panic!("the note projection has a ranked arm"),
            }
        };
        assert!(
            staged(shipped.exact.floor, shipped.rescue.ceiling),
            "0.50 over 0.45"
        );
        assert!(staged(0.46, 0.45), "one rendered place apart");
        assert!(!staged(0.45, 0.45), "a shared edge");
        assert!(
            !staged(0.454, 0.451),
            "declared apart, rendered at one edge"
        );
        assert!(!staged(0.40, 0.45), "an overlap");
    }
}
