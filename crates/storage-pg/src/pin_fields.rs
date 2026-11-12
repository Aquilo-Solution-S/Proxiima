//! SQL rendered from the pin-column table, [`MemoryPinField`].
//!
//! Every statement and definition that lists pin columns takes the list from
//! here. The statements the host runs are built at the call site; the text
//! the definitions carry is [`definition_artifacts`], which the migration
//! tests require `definitions/*.sql` to contain verbatim (whitespace runs
//! collapsed), so a new pin column cannot be added to the table and left out
//! of the database.

use proxiima_core::{EdgeKind, EntityKind, MemoryPinField, PinShape, PinTargetKind, SqlText};

const ARRAY_BOUND: &str = "$1::uuid[]";

fn fields() -> impl Iterator<Item = MemoryPinField> {
    MemoryPinField::ALL.into_iter()
}

fn array_fields() -> impl Iterator<Item = MemoryPinField> {
    fields().filter(|field| field.spec().shape == PinShape::Array)
}

fn memory_array_fields() -> impl Iterator<Item = MemoryPinField> {
    array_fields().filter(|field| field.target() == PinTargetKind::Memory)
}

/// The columns of `fields`, each prefixed with `row`, between `separator`s.
fn join_columns(
    fields: impl Iterator<Item = MemoryPinField>,
    row: &'static str,
    separator: &'static str,
) -> SqlText {
    let mut sql = SqlText::new("");
    for (index, field) in fields.enumerate() {
        if index > 0 {
            sql.push(separator);
        }
        sql.push(row).push(field.spec().column);
    }
    sql
}

/// The terms joined by `separator`.
fn join_terms(terms: impl Iterator<Item = SqlText>, separator: &'static str) -> SqlText {
    let mut sql = SqlText::new("");
    for (index, term) in terms.enumerate() {
        if index > 0 {
            sql.push(separator);
        }
        sql.push_text(&term);
    }
    sql
}

/// `t, kind, schema_id` and every pin column, each prefixed with `row`
/// (`"m."`, or `""` for none): what a pin carrier decodes.
#[must_use]
pub fn pin_carrier_columns(row: &'static str) -> SqlText {
    let mut sql = SqlText::new("");
    sql.push(row)
        .push("t, ")
        .push(row)
        .push("kind, ")
        .push(row)
        .push("schema_id, ")
        .push_text(&join_columns(fields(), row, ", "));
    sql
}

/// Whether `row` pins any of `$1` as a target of `kind` (every kind when
/// `None`) and `target`: the overlap of each matching column, or `false`
/// when no column can hold such a pin.
#[must_use]
pub fn inbound_overlap(
    row: &'static str,
    target: PinTargetKind,
    kind: Option<EdgeKind>,
) -> SqlText {
    let terms = fields()
        .filter(|field| {
            field.target() == target && kind.is_none_or(|wanted| field.spec().kind == wanted)
        })
        .map(|field| {
            let spec = field.spec();
            let mut term = SqlText::new(row);
            match spec.shape {
                PinShape::Array => {
                    term.push(spec.column).push(" && ").push(ARRAY_BOUND);
                }
                PinShape::Scalar => {
                    term.push(spec.column)
                        .push("=ANY(")
                        .push(ARRAY_BOUND)
                        .push(")");
                }
            }
            term
        })
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return SqlText::new("false");
    }
    let mut sql = SqlText::new("(");
    sql.push_text(&join_terms(terms.into_iter(), " OR "))
        .push(")");
    sql
}

/// The Memory-target pin columns of kind Origin or Read that a lineage walk
/// follows, concatenated into one array.
#[must_use]
pub fn lineage_pins(row: &'static str) -> SqlText {
    join_columns(
        memory_array_fields()
            .filter(|field| matches!(field.spec().kind, EdgeKind::Origin | EdgeKind::Read)),
        row,
        " || ",
    )
}

/// The lineage pin columns of kind Origin, concatenated; a lineage pin
/// outside them is a read.
#[must_use]
pub fn lineage_origin_pins(row: &'static str) -> SqlText {
    join_columns(
        memory_array_fields().filter(|field| field.spec().kind == EdgeKind::Origin),
        row,
        " || ",
    )
}

/// Whether `row` lists any of `$1` in a lineage pin column.
#[must_use]
pub fn lineage_overlap(row: &'static str) -> SqlText {
    join_terms(
        memory_array_fields()
            .filter(|field| matches!(field.spec().kind, EdgeKind::Origin | EdgeKind::Read))
            .map(|field| {
                let mut term = SqlText::new(row);
                term.push(field.spec().column)
                    .push(" && ")
                    .push(ARRAY_BOUND);
                term
            }),
        " OR ",
    )
}

/// Whether `row` pins `target` (a SQL expression) through any column that
/// holds a Memory.
#[must_use]
pub fn memory_pin_predicate(row: &'static str, target: &'static str) -> SqlText {
    join_terms(
        fields()
            .filter(|field| field.target() == PinTargetKind::Memory)
            .map(|field| {
                let spec = field.spec();
                let mut term = SqlText::new(row);
                match spec.shape {
                    PinShape::Array => {
                        term.push(spec.column)
                            .push(" @> ARRAY[")
                            .push(target)
                            .push("]");
                    }
                    PinShape::Scalar => {
                        term.push(spec.column).push(" = ").push(target);
                    }
                }
                term
            }),
        " OR ",
    )
}

