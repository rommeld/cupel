//! Interactive mode: the ratatui frontend. [`run`] is the entry point; the
//! `cupel` binary (`main.rs`) calls it after building the agent with
//! `cupel_coding_agent`.
//!
//! ## Event architecture
//!
//! Terminal input, agent events and termination signals feed the event loop:
//!
//! ```text
//!  crossterm (blocking thread) ──channel──▶            ┌── ui::render
//!                                          tokio::select ──▶ App ──┘
//!  AgentEventStream (active run) ─────────▶
//!  TerminationSignals ──────────────────▶
//! ```
//!
//! Terminal input is read on a dedicated OS thread because crossterm's
//! `read()` is blocking; a channel bridges it into the async world. Agent
//! events already arrive as a `Stream`. After each `select!` wakeup, the
//! loop handles the ready background events before redrawing, so a burst
//! of streaming deltas shares one render pass.

pub mod app;
pub mod autocomplete;
pub mod fuzzy;
pub mod input;
pub mod login;
pub mod markdown;
pub mod project_trust;
pub mod sessions;
mod terminal_text;
pub mod theme;
pub mod transcript;
pub mod ui;

use cupel_agent::Agent;
use cupel_coding_agent::modes::{SessionMeta, TerminationSignals};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;

/// Run the interactive session until the user quits.
///
/// Errors are I/O failures; agent failures surface inside the UI.
pub async fn run(
    agent: Agent,
    meta: SessionMeta,
    recorder: cupel_coding_agent::session::SessionRecorder,
) -> std::io::Result<()> {
    // Register before entering raw mode so termination always takes the restore path.
    let signals = TerminationSignals::new()?;
    // `ratatui::init` enters raw mode + the alternate screen and installs a
    // panic hook that restores the terminal without that, a panic would
    // leave the user's shell in raw mode (no echo, no line editing).
    let mut terminal = ratatui::init();
    // Mouse capture (wheel-scrolling) and bracketed paste are opt-in and not
    // covered by ratatui's init/restore or its panic hook. Both must be
    // released on every exit path: a terminal left in mouse mode swallows
    // normal wheel scrolling and text selection even after cupel exits. The
    // panic hook is chained so the release runs before ratatui's restore.
    //
    // Bracketed paste makes a terminal paste arrive as one Event::Paste
    // instead of a stream of key presses without it, every newline in the
    // pasted text would hit the Enter handler and submit a partial prompt.
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // Keyboard enhancement (the kitty keyboard protocol) makes the terminal report
    // modified keys as unambiguous escape codes, so Ctrl+Enter arrives as Ctrl+Enter
    // instead of plain Enter. Terminals without the protocol ignore the request. It
    // gets its own `execute!` because it fails on Windows, and a failed command would
    // skip the ones after it.
    let _ = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste
        );
        ratatui_hook(info);
    }));
    let result = event_loop(&mut terminal, agent, meta, recorder, signals).await;
    // Pop what was pushed, before `restore` leaves the alternate screen it was
    // pushed on.
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    // A hung-up terminal can reject restoration. Avoid restore()'s stderr
    // diagnostic, which can itself panic when stderr was that terminal too.
    let _ = ratatui::try_restore();
    result
}

