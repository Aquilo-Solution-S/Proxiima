//! Spiice lifecycle scope declarations for `Proxiima.Payloads.scopeAdmissionOrder`.
//!
//! Sidecars need no foreign key to the scope registry. The shared fence
//! serializes scoped admissions with erasure of the same scope.

use uuid::Uuid;

/// The name of a spiice-owned lifecycle scope, e.g. `notebook`.
///
/// `&'static str` rather than an enum: the closed vocabulary lives in the
/// linked spiices, not in core, and a core enum would have to be edited to
/// add a scope to an out-of-tree spiice. The string is namespaced by
/// convention (`<spiice>-<scope>`) and hashed into the fence key, so two
/// spiices that pick the same name share a lane — which freeze refuses as a
/// duplicate declaration rather than letting it become a silent collision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeKind(&'static str);

impl ScopeKind {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for ScopeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// One scope a payload belongs to: which kind, and which row of it.
///
/// Read off a payload value, because the id is a payload field and the kind
/// is a property of the payload TYPE. A payload whose scope column is
/// nullable (a row that may span several scopes on purpose) returns
/// `None` for the rows that name no scope, and those rows are
/// unscoped in fact as well as in declaration.
///
/// Ordered so a batch's distinct scopes can be taken as one sorted,
/// deduplicated set: two writers whose batches overlap must not take the
/// same two keys in opposite orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeRef {
    pub kind: ScopeKind,
    pub id: Uuid,
}

impl ScopeRef {
    #[must_use]
    pub const fn new(kind: ScopeKind, id: Uuid) -> Self {
        Self { kind, id }
    }
}

impl std::fmt::Display for ScopeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind, self.id)
    }
}

/// The registry of one [`ScopeKind`], supplied by its owning spiice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeDecl {
    pub kind: ScopeKind,
    /// Schema-qualified registry table, e.g. `proxiima_notes.notebooks`.
    pub registry_table: &'static str,
    /// Column holding the scope id.
    pub id_column: &'static str,
    /// Column holding the owner id.
    pub owner_id_column: &'static str,
}

impl ScopeDecl {
    /// Report a name the scope registry cannot use.
    #[must_use]
    pub fn shape_error(&self) -> Option<&'static str> {
        if self.kind.as_str().is_empty() {
            return Some("scope kind must not be empty");
        }
        // `Proxiima.Payloads.scopeRegistryIdentifiers`.
        if !self.registry_table.contains('.')
            || crate::sql_text::PgIdent::table(self.registry_table).is_err()
        {
            return Some("registry table must be schema-qualified");
        }
        for column in [self.id_column, self.owner_id_column] {
            if crate::sql_text::PgIdent::column(column).is_err() {
                return Some("scope registry columns must be valid identifiers");
            }
        }
        None
    }
}

/// The sorted, deduplicated scope set of a batch.
///
/// A batch may span several scopes; taking their keys in one canonical
/// order is what keeps two crossed batches from forming a fence cycle. The
/// same rule the owner fence follows for its multi-owner arm.
#[must_use]
pub fn scope_set(scopes: impl IntoIterator<Item = ScopeRef>) -> Vec<ScopeRef> {
    let mut set: Vec<ScopeRef> = scopes.into_iter().collect();
    set.sort_unstable();
    set.dedup();
    set
}

#[cfg(test)]
mod tests {
    use super::{ScopeDecl, ScopeKind, ScopeRef, scope_set};
    use uuid::Uuid;

    const KIND: ScopeKind = ScopeKind::new("test-scope");

    #[test]
    fn a_scope_set_is_sorted_and_deduplicated() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let set = scope_set([
            ScopeRef::new(KIND, b),
            ScopeRef::new(KIND, a),
            ScopeRef::new(KIND, b),
        ]);
        assert_eq!(set, vec![ScopeRef::new(KIND, a), ScopeRef::new(KIND, b)]);
    }

    #[test]
    fn a_declaration_must_qualify_its_registry_table() {
        let decl = ScopeDecl {
            kind: KIND,
            registry_table: "repos",
            id_column: "repo_id",
            owner_id_column: "owner_id",
        };
        assert_eq!(
            decl.shape_error(),
            Some("registry table must be schema-qualified")
        );
        assert!(
            ScopeDecl {
                registry_table: "proxiima_notes.notebooks",
                ..decl
            }
            .shape_error()
            .is_none()
        );
    }

    #[test]
    fn a_declaration_rejects_malformed_registry_identifiers() {
        let decl = ScopeDecl {
            kind: KIND,
            registry_table: "proxiima_notes.notebooks",
            id_column: "repo_id",
            owner_id_column: "owner_id",
        };
        for registry_table in ["proxiima_notes.", ".notebooks", "a.b.c"] {
            assert!(
                ScopeDecl {
                    registry_table,
                    ..decl
                }
                .shape_error()
                .is_some()
            );
        }
        for column in ["", "with space", "a.b"] {
            assert!(
                ScopeDecl {
                    id_column: column,
                    ..decl
                }
                .shape_error()
                .is_some()
            );
            assert!(
                ScopeDecl {
                    owner_id_column: column,
                    ..decl
                }
                .shape_error()
                .is_some()
            );
        }
    }
}
