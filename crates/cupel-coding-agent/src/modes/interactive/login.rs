//! `/login` background flows.
//!
//! A login WAITS on things the key handler must never wait on: a browser
//! redirect hitting port 1455, or a device-code poll loop. So the flow
//! runs as a spawned task that talks back over a channel - the exact
//! shape of a run's `AgentEventStream`: the `select!` in mod.rs wakes on
//! [`LoginEvent`]s and the App turns them into transcript notices.
//!
//! pi models the same thing as an `AuthInteraction` (notify + prompt);
//! cupel's translation swaps the modal prompt for commands: the browser
//! URL arrives as a notice, and the manual fallback is a second
//! `/login openai-codex <redirect-url>` invocation racing the callback
//! server - the same race pi runs between its server and its paste
//! prompt (openai-codex.ts, `loginOpenAICodex`).

use std::path::PathBuf;

use cupel_core::oauth::openai_codex::{
    self, CallbackServer, DEVICE_VERIFICATION_URI, OAuthCredential, REDIRECT_URI,
};
use tokio_util::sync::CancellationToken;

/// What a login task reports back to the UI.
pub enum LoginEvent {
    /// Progress worth a transcript notice (the URL, the device code, ...).
    Notice(String),
    /// Terminal: the flow finished. Ok carries a success summary.
    Done(Result<String, String>),
}

/// A running login attempt, owned by the App.
pub struct LoginFlow {
    events: tokio::sync::mpsc::UnboundedReceiver<LoginEvent>,
    cancel: CancellationToken,
    /// The manual-paste channel (browser flow only). `take`n on first use.
    manual_code: Option<tokio::sync::oneshot::Sender<String>>,
}

impl LoginFlow {
    /// The next event from the background task; None = channel closed.
    pub async fn next_event(&mut self) -> Option<LoginEvent> {
        self.events.recv().await
    }

    /// Hand a pasted code/redirect to the waiting flow. False when this
    /// flow cannot take one (device flow, or a code already arrived).
    pub fn submit_manual_code(&mut self, input: &str) -> bool {
        match self.manual_code.take() {
            Some(sender) => sender.send(input.to_string()).is_ok(),
            None => false,
        }
    }
}

/// Dropping the flow cancels its task - one place instead of one per
/// exit path (esc, /new login, hot-reload, quit).
impl Drop for LoginFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Spawn the browser flow: authorize URL -> browser -> localhost:1455
/// callback (or a pasted redirect) -> token exchange -> auth.json.
#[must_use]
pub fn spawn_browser(home: Option<PathBuf>) -> LoginFlow {
    let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
    let (code_tx, code_rx) = tokio::sync::oneshot::channel();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            // biased: a cancel that raced the finish line still wins -
            // the user asked to stop, so stop.
            biased;
            () = task_cancel.cancelled() => Err("login cancelled".to_string()),
            result = browser_flow(home, &event_tx, code_rx) => result,
        };
        // The receiver may already be gone (esc dropped the flow) - fine.
        let _ = event_tx.send(LoginEvent::Done(result));
    });
    LoginFlow {
        events,
        cancel,
        manual_code: Some(code_tx),
    }
}

/// Spawn the device-code flow (headless: SSH sessions, containers).
#[must_use]
pub fn spawn_device(home: Option<PathBuf>) -> LoginFlow {
    let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            () = task_cancel.cancelled() => Err("login cancelled".to_string()),
            result = device_flow(home, &event_tx) => result,
        };
        let _ = event_tx.send(LoginEvent::Done(result));
    });
    LoginFlow {
        events,
        cancel,
        manual_code: None,
    }
}

fn notify(events: &tokio::sync::mpsc::UnboundedSender<LoginEvent>, text: impl Into<String>) {
    let _ = events.send(LoginEvent::Notice(text.into()));
}

