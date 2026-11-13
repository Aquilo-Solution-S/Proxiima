#[test]
fn host_shaping_cannot_widen_either_tool_policy() {
    use proxiima_core::{ScopeKey, ServedSurface};
    let subject = proxiima_core::UserId::new(uuid::Uuid::now_v7());
    let search = ScopeKey::parse("core_search_memories").unwrap();
    let caller = ToolScope::Palette(vec![search.clone()]);
    let served = ServedSurface::Palette(vec![search]);
    let resolved = AuthzContext::for_subject(subject, AuthPath::HostBearer)
        .with_tool_scope(caller.clone())
        .with_served_surface(served.clone());
    let shaped = AuthzContext::for_subject(subject, AuthPath::HostBearer);
    let expiry = std::time::SystemTime::now() + std::time::Duration::from_mins(1);
    let narrowed = super::narrowed_by_host(&resolved, shaped, expiry).unwrap();
    assert_eq!(narrowed.tool_scope(), &caller);
    assert_eq!(narrowed.served_surface(), &served);
}

use std::collections::HashMap;

use async_trait::async_trait;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::DecodingKey;
use proxiima_core::{
    AccessError, ActionName, AuthPath, MemoryKind, OwnerId, OwnerRoles, Role, ScopeKey, ToolName,
    UserId,
};
use serde::Serialize;
use uuid::Uuid;

use super::*;
use crate::StaticJwksResolver;

const ISSUER: &str = "https://issuer.example";

fn key(text: &str) -> ScopeKey {
    ScopeKey::parse(text).expect("a scope key")
}

/// The call gate's question, asked in the spelling the tests read best.
fn allows_leaf(scope: &ToolScope, tool: &str, action: &str) -> bool {
    scope.allows_action(
        &ToolName::parse(tool).expect("a tool name"),
        &ActionName::parse(action).expect("an action name"),
    )
}

const AGENT_AUD: &str = "centauri-agent";
const OWNER_AUD: &str = "centauri-owner";
const KID: &str = "binding-key";

struct TestKeys {
    signing: RsaKeyPair,
    decoding: DecodingKey,
}

#[derive(Debug, Serialize)]
struct TestClaims {
    sub: String,
    iss: String,
    aud: String,
    exp: u64,
}

#[derive(Debug)]
struct StaticOwnerAccess {
    agent: UserId,
    owner: UserId,
    agent_group: proxiima_core::GroupId,
    owner_group: proxiima_core::GroupId,
}

#[async_trait]
impl OwnerAccessPort for StaticOwnerAccess {
    async fn resolve_roles_for_subject(&self, subject: UserId) -> Result<OwnerRoles, AccessError> {
        if subject == self.agent {
            return OwnerRoles::for_subject(
                subject,
                [(OwnerId::from(self.agent_group), Role::editor())],
            );
        }
        if subject == self.owner {
            return OwnerRoles::for_subject(
                subject,
                [(OwnerId::from(self.owner_group), Role::admin())],
            );
        }
        OwnerRoles::for_subject(subject, [])
    }
}

fn test_keys() -> TestKeys {
    let signing = RsaKeyPair::generate(KeySize::Rsa2048).expect("generate test RSA key");
    let decoding = DecodingKey::from_rsa_der(signing.public_key().as_ref());
    TestKeys { signing, decoding }
}

fn resolver(decoding: DecodingKey) -> Arc<dyn KeyResolver> {
    Arc::new(StaticJwksResolver::new(HashMap::from([(
        KID.to_string(),
        Arc::new(decoding),
    )])))
}

fn token(keys: &TestKeys, audience: &str, subject: &str) -> String {
    signed(
        keys,
        &TestClaims {
            sub: subject.to_owned(),
            iss: ISSUER.to_owned(),
            aud: audience.to_owned(),
            exp: jsonwebtoken::get_current_timestamp() + 3_600,
        },
    )
}

