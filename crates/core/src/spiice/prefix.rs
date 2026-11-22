/// Bytewise prefix comparison for `proxiima_spiice!` const assertions.
/// The macro checks typed schema and tool prefixes at compile time under
/// `Proxiima.Composition.prefixCheck`.
#[must_use]
pub const fn schema_id_has_prefix(id: &str, prefix: &str) -> bool {
    let (id, prefix) = (id.as_bytes(), prefix.as_bytes());
    if prefix.len() > id.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if id[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}