/// The targets `row` holds in `field` as a `uuid[]`: the array itself, or
/// the scalar as an array of at most one.
#[must_use]
pub fn pin_ids(row: &'static str, field: MemoryPinField) -> SqlText {
    let spec = field.spec();
    let mut sql = SqlText::new("");
    match spec.shape {
        PinShape::Array => {
            sql.push(row).push(spec.column);
        }
        PinShape::Scalar => {
            sql.push("CASE WHEN ")
                .push(row)
                .push(spec.column)
                .push(" IS NULL THEN '{}'::uuid[] ELSE ARRAY[")
                .push(row)
                .push(spec.column)
                .push("] END");
        }
    }
    sql
}

/// Every pin column of `row` as one `uuid[]`.
fn all_pins(row: &'static str) -> SqlText {
    join_terms(fields().map(|field| pin_ids(row, field)), " || ")
}

fn grounding_fields() -> impl Iterator<Item = MemoryPinField> {
    memory_array_fields().filter(|field| field.spec().kind != EdgeKind::Read)
}

/// The label the database stores for `kind`.
fn kind_label(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Fact => "fact",
        EntityKind::Abstraction => "abstraction",
        EntityKind::Perspective => "perspective",
        EntityKind::Goal => "goal",
    }
}

/// `proxiima_core.memory_pin_kinds(field, source)`: the kinds a memory of
/// kind `source` may pin through the column `field`.
#[must_use]
pub fn memory_pin_kinds_function() -> SqlText {
    let mut text = SqlText::new(
        "CREATE OR REPLACE FUNCTION proxiima_core.memory_pin_kinds(field text, source proxiima_core.memory_kind)\n \
RETURNS text[]\n LANGUAGE sql\n IMMUTABLE\n SET search_path TO 'pg_catalog'\nAS $function$\n    SELECT CASE field\n",
    );
    for field in fields() {
        let spec = field.spec();
        text.push("        WHEN '")
            .push(spec.column)
            .push("' THEN CASE source\n");
        for source in [
            EntityKind::Fact,
            EntityKind::Abstraction,
            EntityKind::Perspective,
        ] {
            text.push("            WHEN '")
                .push(kind_label(source))
                .push("' THEN ARRAY[");
            let admitted = field.admitted(source);
            if admitted.is_empty() {
                text.push("]::text[]");
            } else {
                for (index, kind) in admitted.iter().enumerate() {
                    if index > 0 {
                        text.push(", ");
                    }
                    text.push("'").push(kind_label(*kind)).push("'");
                }
                text.push("]");
            }
            text.push("\n");
        }
        text.push("        END\n");
    }
    text.push("    END\n$function$;");
    text
}

/// Every text the definitions carry from the table, in the order they appear
/// in the definitions.
#[must_use]
pub fn definition_artifacts() -> Vec<SqlText> {
    let mut grounding = SqlText::new("(");
    grounding
        .push_text(&grounding_predicate("m.", "NEW.t"))
        .push(")");
    vec![
        // functions.sql: schema-evolution `source_pins`.
        all_pins("source."),
        memory_pin_kinds_function(),
        // functions.sql: forget grounding dependers.
        grounding,
        // functions.sql: append-only comparison.
        changed_array_pins(),
        // functions.sql: `memory_pin_checks` variables.
        pin_targets_declaration(),
        memory_pins_declaration(),
        goal_pins_declaration(),
        grounding_pins_declaration(),
        // triggers.sql: the pin-check update trigger.
        update_trigger_columns(),
        // policies.sql: the erased-target read policy.
        memory_pin_predicate("pinning.", "erased_pin_target.t"),
    ]
}

/// Whether `row` lists `target` in a grounding column.
#[must_use]
pub fn grounding_predicate(row: &'static str, target: &'static str) -> SqlText {
    join_terms(
        grounding_fields().map(|field| {
            let mut term = SqlText::new(row);
            term.push(field.spec().column)
                .push(" @> ARRAY[")
                .push(target)
                .push("]");
            term
        }),
        " OR ",
    )
}

fn changed_array_pins() -> SqlText {
    join_terms(
        array_fields().map(|field| {
            let column = field.spec().column;
            let mut term = SqlText::new("NEW.");
            term.push(column)
                .push(" IS DISTINCT FROM OLD.")
                .push(column);
            term
        }),
        " OR ",
    )
}

fn declaration(name: &'static str, columns: &SqlText) -> SqlText {
    let mut sql = SqlText::new(name);
    sql.push_text(columns).push(";");
    sql
}

fn pin_targets_declaration() -> SqlText {
    declaration(
        "pin_targets uuid[] := ARRAY[NEW.t] || ",
        &join_columns(array_fields(), "NEW.", " || "),
    )
}

fn memory_pins_declaration() -> SqlText {
    declaration(
        "memory_pins uuid[] := ",
        &join_columns(memory_array_fields(), "NEW.", " || "),
    )
}

fn goal_pins_declaration() -> SqlText {
    declaration(
        "goal_pins uuid[] := ",
        &join_columns(
            array_fields().filter(|field| field.target() == PinTargetKind::Goal),
            "NEW.",
            " || ",
        ),
    )
}

fn grounding_pins_declaration() -> SqlText {
    declaration(
        "grounding_pins uuid[] := ",
        &join_columns(grounding_fields(), "NEW.", " || "),
    )
}

fn update_trigger_columns() -> SqlText {
    let mut sql = SqlText::new("AFTER UPDATE OF ");
    sql.push_text(&join_columns(fields(), "", ", "))
        .push(" ON proxiima_core.memory");
    sql
}
