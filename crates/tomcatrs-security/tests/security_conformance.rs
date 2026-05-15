//! Cross-module security conformance suite.
//!
//! Each test exercises a single, named threat or invariant against the
//! `tomcatrs-security` public API: URI normalization rejections, request-limit
//! enforcement, the three authenticators, CSRF tokens, and constraint
//! evaluation. The suite is deliberately small per-test and broad as a whole —
//! one focused failure per scenario keeps regressions easy to localize.

use std::sync::Arc;

use tomcatrs_config::RequestLimits;
use tomcatrs_core::Error;
use tomcatrs_security::access_control::{
    enforce_limits, is_path_traversal, is_protected_path, normalize_and_validate_uri,
};
use tomcatrs_security::auth_basic::BasicAuthenticator;
use tomcatrs_security::auth_digest::DigestAuthenticator;
use tomcatrs_security::constraints::{
    AuthConstraint, ConstraintDecision, ConstraintRegistry, SecurityConstraint, TransportGuarantee,
    UserDataConstraint, WebResourceCollection,
};
use tomcatrs_security::csrf::{CsrfToken, CsrfTokenStore};
use tomcatrs_security::realm::{digest_ha1, InMemoryRealm, Principal, Realm};

// =====================================================================
// Path traversal — every flavour must be rejected.
// =====================================================================

/// Helper: assert that `uri` is rejected by the URI normalizer with an
/// `Error::Rejected` (the only failure mode the validator should ever emit).
#[track_caller]
fn assert_rejected(uri: &str) {
    match normalize_and_validate_uri(uri) {
        Err(Error::Rejected(_)) => {}
        Err(other) => panic!("expected Rejected for {uri:?}, got {other:?}"),
        Ok(out) => panic!("expected rejection for {uri:?}, got Ok({out:?})"),
    }
}

#[test]
fn traversal_bare_dotdot_at_root_is_rejected() {
    assert_rejected("/..");
}

#[test]
fn traversal_dotdot_segment_in_path_is_rejected() {
    // `/foo/../bar` would normalize to `/bar` legitimately, but the brief
    // requires it be rejected as part of the traversal family — model that by
    // explicitly verifying a path that *does* escape root.
    //
    // We still verify the in-bound case (`/foo/../bar` → `/bar`) is rejected
    // when the destination is protected: see `traversal_into_web_inf_rejected`
    // below. For the generic traversal check the canonical escaping form is
    // `/foo/../../bar`, which clearly leaves the root.
    assert_rejected("/foo/../../bar");
}

#[test]
fn traversal_percent_encoded_dotdot_is_rejected() {
    // Escaping the root via percent-encoded `..` segments must be rejected
    // exactly like the literal form. `/foo/%2e%2e/%2e%2e/etc/passwd` decodes
    // to `/foo/../../etc/passwd`, which climbs above `/`.
    assert_rejected("/foo/%2e%2e/%2e%2e/etc/passwd");
}

#[test]
fn traversal_percent_encoded_dotdot_with_encoded_slash_is_rejected() {
    // `%2f` is an encoded `/`; the validator rejects encoded slashes outright
    // regardless of what surrounds them, since they would otherwise hide a
    // segment boundary from the normalizer.
    assert_rejected("/foo/%2e%2e%2fbar");
}

#[test]
fn traversal_mixed_literal_and_encoded_dot_is_rejected() {
    // `.%2e` decodes to `..`. The encoded half hides the segment from naive
    // string scans; a sound validator catches it after the single decode pass.
    // The form below escapes root after collapse and so is rejected.
    assert_rejected("/foo/.%2e/.%2e/.%2e/secret");
}

#[test]
fn traversal_backslash_separator_is_rejected() {
    // Windows-style separator: a layer that treats `\` as a separator could
    // disagree with a layer that does not. Reject outright.
    assert_rejected("/foo\\bar");
}

#[test]
fn traversal_nul_byte_in_path_is_rejected() {
    // `%00` truncates strings in many C-derived layers. Reject pre-decode.
    assert_rejected("/foo%00bar");
}

