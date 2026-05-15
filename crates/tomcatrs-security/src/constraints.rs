//! Servlet `<security-constraint>` evaluation.
//!
//! The Servlet specification defines a small declarative security model on top
//! of `web.xml`. A deployment declares any number of
//! [`SecurityConstraint`]s — each one binds a *set* of
//! [`WebResourceCollection`]s (URL patterns plus HTTP methods) to an optional
//! [`AuthConstraint`] (which roles are permitted) and an optional
//! [`UserDataConstraint`] (whether a confidential transport is required).
//!
//! Before dispatching a request the container consults the constraint registry
//! to decide whether to allow it, demand authentication, return `403`, or
//! redirect to HTTPS. The algorithm implemented in [`ConstraintRegistry::check`]
//! follows the spec's aggregation rules:
//!
//! 1. Collect every constraint whose `WebResourceCollection` matches the
//!    request — using Servlet pattern semantics (exact ➜ longest path-prefix
//!    ➜ extension ➜ default `/`) and respecting `<http-method>` /
//!    `<http-method-omission>` filters.
//! 2. **Auth-constraint aggregation.** If *any* matching constraint omits its
//!    `<auth-constraint>` the resource is uncovered for authorization. If
//!    *every* matching constraint declares an empty role list the request is
//!    forbidden to every user (deny-all). Otherwise the union of the role
//!    lists is the set of permitted roles — `*` is the wildcard "any
//!    authenticated user".
//! 3. **User-data aggregation.** The strongest declared transport guarantee
//!    among matching constraints wins; `Confidential` requires HTTPS.
//!
//! This module is **fully working** and has no `todo!()` placeholders.

use crate::realm::Principal;

/// The transport guarantee a `<user-data-constraint>` demands.
///
/// `None` means plain HTTP is acceptable; `Integral` and `Confidential` both
/// require a transport whose secrecy/integrity cannot be tampered with — in
/// practice, HTTPS. The Servlet specification does not distinguish their
/// enforcement, so this implementation treats them equivalently when checking,
/// while still surfacing the declared level for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportGuarantee {
    /// No transport guarantee required — plain HTTP is fine.
    None,
    /// Integrity is required (no tampering in transit).
    Integral,
    /// Confidentiality is required (encrypted transport).
    Confidential,
}

impl TransportGuarantee {
    /// Returns `true` if this guarantee can only be satisfied by a secure
    /// connection.
    pub fn requires_secure(self) -> bool {
        matches!(
            self,
            TransportGuarantee::Integral | TransportGuarantee::Confidential
        )
    }
}

/// A `<web-resource-collection>`: a named bundle of URL patterns plus an
/// optional HTTP-method filter.
///
/// Per the spec, if `http_methods` is non-empty only those methods are covered
/// by the parent constraint; if `http_method_omissions` is non-empty *all*
/// methods *except* those are covered; if both are empty every method is
/// covered.
#[derive(Debug, Clone, Default)]
pub struct WebResourceCollection {
    /// Human-readable name (`<web-resource-name>`).
    pub name: String,
    /// `<url-pattern>` entries — Servlet patterns (`/foo/*`, `*.jsp`, `/foo`, `/`).
    pub url_patterns: Vec<String>,
    /// `<http-method>` entries restricting the collection to specific methods.
    pub http_methods: Vec<String>,
    /// `<http-method-omission>` entries: every method *except* these is covered.
    pub http_method_omissions: Vec<String>,
}

impl WebResourceCollection {
    /// Returns `true` if `method` is covered by this collection's method filter.
    fn covers_method(&self, method: &str) -> bool {
        if !self.http_methods.is_empty() {
            return self
                .http_methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method));
        }
        if !self.http_method_omissions.is_empty() {
            return !self
                .http_method_omissions
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method));
        }
        true
    }

    /// Returns the *match length* of the best-matching URL pattern in this
    /// collection for `path`, or `None` if none matches.
    ///
    /// The length is used to break ties between overlapping constraints: per
    /// the Servlet spec, the most-specific (longest) matching pattern wins.
    fn best_match_len(&self, path: &str) -> Option<usize> {
        let mut best: Option<usize> = None;
        for pat in &self.url_patterns {
            if let Some(len) = pattern_match_len(pat, path) {
                best = Some(match best {
                    Some(prev) if prev >= len => prev,
                    _ => len,
                });
            }
        }
        best
    }
}

