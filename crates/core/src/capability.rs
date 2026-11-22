//! Domainless payload capability tags.

/// Opaque schema-declared capability tag.
///
/// Core stores and compares tags; it never interprets their meaning.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityTag(String);

impl CapabilityTag {
    /// Parse a capability tag.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityTagError::Invalid`] unless `raw` matches
    /// `^[a-z][a-z0-9-]*$`.
    pub fn parse(raw: impl Into<String>) -> Result<Self, CapabilityTagError> {
        let value = raw.into();
        if is_valid_capability_tag(&value) {
            Ok(Self(value))
        } else {
            Err(CapabilityTagError::Invalid { value })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityTagError {
    #[error("invalid capability tag {value:?}: expected ^[a-z][a-z0-9-]*$")]
    Invalid { value: String },
}

// `Proxiima.Composition.capabilityTagCharacter`. A tag must have a first character.
fn is_valid_capability_tag(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[cfg(test)]
mod tests {
    use super::CapabilityTag;

    #[test]
    fn tag_character_classes_match_ledger_vectors() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../tests/ledger_vectors.json")).unwrap();
        let rows = vectors["Proxiima.Composition.capabilityTagCharacter"]
            .as_array()
            .unwrap();
        assert_eq!(rows.len(), 8);
        for row in rows {
            let ch = match row["in"][1].as_str().unwrap() {
                "asciiLowercase" => 'a',
                "asciiDigit" => '0',
                "hyphen" => '-',
                "other" => 'A',
                class => panic!("unexpected character class {class}"),
            };
            let tag = match row["in"][0].as_str().unwrap() {
                "first" => ch.to_string(),
                "subsequent" => format!("a{ch}"),
                position => panic!("unexpected tag position {position}"),
            };
            assert_eq!(
                CapabilityTag::parse(&tag).is_ok(),
                row["out"].as_bool().unwrap(),
                "{tag}"
            );
        }
    }

    #[test]
    fn rejects_prefixed_uppercase_slash_and_leading_digit() {
        for raw in [
            "",
            "proxiima-notes/actor",
            "Actor",
            "act/or",
            "1actor",
            "-actor",
        ] {
            assert!(
                CapabilityTag::parse(raw).is_err(),
                "{raw:?} must not parse as a capability tag",
            );
        }
    }
}
