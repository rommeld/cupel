//! Interactive mode, the ratatui frontend. [`run`] is the entry point, the
//! `cupel` binary (`main.rs`) calls it after building the agent with
//! `cupel_coding_agent`.
//!
//! ## Event architecture
//!
//! Terminal input, agent events, update events  and termination signals feed the event
//! loop,
//!
//! ```text
//!  crossterm (blocking thread) ──channel──▶            ┌── render
//!                                          select ──▶ App ──┘
//!  AgentEventStream (active run) ─────────▶
//!  Updates (startup check, /update) ───────────────▶
//!  TerminationSignals ──────────────────▶
//! ```
//!
//! Terminal input is read on a dedicated OS thread because crossterm's
//! `read()` is blocking, a channel bridges it into the async world. Agent
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
mod text_selection;
pub mod theme;
mod token_count;
pub mod transcript;
pub mod ui;
pub mod updates;
pub mod usage;

#[cfg(test)]
mod reasoning_tests;
#[cfg(test)]
mod undo_tests;
#[cfg(test)]
mod usage_tests;

use cupel_agent::Agent;
use cupel_coding_agent::modes::{SessionMeta, TerminationSignals};
use cupel_coding_agent::program_status::Reporter;
use cupel_coding_agent::tools::ask_user::QuestionReceiver;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind, KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;

/// How the interactive session ended.
pub enum Exit {
    Quit,
    Restart(sessions::Restart),
}

/// Run the interactive session until the user quits, or until `/update` asks for a
/// restart onto a new release. `questions` is where the agent's `ask_user` tool
/// sends its questions.
///
/// Errors are I/O failures, agent failures surface inside the UI.
pub async fn run(
    agent: Agent,
    meta: SessionMeta,
    recorder: cupel_coding_agent::session::SessionRecorder,
    questions: Option<QuestionReceiver>,
) -> std::io::Result<Exit> {
    // Register before entering raw mode so termination always takes the restore path.
    let signals = TerminationSignals::new()?;
    // `init` enters raw mode + the alternate screen and installs a
    // panic hook that restores the terminal without that, a panic would
    // leave the user's shell in raw mode (no echo, no line editing).
    let mut terminal = ratatui::init();
    // Mouse capture (wheel scrolling) and bracketed paste are opt in and not
    // covered by ratatui's init/restore or its panic hook. Both must be
    // released on every exit path, a terminal left in mouse mode swallows
    // normal wheel scrolling and text selection even after cupel exits. The
    // panic hook is chained so the release runs before ratatui's restore.
    //
    // Bracketed paste makes a terminal paste arrive as one Paste
    // instead of a stream of key presses without it, every newline in the
    // pasted text would hit the Enter handler and submit a partial prompt.
    let _ = execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    // Keyboard enhancement (the kitty keyboard protocol) makes the terminal report
    // modified keys as unambiguous escape codes, so Shift+Enter and Ctrl+Enter arrive
    // with their modifiers instead of plain Enter. Unsupported terminals ignore the
    // request. It gets its own `execute!` because it fails on Windows, which would
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
    let result = event_loop(&mut terminal, agent, meta, recorder, questions, signals).await;
    // Pop what was pushed, before `restore` leaves the alternate screen it was
    // pushed on.
    let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    // A hung up terminal can reject restoration. Avoid restore()'s stderr
    // diagnostic, which can itself panic when stderr was that terminal too.
    let _ = ratatui::try_restore();
    result
}

/// Bridge crossterm's blocking `read()` into an async channel.
///
/// The reader thread parks in `read()` forever, when the app quits we simply
/// drop the receiver and let the thread die with the process. A shutdown
/// handshake would need `poll()` with a timeout complexity that buys
/// nothing for a process about to exit.
fn spawn_input_thread() -> tokio::sync::mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        // Runs until read() errors or the receiver is dropped.
        while let Ok(event) = ratatui::crossterm::event::read() {
            if is_ignored_terminal_event(&event) {
                continue;
            }
            if tx.send(event).is_err() {
                break; // Receiver dropped, the UI is gone.
            }
        }
    });
    rx
}

/// Terminal events that Cupel never acts on. Dropping them before waking the
/// event loop avoids redrawing on mouse motion or key release floods.
fn is_ignored_terminal_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved
    ) || matches!(
        event,
        Event::Key(key) if key.kind != KeyEventKind::Press
    ) || matches!(event, Event::FocusGained)
}

/// Mouse press and release rely on hit-test geometry from the latest draw, so
/// they wait for a frame before running after other queued events.
fn needs_fresh_geometry(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_) | MouseEventKind::Up(_))
    )
}

