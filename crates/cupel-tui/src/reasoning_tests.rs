use cupel_agent::{Agent, AgentEvent, AgentOptions};
use cupel_core::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Message, StopReason,
    ThinkingContent, Usage,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Modifier;

use crate::app::App;
use crate::theme::{self, CUPEL};
use crate::transcript::{Cell, ToolDisplay, Transcript};
use crate::ui::tests::test_app;

fn text(transcript: &Transcript) -> String {
    transcript
        .to_lines(80, None, &CUPEL)
        .lines
        .iter()
        .map(|line| {
            let mut text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            text.push('\n');
            text
        })
        .collect()
}

fn draw(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, app, frame.area(), &CUPEL))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let mut row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
            row.push('\n');
            row
        })
        .collect()
}

fn toggle_all(app: &mut App) {
    app.on_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )));
}

fn click_line(app: &mut App, line: usize) {
    app.on_terminal_event(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: app.last_transcript_inner.x,
        row: app.last_transcript_inner.y + u16::try_from(line - app.last_top_line).unwrap(),
        modifiers: KeyModifiers::NONE,
    }));
}

#[test]
fn disclosure_defaults_to_expanded_and_preserves_markdown_and_source() {
    let source = "**Reading the parser**\n\n- Check `parse`\n- Keep *emphasis*\n\n**Next section**\n\n```rust\nfn parse() {}\n```";
    let mut transcript = Transcript::default();
    transcript.append_thinking(source);
    let expanded = text(&transcript);
    assert!(
        expanded.starts_with("▾ Reasoning · Reading the parser\n"),
        "{expanded}"
    );
    assert_eq!(expanded.matches("Reading the parser").count(), 1);
    for expected in [
        "Check parse",
        "Keep emphasis",
        "Next section",
        "fn parse() {}",
    ] {
        assert!(expanded.contains(expected), "{expanded}");
    }
    transcript.toggle_reasoning(0);
    assert_eq!(text(&transcript), "▸ Reasoning · Reading the parser\n");
    assert_eq!(transcript.copy_text(0), Some(source));
    transcript.append_thinking("\nmore reasoning");
    assert_eq!(text(&transcript), "▸ Reasoning · Reading the parser\n");
    transcript.toggle_reasoning(0);
    assert!(text(&transcript).contains("more reasoning"));
}

#[test]
fn reasoning_uses_only_a_complete_leading_provider_heading() {
    for (source, label, body) in [
        (
            "# Reading the parser\nBody",
            "▾ Reasoning · Reading the parser",
            "Body",
        ),
        (
            "**Inspect `parse`**\nBody",
            "▾ Reasoning · Inspect parse",
            "Body",
        ),
        ("plain body", "▾ Reasoning", "plain body"),
        ("**unfinished", "▾ Reasoning", "unfinished"),
        ("**one** and **two**", "▾ Reasoning", "one and two"),
    ] {
        let mut transcript = Transcript::default();
        transcript.append_thinking(source);
        let rendered = text(&transcript);
        assert_eq!(rendered.lines().next(), Some(label), "{source}");
        assert!(rendered.contains(body), "{rendered}");
    }
    let mut transcript = Transcript::default();
    transcript.append_thinking(" \n\x1b[31m\x1b[0m");
    transcript.append_assistant("visible commentary");
    assert_eq!(text(&transcript), "visible commentary\n");
}

#[test]
fn reasoning_is_readable_and_wraps_within_the_column_in_every_theme() {
    let mut transcript = Transcript::default();
    transcript.append_thinking("**Reading 日本語 and a very long heading that must not wrap onto a second disclosure row**\n\nPlain body with **bold** and *italic*.\n\n日本語の長い文章を折り返して表示します。\n\n```rust\nlet value = 42;\n```");
    transcript.cells.push(Cell::Answer {
        text: "Plain answer".into(),
    });
    for theme in theme::example_themes() {
        for width in [10, 60, 80, 120] {
            let rendered = transcript.to_lines(width, None, &theme);
            assert!(
                rendered
                    .lines
                    .iter()
                    .all(|line| line.width() <= usize::from(width)),
                "{} at {width}",
                theme.id
            );
            assert!(
                !rendered.lines[0].spans[0]
                    .style
                    .add_modifier
                    .contains(Modifier::ITALIC)
            );
            let plain = rendered
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains("Plain body"));
            if width >= 60 {
                let plain = plain.unwrap();
                assert_eq!(plain.style.fg, theme.reasoning.fg);
                assert!(!plain.style.add_modifier.contains(Modifier::ITALIC));
                let italic = rendered
                    .lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .find(|span| span.content == "italic")
                    .unwrap();
                assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
            }
        }
    }
}