fn signed(keys: &TestKeys, claims: &impl Serialize) -> String {
    let header = serde_json::json!({"alg": "RS256", "kid": KID, "typ": "JWT"});
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("serialize header")),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("serialize claims"))
    );
    let mut signature = vec![0; keys.signing.public_modulus_len()];
    keys.signing
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .expect("sign jwt");
    format!("{}.{}", signing_input, URL_SAFE_NO_PAD.encode(signature))
}

fn config(audience: &str) -> OidcAuthConfig {
    OidcAuthConfig {
        issuer: ISSUER.to_string(),
        jwks_uri: None,
        audience: audience.to_string(),
        allowed_subjects: None,
        leeway_secs: 0,
    }
}

fn subject_map(subject: &str, user_id: UserId) -> OidcSubjectMap {
    let mut map = OidcSubjectMap::new();
    map.insert(ISSUER, subject, user_id).expect("subject map");
    map
}

fn binding_set(keys: &TestKeys) -> (OidcBindingSet, proxiima_core::GroupId) {
    let agent = UserId::new(Uuid::from_u128(0xA9E1));
    let owner = UserId::new(Uuid::from_u128(0x0E1E));
    let agent_group = proxiima_core::GroupId::new(Uuid::now_v7());
    let owner_group = proxiima_core::GroupId::new(Uuid::now_v7());
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        agent,
        owner,
        agent_group,
        owner_group,
    });
    let agent_binding = OidcBinding::with_role_shape(
        config(AGENT_AUD),
        resolver(keys.decoding.clone()),
        subject_map("agent-sub", agent),
        owner_access.clone(),
        OidcRoleShape::ServerResolvedWithToolScope(ToolScope::Palette(vec![key("core_goal:set")])),
    )
    .expect("agent binding");
    let owner_binding = OidcBinding::new(
        config(OWNER_AUD),
        resolver(keys.decoding.clone()),
        subject_map("owner-sub", owner),
        owner_access,
    )
    .expect("owner binding");
    (
        OidcBindingSet::new([agent_binding, owner_binding]).expect("binding set"),
        agent_group,
    )
}

/// Trusted model provenance is per-binding, resolved from that
/// binding's own subject map — the agent audience certifies a runner,
/// the owner audience certifies a person.
#[tokio::test]
async fn a_binding_attaches_its_own_trusted_model_id() {
    let keys = test_keys();
    let agent = UserId::new(Uuid::from_u128(0xA9E1));
    let owner = UserId::new(Uuid::from_u128(0x0E1E));
    let agent_group = proxiima_core::GroupId::new(Uuid::now_v7());
    let owner_group = proxiima_core::GroupId::new(Uuid::now_v7());
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        agent,
        owner,
        agent_group,
        owner_group,
    });
    let mut agent_map = OidcSubjectMap::new();
    agent_map
        .insert_binding(
            ISSUER,
            "agent-sub",
            crate::SubjectBinding::new(agent).with_trusted_model_id("acme/runner-v3"),
        )
        .expect("subject map");
    let bindings = OidcBindingSet::new([
        OidcBinding::new(
            config(AGENT_AUD),
            resolver(keys.decoding.clone()),
            agent_map,
            owner_access.clone(),
        )
        .expect("agent binding"),
        OidcBinding::new(
            config(OWNER_AUD),
            resolver(keys.decoding.clone()),
            subject_map("owner-sub", owner),
            owner_access,
        )
        .expect("owner binding"),
    ])
    .expect("binding set");

    let agent_ctx = bindings
        .authenticate(&Credentials::Bearer(token(&keys, AGENT_AUD, "agent-sub")))
        .await
        .expect("agent binding authenticates");
    assert_eq!(agent_ctx.trusted_model_id(), Some("acme/runner-v3"));

    let owner_ctx = bindings
        .authenticate(&Credentials::Bearer(token(&keys, OWNER_AUD, "owner-sub")))
        .await
        .expect("owner binding authenticates");
    assert_eq!(
        owner_ctx.trusted_model_id(),
        None,
        "a binding that declares no runner certifies none"
    );
}

