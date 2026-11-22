use crate::{
    DurablePayloadBuilder, KeyField, MemoryId, MemoryKind, PayloadDisplay, PayloadProjection,
    PayloadReference, PerspectivePayload,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(crate) const CONFIDENCE_MAX: u8 = crate::ledger!(Proxiima.Memory.confidenceMax);
pub(crate) const CONFIDENCE_DEFAULT: u8 = crate::ledger!(Proxiima.Memory.confidenceDefault);

/// An interpretation score on the inclusive 0..=100 scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i16")]
pub struct InterpretationConfidence(u8);

/// The supplied interpretation score lies outside the scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("confidence must be 0..={CONFIDENCE_MAX}")]
pub struct InvalidInterpretationConfidence;

impl TryFrom<i16> for InterpretationConfidence {
    type Error = InvalidInterpretationConfidence;

    fn try_from(value: i16) -> Result<Self, Self::Error> {
        let value = u8::try_from(value).map_err(|_| InvalidInterpretationConfidence)?;
        if value <= CONFIDENCE_MAX {
            Ok(Self(value))
        } else {
            Err(InvalidInterpretationConfidence)
        }
    }
}

impl InterpretationConfidence {
    /// The numeric confidence stored by the sidecar.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// The two persisted subject columns have different lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("interpretation subject arrays have different lengths ({ids} ids, {kinds} kinds)")]
pub struct MisalignedInterpretationSubjects {
    ids: usize,
    kinds: usize,
}

/// An agent's claim about existing nodes.
///
/// A claim with a reason and a confidence is a Perspective. Its subject
/// references can be rebuilt from the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretationV1 {
    /// The claim being made about the subjects.
    pub claim: String,
    /// How strongly the author holds the claim, 0..=100.
    pub confidence: InterpretationConfidence,
    /// Each subject's exact Memory version and layer, used to rebuild its
    /// `Reference` index entry from this Perspective.
    pub subjects: Vec<(MemoryId, MemoryKind)>,
    pub model_id: String,
    pub client_name: String,
    pub client_version: String,
}

/// An agent's claim about existing nodes.
///
/// A claim with a reason and a confidence is a judgment — a Perspective
/// (docs/concepts.md). Subjects are schema-declared reference fields;
/// index rows are re-derivable from this payload.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename = "InterpretationV1")]
struct InterpretationWire<Text> {
    /// The claim being made about the subjects.
    claim: Text,
    /// How strongly the author holds the claim, 0..=100.
    #[schemars(with = "u8")]
    confidence: InterpretationConfidence,
    /// Memories the claim is about. Each becomes a `Reference` index
    /// entry sourced at this Perspective.
    subject_memory_ids: Vec<uuid::Uuid>,
    /// Entity kind of each subject, positionally aligned with
    /// `subject_memory_ids`. Carried because a reference declaration
    /// needs its target's kind and this payload is the only place the
    /// resolved kinds survive re-derivation.
    subject_kinds: Vec<MemoryKind>,
    model_id: Text,
    client_name: Text,
    client_version: Text,
}

impl InterpretationV1 {
    /// Builds a stable interpretation payload for core and integration fixtures.
    ///
    /// # Panics
    ///
    /// Panics when the supplied confidence exceeds the interpretation scale.
    #[cfg(any(test, feature = "testkit"))]
    #[must_use]
    pub fn fixture(
        claim: impl Into<String>,
        confidence: u8,
        subjects: impl IntoIterator<Item = (MemoryId, MemoryKind)>,
    ) -> Self {
        Self {
            claim: claim.into(),
            confidence: i16::from(confidence)
                .try_into()
                .expect("fixture confidence is within the interpretation scale"),
            subjects: subjects.into_iter().collect(),
            model_id: "fixture".into(),
            client_name: "fixture".into(),
            client_version: "1".into(),
        }
    }

    /// Decode the persisted subject columns into exact version and kind pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if a subject ID or kind has no corresponding entry.
    pub fn subjects_from_columns(
        ids: Vec<uuid::Uuid>,
        kinds: Vec<MemoryKind>,
    ) -> Result<Vec<(MemoryId, MemoryKind)>, MisalignedInterpretationSubjects> {
        // Check both lengths before zipping so no subject is silently dropped.
        if ids.len() != kinds.len() {
            return Err(MisalignedInterpretationSubjects {
                ids: ids.len(),
                kinds: kinds.len(),
            });
        }
        Ok(ids.into_iter().map(MemoryId::new).zip(kinds).collect())
    }

    fn wire(&self) -> InterpretationWire<&str> {
        let (subject_memory_ids, subject_kinds) = self
            .subjects
            .iter()
            .map(|&(id, kind)| (id.into_inner(), kind))
            .unzip();
        InterpretationWire {
            claim: self.claim.as_str(),
            confidence: self.confidence,
            subject_memory_ids,
            subject_kinds,
            model_id: self.model_id.as_str(),
            client_name: self.client_name.as_str(),
            client_version: self.client_version.as_str(),
        }
    }
}

impl Serialize for InterpretationV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.wire().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InterpretationV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = InterpretationWire::<String>::deserialize(deserializer)?;
        Ok(Self {
            claim: wire.claim,
            confidence: wire.confidence,
            subjects: Self::subjects_from_columns(wire.subject_memory_ids, wire.subject_kinds)
                .map_err(serde::de::Error::custom)?,
            model_id: wire.model_id,
            client_name: wire.client_name,
            client_version: wire.client_version,
        })
    }
}

