use cupel_agent::{Agent, AgentEvent, AgentMessage, AgentOptions};
use cupel_coding_agent::usage::HistoryUsage;
use cupel_core::types::{Api, AssistantMessage, Message, Provider, StopReason, Usage};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::App;
use crate::sessions::{Session, Sessions};
use crate::theme::CUPEL;
use crate::ui::tests::test_app;
use crate::usage::HistoryState;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn reply(model: &str, input: u64, reason: StopReason) -> AgentMessage {
    AgentMessage::Llm(Message::Assistant(AssistantMessage {
        content: Vec::new(),
        api: Api::from("test"),
        provider: Provider::from("test"),
        model: model.to_string(),
        response_model: None,
        response_id: None,
        usage: Usage {
            input,
            output: 4,
            cache_read: 6,
            ..Usage::default()
        },
        stop_reason: reason,
        error_message: None,
        error_kind: None,
        timestamp: 0,
    }))
}

async fn record(app: &mut App, model: &str, input: u64, reason: StopReason) {
    app.on_agent_event(Some(AgentEvent::MessageEnd {
        message: reply(model, input, reason),
    }))
    .await;
}

fn open(app: &mut App) {
    app.input.insert_str("/usage");
    app.on_terminal_event(key(KeyCode::Enter));
    assert!(app.usage_popup.is_some());
}

async fn finish_loading(app: &mut App) {
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), app.next_event())
        .await
        .unwrap();
    assert!(matches!(&event, crate::app::AppEvent::Usage(_)));
    app.on_event(event).await;
}

fn draw(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, app, frame.area(), &CUPEL))
        .unwrap();
    buffer_text(terminal.backend().buffer())
}

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[tokio::test]
async fn usage_opens_a_bordered_table_without_a_conversation_notice() {
    let mut app = test_app();
    record(&mut app, "first", 10, StopReason::Stop).await;
    record(&mut app, "second", 20, StopReason::Error).await;
    assert_eq!(app.totals.input, 30);
    let cells = app.transcript.cells.len();
    open(&mut app);
    finish_loading(&mut app).await;
    assert_eq!(app.transcript.cells.len(), cells);
    let screen = draw(&mut app, 120, 30);
    for expected in [
        "[X] Usage",
        "CURRENT SESSION",
        "ALL SESSIONS",
        "test/first",
        "test/second",
        "Cached",
        "TOTAL",
        "┌",
        "┐",
        "└",
        "┘",
    ] {
        assert!(screen.contains(expected), "missing {expected}:\n{screen}");
    }
    assert!(
        screen.contains("Partial history"),
        "no home is visible, not silently zero: {screen}"
    );
}

#[tokio::test]
async fn usage_formats_tokens_like_the_footer_and_costs_to_three_decimals() {
    let mut app = test_app();
    record(&mut app, "model", 1_234, StopReason::Stop).await;
    for totals in app.usage_by_model.models.values_mut() {
        totals.output = 1_299;
        totals.cache_read = 999;
        totals.cost = 1.23456;
    }
    open(&mut app);
    finish_loading(&mut app).await;
    app.usage_popup.as_mut().unwrap().history = HistoryState::Ready(HistoryUsage::default());
    let screen = draw(&mut app, 120, 30);
    let rows: Vec<_> = screen
        .lines()
        .filter(|line| line.contains("test/model") || line.contains("TOTAL"))
        .collect();
    assert_eq!(rows.len(), 4, "model and total in both sections: {screen}");
    for row in rows {
        let columns: Vec<_> = row
            .trim_matches(|ch: char| ch.is_whitespace() || ch == '│')
            .split_whitespace()
            .collect();
        assert_eq!(&columns[1..], &["1.2k", "1.3k", "999", "$1.235"]);
    }
}

#[tokio::test]
async fn footer_and_usage_abbreviate_millions_billions_and_trillions() {
    let mut app = test_app();
    record(&mut app, "model", 1_200_000, StopReason::Stop).await;
    for totals in app.usage_by_model.models.values_mut() {
        totals.output = 3_400_000_000;
        totals.cache_read = 5_600_000_000_000;
        app.totals = *totals;
    }
    app.context_tokens = 1_200_000;
    let screen = draw(&mut app, 160, 30);
    assert!(
        screen.contains("1.2M in · 3.4B out · 5.6T cached | ctx 1.2M/"),
        "{screen}"
    );

    open(&mut app);
    finish_loading(&mut app).await;
    app.usage_popup.as_mut().unwrap().history = HistoryState::Ready(HistoryUsage::default());
    let screen = draw(&mut app, 120, 30);
    let rows: Vec<_> = screen
        .lines()
        .filter(|line| line.contains("test/model") || line.contains("TOTAL"))
        .collect();
    assert_eq!(rows.len(), 4, "model and total in both sections: {screen}");
    for row in rows {
        let columns: Vec<_> = row
            .trim_matches(|ch: char| ch.is_whitespace() || ch == '│')
            .split_whitespace()
            .collect();
        assert_eq!(&columns[1..], &["1.2M", "3.4B", "5.6T", "$0.000"]);
    }
}

