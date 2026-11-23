//! Contracts shaped to leave one lifecycle leg uncovered.
//!
//! Each is a surface the generic erase, transfer or forget loop skips and
//! no bespoke leg claims. Unrefused they freeze cleanly and boot cleanly;
//! what they produce is rows that outlive the operation meant to reach
//! them, which no later assertion is positioned to see.

use super::fixtures::{
    FIXTURE_SPIICE, erase_fixture, erase_fixture_at, state_surface, transfer_fixture,
    transfer_fixture_at,
};
use crate::SearchProjectionColumnKind;
use crate::spiice::contract::CORE_ORDINAL;
use crate::spiice::contract::{
    CounterRule, DbConstraint, EmbeddingRecipe, EraseRule, ExportRule, ForgetRule, KeyShape,
    LanguagePolicy, ProjectionDecl, Provenance, SchemaContract, SchemaRef, SearchProjectionDecl,
    SpiiceContract, SubstringArm, Surface, TransferRule, WeightedField,
};
use crate::verbs::schema::PayloadKind;

/// The out-of-tree spiice this whole check exists for: `ByKey` on a key
/// the erase builds no selection set for, claimed by no bespoke leg.
/// Both generic loops skip it, so nothing deletes these rows and the
/// erase still reports `Completed`.
pub(super) static UNDELETABLE_SURFACE: SpiiceContract = erase_fixture(
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::Custom(&["thing_id"]),
        EraseRule::ByKey,
        ExportRule::Rows,
    )],
    &[],
);

/// A bespoke leg claiming a table the spiice does not declare — the
/// stale name that let the hand-written lists rot.
pub(super) static BESPOKE_LEG_FOR_NOTHING: SpiiceContract = erase_fixture_at(
    CORE_ORDINAL,
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::ByKey,
        ExportRule::Rows,
    )],
    &["test_spiice.gone_v1"],
);

/// A `Follow` surface whose transfer no leg can perform, and the whole
/// reason the transfer partition exists.
///
/// Keyed on a `Custom` column the transfer builds no `t` set for, and
/// claimed by no bespoke leg. Unrefused it would freeze cleanly, boot
/// cleanly, and leave its rows with the SOURCE owner after every memory
/// that referenced them moved — which is not a stale row, it is a
/// cross-tenant read arrived at by silence.
pub(super) static UNMOVABLE_SURFACE: SpiiceContract = transfer_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::Custom(&["thing_id"]),
        owner_column: Some("owner_id"),
        transfer: TransferRule::Follow,
        erase: EraseRule::Never {
            why: "the transfer rule is what this fixture is about",
        },
        export: ExportRule::Excluded {
            why: "the transfer rule is what this fixture is about",
        },
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &[],
);

/// `Follow` with no owner column to set. The rows are reached through
/// their key's owner, which is what `StaysOnKey` says and what a `None`
/// `owner_column` claims; declaring `Follow` over it asks for an
/// `UPDATE` with an empty `SET`.
pub(super) static FOLLOW_WITH_NOTHING_TO_SET: SpiiceContract = transfer_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::MemoryT { column: "t" },
        owner_column: None,
        transfer: TransferRule::Follow,
        erase: EraseRule::Never {
            why: "the transfer rule is what this fixture is about",
        },
        export: ExportRule::Excluded {
            why: "the transfer rule is what this fixture is about",
        },
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &[],
);

/// A surface claiming forget destroys its rows over a key the forget
/// builds no `t` for, and no constraint to claim completeness either.
/// The rows outlive the memory that declared them gone.
pub(super) static UNFORGETTABLE_SURFACE: SpiiceContract = transfer_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::Custom(&["thing_id"]),
        owner_column: None,
        transfer: TransferRule::StaysOnKey,
        erase: EraseRule::Never {
            why: "the forget rule is what this fixture is about",
        },
        export: ExportRule::Excluded {
            why: "the forget rule is what this fixture is about",
        },
        forget: ForgetRule::DeleteWithMemory,
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &[],
);

/// A bespoke transfer leg naming a table the spiice does not declare.
pub(super) static BESPOKE_TRANSFER_LEG_FOR_NOTHING: SpiiceContract = transfer_fixture_at(
    CORE_ORDINAL,
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::Never {
            why: "the transfer rule is what this fixture is about",
        },
        ExportRule::Excluded {
            why: "the transfer rule is what this fixture is about",
        },
    )],
    &["test_spiice.gone_v1"],
);

