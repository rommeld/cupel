//! PKCE (RFC 7636) for the OAuth authorization-code flow.
//!
//! PKCE proves that the app REDEEMING an authorization code is the same
//! app that STARTED the login: the authorize request carries a hash (the
//! challenge), and the token exchange must present the preimage.
//! An attacker who intercepts the redirect gets a code they
//! cannot redeem.

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// One login attempt's verifier/challenge pair.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// base64url WITHOUT padding - RFC 7636 prescribes exactly this alphabet,
/// and a trailing `=` would be percent-encoded into URL noise anyway.
fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generate a fresh pair from OS entropy.
#[must_use]
pub fn generate() -> Pkce {
    // 32 random bytes -> 43 base64url chars, comfortably inside
    // the RFC's 43-128 character window.
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("OS entropy source unavailable");
    let verifier = base64url(&bytes);
    Pkce {
        challenge: challenge_for(&verifier),
        verifier,
    }
}

/// The S256 method: `challenge = base64url(sha256(ascii(verifier)))`.
/// Split out (and pub(crate)) so tests can pin the RFC 7636 test vector.
#[must_use]
pub(crate) fn challenge_for(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_the_rfc_7636_test_vector() {
        // RFC 7636 appendix B pins this exact pair - if the hash, the
        // encoding, or the padding handling is wrong, this cannot pass.
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pairs_are_fresh_and_well_formed() {
        let a = generate();
        let b = generate();
        // 32 bytes -> ceil(32*8/6) = 43 chars, no padding.
        assert_eq!(a.verifier.len(), 43);
        assert_eq!(a.challenge.len(), 43);
        assert!(
            a.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "verifier must stay in the base64url alphabet: {}",
            a.verifier
        );
        // Two logins must never share a verifier.
        assert_ne!(a.verifier, b.verifier);
        // The challenge is DERIVED, never equal to its verifier.
        assert_ne!(a.verifier, a.challenge);
        assert_eq!(challenge_for(&a.verifier), a.challenge);
    }
}