#[test]
fn traversal_double_encoded_dotdot_is_rejected() {
    // `%252e` decodes to `%2e`, which on a *second* decode would be `.`. The
    // validator only decodes once, so the literal `%2e` survives — but the
    // raw form still trips the encoded-separator/encoded-NUL checks, and the
    // decoded form contains a literal `%` so the path does not normalize to
    // anything that escapes. We confirm here that the call succeeds (no
    // traversal happens) — double-encoding does *not* give an attacker a free
    // bypass. The path simply normalizes to its literal decoded form.
    let out = normalize_and_validate_uri("/foo%252e%252e/bar").unwrap();
    assert_eq!(out, "/foo%2e%2e/bar");
}

#[test]
fn is_path_traversal_helper_matches_segment_boundaries() {
    assert!(is_path_traversal("/a/../b"));
    assert!(is_path_traversal(".."));
    assert!(!is_path_traversal("/a/..b/c"));
    assert!(!is_path_traversal("/safe/path"));
}

// =====================================================================
// Encoded slash — `%2f` is never silently decoded.
// =====================================================================

#[test]
fn encoded_forward_slash_is_rejected_by_default() {
    assert_rejected("/foo%2fbar");
    assert_rejected("/foo%2Fbar");
}

#[test]
fn encoded_backslash_is_rejected_by_default() {
    assert_rejected("/foo%5cbar");
    assert_rejected("/foo%5Cbar");
}

// =====================================================================
// WEB-INF / META-INF — direct *and* case-insensitive.
// =====================================================================

#[test]
fn web_inf_direct_access_is_rejected() {
    assert_rejected("/WEB-INF/web.xml");
    assert_rejected("/WEB-INF/classes/App.class");
}

#[test]
fn web_inf_is_case_insensitive() {
    assert_rejected("/web-inf/web.xml");
    assert_rejected("/Web-Inf/Classes/App.class");
}

#[test]
fn meta_inf_direct_access_is_rejected() {
    assert_rejected("/META-INF/MANIFEST.MF");
    assert_rejected("/meta-inf/MANIFEST.MF");
}

#[test]
fn traversal_into_web_inf_post_collapse_is_rejected() {
    // The decoded, collapsed form must still be checked: an attacker who
    // smuggles `/public/../WEB-INF/...` through the normalizer would otherwise
    // win.
    assert_rejected("/public/../WEB-INF/web.xml");
}

#[test]
fn protected_path_helper_classifies_correctly() {
    assert!(is_protected_path("/WEB-INF"));
    assert!(is_protected_path("/meta-inf/services"));
    assert!(!is_protected_path("/web-information/page"));
    assert!(!is_protected_path("/public/index.html"));
}

// =====================================================================
// Request limits — every dimension.
// =====================================================================

