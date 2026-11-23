use std::time::Duration;

use proxiima_blob_s3::S3RuntimeConfig;
use proxiima_core::Setting;
use proxiima_core::publication::{PublicationConfig, PublicationLimits, PublicationSource};
use proxiima_storage_pg::{PgPoolConfig, PgTuning};

use crate::ProxiimaError;
use crate::settings::{
    PROXIIMA_OBJECT_PURGE_INTERVAL_SECS as ENV_OBJECT_PURGE_INTERVAL_SECS,
    PROXIIMA_OUTBOX_MAX_PAYLOAD_BYTES as ENV_OUTBOX_MAX_PAYLOAD_BYTES,
    PROXIIMA_OUTBOX_MAX_PENDING as ENV_OUTBOX_MAX_PENDING,
    PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS as ENV_OUTBOX_PUBLISHED_RETENTION_SECS,
    PROXIIMA_PUBLICATION_SOURCE as ENV_PUBLICATION_SOURCE,
};

/// Read object storage settings through the blob crate's parser.
pub(crate) fn s3_from_lookup(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
) -> Result<Option<S3RuntimeConfig>, ProxiimaError> {
    S3RuntimeConfig::from_lookup_source(lookup)
        .map_err(|error| ProxiimaError::Config(error.to_string()))
}

/// Read query tuning through the storage crate's parser.
pub(crate) fn pg_tuning_from_lookup(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
) -> Result<Option<PgTuning>, ProxiimaError> {
    PgTuning::from_lookup(lookup).map_err(|error| ProxiimaError::Config(error.to_string()))
}

/// Read pool policy through the storage crate's parser.
pub(crate) fn pg_pool_config_from_lookup(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
) -> Result<Option<PgPoolConfig>, ProxiimaError> {
    PgPoolConfig::from_lookup(lookup).map_err(|error| ProxiimaError::Config(error.to_string()))
}

/// How often the object-purge feature runs when nothing says otherwise.
pub(crate) const DEFAULT_OBJECT_PURGE_INTERVAL: Duration = Duration::from_secs(
    proxiima_core::ledger!(Proxiima.ObjectMaintenance.purgeIntervalSeconds),
);

/// Read the object-purge interval. `Some(Duration::ZERO)` is the explicit
/// opt-out; `None` leaves the default.
///
/// # Errors
///
/// [`ProxiimaError::Config`] for a non-numeric value.
pub(crate) fn object_purge_interval_from_lookup(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
) -> Result<Option<Duration>, ProxiimaError> {
    let Some(raw) = ENV_OBJECT_PURGE_INTERVAL_SECS.read_source(lookup) else {
        return Ok(None);
    };
    let seconds: u64 = raw.parse().map_err(|_| {
        ProxiimaError::Config(format!(
            "{ENV_OBJECT_PURGE_INTERVAL_SECS} must be a whole number of seconds, got {raw:?}"
        ))
    })?;
    Ok(Some(Duration::from_secs(seconds)))
}

/// Shortest retention horizon an operator may configure.
///
/// A horizon under a minute is indistinguishable from "delete on delivery",
/// and delivery is not the same event as consumption: a consumer that is
/// behind, or an operator reconciling what was sent, has nothing left to
/// read from. One minute is not a safe replay window either — it is the
/// point below which the setting is certainly a mistake, so it is refused
/// rather than honoured.
pub(crate) const MIN_PUBLISHED_RETENTION: Duration = Duration::from_secs(proxiima_core::ledger!(
    Proxiima.Publication.minRetentionSeconds
));

/// How long a DELIVERED record is kept when nothing says otherwise.
pub(crate) const DEFAULT_PUBLISHED_RETENTION: Duration = Duration::from_secs(
    proxiima_core::ledger!(Proxiima.Publication.defaultRetentionSeconds),
);

/// Read how long a DELIVERED record is kept before the publisher task
/// reclaims it (docs/host-integration.md §Retention). `Some(Duration::ZERO)`
/// is the explicit opt-out that keeps published records forever; `None`
/// leaves the default, [`DEFAULT_PUBLISHED_RETENTION`]. Only `published`
/// rows are ever in scope; see
/// [`proxiima_core::storage_ports::publication::PublicationRetentionPort`].
///
/// # Errors
///
/// [`ProxiimaError::Config`] for a non-numeric value or a non-zero horizon
/// under [`MIN_PUBLISHED_RETENTION`].
pub(crate) fn published_retention_from_lookup(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
) -> Result<Option<Duration>, ProxiimaError> {
    let Some(raw) = ENV_OUTBOX_PUBLISHED_RETENTION_SECS.read_source(lookup) else {
        return Ok(None);
    };
    let seconds: u64 = raw.parse().map_err(|_| {
        ProxiimaError::Config(format!(
            "{ENV_OUTBOX_PUBLISHED_RETENTION_SECS} must be a whole number of seconds, \
             got {raw:?}"
        ))
    })?;
    if seconds == proxiima_core::ledger!(Proxiima.Publication.retentionOffSeconds) {
        return Ok(Some(Duration::ZERO));
    }
    let horizon = Duration::from_secs(seconds);
    if horizon < MIN_PUBLISHED_RETENTION {
        return Err(ProxiimaError::Config(format!(
            "{ENV_OUTBOX_PUBLISHED_RETENTION_SECS} is {seconds}s, under the \
             {}s floor; set 0 to keep published records forever",
            MIN_PUBLISHED_RETENTION.as_secs()
        )));
    }
    Ok(Some(horizon))
}

