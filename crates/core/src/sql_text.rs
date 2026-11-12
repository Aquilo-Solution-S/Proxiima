//! Composed SQL text whose every byte is a program literal or a checked
//! identifier.
//!
//! Bind parameters cannot stand in for identifiers, so a statement that names
//! a build-time table or column must splice it. [`SqlText`] is the one place
//! such a statement is assembled. Every byte of it comes from a program
//! literal, a checked identifier, a number, a UUID, hex digits, a quoted
//! literal ([`SqlText::push_literal`], quote-doubled, for program data such
//! as a schema id; request values bind), another `SqlText` or a
//! [`sqlx::SqlStr`]. A `String` built at run time does not convert. The one
//! residual: a leaked heap string is `&'static str` to the compiler, as it is
//! for sqlx's own literal door. It is also the one place that tells sqlx the
//! text is safe: it implements [`sqlx::SqlSafeStr`], so a call site writes
//! `sqlx::query(text)`.
//!
//! Text is rewritten only before anything is spliced into it:
//! [`SqlText::fill`] takes a program literal and fills its placeholders in
//! one pass. There is no way to search and replace in composed text, which
//! could destroy a validated quote.
//!
//! ```
//! use proxiima_core::{PgIdent, SqlText};
//!
//! let table = PgIdent::table("proxiima_fixture.note_v1").unwrap();
//! let mut sql = SqlText::new("SELECT t FROM ");
//! sql.push_ident(&table).push(" WHERE t = $1");
//! assert_eq!(sql.as_str(), "SELECT t FROM proxiima_fixture.note_v1 WHERE t = $1");
//! ```
//!
//! A `String` built at run time does not make one:
//!
//! ```compile_fail
//! # use proxiima_core::SqlText;
//! let _ = SqlText::new(format!("SELECT {}", 1));
//! ```
//!
//! Nor does the conversion a caller of `impl Into<SqlText>` relies on:
//!
//! ```compile_fail
//! # use proxiima_core::SqlText;
//! let _: SqlText = format!("SELECT {}", 1).into();
//! ```

use std::num::NonZeroU16;

use sqlx::postgres::{PgArguments, Postgres};
use sqlx::query::{Query, QueryScalar};
use sqlx::{Encode, Type};

use crate::StorageError;

/// A Postgres identifier that is safe to splice into SQL text: a column name,
/// or a table name optionally qualified by one schema.
///
/// The constructors are the only way in, and each returns the value or the
/// reason it is refused, so a `PgIdent` that exists is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgIdent(String);

impl PgIdent {
    /// A table name, `name` or `schema.name`.
    ///
    /// # Errors
    ///
    /// `StorageError::Internal` when `value` is not one or two identifier
    /// parts.
    pub fn table(value: &str) -> Result<Self, StorageError> {
        if is_qualified_table_ident(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(StorageError::Internal(format!(
                "invalid table identifier: {value:?}"
            )))
        }
    }

    /// A column name: one identifier part, never qualified.
    ///
    /// # Errors
    ///
    /// `StorageError::Internal` when `value` is not one identifier part.
    pub fn column(value: &str) -> Result<Self, StorageError> {
        if is_ident_part(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(StorageError::Internal(format!(
                "invalid column identifier: {value:?}"
            )))
        }
    }

    /// One unqualified name: a schema, role, constraint or index that is a
    /// plain identifier. Spliced as written, like [`PgIdent::column`].
    ///
    /// # Errors
    ///
    /// `StorageError::Internal` when `value` is not one identifier part.
    pub fn name(value: &str) -> Result<Self, StorageError> {
        if is_ident_part(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(StorageError::Internal(format!(
                "invalid identifier: {value:?}"
            )))
        }
    }

