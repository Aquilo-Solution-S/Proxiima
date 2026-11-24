//! Host-served MCP resources: static resources and URI templates a host
//! exposes beside Proxiima's `proxiima://` substrate resources.

use async_trait::async_trait;
use base64::Engine as _;
use proxiima_core::{ToolCtx, ToolError};

use crate::auth::AuthContext;

/// The URI scheme reserved for core substrate resources (`proxiima://`).
pub(crate) const RESERVED_RESOURCE_SCHEME: &str =
    proxiima_core::ledger!(Proxiima.ToolSurface.reservedResourceScheme);

/// One static resource a host serves on `resources/list`.
///
/// Built with [`Self::new`] and the `with_*` setters; the struct is
/// `#[non_exhaustive]`, so a field added later does not break a host.
///
/// ```compile_fail
/// let _resource = proxiima_server::HostResource {
///     uri: "app://items".into(),
///     name: "items".into(),
///     title: None,
///     description: None,
///     mime_type: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostResource {
    /// Non-`proxiima://` URI with a valid RFC 3986 scheme.
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

impl HostResource {
    #[must_use]
    pub fn new(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: name.into(),
            title: None,
            description: None,
            mime_type: None,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }
}

/// One URI template a host serves on `resources/templates/list`.
///
/// Built with [`Self::new`] and the `with_*` setters; the struct is
/// `#[non_exhaustive]`, so a field added later does not break a host.
///
/// ```compile_fail
/// let _template = proxiima_server::HostResourceTemplate {
///     uri_template: "app://item/{id}".into(),
///     name: "item".into(),
///     title: None,
///     description: None,
///     mime_type: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostResourceTemplate {
    /// Non-`proxiima://` URI template with a valid RFC 3986 scheme.
    pub uri_template: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

impl HostResourceTemplate {
    #[must_use]
    pub fn new(uri_template: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri_template: uri_template.into(),
            name: name.into(),
            title: None,
            description: None,
            mime_type: None,
        }
    }

    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }
}

/// One content entry returned by [`HostResources::read`].
///
/// Built with [`Self::text`], [`Self::json`], or [`Self::blob`] and inspected
/// through its accessors. Keeping the representation private ensures binary
/// payloads are always valid padded base64 and lets new fields or encodings
/// ship without breaking hosts.
///
/// ```compile_fail
/// let _content = proxiima_server::HostResourceContent {
///     uri: "app://items".into(),
///     mime_type: None,
///     text: "hi".into(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResourceContent {
    uri: String,
    mime_type: Option<String>,
    body: HostResourceBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostResourceBody {
    Text(String),
    Blob(String),
}

impl HostResourceContent {
    /// UTF-8 text resource contents.
    #[must_use]
    pub fn text(uri: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            mime_type: None,
            body: HostResourceBody::Text(text.into()),
        }
    }

    /// JSON resource contents with `application/json` MIME type.
    ///
    /// # Errors
    ///
    /// Any serialization error from `serde_json::to_string`.
    pub fn json(
        uri: impl Into<String>,
        value: &serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            uri: uri.into(),
            mime_type: Some("application/json".to_owned()),
            body: HostResourceBody::Text(serde_json::to_string(value)?),
        })
    }

    /// Binary resource contents encoded as standard padded base64.
    #[must_use]
    pub fn blob(uri: impl Into<String>, bytes: impl AsRef<[u8]>) -> Self {
        Self {
            uri: uri.into(),
            mime_type: None,
            body: HostResourceBody::Blob(base64::engine::general_purpose::STANDARD.encode(bytes)),
        }
    }

    /// Attach or replace the MIME type on this content entry.
    #[must_use]
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// The content entry's resource URI.
    #[must_use]
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// The optional MIME type declared for this content entry.
    #[must_use]
    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    /// The UTF-8 text payload, or `None` when this entry carries a binary blob.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match &self.body {
            HostResourceBody::Text(text) => Some(text),
            HostResourceBody::Blob(_) => None,
        }
    }

    /// The standard padded base64 payload, or `None` when this entry carries text.
    #[must_use]
    pub fn as_blob_base64(&self) -> Option<&str> {
        match &self.body {
            HostResourceBody::Text(_) => None,
            HostResourceBody::Blob(blob) => Some(blob),
        }
    }

    pub(crate) fn into_parts(self) -> (String, Option<String>, HostResourceBody) {
        (self.uri, self.mime_type, self.body)
    }
}

