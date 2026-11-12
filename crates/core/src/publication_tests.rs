use super::*;
use crate::{MemoryId, SchemaId, SchemaVersion, UserId};

fn draft() -> PublicationDraft {
    PublicationDraft::new(
        SchemaId::new("test/listenable-v1".into()),
        SchemaVersion::new(1),
        PublicationSource::new("urn:proxiima:test").expect("source"),
        OwnerId::from(UserId::new(Uuid::nil())),
        Some("model-x".into()),
        PublicationExtensions::new(),
        serde_json::json!({ "body": "hello" }),
    )
}

#[test]
fn a_source_must_be_an_absolute_uri_or_urn() {
    assert!(PublicationSource::new("https://proxiima.example/prod").is_ok());
    assert!(PublicationSource::new("urn:proxiima:prod").is_ok());
    assert!(matches!(
        PublicationSource::new(""),
        Err(PublicationSourceError::Empty)
    ));
    assert!(matches!(
        PublicationSource::new("proxiima prod"),
        Err(PublicationSourceError::IllegalCharacter { .. })
    ));
    assert!(matches!(
        PublicationSource::new("/relative/reference"),
        Err(PublicationSourceError::NotAbsolute { .. })
    ));
    assert!(matches!(
        PublicationSource::new("scheme:"),
        Err(PublicationSourceError::NotAbsolute { .. })
    ));
    assert_eq!(
        "urn:x:y".parse::<PublicationSource>().expect("parse"),
        PublicationSource::new("urn:x:y").expect("new")
    );
}

fn position() -> SubjectPosition {
    SubjectPosition::new(
        Uuid::parse_str("01930000-0000-8000-8000-0000000000aa").expect("v8 id"),
        NonZeroU64::new(7).expect("not zero"),
    )
}

#[test]
fn a_position_reads_back_only_in_the_producers_spelling() {
    let written = position();
    assert_eq!(
        SubjectPosition::parse(&written.subject_attribute(), &written.sequence_attribute()),
        Ok(written)
    );
    let max = SubjectPosition::new(written.subject, NonZeroU64::MAX);
    assert_eq!(max.sequence_attribute(), u64::MAX.to_string());
    // The zero padding makes string order numeric order.
    let nine = SubjectPosition::new(written.subject, NonZeroU64::new(9).expect("9"));
    let ten = SubjectPosition::new(written.subject, NonZeroU64::new(10).expect("10"));
    assert!(nine.sequence_attribute() < ten.sequence_attribute());

    let subject = written.subject_attribute();
    for bad in [
        subject.to_uppercase(),
        written.subject.simple().to_string(),
        format!("urn:uuid:{subject}"),
    ] {
        assert_eq!(
            SubjectPosition::parse(&bad, &written.sequence_attribute()),
            Err(SubjectPositionError::Subject),
            "{bad:?}"
        );
    }
    for bad in [
        "7",
        "00000000000000000000",
        "0000000000000000000x",
        "+0000000000000000007",
    ] {
        assert_eq!(
            SubjectPosition::parse(&subject, bad),
            Err(SubjectPositionError::Sequence),
            "{bad:?}"
        );
    }
}

/// Bound in a deliberately non-alphabetical order, so the ordering
/// assertion below is about the type and not about the caller.
fn extensions() -> PublicationExtensions {
    PublicationExtensions::new()
        .with("stepid", "step-7")
        .expect("a step id is a legal attribute")
        .with("runid", "run-3")
        .expect("a run id is a legal attribute")
        .with("attempt", 2)
        .expect("an integer is a legal value")
        .with("replay", false)
        .expect("a boolean is a legal value")
}

