use std::collections::HashSet;

use proxiima_core::catalog::core_tools::membership::AddMemberArgs;
use proxiima_core::{
    AccessCeiling, AuthPath, AuthzContext, GoalAccess, GroupId, MemoryKind, OwnerId, Relation,
    Role, UserId,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Deserialize)]
struct Vector<Input, Output> {
    #[serde(rename = "in")]
    input: Input,
    out: Output,
}

fn vectors<Input: DeserializeOwned, Output: DeserializeOwned>(
    declaration: &str,
) -> Vec<Vector<Input, Output>> {
    let mut ledger: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("ledger_vectors.json"))
            .expect("generated ledger vectors");
    let vectors = ledger
        .remove(declaration)
        .expect("decision missing from ledger");
    let rows: Vec<_> = serde_json::from_value(vectors).expect("decision vector shape");
    assert!(!rows.is_empty(), "{declaration}");
    rows
}

fn ceiling(label: &str) -> AccessCeiling {
    match label {
        "none" => AccessCeiling::None,
        "fact" => AccessCeiling::Fact,
        "abstraction" => AccessCeiling::Abstraction,
        "perspective" => AccessCeiling::Perspective,
        _ => panic!("unknown ledger ceiling {label}"),
    }
}

fn goal_access(label: &str) -> GoalAccess {
    match label {
        "none" => GoalAccess::None,
        "reads" => GoalAccess::Reads,
        "works" => GoalAccess::Works,
        _ => panic!("unknown ledger Goal access {label}"),
    }
}

#[test]
fn memory_ceilings_match_ledger_vectors() {
    for row in vectors::<(String,), u8>("Proxiima.OwnerAuthority.AccessCeiling.rank") {
        assert_eq!(ceiling(&row.input.0).rank(), row.out);
    }
    for row in vectors::<(MemoryKind,), String>("Proxiima.OwnerAuthority.coveringCeiling") {
        assert_eq!(AccessCeiling::covering(row.input.0), ceiling(&row.out));
    }
    for row in vectors::<(String, MemoryKind), bool>("Proxiima.OwnerAuthority.covers") {
        assert_eq!(ceiling(&row.input.0).allows(row.input.1), row.out);
    }
    for row in vectors::<(String, String), bool>("Proxiima.OwnerAuthority.ceilingAdmitted") {
        assert_eq!(
            Role::new(ceiling(&row.input.0), ceiling(&row.input.1)).is_ok(),
            row.out
        );
    }
}

#[test]
fn goal_access_matches_ledger_vectors() {
    for row in vectors::<(String,), u8>("Proxiima.OwnerAuthority.GoalAccess.rank") {
        assert_eq!(goal_access(&row.input.0).rank(), row.out);
    }
    let with_goals = |goals| {
        Role::new(AccessCeiling::None, AccessCeiling::None)
            .unwrap()
            .with_goals(goals)
    };
    for row in vectors::<(String,), bool>("Proxiima.OwnerAuthority.GoalAccess.canRead") {
        assert_eq!(with_goals(goal_access(&row.input.0)).reads_goals(), row.out);
    }
    for row in vectors::<(String,), bool>("Proxiima.OwnerAuthority.GoalAccess.canWork") {
        assert_eq!(with_goals(goal_access(&row.input.0)).works_goals(), row.out);
    }
}

#[test]
fn membership_presets_match_ledger_vectors() {
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq)]
    #[serde(rename_all = "camelCase")]
    enum Preset {
        Personal,
        Member([Relation; 1]),
    }

    #[derive(Deserialize)]
    struct LedgerRole {
        read: String,
        write: String,
        goals: String,
        administers: bool,
        manages: bool,
    }

    let mut presets = HashSet::new();
    for row in vectors::<(Preset,), LedgerRole>("Proxiima.OwnerAuthority.rolePresets") {
        let (preset,) = row.input;
        presets.insert(preset);
        let role = match preset {
            Preset::Personal => Role::personal(),
            Preset::Member([relation]) => relation.role(),
        };
        assert_eq!(role.read_ceiling(), ceiling(&row.out.read));
        assert_eq!(role.write_ceiling(), ceiling(&row.out.write));
        assert_eq!(role.goals(), goal_access(&row.out.goals));
        assert_eq!(role.administers(), row.out.administers);
        assert_eq!(role.manages(), row.out.manages);
    }
    let rust_presets: HashSet<_> = std::iter::once(Preset::Personal)
        .chain(Relation::ALL.map(|relation| Preset::Member([relation])))
        .collect();
    assert_eq!(presets, rust_presets);
}

#[test]
fn relation_labels_match_ledger_vectors() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum MemberAddRelation {
        Omitted,
        Supplied([Relation; 1]),
    }

    let mut relations = Vec::new();
    for row in vectors::<(Relation,), String>("Proxiima.OwnerAuthority.relationLabel") {
        relations.push(row.input.0);
        assert_eq!(row.input.0.as_str(), row.out);
        assert_eq!(
            row.out.to_ascii_uppercase().parse::<Relation>().unwrap(),
            row.input.0
        );
    }
    assert_eq!(relations, Relation::ALL);

    let mut replayed_default = false;
    for row in
        vectors::<(MemberAddRelation,), Relation>("Proxiima.OwnerAuthority.memberAddRelation")
    {
        let mut args = serde_json::json!({"group": "current", "member": uuid::Uuid::nil()});
        match row.input.0 {
            MemberAddRelation::Omitted => replayed_default = true,
            MemberAddRelation::Supplied([relation]) => {
                args["relation"] = serde_json::json!(relation);
            }
        }
        let args: AddMemberArgs = serde_json::from_value(args).expect("member-add arguments");
        assert_eq!(args.relation, row.out);
    }
    assert!(
        replayed_default,
        "member-add vectors cover the omitted relation"
    );
}

#[test]
fn owner_goal_work_matches_ledger_vectors() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    enum GoalWorkPath {
        HostBearer,
        Delegated,
        System,
    }
    let subject = UserId::new(uuid::Uuid::now_v7());
    let owner = OwnerId::from(GroupId::new(uuid::Uuid::now_v7()));
    for row in
        vectors::<(GoalWorkPath, bool, bool), bool>("Proxiima.OwnerAuthority.goalWorkAdmitted")
    {
        let (path, works, administers) = row.input;
        let path = match path {
            GoalWorkPath::HostBearer => AuthPath::HostBearer,
            GoalWorkPath::Delegated => AuthPath::Delegated,
            GoalWorkPath::System => AuthPath::System,
        };
        let mut role = Role::new(AccessCeiling::None, AccessCeiling::None).unwrap();
        if works {
            role = role.with_goals(GoalAccess::Works);
        }
        if administers {
            role = role.administering();
        }
        let authz = AuthzContext::for_subject_with_role(subject, [(owner, role)], path);
        assert_eq!(authz.may_work_goals(&owner), row.out);
    }
}
