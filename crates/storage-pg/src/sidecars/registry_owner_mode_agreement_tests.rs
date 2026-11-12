use super::{PgSidecarEntry, PgSidecarKey, PgSidecarRegistry, PgSidecarRegistryFrozen};
use crate::sidecars::SidecarOwnerMode;
use proxiima_core::spiice::{
    CounterRule, EmbeddingRecipe, EraseRule, ExportRule, ForgetRule, KeyShape, ProjectionDecl,
    Provenance, SchemaContract, SchemaRef, SearchProjectionDecl, SpiiceContract, Surface,
    TransferRule,
};
use proxiima_core::verbs::schema::PayloadKind;
use proxiima_core::{FactPayload, SchemaId, SchemaVersion, StorageError};
use std::collections::HashMap;

const SPIICE: &str = "test-owner-mode";
const TABLE: &str = "test_owner_mode.stamp_v1";
const SCHEMA_ID: &str = "test-owner-mode/stamp-v1";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StampFixture {
    mark: String,
}

impl proxiima_core::PayloadProjection for StampFixture {
    const KEY_FIELDS: &'static [proxiima_core::KeyField<Self>] = &[];
    fn display(&self) -> proxiima_core::PayloadDisplay {
        proxiima_core::PayloadDisplay {
            body: Some(self.mark.clone()),
            ..Default::default()
        }
    }
    fn durable(&self) -> Result<proxiima_core::DurablePayload, String> {
        let mut fields = proxiima_core::DurablePayloadBuilder::new();
        fields.field("mark", &self.mark)?;
        Ok(fields.finish())
    }
}

impl FactPayload for StampFixture {
    const SCHEMA_ID: &'static str = SCHEMA_ID;
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        self.mark.as_bytes().to_vec()
    }

    fn render(&self) -> String {
        self.mark.clone()
    }

    fn sidecar_table() -> Option<&'static str> {
        Some(TABLE)
    }

    fn json_schema() -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": { "mark": { "type": "string" } },
            "required": ["mark"],
        }))
    }
}

const fn surface(
    transfer: TransferRule,
    forget: ForgetRule,
    owner_column: Option<&'static str>,
) -> Surface {
    Surface {
        table: TABLE,
        key: KeyShape::MemoryT { column: "t" },
        owner_column,
        transfer,
        erase: EraseRule::ByKey,
        export: ExportRule::Rows,
        forget,
        lexical_language_column: None,
        counter: CounterRule::Counted("sidecar_rows"),
        completeness: None,
    }
}

fn pg_registry_with(owner: SidecarOwnerMode) -> PgSidecarRegistry {
    let key = PgSidecarKey::new(
        PayloadKind::Fact,
        SchemaId::new(SCHEMA_ID.to_owned()),
        SchemaVersion::new(1),
    );
    let entry = PgSidecarEntry {
        key: key.clone(),
        sidecar_table: TABLE.to_owned(),
        owner,
        fork: crate::sidecars::ForkPolicy::Refuse,
        memory_key_column: Some("t"),
        natural_key_declaration: proxiima_core::NaturalKeyDeclaration::EMPTY,
        memory_insert: Some(|_, _, _, _| Box::pin(async { Ok(()) })),
        memory_load: Some(|_, _| Box::pin(async { Ok(None) })),
        memory_load_batch: Some(|_, _, _| Box::pin(async { Ok(Vec::new()) })),
        goal_insert: None,
        goal_copy: None,
        projection_insert: None,
        projection_table: None,
        projection_language: None,
    };
    PgSidecarRegistry {
        entries: HashMap::from([(key, entry)]),
        evolutions: HashMap::new(),
    }
}

