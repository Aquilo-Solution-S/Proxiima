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
    /// Both readers answer `None` for an environment that leaves them at
    /// their defaults (the pool reader also when a value is merely present),
    /// so the comparison is of the effective values: `None` becomes the
    /// type's `Default`.
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
}