    /// Any name, rendered quoted: `"` doubled inside, wrapped in `"`. For
    /// names that are not plain identifiers or that come from the catalog or
    /// configuration. A name that needs quoting renders exactly as
    /// `quote_ident`/`format('%I')` renders it; a plain name is quoted too,
    /// which is not byte-identical to `quote_ident` (use [`PgIdent::name`]
    /// where the text must not change). A quoted plain name names the same
    /// object as the unquoted one only when it is already lower case:
    /// unquoted `Foo` folds to `foo`, `"Foo"` does not.
    ///
    /// # Errors
    ///
    /// `StorageError::Internal` when `value` is empty, longer than 63 bytes
    /// (Postgres truncates silently, which would name another object) or
    /// contains NUL.
    pub fn quoted(value: &str) -> Result<Self, StorageError> {
        if value.is_empty() || value.len() > 63 || value.contains('\0') {
            return Err(StorageError::Internal(format!(
                "invalid quoted identifier: {value:?}"
            )));
        }
        Ok(Self(format!("\"{}\"", value.replace('"', "\"\""))))
    }

    /// The name as it is spliced: as written, or quoted for
    /// [`PgIdent::quoted`].
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_qualified_table_ident(value: &str) -> bool {
    let mut parts = value.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if !is_ident_part(first) {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(second), None) => is_ident_part(second),
        _ => false,
    }
}

fn is_ident_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// SQL text assembled from program literals and checked identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlText(String);

impl SqlText {
    /// Start a statement from a program literal.
    #[must_use]
    pub fn new(sql: &'static str) -> Self {
        Self(sql.to_owned())
    }

    /// Append a program literal.
    pub fn push(&mut self, sql: &'static str) -> &mut Self {
        self.0.push_str(sql);
        self
    }

    /// Append a checked identifier.
    pub fn push_ident(&mut self, ident: &PgIdent) -> &mut Self {
        self.0.push_str(ident.as_str());
        self
    }

    /// Append a number: a `LIMIT`, a dimension, a count, a timeout.
    pub fn push_u64(&mut self, n: u64) -> &mut Self {
        self.0.push_str(&n.to_string());
        self
    }

    /// Append a signed number: a flag word, an offset.
    pub fn push_i64(&mut self, n: i64) -> &mut Self {
        self.0.push_str(&n.to_string());
        self
    }

    /// Append a UUID in hyphenated lower-case form, without quotes: the caller
    /// writes the quotes as literals. A `Uuid` cannot be wrong text.
    pub fn push_uuid(&mut self, id: uuid::Uuid) -> &mut Self {
        self.0.push_str(&id.hyphenated().to_string());
        self
    }

    /// Append the placeholder `$n`. Postgres binds at most 65535 values, so
    /// `NonZeroU16` makes `$0` and an out-of-range index unrepresentable.
    pub fn push_param(&mut self, n: NonZeroU16) -> &mut Self {
        self.0.push('$');
        self.0.push_str(&n.to_string());
        self
    }

    /// Append bytes as lowercase hexadecimal digits: a digest inside a
    /// literal that cannot take a bind parameter.
    pub fn push_hex(&mut self, bytes: &[u8]) -> &mut Self {
        use std::fmt::Write as _;
        for byte in bytes {
            write!(self.0, "{byte:02x}").expect("writing into a String cannot fail");
        }
        self
    }

    /// Append a finite `f32` as `Display` renders it (`0.1`, `32`).
    ///
    /// # Errors
    ///
    /// `StorageError::Internal` for NaN or an infinity, which are not SQL
    /// numbers.
    pub fn push_f32(&mut self, n: f32) -> Result<&mut Self, StorageError> {
        use std::fmt::Write as _;
        if !n.is_finite() {
            return Err(StorageError::Internal(format!("not a SQL number: {n}")));
        }
        write!(self.0, "{n}").expect("writing into a String cannot fail");
        Ok(self)
    }