#[test]
fn the_envelope_key_order_is_fixed_and_the_time_is_the_write_time_by_default() {
    let t = Uuid::parse_str("01930000-0000-8000-8000-000000000001").expect("v8 id");
    let mut draft = draft();
    draft.extensions = extensions();
    let sealed = SealedPublication::seal(
        &PublicationPlan::new(draft, PublicationLimits::default()),
        t,
        MemoryId::new(t).created_at(),
        position(),
    )
    .expect("seal");
    let text = String::from_utf8(sealed.bytes.clone()).expect("utf8");
    // The ORDER assertion reads the serialized text: a parsed `Map` is a
    // container whose iteration order is a build-feature detail, and the
    // bytes are what the digest and the consumer see.
    let mut cursor = 0usize;
    for key in [
        "\"specversion\"",
        "\"id\"",
        "\"source\"",
        "\"type\"",
        "\"datacontenttype\"",
        "\"dataschema\"",
        "\"time\"",
        "\"subject\":\"01930000-0000-8000-8000-0000000000aa\"",
        "\"sequence\":\"00000000000000000007\"",
        "\"proxiimaowner\"",
        "\"proxiimamodel\"",
        // Host-bound extensions: after the substrate's own attributes,
        // before `data`, in NAME order rather than binding order.
        "\"attempt\":2",
        "\"replay\":false",
        "\"runid\":\"run-3\"",
        "\"stepid\":\"step-7\"",
        "\"data\"",
    ] {
        let at = text[cursor..]
            .find(key)
            .unwrap_or_else(|| panic!("{key} missing or out of order in {text}"));
        cursor += at + key.len();
    }
    assert_eq!(sealed.event_id, t.to_string());

    let expected = time::OffsetDateTime::from_unix_timestamp_nanos(1_730_871_820_288_000_000)
        .expect("moment")
        .format(&time::format_description::well_known::Rfc3339)
        .expect("rfc3339");
    assert!(
        text.contains(&format!("\"time\":\"{expected}\"")),
        "{text} should carry the v8 time {expected}"
    );
    assert!(expected.ends_with('Z'), "{expected} must be UTC-normalized");
    assert_eq!(sealed.digest, *blake3::hash(&sealed.bytes).as_bytes());
}

