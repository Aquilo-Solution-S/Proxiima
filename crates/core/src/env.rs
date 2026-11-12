//! Configuration settings and the one rule for reading them.
//!
//! Each crate that reads a setting declares it once, in its
//! [`settings!`](crate::settings!) block, as a [`Setting`]: name, purpose,
//! when it is needed, kind of value and default. A setting is read only
//! through [`Setting::read`], which trims the value and treats an empty one
//! as "not configured". The operator page `docs/configuration.md` is
//! rendered from these declarations. This crate's own block is
//! [`SETTINGS`].

/// One configuration setting: its name, what it is for, when an operator
/// needs it, the kind of value it takes, and its default.
///
/// Declared once, in its crate's [`settings!`](crate::settings!) block, and
/// read only through [`Self::read`]. Reading by bare name is private to
/// this crate:
///
/// ```compile_fail,E0603
/// let _ = proxiima_core::env::env_value(&proxiima_core::process_env, "PATH");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    name: &'static str,
    purpose: &'static str,
    when_needed: &'static str,
    kind: SettingKind,
    default: Option<&'static str>,
}

impl Setting {
    /// The constructor [`settings!`](crate::settings!) expands to; not for
    /// direct use.
    #[doc(hidden)]
    #[must_use]
    pub const fn __declare(
        name: &'static str,
        purpose: &'static str,
        when_needed: &'static str,
        kind: SettingKind,
        default: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            purpose,
            when_needed,
            kind,
            default,
        }
    }

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

    /// The literal value an unset setting means: setting the variable to
    /// this text behaves exactly like leaving it unset. `None` when unset
    /// means "not configured" or a value derived from other settings, which
    /// the purpose then states. Each crate's test checks its declared
    /// defaults against its readers (`test_fixtures::assert_declared_defaults`
    /// under the `testkit` feature).
    #[must_use]
    pub const fn default(&self) -> Option<&'static str> {
        self.default
    }

    /// Read this setting through `lookup`, trimmed, treating an empty result
    /// as unset.
    ///
    /// An operator who writes `FOO=` or leaves whitespace has not configured
    /// `FOO`, and gets the same behaviour as leaving it out. Returning `None`
    /// rather than `Some("")` keeps that decision here instead of at each
    /// parse site, where forgetting it turns "unset" into a parse error that
    /// names a value the operator never typed.
    ///
    /// `lookup` is any environment source: [`process_env`] for the process
    /// environment, or [`env_from_pairs`] and other injected maps in tests
    /// and embedded hosts.
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
    pub fn read(&self, lookup: &impl Fn(&str) -> Option<String>) -> Option<String> {
        env_value(lookup, self.name)
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
            purpose: $purpose:literal,
            when_needed: $when_needed:literal,
            $(default: $default:literal,)?
        }
    )+) => {
        $(
            #[doc = $purpose]
            $(#[$meta])*
            pub const $name: $crate::env::Setting = $crate::env::Setting::__declare(
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

/// Read `key` through `lookup`, trimmed, treating an empty result as unset:
/// the rule [`Setting::read`] states. Private to this crate, so a setting
/// is read only through its declaration.
fn env_value(lookup: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    lookup(key).and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// The process environment as a lookup, for callers with no injected source.
///
/// [`Setting::read`] takes a lookup so that hosts and tests can supply their
/// own; the common argument is still the real environment. This is one
/// grep-able name for "the process environment enters the program here",
/// which is the question asked when auditing what a deployment can
/// influence.
///
/// Pass it by name, not by call: it is an ordinary `fn` item and coerces
/// wherever an `impl Fn(&str) -> Option<String>` is wanted.
///
/// ```
/// let _path = proxiima_core::process_env("PATH");
/// ```
///
/// Note this is the *raw* lookup: it does not trim and does not treat an
/// empty value as unset. Those are [`Setting::read`]'s job, and every
/// configuration read goes through it.
#[must_use]
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// An environment lookup over fixed `pairs`, for a host that supplies its
/// configuration by value instead of from the process environment, and for
/// tests. When a key appears more than once, the later pair wins.
///
/// ```
/// let env = proxiima_core::env_from_pairs(&[("MODE", "a"), ("MODE", "b")]);
/// assert_eq!(env("MODE").as_deref(), Some("b"));
/// assert_eq!(env("MISSING"), None);
/// ```
pub fn env_from_pairs<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |key| {
        pairs
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == key)
            .map(|(_, value)| (*value).to_owned())
    }
}

/// The boolean a setting's value spells: `1`, `true`, `yes` or `on` is true;
/// `0`, `false`, `no` or `off` is false; case and surrounding whitespace do
/// not matter. Anything else is `None`, and the caller names the setting in
/// its own error. Every boolean setting of every crate reads its words here,
/// so one setting cannot mean different things to different readers.
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
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
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

    #[test]
    fn whitespace_only_is_unset() {
        assert_eq!(FOO.read(&env(&[("FOO", " \t\n ")])), None);
    }

    /// Trailing newline from a here-doc or secret mount is trimmed, not rejected.
    #[test]
    fn surrounding_whitespace_is_trimmed_not_rejected() {
        assert_eq!(FOO.read(&env(&[("FOO", "900\n")])).as_deref(), Some("900"));
    }

    #[test]
    fn interior_whitespace_is_preserved() {
        assert_eq!(FOO.read(&env(&[("FOO", " a b ")])).as_deref(), Some("a b"));
    }

    /// Every declared default of this crate is what its readers do unset.
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