async fn browser_flow(
    home: Option<PathBuf>,
    events: &tokio::sync::mpsc::UnboundedSender<LoginEvent>,
    code_rx: tokio::sync::oneshot::Receiver<String>,
) -> Result<String, String> {
    let flow = openai_codex::authorization_flow();

    // Bind BEFORE opening the browser, so a fast redirect cannot land on
    // a closed port. A taken port (another Codex-family login?) degrades
    // to paste-only instead of failing the login.
    let server = match CallbackServer::bind().await {
        Ok(server) => Some(server),
        Err(e) => {
            notify(
                events,
                format!(
                    "cannot listen on 127.0.0.1:1455 ({e}) - complete the login in the \
                     browser, then paste the redirect URL: /login openai-codex <url>"
                ),
            );
            None
        }
    };

    if server.is_some() {
        open_browser(&flow.url);
    }
    notify(
        events,
        format!(
            "complete the ChatGPT login in your browser:\n  {}\n(nothing opened? open the \
             URL yourself; redirect cannot load? paste it: /login openai-codex <url>; esc \
             cancels)",
            flow.url
        ),
    );

    // The race pi runs between its callback server and its manual
    // prompt: whichever produces a code first settles the login.
    let code = {
        let served = async {
            match &server {
                Some(server) => server.wait_for_code(&flow.state).await,
                // No server: this branch must never win, only the paste can.
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            served = served => served.map_err(|e| e.to_string())?,
            pasted = code_rx => {
                let input = pasted.map_err(|_dropped| "login cancelled".to_string())?;
                manual_input_to_code(&input, &flow.state)?
            }
        }
    };

    notify(
        events,
        "authorization code received - exchanging for tokens...",
    );
    let http = reqwest::Client::new();
    let credential = openai_codex::exchange_code(&http, &code, &flow.verifier, REDIRECT_URI)
        .await
        .map_err(|e| e.to_string())?;
    finish(home, &credential)
}

async fn device_flow(
    home: Option<PathBuf>,
    events: &tokio::sync::mpsc::UnboundedSender<LoginEvent>,
) -> Result<String, String> {
    let http = reqwest::Client::new();
    let device = openai_codex::start_device_auth(&http)
        .await
        .map_err(|e| e.to_string())?;
    notify(
        events,
        format!(
            "on any device, open {DEVICE_VERIFICATION_URI}\nand enter the code: {}\n\
             (waiting up to 15 minutes; esc cancels)",
            device.user_code
        ),
    );
    // poll_device_auth polls until the user finishes, then exchanges the
    // code (the verifier comes back from the server in this flow).
    let credential = openai_codex::poll_device_auth(&http, &device)
        .await
        .map_err(|e| e.to_string())?;
    finish(home, &credential)
}

/// Parse a pasted redirect/code and hold it against THIS flow's state -
/// pure so tests can pin the acceptance rules without a browser.
fn manual_input_to_code(input: &str, state: &str) -> Result<String, String> {
    let (code, pasted_state) = openai_codex::parse_authorization_input(input);
    if let Some(pasted_state) = pasted_state
        && pasted_state != state
    {
        // A stale paste from an EARLIER attempt would exchange fine but
        // bind the wrong PKCE verifier - reject it up front.
        return Err("state mismatch - paste the redirect of THIS login attempt".to_string());
    }
    code.ok_or_else(|| "no authorization code in the pasted input".to_string())
}

/// Persist and summarize. Saving is the login's LAST step: a credential
/// that never reaches auth.json is a login the next session forgets.
fn finish(home: Option<PathBuf>, credential: &OAuthCredential) -> Result<String, String> {
    match crate::auth::save_credential(
        home.as_deref(),
        cupel_core::types::Provider::OPENAI_CODEX,
        credential,
    ) {
        Ok(path) => Ok(format!(
            "logged in with ChatGPT (account {}) - saved to {}",
            credential.account_id,
            path.display()
        )),
        Err(e) => Err(format!("login succeeded but could not be saved: {e}")),
    }
}

/// Open a URL in the platform browser - pi's open-browser.ts, ported:
/// never through a shell (cmd.exe re-parses metacharacters, which would
/// make URLs injectable), always detached, always best-effort (the
/// notice above shows the URL either way).
fn open_browser(url: &str) {
    let (command, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(windows) {
        ("rundll32", vec!["url.dll,FileProtocolHandler", url])
    } else {
        ("xdg-open", vec![url])
    };
    let _ = std::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Test-only flow around dummy channels - lets App tests exercise the
/// esc/paste/event paths without a server, browser, or network.
#[cfg(test)]
pub(crate) fn stub_flow() -> (
    LoginFlow,
    tokio::sync::mpsc::UnboundedSender<LoginEvent>,
    CancellationToken,
    tokio::sync::oneshot::Receiver<String>,
) {
    let (event_tx, events) = tokio::sync::mpsc::unbounded_channel();
    // The receiver is handed OUT: a dropped receiver would make every
    // paste fail as "already used" (oneshot send errors then).
    let (code_tx, code_rx) = tokio::sync::oneshot::channel();
    let cancel = CancellationToken::new();
    let flow = LoginFlow {
        events,
        cancel: cancel.clone(),
        manual_code: Some(code_tx),
    };
    (flow, event_tx, cancel, code_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_input_enforces_the_state_check() {
        // Right state (or none at all - a bare code) passes...
        assert_eq!(
            manual_input_to_code("http://localhost:1455/auth/callback?code=c1&state=s1", "s1"),
            Ok("c1".to_string())
        );
        assert_eq!(
            manual_input_to_code("bare-code", "s1"),
            Ok("bare-code".to_string())
        );
        // ...a foreign state or a codeless paste is refused.
        assert!(
            manual_input_to_code(
                "http://localhost:1455/auth/callback?code=c1&state=EVIL",
                "s1"
            )
            .unwrap_err()
            .contains("state mismatch")
        );
        assert!(manual_input_to_code("", "s1").is_err());
    }

    #[tokio::test]
    async fn manual_code_channel_is_single_use() {
        // A LoginFlow shell around dummy channels - no server, no
        // browser, no network; only the paste state machine.
        let (_event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let (code_tx, mut code_rx) = tokio::sync::oneshot::channel();
        let mut flow = LoginFlow {
            events,
            cancel: CancellationToken::new(),
            manual_code: Some(code_tx),
        };
        assert!(flow.submit_manual_code("first"), "first paste is accepted");
        assert_eq!(code_rx.try_recv().unwrap(), "first");
        assert!(!flow.submit_manual_code("second"), "channel is spent");

        // The device flow never takes a paste.
        let (_event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let mut device = LoginFlow {
            events,
            cancel: CancellationToken::new(),
            manual_code: None,
        };
        assert!(!device.submit_manual_code("anything"));
    }

    #[tokio::test]
    async fn dropping_the_flow_cancels_its_task() {
        let (_event_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let observed = cancel.clone();
        let flow = LoginFlow {
            events,
            cancel,
            manual_code: None,
        };
        assert!(!observed.is_cancelled());
        drop(flow);
        // This is the one line that makes esc, /login-again, hot-reload,
        // and quit all stop the background task.
        assert!(observed.is_cancelled());
    }
}