/// Source of host-served MCP resources and URI templates.
///
/// Wired into a host through `RuntimeBuilder::host_resources` or
/// [`ToolHost::with_host_resources`]. Proxiima keeps the `proxiima://`
/// scheme unconditionally: inbound requests, listings, templates, and returned
/// [`HostResourceContent`] URIs are all restricted to valid non-`proxiima://`
/// URIs.
///
/// Edge authentication runs ahead of every callback, and [`Self::read`]
/// receives a [`ToolCtx`] carrying the same boot and per-request services as
/// [`HostTools`](crate::HostTools). Unlike substrate resources and tools,
/// however, host resources bypass the registry's `RequestBehavior` onion and
/// static `ScopeKey::Resource` gate. The host therefore owns authorization for
/// its own namespace and **must** authorize every listed entry in
/// [`Self::list`] / [`Self::list_templates`] and independently authorize every
/// direct [`Self::read`] call — a client may issue `resources/read` without
/// listing first.
///
/// [`ToolHost::with_host_resources`]: crate::ToolHost::with_host_resources
#[async_trait]
pub trait HostResources: Send + Sync + std::fmt::Debug {
    /// Static resources served to `auth` on `resources/list`.
    ///
    /// # Errors
    ///
    /// Any [`ToolError`] fails the `resources/list` request.
    async fn list(&self, auth: &AuthContext) -> Result<Vec<HostResource>, ToolError>;

    /// URI templates served to `auth` on `resources/templates/list`.
    /// Default: none.
    ///
    /// # Errors
    ///
    /// Any [`ToolError`] fails the `resources/templates/list` request.
    async fn list_templates(
        &self,
        _auth: &AuthContext,
    ) -> Result<Vec<HostResourceTemplate>, ToolError> {
        Ok(Vec::new())
    }

    /// Read one non-`proxiima://` resource URI as `ctx`.
    ///
    /// Return `Ok(None)` when `uri` does not belong to this source; the server
    /// answers resource-not-found. Return `Err(ToolError::NotFound(_))` when
    /// `uri` matched a host resource or template whose target does not exist
    /// or is not visible to `ctx`.
    ///
    /// # Errors
    ///
    /// Any [`ToolError`]; mapped onto MCP resource error codes by the handler.
    async fn read(
        &self,
        uri: &str,
        ctx: ToolCtx,
    ) -> Result<Option<Vec<HostResourceContent>>, ToolError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceSchemeClass {
    Reserved,
    HostDefined,
    Invalid,
}

/// Classify a concrete host resource URI (inbound `resources/read`, static
/// `HostResource`, and outbound `HostResourceContent`) using RFC 3986 parsing
/// (`Proxiima.ToolSurface.hostResourceSchemeAdmitted`).
pub(crate) fn classify_concrete_resource_uri(uri: &str) -> ResourceSchemeClass {
    let class = classify_resource_template_scheme(uri);
    if class == ResourceSchemeClass::Invalid || fluent_uri::Uri::parse(uri).is_err() {
        return ResourceSchemeClass::Invalid;
    }
    class
}

/// Whether a follow-up [`proxiima_core::ResourceLink`] carries an admissible
/// concrete RFC 3986 URI (either host-defined or lowercase `proxiima://`) and
/// a valid name.
pub(crate) fn resource_link_refusal(uri: &str, name: &str) -> Option<&'static str> {
    match classify_concrete_resource_uri(uri) {
        ResourceSchemeClass::HostDefined => validate_resource_name(name),
        ResourceSchemeClass::Reserved if uri.starts_with("proxiima://") => {
            validate_resource_name(name)
        }
        ResourceSchemeClass::Reserved => {
            Some("substrate resource links must use the canonical lowercase proxiima:// scheme")
        }
        ResourceSchemeClass::Invalid => {
            Some("a resource link URI must be a valid RFC 3986 scheme:// URI")
        }
    }
}

/// Classify a resource URI template by its scheme
/// (`Proxiima.ToolSurface.hostResourceSchemeAdmitted`).
///
/// Kept separate from [`classify_concrete_resource_uri`] because RFC 6570
/// template expressions (`{id}`, `{?view}`) are valid in templates and not in
/// concrete RFC 3986 URIs.
pub(crate) fn classify_resource_template_scheme(uri_template: &str) -> ResourceSchemeClass {
    if uri_template.is_empty()
        || uri_template
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return ResourceSchemeClass::Invalid;
    }
    let Some((scheme, rest)) = uri_template.split_once("://") else {
        return ResourceSchemeClass::Invalid;
    };
    if rest.is_empty() || !is_valid_scheme(scheme) {
        return ResourceSchemeClass::Invalid;
    }
    if scheme.eq_ignore_ascii_case(RESERVED_RESOURCE_SCHEME) {
        ResourceSchemeClass::Reserved
    } else {
        ResourceSchemeClass::HostDefined
    }
}

/// Whether `uri` uses the reserved `proxiima://` scheme (case-insensitively).
pub(crate) fn is_reserved_resource_uri(uri: &str) -> bool {
    uri.split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case(RESERVED_RESOURCE_SCHEME))
}