#[tokio::test]
async fn usage_is_modal_and_escape_does_not_abort_the_running_agent() {
    let mut app = test_app();
    let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
    app.run_events = Some(events);
    app.queued.push("keep queued".to_string());
    open(&mut app);
    app.on_terminal_event(Event::Paste("must not enter the editor".to_string()));
    app.on_terminal_event(key(KeyCode::Char('z')));
    app.on_terminal_event(key(KeyCode::Enter));
    app.on_terminal_event(mouse(MouseEventKind::ScrollUp, 5, 5));
    assert!(app.input.is_empty());
    assert_eq!(app.scroll_from_bottom, 0);
    app.on_terminal_event(key(KeyCode::Esc));
    assert!(app.usage_popup.is_none());
    assert!(app.is_running());
    assert_eq!(app.queued, ["keep queued"], "Esc must not call abort_run");
}

#[tokio::test]
async fn usage_x_hit_testing_and_sidebar_routing_use_the_rendered_coordinates() {
    let mut sessions = Sessions::new(test_app());
    sessions.list.push(Session {
        label: "other".into(),
        app: test_app(),
    });
    open(sessions.active_mut());
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render_sessions(frame, &mut sessions))
        .unwrap();
    let row = sessions
        .rows
        .iter()
        .find(|(_, index)| *index == 1)
        .unwrap()
        .0;
    sessions.on_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y));
    assert_eq!(sessions.active, 0, "the sidebar is blocked by the modal");
    sessions.on_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Char('n'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(sessions.active, 0);
    let close = sessions.active().usage_popup.as_ref().unwrap().close_button;
    assert!(
        close.x > 24,
        "popup lives inside the main pane, after the sidebar"
    );
    sessions.on_terminal_event(mouse(
        MouseEventKind::Down(MouseButton::Left),
        close.x + 1,
        close.y,
    ));
    assert!(sessions.active().usage_popup.is_none());
    sessions.on_terminal_event(mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y));
    assert_eq!(sessions.active, 1, "normal routing returns after closing");
}

#[tokio::test]
async fn usage_scrolls_long_tables_and_survives_tiny_terminals_and_resize() {
    let mut app = test_app();
    for index in 0..40 {
        record(&mut app, &format!("model-{index:02}"), 10, StopReason::Stop).await;
    }
    open(&mut app);
    finish_loading(&mut app).await;
    let _ = draw(&mut app, 120, 18);
    app.on_terminal_event(key(KeyCode::End));
    let screen = draw(&mut app, 120, 18);
    assert!(app.usage_popup.as_ref().unwrap().scroll > 0);
    assert!(
        screen.contains("model-39"),
        "last model is reachable: {screen}"
    );
    app.on_terminal_event(key(KeyCode::Home));
    let _ = draw(&mut app, 120, 18);
    app.on_terminal_event(mouse(MouseEventKind::ScrollDown, 10, 10));
    assert_eq!(app.usage_popup.as_ref().unwrap().scroll, 3);
    for (width, height) in [(1, 1), (20, 5), (60, 12), (120, 30)] {
        let _ = draw(&mut app, width, height);
    }
    app.on_terminal_event(key(KeyCode::Esc));
    assert!(app.usage_popup.is_none());
}

#[tokio::test]
async fn usage_scrollbar_clicks_and_drags_reach_both_ends_without_affecting_chat() {
    let mut sessions = Sessions::new(test_app());
    sessions.list.push(Session {
        label: "other".into(),
        app: test_app(),
    });
    let app = sessions.active_mut();
    for index in 0..40 {
        record(app, &format!("model-{index:02}"), 10, StopReason::Stop).await;
    }
    open(app);
    finish_loading(app).await;
    let mut terminal = Terminal::new(TestBackend::new(120, 18)).unwrap();
    terminal
        .draw(|frame| crate::ui::render_sessions(frame, &mut sessions))
        .unwrap();
    let popup = sessions.active().usage_popup.as_ref().unwrap();
    let area = popup.scrollbar_area;
    let max = popup.content_rows - popup.viewport_rows;
    assert!(!area.is_empty());
    assert!(area.x > 24, "hit testing includes the sidebar offset");
    assert_eq!(terminal.backend().buffer()[(area.x, area.y)].symbol(), "█");
    let cursor = sessions.active().input.cursor();
    sessions.on_terminal_event(mouse(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.y + area.height / 2,
    ));
    assert_eq!(
        sessions.active().usage_popup.as_ref().unwrap().scroll,
        max * usize::from(area.height / 2) / usize::from(area.height - 1)
    );
    sessions.on_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), 0, u16::MAX));
    assert_eq!(sessions.active().usage_popup.as_ref().unwrap().scroll, max);
    terminal
        .draw(|frame| crate::ui::render_sessions(frame, &mut sessions))
        .unwrap();
    assert_eq!(
        terminal.backend().buffer()[(area.x, area.bottom() - 1)].symbol(),
        "█"
    );
    assert!(buffer_text(terminal.backend().buffer()).contains("model-39"));
    sessions.on_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0));
    assert_eq!(sessions.active().usage_popup.as_ref().unwrap().scroll, 0);
    sessions.on_terminal_event(mouse(MouseEventKind::Up(MouseButton::Left), 0, 0));
    sessions.on_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), 0, u16::MAX));
    assert_eq!(sessions.active().usage_popup.as_ref().unwrap().scroll, 0);
    assert_eq!(sessions.active, 0);
    assert_eq!(sessions.active().scroll_from_bottom, 0);
    assert_eq!(sessions.active().input.cursor(), cursor);
    assert!(sessions.active().text_selection.is_none());
}

