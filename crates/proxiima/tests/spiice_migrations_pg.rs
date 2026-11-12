//! The generic rule a spiice lane's ledger relies on, through the example
//! spiice's migrations.

/// `SQLx` records each spiice migration as SHA-384 of its file minus the
/// `ignored-chars` of the spiice's `sqlx.toml` (`\r`). The released file
/// bytes are pinned by `scripts/check-migrations.py`; this pins the
/// computation, which an `ignored-chars` edit or an `SQLx` upgrade would
/// change for every deployed ledger at once.
#[test]
fn embedded_spiice_checksums_are_sha384_of_the_files_without_carriage_returns() {
    use sha2::{Digest, Sha384};

    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spiices/example/migrations");
    let migrator = proxiima_example::migrator();
    let mut checked = 0;
    for entry in std::fs::read_dir(&directory).expect("spiice migrations directory") {
        let path = entry.expect("migration entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Some(version) = name
            .strip_suffix(".sql")
            .and_then(|stem| stem.split('_').next())
            .and_then(|version| version.parse::<i64>().ok())
        else {
            continue;
        };
        let bytes: Vec<u8> = std::fs::read(&path)
            .expect("readable migration")
            .into_iter()
            .filter(|byte| *byte != b'\r')
            .collect();
        let embedded = migrator
            .iter()
            .find(|migration| migration.version == version)
            .unwrap_or_else(|| panic!("{name} is not embedded"));
        assert_eq!(
            embedded.checksum.as_ref(),
            &Sha384::digest(&bytes)[..],
            "{name}: SQLx checksum computation changed"
        );
        checked += 1;
    }
    assert_eq!(
        checked,
        migrator.iter().count(),
        "every embedded migration checked"
    );
}