#[test]
fn the_envelope_time_is_when_the_fact_occurred() {
    let t = crate::new_memory_uuid(3);
    let occurred_at = time::OffsetDateTime::parse(
        "2023-03-22T10:15:00.123456+02:00",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("rfc3339");
    let sealed = SealedPublication::seal(
        &PublicationPlan::new(draft(), PublicationLimits::default()),
        t,
        occurred_at,
        position(),
    )
    .expect("seal");
    let text = String::from_utf8(sealed.bytes).expect("utf8");
    assert!(
        text.contains("\"time\":\"2023-03-22T08:15:00.123456Z\""),
        "{text} should carry the occurrence time in UTC"
    );
}

#[test]
fn an_oversized_export_is_refused_rather_than_truncated() {
    let limits = PublicationLimits {
        max_payload_bytes: 16,
        ..PublicationLimits::default()
    };
    let t = crate::new_memory_uuid(3);
    let err = SealedPublication::seal(
        &PublicationPlan::new(draft(), limits),
        t,
        MemoryId::new(t).created_at(),
        position(),
    )
    .expect_err("too big");
    assert!(matches!(
        err,
        PublicationError::PayloadTooLarge { max: 16, .. }
    ));
}

#[test]
fn an_unbound_source_names_every_listenable_schema() {
    let config = PublicationConfig::default();
    assert!(config.validate_against(Vec::<&str>::new()).is_ok());
    let err = config
        .validate_against(vec!["b/two-v1", "a/one-v1", "b/two-v1"])
        .expect_err("unbound");
    assert_eq!(
        err,
        PublicationError::SourceUnbound {
            schema_id: "a/one-v1, b/two-v1".into()
        }
    );
    assert!(
        PublicationConfig::new(PublicationSource::new("urn:x:y").expect("source"))
            .validate_against(vec!["a/one-v1"])
            .is_ok()
    );
}

#[test]
fn sealing_the_same_bindings_twice_produces_the_same_bytes() {
    let t = Uuid::parse_str("01930000-0000-8000-8000-000000000002").expect("v8 id");
    let seal_once = |ext: PublicationExtensions| {
        let mut draft = draft();
        draft.extensions = ext;
        SealedPublication::seal(
            &PublicationPlan::new(draft, PublicationLimits::default()),
            t,
            MemoryId::new(t).created_at(),
            position(),
        )
        .expect("seal")
    };
    // Same attributes, opposite binding order.
    let forwards = seal_once(
        PublicationExtensions::new()
            .with("runid", "r")
            .expect("bind")
            .with("stepid", "s")
            .expect("bind"),
    );
    let backwards = seal_once(
        PublicationExtensions::new()
            .with("stepid", "s")
            .expect("bind")
            .with("runid", "r")
            .expect("bind"),
    );
    assert_eq!(forwards.bytes, backwards.bytes);
    assert_eq!(forwards.digest, backwards.digest);
}

#[test]
fn an_extension_name_follows_the_cloudevents_naming_rule() {
    let empty = PublicationExtensions::new();
    assert!(matches!(
        empty.clone().with("", "v"),
        Err(PublicationExtensionsError::EmptyName)
    ));
    assert!(matches!(
        empty.clone().with("run_id", "v"),
        Err(PublicationExtensionsError::IllegalNameCharacter { .. })
    ));
    assert!(matches!(
        empty.clone().with("RunId", "v"),
        Err(PublicationExtensionsError::IllegalNameCharacter { .. })
    ));
    assert!(matches!(
        empty.clone().with("a".repeat(21), "v"),
        Err(PublicationExtensionsError::NameTooLong { max: 20, .. })
    ));
    assert!(empty.clone().with("a".repeat(20), "v").is_ok());
    assert!(matches!(
        empty.clone().with("time", "v"),
        Err(PublicationExtensionsError::CoreAttributeName { .. })
    ));
    assert!(matches!(
        empty.clone().with("data", "v"),
        Err(PublicationExtensionsError::CoreAttributeName { .. })
    ));
    assert!(matches!(
        empty.clone().with("sequence", "v"),
        Err(PublicationExtensionsError::SubstrateAttributeName { .. })
    ));
    assert!(matches!(
        empty.with("proxiimaowner", "v"),
        Err(PublicationExtensionsError::ReservedName {
            prefix: "proxiima",
            ..
        })
    ));
}

#[test]
fn a_string_value_is_bounded_and_printable() {
    let empty = PublicationExtensions::new();
    assert!(matches!(
        empty.clone().with("runid", ""),
        Err(PublicationExtensionsError::EmptyValue { .. })
    ));
    assert!(matches!(
        empty.clone().with("runid", "a\nb"),
        Err(PublicationExtensionsError::IllegalValueCharacter { .. })
    ));
    assert!(matches!(
        empty.clone().with("runid", "x".repeat(257)),
        Err(PublicationExtensionsError::ValueTooLong { max: 256, .. })
    ));
    assert!(empty.with("runid", "x".repeat(256)).is_ok());
}

#[test]
fn the_set_is_bounded_and_a_bound_name_is_never_replaced() {
    let mut set = PublicationExtensions::new();
    for index in 0..8 {
        set = set.with(format!("a{index}"), index).expect("within bounds");
    }
    assert_eq!(set.len(), 8);
    assert!(matches!(
        set.clone().with("a9", 9),
        Err(PublicationExtensionsError::TooMany { max: 8, .. })
    ));
    assert!(matches!(
        set.clone().with("a0", "other"),
        Err(PublicationExtensionsError::DuplicateName { .. })
    ));
    assert_eq!(set.get("a0"), Some(&ExtensionValue::Integer(0)));
    assert!(PublicationExtensions::new().is_empty());
}

#[test]
fn merging_is_additive_and_refuses_a_name_both_sides_bind() {
    let host = PublicationExtensions::new()
        .with("runid", "r")
        .expect("bind");
    let more = PublicationExtensions::new()
        .with("stepid", "s")
        .expect("bind");
    let merged = host.clone().merged(more).expect("disjoint sets merge");
    assert_eq!(
        merged.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        vec!["runid", "stepid"]
    );
    assert!(matches!(
        host.merged(
            PublicationExtensions::new()
                .with("runid", "other")
                .expect("bind")
        ),
        Err(PublicationExtensionsError::DuplicateName { .. })
    ));
}
