//! The settings this crate reads ([`proxiima_core::settings!`]).

proxiima_core::settings! {
    PROXIIMA_S3_BUCKET: Text {
        purpose: "S3 bucket of cited blobs",
        when_needed: "enable S3 storage",
    }
    PROXIIMA_S3_REGION: Text {
        purpose: "S3 region",
        when_needed: "S3 storage",
    }
    PROXIIMA_S3_ACCESS_KEY_ID: Text {
        purpose: "S3 access key",
        when_needed: "S3 storage",
    }
    PROXIIMA_S3_SECRET_ACCESS_KEY: Secret {
        purpose: "S3 secret key",
        when_needed: "S3 storage",
    }
    PROXIIMA_S3_ENDPOINT_URL: Url {
        purpose: "object I/O endpoint, also signed into upload URLs; AWS endpoint resolution when unset",
        when_needed: "non-AWS S3 storage",
    }
    PROXIIMA_S3_STORE_ID: Text {
        purpose: "object store identity, 1 to 64 of `[A-Za-z0-9._-]`; derived from the endpoint and bucket when unset",
        when_needed: "processes reach one store by different endpoints, or stores share an endpoint and bucket",
    }
    PROXIIMA_S3_FORCE_PATH_STYLE: Boolean {
        purpose: "S3 path-style switch",
        when_needed: "path-style endpoint",
        default: "false",
    }
    PROXIIMA_S3_UPLOAD_TTL_SECONDS: Seconds {
        purpose: "upload URL lifetime, at most 604800 seconds (7 days)",
        when_needed: "cited blob uploads",
        default: "900",
    }
    PROXIIMA_S3_MAX_BLOB_BYTES: Integer {
        purpose: "cited blob size ceiling in bytes",
        when_needed: "S3 blob storage",
        default: "104857600",
    }
}

#[cfg(test)]
mod tests {
    use crate::S3RuntimeConfig;
    use crate::config::DEFAULT_MAX_BLOB_BYTES;

    /// Every declared default of this crate is what its reader does unset.
    /// The reader reads the optional rows only beside a bucket, so the
    /// environment names one with its required companions. An unset blob
    /// ceiling stays `None` until the store applies the default, so the
    /// comparison is of the effective ceiling.
    #[test]
    fn declared_defaults_are_what_the_reader_does_unset() {
        let base = [
            ("PROXIIMA_S3_BUCKET", "bucket"),
            ("PROXIIMA_S3_REGION", "eu-central-1"),
            ("PROXIIMA_S3_ACCESS_KEY_ID", "access"),
            ("PROXIIMA_S3_SECRET_ACCESS_KEY", "secret"),
        ];
        proxiima_core::test_fixtures::assert_declared_defaults(super::SETTINGS, &base, |lookup| {
            let config = S3RuntimeConfig::from_lookup(&lookup).map(|config| {
                config.map(|mut config| {
                    config.max_blob_bytes =
                        Some(config.max_blob_bytes.unwrap_or(DEFAULT_MAX_BLOB_BYTES));
                    config
                })
            });
            format!("{config:?}")
        });
    }
}
