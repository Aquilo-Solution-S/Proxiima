//! Three-state owner mode for a memory sidecar.

/// How a memory sidecar relates to its memory's owner.
///
/// The owner relationship controls write stamping and inverse traversal
/// under `Proxiima.Payloads.stampsOwner` and `Proxiima.Payloads.pinnedInverse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarOwnerMode {
    None,
    Pinned,
    Follows,
}

impl SidecarOwnerMode {
    /// Whether the sidecar table has an `owner_id` column.
    #[must_use]
    pub const fn has_owner_column(self) -> bool {
        // `Proxiima.Payloads.stampsOwner`.
        matches!(self, Self::Pinned | Self::Follows)
    }

    /// Whether the row retains its stamped owner when its Memory transfers.
    #[must_use]
    pub const fn is_pinned(self) -> bool {
        matches!(self, Self::Pinned)
    }

    /// Whether insert stamps `owner_id` from `proxiima_core.memory`.
    #[must_use]
    pub const fn stamps_owner_at_insert(self) -> bool {
        self.has_owner_column()
    }
}

#[cfg(test)]
mod tests {
    use super::SidecarOwnerMode;

    // `Proxiima.Payloads.stampsOwner`.
    #[test]
    fn owner_stamping_matches_ledger_vectors() {
        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            inputs: [String; 1],
            out: bool,
        }
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../../core/tests/ledger_vectors.json"))
                .expect("valid ledger vectors");
        let rows: Vec<Vector> =
            serde_json::from_value(vectors["Proxiima.Payloads.stampsOwner"].clone())
                .expect("owner stamping vectors");
        assert_eq!(rows.len(), 3);
        for row in rows {
            let [mode] = row.inputs;
            let mode = match mode.as_str() {
                "none" => SidecarOwnerMode::None,
                "pinned" => SidecarOwnerMode::Pinned,
                "follows" => SidecarOwnerMode::Follows,
                _ => panic!("unknown sidecar owner mode {mode}"),
            };
            assert_eq!(mode.stamps_owner_at_insert(), row.out);
        }
    }
}
