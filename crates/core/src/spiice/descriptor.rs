/// Structured per-spiice metadata. Populated by `proxiima_spiice!` at
/// macro-expansion time so the `package_version` and `author` fields
/// reflect the calling crate's `Cargo.toml`.
///
/// One descriptor per `proxiima_spiice!` invocation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpiiceDescriptor {
    /// Spiice ID used by `proxiima_spiice!`, for example `"proxiima-notes"`.
    /// Typed and opaque cited-object schema IDs use the `<spiice>/` prefix.
    /// The macro checks typed IDs at compile time and opaque IDs at registration.
    /// Declared MCP tool prefixes accept `<spiice>/` or `<spiice>_` at compile
    /// time. Tool registration requires canonical provider-safe spelling,
    /// which uses the underscore prefix.
    pub spiice_id: String,
    /// Human-friendly name shown in the UI. Defaults to `spiice_id`
    /// when the macro caller omits `display_name`.
    pub display_name: String,
    /// `CARGO_PKG_VERSION` of the spiice crate at build time.
    pub package_version: String,
    /// First author from `CARGO_PKG_AUTHORS` (split on `:` per Cargo
    /// convention, trimmed). `None` when the crate's `authors` field
    /// is empty.
    pub author: Option<String>,
    /// How this spiice was loaded into the binary.
    pub provenance: SpiiceProvenance,
}

/// The build-time spiice provenance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpiiceProvenance {
    Builtin,
}
