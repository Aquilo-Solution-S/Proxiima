//! SQL rendered from the pin-column table, [`MemoryPinField`].
//!
//! Every statement and definition that lists pin columns takes the list from
//! here. The statements the host runs are built at the call site; the text
//! the definitions carry is [`definition_artifacts`], which the migration
//! tests require `definitions/*.sql` to contain verbatim (whitespace runs
//! collapsed), so a new pin column cannot be added to the table and left out
//! of the database.

use proxiima_core::{EdgeKind, EntityKind, MemoryPinField, PinShape, PinTargetKind};

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

fn join_columns(
    fields: impl Iterator<Item = MemoryPinField>,
    row: &str,
    separator: &str,
) -> String {
    fields
        .map(|field| [row, field.spec().column].concat())
        .collect::<Vec<_>>()
        .join(separator)
}

/// `t, kind, schema_id` and every pin column, each prefixed with `row`
/// (`"m."`, or `""` for none): what a pin carrier decodes.
#[must_use]
pub fn pin_carrier_columns(row: &str) -> String {
    let pins = join_columns(fields(), row, ", ");
    [row, "t, ", row, "kind, ", row, "schema_id, ", &pins].concat()
}

/// Whether `row` pins any of `$1` as a target of `kind` (every kind when
/// `None`) and `target`: the overlap of each matching column, or `false`
/// when no column can hold such a pin.
#[must_use]
pub fn inbound_overlap(row: &str, target: PinTargetKind, kind: Option<EdgeKind>) -> String {
    let terms: Vec<String> = fields()
        .filter(|field| {
            field.target() == target && kind.is_none_or(|wanted| field.spec().kind == wanted)
        })
        .map(|field| {
            let spec = field.spec();
            match spec.shape {
                PinShape::Array => [row, spec.column, " && ", ARRAY_BOUND].concat(),
                PinShape::Scalar => [row, spec.column, "=ANY(", ARRAY_BOUND, ")"].concat(),
            }
        })
        .collect();
    if terms.is_empty() {
        "false".to_owned()
    } else {
        ["(", &terms.join(" OR "), ")"].concat()
    }
}

/// The Memory-target pin columns of kind Origin or Read that a lineage walk
/// follows, concatenated into one array.
#[must_use]
pub fn lineage_pins(row: &str) -> String {
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
pub fn lineage_origin_pins(row: &str) -> String {
    join_columns(
        memory_array_fields().filter(|field| field.spec().kind == EdgeKind::Origin),
        row,
        " || ",
    )
}

/// Whether `row` lists any of `$1` in a lineage pin column.
#[must_use]
pub fn lineage_overlap(row: &str) -> String {
    memory_array_fields()
        .filter(|field| matches!(field.spec().kind, EdgeKind::Origin | EdgeKind::Read))
        .map(|field| [row, field.spec().column, " && ", ARRAY_BOUND].concat())
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Whether `row` pins `target` (a SQL expression) through any column that
/// holds a Memory.
#[must_use]
pub fn memory_pin_predicate(row: &str, target: &str) -> String {
    fields()
        .filter(|field| field.target() == PinTargetKind::Memory)
        .map(|field| {
            let spec = field.spec();
            match spec.shape {
                PinShape::Array => [row, spec.column, " @> ARRAY[", target, "]"].concat(),
                PinShape::Scalar => [row, spec.column, " = ", target].concat(),
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// The targets `row` holds in `field` as a `uuid[]`: the array itself, or
/// the scalar as an array of at most one.
#[must_use]
pub fn pin_ids(row: &str, field: MemoryPinField) -> String {
    let spec = field.spec();
    let column = [row, spec.column].concat();
    match spec.shape {
        PinShape::Array => column,
        PinShape::Scalar => [
            "CASE WHEN ",
            &column,
            " IS NULL THEN '{}'::uuid[] ELSE ARRAY[",
            &column,
            "] END",
        ]
        .concat(),
    }
}

/// Every pin column of `row` as one `uuid[]`.
fn all_pins(row: &str) -> String {
    fields()
        .map(|field| pin_ids(row, field))
        .collect::<Vec<_>>()
        .join(" || ")
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
pub fn memory_pin_kinds_function() -> String {
    let mut text = String::from(
        "CREATE OR REPLACE FUNCTION proxiima_core.memory_pin_kinds(field text, source proxiima_core.memory_kind)\n \
RETURNS text[]\n LANGUAGE sql\n IMMUTABLE\n SET search_path TO 'pg_catalog'\nAS $function$\n    SELECT CASE field\n",
    );
    for field in fields() {
        let spec = field.spec();
        text.push_str(&["        WHEN '", spec.column, "' THEN CASE source\n"].concat());
        for source in [
            EntityKind::Fact,
            EntityKind::Abstraction,
            EntityKind::Perspective,
        ] {
            let labels: Vec<String> = field
                .admitted(source)
                .iter()
                .map(|kind| ["'", kind_label(*kind), "'"].concat())
                .collect();
            let array = if labels.is_empty() {
                "ARRAY[]::text[]".to_owned()
            } else {
                ["ARRAY[", &labels.join(", "), "]"].concat()
            };
            text.push_str(
                &[
                    "            WHEN '",
                    kind_label(source),
                    "' THEN ",
                    &array,
                    "\n",
                ]
                .concat(),
            );
        }
        text.push_str("        END\n");
    }
    text.push_str("    END\n$function$;");
    text
}

/// Every text the definitions carry from the table, in the order they appear
/// in the definitions.
#[must_use]
pub fn definition_artifacts() -> Vec<String> {
    vec![
        // functions.sql: schema-evolution `source_pins`.
        all_pins("source."),
        memory_pin_kinds_function(),
        // functions.sql: forget grounding dependers.
        ["(", &grounding_predicate("m.", "NEW.t"), ")"].concat(),
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
pub fn grounding_predicate(row: &str, target: &str) -> String {
    grounding_fields()
        .map(|field| [row, field.spec().column, " @> ARRAY[", target, "]"].concat())
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn changed_array_pins() -> String {
    array_fields()
        .map(|field| {
            let column = field.spec().column;
            ["NEW.", column, " IS DISTINCT FROM OLD.", column].concat()
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn pin_targets_declaration() -> String {
    [
        "pin_targets uuid[] := ARRAY[NEW.t] || ",
        &join_columns(array_fields(), "NEW.", " || "),
        ";",
    ]
    .concat()
}

fn memory_pins_declaration() -> String {
    [
        "memory_pins uuid[] := ",
        &join_columns(memory_array_fields(), "NEW.", " || "),
        ";",
    ]
    .concat()
}

fn goal_pins_declaration() -> String {
    [
        "goal_pins uuid[] := ",
        &join_columns(
            array_fields().filter(|field| field.target() == PinTargetKind::Goal),
            "NEW.",
            " || ",
        ),
        ";",
    ]
    .concat()
}

fn grounding_pins_declaration() -> String {
    [
        "grounding_pins uuid[] := ",
        &join_columns(grounding_fields(), "NEW.", " || "),
        ";",
    ]
    .concat()
}

fn update_trigger_columns() -> String {
    [
        "AFTER UPDATE OF ",
        &join_columns(fields(), "", ", "),
        " ON proxiima_core.memory",
    ]
    .concat()
}