#[tokio::test]
async fn two_binding_routing_uses_matching_audience() {
    let keys = test_keys();
    let (bindings, agent_group) = binding_set(&keys);
    let token = token(&keys, AGENT_AUD, "agent-sub");

    let ctx = bindings
        .authenticate(&Credentials::Bearer(token))
        .await
        .expect("agent binding authenticates");

    assert_eq!(ctx.auth_path(), AuthPath::HostBearer);
    assert!(ctx.may_write(&OwnerId::from(agent_group), MemoryKind::Perspective));
    assert!(allows_leaf(ctx.tool_scope(), "core_goal", "set"));
    assert!(!ctx.tool_scope().allows(&key("core_membership")));
    assert!(!allows_leaf(
        ctx.tool_scope(),
        "core_transfer",
        "transfer_to_owner"
    ));
}

#[tokio::test]
async fn unknown_audience_is_denied() {
    let keys = test_keys();
    let (bindings, _) = binding_set(&keys);
    let token = token(&keys, "unknown-api", "agent-sub");

    let err = bindings
        .authenticate(&Credentials::Bearer(token))
        .await
        .expect_err("unknown audience rejected");

    assert_eq!(err, AuthError::InvalidCredentials);
}

#[test]
fn duplicate_route_is_boot_error() {
    let keys = test_keys();
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        agent: UserId::new(Uuid::from_u128(0xA9E1)),
        owner: UserId::new(Uuid::from_u128(0x0E1E)),
        agent_group: proxiima_core::GroupId::new(Uuid::now_v7()),
        owner_group: proxiima_core::GroupId::new(Uuid::now_v7()),
    });
    let user = UserId::new(Uuid::now_v7());
    let first = OidcBinding::new(
        config(AGENT_AUD),
        resolver(keys.decoding.clone()),
        subject_map("subject-a", user),
        owner_access.clone(),
    )
    .expect("first binding");
    let second = OidcBinding::new(
        config(AGENT_AUD),
        resolver(keys.decoding),
        subject_map("subject-b", user),
        owner_access,
    )
    .expect("second binding");

    let err = OidcBindingSet::new([first, second]).expect_err("duplicate route rejected");

    assert_eq!(
        err,
        OidcBindingSetError::DuplicateRoute {
            issuer: ISSUER.to_string(),
            audience: AGENT_AUD.to_string(),
        }
    );
}

#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
struct TenantClaims {
    sub: String,
    tenant: String,
    #[serde(default)]
    groups: Vec<String>,
}

fn claims_with(audience: &str, exp: u64, extra: serde_json::Value) -> serde_json::Value {
    let mut claims = serde_json::json!({
        "sub": "owner-sub",
        "iss": ISSUER,
        "aud": audience,
        "exp": exp,
    });
    let object = claims.as_object_mut().expect("claims object");
    let serde_json::Value::Object(extra) = extra else {
        panic!("extra claims must be an object");
    };
    object.extend(extra);
    claims
}