/// A spiice arguing with itself: `StaysOnKey` says nothing moves, and
/// the exemption list says a hand-written statement moves it.
pub(super) static BESPOKE_TRANSFER_LEG_OVER_A_NON_MOVE: SpiiceContract = transfer_fixture_at(
    CORE_ORDINAL,
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::Never {
            why: "the transfer rule is what this fixture is about",
        },
        ExportRule::Excluded {
            why: "the transfer rule is what this fixture is about",
        },
    )],
    &["test_spiice.thing_v1"],
);

/// A spiice arguing with itself: the declaration says a constraint
/// removes the rows, and the exemption list says a hand-written
/// statement does.
pub(super) static BESPOKE_LEG_OVER_A_CASCADE: SpiiceContract = erase_fixture_at(
    CORE_ORDINAL,
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::Cascade {
            via: DbConstraint {
                relation: "test_spiice.thing_v1",
                name: "thing_v1_t_fkey",
            },
        },
        ExportRule::Rows,
    )],
    &["test_spiice.thing_v1"],
);

/// Exportable while carrying neither an owner column nor a key with a
/// home table: the generator has no statement that reaches it from the
/// owner, so it would go missing from every bundle in silence.
pub(super) static UNREACHABLE_EXPORT_SURFACE: SpiiceContract = erase_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::Custom(&["thing_id"]),
        owner_column: None,
        transfer: TransferRule::StaysOnKey,
        erase: EraseRule::Never {
            why: "the export rule is what this fixture is about",
        },
        export: ExportRule::Rows,
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &[],
);

/// Host-managed exports are reached by the registered lifecycle callback,
/// which is checked against the exact declared table set by storage at
/// boot, and name the column that carries each row's owner.
pub(super) static HOST_MANAGED_EXPORTS: SpiiceContract = erase_fixture(
    &[
        Surface {
            table: "test_spiice.host_rows",
            key: KeyShape::Custom(&["thing_id"]),
            owner_column: Some("owner_id"),
            transfer: TransferRule::RetainAtSource {
                why: "the host state belongs to the publishing owner",
            },
            erase: EraseRule::HostState {
                whole_owner: crate::spiice::contract::HostStateEraseDisposition::Erase,
                source: crate::spiice::contract::HostStateEraseDisposition::Retain,
                exact_fact: crate::spiice::contract::HostStateEraseDisposition::Retain,
            },
            export: ExportRule::Rows,
            forget: ForgetRule::Keep {
                why: "a fixture, not a memory",
            },
            lexical_language_column: None,
            counter: CounterRule::Uncounted {
                why: "a fixture contributes to no receipt",
            },
            completeness: None,
        },
        Surface {
            table: "test_spiice.host_allowlist",
            key: KeyShape::Custom(&["thing_id"]),
            owner_column: Some("owner_id"),
            transfer: TransferRule::RetainAtSource {
                why: "the host state belongs to the publishing owner",
            },
            erase: EraseRule::HostState {
                whole_owner: crate::spiice::contract::HostStateEraseDisposition::Erase,
                source: crate::spiice::contract::HostStateEraseDisposition::Retain,
                exact_fact: crate::spiice::contract::HostStateEraseDisposition::Retain,
            },
            export: ExportRule::Allowlist(&["thing_id"]),
            forget: ForgetRule::Keep {
                why: "a fixture, not a memory",
            },
            lexical_language_column: None,
            counter: CounterRule::Uncounted {
                why: "a fixture contributes to no receipt",
            },
            completeness: None,
        },
    ],
    &[],
);