#[test]
fn follows_tables_omit_other_owner_modes_and_are_distinct_in_name_order() {
    let mut entries = HashMap::new();
    for (version, owner, table) in [
        (1, SidecarOwnerMode::Follows, "test_owner_mode.zeta_v1"),
        (2, SidecarOwnerMode::Pinned, "test_owner_mode.pinned_v1"),
        (3, SidecarOwnerMode::None, "test_owner_mode.plain_v1"),
        (4, SidecarOwnerMode::Follows, "test_owner_mode.alpha_v1"),
        (5, SidecarOwnerMode::Follows, "test_owner_mode.zeta_v1"),
    ] {
        let mut entry = pg_registry_with(owner)
            .entries
            .into_values()
            .next()
            .expect("one fixture registration");
        entry.key.schema_version = SchemaVersion::new(version);
        entry.sidecar_table = table.to_owned();
        entries.insert(entry.key.clone(), entry);
    }
    let frozen = PgSidecarRegistryFrozen {
        entries: std::sync::Arc::new(entries),
        ..PgSidecarRegistryFrozen::default()
    };
    assert_eq!(
        frozen.owner_follows_memory_sidecar_tables(),
        ["test_owner_mode.alpha_v1", "test_owner_mode.zeta_v1"]
    );
}

fn freeze_contract(contract: &'static SpiiceContract) -> proxiima_core::SpiiceRegistryFrozen {
    let mut registry = proxiima_core::SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<StampFixture>();
    registry.add_contract_or_panic_for_tests(contract);
    registry.freeze_or_panic_for_tests()
}

#[test]
fn freeze_refuses_follows_declared_retain_at_source() {
    static CONTRACT: SpiiceContract = SpiiceContract {
        retired_facts: &[],
        spiice_id: SPIICE,
        ordinal: 91,
        schemas: &[SchemaContract {
            id: SchemaRef::new(SPIICE, "stamp", 1),
            kind: PayloadKind::Fact,
            sidecar_table: Some(TABLE),
            search: SearchProjectionDecl::None {
                why: "owner-mode freeze fixture",
                tag_column: None,
            },
            embedding: EmbeddingRecipe::Never {
                why: "owner-mode freeze fixture",
            },
            transfer: TransferRule::RetainAtSource {
                why: "fixture: retain so Follows must disagree",
            },
            provenance: Provenance::None,
            surfaces: &[surface(
                TransferRule::RetainAtSource {
                    why: "fixture: retain so Follows must disagree",
                },
                ForgetRule::Keep {
                    why: "a retained sidecar stays with its source owner",
                },
                Some("owner_id"),
            )],
            natural_key_columns: proxiima_core::NaturalKeyDeclaration::EMPTY,
        }],
        state_surfaces: &[],
        scopes: &[],
        kernel_surfaces: &[],
        tools: &[],
        resources: &[],
        projection: ProjectionDecl::None {
            why: "owner-mode freeze fixture",
        },
        bespoke_erase_legs: &[],
        bespoke_transfer_legs: &[],
        evolutions: &[],
    };
    let frozen = freeze_contract(&CONTRACT);
    let err = pg_registry_with(SidecarOwnerMode::Follows)
        .check_owner_pinned_against_contracts(&frozen)
        .expect_err("Follows vs RetainAtSource");
    let StorageError::ConstraintViolation(message) = err else {
        panic!("expected ConstraintViolation, got {err:?}");
    };
    for named in ["OWNER=Follows", "RetainAtSource", TABLE] {
        assert!(message.contains(named), "refusal names {named}: {message}");
    }

    let mut reg = pg_registry_with(SidecarOwnerMode::Pinned);
    crate::sidecars::register_core_pg_sidecars(&mut reg);
    let frozen = reg
        .freeze_against(&frozen)
        .expect("the matching pinned registration freezes");
    assert!(frozen.owner_follows_memory_sidecar_tables().is_empty());
    assert_eq!(
        frozen.spiice_schema_names(),
        ["test_owner_mode"],
        "a composition with no Follows must still clear its stale marks"
    );
}

