//! Configuration setting declarations and readers.

/// One configuration setting: its name, what it is for, when an operator
/// needs it, the kind of value it takes, and its default.
///
/// Declare settings with [`settings!`](crate::settings!).
///
/// ```compile_fail,E0308
/// use proxiima_core::{EnvLookup, ProcessEnv};
/// let _ = ProcessEnv.lookup("PATH");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    name: &'static str,
    purpose: &'static str,
    when_needed: &'static str,
    kind: SettingKind,
    default: Option<&'static str>,
}

/// Construction bridge for the cross-crate expansion of [`settings!`](crate::settings!).
#[doc(hidden)]
#[must_use]
pub const fn __declare_setting(
    name: &'static str,
    purpose: &'static str,
    when_needed: &'static str,
    kind: SettingKind,
    default: Option<&'static str>,
) -> Setting {
    Setting {
        name,
        purpose,
        when_needed,
        kind,
        default,
    }
}

impl Setting {
    /// The environment variable name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// What the setting configures.
    #[must_use]
    pub const fn purpose(&self) -> &'static str {
        self.purpose
    }

    /// When an operator needs to set it.
    #[must_use]
    pub const fn when_needed(&self) -> &'static str {
        self.when_needed
    }

    /// The kind of value it takes.
    #[must_use]
    pub const fn kind(&self) -> SettingKind {
        self.kind
    }

    /// The literal that behaves like omission, or `None` for an unconfigured or derived value.
    #[must_use]
    pub const fn default(&self) -> Option<&'static str> {
        self.default
    }

    /// Read an injected setting with a lookup such as [`env_from_pairs`],
    /// trimming the value and treating blanks as unset.
    ///
    /// For the process environment, use [`Self::read_source`] with [`ProcessEnv`].
    ///
    /// ```
    /// proxiima_core::settings! {
    ///     EXAMPLE_MODE: Text {
    ///         purpose: "example mode",
    ///         when_needed: "never",
    ///     }
    /// }
    ///
    /// let set = proxiima_core::env_from_pairs(&[("EXAMPLE_MODE", "  fast  ")]);
    /// let blank = proxiima_core::env_from_pairs(&[("EXAMPLE_MODE", "   ")]);
    /// let missing = proxiima_core::env_from_pairs(&[]);
    /// assert_eq!(EXAMPLE_MODE.read(&set).as_deref(), Some("fast"));
    /// assert_eq!(EXAMPLE_MODE.read(&blank), None);
    /// assert_eq!(EXAMPLE_MODE.read(&missing), None);
    /// ```
    #[must_use]
    pub fn read(&self, lookup: &(impl Fn(&str) -> Option<String> + ?Sized)) -> Option<String> {
        self.read_source(lookup)
    }

    /// Read a setting from a typed lookup, trimming the value and treating blanks as unset.
    #[must_use]
    pub fn read_source(&self, lookup: &(impl EnvLookup + ?Sized)) -> Option<String> {
        // `Proxiima.Configuration.lookupPresence`
        lookup.lookup(self).and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
    }
}

/// A setting displays as its name, so an error message can name it.
impl std::fmt::Display for Setting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

/// The kind of value a [`Setting`] takes, as the operator page states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// Free text.
    Text,
    /// A credential.
    Secret,
    /// A URL.
    Url,
    /// A boolean word ([`parse_bool_word`]).
    Boolean,
    /// A non-negative whole number.
    Integer,
    /// A whole number of seconds.
    Seconds,
    /// A whole number of milliseconds.
    Milliseconds,
    /// Comma-separated entries.
    List,
    /// A JSON document.
    Json,
    /// A file path.
    Path,
    /// A socket address, `host:port`.
    SocketAddress,
    /// One of the words its purpose names.
    Choice,
}

impl SettingKind {
    /// The kind as the operator page names it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Secret => "secret",
            Self::Url => "URL",
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Seconds => "seconds",
            Self::Milliseconds => "milliseconds",
            Self::List => "comma-separated list",
            Self::Json => "JSON",
            Self::Path => "file path",
            Self::SocketAddress => "socket address",
            Self::Choice => "choice",
        }
    }
}

