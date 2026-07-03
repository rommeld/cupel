//! Server-Sent Events (SSE) decoder shared by the Anthropic and `OpenAI`
//! providers.
//!
//! SSE is a line-oriented text protocol: fields like `event: foo` and
//! `data: {...}` accumulate until a blank line "flushes" one event. pi
//! implements the same state machine in `anthropic-messages.ts`
//! (`decodeSseLine`/`flushSseEvent`); this is a direct port.
//!
//! Design notes for the Rust version:
//! - The decoder is *push-based*: the caller feeds raw network chunks into
//!   [`SseDecoder::push`] and receives zero or more complete events. This
//!   fits `reqwest`'s `bytes_stream()` which yields chunks at arbitrary
//!   boundaries - an event may be split across chunks, or one chunk may
//!   contain many events.
//! - We buffer bytes (not `String`) because a chunk may end in the middle of
//!   a multi-byte UTF-8 character. Lines are only converted to text once a
//!   line break proves they are complete.

/// One decoded SSE event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerSentEvent {
    /// The `event:` field, if any (e.g. `"message_start"`).
    pub event: Option<String>,
    /// All `data:` lines joined with `\n`, per the SSE spec.
    pub data: String,
}

/// Incremental SSE decoder. Feed it chunks; it emits complete events.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received but not yet terminated by a line break.
    buffer: Vec<u8>,
    /// Fields of the event currently being assembled.
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one network chunk; complete events are appended to `out`.
    ///
    /// Taking an output `Vec` instead of returning one lets the caller reuse
    /// a single allocation across the whole stream.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<ServerSentEvent>) {
        self.buffer.extend_from_slice(chunk);

        // Repeatedly carve complete lines off the front of the buffer.
        // A "line" ends at \n, \r, or \r\n (the SSE spec allows all three).
        // The loop ends when no complete line remains; wait for more bytes.
        while let Some(break_at) = self.buffer.iter().position(|b| *b == b'\n' || *b == b'\r') {
            // If the buffer ends exactly at a '\r' we cannot yet know whether
            // a '\n' follows (a CRLF pair split across chunks). Wait.
            if self.buffer[break_at] == b'\r' && break_at + 1 == self.buffer.len() {
                break;
            }

            let mut next = break_at + 1;
            if self.buffer[break_at] == b'\r' && self.buffer.get(next) == Some(&b'\n') {
                next += 1;
            }

            // `drain` removes the line *and* its terminator from the buffer.
            let line_bytes: Vec<u8> = self.buffer.drain(..next).collect();
            let line = String::from_utf8_lossy(&line_bytes[..break_at]).into_owned();

            if let Some(event) = self.consume_line(&line) {
                out.push(event);
            }
        }
    }

    /// Signal end-of-stream: flush whatever is still buffered.
    /// Some servers omit the final blank line, so a trailing event may only
    /// become visible here.
    pub fn finish(&mut self, out: &mut Vec<ServerSentEvent>) {
        if !self.buffer.is_empty() {
            let rest: Vec<u8> = core::mem::take(&mut self.buffer);
            let line = String::from_utf8_lossy(&rest).into_owned();
            if let Some(event) = self.consume_line(&line) {
                out.push(event);
            }
        }
        if let Some(event) = self.flush() {
            out.push(event);
        }
    }

    /// Process one decoded line, returning a finished event if the line was
    /// the blank separator.
    fn consume_line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }
        // Lines starting with ':' are comments (used by proxies as keepalive).
        if line.starts_with(':') {
            return None;
        }

        // Split into "field: value". A missing colon means the whole line is
        // the field name with an empty value.
        let (field, value) = match line.find(':') {
            Some(i) => {
                let value = &line[i + 1..];
                // The spec strips ONE leading space from the value.
                (&line[..i], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (line, ""),
        };

        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            // `id` and `retry` fields exist in the spec but no provider we
            // support uses them, so they are ignored - same as pi.
            _ => {}
        }
        None
    }

    /// Emit the event currently being assembled, if it has any content.
    fn flush(&mut self) -> Option<ServerSentEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(ServerSentEvent {
            event: self.event.take(),
            data: core::mem::take(&mut self.data).join("\n"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&str]) -> Vec<ServerSentEvent> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk.as_bytes(), &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    #[test]
    fn parses_a_simple_event() {
        let events = decode_all(&["event: ping\ndata: {\"a\":1}\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "{\"a\":1}");
    }

    #[test]
    fn handles_events_split_across_chunks() {
        // The event boundary lands mid-line to prove buffering works.
        let events = decode_all(&["event: mess", "age_start\ndata: 1\n", "\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
    }

    #[test]
    fn joins_multiple_data_lines_with_newline() {
        let events = decode_all(&["data: line1\ndata: line2\n\n"]);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn skips_comment_lines() {
        let events = decode_all(&[": keepalive\n\ndata: x\n\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn handles_crlf_split_across_chunks() {
        let events = decode_all(&["data: x\r", "\n\r\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn flushes_trailing_event_without_blank_line() {
        let events = decode_all(&["data: tail\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "tail");
    }
}
