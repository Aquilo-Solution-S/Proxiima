use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::DecodingKey;
use proxiima_core::{AccessError, AuthPath, GroupId, OwnerId, OwnerRoles, Role, UserId};
use serde::Serialize;
use uuid::Uuid;

use super::*;
use crate::keys::StaticJwksResolver;

const ISSUER: &str = "https://issuer.example";
const AUDIENCE: &str = "proxiima-api";
const KID: &str = "k1";

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

fn test_keys() -> TestKeys {
    let signing = RsaKeyPair::generate(KeySize::Rsa2048).expect("generate test RSA key");
    let decoding = DecodingKey::from_rsa_der(signing.public_key().as_ref());
    TestKeys { signing, decoding }
}

fn token(keys: &TestKeys, kid: &str, issuer: &str, audience: &str, sub: &str, exp: u64) -> String {
    token_with_typ(keys, kid, issuer, audience, sub, exp, Some("JWT"))
}

fn token_with_typ(
    keys: &TestKeys,
    kid: &str,
    issuer: &str,
    audience: &str,
    sub: &str,
    exp: u64,
    typ: Option<&str>,
) -> String {
    let mut header = serde_json::json!({"alg": "RS256", "kid": kid});
    if let Some(typ) = typ {
        header["typ"] = serde_json::Value::String(typ.to_owned());
    }
    let claims = TestClaims {
        sub: sub.to_owned(),
        iss: issuer.to_owned(),
        aud: audience.to_owned(),
        exp,
    };
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

fn future_exp() -> u64 {
    jsonwebtoken::get_current_timestamp() + 3_600
}

/// Signs a token whose `nbf` is in the future (with a valid `exp`) so it is
/// not yet valid. Built directly (not via `token`) because [`TestClaims`]
/// carries no `nbf` field.
fn future_nbf_token(keys: &TestKeys) -> String {
    let now = jsonwebtoken::get_current_timestamp();
    let header = serde_json::json!({"alg": "RS256", "kid": KID, "typ": "JWT"});
    let claims = serde_json::json!({
        "sub": "subject-1",
        "iss": ISSUER,
        "aud": AUDIENCE,
        "exp": now + 3_600,
        "nbf": now + 1_800,
    });
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

fn config() -> OidcAuthConfig {
    OidcAuthConfig {
        issuer: ISSUER.to_owned(),
        jwks_uri: None,
        audience: AUDIENCE.to_owned(),
        allowed_subjects: None,
        leeway_secs: 0,
    }
}

fn resolver(kid: &str, decoding: DecodingKey) -> Arc<dyn KeyResolver> {
    Arc::new(StaticJwksResolver::new(HashMap::from([(
        kid.to_owned(),
        Arc::new(decoding),
    )])))
}

fn mapped_authenticator(
    config: OidcAuthConfig,
    kid: &str,
    decoding: DecodingKey,
    token_subject: &str,
    subject: UserId,
    roles: Vec<(OwnerId, Role)>,
) -> OidcAuthenticator {
    let mut map = OidcSubjectMap::new();
    map.insert(ISSUER, token_subject, subject).expect("insert");
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess { subject, roles });
    OidcAuthenticator::new(config, resolver(kid, decoding), map, owner_access)
        .expect("valid oidc config")
}

/// In-memory `OwnerAccessPort` stub for tests: returns fixed roles for
/// one subject, `AccessError::Resolution` for anyone else.
struct StaticOwnerAccess {
    subject: UserId,
    roles: Vec<(OwnerId, Role)>,
}

#[async_trait]
impl OwnerAccessPort for StaticOwnerAccess {
    async fn resolve_roles_for_subject(&self, subject: UserId) -> Result<OwnerRoles, AccessError> {
        if subject != self.subject {
            return Err(AccessError::Resolution("unknown subject".into()));
        }
        OwnerRoles::for_subject(subject, self.roles.clone())
    }
}

#[tokio::test]
async fn accepted_token_uses_issuer_subject_map_and_owner_roles() {
    let keys = test_keys();
    let subject = UserId::new(Uuid::now_v7());
    let group = GroupId::new(Uuid::now_v7());
    let mut map = OidcSubjectMap::new();
    map.insert(ISSUER, "subject-1", subject).expect("insert");
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        subject,
        roles: vec![(OwnerId::from(group), Role::admin())],
    });
    let auth = OidcAuthenticator::new(
        config(),
        resolver(KID, keys.decoding.clone()),
        map,
        owner_access,
    )
    .expect("valid oidc config");
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp());

    let ctx = auth
        .authenticate(&Credentials::Bearer(token))
        .await
        .expect("authenticate valid token");

    assert_eq!(ctx.auth_path(), AuthPath::HostBearer);
    assert_eq!(ctx.subject(), Some(subject));
    assert!(ctx.may_manage(&OwnerId::from(group)));
    assert!(ctx.expires_at().is_some());
}