/// Declares a crate's settings: one `pub const` [`Setting`] per row, named
/// as its environment variable, and `pub const SETTINGS: &[Setting]` with
/// every row in declaration order.
///
/// A row names the variable and its [`SettingKind`], then its purpose, when
/// it is needed and, when unset stands for a literal value, that value as an
/// operator would write it ([`Setting::default`]). A doc comment on a row is appended to the
/// constant's documentation, which starts with the purpose.
///
/// ```
/// proxiima_core::settings! {
///     EXAMPLE_TIMEOUT_SECONDS: Seconds {
///         purpose: "request timeout",
///         when_needed: "slow upstream",
///         default: "30",
///     }
///     EXAMPLE_TOKEN: Secret {
///         purpose: "upstream bearer token",
///         when_needed: "authenticated upstream",
///     }
/// }
///
/// assert_eq!(EXAMPLE_TIMEOUT_SECONDS.name(), "EXAMPLE_TIMEOUT_SECONDS");
/// assert_eq!(EXAMPLE_TIMEOUT_SECONDS.default(), Some("30"));
/// assert_eq!(EXAMPLE_TOKEN.default(), None);
/// assert_eq!(SETTINGS, &[EXAMPLE_TIMEOUT_SECONDS, EXAMPLE_TOKEN]);
/// ```
#[macro_export]
macro_rules! settings {
    (@default) => { ::core::option::Option::None };
    (@default $default:literal) => { ::core::option::Option::Some($default) };
    ($(
        $(#[$meta:meta])*
        $name:ident: $kind:ident {
            purpose: $purpose:expr,
            when_needed: $when_needed:literal,
            $(default: $default:literal,)?
        }
    )+) => {
        $(
            #[doc = $purpose]
            $(#[$meta])*
            pub const $name: $crate::env::Setting = $crate::env::__declare_setting(
                ::core::stringify!($name),
                $purpose,
                $when_needed,
                $crate::env::SettingKind::$kind,
                $crate::settings!(@default $($default)?),
            );
        )+

        /// Every setting this crate reads, in declaration order.
        pub const SETTINGS: &[$crate::env::Setting] = &[$($name),+];
    };
}

settings! {
    PROXIIMA_EMBED_REQUEST_TIMEOUT_SECONDS: Seconds {
        purpose: "provider request timeout, 1 to 3600 seconds",
        when_needed: "embedding worker tuning",
        default: "120",
    }
    PROXIIMA_EMBED_BATCH_SIZE: Integer {
        purpose: "texts per provider request, 1 to 1024",
        when_needed: "embedding worker tuning",
        default: "32",
    }
    PROXIIMA_EMBED_DRAIN_CONCURRENCY: Integer {
        purpose: "batches one drain pass sends at once, 1 to 64",
        when_needed: "indexing throughput the provider can take",
        default: "1",
    }
    PROXIIMA_EMBED_WORKER_INTERVAL_SECONDS: Seconds {
        purpose: "first idle wait of the embedding worker, 1 to 3600 seconds; doubles while drains find nothing, up to 60 s or this value if longer; a write committed in the process wakes it at once",
        when_needed: "embedding worker tuning",
        default: "5",
    }
    PROXIIMA_EMBED_STALE_CLAIM_TIMEOUT_SECONDS: Seconds {
        purpose: "stale claim timeout, 1 to 86400 seconds and longer than the request timeout",
        when_needed: "embedding worker recovery",
        default: "900",
    }
    PROXIIMA_PLAINTEXT_HOSTS: List {
        purpose: "exact host names, besides loopback, that outbound endpoints may reach over plaintext HTTP",
        when_needed: "named internal HTTP sidecar",
    }
}

/// Source of a declared setting's environment text.
pub trait EnvLookup {
    /// Supply the text for a declaration to [`Setting::read_source`].
    #[doc(hidden)]
    fn lookup(&self, setting: &Setting) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String> + ?Sized> EnvLookup for F {
    fn lookup(&self, setting: &Setting) -> Option<String> {
        self(setting.name())
    }
}

/// Source for settings read from the process environment.
#[derive(Clone, Copy, Debug)]
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn lookup(&self, setting: &Setting) -> Option<String> {
        // Process environment reads go through declarations from settings!.
        std::env::var(setting.name()).ok()
    }
}

/// Build a lookup from fixed pairs for embedded hosts and tests, using the last pair for each key.
///
/// ```
/// let env = proxiima_core::env_from_pairs(&[("MODE", "a"), ("MODE", "b")]);
/// assert_eq!(env("MODE").as_deref(), Some("b"));
/// assert_eq!(env("MISSING"), None);
/// ```
pub fn env_from_pairs<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |key| {
        // Search backward so the last supplied value wins for a repeated setting.
        pairs
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| (*value).to_owned())
    }
}

