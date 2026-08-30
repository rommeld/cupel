//! OAuth credential storage - `~/.cupel/auth.json`.
//!
//! The subscription-login counterpart to settings.rs: settings.json holds
//! API keys the user typed, auth.json holds tokens a login FLOW minted
//! (and rotates on refresh). pi keeps the same split (`~/.pi/agent/auth.json`,
//! core/auth-storage.ts); the file shape is pi's too - a map of provider
//! id to a type-tagged credential:
//!
//! ```json
//! {
//!   "openai-codex": {
//!     "type": "oauth",
//!     "access": "...", "refresh": "...",
//!     "expires": 1756500000000, "accountId": "..."
//!   }
//! }
//! ```
//!
//! Writes reuse settings.rs's mechanics verbatim: re-read from disk,
//! refuse malformed files, 0600 temp file, fsync, atomic rename. What
//! cupel deliberately does NOT mirror: pi's cross-process file lock
//! (proper-lockfile). Two concurrent cupel instances refreshing at once
//! both write a complete valid file and the last rename wins - the same
//! accepted caveat settings.rs documents for keys. The refresh endpoint
//! tolerates that: each grant returns a fresh, complete token pair.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cupel_core::oauth::openai_codex::{self, OAuthCredential};
use serde::{Deserialize, Serialize};

/// One stored credential. The `type` tag is future room (pi stores
/// `api_key` entries here too); cupel writes only OAuth today.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredCredential {
    Oauth(OAuthCredential),
}

/// Refresh when less than five minutes of validity remain - pi's margin
/// (auth/resolve.ts DEFAULT_OAUTH_MINIMUM_VALIDITY_MS). Generous enough
/// that a token can never expire between resolution and the request.
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;

/// `~/.cupel/auth.json`.
#[must_use]
pub fn auth_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

/// Parse the auth file. Missing = empty; malformed = an error the caller
/// surfaces (same tiers as settings::load_settings - a credentials file
/// that stopped parsing deserves a visible failure, not silent logouts).
fn load_auth_file(path: &Path) -> Result<BTreeMap<String, StoredCredential>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&content).map_err(|e| format!("{} is not valid: {e}", path.display()))
}

/// All stored credentials; malformed files are warn-and-empty (via
/// tracing, never stderr - this runs while the TUI owns the screen).
#[must_use]
pub fn load_auth(home: Option<&Path>) -> BTreeMap<String, StoredCredential> {
    let Some(home) = home else {
        return BTreeMap::new();
    };
    match load_auth_file(&auth_path(home)) {
        Ok(auth) => auth,
        Err(e) => {
            tracing::warn!("ignoring auth file: {e}");
            BTreeMap::new()
        }
    }
}

/// The stored OAuth credential for one provider, if any.
#[must_use]
pub fn credential(home: Option<&Path>, provider: &str) -> Option<OAuthCredential> {
    // The closure pattern is irrefutable: Oauth is the only variant.
    load_auth(home)
        .remove(provider)
        .map(|StoredCredential::Oauth(credential)| credential)
}

/// Whether a login is stored - drives `/provider` status lines and
/// startup model selection (never inspects token validity: an expired
/// access token still counts, the refresh token is what matters).
#[must_use]
pub fn has_credential(home: Option<&Path>, provider: &str) -> bool {
    credential(home, provider).is_some()
}

/// Read-modify-write one credential into auth.json - settings.rs's
/// save_provider_key with a different payload: fresh read (hand edits
/// survive, malformed files are refused), 0600 same-directory temp file,
/// fsync, atomic rename.
pub fn save_credential(
    home: Option<&Path>,
    provider: &str,
    credential: &OAuthCredential,
) -> Result<PathBuf, crate::settings::SaveError> {
    modify_auth(home, |auth| {
        auth.insert(
            provider.to_string(),
            StoredCredential::Oauth(credential.clone()),
        );
    })
}

/// Remove a credential (logout). Ok(false) = nothing was stored.
pub fn delete_credential(
    home: Option<&Path>,
    provider: &str,
) -> Result<bool, crate::settings::SaveError> {
    let mut existed = false;
    modify_auth(home, |auth| {
        existed = auth.remove(provider).is_some();
    })?;
    Ok(existed)
}

/// The shared write path. Borrows settings.rs's SaveError so the TUI
/// renders auth and key save failures identically.
fn modify_auth(
    home: Option<&Path>,
    change: impl FnOnce(&mut BTreeMap<String, StoredCredential>),
) -> Result<PathBuf, crate::settings::SaveError> {
    use crate::settings::SaveError;
    use std::io::Write as _;

    let home = home.ok_or(SaveError::NoHome)?;
    let path = auth_path(home);

    // Fresh from disk, not from memory - and a file that no longer
    // parses is refused, never clobbered.
    let mut auth = load_auth_file(&path).map_err(|reason| SaveError::Malformed {
        path: path.clone(),
        reason,
    })?;
    change(&mut auth);

    std::fs::create_dir_all(home).map_err(|e| SaveError::Io {
        path: home.to_path_buf(),
        reason: e.to_string(),
    })?;

    let mut body = serde_json::to_string_pretty(&auth).map_err(|e| SaveError::Io {
        path: path.clone(),
        reason: e.to_string(),
    })?;
    body.push('\n');

    // Same-directory temp file, owner-only BEFORE any token byte lands
    // on disk; rename carries the mode over a pre-existing looser file.
    let tmp = home.join("auth.json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(|e| SaveError::Io {
        path: tmp.clone(),
        reason: e.to_string(),
    })?;
    let written = file
        .write_all(body.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io {
            path: tmp,
            reason: e.to_string(),
        });
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io {
            path: path.clone(),
            reason: e.to_string(),
        });
    }
    Ok(path)
}

