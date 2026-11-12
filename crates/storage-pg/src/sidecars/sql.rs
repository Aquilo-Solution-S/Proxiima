use std::num::NonZeroU16;

use super::{PgIdent, SqlText, StorageError};

/// Convert a byte slice into a fixed 32-byte array.
///
/// # Errors
///
/// Returns `StorageError::Internal` when the input is not exactly 32 bytes.
pub fn bytes32(bytes: &[u8], column: &str) -> Result<[u8; 32], StorageError> {
    <[u8; 32]>::try_from(bytes).map_err(|_| {
        StorageError::Internal(format!(
            "{column} must be exactly 32 bytes, got {}",
            bytes.len()
        ))
    })
}

/// Convert a signed SQL `integer` value into `u32`.
///
/// # Errors
///
/// Returns `StorageError::Internal` when the database value is negative.
pub fn int_to_u32(value: i32, column: &str) -> Result<u32, StorageError> {
    u32::try_from(value).map_err(|err| StorageError::Internal(format!("invalid {column}: {err}")))
}

/// Convert a signed SQL `bigint` value into `u64`.
///
/// # Errors
///
/// Returns `StorageError::Internal` when the database value is negative.
pub fn int_to_u64(value: i64, column: &str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|err| StorageError::Internal(format!("invalid {column}: {err}")))
}

/// Build a memory sidecar insert statement.
///
/// `stamp_owner` stamps the row with the owner that WROTE it, read out of
/// `proxiima_core.memory` in the same statement — so the value is the acting
/// owner at write time and no caller can supply it. Both [`super::SidecarOwnerMode::Pinned`]
/// and [`super::SidecarOwnerMode::Follows`] pass `true` and share this SELECT
/// arm: Pinned rows then stay with that owner across transfer, while Follows
/// rows are later rehomed by the existing `TransferLeg::Rehomed` path.
///
/// # Errors
///
/// Returns `StorageError::Internal` when the table, key column, or payload
/// column identifiers are not valid Postgres identifiers.
pub fn memory_insert_sql(
    table: &str,
    key_column: &str,
    columns: &[(&str, Option<&str>)],
    stamp_owner: bool,
) -> Result<SqlText, StorageError> {
    let table = PgIdent::table(table)?;
    let key_column = PgIdent::column(key_column)?;
    let columns = columns
        .iter()
        .map(|(column, cast)| Ok((PgIdent::column(column)?, *cast)))
        .collect::<Result<Vec<_>, StorageError>>()?;
    let mut sql = SqlText::new("INSERT INTO ");
    sql.push_ident(&table).push(" (").push_ident(&key_column);
    if stamp_owner {
        sql.push(", owner_id");
    }
    for (column, _) in &columns {
        sql.push(", ").push_ident(column);
    }
    if stamp_owner {
        sql.push(") SELECT $1, m.owner_id");
    } else {
        sql.push(") VALUES ($1");
    }
    for (index, (_, cast)) in columns.iter().enumerate() {
        let placeholder = u16::try_from(index + 2)
            .ok()
            .and_then(NonZeroU16::new)
            .ok_or_else(|| StorageError::Internal("too many sidecar columns".into()))?;
        sql.push(", ").push_param(placeholder);
        if let Some(pg_type) = cast {
            // The cast target is a (possibly schema-qualified) Postgres type
            // name supplied via `pg_sidecar_cast!`; validate it as an identifier
            // so a spiice author cannot splice arbitrary SQL through `$pg_type`.
            sql.push("::").push_ident(&PgIdent::table(pg_type)?);
        }
    }
    if stamp_owner {
        sql.push(" FROM proxiima_core.memory_including_forgotten m WHERE m.t = $1");
    } else {
        sql.push(")");
    }
    Ok(sql)
}

/// Build a batched memory sidecar select statement.
///
/// # Errors
///
/// Returns `StorageError::Internal` when the table or key column are not valid
/// Postgres identifiers. The `columns` entries are compile-time SELECT
/// projection expressions emitted by `pg_sidecar_select_col!` from
/// `$column:ident` tokens — bare `col` for most kinds, `col::text AS col` for
/// enum columns. Rust's identifier grammar makes them injection-safe, and
/// because the enum form is a projection expression (not a bare identifier) it
/// must NOT be routed through `PgIdent::column`.
pub fn memory_select_batch_sql(
    table: &str,
    key_column: &str,
    columns: &[&'static str],
) -> Result<SqlText, StorageError> {
    let table = PgIdent::table(table)?;
    let key_column = PgIdent::column(key_column)?;
    let mut sql = SqlText::new("SELECT ");
    sql.push_ident(&key_column);
    for column in columns {
        sql.push(", ").push(column);
    }
    sql.push(" FROM ")
        .push_ident(&table)
        .push(" WHERE ")
        .push_ident(&key_column)
        .push(" = ANY($1)");
    Ok(sql)
}

/// The owner-pinned variant of [`memory_select_batch_sql`].
///
/// Hydrates a row only while the Memory it hangs off is still held by the
/// owner that WROTE the row. That is the whole difference an owner-pinned
/// sidecar makes: an ordinary sidecar is a column on the Memory and follows
/// it, while this one records an act and stays with its actor. After a
/// transfer the two owners differ, the join drops the row, and the
/// destination's `get_memory`/`get_memories`/`query_memories` never sees
/// the prior owner's actor identities.
///
/// The predicate is the Memory's owner rather than the reader's, which is
/// stricter: a caller that can read BOTH owners still gets the row only
/// where it belongs.
///
/// # Errors
///
/// Returns `StorageError::Internal` when the table or key column are not
/// valid Postgres identifiers. `columns` are the same compile-time
/// projection expressions [`memory_select_batch_sql`] documents.
pub fn memory_select_batch_owner_pinned_sql(
    table: &str,
    key_column: &str,
    columns: &[&'static str],
) -> Result<SqlText, StorageError> {
    let table = PgIdent::table(table)?;
    let key_column = PgIdent::column(key_column)?;
    let mut sql = SqlText::new("SELECT s.");
    sql.push_ident(&key_column);
    for column in columns {
        // `s.` prefixes both projection shapes the macro emits: `col` and
        // the enum form `col::text AS col`.
        sql.push(", s.").push(column);
    }
    sql.push(" FROM ")
        .push_ident(&table)
        .push(
            " s \
           JOIN proxiima_core.memory_including_forgotten m \
             ON m.t = s.",
        )
        .push_ident(&key_column)
        .push(" AND m.owner_id = s.owner_id WHERE s.")
        .push_ident(&key_column)
        .push(" = ANY($1)");
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::{int_to_u32, int_to_u64};
    use proxiima_core::StorageError;

    /// `count(*)` arrives as a signed `bigint`; a negative one is refused as
    /// a backend fault, naming the column, rather than wrapped or clamped.
    #[test]
    fn a_negative_database_count_is_refused_not_wrapped() {
        assert_eq!(int_to_u64(0, "rows").expect("zero"), 0);
        assert_eq!(int_to_u64(412, "rows").expect("positive"), 412);
        assert_eq!(int_to_u32(7, "attempts").expect("positive"), 7);
        for error in [
            int_to_u64(-1, "rows").expect_err("negative"),
            int_to_u32(-1, "rows").map(u64::from).expect_err("negative"),
        ] {
            assert!(
                matches!(&error, StorageError::Internal(message) if message.contains("rows")),
                "{error:?}"
            );
        }
    }
}
