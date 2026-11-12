use crate::MemoryId;
use crate::ToolCtx;
use crate::ToolError;
use crate::verbs::goal_write::IdempotencyKey;

/// Parse the authoring destination once for source resolution and admission.
pub(super) fn resolve_destination(
    ctx: &ToolCtx,
    space: Option<&str>,
    working_goal: Option<&str>,
) -> Result<crate::MemoryDestination, ToolError> {
    match (space, working_goal) {
        (Some(_), Some(_)) => Err(ToolError::InvalidInput(
            "space and working_goal are mutually exclusive".into(),
        )),
        (None, Some(goal)) => Ok(crate::MemoryDestination::WorkingGoal(
            crate::GoalHandle::new(crate::catalog::ids::parse_wire_uuid(goal)?),
        )),
        (space, None) => Ok(crate::MemoryDestination::Owner(
            super::super::memory_spaces::resolve_space_owner(
                ctx,
                space,
                super::super::memory_spaces::SpaceDefault::Current,
            )?
            .owner,
        )),
    }
}

/// Resolve a caller's handle list to memories in request order with repeats
/// dropped: a handle named twice is one input, and an empty result is not a
/// declaration.
///
/// `resolve` is the surface's own handle resolver — `core_derive` reads the
/// public handle vocabulary, `core_episode_commit` also reads the slots
/// written earlier in the same transaction — and `T` is whatever that
/// resolver classifies each handle as.
///
/// `field`, `max` and `empty_message` carry the calling surface's own error
/// strings. The `max` bound is re-checked here so the helper is safe on its
/// own, but every caller checks it first, against the raw argument and before
/// any space or handle resolution, which is where each surface's error
/// precedence is pinned.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `handles` is over `max` or
/// resolves to nothing, and whatever `resolve` returns for a bad handle.
pub async fn dedup_resolved<'a, T: Send>(
    handles: &'a [String],
    max: usize,
    field: &'static str,
    empty_message: &'static str,
    mut resolve: impl FnMut(&'a str) -> futures::future::BoxFuture<'a, Result<(MemoryId, T), ToolError>>
    + Send,
) -> Result<Vec<(MemoryId, T)>, ToolError> {
    if handles.len() > max {
        return Err(ToolError::InvalidInput(format!(
            "{field} must contain at most {max} handles"
        )));
    }
    let mut seen = std::collections::HashSet::with_capacity(handles.len());
    let mut resolved = Vec::with_capacity(handles.len());
    for handle in handles {
        let (memory_id, class) = resolve(handle).await?;
        if seen.insert(memory_id.into_inner()) {
            resolved.push((memory_id, class));
        }
    }
    if resolved.is_empty() {
        return Err(ToolError::InvalidInput(empty_message.into()));
    }
    Ok(resolved)
}

/// Normalize an explicitly-provided idempotency key to the shared
/// write-surface contract — trimmed, 1..=180 chars — by parsing it
/// through the same [`IdempotencyKey`] type the goal tools use, so the
/// memory and goal families cannot drift on cap, whitespace handling,
/// or error text. The trimmed key is what feeds dedup (`uuid::new_v5`)
/// and the stored payload. An omitted key (`None`) is always allowed —
/// the caller derives one instead.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when `key` is `Some` and blank
/// after trimming or over the character cap.
pub fn normalize_idempotency_key(key: Option<String>) -> Result<Option<String>, ToolError> {
    key.map(|raw| {
        IdempotencyKey::new(raw)
            .map(IdempotencyKey::into_string)
            .map_err(ToolError::InvalidInput)
    })
    .transpose()
}

/// Parse an optional caller-supplied `observed_at` backdate (RFC3339).
/// Historical import writes the original observation time as the Fact's
/// `occurred_at`, which every read returns. It does not alter `created_at`,
/// the write time that orders supersession heads and recency and has no
/// write path.
///
/// Parse only: the engine refuses an occurrence more than
/// [`crate::verbs::fact_ingest::OCCURRED_AT_FUTURE_BOUND`] in the future on
/// every Fact write path, this one included.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when the value is not RFC3339.
pub fn parse_observed_at(raw: Option<&str>) -> Result<Option<time::OffsetDateTime>, ToolError> {
    raw.map(|raw| {
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).map_err(
            |err| {
                ToolError::InvalidInput(format!("observed_at must be an RFC3339 timestamp: {err}"))
            },
        )
    })
    .transpose()
}

/// Upper bound on distinct normalized tags per memory.
pub(super) const MAX_TAGS: usize = 16;

/// Upper bound on one normalized tag, in characters.
pub(super) const MAX_TAG_CHARS: usize = 48;

/// Fold one tag to the form it is stored and compared in: trimmed and
/// ASCII-lowercased.
///
/// The write side and the search filter must fold identically or a tag
/// written as `Rust` cannot be found by searching `Rust` — a silent miss,
/// since a filter that matches nothing is indistinguishable from a memory
/// that does not exist. Both call this rather than repeating the two steps.
#[must_use]
pub fn fold_tag(tag: &str) -> String {
    tag.trim().to_ascii_lowercase()
}