/// A host reads its own claims from the verified payload, and a refusal
/// keeps its reason instead of collapsing to `InvalidCredentials`.
#[tokio::test]
async fn validate_with_reads_custom_claims_and_names_the_refusal() {
    let keys = test_keys();
    let validator = OidcTokenValidator::new(config(OWNER_AUD), resolver(keys.decoding.clone()))
        .expect("validator");
    let later = jsonwebtoken::get_current_timestamp() + 3_600;
    let good = signed(
        &keys,
        &claims_with(
            OWNER_AUD,
            later,
            serde_json::json!({"tenant": "acme", "groups": ["ops"]}),
        ),
    );
    let validated = validator
        .validate_with::<TenantClaims>(&good)
        .await
        .expect("valid token");
    assert_eq!(validated.claims.subject, "owner-sub");
    assert_eq!(
        validated.custom,
        TenantClaims {
            sub: "owner-sub".into(),
            tenant: "acme".into(),
            groups: vec!["ops".into()],
        }
    );

    let expired = signed(
        &keys,
        &claims_with(OWNER_AUD, 1_000, serde_json::json!({"tenant": "acme"})),
    );
    assert_eq!(
        validator.validate_with::<TenantClaims>(&expired).await,
        Err(crate::OidcRejection::Expired)
    );
    let other_audience = signed(
        &keys,
        &claims_with("other", later, serde_json::json!({"tenant": "acme"})),
    );
    assert_eq!(
        validator
            .validate_with::<TenantClaims>(&other_audience)
            .await,
        Err(crate::OidcRejection::WrongAudience {
            expected: OWNER_AUD.into()
        })
    );
    let no_tenant = signed(&keys, &claims_with(OWNER_AUD, later, serde_json::json!({})));
    assert!(matches!(
        validator.validate_with::<TenantClaims>(&no_tenant).await,
        Err(crate::OidcRejection::CustomClaims(_))
    ));
    // The plain path accepts what it accepted before: custom claims are
    // the host's business, not a validity condition.
    assert!(validator.validate(&no_tenant).await.is_ok());
    assert_eq!(
        validator.validate(&expired).await,
        Err(AuthError::InvalidCredentials)
    );
}

/// Narrows the palette to the token's `tenant`; refuses a token without.
#[derive(Debug)]
struct TenantShaper;

impl OidcRoleShaper for TenantShaper {
    fn shape(
        &self,
        context: AuthzContext,
        token: &ValidatedOidcToken<OidcClaimMap>,
    ) -> Result<AuthzContext, AuthError> {
        let tenant = token
            .custom
            .get("tenant")
            .and_then(serde_json::Value::as_str)
            .ok_or(AuthError::InvalidCredentials)?;
        // The claim is outside data: it becomes a flat tool name here,
        // or the token is refused. Going through `ScopeKey::parse` would
        // let a tenant of `a:b` spell the leaf `a:b_tool`.
        let tool = ToolName::parse(&format!("{tenant}_tool"))
            .map_err(|_| AuthError::InvalidCredentials)?;
        Ok(context.with_tool_scope(ToolScope::Palette(vec![ScopeKey::Tool(tool)])))
    }
}

#[tokio::test]
async fn a_host_role_shape_sees_every_verified_claim() {
    let keys = test_keys();
    let owner = UserId::new(Uuid::from_u128(0x0E1E));
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        agent: UserId::new(Uuid::from_u128(0xA9E1)),
        owner,
        agent_group: proxiima_core::GroupId::new(Uuid::now_v7()),
        owner_group: proxiima_core::GroupId::new(Uuid::now_v7()),
    });
    let shaper: Arc<dyn OidcRoleShaper> = Arc::new(TenantShaper);
    let bindings = OidcBindingSet::new([OidcBinding::with_role_shape(
        config(OWNER_AUD),
        resolver(keys.decoding.clone()),
        subject_map("owner-sub", owner),
        owner_access,
        OidcRoleShape::Host(shaper.clone()),
    )
    .expect("binding")])
    .expect("binding set");
    assert_eq!(
        OidcRoleShape::Host(shaper.clone()),
        OidcRoleShape::Host(shaper)
    );

    let later = jsonwebtoken::get_current_timestamp() + 3_600;
    let ctx = bindings
        .authenticate(&Credentials::Bearer(signed(
            &keys,
            &claims_with(OWNER_AUD, later, serde_json::json!({"tenant": "acme"})),
        )))
        .await
        .expect("the shaper admits a tenant token");
    assert_eq!(ctx.subject(), Some(owner));
    assert!(ctx.tool_scope().allows(&key("acme_tool")));
    assert!(!ctx.tool_scope().allows(&key("core_membership")));

    // A claim that cannot be a tool name never becomes part of a palette:
    // the delimiter in a tenant would make `{tenant}_tool` a leaf.
    for tenant in ["a:b", "resource:x", "acme tool", "a/b"] {
        let refused = bindings
            .authenticate(&Credentials::Bearer(signed(
                &keys,
                &claims_with(OWNER_AUD, later, serde_json::json!({ "tenant": tenant })),
            )))
            .await;
        assert_eq!(
            refused.err(),
            Some(AuthError::InvalidCredentials),
            "tenant {tenant:?}"
        );
    }

    let refused = bindings
        .authenticate(&Credentials::Bearer(signed(
            &keys,
            &claims_with(OWNER_AUD, later, serde_json::json!({})),
        )))
        .await;
    assert_eq!(refused.err(), Some(AuthError::InvalidCredentials));
}