    /// Append a finite `f32` rounded to `places` decimals (`0.50`).
    ///
    /// # Errors
    ///
    /// As [`SqlText::push_f32`].
    pub fn push_f32_fixed(&mut self, n: f32, places: u8) -> Result<&mut Self, StorageError> {
        use std::fmt::Write as _;
        if !n.is_finite() {
            return Err(StorageError::Internal(format!("not a SQL number: {n}")));
        }
        write!(self.0, "{n:.*}", usize::from(places)).expect("writing into a String cannot fail");
        Ok(self)
    }

    /// Append a text as a SQL literal, `'` doubled. For a name or label that
    /// is data of the program (a schema id), not a request value: those bind.
    ///
    /// A text without a backslash is emitted as `'…'`. A text with one is
    /// emitted as an escape-string `E'…'` with each `\` and each `'` doubled,
    /// which reads the same under either `standard_conforming_strings`.
    ///
    /// # Errors
    ///
    /// `StorageError::Internal` when the text holds NUL, which a literal
    /// cannot carry.
    pub fn push_literal(&mut self, text: &str) -> Result<&mut Self, StorageError> {
        if text.contains('\0') {
            return Err(StorageError::Internal(format!(
                "invalid SQL literal: {text:?}"
            )));
        }
        if text.contains('\\') {
            self.0.push('E');
        }
        self.0.push('\'');
        for c in text.chars() {
            if matches!(c, '\'' | '\\') {
                self.0.push(c);
            }
            self.0.push(c);
        }
        self.0.push('\'');
        Ok(self)
    }

    /// Append another composed statement as a single-quoted literal, `'`
    /// doubled: SQL text that a function body runs with `EXECUTE`. The
    /// backslashes of the statement stay as written, so the literal reads as
    /// written under `standard_conforming_strings = on`, the server default
    /// since 9.1, which the store's functions already rely on.
    pub fn push_sql_literal(&mut self, sql: &SqlText) -> &mut Self {
        self.0.push('\'');
        self.0.push_str(&sql.0.replace('\'', "''"));
        self.0.push('\'');
        self
    }

    /// Append another composed statement.
    pub fn push_text(&mut self, other: &SqlText) -> &mut Self {
        self.0.push_str(&other.0);
        self
    }

    /// A statement from a program literal whose placeholders are filled with
    /// `parts` in one pass: the one way to rewrite text. The template is
    /// scanned once, left to right; at each position the earliest placeholder
    /// wins (a tie goes to the longest), its part is appended and scanning
    /// resumes after the placeholder in the template. Inserted text is never
    /// rescanned, so a part that contains another placeholder stays as it is.
    /// A fixed word is a part made with `SqlText::new("word")`, a name one
    /// made with `SqlText::from(&ident)`.
    ///
    /// # Panics
    ///
    /// When a placeholder is empty.
    #[must_use]
    pub fn fill(template: &'static str, parts: &[(&'static str, &SqlText)]) -> Self {
        assert!(
            parts.iter().all(|(placeholder, _)| !placeholder.is_empty()),
            "a placeholder must not be empty"
        );
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        loop {
            let next = parts
                .iter()
                .filter_map(|(placeholder, part)| {
                    rest.find(placeholder)
                        .map(|at| (at, placeholder.len(), part))
                })
                .min_by_key(|&(at, len, _)| (at, std::cmp::Reverse(len)));
            let Some((at, len, part)) = next else {
                out.push_str(rest);
                return Self(out);
            };
            out.push_str(&rest[..at]);
            out.push_str(&part.0);
            rest = &rest[at + len..];
        }
    }

    /// The text, for logging and tests. It cannot be modified through this.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&PgIdent> for SqlText {
    fn from(ident: &PgIdent) -> Self {
        Self(ident.as_str().to_owned())
    }
}

impl From<&'static str> for SqlText {
    fn from(sql: &'static str) -> Self {
        Self::new(sql)
    }
}

/// A `SqlStr` is already safe-asserted text: only a literal or this module can
/// make one. An embedded migration's `sql` is one.
impl From<sqlx::SqlStr> for SqlText {
    fn from(sql: sqlx::SqlStr) -> Self {
        Self(sql.as_str().to_owned())
    }
}