/// A host-state surface that names no owner column: an erase of one owner
/// could not fence that owner's rows, so freeze refuses it.
pub(super) static HOST_STATE_WITHOUT_OWNER_COLUMN: SpiiceContract = erase_fixture(
    &[Surface {
        table: "test_spiice.host_rows",
        key: KeyShape::Custom(&["thing_id"]),
        owner_column: None,
        transfer: TransferRule::RetainAtSource {
            why: "the callback resolves owner from execution",
        },
        erase: EraseRule::HostState {
            whole_owner: crate::spiice::contract::HostStateEraseDisposition::Erase,
            source: crate::spiice::contract::HostStateEraseDisposition::Retain,
            exact_fact: crate::spiice::contract::HostStateEraseDisposition::Retain,
        },
        export: ExportRule::Rows,
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &[],
);

pub(super) static UNREACHABLE_ALLOWLIST_EXPORT_SURFACE: SpiiceContract = erase_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::Custom(&["thing_id"]),
        owner_column: None,
        transfer: TransferRule::StaysOnKey,
        erase: EraseRule::Never {
            why: "the export rule is what this fixture is about",
        },
        export: ExportRule::Allowlist(&["thing_id"]),
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &[],
);

/// A `PerRow` policy naming a column that is not the projection
/// table's. The generator emits one language column per projection
/// table and names it `lexical_language`; a second name is a
/// declaration nothing renders.
pub(super) static PER_ROW_ON_THE_WRONG_COLUMN: SpiiceContract = SpiiceContract {
    retired_facts: &[],
    spiice_id: FIXTURE_SPIICE,
    ordinal: 7,
    schemas: &[SchemaContract {
        // A Fact, not a citation payload: a citation schema declaring a
        // sidecar trips CitationSidecarNotRemappable first and this
        // fixture would test that instead.
        id: SchemaRef::new(FIXTURE_SPIICE, "thing", 1),
        kind: PayloadKind::Fact,
        sidecar_table: Some("test_spiice.thing_v1"),
        search: SearchProjectionDecl::Projected {
            fields: &[WeightedField {
                column: "a",
                kind: SearchProjectionColumnKind::Text,
                weight: 1.0,
            }],
            tag_column: None,
            language: LanguagePolicy::PerRow {
                column: "row_config",
            },
            bands: &[],
            substring: SubstringArm::Off,
        },
        embedding: EmbeddingRecipe::Never {
            why: "a fixture, not a memory",
        },
        transfer: TransferRule::StaysOnKey,
        provenance: Provenance::None,
        surfaces: &[],
        natural_key_columns: crate::NaturalKeyDeclaration::EMPTY,
    }],
    state_surfaces: &[],
    scopes: &[],
    kernel_surfaces: &[],
    tools: &[],
    resources: &[],
    bespoke_erase_legs: &[],
    bespoke_transfer_legs: &[],
    projection: ProjectionDecl::Table(crate::spiice::contract::ProjectionSpec {
        table: "test_spiice.projection",
        index: "test_spiice_projection_owner_tsv_gin",
        overfetch_k: 0,
        band_comparability: crate::spiice::contract::BandComparability::CoreBands,
        // Sidecar-ranked so this fixture tests ONE rule. Under
        // `Projection` the empty band set would trip
        // `ProjectionBandName` as well, and a fixture that can fail two
        // ways proves neither.
        rank_source: crate::spiice::contract::RankSource::SidecarWithProjectionOwner {
            why: "a fixture, not a search surface",
        },
    }),
    evolutions: &[],
};

/// A non-core spiice naming a hand-written erase leg. Only storage-pg code
/// runs bespoke legs, and storage-pg depends on no spiice but core, so the
/// name is a promise nothing keeps: the erase reports `Completed` over rows
/// no statement touched (#478 item 3).
pub(super) static NON_CORE_BESPOKE_ERASE_LEG: SpiiceContract = erase_fixture(
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::Custom(&["thing_id"]),
        EraseRule::ByOwner,
        ExportRule::Rows,
    )],
    &["test_spiice.thing_v1"],
);

/// The transfer twin: a `Follow` surface on a `Custom` key that only a
/// hand-written transfer leg could move.
pub(super) static NON_CORE_BESPOKE_TRANSFER_LEG: SpiiceContract = transfer_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::Custom(&["thing_id"]),
        owner_column: Some("owner_id"),
        transfer: TransferRule::Follow,
        erase: EraseRule::Never {
            why: "the transfer rule is what this fixture is about",
        },
        export: ExportRule::Excluded {
            why: "the transfer rule is what this fixture is about",
        },
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &["test_spiice.thing_v1"],
);

