//! OpenAI Codex OAuth flow - login with a ChatGPT Plus/Pro
//! subscription instead of an API key.
//!
//! The flow is the one the official Codex CLI ships:
//! an RFC-standard authorization-code flow with PKCE against
//! `auth.openai.com`, using Codex's PUBLIC client id.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::oauth::pkce;
use crate::types::now_ms;

/// Codex CLI's public OAuth client id.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The redirect the browser flow registers; port 1455 is Codex's fixed,
/// allowlosted callback port.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
/// The JWT claim namespace OpenAI parks its account metadata under.
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Who is asking, sent in the authorize URL and on every backend
/// request.
pub const ORIGINATOR: &str = "cupel";

/// A ChatGPT OAuth credential. `OAuthCredential` and the exact JSON
/// shape stored in auth.json (plus the storage layer's `type` tag).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthCredential {
    /// Short-lived bearer token (a JWT); THIS is what requests send.
    pub access: String,
    /// Long-lived token that mints new access tokens.
    pub refresh: String,
    /// Unix ms when `access` expires (mint time + the server's
    /// `expires_in`).
    pub expires: u64,
    /// The ChatGPT account id extracted from the access token's claim;
    /// the backend wants it back as the `chatgpt-account-id` header.
    pub account_id: String,
}

/// Manual Debug that never prints token values: `{:?}`output reaches logs
/// and failed-asseration messages, secrets must not.
impl core::fmt::Debug for OAuthCredential {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OAuthCredential")
            .field("access", &format_args!("<{} chars>", self.access.len()))
            .field("refresh", &format_args!("<{} chars>", self.refresh.len()))
            .field("expires", &self.expires)
            .field("account_id", &self.account_id)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("token {operation} failed (HTTP {status}): {body}")]
    TokenStatus {
        operation: &'static str,
        status: u16,
        body: String,
    },
    #[error("token {operation} response is missing fields")]
    MalformedToken { operation: &'static str },
    #[error("access token carries no chatgpt_account_id claim")]
    MissingAccountId,
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

/// Everything one browser-login attempt needs to remember: the PKCE
/// verifier and state stay LOCAL; only the URL leaves the process.
pub struct AuthorizationFlow {
    pub verifier: String,
    pub state: String,
    pub url: String,
}

/// Start a browser login: fresh PKCE pair, fresh CSRF state, and the
/// authorize URL to open.
#[must_use]
pub fn authorization_flow() -> AuthorizationFlow {
    let pkce = pkce::generate();
    let state = random_state();
    let url = build_authorize_url(&pkce.challenge, &state);
    AuthorizationFlow {
        verifier: pkce.verifier,
        state,
        url,
    }
}

/// 16 random bytes as hex `state` parameter that redirect back
/// to THIS login attempt (CSRF protection: `createState`).
fn random_state() -> String {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).expect("OS entropy source unavailable");
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn build_authorize_url(challenge: &str, state: &str) -> String {
    // reqwest re-exports the `url` crate; query_pairs_mut percent-encodes
    // every value correctly.
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("static URL parses");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    url.to_string()
}

/// Redeem an authorization code for a credential. `redirect_uri` must repeat the
/// one from the authorize step. The browser flow and the device flow redeem against
/// different URIs.
pub async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredential, OAuthError> {
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];
    // .from() sets application/x-www-form-urlencoded. The token endpoint
    // speaks forms, not JSON.
    let response = http.post(TOKEN_URL).form(&form).send().await?;
    read_token_response(response, "exchange").await
}

/// Mint a fresh access token from the refresh token. The response rotates BOTK
/// tokens.
pub async fn refresh(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthCredential, OAuthError> {
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
    ];
    let response = http.post(TOKEN_URL).form(&form).send().await?;
    read_token_response(response, "refresh").await
}

async fn read_token_response(
    response: reqwest::Response,
    operation: &'static str,
) -> Result<OAuthCredential, OAuthError> {
    let status = response.status();
    if !status.is_success() {
        return Err(OAuthError::TokenStatus {
            operation,
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        });
    }
    let json: Value = response.json().await?;
    credential_from_json(&json, operation)
}

/// The pure half of the token handling, split from the HTTP so tests can
/// drive it with fixtures.
fn credential_from_json(
    json: &Value,
    operation: &'static str,
) -> Result<OAuthCredential, OAuthError> {
    let access = json.get("access_token").and_then(Value::as_str);
    let refresh = json.get("refresh_token").and_then(Value::as_str);
    let expires_in = json.get("expires_in").and_then(Value::as_u64);
    let (Some(access), Some(refresh), Some(expires_in)) = (access, refresh, expires_in) else {
        return Err(OAuthError::MalformedToken { operation });
    };
    let account_id = account_id_from_access_token(access).ok_or(OAuthError::MissingAccountId)?;
    Ok(OAuthCredential {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: now_ms() + expires_in * 1000,
        account_id,
    })
}

