//! `tomcatrs-security` — realms, authenticators, CSRF protection, and
//! connector-layer request hardening for the **Tomcat-RS Compatibility
//! Runtime**.
//!
//! # Why this crate matters
//!
//! The central thesis of the Tomcat-RS rewrite is that the *connector and
//! parsing layer* is where the security value lives. A servlet container
//! written in a memory-safe language with strict, explicit request validation
//! eliminates entire bug classes (buffer mishandling, ambiguous path
//! normalization, request smuggling) that have historically plagued the JVM
//! implementation. This crate concentrates that effort.
//!
//! # Modules
//!
//! * [`realm`] — the [`Realm`](realm::Realm) authentication abstraction and a
//!   fully-working in-memory implementation that stores salted password
//!   hashes.
//! * [`auth_basic`] — a complete HTTP `BASIC` authenticator.
//! * [`auth_digest`] — an HTTP `DIGEST` authenticator scaffold: real challenge
//!   generation and types, with credential verification deferred to a later
//!   release.
//! * [`auth_form`] — a `FORM` (`j_security_check`) authenticator scaffold.
//! * [`csrf`] — a complete cryptographically-random CSRF token store with
//!   constant-time validation.
//! * [`access_control`] — request hardening: URI normalization/validation and
//!   request-limit enforcement. The most important module in the crate.
//!
//! All public items are documented and the "fully working" modules contain no
//! `todo!()`/`unimplemented!()` placeholders.

#![deny(missing_docs)]

pub mod access_control;
pub mod auth_basic;
pub mod auth_digest;
pub mod auth_form;
pub mod constraints;
pub mod csrf;
pub mod realm;
pub mod realm_backends;

pub use access_control::{
    enforce_limits, is_path_traversal, is_protected_path, normalize_and_validate_uri,
};
pub use auth_basic::BasicAuthenticator;
pub use auth_digest::DigestAuthenticator;
pub use auth_form::{FormAuthOutcome, FormAuthenticator, FormCredentials, SavedRequestStore};
pub use constraints::{ConstraintDecision, ConstraintRegistry, SecurityConstraint};
pub use csrf::{CsrfToken, CsrfTokenStore};
pub use realm::{InMemoryRealm, Principal, Realm};
pub use realm_backends::{CombinedRealm, FileRealm, LockOutRealm};

/// Crate version, sourced from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
