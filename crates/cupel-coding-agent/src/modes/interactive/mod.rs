//! Interactive mode: the ratatui frontend.
//!
//! ## Event architecture
//!
//! Two async event sources feed one state struct ([`app::App`]):
//!
//! ```text
//!  crossterm (blocking thread) ──channel──▶            ┌── ui::render
//!                                          tokio::select ──▶ App ──┘
//!  AgentEventStream (active run) ─────────▶
//! ```
//!
//! Terminal input is read on a dedicated OS thread because crossterm's
//! `read()` is blocking; a channel bridges it into the async world. Agent
//! events already arrive as a `Stream`. Each `select!` wakeup mutates the
//! `App`, then the loop redraws. That's the whole architecture.

pub mod app;
pub mod autocomplete;
pub mod fuzzy;
pub mod input;
pub mod transcript;
pub mod ui;

use cupel_agent::Agent;
use ratatui::crossterm::event::Event;

use crate::modes::SessionMeta;

/// Run the interactive session until the user quits.
///
/// Errors are terminal I/O failures; agent failures surface inside the UI.
pub async fn run(agent: Agent, meta: SessionMeta) -> std::io::Result<()> {
    // `ratatui::init` enters raw mode + the alternate screen and installs a
    // panic hook that restores the terminal - without that, a panic would
    // leave the user's shell in raw mode (no echo, no line editing).
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, agent, meta).await;
    ratatui::restore();
    result
}

/// Bridge crossterm's blocking `read()` into an async channel.
///
/// The reader thread parks in `read()` forever; when the app quits we simply
/// drop the receiver and let the thread die with the process. A shutdown
/// handshake would need `poll()` with a timeout - complexity that buys
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
) -> std::io::Result<()> {
    let mut app = app::App::new(agent, meta);
    let mut terminal_events = spawn_input_thread();

    loop {
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        // Wait for whichever source has something first. `next_agent_event`
        // parks forever while idle, so this never busy-spins.
        tokio::select! {
            event = terminal_events.recv() => {
                match event {
                    Some(event) => app.on_terminal_event(event),
                    None => break, // Input thread died; nothing left to do.
                }
            }
            event = app.next_agent_event() => {
                app.on_agent_event(event).await;
            }
        }

        if app.should_quit {
            // Don't leave a run mid-flight: abort and let it settle so the
            // terminal restore doesn't race provider output.
            if app.is_running() {
                app.agent.abort();
            }
            app.agent.wait_for_idle().await;
            break;
        }
    }
    Ok(())
}