/// A non-core `FollowOrDedupe`: the dedupe arm always needs a hand-written
/// orchestrator, so it is a bespoke transfer leg under another name.
pub(super) static NON_CORE_DEDUPE: SpiiceContract = transfer_fixture(
    &[Surface {
        table: "test_spiice.thing_v1",
        key: KeyShape::MemoryT { column: "t" },
        owner_column: Some("owner_id"),
        transfer: TransferRule::FollowOrDedupe {
            dedupe_key: &["content_hash"],
            remaps: &[],
        },
        erase: EraseRule::Never {
            why: "the transfer rule is what this fixture is about",
        },
        export: ExportRule::Excluded {
            why: "the transfer rule is what this fixture is about",
        },
        forget: ForgetRule::Keep {
            why: "a fixture, not a memory",
        },
        lexical_language_column: None,
        counter: CounterRule::Uncounted {
            why: "a fixture contributes to no receipt",
        },
        completeness: None,
    }],
    &["test_spiice.thing_v1"],
);

/// An erased surface (owner-filtered delete) declared out of the bundle:
/// the erase destroys rows the owner's export never handed over (#478).
pub(super) static ERASED_BUT_EXCLUDED: SpiiceContract = erase_fixture(
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::ByOwner,
        ExportRule::Excluded {
            why: "the export rule is what this fixture is about",
        },
    )],
    &[],
);

/// Rebuilt from a surface no spiice declares.
pub(super) static DERIVED_FROM_NOWHERE: SpiiceContract = erase_fixture(
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::ByOwner,
        ExportRule::DerivedFrom {
            source: "test_spiice.gone_v1",
            why: "the export rule is what this fixture is about",
        },
    )],
    &[],
);

/// Rebuilt from a surface the bundle does not carry either.
pub(super) static DERIVED_FROM_THE_EXCLUDED: SpiiceContract = erase_fixture(
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::ByOwner,
        ExportRule::DerivedFrom {
            source: "proxiima_core.owners",
            why: "the export rule is what this fixture is about",
        },
    )],
    &[],
);

/// Two surfaces each rebuilt from the other: nothing is carried.
pub(super) static DERIVED_FROM_A_LOOP: SpiiceContract = erase_fixture(
    &[
        state_surface(
            "test_spiice.thing_v1",
            KeyShape::MemoryT { column: "t" },
            EraseRule::ByOwner,
            ExportRule::DerivedFrom {
                source: "test_spiice.other_v1",
                why: "the export rule is what this fixture is about",
            },
        ),
        state_surface(
            "test_spiice.other_v1",
            KeyShape::MemoryT { column: "t" },
            EraseRule::ByOwner,
            ExportRule::DerivedFrom {
                source: "test_spiice.thing_v1",
                why: "the export rule is what this fixture is about",
            },
        ),
    ],
    &[],
);

/// Rebuilt from the memory spine, which the bundle carries: freezes.
pub(super) static DERIVED_FROM_MEMORY: SpiiceContract = erase_fixture(
    &[state_surface(
        "test_spiice.thing_v1",
        KeyShape::MemoryT { column: "t" },
        EraseRule::ByOwner,
        ExportRule::DerivedFrom {
            source: "proxiima_core.memory",
            why: "the export rule is what this fixture is about",
        },
    )],
    &[],
);