/// Parse a trimmed, case-insensitive boolean word, returning true for
/// `1`, `true`, `yes`, and `on`, false for `0`, `false`, `no`, and `off`,
/// and `None` for any other input.
///
/// ```
/// use proxiima_core::parse_bool_word;
///
/// assert_eq!(parse_bool_word(" On\n"), Some(true));
/// assert_eq!(parse_bool_word("NO"), Some(false));
/// assert_eq!(parse_bool_word("maybe"), None);
/// ```
#[must_use]
pub fn parse_bool_word(raw: &str) -> Option<bool> {
    let word = raw.trim().to_ascii_lowercase();
    if crate::ledger!(Proxiima.Configuration.trueWords).contains(&word.as_str()) {
        Some(true)
    } else if crate::ledger!(Proxiima.Configuration.falseWords).contains(&word.as_str()) {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{env_from_pairs as env, parse_bool_word};

    crate::settings! {
        FOO: Text {
            purpose: "test setting",
            when_needed: "never",
        }
    }

    #[test]
    fn boolean_words_are_the_documented_set() {
        for word in ["1", "true", "YES", " On\n"] {
            assert_eq!(parse_bool_word(word), Some(true), "{word:?}");
        }
        for word in ["0", "False", "no", "OFF"] {
            assert_eq!(parse_bool_word(word), Some(false), "{word:?}");
        }
        for word in ["", "2", "y", "enabled", "tru e"] {
            assert_eq!(parse_bool_word(word), None, "{word:?}");
        }
    }

    #[derive(serde::Deserialize)]
    struct Vector<Output> {
        #[serde(rename = "in")]
        inputs: [String; 1],
        out: Output,
    }

    fn vectors<Output: serde::de::DeserializeOwned>(name: &str) -> Vec<Vector<Output>> {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../tests/ledger_vectors.json")).unwrap();
        serde_json::from_value(vectors[name].clone()).expect("configuration vectors")
    }

    // `Proxiima.Configuration.lookupPresence`
    #[test]
    fn setting_presence_matches_ledger_vectors() {
        let cases: Vec<Vector<String>> = vectors("Proxiima.Configuration.lookupPresence");
        assert_eq!(cases.len(), 3);
        for Vector {
            inputs: [class],
            out,
        } in cases
        {
            let pairs: &[(&str, &str)] = match class.as_str() {
                "missing" => &[],
                "blank" => &[("FOO", " \t\n ")],
                "nonblank" => &[("FOO", " 900 ")],
                other => panic!("unknown lookup text: {other}"),
            };
            let expected = match out.as_str() {
                "omitted" => None,
                "supplied" => Some("900"),
                other => panic!("unknown lookup presence: {other}"),
            };
            assert_eq!(FOO.read(&env(pairs)).as_deref(), expected, "{class}");
        }
    }

    #[test]
    fn last_repeated_pair_wins() {
        let pairs = [("FOO", "earlier"), ("FOO", "later")];
        assert_eq!(FOO.read(&env(&pairs)).as_deref(), Some("later"));
    }

    /// Trailing newline from a here-doc or secret mount is trimmed, not rejected.
    #[test]
    fn surrounding_whitespace_is_trimmed_not_rejected() {
        assert_eq!(FOO.read(&env(&[("FOO", "900\n")])).as_deref(), Some("900"));
    }

    #[test]
    fn interior_whitespace_is_preserved() {
        assert_eq!(
            FOO.read(&|name| (name == "FOO").then(|| " a b ".to_owned()))
                .as_deref(),
            Some("a b")
        );
    }

    #[test]
    fn declared_defaults_are_what_the_readers_do_unset() {
        crate::test_fixtures::assert_declared_defaults(super::SETTINGS, &[], |lookup| {
            (
                crate::EmbeddingRuntimePolicy::from_lookup(&lookup),
                crate::PlaintextHosts::from_lookup(&lookup),
            )
        });
    }

    #[test]
    fn a_declaration_names_its_variable_and_lists_its_rows() {
        assert_eq!(FOO.name(), "FOO");
        assert_eq!(FOO.to_string(), "FOO");
        assert_eq!(SETTINGS, &[FOO]);
    }
}