/// Trim, lowercase, sort, and dedup `tags`, then cap the *distinct* result
/// at [`MAX_TAGS`]. The cap deliberately applies after normalization: a
/// caller sending `["Rust", "rust", " RUST "]` holds one tag, not three,
/// and must not be rejected for a duplicate-heavy spelling of an
/// in-contract set.
///
/// # Errors
///
/// Returns [`ToolError::InvalidInput`] when a tag is blank, a tag
/// exceeds [`MAX_TAG_CHARS`], or more than [`MAX_TAGS`] distinct tags
/// remain after normalization. Blank and oversized get different
/// messages — see [`crate::catalog::validate_trimmed_len`].
pub fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>, ToolError> {
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = fold_tag(&tag);
        out.push(crate::catalog::validate_trimmed_len("tag", &tag, MAX_TAG_CHARS)?.to_string());
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_TAGS {
        return Err(ToolError::InvalidInput("at most 16 distinct tags".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::super::memory_spaces::test_ctx::ctx_for;
    use super::{
        MAX_TAGS, ToolError, dedup_resolved, normalize_idempotency_key, normalize_tags,
        parse_observed_at,
    };
    use crate::catalog::MAX_OPERATOR_LABEL_CHARS;
    use crate::verbs::goal_write::IdempotencyKey;
    use crate::{MemoryId, ToolCtx, UserId};
    use uuid::Uuid;

    fn test_ctx() -> ToolCtx {
        ctx_for(
            UserId::new(Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("uuid")),
            Vec::new(),
        )
    }

    #[test]
    fn authoring_destination_refuses_both_space_and_working_goal() {
        assert!(matches!(
            super::resolve_destination(&test_ctx(), Some("personal"), Some("goal")),
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[test]
    fn an_oversized_operator_label_is_rejected() {
        let ctx = test_ctx();
        assert!(
            ctx.operator_label(Some(&"m".repeat(MAX_OPERATOR_LABEL_CHARS)))
                .is_ok()
        );
        assert!(
            ctx.operator_label(Some(&"m".repeat(MAX_OPERATOR_LABEL_CHARS + 1)))
                .is_err()
        );
    }

    /// A context whose token binds a runner identity.
    ///
    /// The binding lives on `authz`: that is the carrier the authenticator
    /// hardened, and the only one `operator_label` consults.
    fn trusted_ctx() -> ToolCtx {
        let ctx = test_ctx();
        let authz = ctx
            .authz()
            .clone()
            .with_trusted_model_id("acme/runner-v3")
            .expect("a well-formed runner id binds");
        ctx.with_authz(authz)
    }

    /// The enforcement point. Transport edges only ever see a *top-level*
    /// `model_id`; a per-item one (`core_episode_commit`'s
    /// `derive.model_id` / `stance[].model_id`) and every argument on the
    /// embedded host API arrive here untouched, so this is the check that
    /// actually covers them.
    #[test]
    fn an_explicit_label_differing_from_the_bound_identity_is_refused() {
        let err = trusted_ctx()
            .operator_label(Some("openai/gpt-9"))
            .expect_err("a caller may not relabel an authenticated runner");

        let ToolError::InvalidInput(message) = err else {
            panic!("a bad reserved argument is invalid input");
        };
        assert!(message.contains("model_id"), "{message}");
        assert!(message.contains("openai/gpt-9"), "{message}");
        assert!(message.contains("acme/runner-v3"), "{message}");
        assert!(
            message.contains("authenticated token already binds"),
            "{message}"
        );
    }

    #[test]
    fn an_agreeing_or_absent_explicit_label_records_the_bound_identity() {
        let ctx = trusted_ctx();
        for explicit in [None, Some("acme/runner-v3"), Some("  acme/runner-v3  ")] {
            assert_eq!(
                ctx.operator_label(explicit)
                    .expect("agreeing or absent is accepted"),
                "acme/runner-v3",
                "explicit {explicit:?}"
            );
        }
    }

    /// The bound identity is recorded even if the caller a host assembled
    /// by hand names another model, so a host cannot launder a label past
    /// the binding: the context replaces the caller's label with the bound
    /// identity when it is built.
    #[test]
    fn the_bound_identity_outranks_a_mismatched_caller() {
        let trusted = trusted_ctx();
        let ctx = crate::test_fixtures::tool_ctx_with(
            trusted.owner(),
            trusted.authz().clone(),
            trusted.registry().clone(),
            crate::SpiiceServices::default(),
            crate::ToolCaller::new("stale/label", "test", "0"),
        );

        assert_eq!(ctx.caller().model_id, "acme/runner-v3");
        assert_eq!(
            ctx.operator_label(None).expect("bound identity wins"),
            "acme/runner-v3"
        );
        assert!(
            ctx.operator_label(Some("openai/gpt-9")).is_err(),
            "the binding still refuses a differing claim"
        );
    }

    fn memory(byte: u8) -> MemoryId {
        MemoryId::new(Uuid::from_bytes([byte; 16]))
    }

    fn resolve_by_last_char(handle: &str) -> Result<(MemoryId, char), ToolError> {
        let last = handle
            .chars()
            .next_back()
            .ok_or_else(|| ToolError::InvalidInput("blank handle".into()))?;
        Ok((memory(last as u8), last))
    }

    #[tokio::test]
    async fn dedup_resolved_rejects_an_empty_result_with_the_callers_message() {
        let ToolError::InvalidInput(message) = dedup_resolved(
            &[],
            8,
            "source_handles",
            "source_handles must be nonempty for operator derivation",
            |handle| Box::pin(std::future::ready(resolve_by_last_char(handle))),
        )
        .await
        .expect_err("no declared inputs") else {
            panic!("an empty declaration must be invalid input");
        };
        assert_eq!(
            message,
            "source_handles must be nonempty for operator derivation"
        );
    }

    #[tokio::test]
    async fn dedup_resolved_rejects_more_handles_than_the_cap() {
        let handles: Vec<String> = (0..3).map(|i| format!("mem-{i}")).collect();
        let ToolError::InvalidInput(message) =
            dedup_resolved(&handles, 2, "subjects", "nonempty", |handle| {
                Box::pin(std::future::ready(resolve_by_last_char(handle)))
            })
            .await
            .expect_err("over the cap")
        else {
            panic!("an over-cap declaration must be invalid input");
        };
        assert_eq!(message, "subjects must contain at most 2 handles");
    }

    /// A handle the surface cannot resolve fails the whole declaration:
    /// silently dropping it would write a derivation over fewer inputs
    /// than the caller declared.
    #[tokio::test]
    async fn dedup_resolved_propagates_a_resolver_error() {
        let handles = vec!["ok".to_string(), String::new()];
        assert!(
            dedup_resolved(&handles, 8, "subjects", "nonempty", |handle| Box::pin(
                std::future::ready(resolve_by_last_char(handle))
            ))
            .await
            .is_err()
        );
    }

    #[test]
    fn non_rfc3339_observed_at_is_rejected() {
        assert!(parse_observed_at(Some("22.03.2023")).is_err());
        assert!(parse_observed_at(Some("2023-03-22")).is_err());
        assert!(parse_observed_at(Some("")).is_err());
    }

    const MAX_IDEMPOTENCY_KEY_CHARS: usize = IdempotencyKey::MAX_CHARS;

    #[test]
    fn blank_and_oversized_tags_are_rejected() {
        assert!(normalize_tags(vec!["  ".into()]).is_err());
        assert!(normalize_tags(vec!["x".repeat(49)]).is_err());
        assert!(normalize_tags(vec!["x".repeat(48)]).is_ok());
    }

    /// A blank tag and an over-length one are different mistakes, and the
    /// old shared `tag must be 1..=48 chars` quoted the blank one a range
    /// it satisfies.
    #[test]
    fn a_blank_tag_and_an_oversized_tag_are_told_apart() {
        let ToolError::InvalidInput(blank) =
            normalize_tags(vec!["  ".into()]).expect_err("blank tag")
        else {
            panic!("a bad tag must be invalid input");
        };
        let ToolError::InvalidInput(over) =
            normalize_tags(vec!["x".repeat(49)]).expect_err("oversized tag")
        else {
            panic!("a bad tag must be invalid input");
        };
        assert_ne!(blank, over, "one message for two mistakes tells neither");
        assert!(
            !blank.contains("48"),
            "blank quoted a bound it meets: {blank}"
        );
        assert!(
            over.contains("49"),
            "oversized must say what it sent: {over}"
        );
    }

    #[test]
    fn too_many_distinct_tags_are_rejected() {
        let at_cap: Vec<String> = (0..MAX_TAGS).map(|i| format!("tag-{i:02}")).collect();
        assert_eq!(normalize_tags(at_cap).expect("at the cap").len(), MAX_TAGS);
        let over_cap: Vec<String> = (0..=MAX_TAGS).map(|i| format!("tag-{i:02}")).collect();
        assert!(normalize_tags(over_cap).is_err());
    }

    #[test]
    fn blank_idempotency_key_is_rejected() {
        // An empty or whitespace-only string must not slip through as a
        // real dedup key.
        assert!(normalize_idempotency_key(Some(String::new())).is_err());
        assert!(normalize_idempotency_key(Some("   ".into())).is_err());
    }

    #[test]
    fn idempotency_key_over_the_cap_is_rejected() {
        let key = "k".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1);
        assert!(normalize_idempotency_key(Some(key)).is_err());
    }

    #[test]
    fn idempotency_key_cap_counts_characters_not_bytes() {
        // Multi-byte chars: MAX of them is fine, MAX+1 is not, even though
        // the byte length is well over the character cap in both cases.
        let at_cap = "é".repeat(MAX_IDEMPOTENCY_KEY_CHARS);
        let over_cap = "é".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1);
        assert!(normalize_idempotency_key(Some(at_cap)).is_ok());
        assert!(normalize_idempotency_key(Some(over_cap)).is_err());
    }
}