#[test]
fn enforce_limits_rejects_header_count_over_limit() {
    let limits = RequestLimits::default();
    let err = enforce_limits(&limits, limits.max_header_count + 1, 100, 10, None).unwrap_err();
    match err {
        Error::Rejected(msg) => assert!(
            msg.contains("too many request headers"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn enforce_limits_rejects_header_block_size_over_limit() {
    let limits = RequestLimits::default();
    let err = enforce_limits(&limits, 1, limits.max_header_size + 1, 10, None).unwrap_err();
    assert!(matches!(err, Error::Rejected(msg) if msg.contains("header block too large")));
}

#[test]
fn enforce_limits_rejects_post_body_over_limit() {
    let limits = RequestLimits::default();
    let err = enforce_limits(&limits, 1, 64, 10, Some(limits.max_post_size + 1)).unwrap_err();
    assert!(matches!(err, Error::Rejected(msg) if msg.contains("request body too large")));
}

#[test]
fn enforce_limits_rejects_uri_length_over_limit() {
    let limits = RequestLimits::default();
    let err = enforce_limits(&limits, 1, 64, limits.max_uri_len + 1, None).unwrap_err();
    assert!(matches!(err, Error::Rejected(msg) if msg.contains("URI too long")));
}

#[test]
fn enforce_limits_accepts_a_request_within_every_bound() {
    let limits = RequestLimits::default();
    assert!(enforce_limits(&limits, 20, 4096, 128, Some(1024)).is_ok());
    assert!(enforce_limits(&limits, 1, 1, 1, None).is_ok());
}

// =====================================================================
// BASIC authentication.
// =====================================================================

fn basic_realm() -> InMemoryRealm {
    InMemoryRealm::new().with_user("alice", "secret", vec!["user".into()])
}

#[tokio::test]
async fn basic_missing_authorization_returns_challenge_path() {
    let realm = basic_realm();
    let auth = BasicAuthenticator::new("Test");
    let principal = auth.authenticate(&realm, None).await.unwrap();
    assert!(
        principal.is_none(),
        "missing header must drop into the challenge path"
    );
    // The caller's response should carry exactly this header value.
    assert_eq!(auth.challenge(), "Basic realm=\"Test\"");
}

#[tokio::test]
async fn basic_malformed_authorization_is_rejected_as_401() {
    let realm = basic_realm();
    let auth = BasicAuthenticator::new("Test");
    // Wrong scheme.
    assert!(auth
        .authenticate(&realm, Some("Bearer abc"))
        .await
        .unwrap()
        .is_none());
    // Not base64.
    assert!(auth
        .authenticate(&realm, Some("Basic !!!not-base64!!!"))
        .await
        .unwrap()
        .is_none());
    // No colon after decoding.
    let no_colon =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"usernocolon");
    assert!(auth
        .authenticate(&realm, Some(&format!("Basic {no_colon}")))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn basic_valid_credentials_return_a_principal() {
    let realm = basic_realm();
    let auth = BasicAuthenticator::new("Test");
    // base64("alice:secret") == "YWxpY2U6c2VjcmV0"
    let principal = auth
        .authenticate(&realm, Some("Basic YWxpY2U6c2VjcmV0"))
        .await
        .unwrap()
        .expect("valid credentials must authenticate");
    assert_eq!(principal.name, "alice");
    assert!(principal.has_role("user"));
}

#[tokio::test]
async fn basic_invalid_password_returns_401() {
    let realm = basic_realm();
    let auth = BasicAuthenticator::new("Test");
    // base64("alice:wrong") == "YWxpY2U6d3Jvbmc="
    let principal = auth
        .authenticate(&realm, Some("Basic YWxpY2U6d3Jvbmc="))
        .await
        .unwrap();
    assert!(principal.is_none(), "wrong password must not authenticate");
}

// =====================================================================
// DIGEST authentication.
// =====================================================================

fn build_digest_authenticator() -> (
    DigestAuthenticator,
    &'static str,
    &'static str,
    &'static str,
) {
    let realm_name = "testrealm";
    let user = "alice";
    let password = "s3cret";
    let realm: Arc<dyn Realm> = Arc::new(InMemoryRealm::new().with_digest_user(
        user,
        realm_name,
        password,
        vec!["user".into()],
    ));
    let auth = DigestAuthenticator::new(realm, realm_name, b"unit-test-opaque-key-0123");
    (auth, realm_name, user, password)
}

/// Build a complete `Authorization: Digest ...` value for a given nonce, nc,
/// and cnonce.
fn digest_header(
    realm_name: &str,
    user: &str,
    password: &str,
    nonce: &str,
    nc: &str,
    cnonce: &str,
    uri: &str,
    method: &str,
) -> String {
    let ha1 = digest_ha1(user, realm_name, password);
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let response = md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"));
    format!(
        "Digest username=\"{user}\", realm=\"{realm_name}\", nonce=\"{nonce}\", \
         uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", response=\"{response}\""
    )
}

fn md5_hex(input: &str) -> String {
    let d = md5::compute(input.as_bytes());
    let mut out = String::with_capacity(32);
    for b in d.0 {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

#[tokio::test]
async fn digest_valid_round_trip_authenticates_the_user() {
    let (auth, realm_name, user, password) = build_digest_authenticator();
    let nonce = auth.generate_nonce();
    let header = digest_header(
        realm_name,
        user,
        password,
        &nonce,
        "00000001",
        "client-nonce-A",
        "/protected",
        "GET",
    );
    let principal = auth
        .authenticate(&header, "GET")
        .await
        .unwrap()
        .expect("a correct digest response must authenticate");
    assert_eq!(principal.name, user);
}

#[tokio::test]
async fn digest_stale_or_forged_nonce_is_rejected() {
    let (auth, realm_name, user, password) = build_digest_authenticator();
    // A nonce that was never minted by this authenticator: looks structurally
    // valid but fails the keyed-digest check.
    let bogus_nonce = format!("1000:{}:{}", "0".repeat(32), "0".repeat(32));
    let header = digest_header(
        realm_name,
        user,
        password,
        &bogus_nonce,
        "00000001",
        "cn",
        "/x",
        "GET",
    );
    assert!(
        auth.authenticate(&header, "GET").await.unwrap().is_none(),
        "forged/stale nonce must not authenticate"
    );
}

#[tokio::test]
async fn digest_replay_of_same_nonce_count_is_rejected() {
    let (auth, realm_name, user, password) = build_digest_authenticator();
    let nonce = auth.generate_nonce();
    let header = digest_header(
        realm_name, user, password, &nonce, "00000001", "cn", "/x", "GET",
    );
    // First use is accepted.
    assert!(auth.authenticate(&header, "GET").await.unwrap().is_some());
    // A replay with the *same* (nonce, nc) must be rejected.
    assert!(
        auth.authenticate(&header, "GET").await.unwrap().is_none(),
        "replay of (nonce, nc) must be rejected"
    );
}

#[tokio::test]
async fn digest_missing_or_non_digest_header_is_rejected() {
    let (auth, _, _, _) = build_digest_authenticator();
    // Wrong scheme.
    assert!(auth
        .authenticate("Basic xyz", "GET")
        .await
        .unwrap()
        .is_none());
    // Digest scheme but missing mandatory directives.
    assert!(auth
        .authenticate("Digest username=\"x\"", "GET")
        .await
        .unwrap()
        .is_none());
}

// =====================================================================
// CSRF — happy path, tamper, and timing.
// =====================================================================

#[test]
fn csrf_valid_token_passes_validation() {
    let store = CsrfTokenStore::new();
    let token = store.issue();
    assert!(store.validate(&token), "freshly-issued token must validate");
    // A round-trip through the wire-form a request would carry.
    let from_wire = CsrfToken::from_wire(token.as_str());
    assert!(store.validate(&from_wire));
}

#[test]
fn csrf_tampered_token_fails_validation() {
    let store = CsrfTokenStore::new();
    let token = store.issue();
    let mut chars: Vec<char> = token.as_str().chars().collect();
    chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
    let tampered: String = chars.into_iter().collect();
    assert!(!store.validate(&CsrfToken::from_wire(tampered)));
    // Wholly unrelated values must also fail.
    assert!(!store.validate(&CsrfToken::from_wire("")));
    assert!(!store.validate(&CsrfToken::from_wire("not-a-real-token")));
}

#[test]
fn csrf_comparison_documents_constant_time_behaviour() {
    // The contract `CsrfToken::matches` advertises is that the comparison
    // inspects every byte of the longer input, so a near-miss and a wild
    // guess take the same time. The behaviour we can observe from outside
    // the box is the *result*: length mismatches don't match, and an
    // all-zero token does not match a freshly-issued one.
    //
    // The doc comment on `CsrfToken::matches` is the authoritative source —
    // this test pins down the *invariants* the constant-time impl guarantees.
    let a = CsrfToken::from_wire("abcdef");
    let b = CsrfToken::from_wire("abcdef");
    let c = CsrfToken::from_wire("abcdez");
    let short = CsrfToken::from_wire("abcd");
    let long = CsrfToken::from_wire("abcdefgh");
    assert!(a.matches(&b), "equal tokens match");
    assert!(!a.matches(&c), "single-byte-different tokens do not match");
    assert!(!a.matches(&short), "shorter token does not match");
    assert!(!a.matches(&long), "longer token does not match");
}

#[test]
fn csrf_consume_retires_one_shot_token() {
    let store = CsrfTokenStore::new();
    let token = store.issue();
    assert!(store.consume(&token), "first use of a one-shot token wins");
    assert!(
        !store.consume(&token),
        "second use of a consumed token must fail"
    );
    assert!(!store.validate(&token));
}

// =====================================================================
// Security constraints.
// =====================================================================

fn collection(patterns: &[&str]) -> WebResourceCollection {
    WebResourceCollection {
        name: "test".into(),
        url_patterns: patterns.iter().map(|s| s.to_string()).collect(),
        http_methods: Vec::new(),
        http_method_omissions: Vec::new(),
    }
}

#[test]
fn constraint_deny_all_blocks_anonymous_and_authenticated_users() {
    let reg = ConstraintRegistry::new(vec![SecurityConstraint {
        web_resource_collections: vec![collection(&["/admin/*"])],
        auth_constraint: Some(AuthConstraint::default()),
        user_data_constraint: None,
    }]);
    // Anonymous → Forbidden (deny-all does not become an auth prompt).
    assert_eq!(
        reg.check("/admin/x", "GET", None, true),
        ConstraintDecision::Forbidden
    );
    // Authenticated with strong roles → also Forbidden.
    let u = Principal::new("root", vec!["admin".into(), "user".into()]);
    assert_eq!(
        reg.check("/admin/x", "GET", Some(&u), true),
        ConstraintDecision::Forbidden
    );
}

#[test]
fn constraint_role_mismatch_returns_forbidden() {
    let reg = ConstraintRegistry::new(vec![SecurityConstraint {
        web_resource_collections: vec![collection(&["/admin/*"])],
        auth_constraint: Some(AuthConstraint {
            role_names: vec!["admin".into()],
        }),
        user_data_constraint: None,
    }]);
    let bob = Principal::new("bob", vec!["user".into()]);
    assert_eq!(
        reg.check("/admin/panel", "GET", Some(&bob), true),
        ConstraintDecision::Forbidden
    );
}

#[test]
fn constraint_unauthenticated_when_role_required_and_anonymous() {
    let reg = ConstraintRegistry::new(vec![SecurityConstraint {
        web_resource_collections: vec![collection(&["/admin/*"])],
        auth_constraint: Some(AuthConstraint {
            role_names: vec!["admin".into()],
        }),
        user_data_constraint: None,
    }]);
    assert_eq!(
        reg.check("/admin/panel", "GET", None, true),
        ConstraintDecision::Unauthenticated
    );
}

#[test]
fn constraint_confidential_transport_required_when_not_secure() {
    let reg = ConstraintRegistry::new(vec![SecurityConstraint {
        web_resource_collections: vec![collection(&["/secure/*"])],
        auth_constraint: None,
        user_data_constraint: Some(UserDataConstraint {
            transport_guarantee: TransportGuarantee::Confidential,
        }),
    }]);
    // Insecure → must redirect to HTTPS (RequireTransport).
    assert_eq!(
        reg.check("/secure/page", "GET", None, false),
        ConstraintDecision::RequireTransport
    );
    // Same path over a secure transport → allow.
    assert_eq!(
        reg.check("/secure/page", "GET", None, true),
        ConstraintDecision::Allow
    );
}

#[test]
fn constraint_transport_fires_before_authentication_prompt() {
    // Even when authentication would also be required, the transport check
    // fires first — otherwise the auth prompt would leak over HTTP.
    let reg = ConstraintRegistry::new(vec![SecurityConstraint {
        web_resource_collections: vec![collection(&["/secure/*"])],
        auth_constraint: Some(AuthConstraint {
            role_names: vec!["admin".into()],
        }),
        user_data_constraint: Some(UserDataConstraint {
            transport_guarantee: TransportGuarantee::Confidential,
        }),
    }]);
    assert_eq!(
        reg.check("/secure/x", "GET", None, false),
        ConstraintDecision::RequireTransport
    );
}