/// Whether a concrete host resource entry's URI and name are admissible.
pub(crate) fn host_resource_refusal(uri: &str, name: &str) -> Option<&'static str> {
    // `Proxiima.ToolSurface.hostResourceSchemeAdmitted`.
    match classify_concrete_resource_uri(uri) {
        ResourceSchemeClass::HostDefined => validate_resource_name(name),
        ResourceSchemeClass::Reserved => {
            Some("the proxiima:// scheme is reserved for substrate resources")
        }
        ResourceSchemeClass::Invalid => {
            Some("a host resource URI must be a valid RFC 3986 scheme:// URI")
        }
    }
}

/// Whether a host resource URI template and name are admissible.
pub(crate) fn host_resource_template_refusal(
    uri_template: &str,
    name: &str,
) -> Option<&'static str> {
    // `Proxiima.ToolSurface.hostResourceSchemeAdmitted`.
    match classify_resource_template_scheme(uri_template) {
        ResourceSchemeClass::HostDefined => validate_resource_name(name),
        ResourceSchemeClass::Reserved => {
            Some("the proxiima:// scheme is reserved for substrate resources")
        }
        ResourceSchemeClass::Invalid => Some(
            "a host resource URI template must carry a valid scheme:// target with no spaces or control characters",
        ),
    }
}

fn validate_resource_name(name: &str) -> Option<&'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() || name.chars().any(char::is_control) {
        return Some("a host resource name must be nonempty and contain no control characters");
    }
    None
}

fn is_valid_scheme(scheme: &str) -> bool {
    let mut bytes = scheme.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_resource_scheme_admission_matches_ledger_vectors() {
        let cases = [
            ("reserved", "proxiima://memories/counts"),
            ("hostDefined", "app://workspaces/1"),
            ("invalid", "no-scheme"),
        ];
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../core/tests/ledger_vectors.json")).unwrap();
        for row in vectors["Proxiima.ToolSurface.hostResourceSchemeAdmitted"]
            .as_array()
            .unwrap()
        {
            let (_, sample) = cases
                .iter()
                .find(|(name, _)| row["in"][0] == *name)
                .unwrap();
            let admitted = host_resource_refusal(sample, "sample").is_none();
            assert_eq!(admitted, row["out"].as_bool().unwrap(), "{row}");
        }
    }

    #[test]
    fn scheme_classifier_rejects_reserved_case_insensitively_and_malformed_uris() {
        assert_eq!(
            classify_concrete_resource_uri("proxiima://how-to"),
            ResourceSchemeClass::Reserved
        );
        assert_eq!(
            classify_concrete_resource_uri("PROXIIMA://memory/123"),
            ResourceSchemeClass::Reserved
        );
        assert_eq!(
            classify_resource_template_scheme("app://items/{id}{?view}"),
            ResourceSchemeClass::HostDefined
        );
        assert_eq!(
            classify_concrete_resource_uri("app://items/{id}{?view}"),
            ResourceSchemeClass::Invalid
        );
        assert_eq!(
            classify_concrete_resource_uri("x-host.v1+json://catalog?limit=10"),
            ResourceSchemeClass::HostDefined
        );
        assert_eq!(
            classify_concrete_resource_uri("app://items/hello%20world"),
            ResourceSchemeClass::HostDefined
        );
        for bad in [
            "",
            "relative/path",
            "app://",
            "1bad://host",
            "app://has space",
            "app://has\0nul",
            "app://has\nnewline",
            "app://items/%ZZ",
            "app://items/%2",
            "app://items/[bad]",
            "proxiima://%ZZ",
        ] {
            assert_eq!(
                classify_concrete_resource_uri(bad),
                ResourceSchemeClass::Invalid,
                "{bad:?}"
            );
            assert!(resource_link_refusal(bad, "item").is_some(), "{bad:?}");
        }
        assert_eq!(resource_link_refusal("proxiima://memory/123", "mem"), None);
        assert_eq!(resource_link_refusal("app://doc/doc-1", "doc"), None);
        assert!(resource_link_refusal("PROXIIMA://memory/123", "mem").is_some());
        assert!(resource_link_refusal("app://doc/doc-1", "   ").is_some());
    }

    #[test]
    fn content_helpers_encode_json_and_base64_blob() {
        let json = HostResourceContent::json("app://x", &serde_json::json!({"ok": true})).unwrap();
        assert_eq!(json.uri(), "app://x");
        assert_eq!(json.mime_type(), Some("application/json"));
        assert_eq!(json.as_text(), Some(r#"{"ok":true}"#));
        assert_eq!(json.as_blob_base64(), None);

        let blob = HostResourceContent::blob("app://img", [0xfb, 0xff]).with_mime_type("image/png");
        assert_eq!(blob.uri(), "app://img");
        assert_eq!(blob.mime_type(), Some("image/png"));
        assert_eq!(blob.as_text(), None);
        assert_eq!(blob.as_blob_base64(), Some("+/8="));
    }
}
