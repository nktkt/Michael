//! CSRF (Cross-Site Request Forgery) token support.
//!
//! This module is **fully working**. It mirrors Tomcat's
//! `CsrfPreventionFilter`: a per-session set of unguessable tokens is issued,
//! embedded in forms/links, and required back on state-changing requests.
//!
//! Two properties matter for correctness:
//!
//! * **Unpredictability** — tokens are 256 bits of CSPRNG output, so an
//!   attacker cannot guess a valid token.
//! * **Constant-time comparison** — [`CsrfToken::matches`] compares in time
//!   independent of where the first mismatching byte falls, so validation does
//!   not leak the token through a timing side channel.

use std::collections::HashSet;

use parking_lot::Mutex;
use rand::RngCore;

/// Number of random bytes backing a token (256 bits).
const TOKEN_BYTES: usize = 32;

/// A single CSRF token.
///
/// The wire form is the lowercase hex encoding of 32 random bytes — 64 ASCII
/// characters, safe to embed in HTML attributes and headers without further
/// escaping.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CsrfToken(String);

impl CsrfToken {
    /// Generate a fresh, cryptographically-random token.
    pub fn generate() -> CsrfToken {
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        let mut hex = String::with_capacity(TOKEN_BYTES * 2);
        for byte in bytes {
            use std::fmt::Write;
            let _ = write!(hex, "{:02x}", byte);
        }
        CsrfToken(hex)
    }

    /// The token's wire representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reconstruct a token from a value received on a request.
    ///
    /// This does not validate the value against any store — it only wraps it
    /// so it can be compared. Validation is [`CsrfTokenStore::validate`].
    pub fn from_wire(value: impl Into<String>) -> CsrfToken {
        CsrfToken(value.into())
    }

    /// Compare two tokens in constant time.
    ///
    /// The comparison always inspects every byte of the longer input, so the
    /// time it takes does not reveal the position of the first difference.
    /// Tokens of differing length never match.
    pub fn matches(&self, other: &CsrfToken) -> bool {
        let a = self.0.as_bytes();
        let b = other.0.as_bytes();
        // Fold length difference into the accumulator so unequal lengths
        // cannot match, without an early return that would leak length.
        let mut diff: u8 = (a.len() as u64 ^ b.len() as u64) as u8
            | ((a.len() as u64 ^ b.len() as u64) >> 8) as u8;
        let len = a.len().max(b.len());
        for i in 0..len {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            diff |= x ^ y;
        }
        diff == 0
    }
}

/// A thread-safe set of currently-valid CSRF tokens.
///
/// In a real deployment one store is held per HTTP session. Tokens are issued
/// with [`issue`], checked with [`validate`], and (for one-shot tokens) can be
/// retired with [`consume`].
///
/// [`issue`]: CsrfTokenStore::issue
/// [`validate`]: CsrfTokenStore::validate
/// [`consume`]: CsrfTokenStore::consume
#[derive(Debug, Default)]
pub struct CsrfTokenStore {
    tokens: Mutex<HashSet<CsrfToken>>,
}

impl CsrfTokenStore {
    /// Create an empty store.
    pub fn new() -> CsrfTokenStore {
        CsrfTokenStore {
            tokens: Mutex::new(HashSet::new()),
        }
    }

    /// Generate a fresh token, record it as valid, and return it.
    pub fn issue(&self) -> CsrfToken {
        let token = CsrfToken::generate();
        self.tokens.lock().insert(token.clone());
        token
    }

    /// Returns `true` if `presented` matches a currently-valid token.
    ///
    /// The presented value is compared against every stored token in constant
    /// time, so a near-miss is indistinguishable from a wild guess.
    pub fn validate(&self, presented: &CsrfToken) -> bool {
        let tokens = self.tokens.lock();
        let mut ok = false;
        for stored in tokens.iter() {
            // Bitwise-or so the loop never short-circuits on the first match.
            ok |= stored.matches(presented);
        }
        ok
    }

    /// Validate `presented` and, if valid, retire it so it cannot be reused.
    ///
    /// Use this for one-shot tokens guarding sensitive single actions.
    pub fn consume(&self, presented: &CsrfToken) -> bool {
        let mut tokens = self.tokens.lock();
        let hit = tokens.iter().find(|t| t.matches(presented)).cloned();
        match hit {
            Some(token) => {
                tokens.remove(&token);
                true
            }
            None => false,
        }
    }

    /// Number of currently-valid tokens.
    pub fn len(&self) -> usize {
        self.tokens.lock().len()
    }

    /// Returns `true` if no tokens are currently valid.
    pub fn is_empty(&self) -> bool {
        self.tokens.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_unique_and_long() {
        let a = CsrfToken::generate();
        let b = CsrfToken::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), TOKEN_BYTES * 2);
    }

    #[test]
    fn valid_token_passes_validation() {
        let store = CsrfTokenStore::new();
        let token = store.issue();
        assert!(store.validate(&token));
        // Round-trip through the wire form a request would carry.
        let from_request = CsrfToken::from_wire(token.as_str());
        assert!(store.validate(&from_request));
    }

    #[test]
    fn tampered_token_fails_validation() {
        let store = CsrfTokenStore::new();
        let token = store.issue();
        let mut tampered: Vec<char> = token.as_str().chars().collect();
        // Flip the first hex character to something different.
        tampered[0] = if tampered[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = tampered.into_iter().collect();
        assert!(!store.validate(&CsrfToken::from_wire(tampered)));
        assert!(!store.validate(&CsrfToken::from_wire("")));
        assert!(!store.validate(&CsrfToken::from_wire("short")));
    }

    #[test]
    fn matches_is_length_sensitive() {
        let a = CsrfToken::from_wire("abcd");
        let b = CsrfToken::from_wire("abcde");
        assert!(!a.matches(&b));
        assert!(a.matches(&CsrfToken::from_wire("abcd")));
    }

    #[test]
    fn consume_retires_the_token() {
        let store = CsrfTokenStore::new();
        let token = store.issue();
        assert!(store.consume(&token));
        // Second use must fail — the token was retired.
        assert!(!store.consume(&token));
        assert!(!store.validate(&token));
    }
}