#[tokio::test]
async fn ctrl_r_toggles_all_reasoning_without_touching_tools_or_commentary() {
    let mut app = test_app();
    app.transcript.append_thinking("first thought");
    app.transcript.cells.push(Cell::Tool {
        id: "tool".into(),
        name: "bash".into(),
        call: "$ true".into(),
        display: ToolDisplay::Preview,
        started_at: None,
        live: None,
        result: None,
    });
    app.transcript.append_thinking("second thought");
    app.transcript.append_assistant("visible commentary");
    let before = draw(&mut app, 140, 30);
    assert_eq!(before.matches("▾ Reasoning").count(), 2, "{before}");
    toggle_all(&mut app);
    let collapsed = draw(&mut app, 140, 30);
    assert_eq!(collapsed.matches("▸ Reasoning").count(), 2, "{collapsed}");
    assert!(!collapsed.contains("first thought") && !collapsed.contains("second thought"));
    assert!(collapsed.contains("visible commentary") && collapsed.contains("$ true"));
    assert!(collapsed.contains("ctrl+r exp. reasoning"));
    app.on_agent_event(Some(AgentEvent::MessageUpdate {
        event: AssistantMessageEvent::ThinkingDelta {
            content_index: 2,
            delta: "new thought".into(),
        },
    }))
    .await;
    app.on_agent_event(Some(AgentEvent::MessageUpdate {
        event: AssistantMessageEvent::ThinkingDelta {
            content_index: 2,
            delta: " continues".into(),
        },
    }))
    .await;
    let streamed = draw(&mut app, 140, 30);
    assert_eq!(streamed.matches("▸ Reasoning").count(), 3, "{streamed}");
    assert!(!streamed.contains("new thought"));
    toggle_all(&mut app);
    let expanded = draw(&mut app, 140, 30);
    assert!(expanded.contains("new thought continues"));
    assert!(matches!(
        &app.transcript.cells[1],
        Cell::Tool {
            display: ToolDisplay::Preview,
            ..
        }
    ));
    app.on_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
    )));
    assert!(matches!(
        &app.transcript.cells[1],
        Cell::Tool {
            display: ToolDisplay::Collapsed,
            ..
        }
    ));
    assert!(app.transcript.cells.iter().all(|cell| !matches!(
        cell,
        Cell::Thinking {
            collapsed: true,
            ..
        }
    )));
}

#[test]
fn clicking_headers_toggles_only_that_block_and_body_clicks_still_copy() {
    let mut app = test_app();
    let source = "**First section**\n\nraw `source`";
    app.transcript.append_thinking(source);
    app.transcript.append_assistant("commentary");
    app.transcript.append_thinking("other reasoning");
    let _ = draw(&mut app, 80, 30);
    click_line(&mut app, 0);
    let screen = draw(&mut app, 80, 30);
    assert!(screen.contains("▸ Reasoning · First section"));
    assert!(!screen.contains("raw source"));
    assert!(screen.contains("other reasoning"));
    click_line(&mut app, 0);
    let _ = draw(&mut app, 80, 30);
    click_line(&mut app, 1);
    assert_eq!(app.selected_cell, Some(0));
    app.on_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.pending_copy.as_deref(), Some(source));
}

#[test]
fn disclosure_keeps_a_scrolled_reader_on_the_same_content() {
    let mut app = test_app();
    app.transcript
        .append_thinking(&"old reasoning\n".repeat(25));
    for index in 0..25 {
        app.transcript.cells.push(Cell::Assistant {
            text: format!("history {index}"),
        });
    }
    let _ = draw(&mut app, 80, 15);
    app.scroll_from_bottom = 10;
    let _ = draw(&mut app, 80, 15);
    let top_cell = app.last_line_cells[app.last_top_line];
    toggle_all(&mut app);
    let _ = draw(&mut app, 80, 15);
    assert_eq!(app.last_line_cells[app.last_top_line], top_cell);
    toggle_all(&mut app);
    let _ = draw(&mut app, 80, 15);
    assert_eq!(app.last_line_cells[app.last_top_line], top_cell);
    let top_line = app.last_top_line;
    app.transcript.append_thinking("new live reasoning");
    let screen = draw(&mut app, 80, 15);
    assert_eq!(app.last_top_line, top_line);
    assert!(!screen.contains("new live reasoning"));
}

#[test]
fn a_clicked_header_stays_in_place_when_the_body_changes_height() {
    let mut app = test_app();
    app.transcript.append_thinking(&"body\n".repeat(15));
    for index in 0..25 {
        app.transcript.cells.push(Cell::Assistant {
            text: format!("tail {index}"),
        });
    }
    let _ = draw(&mut app, 80, 15);
    app.scroll_from_bottom = app.last_total_lines;
    let _ = draw(&mut app, 80, 15);
    click_line(&mut app, 0);
    let _ = draw(&mut app, 80, 15);
    assert_eq!(app.last_top_line, 0);
    click_line(&mut app, 0);
    let _ = draw(&mut app, 80, 15);
    assert_eq!(app.last_top_line, 0);
    for (width, height) in [(1, 1), (20, 5), (60, 12), (120, 30)] {
        let _ = draw(&mut app, width, height);
        toggle_all(&mut app);
        let _ = draw(&mut app, width, height);
    }
}

#[test]
fn replayed_reasoning_starts_expanded() {
    let old = test_app();
    let model = old.agent.state().model;
    let mut options = AgentOptions::new(model.clone(), old.agent.registry());
    options.messages = vec![cupel_agent::AgentMessage::Llm(Message::Assistant(
        AssistantMessage {
            content: vec![AssistantContent::Thinking(ThinkingContent {
                thinking: "**Replayed title**\n\nReplayed body".into(),
                thinking_signature: None,
                redacted: None,
            })],
            api: model.api,
            provider: model.provider,
            model: model.id,
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            error_kind: None,
            timestamp: 0,
        },
    ))];
    let mut app = App::new(Agent::new(options), old.meta, old.recorder);
    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("▾ Reasoning · Replayed title"), "{screen}");
    assert!(screen.contains("Replayed body"));
    toggle_all(&mut app);
    assert!(!draw(&mut app, 80, 24).contains("Replayed body"));
}