/// The ChatGPT account id baked into an access token. Used at login time to fill the credential
/// AND by the provider on every request to build the `chatgpt-account-id` header.
#[must_use]
pub fn account_id_from_access_token(access: &str) -> Option<String> {
    decode_jwt_payload(access)?
        .get(JWT_AUTH_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Decode a JWT's payload WITHOUT verifying the signature. Verification
/// is the server's job; the client only reads a routing claim out of a
/// token if just received over TLS.
fn decode_jwt_payload(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let parts: Vec<&str> = token.split('.').collect();
    // header.payload.signature.
    if parts.len() != 3 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub const CALLBACK_ADDR: &str = "127.0.0.1:1455";

/// The page shown in the browser after the redirect.
fn callback_page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>cupel - {title}</title></head>\
         <body style=\"font-family: system-ui; margin: 4rem\">\
         <h1>{title}</h1><p>{detail}</p></body></html>"
    )
}

/// One-shot local HTTP endpoing for the OAuth redirect.
pub struct CallbackServer {
    listener: tokio::net::TcpListener,
}

impl CallbackServer {
    /// Bind the fixed Codex port. Fails when another login (or another
    /// Codex-family tool) is already listening.
    pub async fn bind() -> std::io::Result<Self> {
        Self::bind_addr(CALLBACK_ADDR).await
    }

    /// Test seam: bind an arbitrary address (tests use port 0).
    pub async fn bind_addr(addr: &str) -> std::io::Result<Self> {
        Ok(Self {
            listener: tokio::net::TcpListener::bind(addr).await?,
        })
    }

    /// The actually-bound address (tests bind port 0 and need the real one).
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Serve until a redirect with the RIGHT state and a code arrives;
    /// return that code. Wrong paths (favicon probes), state mismatches,
    /// and codeless callbacks are answered with an error page and the
    /// server keeps waiting. Cancellation is the caller's: select! against
    /// this future and drop it.
    pub async fn wait_for_code(&self, state: &str) -> Result<String, OAuthError> {
        loop {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| OAuthError::Other(format!("callback accept: {e}")))?;
            if let Some(code) = handle_connection(&mut stream, state).await {
                return Ok(code);
            }
        }
    }
}

/// Read one HTTP request head, answer it, and return the code when this
/// was the valid callback. Every malformed/foreign request returns None.
async fn handle_connection(stream: &mut tokio::net::TcpStream, state: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // Read until the blank line that ends the request head, capped at
    // 8 KiB.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > 8 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();

    // "GET /auth/callback?code=...&state=... HTTP/1.1"
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let (status, page, code) = evaluate_callback(target, state);

    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
        content-length: {}\r\nconnection: close\r\n\r\n{page}",
        page.len()
    );
    // Best effort: the browser closing early must not kill the login.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    code
}

/// Position one past the `\r\n\r\n` that ends a request head.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// The routing decision, pure and etstable.
fn evaluate_callback(target: &str, state: &str) -> (&'static str, String, Option<String>) {
    // The target is origin-relative; give Url::parse a base to hang it on.
    let parsed = reqwest::Url::parse(&format!("http://localhost{target}"));
    let Ok(url) = parsed else {
        return (
            "400 Bad Request",
            callback_page("Login failed", "Malformed request."),
            None,
        );
    };
    if url.path() != "/auth/callback" {
        return (
            "404 Not Found",
            callback_page("Not found", "Callback route not found."),
            None,
        );
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if param("state").as_deref() != Some(state) {
        return (
            "400 Bad Request",
            callback_page("Login failed", "State mismatch."),
            None,
        );
    }
    match param("code") {
        Some(code) if !code.is_empty() => (
            "200 OK",
            callback_page(
                "Login complete",
                "OpenAI authentication completed. You can close this window and return to cupel.",
            ),
            Some(code),
        ),
        _ => (
            "400 Bad Request",
            callback_page("Login failed", "Missing authorization code."),
            None,
        ),
    }
}

/// Parse whatever the user pasted as a manual fallback: the full redirect
/// URL, a raw `code=...&state=...` query, `code#state`, or the bare code.
/// Returns (code, state).
#[must_use]
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Ok(url) = reqwest::Url::parse(value) {
        let param = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, v)| v.into_owned())
        };
        return (param("code"), param("state"));
    }
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_string()), Some(state.to_string()));
    }
    if value.contains("code=") {
        let query = reqwest::Url::parse(&format!("http://localhost/?{value}"));
        if let Ok(url) = query {
            let param = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, v)| v.into_owned())
            };
            return (param("code"), param("state"));
        }
    }
    (Some(value.to_string()), None)
}