impl sqlx::SqlSafeStr for SqlText {
    fn into_sql_str(self) -> sqlx::SqlStr {
        // The only `AssertSqlSafe` for composed SQL: `SqlText` holds only
        // program literals, checked identifiers, numbers, UUIDs, hex digits,
        // quote-doubled literals, other `SqlText` and `sqlx::SqlStr`.
        sqlx::AssertSqlSafe(self.0).into_sql_str()
    }
}

/// A statement under construction with bound values: the typed counterpart
/// of `sqlx::QueryBuilder<Postgres>`, which accepts any `Display`.
///
/// Text enters as `&'static str`, [`PgIdent`] or [`SqlText`]; request values
/// enter only through [`SqlBuilder::push_bind`].
pub struct SqlBuilder {
    inner: sqlx::QueryBuilder<Postgres>,
}

impl std::fmt::Debug for SqlBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlBuilder")
            .field("sql", &self.sql())
            .finish()
    }
}

impl SqlBuilder {
    /// Start a statement from a program literal.
    #[must_use]
    pub fn new(sql: &'static str) -> Self {
        Self {
            inner: sqlx::QueryBuilder::new(sql),
        }
    }

    /// Append a program literal.
    pub fn push(&mut self, sql: &'static str) -> &mut Self {
        self.inner.push(sql);
        self
    }

    /// Append a checked identifier.
    pub fn push_ident(&mut self, ident: &PgIdent) -> &mut Self {
        self.inner.push(ident.as_str());
        self
    }

    /// Append a composed statement.
    pub fn push_text(&mut self, text: &SqlText) -> &mut Self {
        self.inner.push(text.as_str());
        self
    }

    /// Append the next placeholder and bind `value` to it.
    pub fn push_bind<'t, T>(&mut self, value: T) -> &mut Self
    where
        T: Encode<'t, Postgres> + Type<Postgres>,
    {
        self.inner.push_bind(value);
        self
    }

    /// The text so far, for logging and tests.
    #[must_use]
    pub fn sql(&self) -> String {
        self.inner.sql().as_str().to_owned()
    }

    /// Finish into a query.
    pub fn build(&mut self) -> Query<'_, Postgres, PgArguments> {
        self.inner.build()
    }

    /// Finish into a query returning one column.
    pub fn build_query_scalar<'q, T>(&'q mut self) -> QueryScalar<'q, Postgres, T, PgArguments>
    where
        (T,): for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow>,
    {
        self.inner.build_query_scalar()
    }
}

#[cfg(test)]
mod tests {
    use super::{PgIdent, SqlBuilder, SqlText};
    use std::num::NonZeroU16;

    #[test]
    fn uuid_is_appended_hyphenated_lower_case_without_quotes() {
        let id = uuid::Uuid::parse_str("0A1B2C3D-0000-4000-8000-00000000ABCD").unwrap();
        let mut sql = SqlText::new("'");
        sql.push_uuid(id).push("'");
        assert_eq!(sql.as_str(), "'0a1b2c3d-0000-4000-8000-00000000abcd'");
    }

    #[test]
    fn composed_text_is_the_fragments_and_names_in_order() {
        let table = PgIdent::table("proxiima_fixture.summary_v1").unwrap();
        let key = PgIdent::column("repo_id").unwrap();
        let mut sql = SqlText::new("DELETE FROM ");
        sql.push_ident(&table)
            .push(" WHERE ")
            .push_ident(&key)
            .push(" = $1");
        assert_eq!(
            sql.as_str(),
            "DELETE FROM proxiima_fixture.summary_v1 WHERE repo_id = $1"
        );
    }

    #[test]
    fn table_accepts_schema_qualified_identifiers() {
        assert_eq!(
            PgIdent::table("proxiima_fixture.summary_v1")
                .unwrap()
                .as_str(),
            "proxiima_fixture.summary_v1"
        );
        assert_eq!(PgIdent::table("_local").unwrap().as_str(), "_local");
    }

