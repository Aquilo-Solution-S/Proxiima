//! Migration ledger and schema names, read as Postgres reads an identifier:
//! the one parser the spiice migration ledger and the host migration runner
//! share.

/// Whether `table_name` names core's ledger, `public._sqlx_migrations`, by
/// what it resolves to ([`parse_ledger_name`]).
#[must_use]
pub fn is_core_ledger(table_name: &str) -> bool {
    matches!(parse_ledger_name(table_name), Some((schema, table))
        if schema == "public" && table == "_sqlx_migrations")
}

/// Postgres' whitespace between the tokens of a name.
fn is_name_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// The `(schema, table)` a ledger name resolves to, by Postgres' rules for an
/// identifier: parts split on `.` outside double quotes, with whitespace
/// allowed around parts and dots; a quoted part keeps its case, with `""` for
/// `"`; an unquoted part folds to ASCII lower case; each is cut to 63 bytes
/// (`NAMEDATALEN` - 1) on a character boundary; a name without a schema is in
/// `public`, where a run pins `search_path`.
///
/// `None` for a name that is not one or two such parts. That says nothing
/// against the name (SQL accepts more spellings than this reads, comments
/// among them): it is not matched by what it resolves to.
#[must_use]
pub fn parse_ledger_name(name: &str) -> Option<(String, String)> {
    match <[String; 1]>::try_from(parse_name_parts(name)?) {
        Ok([table]) => Some(("public".to_owned(), table)),
        Err(parts) => <[String; 2]>::try_from(parts)
            .ok()
            .map(|[schema, table]| (schema, table)),
    }
}

/// The identifier parts of a dotted name, resolved as
/// [`parse_ledger_name`] describes; `None` for a spelling it does not read.
/// At least one part.
#[must_use]
pub fn parse_name_parts(name: &str) -> Option<Vec<String>> {
    const MAX_IDENTIFIER_BYTES: usize = 63;
    let mut parts: Vec<String> = Vec::new();
    let mut chars = name.chars().peekable();
    loop {
        while chars.next_if(|c| is_name_space(*c)).is_some() {}
        let mut part = String::new();
        if chars.next_if_eq(&'"').is_some() {
            loop {
                match chars.next()? {
                    '"' if chars.next_if_eq(&'"').is_some() => part.push('"'),
                    '"' => break,
                    c => part.push(c),
                }
            }
            if part.is_empty() {
                return None;
            }
        } else {
            while let Some(c) = chars.next_if(|c| *c != '.' && !is_name_space(*c)) {
                part.push(c);
            }
            let mut letters = part.chars();
            let starts = letters
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || !c.is_ascii());
            if !starts
                || !letters
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii())
            {
                return None;
            }
            part.make_ascii_lowercase();
        }
        let mut end = part.len().min(MAX_IDENTIFIER_BYTES);
        while !part.is_char_boundary(end) {
            end -= 1;
        }
        part.truncate(end);
        parts.push(part);
        while chars.next_if(|c| is_name_space(*c)).is_some() {}
        match chars.next() {
            None => break,
            Some('.') => {}
            Some(_) => return None,
        }
    }
    Some(parts)
}