const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// Where the user types the code in (shown in the TUI notice).
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
/// The device flow redeems its code against this server-side redirect.
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(15);
/// RFC 8628: no server interval means poll every 5 seconds.
const DEVICE_DEFAULT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
/// ...and every `slow_down`answer stretches the interval by 5 seconds.
const DEVICE_SLOW_DOWN_INCREMENT: std::time::Duration = std::time::Duration::from_secs(5);

/// A started device authorization: show `user_code` to the user, then poll.
#[derive(Debug)]
pub struct DeviceAuth {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: std::time::Duration,
}

/// Ask the auth server for a user code.
pub async fn start_device_auth(http: &reqwest::Client) -> Result<DeviceAuth, OAuthError> {
    let response = http
        .post(DEVICE_USER_CODE_URL)
        .json(&serde_json::json!({"client_id": CLIENT_ID}))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(OAuthError::Other(format!(
            "device code request failed with status {}",
            status.as_u16()
        )));
    }
    let json: Value = response.json().await?;
    let device_auth_id = json.get("device_auth_id").and_then(Value::as_str);
    let user_code = json.get("user_code").and_then(Value::as_str);
    let interval_seconds = match json.get("interval") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    let (Some(device_auth_id), Some(user_code)) = (device_auth_id, user_code) else {
        return Err(OAuthError::Other(
            "invalid device code response".to_string(),
        ));
    };
    Ok(DeviceAuth {
        device_auth_id: device_auth_id.to_string(),
        user_code: user_code.to_string(),
        interval: interval_seconds.map_or(DEVICE_DEFAULT_INTERVAL, std::time::Duration::from_secs),
    })
}

#[derive(Debug, PartialEq, Eq)]
enum DevicePoll {
    /// The user has not finished logging on in the browser.
    Pending,
    /// RFC 8628 back-pressure.
    SlowDown,
    /// The code arrived, PLUS the verifier for the exchange.
    Complete {
        code: String,
        verifier: String,
    },
    Failed(String),
}

/// OpenAI's device endpoint signals "not yet" in three shapes: plain 403
/// or 404, and a JSON error code. `error` may be a bare string or an object
/// with `code`.
fn classify_device_poll(status: u16, body: &str) -> DevicePoll {
    if status == 200 {
        let json: Value = match serde_json::from_str(body) {
            Ok(json) => json,
            Err(_) => return DevicePoll::Failed("invalid device auth token response".to_string()),
        };
        let code = json.get("authorization_code").and_then(Value::as_str);
        let verifier = json.get("code_verifier").and_then(Value::as_str);
        return match (code, verifier) {
            (Some(code), Some(verifier)) => DevicePoll::Complete {
                code: code.to_string(),
                verifier: verifier.to_string(),
            },
            _ => DevicePoll::Failed("invalid device auth token response".to_string()),
        };
    }
    if status == 403 || status == 404 {
        return DevicePoll::Pending;
    }
    let error_code = serde_json::from_str::<Value>(body).ok().and_then(|json| {
        let error = json.get("error")?.clone();
        match error {
            Value::String(code) => Some(code),
            Value::Object(map) => map.get("code").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }
    });
    match error_code.as_deref() {
        Some("deviceauth_authorization_pending") => DevicePoll::Pending,
        Some("slow_down") => DevicePoll::SlowDown,
        _ => DevicePoll::Failed(format!("device auth failed with status {status}")),
    }
}

