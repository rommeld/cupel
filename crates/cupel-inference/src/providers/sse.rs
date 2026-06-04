/// One parsed Server-Sent Event frame.
///
/// SSE allows an optional event name plus one or more data lines.
/// Event name will stay optional because OpenAI often relies on the
/// JSON `type`field, while Anthropic also sends explicit SSE event names such
/// as `message_start`.
pub struct SseEvent {
    /// The optional value from an `event:` line.
    ///
    /// Examples:
    /// - `message_start`
    /// - `content_block_delta`
    /// - `response.completed`
    pub event: Option<String>,

    /// The joined payload from all `data:` lines.
    ///
    /// SSE joins multiple data lines with newlines. Provider adapters can then
    /// parse this string as JSON or compare it with sentinels such as `[DONE]`.
    pub data: String,
}

/// Incremental decoder for SSE.
///
/// Network chunks do not necessarily line up with event boundaries. One chunk
/// may contain half an event, exactly one event, or serveral events. The decoder
/// stores incomplete text in `buffer` until a blank line finishes an event.
#[derive(Default)]
pub struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    /// Push newly received bytes and return every complete event now available.
    ///
    /// Providers call this from their `response.bytes_stream()` loop. If the
    /// bytes only contain part of an event, this returns an empty vector and
    /// keeps the partial data for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        // SSE is UTF-8 text. `from_utf8_lossy` avoids panicking if a provider
        // sends a malformed byte; malformed bytes become replacement chars.
        self.buffer.push_str(&String::from_utf8_lossy(bytes));

        let mut events = Vec::new();

        // Normalize CRLF to LF so both Unix and HTTP-style line endings work.
        // This keeps the parsing logic below simple and deterministic.
        self.buffer = self.buffer.replace("\r\n", "\n");

        while let Some((raw_event, rest)) = self.buffer.split_once("\n\n") {
            let raw_event = raw_event.to_owned();
            self.buffer = rest.to_owned();

            if let Some(event) = parse_sse_event(&raw_event) {
                events.push(event);
            }
        }

        events
    }
}

/// Parse one complete SSE frame.
///
/// Returns `None` for emtpy/comment-only events. SSE comments start with `:`;
/// providers sometimes use comments as kee-alives, and cupel does not need to
/// expose those as model events.
fn parse_sse_event(raw_event: &str) -> Option<SseEvent> {
    let mut event_name = None;
    let mut data_lines = Vec::new();

    for line in raw_event.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_owned());
            continue;
        }

        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_owned());
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    Some(SseEvent {
        event: event_name,
        data: data_lines.join("\n"),
    })
}

#[test]
fn sse_decoder_waits_for_complete_events() {
    let mut decoder = SseDecoder::default();

    // No blank line yet, so there is no complete SSE event to return.
    assert!(decoder.push(b"data: {\"type\"").is_empty());

    let events = decoder.push(br#": "response.created"}"#);
    assert!(events.is_empty());

    // The blank line completes the event.
    let events = decoder.push(b"\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, r#"{"type": "response.creaded"}"#);
}

#[test]
fn sse_decoder_handels_named_events_and_multiline_data() {
    let mut decoder = SseDecoder::default();

    let events = decoder.push(
        b"event: message_start\n\
            data: {\"a\":1}\n\
            data: {\"b\":2}\n\n"
    );

    assert_eq!(events[0].event.as_deref(), Some("message_start"));
    assert_eq!(events[0].data, "{\"a\":1}\n{\"b\":2}");
}