/// Read the publication capture block for the engine's authorization witness.
///
/// # Errors
///
/// [`ProxiimaError::Config`] for a source that is not an absolute URI/URN, a
/// non-numeric or zero bound.
pub(crate) fn publication_config_from_lookup(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
) -> Result<Option<PublicationConfig>, ProxiimaError> {
    let source = ENV_PUBLICATION_SOURCE
        .read_source(lookup)
        .map(|raw| {
            PublicationSource::new(raw).map_err(|error| {
                ProxiimaError::Config(format!("{ENV_PUBLICATION_SOURCE}: {error}"))
            })
        })
        .transpose()?;
    let max_pending = parse_positive::<u64>(lookup, &ENV_OUTBOX_MAX_PENDING)?;
    let max_payload_bytes = parse_positive::<usize>(lookup, &ENV_OUTBOX_MAX_PAYLOAD_BYTES)?;
    if source.is_none() && max_pending.is_none() && max_payload_bytes.is_none() {
        return Ok(None);
    }
    let defaults = PublicationLimits::default();
    let limits = PublicationLimits {
        max_pending: max_pending.unwrap_or(defaults.max_pending),
        max_payload_bytes: max_payload_bytes.unwrap_or(defaults.max_payload_bytes),
    };
    Ok(Some(PublicationConfig { source, limits }))
}

/// A bound must be a positive integer. Zero is refused rather than treated
/// as "unlimited": both limits are refusals, and a zero one would refuse
/// every listenable write instead of none.
fn parse_positive<T: std::str::FromStr + PartialEq + From<u8>>(
    lookup: &(impl proxiima_core::EnvLookup + ?Sized),
    key: &Setting,
) -> Result<Option<T>, ProxiimaError> {
    let Some(raw) = key.read_source(lookup) else {
        return Ok(None);
    };
    let value: T = raw.parse().map_err(|_| {
        ProxiimaError::Config(format!("{key} must be a positive integer, got {raw:?}"))
    })?;
    if value == T::from(0) {
        return Err(ProxiimaError::Config(format!(
            "{key} must be greater than zero; a zero bound refuses every listenable write"
        )));
    }
    Ok(Some(value))
}

pub(crate) fn parse_bool_value(key: &Setting, raw: &str) -> Result<bool, ProxiimaError> {
    proxiima_core::parse_bool_word(raw)
        .ok_or_else(|| ProxiimaError::Config(format!("{key} must be a boolean, got {raw:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;
    use proxiima_core::env_from_pairs as env;

    const ENV_PUBLICATION_SOURCE: &str = settings::PROXIIMA_PUBLICATION_SOURCE.name();
    const ENV_OUTBOX_MAX_PENDING: &str = settings::PROXIIMA_OUTBOX_MAX_PENDING.name();
    const ENV_OUTBOX_MAX_PAYLOAD_BYTES: &str = settings::PROXIIMA_OUTBOX_MAX_PAYLOAD_BYTES.name();
    const ENV_OUTBOX_PUBLISHED_RETENTION_SECS: &str =
        settings::PROXIIMA_OUTBOX_PUBLISHED_RETENTION_SECS.name();

    /// The retention knob leaves the default when unset, spells "off" as `0`,
    /// and refuses a horizon so short that delivery and deletion would be the
    /// same event.
    #[test]
    fn the_published_retention_horizon_is_floored_and_zero_is_off() {
        assert_eq!(published_retention_from_lookup(&env(&[])).unwrap(), None);
        assert_eq!(
            published_retention_from_lookup(&env(&[(ENV_OUTBOX_PUBLISHED_RETENTION_SECS, "0")]))
                .unwrap(),
            Some(Duration::ZERO),
            "0 is the explicit spelling of keep-forever, not a zero-second horizon"
        );
        assert_eq!(
            published_retention_from_lookup(&env(&[(
                ENV_OUTBOX_PUBLISHED_RETENTION_SECS,
                "604800"
            )]))
            .unwrap(),
            Some(Duration::from_hours(24 * 7))
        );
        let too_short =
            published_retention_from_lookup(&env(&[(ENV_OUTBOX_PUBLISHED_RETENTION_SECS, "59")]))
                .expect_err("under the floor");
        assert!(
            too_short
                .to_string()
                .contains(ENV_OUTBOX_PUBLISHED_RETENTION_SECS),
            "{too_short}"
        );
        let malformed = published_retention_from_lookup(&env(&[(
            ENV_OUTBOX_PUBLISHED_RETENTION_SECS,
            "a week",
        )]))
        .expect_err("not a number");
        assert!(malformed.to_string().contains("a week"), "{malformed}");
    }

    #[test]
    fn pg_pool_config_uses_the_injected_lookup() {
        let config = pg_pool_config_from_lookup(&env(&[("PROXIIMA_PG_MAX_CONNECTIONS", "4")]))
            .unwrap()
            .expect("non-default pool config");
        assert_eq!(config.max_connections, 4);
    }

    #[test]
    fn a_malformed_publication_block_is_a_boot_error() {
        let too_wide = format!("{}0", usize::MAX);
        for pairs in [
            vec![(ENV_PUBLICATION_SOURCE, "not a uri")],
            vec![(ENV_PUBLICATION_SOURCE, "urn:example:bad%XX")],
            vec![(ENV_OUTBOX_MAX_PENDING, "lots")],
            vec![(ENV_OUTBOX_MAX_PENDING, "0")],
            vec![(ENV_OUTBOX_MAX_PAYLOAD_BYTES, "-1")],
            vec![(ENV_OUTBOX_MAX_PAYLOAD_BYTES, "0")],
            vec![(ENV_OUTBOX_MAX_PAYLOAD_BYTES, &too_wide)],
        ] {
            let err = publication_config_from_lookup(&env(&pairs))
                .expect_err("a malformed publication block must refuse the boot");
            assert!(
                matches!(err, ProxiimaError::Config(_)),
                "{err} for {pairs:?}"
            );
        }
    }
}