    #[test]
    fn table_rejects_loose_or_malformed_identifiers() {
        for value in ["", "a..b", "1.2", "schema.", ".table", "a.b.c", "a-b"] {
            assert!(PgIdent::table(value).is_err(), "{value:?} should fail");
        }
    }

    #[test]
    fn column_rejects_qualified_identifiers() {
        assert!(PgIdent::column("repo_id").is_ok());
        assert!(PgIdent::column("sidecar.repo_id").is_err());
    }

    #[test]
    fn numbers_and_placeholders_render_as_decimal_text() {
        let mut sql = SqlText::new("SELECT ");
        sql.push_param(NonZeroU16::new(7).unwrap())
            .push(" LIMIT ")
            .push_u64(u64::MAX);
        assert_eq!(sql.as_str(), "SELECT $7 LIMIT 18446744073709551615");
        assert!(NonZeroU16::new(0).is_none());
    }

    #[test]
    fn a_composed_statement_appends_whole() {
        let mut inner = SqlText::new("SELECT 1");
        inner.push(" WHERE t = $1");
        let mut sql = SqlText::new("EXPLAIN ");
        sql.push_text(&inner);
        assert_eq!(sql.as_str(), "EXPLAIN SELECT 1 WHERE t = $1");
    }

    #[test]
    fn fill_does_not_rescan_an_inserted_name() {
        let schema = SqlText::from(&PgIdent::quoted("{runtime}").unwrap());
        let runtime = SqlText::from(&PgIdent::quoted("rt").unwrap());
        let sql = SqlText::fill(
            "GRANT USAGE ON SCHEMA {schema} TO {runtime}",
            &[("{schema}", &schema), ("{runtime}", &runtime)],
        );
        assert_eq!(
            sql.as_str(),
            "GRANT USAGE ON SCHEMA \"{runtime}\" TO \"rt\""
        );
    }

    #[test]
    fn fill_fills_a_placeholder_used_twice() {
        let schema = SqlText::from(&PgIdent::name("tenant_a").unwrap());
        let sql = SqlText::fill(
            "CREATE SCHEMA {s}; CREATE TABLE {s}.t (x int)",
            &[("{s}", &schema)],
        );
        assert_eq!(
            sql.as_str(),
            "CREATE SCHEMA tenant_a; CREATE TABLE tenant_a.t (x int)"
        );
    }

    #[test]
    fn fill_returns_a_template_without_the_placeholder_unchanged() {
        let schema = SqlText::from(&PgIdent::name("tenant_a").unwrap());
        let sql = SqlText::fill("SELECT 1", &[("{s}", &schema)]);
        assert_eq!(sql.as_str(), "SELECT 1");
    }

    #[test]
    fn fill_prefers_the_earliest_then_the_longest_placeholder() {
        let short = SqlText::new("short");
        let long = SqlText::new("long");
        let sql = SqlText::fill(
            "{a}{ab} {a}",
            &[("{a", &short), ("{ab}", &long), ("{a}", &long)],
        );
        assert_eq!(sql.as_str(), "longlong long");
    }

    #[test]
    fn fill_rewrites_a_literal_word_by_a_static_part() {
        let to = SqlText::new("-- new");
        let sql = SqlText::fill("SELECT 1; -- old; -- old", &[("-- old", &to)]);
        assert_eq!(sql.as_str(), "SELECT 1; -- new; -- new");
    }

    #[test]
    fn name_is_one_plain_part() {
        assert_eq!(
            PgIdent::name("proxiima_core").unwrap().as_str(),
            "proxiima_core"
        );
        for value in ["", "a.b", "a-b", "1a", "a b", "a\"b"] {
            assert!(PgIdent::name(value).is_err(), "{value:?} should fail");
        }
    }