#[test]
fn erase_legs_match_ledger_vectors() {
    use crate::spiice::contract::{EraseLeg, HostStateEraseDisposition};
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum Declaration {
        Keyed([bool; 2]),
        Owned([bool; 2]),
        Cascade,
        HostState,
        Retain,
    }

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum Leg {
        Keyed,
        OwnerOnly,
        OwnerAndSource,
        Bespoke,
        Cascade,
        HostState,
        Retained,
        Unreachable,
    }

    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "in")]
        input: [Declaration; 1],
        out: Leg,
    }

    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/ledger_vectors.json")).unwrap();
    let rows: Vec<Row> =
        serde_json::from_value(vectors["Proxiima.ErasureExport.classifyErase"].clone()).unwrap();
    assert_eq!(rows.len(), 11);
    let table = "fixture.surface";
    let single_home = KeyShape::MemoryT { column: "t" };
    let entity = KeyShape::EntityT { column: "t" };
    for Row {
        input: [declaration],
        out,
    } in rows
    {
        let (key, erase, bespoke, source_attributed) = match declaration {
            Declaration::Keyed([home, bespoke]) => (
                if home { single_home } else { entity },
                EraseRule::ByKey,
                bespoke,
                false,
            ),
            Declaration::Owned([attributed, bespoke]) => {
                (single_home, EraseRule::ByOwner, bespoke, attributed)
            }
            Declaration::Cascade => (
                single_home,
                EraseRule::Cascade {
                    via: DbConstraint {
                        relation: table,
                        name: "fixture_fk",
                    },
                },
                false,
                false,
            ),
            Declaration::HostState => (
                single_home,
                EraseRule::HostState {
                    whole_owner: HostStateEraseDisposition::Erase,
                    source: HostStateEraseDisposition::Retain,
                    exact_fact: HostStateEraseDisposition::Retain,
                },
                false,
                false,
            ),
            Declaration::Retain => (entity, EraseRule::Never { why: "fixture" }, false, false),
        };
        let mut surface = state_surface(table, key, erase, ExportRule::Rows);
        if source_attributed {
            surface.transfer = TransferRule::RetainAtSource { why: "fixture" };
        }
        let bespoke_tables: &[&str] = if bespoke { &[table] } else { &[] };
        let actual = match EraseLeg::derive(&surface, bespoke_tables) {
            EraseLeg::Keyed(_) => Leg::Keyed,
            EraseLeg::Owned {
                source_scoped: None,
            } => Leg::OwnerOnly,
            EraseLeg::Owned {
                source_scoped: Some(_),
            } => Leg::OwnerAndSource,
            EraseLeg::Bespoke => Leg::Bespoke,
            EraseLeg::Cascade => Leg::Cascade,
            EraseLeg::HostState => Leg::HostState,
            EraseLeg::Never { .. } => Leg::Retained,
            EraseLeg::Unreachable => Leg::Unreachable,
        };
        assert_eq!(actual, out);
    }
}

#[test]
fn transfer_legs_match_ledger_vectors() {
    use crate::spiice::contract::TransferLeg;

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum Declaration {
        Follow([bool; 2]),
        Drop([bool; 2]),
        Dedupe([bool; 1]),
        StayOnKey,
        RetainAtSource,
        RefuseTransfer,
    }

    #[derive(serde::Deserialize)]
    struct Row {
        #[serde(rename = "in")]
        input: [Declaration; 1],
        out: String,
    }

    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../../tests/ledger_vectors.json")).unwrap();
    let rows: Vec<Row> =
        serde_json::from_value(vectors["Proxiima.ErasureExport.classifyTransfer"].clone()).unwrap();
    assert_eq!(rows.len(), 13);

    let table = "fixture.surface";
    let version = KeyShape::EntityT { column: "t" };
    for Row {
        input: [declaration],
        out,
    } in rows
    {
        let (rule, key, bespoke) = match declaration {
            Declaration::Follow([supported, bespoke]) => (
                TransferRule::Follow,
                if supported {
                    version
                } else {
                    KeyShape::OwnerId
                },
                bespoke,
            ),
            Declaration::Drop([supported, bespoke]) => (
                TransferRule::Drop { why: "fixture" },
                if supported {
                    version
                } else {
                    KeyShape::OwnerId
                },
                bespoke,
            ),
            Declaration::Dedupe([bespoke]) => (
                TransferRule::FollowOrDedupe {
                    dedupe_key: &["digest"],
                    remaps: &[],
                },
                KeyShape::OwnerId,
                bespoke,
            ),
            Declaration::StayOnKey => (TransferRule::StaysOnKey, version, true),
            Declaration::RetainAtSource => (
                TransferRule::RetainAtSource { why: "fixture" },
                version,
                true,
            ),
            Declaration::RefuseTransfer => (
                TransferRule::NotTransferable {
                    why: "fixture",
                    enforced_by: &[],
                },
                version,
                true,
            ),
        };
        let mut surface = state_surface(table, key, EraseRule::ByOwner, ExportRule::Rows);
        surface.transfer = rule;
        let bespoke_tables: &[&str] = if bespoke { &[table] } else { &[] };
        let actual = match TransferLeg::derive(&surface, bespoke_tables) {
            TransferLeg::Rehomed { .. } => "follow",
            TransferLeg::Dropped { .. } => "drop",
            TransferLeg::Deduped { .. } => "dedupe",
            TransferLeg::Bespoke => "bespoke",
            TransferLeg::StaysOnKey => "stayOnKey",
            TransferLeg::Retained { .. } => "retained",
            TransferLeg::Refused { .. } => "refused",
            TransferLeg::Unreachable => "unreachable",
        };
        assert_eq!(actual, out);
    }
}