impl JsonSchema for InterpretationV1 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        InterpretationWire::<String>::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        InterpretationWire::<String>::json_schema(generator)
    }
}

impl PerspectivePayload for InterpretationV1 {
    const SCHEMA_ID: &'static str = "core/interpretation-v1";
    const SCHEMA_VERSION: u32 = 1;

    fn sidecar_table() -> &'static str {
        "proxiima_core.interpretation_v1"
    }

    /// Project each subject as a schema-declared reference.
    fn references(&self) -> Vec<PayloadReference> {
        self.subjects
            .iter()
            .map(|&(id, kind)| PayloadReference::memory("subject_memory_ids", kind.into(), id))
            .collect()
    }

    fn json_schema() -> Option<serde_json::Value> {
        Some(
            serde_json::to_value(schemars::schema_for!(Self))
                .expect("InterpretationV1 schema serializes"),
        )
    }
}

impl PayloadProjection for InterpretationV1 {
    const KEY_FIELDS: &'static [KeyField<Self>] = &[];
    fn display(&self) -> PayloadDisplay {
        PayloadDisplay {
            body: Some(self.claim.clone()),
            sketch: Some(self.claim.clone()),
            ..PayloadDisplay::default()
        }
    }
    fn durable(&self) -> Result<crate::DurablePayload, String> {
        let wire = self.wire();
        let mut fields = DurablePayloadBuilder::new();
        fields
            .field("claim", &wire.claim)?
            .field("confidence", &wire.confidence)?
            .field("subject_memory_ids", &wire.subject_memory_ids)?
            .field("subject_kinds", &wire.subject_kinds)?
            .field("model_id", &wire.model_id)?
            .field("client_name", &wire.client_name)?
            .field("client_version", &wire.client_version)?;
        Ok(fields.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntityKind, ReferenceBinding};

    fn fixture() -> InterpretationV1 {
        InterpretationV1::fixture(
            "the outage followed the deploy",
            80,
            MemoryKind::ALL
                .into_iter()
                .map(|kind| (MemoryId::new(uuid::Uuid::now_v7()), kind)),
        )
    }

    #[test]
    fn durable_matches_current_wire_encoding() {
        let value = fixture();
        let wire = serde_json::to_value(&value).expect("wire");
        assert_eq!(
            value.durable().expect("durable").canonical_bytes(),
            crate::canonical_json_bytes(&wire)
        );
        assert_eq!(wire["confidence"], 80);
        assert_eq!(
            wire["subject_memory_ids"],
            serde_json::json!(value.subjects.iter().map(|(id, _)| id).collect::<Vec<_>>())
        );
        assert_eq!(wire["subject_kinds"], serde_json::json!(MemoryKind::ALL));
        assert_eq!(
            serde_json::from_value::<InterpretationV1>(wire).unwrap(),
            value
        );
    }

    #[test]
    fn invalid_confidence_and_misaligned_columns_refuse_decode() {
        let value = fixture();
        let wire = serde_json::to_value(&value).expect("wire");
        for score in [-1, 101] {
            assert!(InterpretationConfidence::try_from(score).is_err());
            let mut invalid = wire.clone();
            invalid["confidence"] = score.into();
            assert!(serde_json::from_value::<InterpretationV1>(invalid).is_err());
        }
        for field in ["subject_memory_ids", "subject_kinds"] {
            let mut invalid = wire.clone();
            invalid[field] = serde_json::json!([]);
            let error = serde_json::from_value::<InterpretationV1>(invalid).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("subject arrays have different lengths")
            );
        }
    }

    #[test]
    fn subjects_become_pinned_reference_declarations() {
        let payload = fixture();
        let references = payload.references();
        assert_eq!(references.len(), payload.subjects.len());
        for (reference, &(id, kind)) in references.iter().zip(&payload.subjects) {
            assert_eq!(reference.binding, ReferenceBinding::Pin);
            assert_eq!(reference.target.kind, EntityKind::from(kind));
            assert_eq!(reference.target.memory_id(), Some(id));
        }
    }
}