#[tokio::test]
async fn valid_token_with_unknown_issuer_subject_is_denied() {
    let keys = test_keys();
    let subject = UserId::new(Uuid::now_v7());
    let map = OidcSubjectMap::new(); // deliberately empty
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        subject,
        roles: Vec::new(),
    });
    let auth = OidcAuthenticator::new(
        config(),
        resolver(KID, keys.decoding.clone()),
        map,
        owner_access,
    )
    .expect("valid oidc config");
    let token = token(
        &keys,
        KID,
        ISSUER,
        AUDIENCE,
        "unmapped-subject",
        future_exp(),
    );

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn validation_only_api_returns_claims_without_authz_context() {
    let keys = test_keys();
    let validator = OidcTokenValidator::new(config(), resolver(KID, keys.decoding.clone()))
        .expect("valid oidc config");
    let exp = future_exp();
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", exp);

    let claims = validator.validate(&token).await.expect("validates");

    assert_eq!(claims.issuer, ISSUER);
    assert_eq!(claims.audience, AUDIENCE);
    assert_eq!(claims.subject, "subject-1");
    assert_eq!(claims.expires_at, UNIX_EPOCH + Duration::from_secs(exp));
}

#[tokio::test]
async fn access_token_typ_requirement_accepts_rfc9068_values_and_rejects_id_tokens() {
    let keys = test_keys();
    let validator = OidcTokenValidator::new(config(), resolver(KID, keys.decoding.clone()))
        .expect("valid oidc config")
        .require_access_token_typ();
    for typ in [
        "at+jwt",
        "AT+JWT",
        "application/at+jwt",
        "APPLICATION/AT+JWT",
    ] {
        let token = token_with_typ(
            &keys,
            KID,
            ISSUER,
            AUDIENCE,
            "subject-1",
            future_exp(),
            Some(typ),
        );
        validator
            .validate(&token)
            .await
            .expect("valid access token typ");
    }
    for typ in [Some("JWT"), None] {
        let token = token_with_typ(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp(), typ);
        assert_eq!(
            validator.validate_with::<serde_json::Value>(&token).await,
            Err(OidcRejection::WrongTokenType)
        );
    }
}

#[tokio::test]
async fn access_token_typ_requirement_is_opt_in() {
    let keys = test_keys();
    let validator = OidcTokenValidator::new(config(), resolver(KID, keys.decoding.clone()))
        .expect("valid oidc config");
    for typ in [
        None,
        Some("JWT"),
        Some("jwt"),
        Some("at+jwt"),
        Some("application/at+jwt"),
    ] {
        let token = token_with_typ(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp(), typ);
        validator
            .validate(&token)
            .await
            .expect("default remains compatible");
    }
}

/// A header that names another kind of token is refused whether or not
/// the access-token type is required.
#[tokio::test]
async fn a_typ_naming_another_kind_of_token_is_refused() {
    let keys = test_keys();
    let lenient = OidcTokenValidator::new(config(), resolver(KID, keys.decoding.clone()))
        .expect("valid oidc config");
    let strict = OidcTokenValidator::new(config(), resolver(KID, keys.decoding.clone()))
        .expect("valid oidc config")
        .require_access_token_typ();
    for typ in [
        "id_token+jwt",
        "logout+jwt",
        "dpop+jwt",
        "application/jwt",
        "at+jwt ",
        "",
    ] {
        let token = token_with_typ(
            &keys,
            KID,
            ISSUER,
            AUDIENCE,
            "subject-1",
            future_exp(),
            Some(typ),
        );
        for validator in [&lenient, &strict] {
            assert_eq!(
                validator.validate_with::<serde_json::Value>(&token).await,
                Err(OidcRejection::WrongTokenType),
                "typ {typ:?}"
            );
        }
    }
}