    #[test]
    fn quoted_renders_what_quote_ident_renders_for_a_name_that_needs_quoting() {
        for (value, expected) in [
            ("Mixed Case", "\"Mixed Case\""),
            ("a\"b", "\"a\"\"b\""),
            ("\"", "\"\"\"\""),
            ("with-dash.dot", "\"with-dash.dot\""),
            ("select", "\"select\""),
            ("ünï", "\"ünï\""),
        ] {
            assert_eq!(PgIdent::quoted(value).unwrap().as_str(), expected);
        }
    }

    #[test]
    fn quoted_refuses_empty_nul_and_over_63_bytes() {
        let longest = "a".repeat(63);
        assert_eq!(
            PgIdent::quoted(&longest).unwrap().as_str(),
            format!("\"{longest}\"")
        );
        // 64 bytes, and 32 two-byte characters = 64 bytes.
        for value in [
            "a".repeat(64),
            "é".repeat(32),
            String::new(),
            "a\0b".to_owned(),
        ] {
            assert!(PgIdent::quoted(&value).is_err(), "{value:?} should fail");
        }
        assert!(PgIdent::quoted(&"é".repeat(31)).is_ok());
    }

    #[test]
    fn the_builder_numbers_binds_and_splices_only_checked_text() {
        let table = PgIdent::table("proxiima_fixture.note_v1").unwrap();
        let key = PgIdent::column("t").unwrap();
        let mut b = SqlBuilder::new("SELECT t FROM ");
        b.push_ident(&table)
            .push(" s WHERE s.")
            .push_ident(&key)
            .push(" = ");
        b.push_bind(1_i64)
            .push(" AND x = ")
            .push_bind("y".to_owned());
        let tail = SqlText::new(" LIMIT 1");
        b.push_text(&tail);
        assert_eq!(
            b.sql(),
            "SELECT t FROM proxiima_fixture.note_v1 s WHERE s.t = $1 AND x = $2 LIMIT 1"
        );
        let _ = b.build();
        let mut scalar = SqlBuilder::new("SELECT ");
        scalar.push_bind(5_i32);
        let _ = scalar.build_query_scalar::<i32>();
    }

    #[test]
    fn hex_renders_lowercase_two_digit_bytes() {
        let mut sql = SqlText::new("'");
        sql.push_hex(&[0x00, 0x0f, 0xab, 0xff]).push("'");
        assert_eq!(sql.as_str(), "'000fabff'");
    }

    #[test]
    fn numbers_render_as_display_or_fixed_and_refuse_non_finite() {
        let mut sql = SqlText::new("");
        sql.push_f32(0.1).unwrap().push(" ").push_f32(32.0).unwrap();
        sql.push(" ").push_f32_fixed(0.5, 2).unwrap();
        assert_eq!(sql.as_str(), "0.1 32 0.50");
        assert!(SqlText::new("").push_f32(f32::NAN).is_err());
        assert!(SqlText::new("").push_f32_fixed(f32::INFINITY, 2).is_err());
    }

    #[test]
    fn literals_are_plain_or_escape_strings_and_refuse_nul() {
        let literal = |text: &str| {
            let mut sql = SqlText::new("");
            sql.push_literal(text).unwrap();
            sql.as_str().to_owned()
        };
        assert_eq!(literal("core/note-v1"), "'core/note-v1'");
        assert_eq!(literal("it's"), "'it''s'");
        assert_eq!(literal(r"a\b"), r"E'a\\b'");
        assert_eq!(literal(r"a\b'c"), r"E'a\\b''c'");
        assert!(SqlText::new("").push_literal("a\0b").is_err());
    }

    #[test]
    fn a_statement_embeds_as_a_literal() {
        let inner = SqlText::new("SELECT 'x' ESCAPE '\\'");
        let mut sql = SqlText::new("EXECUTE ");
        sql.push_sql_literal(&inner);
        assert_eq!(sql.as_str(), "EXECUTE 'SELECT ''x'' ESCAPE ''\\'''");
    }
}