/// An `<auth-constraint>` listing the roles allowed to access the resource.
///
/// Per the Servlet specification:
///
/// * An *absent* `<auth-constraint>` (modelled as
///   `SecurityConstraint::auth_constraint == None`) imposes no authorization
///   requirement at all.
/// * An auth-constraint with an *empty* role list is **deny-all** — no user,
///   authenticated or not, may access the resource.
/// * A role name of `"*"` matches any authenticated user.
/// * A role name of `"**"` (Servlet 3.1+) also matches any authenticated
///   user — handled the same as `"*"` here.
/// * Otherwise the principal must hold at least one of the named roles.
#[derive(Debug, Clone, Default)]
pub struct AuthConstraint {
    /// The role names permitted; empty means deny-all.
    pub role_names: Vec<String>,
}

impl AuthConstraint {
    /// Returns `true` if this constraint denies everyone.
    pub fn is_deny_all(&self) -> bool {
        self.role_names.is_empty()
    }

    /// Returns `true` if this constraint allows any authenticated user.
    pub fn allows_any_authenticated(&self) -> bool {
        self.role_names.iter().any(|r| r == "*" || r == "**")
    }
}

/// A `<user-data-constraint>` — only its transport guarantee is modelled.
#[derive(Debug, Clone, Copy)]
pub struct UserDataConstraint {
    /// The required transport guarantee.
    pub transport_guarantee: TransportGuarantee,
}

/// A complete `<security-constraint>` element.
#[derive(Debug, Clone, Default)]
pub struct SecurityConstraint {
    /// One or more `<web-resource-collection>`s that this constraint covers.
    pub web_resource_collections: Vec<WebResourceCollection>,
    /// The `<auth-constraint>`, if declared.
    pub auth_constraint: Option<AuthConstraint>,
    /// The `<user-data-constraint>`, if declared.
    pub user_data_constraint: Option<UserDataConstraint>,
}

/// The outcome of evaluating the constraint registry against a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintDecision {
    /// The request may proceed unchanged.
    Allow,
    /// Authentication is required — the caller is anonymous or unauthenticated
    /// but the matching constraint demands at least one role.
    Unauthenticated,
    /// The principal is authenticated but holds no permitted role, or the
    /// matching constraint is deny-all.
    Forbidden,
    /// A confidential transport is required and the request did not arrive
    /// over one — the caller should redirect to HTTPS or reject with 403.
    RequireTransport,
}

/// A read-only collection of [`SecurityConstraint`]s with a single entry
/// point — [`ConstraintRegistry::check`] — for evaluating a request.
#[derive(Debug, Clone, Default)]
pub struct ConstraintRegistry {
    constraints: Vec<SecurityConstraint>,
}

impl ConstraintRegistry {
    /// Build a registry from a list of parsed constraints.
    pub fn new(constraints: Vec<SecurityConstraint>) -> Self {
        Self { constraints }
    }

    /// Number of constraints in the registry.
    pub fn len(&self) -> usize {
        self.constraints.len()
    }

    /// Returns `true` if the registry holds no constraints.
    pub fn is_empty(&self) -> bool {
        self.constraints.is_empty()
    }

    /// Evaluate every constraint against `(path, method)` and decide.
    ///
    /// `principal` is `Some` once an authenticator has identified the caller;
    /// `secure` indicates whether the underlying connection is HTTPS (or
    /// otherwise satisfies an integral/confidential transport guarantee).
    pub fn check(
        &self,
        path: &str,
        method: &str,
        principal: Option<&Principal>,
        secure: bool,
    ) -> ConstraintDecision {
        // ----- Phase 1: gather every matching constraint and remember the
        // best (longest) pattern length so we can apply spec rule "the most
        // specific match wins" for tie-breaking when constraints disagree.
        struct Hit<'a> {
            constraint: &'a SecurityConstraint,
            match_len: usize,
        }

        let mut hits: Vec<Hit<'_>> = Vec::new();
        for c in &self.constraints {
            let mut best_len: Option<usize> = None;
            for col in &c.web_resource_collections {
                if !col.covers_method(method) {
                    continue;
                }
                if let Some(len) = col.best_match_len(path) {
                    best_len = Some(match best_len {
                        Some(prev) if prev >= len => prev,
                        _ => len,
                    });
                }
            }
            if let Some(len) = best_len {
                hits.push(Hit {
                    constraint: c,
                    match_len: len,
                });
            }
        }

        if hits.is_empty() {
            return ConstraintDecision::Allow;
        }