/// Drain already-queued terminal events so wheel bursts, drags, and fast
/// typing share one render pass instead of queueing one draw per event.
fn drain_terminal_events(
    terminal_events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    sessions: &mut sessions::Sessions,
    pending_event: &mut Option<Event>,
) {
    let start_active = sessions.active;
    let start_usage_open = sessions.active().usage_popup.is_some();
    while pending_event.is_none()
        && sessions.active == start_active
        && sessions.active().usage_popup.is_some() == start_usage_open
        && !sessions.active().has_pending_loop_action()
    {
        match terminal_events.try_recv() {
            Ok(event) => {
                if is_ignored_terminal_event(&event) {
                    continue;
                }
                if needs_fresh_geometry(&event) {
                    *pending_event = Some(event);
                    break;
                }
                sessions.on_terminal_event(event);
            }
            Err(_) => break,
        }
    }
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    agent: Agent,
    meta: SessionMeta,
    recorder: cupel_coding_agent::session::SessionRecorder,
    questions: Option<QuestionReceiver>,
    mut signals: TerminationSignals,
) -> std::io::Result<Exit> {
    let mut app = app::App::new(agent, meta, recorder);
    app.questions = questions;
    let mut sessions = sessions::Sessions::new(app);
    sessions.restore().await;
    let mut updates = updates::Updates::default();
    if sessions.active().meta.settings.checks_for_updates() {
        updates.check();
    }
    let mut terminal_events = spawn_input_thread();
    // The spinner's clock lives outside the loop.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Keep session time live while idle without redrawing at spinner speed.
    let mut session_clock = tokio::time::interval(std::time::Duration::from_secs(1));
    session_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut status = Reporter::for_stdout();
    let mut pending_terminal_event: Option<Event> = None;

    let result = loop {
        // Startup prompts use the same hooks as typed prompts, before blocking.
        let app = sessions.active_mut();
        if let Some(prompt) = app.pending_prompt.take() {
            app.recorder.before_prompt(&prompt).await;
            app.start_run(&prompt);
        }
        status.sync(terminal.backend_mut(), sessions.status_records());
        if let Err(error) = terminal.draw(|frame| ui::render_sessions(frame, &mut sessions)) {
            break Err(error);
        }

        if let Some(event) = pending_terminal_event.take() {
            sessions.on_terminal_event(event);
        } else {
            // Wait for whichever source has something first. `next_agent_event`
            // parks forever while idle, so this never busy spins.
            tokio::select! {
                // External termination must not wait for the busy spinoff quit confirmation.
                _ = signals.recv() => break Ok(Exit::Quit),
                event = terminal_events.recv() => {
                    match event {
                        Some(event) => sessions.on_terminal_event(event),
                        None => break Ok(Exit::Quit), // Input thread died, nothing left to do.
                    }
                }
                // Agent events and login events share one wakeup, two
                // `&mut app` futures cannot sit in the same select!, so the
                // App multiplexes them itself (next_event).
                (index, event) = sessions.next_event() => {
                    sessions.list[index].app.on_event(event).await;
                }
                // The update work has its own source, because it belongs to no session.
                event = updates.next_event() => {
                    if let Some(restart) = updates.on_event(event, &mut sessions) {
                        break Ok(Exit::Restart(restart));
                    }
                }
                _ = ticker.tick(), if sessions.any_running() => {
                    sessions.tick();
                }
                _ = session_clock.tick(), if !sessions.any_running() => {}
            }
        }

        // Catch up without waiting, rendering the entire transcript per queued
        // delta would make a fast stream fall behind the producer.
        drain_terminal_events(
            &mut terminal_events,
            &mut sessions,
            &mut pending_terminal_event,
        );
        sessions.drain_ready_events().await;

        // A run ended somewhere. Check for conflicts between the checkouts.
        if sessions.take_finished_runs() {
            sessions.check_conflicts().await;
        }

        // Keys only reach the session on screen, so the requests below always come
        // from there.
        let app = sessions.active_mut();

        // Ctrl+O queued a copy, emit it as OSC 52 the "set clipboard"
        // escape sequence straight to stdout. It paints nothing, so
        // ratatui's frame diff never notices, the terminal (not cupel)
        // performs the actual clipboard write, which is why this works
        // across SSH sessions too.
        if let Some(text) = app.pending_copy.take() {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            let _ = out.write_all(osc52(&text).as_bytes());
            let _ = out.flush();
        }

        // Ctrl+Y requested a selection mode toggle, release the mouse so
        // the terminal can select/copy text natively, or recapture it for
        // wheel scrolling. The command goes to the terminal first, state
        // (and the user facing notice) only flips when it succeeded.
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

        // /new or /hot reload requested a rebuild, the loader re reads every .cupel
        // layer, so this must run here in async context. The old App is consumed and
        // its replacement rebound in place.
        if let Some(target) = app.pending_reload.take() {
            sessions.reload(target).await;
        }
        if let Some(command) = sessions.active_mut().pending_spinoff.take() {
            sessions.spinoff(command).await;
        }
        // `/update` is the process's business, not the session's, the update work gets
        // every session, to see whether one of them is busy.
        if core::mem::take(&mut sessions.active_mut().update_requested)
            && let Some(restart) = updates.request(&mut sessions)
        {
            break Ok(Exit::Restart(restart));
        }
        if let Some(request) = sessions.active_mut().pending_theme.take() {
            sessions.on_theme_request(request);
        }
        let app = sessions.active_mut();
        if let Some(command) = app.pending_undo.take() {
            app.run_undo_command(command).await;
        }

        // A prompt accepted by the (synchronous) key handler starts here,
        // the prompt path hooks are awaited first, so a pending `stop` hook
        // from the previous run is guaranteed to have finished.
        if let Some(prompt) = app.pending_prompt.take() {
            app.recorder.before_prompt(&prompt).await;
            app.start_run(&prompt);
        }

        if sessions.quit_requested() {
            break Ok(Exit::Quit);
        }
    };
    // Don't leave a run mid flight, even if drawing failed after a terminal hangup.
    // Abort and let each session settle so the terminal restore doesn't race provider
    // output. Then drain the hook chains and announce session end.
    sessions.shutdown().await;
    if matches!(&result, Ok(Exit::Restart(_))) {
        status.clear(terminal.backend_mut());
    } else {
        // Keep done/error across normal exit, but never leave working/auth waits
        // behind after cancellation, a signal, or a failed terminal draw.
        status.sync(terminal.backend_mut(), sessions.status_records());
    }
    result
}