/// Poll untile the user finishes in the browser, then exchange the code.
/// The DEVICE flow's PKCE verifier comes back FROM the server.
pub async fn poll_device_auth(
    http: &reqwest::Client,
    device: &DeviceAuth,
) -> Result<OAuthCredential, OAuthError> {
    let deadline = tokio::time::Instant::now() + DEVICE_CODE_TIMEOUT;
    let mut interval = device.interval.max(std::time::Duration::from_secs(1));

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(OAuthError::Other("device flow timed out".to_string()));
        }
        let response = http
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .send()
            .await?;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        match classify_device_poll(status, &body) {
            DevicePoll::Complete { code, verifier } => {
                return exchange_code(http, &code, &verifier, DEVICE_REDIRECT_URI).await;
            }
            DevicePoll::Failed(message) => return Err(OAuthError::Other(message)),
            DevicePoll::SlowDown => interval += DEVICE_SLOW_DOWN_INCREMENT,
            DevicePoll::Pending => {}
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an unsigned JWT-shaped token around `payload` - three
    /// base64url segments; the signature is junk because nothing here
    /// verifies it.
    pub(crate) fn fake_jwt(payload: &Value) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.{}",
            b64.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
            b64.encode(payload.to_string()),
            b64.encode("junk-signature")
        )
    }

    pub(crate) fn access_token_for_account(account_id: &str) -> String {
        fake_jwt(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        }))
    }

    #[test]
    fn authorize_url_carries_the_codex_parameters() {
        let flow = authorization_flow();
        let url = reqwest::Url::parse(&flow.url).expect("flow URL parses");
        assert_eq!(url.host_str(), Some("auth.openai.com"));
        assert_eq!(url.path(), "/oauth/authorize");

        let params: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["redirect_uri"], REDIRECT_URI);
        assert_eq!(params["scope"], SCOPE, "spaces must survive the encoding");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], flow.state);
        assert_eq!(params["codex_cli_simplified_flow"], "true");
        assert_eq!(params["id_token_add_organizations"], "true");
        assert_eq!(params["originator"], "cupel");
        // The challenge derives from the verifier the flow kept local.
        assert_eq!(
            params["code_challenge"],
            crate::oauth::pkce::challenge_for(&flow.verifier)
        );
    }

    #[test]
    fn state_is_fresh_hex_per_flow() {
        let a = authorization_flow();
        let b = authorization_flow();
        assert_eq!(a.state.len(), 32, "16 bytes -> 32 hex chars");
        assert!(a.state.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.state, b.state);
    }

    #[test]
    fn account_id_reads_the_nested_claim() {
        let token = access_token_for_account("acc-123");
        assert_eq!(
            account_id_from_access_token(&token).as_deref(),
            Some("acc-123")
        );

        // Missing claim, empty id, and non-JWT shapes all yield None.
        assert_eq!(
            account_id_from_access_token(&fake_jwt(&serde_json::json!({}))),
            None
        );
        assert_eq!(
            account_id_from_access_token(&access_token_for_account("")),
            None
        );
        assert_eq!(account_id_from_access_token("not-a-jwt"), None);
        assert_eq!(account_id_from_access_token("a.b"), None);
    }

    #[test]
    fn credential_from_json_maps_and_stamps_expiry() {
        let json = serde_json::json!({
            "access_token": access_token_for_account("acc-9"),
            "refresh_token": "refresh-1",
            "expires_in": 3600,
            "id_token": "ignored"
        });
        let before = now_ms();
        let credential = credential_from_json(&json, "exchange").expect("parses");
        assert_eq!(credential.refresh, "refresh-1");
        assert_eq!(credential.account_id, "acc-9");
        // expires = now + 3600s, within the test's own runtime jitter.
        assert!(credential.expires >= before + 3_600_000);
        assert!(credential.expires <= now_ms() + 3_600_000);
    }

    #[test]
    fn malformed_token_responses_name_the_operation_without_leaking() {
        let json = serde_json::json!({"access_token": "only-half"});
        let error = credential_from_json(&json, "refresh").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("refresh"), "{message}");
        assert!(
            !message.contains("only-half"),
            "no secrets in errors: {message}"
        );

        // A well-formed response whose access token carries no account
        // claim is ALSO rejected - the header cannot be built without it.
        let json = serde_json::json!({
            "access_token": fake_jwt(&serde_json::json!({"sub": "x"})),
            "refresh_token": "r",
            "expires_in": 60
        });
        assert!(matches!(
            credential_from_json(&json, "exchange").unwrap_err(),
            OAuthError::MissingAccountId
        ));
    }

    #[test]
    fn debug_never_prints_token_values() {
        let credential = OAuthCredential {
            access: "secret-access".to_string(),
            refresh: "secret-refresh".to_string(),
            expires: 1,
            account_id: "acc-1".to_string(),
        };
        let rendered = format!("{credential:?}");
        assert!(!rendered.contains("secret"), "token leaked: {rendered}");
        assert!(rendered.contains("acc-1"), "{rendered}");
    }

    #[test]
    fn credential_serde_matches_the_camel_case_wire_shape() {
        let credential = OAuthCredential {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires: 5,
            account_id: "acc".to_string(),
        };
        let json = serde_json::to_value(&credential).expect("serializes");
        // accountId camelCase - the auth.json shape pi writes too.
        assert_eq!(json["accountId"], "acc");
        let back: OAuthCredential = serde_json::from_value(json).expect("parses");
        assert_eq!(back, credential);
    }

    #[test]
    fn evaluate_callback_routes_like_pi() {
        // Success: right path, right state, a code.
        let (status, page, code) = evaluate_callback("/auth/callback?code=c-1&state=s-1", "s-1");
        assert_eq!(status, "200 OK");
        assert!(page.contains("Login complete"), "{page}");
        assert_eq!(code.as_deref(), Some("c-1"));

        // Foreign route (favicon probes), wrong state, missing code.
        assert_eq!(evaluate_callback("/favicon.ico", "s-1").0, "404 Not Found");
        let (status, _, code) = evaluate_callback("/auth/callback?code=c&state=WRONG", "s-1");
        assert_eq!(status, "400 Bad Request");
        assert!(code.is_none());
        let (status, ..) = evaluate_callback("/auth/callback?state=s-1", "s-1");
        assert_eq!(status, "400 Bad Request");
    }

    #[tokio::test]
    async fn callback_server_waits_through_noise_and_returns_the_code() {
        let server = CallbackServer::bind_addr("127.0.0.1:0")
            .await
            .expect("binds");
        let port = server.local_addr().expect("addr").port();

        let wait = tokio::spawn(async move { server.wait_for_code("state-1").await });

        let http = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        // Noise first: wrong route, then wrong state - the server answers
        // both AND keeps waiting.
        let response = http
            .get(format!("{base}/favicon.ico"))
            .send()
            .await
            .expect("sends");
        assert_eq!(response.status().as_u16(), 404);
        let response = http
            .get(format!("{base}/auth/callback?code=x&state=evil"))
            .send()
            .await
            .expect("sends");
        assert_eq!(response.status().as_u16(), 400);

        // The real redirect settles it.
        let response = http
            .get(format!("{base}/auth/callback?code=the-code&state=state-1"))
            .send()
            .await
            .expect("sends");
        assert_eq!(response.status().as_u16(), 200);
        assert!(
            response
                .text()
                .await
                .expect("body")
                .contains("Login complete")
        );
        assert_eq!(wait.await.expect("join").expect("code"), "the-code");
    }

    #[test]
    fn parse_authorization_input_accepts_every_paste_shape() {
        // Full redirect URL...
        let (code, state) =
            parse_authorization_input("http://localhost:1455/auth/callback?code=c-2&state=s-2");
        assert_eq!(code.as_deref(), Some("c-2"));
        assert_eq!(state.as_deref(), Some("s-2"));
        // ...raw query string...
        let (code, state) = parse_authorization_input("code=c-3&state=s-3");
        assert_eq!(code.as_deref(), Some("c-3"));
        assert_eq!(state.as_deref(), Some("s-3"));
        // ...code#state...
        let (code, state) = parse_authorization_input("c-4#s-4");
        assert_eq!(code.as_deref(), Some("c-4"));
        assert_eq!(state.as_deref(), Some("s-4"));
        // ...bare code (no state to check), and nothing at all.
        assert_eq!(
            parse_authorization_input("  c-5  "),
            (Some("c-5".to_string()), None)
        );
        assert_eq!(parse_authorization_input(""), (None, None));
    }

    #[test]
    fn device_poll_classification_covers_openais_quirks() {
        // 200 with both halves: done (the verifier comes FROM the server).
        assert_eq!(
            classify_device_poll(200, r#"{"authorization_code": "c", "code_verifier": "v"}"#),
            DevicePoll::Complete {
                code: "c".to_string(),
                verifier: "v".to_string()
            }
        );
        // Plain 403/404 mean "user not done yet", not failure.
        assert_eq!(classify_device_poll(403, ""), DevicePoll::Pending);
        assert_eq!(classify_device_poll(404, "not found"), DevicePoll::Pending);
        // The JSON error code variants: bare string and nested object.
        assert_eq!(
            classify_device_poll(400, r#"{"error": "deviceauth_authorization_pending"}"#),
            DevicePoll::Pending
        );
        assert_eq!(
            classify_device_poll(429, r#"{"error": {"code": "slow_down"}}"#),
            DevicePoll::SlowDown
        );
        // Everything else fails loudly with the status.
        assert!(matches!(
            classify_device_poll(500, "boom"),
            DevicePoll::Failed(message) if message.contains("500")
        ));
        // A 200 with missing halves is a protocol error, not success.
        assert!(matches!(
            classify_device_poll(200, r#"{"authorization_code": "only"}"#),
            DevicePoll::Failed(_)
        ));
    }
}