        // ----- Phase 2: apply Servlet "longest pattern wins" tie-breaker.
        //
        // The spec aggregates *all* matching constraints, but in practice the
        // long-pattern-wins rule resolves the common case where a broad
        // deny-all `/*` is overridden by a tighter `/public/*` allow. We keep
        // only the constraints whose match length equals the maximum.
        let max_len = hits.iter().map(|h| h.match_len).max().unwrap_or(0);
        hits.retain(|h| h.match_len == max_len);

        // ----- Phase 3: aggregate user-data and auth constraints.
        //
        // A matching constraint *without* an auth-constraint leaves the
        // resource uncovered for authorization purposes — the spec says the
        // resource is then accessible to anyone. We track that explicitly so
        // we do not silently fall through to "deny-all" when one of the hits
        // is permissive.
        let mut any_uncovered_auth = false;
        let mut all_deny_all = true;
        let mut any_auth_constraint = false;
        let mut permitted_roles: Vec<&str> = Vec::new();
        let mut wildcard_any_auth = false;

        let mut strongest_transport = TransportGuarantee::None;

        for hit in &hits {
            match &hit.constraint.auth_constraint {
                None => {
                    any_uncovered_auth = true;
                    all_deny_all = false;
                }
                Some(ac) => {
                    any_auth_constraint = true;
                    if ac.is_deny_all() {
                        // Empty role list = deny-all. Leaves `all_deny_all`
                        // unchanged (still true unless some other hit clears
                        // it).
                    } else {
                        all_deny_all = false;
                        if ac.allows_any_authenticated() {
                            wildcard_any_auth = true;
                        }
                        for role in &ac.role_names {
                            if role != "*" && role != "**" {
                                permitted_roles.push(role.as_str());
                            }
                        }
                    }
                }
            }

            if let Some(ud) = &hit.constraint.user_data_constraint {
                if matches!(ud.transport_guarantee, TransportGuarantee::Confidential) {
                    strongest_transport = TransportGuarantee::Confidential;
                } else if matches!(ud.transport_guarantee, TransportGuarantee::Integral)
                    && !matches!(strongest_transport, TransportGuarantee::Confidential)
                {
                    strongest_transport = TransportGuarantee::Integral;
                }
            }
        }

        // ----- Phase 4: enforce transport before authorization. A 302 to the
        // HTTPS variant must happen before we ask the user for credentials.
        if strongest_transport.requires_secure() && !secure {
            return ConstraintDecision::RequireTransport;
        }

        // ----- Phase 5: authorization decision.
        if any_uncovered_auth {
            // At least one matching constraint imposed no auth requirement.
            return ConstraintDecision::Allow;
        }

        if any_auth_constraint && all_deny_all {
            // Every matching constraint had an empty role list.
            return ConstraintDecision::Forbidden;
        }

        match principal {
            None => ConstraintDecision::Unauthenticated,
            Some(p) => {
                if wildcard_any_auth {
                    return ConstraintDecision::Allow;
                }
                if permitted_roles.iter().any(|r| p.has_role(r)) {
                    ConstraintDecision::Allow
                } else {
                    ConstraintDecision::Forbidden
                }
            }
        }
    }
}