/// What a shaper may do to the context it is handed.
#[derive(Debug)]
enum Reshape {
    /// Grant admin on a Group the resolved roles do not carry.
    Widen,
    /// Swap in another subject.
    OtherSubject,
    /// Drop the expiry; the binding clamps it back to the token's.
    DropExpiry,
    /// Rebuild an allowed context without carrying request claims forward.
    Rebuild,
    /// Replace the request claims while rebuilding an allowed context.
    ReplaceClaims,
}

impl OidcRoleShaper for Reshape {
    fn shape(
        &self,
        context: AuthzContext,
        _token: &ValidatedOidcToken<OidcClaimMap>,
    ) -> Result<AuthzContext, AuthError> {
        let subject = context.subject().expect("resolved subject");
        Ok(match self {
            Self::Widen => AuthzContext::for_subject_with_role(
                subject,
                [(
                    OwnerId::from(proxiima_core::GroupId::new(Uuid::now_v7())),
                    Role::admin(),
                )],
                AuthPath::HostBearer,
            ),
            Self::OtherSubject => {
                AuthzContext::for_subject(UserId::new(Uuid::now_v7()), AuthPath::HostBearer)
            }
            Self::DropExpiry => context.with_expires_at(None),
            Self::Rebuild => AuthzContext::for_subject(subject, AuthPath::HostBearer),
            Self::ReplaceClaims => AuthzContext::for_subject(subject, AuthPath::HostBearer)
                .with_verified_claims(serde_json::Map::from_iter([(
                    "application".to_owned(),
                    serde_json::json!("replacement"),
                )])),
        })
    }
}

#[tokio::test]
async fn a_host_role_shape_narrows_or_refuses() {
    let keys = test_keys();
    let owner = UserId::new(Uuid::from_u128(0x0E1E));
    let later = jsonwebtoken::get_current_timestamp() + 3_600;
    let claims = claims_with(
        OWNER_AUD,
        later,
        serde_json::json!({"application": "original"}),
    );
    let authenticate = |reshape: Reshape| {
        let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
            agent: UserId::new(Uuid::from_u128(0xA9E1)),
            owner,
            agent_group: proxiima_core::GroupId::new(Uuid::now_v7()),
            owner_group: proxiima_core::GroupId::new(Uuid::now_v7()),
        });
        let bindings = OidcBindingSet::new([OidcBinding::with_role_shape(
            config(OWNER_AUD),
            resolver(keys.decoding.clone()),
            subject_map("owner-sub", owner),
            owner_access,
            OidcRoleShape::Host(Arc::new(reshape)),
        )
        .expect("binding")])
        .expect("binding set");
        let token = signed(&keys, &claims);
        async move { bindings.authenticate(&Credentials::Bearer(token)).await }
    };

    assert_eq!(
        authenticate(Reshape::Widen).await.err(),
        Some(AuthError::InvalidCredentials),
        "a Group the resolved roles lack"
    );
    assert_eq!(
        authenticate(Reshape::OtherSubject).await.err(),
        Some(AuthError::InvalidCredentials),
        "another subject"
    );
    let clamped = authenticate(Reshape::DropExpiry)
        .await
        .expect("dropping the expiry narrows nothing");
    assert_eq!(
        clamped.expires_at(),
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(later)),
        "the stream cannot outlive the token"
    );
    for shape in [Reshape::Rebuild, Reshape::ReplaceClaims] {
        let rebuilt = authenticate(shape)
            .await
            .expect("a context with fewer owner roles is an allowed shape");
        assert_eq!(rebuilt.verified_claims(), claims.as_object());
    }
}

