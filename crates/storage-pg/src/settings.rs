//! The settings this crate reads ([`proxiima_core::settings!`]).

proxiima_core::settings! {
    PROXIIMA_PG_MAX_CONNECTIONS: Integer {
        purpose: "size of each pool, at least 1: runtime, platform, and the migration pool of boot and the migrate step",
        when_needed: "pool tuning",
        default: "5",
    }
    PROXIIMA_PG_STATEMENT_TIMEOUT_MS: Milliseconds {
        purpose: "statement timeout of every pool, sent as a startup option; `0` sends none",
        when_needed: "database tuning",
        default: "300000",
    }
    PROXIIMA_PG_ACQUIRE_TIMEOUT_SECS: Seconds {
        purpose: "pool acquire timeout",
        when_needed: "database tuning",
        default: "5",
    }
    PROXIIMA_PG_IDLE_TIMEOUT_SECS: Seconds {
        purpose: "idle connection timeout",
        when_needed: "database tuning",
        default: "600",
    }
    PROXIIMA_PG_MAX_LIFETIME_SECS: Seconds {
        purpose: "connection lifetime",
        when_needed: "database tuning",
        default: "1800",
    }
    PROXIIMA_PG_HNSW_EF_SEARCH: Integer {
        purpose: "HNSW search breadth, 1 to 1000",
        when_needed: "semantic search tuning",
        default: "100",
    }
    PROXIIMA_PG_HNSW_ITERATIVE_SCAN: Choice {
        purpose: "iterative scan mode: `off`, `strict_order` or `relaxed_order`",
        when_needed: "filtered semantic search",
        default: "relaxed_order",
    }
    PROXIIMA_PG_HNSW_MAX_SCAN_TUPLES: Integer {
        purpose: "iterative scan bound, 1 to 2147483647",
        when_needed: "filtered semantic search",
        default: "20000",
    }
}

#[cfg(test)]
mod tests {
    use crate::{PgPoolConfig, PgTuning};

    /// Every declared default of this crate is what its readers do unset.
    /// Compare effective values through `Proxiima.DatabaseAuthority.overlaySupplied`.
    #[test]
    fn declared_defaults_are_what_the_readers_do_unset() {
        proxiima_core::test_fixtures::assert_declared_defaults(super::SETTINGS, &[], |lookup| {
            format!(
                "{:?}",
                (
                    PgPoolConfig::from_lookup(&lookup).map(Option::unwrap_or_default),
                    PgTuning::from_lookup(&lookup).map(Option::unwrap_or_default),
                )
            )
        });
    }

    #[test]
    fn overlay_presence_matches_ledger_vectors() {
        #[derive(serde::Deserialize)]
        struct Vector {
            #[serde(rename = "in")]
            input: [String; 2],
            out: bool,
        }

        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json"))
                .expect("ledger vectors are valid JSON");
        let rows: Vec<Vector> =
            serde_json::from_value(vectors["Proxiima.DatabaseAuthority.overlaySupplied"].clone())
                .expect("the ledger emits the overlay decision");

        assert_eq!(rows.len(), 6);
        for Vector {
            input: [block, source],
            out,
        } in rows
        {
            let (setting, non_default) = match block.as_str() {
                "pool" => (super::PROXIIMA_PG_MAX_CONNECTIONS, "25"),
                "tuning" => (super::PROXIIMA_PG_HNSW_EF_SEARCH, "101"),
                _ => panic!("unknown overlay block {block}"),
            };
            let input = match source.as_str() {
                "unset" => None,
                "explicitDefault" => Some(setting.default().expect("declared default")),
                "nonDefault" => Some(non_default),
                _ => panic!("unknown overlay source {source}"),
            };
            let lookup = |key: &str| input.filter(|_| key == setting.name()).map(str::to_owned);
            let supplied = match block.as_str() {
                "pool" => PgPoolConfig::from_lookup(&lookup).unwrap().is_some(),
                "tuning" => PgTuning::from_lookup(&lookup).unwrap().is_some(),
                _ => unreachable!(),
            };
            assert_eq!(supplied, out, "{block} with {source}");
        }
    }
}