#[test]
fn freeze_refuses_follows_declared_keep() {
    static CONTRACT: SpiiceContract = SpiiceContract {
        retired_facts: &[],
        spiice_id: SPIICE,
        ordinal: 92,
        schemas: &[SchemaContract {
            id: SchemaRef::new(SPIICE, "stamp", 1),
            kind: PayloadKind::Fact,
            sidecar_table: Some(TABLE),
            search: SearchProjectionDecl::None {
                why: "owner-mode freeze fixture",
                tag_column: None,
            },
            embedding: EmbeddingRecipe::Never {
                why: "owner-mode freeze fixture",
            },
            transfer: TransferRule::Follow,
            provenance: Provenance::None,
            surfaces: &[surface(
                TransferRule::Follow,
                ForgetRule::Keep {
                    why: "fixture: Keep so Follows must disagree",
                },
                Some("owner_id"),
            )],
            natural_key_columns: proxiima_core::NaturalKeyDeclaration::EMPTY,
        }],
        state_surfaces: &[],
        scopes: &[],
        kernel_surfaces: &[],
        tools: &[],
        resources: &[],
        projection: ProjectionDecl::None {
            why: "owner-mode freeze fixture",
        },
        bespoke_erase_legs: &[],
        bespoke_transfer_legs: &[],
        evolutions: &[],
    };
    let frozen = freeze_contract(&CONTRACT);
    pg_registry_with(SidecarOwnerMode::Follows)
        .check_owner_pinned_against_contracts(&frozen)
        .expect("Follows matches Follow+owner_column");
    let err = pg_registry_with(SidecarOwnerMode::Follows)
        .check_keep_is_owner_pinned(&frozen)
        .expect_err("Follows + Keep");
    let StorageError::ConstraintViolation(message) = err else {
        panic!("expected ConstraintViolation, got {err:?}");
    };
    for named in ["OWNER=Follows", "ForgetRule::Keep", "Pinned"] {
        assert!(message.contains(named), "refusal names {named}: {message}");
    }
}

#[test]
fn freeze_admits_follows_with_follow_and_owner_column() {
    static CONTRACT: SpiiceContract = SpiiceContract {
        retired_facts: &[],
        spiice_id: SPIICE,
        ordinal: 93,
        schemas: &[SchemaContract {
            id: SchemaRef::new(SPIICE, "stamp", 1),
            kind: PayloadKind::Fact,
            sidecar_table: Some(TABLE),
            search: SearchProjectionDecl::None {
                why: "owner-mode freeze fixture",
                tag_column: None,
            },
            embedding: EmbeddingRecipe::Never {
                why: "owner-mode freeze fixture",
            },
            transfer: TransferRule::Follow,
            provenance: Provenance::None,
            surfaces: &[surface(
                TransferRule::Follow,
                ForgetRule::DumpThenDelete,
                Some("owner_id"),
            )],
            natural_key_columns: proxiima_core::NaturalKeyDeclaration::EMPTY,
        }],
        state_surfaces: &[Surface {
            table: "test_owner_state.jobs",
            key: KeyShape::Custom(&["id"]),
            erase: EraseRule::ByOwner,
            ..surface(
                TransferRule::RetainAtSource {
                    why: "state stays with its owner",
                },
                ForgetRule::Keep {
                    why: "state is independent of memory",
                },
                Some("owner_id"),
            )
        }],
        scopes: &[],
        kernel_surfaces: &[],
        tools: &[],
        resources: &[],
        projection: ProjectionDecl::None {
            why: "owner-mode freeze fixture",
        },
        bespoke_erase_legs: &[],
        bespoke_transfer_legs: &[],
        evolutions: &[],
    };
    let frozen = freeze_contract(&CONTRACT);
    let mut reg = pg_registry_with(SidecarOwnerMode::Follows);
    reg.check_owner_pinned_against_contracts(&frozen)
        .expect("Follows matches");
    reg.check_keep_is_owner_pinned(&frozen)
        .expect("DumpThenDelete admits Follows");
    crate::sidecars::register_core_pg_sidecars(&mut reg);
    let frozen = reg
        .freeze_against(&frozen)
        .expect("complete fixture coverage");
    assert_eq!(frozen.owner_follows_memory_sidecar_tables(), [TABLE]);
    assert_eq!(
        frozen.spiice_schema_names(),
        ["test_owner_mode", "test_owner_state"]
    );
    let amended =
        frozen.with_unusable_memory_table("test-owner-mode/unusable-v1", "unusable.sidecar_v1");
    assert_eq!(amended.spiice_schema_names(), frozen.spiice_schema_names());
}