/// Bridge crossterm's blocking `read()` into an async channel.
///
/// The reader thread parks in `read()` forever; when the app quits we simply
/// drop the receiver and let the thread die with the process. A shutdown
/// handshake would need `poll()` with a timeout complexity that buys
/// nothing for a process about to exit.
fn spawn_input_thread() -> tokio::sync::mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        // Runs until read() errors or the receiver is dropped.
        while let Ok(event) = ratatui::crossterm::event::read() {
            if tx.send(event).is_err() {
                break; // Receiver dropped: the UI is gone.
            }
        }
    });
    rx
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    agent: Agent,
    meta: SessionMeta,
    recorder: cupel_coding_agent::session::SessionRecorder,
    mut signals: TerminationSignals,
) -> std::io::Result<()> {
    let mut sessions = sessions::Sessions::new(app::App::new(agent, meta, recorder));
    sessions.restore().await;
    let mut terminal_events = spawn_input_thread();
    // The spinner's clock lives outside the loop.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result = loop {
        if let Err(error) = terminal.draw(|frame| ui::render_sessions(frame, &mut sessions)) {
            break Err(error);
        }

        // Wait for whichever source has something first. `next_agent_event`
        // parks forever while idle, so this never busy-spins.
        tokio::select! {
            // External termination must not wait for the busy-spinoff quit confirmation.
            _ = signals.recv() => break Ok(()),
            event = terminal_events.recv() => {
                match event {
                    Some(event) => sessions.on_terminal_event(event),
                    None => break Ok(()), // Input thread died; nothing left to do.
                }
            }
            // Agent events and login events share one wakeup: two
            // `&mut app` futures cannot sit in the same select!, so the
            // App multiplexes them itself (App::next_event).
            (index, event) = sessions.next_event() => {
                sessions.list[index].app.on_event(event).await;
            }
            _ = ticker.tick(), if sessions.any_running() => {
                sessions.tick();
            }
        }

        // Catch up without waiting: rendering the entire transcript per queued
        // delta would make a fast stream fall behind the producer.
        sessions.drain_ready_events().await;

        // A run ended somewhere. Check for conflicts between the checkouts.
        if sessions.take_finished_runs() {
            sessions.check_conflicts().await;
        }

        // Keys only reach the session on screen, so the requests below always come
        // from there.
        let app = sessions.active_mut();

        // Ctrl+O queued a copy: emit it as OSC 52 the "set clipboard"
        // escape sequence straight to stdout. It paints nothing, so
        // ratatui's frame diff never notices; the terminal (not cupel)
        // performs the actual clipboard write, which is why this works
        // across SSH sessions too.
        if let Some(text) = app.pending_copy.take() {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            let _ = out.write_all(osc52(&text).as_bytes());
            let _ = out.flush();
        }

        // Ctrl+Y requested a selection-mode toggle: release the mouse so
        // the terminal can select/copy text natively, or recapture it for
        // wheel scrolling. The command goes to the terminal first; state
        // (and the user-facing notice) only flips when it succeeded.
        if app.mouse_toggle_requested {
            let command = if app.mouse_captured {
                execute!(std::io::stdout(), DisableMouseCapture)
            } else {
                execute!(std::io::stdout(), EnableMouseCapture)
            };
            if command.is_ok() {
                app.apply_mouse_toggle();
            } else {
                app.mouse_toggle_requested = false;
            }
        }

        // /new or /hot-reload requested a rebuild: the loader re-reads every .cupel
        // layer, so this must run here in async context. The old App is consumed and
        // its replacement rebound in place.
        if let Some(target) = app.pending_reload.take() {
            sessions.reload(target).await;
        }
        if let Some(command) = sessions.active_mut().pending_spinoff.take() {
            sessions.spinoff(command).await;
        }
        let app = sessions.active_mut();

        // A prompt accepted by the (synchronous) key handler starts here:
        // the prompt-path hooks are awaited first, so a pending `stop` hook
        // from the previous run is guaranteed to have finished.
        if let Some(prompt) = app.pending_prompt.take() {
            app.recorder.before_prompt(&prompt).await;
            app.start_run(&prompt);
        }

        if sessions.quit_requested() {
            break Ok(());
        }
    };
    // Don't leave a run mid-flight, even if drawing failed after a terminal hangup.
    // Abort and let each session settle so the terminal restore doesn't race provider
    // output. Then drain the hook chains and announce session-end.
    sessions.shutdown().await;
    result
}

/// The OSC 52 "set clipboard" sequence for `text`.
///
/// Shape: `ESC ] 52 ; c ; <base64> BEL`. 52 is the clipboard opcode, `c`
/// selects the system clipboard (not the X11 primary selection), and the
/// payload travels base64-encoded because clipboard text may contain any
/// byte including the BEL that would otherwise end the sequence early.
fn osc52(text: &str) -> String {
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{payload}\x07")
}

#[cfg(test)]
mod tests {
    use super::osc52;

    #[test]
    fn osc52_wraps_the_text_in_the_clipboard_sequence() {
        // "hi" is "aGk=" in base64; ESC ] opens the OSC, BEL closes it.
        assert_eq!(osc52("hi"), "\u{1b}]52;c;aGk=\u{7}");
    }
}
