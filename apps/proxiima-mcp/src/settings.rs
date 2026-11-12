//! The settings this host reads itself ([`proxiima::settings!`]); the
//! facade's are [`proxiima::settings::all`].

proxiima::settings! {
    PROXIIMA_TOOL_PROFILE: Choice {
        purpose: "served-surface profile: `memory` (fail-closed) or `full` (adds owner transfer and membership changes); every profile serves the declared reads",
        when_needed: "deployment commands plus mandatory reads",
        default: "memory",
    }
    PROXIIMA_TOOL_ALLOW: List {
        purpose: "canonical tool ids added to the profile",
        when_needed: "extend deployment commands",
    }
    PROXIIMA_TOOL_DENY: List {
        purpose: "canonical tool ids removed from the profile",
        when_needed: "narrow commands; mandatory reads cannot be denied",
    }
    PROXIIMA_EMBED_BASE_URL: Url {
        purpose: "embedding endpoint; redirects are refused, so configure the provider's final endpoint directly",
        when_needed: "enable embeddings",
    }
    PROXIIMA_EMBED_API_KEY: Secret {
        purpose: "embedding bearer",
        when_needed: "hosted embedding endpoint",
    }
    PROXIIMA_EMBED_MODEL: Text {
        purpose: "embedding model",
        when_needed: "enable embeddings",
    }
    PROXIIMA_EMBED_DIM: Integer {
        purpose: "embedding vector width, one the store indexes",
        when_needed: "non-default model width",
        default: "1024",
    }
    PROXIIMA_EMBED_MATRYOSHKA: Boolean {
        purpose: "nested-prefix request switch",
        when_needed: "Matryoshka model",
        default: "false",
    }
    PROXIIMA_EMBED_MAX_INPUT_CHARS: Integer {
        purpose: "embedding input ceiling in characters, at least 1",
        when_needed: "provider input tuning",
        default: "16384",
    }
    PROXIIMA_EMBED_QUERY_INSTRUCTION: Text {
        purpose: "template every search query is embedded in; holds `{query}`, and `\\n` is a newline",
        when_needed: "instruction-tuned model",
    }
}

#[cfg(test)]
mod tests {
    /// Every declared default of this host is what its readers do unset.
    /// The embedding rows are read only once an endpoint and a model are
    /// named, so the environment names both.
    #[test]
    fn declared_defaults_are_what_the_readers_do_unset() {
        let registered = crate::registered_tool_ids().expect("registered keys");
        let base = [
            ("PROXIIMA_EMBED_BASE_URL", "http://127.0.0.1:1/v1"),
            ("PROXIIMA_EMBED_MODEL", "model"),
        ];
        proxiima_core::test_fixtures::assert_declared_defaults(super::SETTINGS, &base, |lookup| {
            format!(
                "{:?} {:?}",
                crate::served_surface_from_env(&lookup, &registered),
                crate::embedding_client_from_env(
                    lookup,
                    proxiima::EmbeddingRuntimePolicy::default()
                ),
            )
        });
    }

    const BEGIN: &str = "<!-- settings:begin -->\n";
    const END: &str = "<!-- settings:end -->";

    /// The operator page's table is the facade's rows and this host's,
    /// rendered. On a difference the test rewrites the region between the
    /// markers and fails, so the regenerated page is committed with the
    /// declaration that changed it.
    #[test]
    fn the_operator_page_is_the_rendered_settings_table() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/configuration.md");
        let page = std::fs::read_to_string(&path).expect("read docs/configuration.md");
        let start = page
            .find(BEGIN)
            .expect("docs/configuration.md has the begin marker")
            + BEGIN.len();
        let end = start
            + page[start..]
                .find(END)
                .expect("docs/configuration.md has the end marker");
        let table = format!(
            "\n{}\n",
            proxiima::settings::markdown_table(proxiima::settings::all().chain(super::SETTINGS))
        );
        if page[start..end] != table {
            std::fs::write(&path, format!("{}{table}{}", &page[..start], &page[end..]))
                .expect("write docs/configuration.md");
            panic!(
                "docs/configuration.md differed from the settings declarations; \
                 it was regenerated and must be committed"
            );
        }
    }
}