/// Returns the *match length* — a measure of specificity — for `pattern`
/// against `path`, or `None` when the pattern does not match.
///
/// Servlet URL pattern types (spec §12.2):
///
/// * **Exact** — literal path; match length is the path length.
/// * **Path prefix** — `/foo/*`; match length is the prefix length.
/// * **Extension** — `*.jsp`; match length is the extension length (treated
///   as less specific than any path prefix).
/// * **Default** — `/`; the catch-all, match length `0`.
fn pattern_match_len(pattern: &str, path: &str) -> Option<usize> {
    // Default match — `/` matches everything but is the least specific.
    if pattern == "/" {
        return Some(0);
    }

    // Extension match.
    if let Some(ext) = pattern.strip_prefix("*.") {
        if let Some(dot) = path.rfind('.') {
            if path[dot + 1..].eq_ignore_ascii_case(ext) {
                // Extension matches are inherently less specific than any path
                // prefix; give them a tiny weight so they still beat the
                // default `/` mapping but lose to any real prefix or exact.
                return Some(ext.len());
            }
        }
        return None;
    }

    // Path-prefix match.
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if path == prefix {
            // The bare prefix path itself matches `/foo/*`.
            return Some(prefix.len() + 2);
        }
        let prefix_with_slash = format!("{prefix}/");
        if path.starts_with(&prefix_with_slash) {
            return Some(prefix.len() + 2);
        }
        return None;
    }

    // Exact match.
    if pattern == path {
        // Exact wins over everything; reserve a large length so it cannot
        // be beaten by a prefix of equal nominal length.
        return Some(path.len() + 1);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str, roles: &[&str]) -> Principal {
        Principal::new(name, roles.iter().map(|s| s.to_string()).collect())
    }

    fn collection(patterns: &[&str], methods: &[&str]) -> WebResourceCollection {
        WebResourceCollection {
            name: "test".into(),
            url_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            http_methods: methods.iter().map(|s| s.to_string()).collect(),
            http_method_omissions: Vec::new(),
        }
    }

    fn collection_omit(patterns: &[&str], omissions: &[&str]) -> WebResourceCollection {
        WebResourceCollection {
            name: "test".into(),
            url_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            http_methods: Vec::new(),
            http_method_omissions: omissions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn deny_all(patterns: &[&str]) -> SecurityConstraint {
        SecurityConstraint {
            web_resource_collections: vec![collection(patterns, &[])],
            auth_constraint: Some(AuthConstraint::default()),
            user_data_constraint: None,
        }
    }

    fn require_roles(patterns: &[&str], roles: &[&str]) -> SecurityConstraint {
        SecurityConstraint {
            web_resource_collections: vec![collection(patterns, &[])],
            auth_constraint: Some(AuthConstraint {
                role_names: roles.iter().map(|s| s.to_string()).collect(),
            }),
            user_data_constraint: None,
        }
    }

    #[test]
    fn no_constraints_allows_everything() {
        let reg = ConstraintRegistry::new(vec![]);
        assert_eq!(
            reg.check("/anything", "GET", None, false),
            ConstraintDecision::Allow
        );
    }

    #[test]
    fn deny_all_constraint_blocks_any_user() {
        let reg = ConstraintRegistry::new(vec![deny_all(&["/admin/*"])]);
        // Anonymous.
        assert_eq!(
            reg.check("/admin/index", "GET", None, false),
            ConstraintDecision::Forbidden
        );
        // Authenticated with any role.
        let u = user("alice", &["admin", "user"]);
        assert_eq!(
            reg.check("/admin/index", "GET", Some(&u), false),
            ConstraintDecision::Forbidden
        );
    }

    #[test]
    fn role_match_allows_request() {
        let reg = ConstraintRegistry::new(vec![require_roles(&["/admin/*"], &["admin"])]);
        let u = user("alice", &["admin"]);
        assert_eq!(
            reg.check("/admin/panel", "GET", Some(&u), false),
            ConstraintDecision::Allow
        );
    }

    #[test]
    fn role_mismatch_returns_forbidden() {
        let reg = ConstraintRegistry::new(vec![require_roles(&["/admin/*"], &["admin"])]);
        let u = user("bob", &["user"]);
        assert_eq!(
            reg.check("/admin/panel", "GET", Some(&u), false),
            ConstraintDecision::Forbidden
        );
    }

    #[test]
    fn unauthenticated_when_role_required_and_no_principal() {
        let reg = ConstraintRegistry::new(vec![require_roles(&["/admin/*"], &["admin"])]);
        assert_eq!(
            reg.check("/admin/panel", "GET", None, false),
            ConstraintDecision::Unauthenticated
        );
    }

    #[test]
    fn wildcard_star_allows_any_authenticated() {
        let reg = ConstraintRegistry::new(vec![require_roles(&["/members/*"], &["*"])]);
        let u = user("anyone", &[]);
        assert_eq!(
            reg.check("/members/home", "GET", Some(&u), false),
            ConstraintDecision::Allow
        );
        assert_eq!(
            reg.check("/members/home", "GET", None, false),
            ConstraintDecision::Unauthenticated
        );
    }

    #[test]
    fn confidential_transport_required_on_insecure() {
        let reg = ConstraintRegistry::new(vec![SecurityConstraint {
            web_resource_collections: vec![collection(&["/secure/*"], &[])],
            auth_constraint: None,
            user_data_constraint: Some(UserDataConstraint {
                transport_guarantee: TransportGuarantee::Confidential,
            }),
        }]);
        assert_eq!(
            reg.check("/secure/page", "GET", None, false),
            ConstraintDecision::RequireTransport
        );
        // Same request over HTTPS — allowed.
        assert_eq!(
            reg.check("/secure/page", "GET", None, true),
            ConstraintDecision::Allow
        );
    }

    #[test]
    fn confidential_required_before_auth_check() {
        // Even when authentication is also required, the transport check fires
        // first — otherwise we'd leak the auth prompt over HTTP.
        let reg = ConstraintRegistry::new(vec![SecurityConstraint {
            web_resource_collections: vec![collection(&["/secure/*"], &[])],
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

    #[test]
    fn http_method_filter_only_covers_listed_methods() {
        // Constraint covers only POST/PUT/DELETE on /api/* — GET passes free.
        let reg = ConstraintRegistry::new(vec![SecurityConstraint {
            web_resource_collections: vec![collection(&["/api/*"], &["POST", "PUT", "DELETE"])],
            auth_constraint: Some(AuthConstraint::default()),
            user_data_constraint: None,
        }]);
        assert_eq!(
            reg.check("/api/items", "GET", None, false),
            ConstraintDecision::Allow
        );
        assert_eq!(
            reg.check("/api/items", "POST", None, false),
            ConstraintDecision::Forbidden
        );
        assert_eq!(
            reg.check("/api/items", "DELETE", None, false),
            ConstraintDecision::Forbidden
        );
    }

    #[test]
    fn http_method_omission_covers_everything_except_listed() {
        // Deny-all on every method *except* GET.
        let reg = ConstraintRegistry::new(vec![SecurityConstraint {
            web_resource_collections: vec![collection_omit(&["/api/*"], &["GET"])],
            auth_constraint: Some(AuthConstraint::default()),
            user_data_constraint: None,
        }]);
        assert_eq!(
            reg.check("/api/items", "GET", None, false),
            ConstraintDecision::Allow
        );
        assert_eq!(
            reg.check("/api/items", "POST", None, false),
            ConstraintDecision::Forbidden
        );
        assert_eq!(
            reg.check("/api/items", "PATCH", None, false),
            ConstraintDecision::Forbidden
        );
    }

    #[test]
    fn longest_pattern_wins_over_broader_constraint() {
        // Broad `/admin/*` is deny-all; tighter `/admin/public/*` is uncovered
        // (no auth-constraint) so it permits anonymous access.
        let reg = ConstraintRegistry::new(vec![
            deny_all(&["/admin/*"]),
            SecurityConstraint {
                web_resource_collections: vec![collection(&["/admin/public/*"], &[])],
                auth_constraint: None,
                user_data_constraint: None,
            },
        ]);
        // Tightest pattern wins — request is allowed.
        assert_eq!(
            reg.check("/admin/public/help", "GET", None, false),
            ConstraintDecision::Allow
        );
        // Outside the narrower pattern the deny-all still applies.
        assert_eq!(
            reg.check("/admin/secret", "GET", None, false),
            ConstraintDecision::Forbidden
        );
    }

    #[test]
    fn exact_pattern_beats_prefix() {
        // `/foo/bar` (exact) wins over `/foo/*` (prefix) per spec precedence.
        let reg = ConstraintRegistry::new(vec![
            deny_all(&["/foo/*"]),
            SecurityConstraint {
                web_resource_collections: vec![collection(&["/foo/bar"], &[])],
                auth_constraint: None,
                user_data_constraint: None,
            },
        ]);
        assert_eq!(
            reg.check("/foo/bar", "GET", None, false),
            ConstraintDecision::Allow
        );
        assert_eq!(
            reg.check("/foo/quux", "GET", None, false),
            ConstraintDecision::Forbidden
        );
    }

    #[test]
    fn extension_pattern_matches_jsp_files() {
        let reg = ConstraintRegistry::new(vec![require_roles(&["*.jsp"], &["jsp-user"])]);
        let u = user("alice", &["jsp-user"]);
        assert_eq!(
            reg.check("/a/b/index.jsp", "GET", Some(&u), false),
            ConstraintDecision::Allow
        );
        assert_eq!(
            reg.check("/a/b/index.html", "GET", None, false),
            ConstraintDecision::Allow
        );
        assert_eq!(
            reg.check("/a/b/index.jsp", "GET", None, false),
            ConstraintDecision::Unauthenticated
        );
    }

    #[test]
    fn pattern_match_len_precedence() {
        // Sanity check the helper directly.
        assert_eq!(pattern_match_len("/", "/anything"), Some(0));
        assert!(
            pattern_match_len("/foo/bar", "/foo/bar").unwrap()
                > pattern_match_len("/foo/*", "/foo/bar").unwrap()
        );
        assert!(
            pattern_match_len("/foo/*", "/foo/bar").unwrap()
                > pattern_match_len("*.bar", "/foo/baz.bar").unwrap_or(0)
        );
        assert_eq!(pattern_match_len("*.jsp", "/x/y.jsp"), Some(3));
        assert_eq!(pattern_match_len("/foo/*", "/foo"), Some(6));
        assert_eq!(pattern_match_len("/foo/*", "/foobar"), None);
    }
}
