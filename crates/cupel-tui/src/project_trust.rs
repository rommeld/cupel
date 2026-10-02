//! Startup trust popup. Runs before bootstrap, model selection, or hooks.

use std::path::Path;

use cupel_coding_agent::project_trust::{ProjectTrust, decision, save};
use ratatui::Frame;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::theme;

/// Ask once per canonical project and persist either answer in cupel home.
/// There is no CLI auto-trust flag; noninteractive startup stays restricted
/// unless an earlier interactive decision explicitly granted trust.
pub fn confirm(home: Option<&Path>, cwd: &Path) -> std::io::Result<()> {
    if decision(home, cwd).is_some() {
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let result = (|| {
        // Pasted tabs/newlines are an Event::Paste, not consent keystrokes.
        execute!(std::io::stdout(), EnableBracketedPaste)?;
        let mut selected = ProjectTrust::Restricted;
        loop {
            terminal.draw(|frame| render(frame, cwd, selected))?;
            if let Event::Key(key) = event::read()? {
                match on_key(&mut selected, key) {
                    Some(Answer::Save(trust)) => return Ok(trust),
                    Some(Answer::Quit) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "project trust confirmation cancelled",
                        ));
                    }
                    None => {}
                }
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    let trust = result?;
    match home {
        Some(home) => save(home, cwd, trust).map_err(|e| {
            std::io::Error::other(format!(
                "cannot save project trust in {}: {e}",
                home.display()
            ))
        }),
        None if trust == ProjectTrust::Restricted => Ok(()),
        None => Err(std::io::Error::other(
            "cannot grant project trust without cupel home; set CUPEL_HOME or a home directory",
        )),
    }
}

enum Answer {
    Save(ProjectTrust),
    Quit,
}

fn on_key(selected: &mut ProjectTrust, key: KeyEvent) -> Option<Answer> {
    // Pasted text and held/released keys must never grant trust.
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Answer::Quit);
    }
    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
            *selected = match selected {
                ProjectTrust::Restricted => ProjectTrust::Trusted,
                ProjectTrust::Trusted => ProjectTrust::Restricted,
            };
            None
        }
        KeyCode::Enter => Some(Answer::Save(*selected)),
        KeyCode::Esc => Some(Answer::Save(ProjectTrust::Restricted)),
        _ => None,
    }
}

fn render(frame: &mut Frame<'_>, cwd: &Path, selected: ProjectTrust) {
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(19),
        Constraint::Fill(1),
    ])
    .areas(frame.area());
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Max(80),
        Constraint::Fill(1),
    ])
    .areas(middle);
    let choice = |text, trust| {
        Span::styled(
            text,
            if selected == trust {
                theme::POPUP_SELECTED
            } else {
                theme::POPUP_ROW
            },
        )
    };
    let lines = vec![
        Line::from("Do you trust this project's authors and configuration?"),
        Line::from(cwd.display().to_string()),
        Line::from(""),
        Line::from("Trust enables executable .cupel/hooks and sensitive .cupel/models.json rows."),
        Line::from(
            "Model overrides can send API keys, OAuth tokens, prompts and files to other hosts.",
        ),
        Line::from(
            "Hooks run as your user. Their environment is cleared, but they can read files.",
        ),
        Line::from(""),
        Line::from(
            "Without trust: no project hooks or model overrides; new keyless local models remain available.",
        ),
        Line::from(
            "This is NOT a sandbox: model-directed tools still execute without per-call approval.",
        ),
        Line::from(""),
        Line::from(vec![
            choice(" Continue without trust ", ProjectTrust::Restricted),
            Span::raw("  "),
            choice(" Trust this project ", ProjectTrust::Trusted),
        ]),
        Line::from("Arrows/Tab: select   Enter: confirm   Esc: without trust   Ctrl+C: quit"),
        Line::from("Decision saved in cupel home, not in this repository."),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Project trust ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use crate::project_trust::{Answer, on_key, render};
    use cupel_coding_agent::project_trust::ProjectTrust;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn confirmation_defaults_to_restricted_and_requires_explicit_selection() {
        let mut selected = ProjectTrust::Restricted;
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert!(matches!(
            on_key(&mut selected, key(KeyCode::Enter)),
            Some(Answer::Save(ProjectTrust::Restricted))
        ));
        assert!(on_key(&mut selected, key(KeyCode::Char('y'))).is_none());
        let mut held = key(KeyCode::Right);
        held.kind = KeyEventKind::Repeat;
        assert!(on_key(&mut selected, held).is_none());
        assert_eq!(selected, ProjectTrust::Restricted);
        assert!(on_key(&mut selected, key(KeyCode::Tab)).is_none());
        assert!(matches!(
            on_key(&mut selected, key(KeyCode::Enter)),
            Some(Answer::Save(ProjectTrust::Trusted))
        ));
        assert!(matches!(
            on_key(&mut selected, key(KeyCode::Esc)),
            Some(Answer::Save(ProjectTrust::Restricted))
        ));
        assert!(matches!(
            on_key(
                &mut selected,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(Answer::Quit)
        ));
    }

    #[test]
    fn popup_renders_the_risks_and_both_choices() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    std::path::Path::new("/foreign/repo"),
                    ProjectTrust::Restricted,
                );
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for text in [
            "Project trust",
            "/foreign/repo",
            "OAuth tokens",
            "read files",
            "Continue without trust",
            "Trust this project",
            "NOT a sandbox",
        ] {
            assert!(screen.contains(text), "missing warning/choice: {text}");
        }
    }
}