/// Counts the key lookups a binding's resolver serves.
struct CountingKeys {
    inner: Arc<dyn KeyResolver>,
    lookups: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl KeyResolver for CountingKeys {
    async fn key_for(&self, kid: &str) -> Result<Arc<DecodingKey>, crate::KeyError> {
        self.lookups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.key_for(kid).await
    }
}

const OTHER_ISSUER: &str = "https://other-issuer.example";

/// One binding per `(issuer, audience)`, each with its own lookup counter,
/// all trusting `keys`.
fn counted_set(
    keys: &TestKeys,
    routes: &[(&str, &str)],
) -> (OidcBindingSet, Vec<Arc<std::sync::atomic::AtomicUsize>>) {
    let subject = UserId::new(Uuid::from_u128(0x0E1E));
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        agent: UserId::new(Uuid::from_u128(0xA9E1)),
        owner: subject,
        agent_group: proxiima_core::GroupId::new(Uuid::now_v7()),
        owner_group: proxiima_core::GroupId::new(Uuid::now_v7()),
    });
    let mut counters = Vec::new();
    let mut bindings = Vec::new();
    for (issuer, audience) in routes {
        let lookups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        counters.push(Arc::clone(&lookups));
        let mut map = OidcSubjectMap::new();
        map.insert(*issuer, "owner-sub", subject)
            .expect("subject map");
        let mut config = config(audience);
        config.issuer = (*issuer).to_owned();
        bindings.push(
            OidcBinding::new(
                config,
                Arc::new(CountingKeys {
                    inner: resolver(keys.decoding.clone()),
                    lookups,
                }),
                map,
                Arc::clone(&owner_access),
            )
            .expect("binding"),
        );
    }
    (
        OidcBindingSet::new(bindings).expect("binding set"),
        counters,
    )
}

fn lookups(counters: &[Arc<std::sync::atomic::AtomicUsize>]) -> Vec<usize> {
    counters
        .iter()
        .map(|counter| counter.load(std::sync::atomic::Ordering::SeqCst))
        .collect()
}

fn claims_of(issuer: &str, audience: &str) -> serde_json::Value {
    claims_with(
        audience,
        jsonwebtoken::get_current_timestamp() + 3_600,
        serde_json::json!({ "iss": issuer }),
    )
}

/// The unverified `iss` only chooses which bindings look up keys: a token
/// naming an issuer no binding serves is refused before any lookup.
#[tokio::test]
async fn a_token_for_an_unserved_issuer_is_refused_without_a_key_lookup() {
    let keys = test_keys();
    let (bindings, counters) = counted_set(
        &keys,
        &[
            (ISSUER, AGENT_AUD),
            (ISSUER, OWNER_AUD),
            (OTHER_ISSUER, OWNER_AUD),
        ],
    );
    let token = signed(&keys, &claims_of("https://third-issuer.example", OWNER_AUD));

    assert_eq!(
        bindings
            .authenticate(&Credentials::Bearer(token))
            .await
            .err(),
        Some(AuthError::InvalidCredentials)
    );
    assert_eq!(lookups(&counters), [0, 0, 0]);
}