#[tokio::test]
async fn usage_scrollbar_drag_cancels_on_focus_resize_capture_and_hidden_bar() {
    let mut app = test_app();
    for index in 0..40 {
        record(&mut app, &format!("model-{index:02}"), 10, StopReason::Stop).await;
    }
    open(&mut app);
    finish_loading(&mut app).await;
    let _ = draw(&mut app, 120, 18);
    let area = app.usage_popup.as_ref().unwrap().scrollbar_area;
    for event in [Event::FocusLost, Event::Resize(100, 20)] {
        app.on_terminal_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y,
        ));
        app.on_terminal_event(event);
        app.on_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), 0, u16::MAX));
        assert_eq!(app.usage_popup.as_ref().unwrap().scroll, 0);
    }
    app.on_terminal_event(mouse(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.y,
    ));
    assert!(!app.apply_mouse_toggle());
    app.on_terminal_event(mouse(
        MouseEventKind::Down(MouseButton::Left),
        area.x,
        area.bottom() - 1,
    ));
    app.on_terminal_event(mouse(MouseEventKind::ScrollDown, area.x, area.y));
    assert_eq!(app.usage_popup.as_ref().unwrap().scroll, 0);
    assert!(app.apply_mouse_toggle());
    app.on_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), 0, u16::MAX));
    assert_eq!(app.usage_popup.as_ref().unwrap().scroll, 0);

    for (width, height) in [(1, 1), (20, 5), (100, 20)] {
        let _ = draw(&mut app, 120, 18);
        app.on_terminal_event(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y,
        ));
        let _ = draw(&mut app, width, height);
        if width < 60 {
            assert!(app.usage_popup.as_ref().unwrap().scrollbar_area.is_empty());
        }
        let _ = draw(&mut app, 120, 18);
        app.on_terminal_event(mouse(MouseEventKind::Drag(MouseButton::Left), 0, u16::MAX));
        assert_eq!(app.usage_popup.as_ref().unwrap().scroll, 0);
    }
}

#[tokio::test]
async fn usage_scrollbar_is_hidden_when_all_rows_fit() {
    let mut app = test_app();
    open(&mut app);
    finish_loading(&mut app).await;
    let screen = draw(&mut app, 120, 30);
    assert!(app.usage_popup.as_ref().unwrap().scrollbar_area.is_empty());
    assert!(!screen.contains('█'), "{screen}");
}

#[tokio::test]
async fn usage_is_an_open_time_snapshot_and_agent_events_keep_being_accounted() {
    let mut app = test_app();
    record(&mut app, "m", 10, StopReason::Stop).await;
    open(&mut app);
    record(&mut app, "m", 20, StopReason::Aborted).await;
    assert_eq!(app.totals.input, 30);
    assert_eq!(app.usage_popup.as_ref().unwrap().current.totals().input, 10);
    finish_loading(&mut app).await;
    app.on_terminal_event(key(KeyCode::Esc));
    open(&mut app);
    assert_eq!(app.usage_popup.as_ref().unwrap().current.totals().input, 30);
}

#[tokio::test]
async fn usage_replay_reconstructs_the_same_model_rows_without_counting_again_on_open() {
    let old = test_app();
    let mut options = AgentOptions::new(old.agent.state().model, old.agent.registry());
    options.messages = vec![
        reply("a", 10, StopReason::Stop),
        reply("b", 20, StopReason::Stop),
    ];
    let mut app = App::new(Agent::new(options), old.meta, old.recorder);
    assert_eq!(app.usage_by_model.models.len(), 2);
    assert_eq!(app.totals.input, 30);
    for _ in 0..2 {
        open(&mut app);
        finish_loading(&mut app).await;
        app.on_terminal_event(key(KeyCode::Esc));
    }
    assert_eq!(app.totals.input, 30);
}

#[tokio::test]
async fn usage_displays_loading_failures_inside_the_popup() {
    let mut app = test_app();
    open(&mut app);
    app.usage_popup.as_mut().unwrap().history = HistoryState::Failed("test history failure".into());
    let screen = draw(&mut app, 120, 30);
    assert!(screen.contains("test history failure"), "{screen}");
    assert!(screen.contains("CURRENT SESSION"), "{screen}");
}