/// The OSC 52 "set clipboard" sequence for `text`.
///
/// Shape, ESC, closing bracket, 52, semicolon, c, semicolon, base64 content and BEL. 52 is the clipboard opcode, `c`
/// selects the system clipboard (not the X11 primary selection), and the
/// payload travels base64 encoded because clipboard text may contain any
/// byte including the BEL that would otherwise end the sequence early.
fn osc52(text: &str) -> String {
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{payload}\x07")
}

#[cfg(test)]
mod tests {
    use super::{drain_terminal_events, is_ignored_terminal_event, osc52};
    use crate::sessions::Sessions;
    use crate::transcript::Cell;
    use crate::ui::tests::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

    #[test]
    fn osc52_wraps_the_text_in_the_clipboard_sequence() {
        // "hi" is "aGk=" in base64, ESC ] opens the OSC, BEL closes it.
        assert_eq!(osc52("hi"), "\u{1b}]52;c;aGk=\u{7}");
    }

    #[test]
    fn terminal_event_drain_coalesces_scroll_and_typing_ignores_motion_and_defers_clicks() {
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })
        };
        let mut release = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        let mut repeat = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;

        assert!(is_ignored_terminal_event(&mouse(MouseEventKind::Moved)));
        assert!(is_ignored_terminal_event(&Event::Key(release)));
        assert!(is_ignored_terminal_event(&Event::Key(repeat)));
        assert!(is_ignored_terminal_event(&Event::FocusGained));
        assert!(!is_ignored_terminal_event(&mouse(MouseEventKind::ScrollUp)));
        assert!(!is_ignored_terminal_event(&Event::FocusLost));

        let mut app = test_app();
        for i in 0..80 {
            app.transcript.push(Cell::Assistant {
                text: format!("line {i}"),
            });
        }
        let mut sessions = Sessions::new(app);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| crate::ui::render_sessions(frame, &mut sessions))
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Burst: mouse motion noise + 5 wheel notches + typed text + mouse click + trailing key.
        for _ in 0..20 {
            tx.send(mouse(MouseEventKind::Moved)).unwrap();
        }
        for _ in 0..5 {
            tx.send(mouse(MouseEventKind::ScrollUp)).unwrap();
        }
        for ch in ['h', 'i'] {
            tx.send(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )))
            .unwrap();
        }
        let click = mouse(MouseEventKind::Down(MouseButton::Left));
        tx.send(click.clone()).unwrap();
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        let mut pending = None;
        drain_terminal_events(&mut rx, &mut sessions, &mut pending);
        assert_eq!(sessions.active().scroll_from_bottom, 15);
        assert_eq!(sessions.active().input.text(), "hi");
        assert_eq!(
            pending,
            Some(click),
            "click must wait for a draw so hit testing uses the scrolled viewport"
        );

        // Enter queues a pending prompt and stops draining before the next key.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        let mut pending = None;
        drain_terminal_events(&mut rx, &mut sessions, &mut pending);
        assert_eq!(sessions.active().pending_prompt.as_deref(), Some("hi"));
        assert!(sessions.active().input.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('z'),
                ..
            }))
        ));
    }
}