#[tokio::test]
async fn only_the_bindings_of_the_claimed_issuer_look_up_keys() {
    let keys = test_keys();
    let (bindings, counters) = counted_set(
        &keys,
        &[
            (ISSUER, AGENT_AUD),
            (ISSUER, OWNER_AUD),
            (OTHER_ISSUER, OWNER_AUD),
        ],
    );

    let mut claims = claims_of(ISSUER, OWNER_AUD);
    claims["application"] = serde_json::json!({"label": "first"});
    let ctx = bindings
        .authenticate(&Credentials::Bearer(signed(&keys, &claims)))
        .await
        .expect("the owner binding of ISSUER accepts");
    assert_eq!(ctx.auth_path(), AuthPath::HostBearer);
    assert_eq!(ctx.verified_claims(), claims.as_object());
    let owner = OwnerId::from(ctx.subject().expect("mapped subject"));
    let narrowed = ctx.narrowed_to_owner(owner).expect("own personal owner");
    assert_eq!(narrowed.verified_claims(), claims.as_object());
    assert_eq!(lookups(&counters), [1, 1, 0]);

    bindings
        .authenticate(&Credentials::Bearer(signed(
            &keys,
            &claims_of(OTHER_ISSUER, OWNER_AUD),
        )))
        .await
        .expect("the binding of the other issuer accepts");
    assert_eq!(lookups(&counters), [1, 1, 1]);
    assert_eq!(narrowed.verified_claims(), claims.as_object());
}

/// A payload that cannot be read selects nothing: every binding runs, as
/// before the selection existed, and the verified `iss` still decides.
#[tokio::test]
async fn an_unreadable_issuer_is_tried_against_every_binding() {
    let keys = test_keys();
    let (bindings, counters) = counted_set(
        &keys,
        &[
            (ISSUER, AGENT_AUD),
            (ISSUER, OWNER_AUD),
            (OTHER_ISSUER, OWNER_AUD),
        ],
    );
    let header = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({"alg": "RS256", "kid": KID, "typ": "JWT"}))
            .expect("header"),
    );
    let unreadable = [
        "!!not base64url!!".to_owned(),
        URL_SAFE_NO_PAD.encode("not json"),
        URL_SAFE_NO_PAD.encode(r#"{"sub":"owner-sub"}"#),
        URL_SAFE_NO_PAD.encode(r#"{"iss":7}"#),
        URL_SAFE_NO_PAD.encode(r#"{"iss":null}"#),
        URL_SAFE_NO_PAD.encode(format!(r#"{{"iss":["{ISSUER}","{OTHER_ISSUER}"]}}"#)),
        URL_SAFE_NO_PAD.encode(format!(r#"{{"iss":"{OTHER_ISSUER}","iss":"{ISSUER}"}}"#)),
    ];
    for (round, payload) in unreadable.iter().enumerate() {
        let token = format!("{header}.{payload}.{}", URL_SAFE_NO_PAD.encode("signature"));
        assert_eq!(
            bindings
                .authenticate(&Credentials::Bearer(token))
                .await
                .err(),
            Some(AuthError::InvalidCredentials),
            "{payload}"
        );
        assert_eq!(lookups(&counters), [round + 1; 3], "{payload}");
    }
}

/// A signed token with a non-string `iss` that names the binding's issuer
/// is accepted exactly as it was: the selection never narrows it away.
#[tokio::test]
async fn a_signed_issuer_array_is_still_accepted_by_the_binding_it_names() {
    let keys = test_keys();
    let (bindings, counters) =
        counted_set(&keys, &[(ISSUER, OWNER_AUD), (OTHER_ISSUER, AGENT_AUD)]);
    let mut claims = claims_of(ISSUER, OWNER_AUD);
    claims["iss"] = serde_json::json!([ISSUER, "https://unrelated.example"]);

    bindings
        .authenticate(&Credentials::Bearer(signed(&keys, &claims)))
        .await
        .expect("the binding for ISSUER accepts an iss array that names it");
    assert_eq!(lookups(&counters), [1, 1]);
}