/// `Follows` needs somewhere to stamp the owner. A `Follow` surface with
/// no `owner_column` never reaches the PG check: core freeze refuses it
/// first as a surface no transfer leg can move, so no frozen registry can
/// pair a `Follows` registration with one.
#[test]
fn freeze_refuses_follows_declared_follow_without_owner_column() {
    static CONTRACT: SpiiceContract = SpiiceContract {
        retired_facts: &[],
        spiice_id: SPIICE,
        ordinal: 94,
        schemas: &[SchemaContract {
            id: SchemaRef::new(SPIICE, "stamp", 1),
            kind: PayloadKind::Fact,
            sidecar_table: Some(TABLE),
            search: SearchProjectionDecl::None {
                why: "owner-mode freeze fixture",
                tag_column: None,
            },
            embedding: EmbeddingRecipe::Never {
                why: "owner-mode freeze fixture",
            },
            transfer: TransferRule::Follow,
            provenance: Provenance::None,
            surfaces: &[surface(
                TransferRule::Follow,
                ForgetRule::DumpThenDelete,
                None,
            )],
            natural_key_columns: proxiima_core::NaturalKeyDeclaration::EMPTY,
        }],
        state_surfaces: &[],
        scopes: &[],
        kernel_surfaces: &[],
        tools: &[],
        resources: &[],
        projection: ProjectionDecl::None {
            why: "owner-mode freeze fixture",
        },
        bespoke_erase_legs: &[],
        bespoke_transfer_legs: &[],
        evolutions: &[],
    };
    let mut registry = proxiima_core::SpiiceRegistry::new();
    registry.add_fact_schema_or_panic_for_tests::<StampFixture>();
    registry.add_contract_or_panic_for_tests(&CONTRACT);
    let err = registry
        .try_freeze()
        .expect_err("core freeze refuses a Follow surface without owner_column");
    assert!(
        matches!(
            err,
            proxiima_core::spiice::SpiiceRegistryError::UnmovableSurface {
                spiice_id: SPIICE,
                table: TABLE,
            }
        ),
        "expected UnmovableSurface for {TABLE}, got {err:?}"
    );
}

/// The symmetric case: a `Follow` surface with `owner_column` set expects
/// `Follows`, so a `None` registration (which never stamps the owner
/// column) is refused.
#[test]
fn freeze_refuses_none_declared_follow_with_owner_column() {
    static CONTRACT: SpiiceContract = SpiiceContract {
        retired_facts: &[],
        spiice_id: SPIICE,
        ordinal: 95,
        schemas: &[SchemaContract {
            id: SchemaRef::new(SPIICE, "stamp", 1),
            kind: PayloadKind::Fact,
            sidecar_table: Some(TABLE),
            search: SearchProjectionDecl::None {
                why: "owner-mode freeze fixture",
                tag_column: None,
            },
            embedding: EmbeddingRecipe::Never {
                why: "owner-mode freeze fixture",
            },
            transfer: TransferRule::Follow,
            provenance: Provenance::None,
            surfaces: &[surface(
                TransferRule::Follow,
                ForgetRule::DumpThenDelete,
                Some("owner_id"),
            )],
            natural_key_columns: proxiima_core::NaturalKeyDeclaration::EMPTY,
        }],
        state_surfaces: &[],
        scopes: &[],
        kernel_surfaces: &[],
        tools: &[],
        resources: &[],
        projection: ProjectionDecl::None {
            why: "owner-mode freeze fixture",
        },
        bespoke_erase_legs: &[],
        bespoke_transfer_legs: &[],
        evolutions: &[],
    };
    let frozen = freeze_contract(&CONTRACT);
    let err = pg_registry_with(SidecarOwnerMode::None)
        .check_owner_pinned_against_contracts(&frozen)
        .expect_err("None vs Follow with owner_column");
    let StorageError::ConstraintViolation(message) = err else {
        panic!("expected ConstraintViolation, got {err:?}");
    };
    for named in [
        "OWNER=None",
        "TransferRule::Follow with Surface.owner_column set",
        TABLE,
    ] {
        assert!(message.contains(named), "refusal names {named}: {message}");
    }
}