#[tokio::test]
async fn rejects_expiration_outside_system_time_range() {
    let keys = test_keys();
    let validator = OidcTokenValidator::new(config(), resolver(KID, keys.decoding.clone()))
        .expect("valid oidc config");
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", u64::MAX);

    assert_eq!(
        validator.validate(&token).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn mapped_subject_authenticates_with_personal_owner_access() {
    let keys = test_keys();
    let subject = UserId::new(Uuid::now_v7());
    let owner = OwnerId::from(subject);
    let auth = mapped_authenticator(
        config(),
        KID,
        keys.decoding.clone(),
        "subject-1",
        subject,
        Vec::new(),
    );
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp());

    let ctx = auth
        .authenticate(&Credentials::Bearer(token))
        .await
        .expect("authenticate valid token");

    assert_eq!(ctx.auth_path(), AuthPath::HostBearer);
    assert_eq!(ctx.subject(), Some(subject));
    assert_eq!(ctx.principal(), owner);
    assert!(ctx.can_access_owner(&owner));
    assert!(ctx.expires_at().is_some());
}

/// The subject map is the only source of trusted model provenance on
/// this path, and it reaches the caller's `AuthzContext` intact.
#[tokio::test]
async fn a_mapped_runner_principal_carries_its_trusted_model_id() {
    let keys = test_keys();
    let subject = UserId::new(Uuid::now_v7());
    let mut map = OidcSubjectMap::new();
    map.insert_binding(
        ISSUER,
        "runner-sub",
        crate::SubjectBinding::new(subject).with_trusted_model_id("acme/runner-v3"),
    )
    .expect("insert");
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        subject,
        roles: Vec::new(),
    });
    let auth = OidcAuthenticator::new(
        config(),
        resolver(KID, keys.decoding.clone()),
        map,
        owner_access,
    )
    .expect("valid oidc config");
    let token = token(&keys, KID, ISSUER, AUDIENCE, "runner-sub", future_exp());

    let ctx = auth
        .authenticate(&Credentials::Bearer(token))
        .await
        .expect("authenticate valid token");

    assert_eq!(ctx.subject(), Some(subject));
    assert_eq!(ctx.trusted_model_id(), Some("acme/runner-v3"));
}

/// A mapped principal the deployment did not declare a runner for stays
/// exactly as it was: authenticated, with no model provenance.
#[tokio::test]
async fn a_mapped_principal_without_the_field_carries_no_trusted_model_id() {
    let keys = test_keys();
    let subject = UserId::new(Uuid::now_v7());
    let auth = mapped_authenticator(
        config(),
        KID,
        keys.decoding.clone(),
        "subject-1",
        subject,
        Vec::new(),
    );
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp());

    let ctx = auth
        .authenticate(&Credentials::Bearer(token))
        .await
        .expect("authenticate valid token");

    assert_eq!(ctx.trusted_model_id(), None);
}

#[tokio::test]
async fn rejects_wrong_audience() {
    let keys = test_keys();
    let auth = mapped_authenticator(
        config(),
        KID,
        keys.decoding.clone(),
        "subject-1",
        UserId::new(Uuid::now_v7()),
        Vec::new(),
    );
    let token = token(
        &keys,
        KID,
        ISSUER,
        "wrong-audience",
        "subject-1",
        future_exp(),
    );

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn rejects_wrong_issuer() {
    let keys = test_keys();
    let auth = mapped_authenticator(
        config(),
        KID,
        keys.decoding.clone(),
        "subject-1",
        UserId::new(Uuid::now_v7()),
        Vec::new(),
    );
    let token = token(
        &keys,
        KID,
        "https://wrong-issuer.example",
        AUDIENCE,
        "subject-1",
        future_exp(),
    );

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn rejects_expired_token() {
    let keys = test_keys();
    let auth = mapped_authenticator(
        config(),
        KID,
        keys.decoding.clone(),
        "subject-1",
        UserId::new(Uuid::now_v7()),
        Vec::new(),
    );
    let token = token(
        &keys,
        KID,
        ISSUER,
        AUDIENCE,
        "subject-1",
        jsonwebtoken::get_current_timestamp() - 3_600,
    );

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn rejects_token_before_nbf() {
    let keys = test_keys();
    let auth = mapped_authenticator(
        config(),
        KID,
        keys.decoding.clone(),
        "subject-1",
        UserId::new(Uuid::now_v7()),
        Vec::new(),
    );
    // Valid signature/iss/aud/exp, but `nbf` is 30 minutes in the future
    // and leeway is 0 — must be rejected as not-yet-valid.
    let token = future_nbf_token(&keys);

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn rejects_subject_not_in_allowlist() {
    let keys = test_keys();
    let mut config = config();
    config.allowed_subjects = Some(HashSet::from(["allowed-subject".to_owned()]));
    let auth = mapped_authenticator(
        config,
        KID,
        keys.decoding.clone(),
        "subject-1",
        UserId::new(Uuid::now_v7()),
        Vec::new(),
    );
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp());

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}

#[tokio::test]
async fn rejects_unknown_kid() {
    let keys = test_keys();
    let subject = UserId::new(Uuid::now_v7());
    let mut map = OidcSubjectMap::new();
    map.insert(ISSUER, "subject-1", subject).expect("insert");
    let owner_access: Arc<dyn OwnerAccessPort> = Arc::new(StaticOwnerAccess {
        subject,
        roles: Vec::new(),
    });
    let auth = OidcAuthenticator::new(
        config(),
        Arc::new(StaticJwksResolver::new(HashMap::new())),
        map,
        owner_access,
    );
    let auth = auth.expect("valid oidc config");
    let token = token(&keys, KID, ISSUER, AUDIENCE, "subject-1", future_exp());

    assert_eq!(
        auth.authenticate(&Credentials::Bearer(token)).await,
        Err(AuthError::InvalidCredentials)
    );
}