/// The refresh decision, pure for tests: inside the margin (or past
/// expiry) means refresh now.
#[must_use]
pub fn needs_refresh(credential: &OAuthCredential, now_ms: u64) -> bool {
    now_ms + REFRESH_MARGIN_MS >= credential.expires
}

/// A request-ready Codex access token: the stored one while it is fresh,
/// or a refreshed (and re-persisted) one. `None` = not logged in or the
/// refresh failed - the provider then errors and the TUI points at
/// /login. This is the coding-agent half of pi's resolveStoredOAuth; it
/// runs on EVERY request via the agent-loop api_key hook, which is what
/// keeps week-long sessions alive across token expiry.
pub async fn openai_codex_access_token(
    home: Option<&Path>,
    http: &reqwest::Client,
) -> Option<String> {
    let stored = credential(home, cupel_core::types::Provider::OPENAI_CODEX)?;
    if !needs_refresh(&stored, cupel_core::types::now_ms()) {
        return Some(stored.access);
    }
    match openai_codex::refresh(http, &stored.refresh).await {
        Ok(fresh) => {
            // Persist the ROTATED pair. A failed save is only a warning:
            // the fresh token still serves this session; the next start
            // refreshes again from the old (still valid) refresh token.
            if let Err(e) = save_credential(home, cupel_core::types::Provider::OPENAI_CODEX, &fresh)
            {
                tracing::warn!("could not persist refreshed codex credential: {e}");
            }
            Some(fresh.access)
        }
        Err(e) => {
            // No silent fallback to a stale token - pi treats a failed
            // refresh as a hard stop too (resolve.ts: "No silent env
            // fallback after a failed refresh").
            tracing::warn!("codex token refresh failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-auth-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn credential_fixture(expires: u64) -> OAuthCredential {
        OAuthCredential {
            access: "access-1".to_string(),
            refresh: "refresh-1".to_string(),
            expires,
            account_id: "acc-1".to_string(),
        }
    }

    #[test]
    fn save_load_delete_round_trip() {
        let home = temp_home("round-trip");
        assert!(!has_credential(Some(&home), "openai-codex"), "empty start");

        let path = save_credential(Some(&home), "openai-codex", &credential_fixture(42)).unwrap();
        assert_eq!(path, home.join("auth.json"));
        let stored = credential(Some(&home), "openai-codex").expect("stored");
        assert_eq!(stored.access, "access-1");
        assert_eq!(stored.expires, 42);
        // Unknown provider and no-home stay empty.
        assert!(credential(Some(&home), "anthropic").is_none());
        assert!(credential(None, "openai-codex").is_none());

        assert!(delete_credential(Some(&home), "openai-codex").unwrap());
        assert!(!has_credential(Some(&home), "openai-codex"));
        // Deleting again reports "was not there" instead of erroring.
        assert!(!delete_credential(Some(&home), "openai-codex").unwrap());
    }

    #[test]
    fn the_file_shape_matches_pis_auth_json() {
        let home = temp_home("shape");
        save_credential(Some(&home), "openai-codex", &credential_fixture(7)).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("auth.json")).unwrap())
                .unwrap();
        let entry = &json["openai-codex"];
        // The tagged-enum + camelCase combination pi's readers expect.
        assert_eq!(entry["type"], "oauth");
        assert_eq!(entry["access"], "access-1");
        assert_eq!(entry["accountId"], "acc-1");
        assert_eq!(entry["expires"], 7);
    }

    #[test]
    fn malformed_files_load_empty_but_refuse_saves() {
        let home = temp_home("malformed");
        std::fs::write(home.join("auth.json"), "{broken").unwrap();
        // Reading fails soft (a broken file must not crash startup)...
        assert!(load_auth(Some(&home)).is_empty());
        // ...but writing refuses to clobber the user's bytes.
        let err = save_credential(Some(&home), "openai-codex", &credential_fixture(1)).unwrap_err();
        assert!(
            matches!(err, crate::settings::SaveError::Malformed { .. }),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            "{broken"
        );
    }

    #[cfg(unix)]
    #[test]
    fn auth_json_is_owner_only_even_over_a_looser_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = temp_home("perms");
        // Pre-existing world-readable file: the rename must tighten it.
        std::fs::write(home.join("auth.json"), "{}").unwrap();
        save_credential(Some(&home), "openai-codex", &credential_fixture(1)).unwrap();
        let mode = std::fs::metadata(home.join("auth.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
    }

    #[test]
    fn refresh_decision_uses_the_five_minute_margin() {
        let now = 1_000_000_000;
        let margin = 5 * 60 * 1000;
        // Comfortably fresh: margin + 1ms of headroom.
        assert!(!needs_refresh(&credential_fixture(now + margin + 1), now));
        // Exactly on the line, inside it, and already expired.
        assert!(needs_refresh(&credential_fixture(now + margin), now));
        assert!(needs_refresh(&credential_fixture(now + 1), now));
        assert!(needs_refresh(&credential_fixture(now - 1), now));
    }

    #[tokio::test]
    async fn access_token_returns_the_stored_token_while_fresh() {
        let home = temp_home("fresh-token");
        let far_future = cupel_core::types::now_ms() + 60 * 60 * 1000;
        save_credential(Some(&home), "openai-codex", &credential_fixture(far_future)).unwrap();
        // Fresh: no network is touched (a refresh would hit
        // auth.openai.com and fail in tests).
        let token = openai_codex_access_token(Some(&home), &reqwest::Client::new()).await;
        assert_eq!(token.as_deref(), Some("access-1"));
        // Not logged in: None, no panic.
        assert!(
            openai_codex_access_token(None, &reqwest::Client::new())
                .await
                .is_none()
        );
    }
}
