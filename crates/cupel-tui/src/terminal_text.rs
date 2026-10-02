//! Untrusted text must never become terminal commands in ratatui spans.

use std::borrow::Cow;

use unicode_width::UnicodeWidthChar as _;

const TAB_STOP: usize = 4;

enum State {
    Text,
    Escape,
    Csi,
    ControlString { osc: bool },
    StringEscape { osc: bool },
}

/// Expand tabs to four-column stops and remove terminal control sequences,
/// C0/C1 controls, and DEL. LF is retained only as a logical line separator.
/// Both ESC-prefixed and Unicode C1 CSI/OSC forms are recognized; other
/// escape/control strings (DCS/SOS/PM/APC) are stripped too.
///
/// Sanitize the complete text *before* splitting, previewing, or wrapping.
/// Incomplete sequences suppress their tail: streaming cells retain the raw
/// source and are sanitized again when the next delta arrives. Stored/model
/// text and explicit clipboard copying are not changed by rendering.
#[must_use]
pub(crate) fn sanitize(text: &str) -> Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut state = State::Text;
    let mut column = 0;
    for c in text.chars() {
        state = match state {
            State::Text => match c {
                '\x1b' => State::Escape,
                '\u{009b}' => State::Csi,
                '\u{009d}' => State::ControlString { osc: true },
                '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                    State::ControlString { osc: false }
                }
                '\t' => {
                    out.extend(std::iter::repeat_n(' ', TAB_STOP - column));
                    column = 0;
                    State::Text
                }
                '\n' => {
                    out.push('\n');
                    column = 0;
                    State::Text
                }
                c if c.is_control() => State::Text,
                c => {
                    out.push(c);
                    column = (column + c.width().unwrap_or(0)) % TAB_STOP;
                    State::Text
                }
            },
            State::Escape => match c {
                '[' => State::Csi,
                ']' => State::ControlString { osc: true },
                'P' | 'X' | '^' | '_' => State::ControlString { osc: false },
                '\x18' | '\x1a' => State::Text,
                '\x1b' | ' '..='/' => State::Escape,
                c if c.is_control() => State::Escape,
                _ => State::Text,
            },
            State::Csi => match c {
                '\x1b' => State::Escape,
                '\x18' | '\x1a' | '@'..='~' => State::Text,
                _ => State::Csi,
            },
            State::ControlString { osc } => match c {
                '\x07' if osc => State::Text,
                '\u{009c}' => State::Text,
                '\x1b' => State::StringEscape { osc },
                _ => State::ControlString { osc },
            },
            State::StringEscape { osc } => match c {
                '\\' | '\u{009c}' => State::Text,
                '\x07' if osc => State::Text,
                '\x1b' => State::StringEscape { osc },
                _ => State::ControlString { osc },
            },
        };
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use crate::terminal_text::sanitize;

    #[test]
    fn plain_unicode_and_newlines_are_borrowed_unchanged() {
        let text = "日本語 🙂 e\u{301}\nnext line";
        assert!(matches!(sanitize(text), Cow::Borrowed(s) if s == text));
    }

    #[test]
    fn tabs_expand_by_display_columns_and_reset_at_newlines() {
        assert_eq!(sanitize("\tgo build ./..."), "    go build ./...");
        assert_eq!(sanitize("a\tb\t\n\tend"), "a   b   \n    end");
        assert_eq!(sanitize("日\tx\ne\u{301}\tx"), "日  x\ne\u{301}   x");
    }

    #[test]
    fn csi_colors_cursor_and_private_modes_are_removed() {
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize("a\x1b[2J\x1b[1;1Hb\x1b[?25lc"), "abc");
        assert_eq!(sanitize("\u{009b}31mred\u{009b}0m"), "red");
        assert_eq!(sanitize("\x1b(Btext\x1b7\x1b8"), "text");
    }

    #[test]
    fn osc_clipboard_title_and_links_are_removed_with_all_terminators() {
        for sequence in [
            "\x1b]52;c;c3RvbGVu\x07",
            "\x1b]52;c;c3RvbGVu\x1b\\",
            "\u{009d}52;c;c3RvbGVu\u{009c}",
            "\x1b]0;title\u{009c}",
            "\u{009d}0;title\x07",
        ] {
            assert_eq!(sanitize(&format!("before{sequence}after")), "beforeafter");
        }
        assert_eq!(
            sanitize("\x1b]8;;https://evil.invalid\x1b\\label\x1b]8;;\x1b\\"),
            "label"
        );
    }

    #[test]
    fn control_strings_and_multiline_payloads_are_removed_before_line_splitting() {
        for start in [
            "\x1bP", "\x1bX", "\x1b^", "\x1b_", "\u{0090}", "\u{0098}", "\u{009e}", "\u{009f}",
        ] {
            assert_eq!(
                sanitize(&format!("a{start}payload\n\tpayload\x1b\\b")),
                "ab"
            );
        }
        assert_eq!(sanitize("a\x1b]52;c;payload\nmore\x07b"), "ab");
    }

    #[test]
    fn all_other_controls_are_removed_and_crlf_keeps_the_line_break() {
        assert_eq!(sanitize("a\0\x07\x08\x0b\x0c\r\x7f\u{0085}b\r\nc"), "ab\nc");
        for code in (0..=0x1f).chain(0x7f..=0x9f) {
            let c = char::from_u32(code).unwrap();
            let input = format!("a{c}");
            let output = sanitize(&input);
            assert!(
                output.chars().all(|c| !c.is_control() || c == '\n'),
                "{code:#x}"
            );
            assert!(!output.contains('\t'));
        }
    }

    #[test]
    fn incomplete_sequences_never_leak_and_sanitizing_is_idempotent() {
        for tail in [
            "\x1b",
            "\x1b[31",
            "\u{009b}1;",
            "\x1b]52;c;payload",
            "\x1b]52;c;payload\x1b",
            "\u{009d}52;c;payload",
        ] {
            assert_eq!(sanitize(&format!("visible{tail}")), "visible");
        }
        let output = sanitize("\t日\x1b[31mred\x1b[0m\n\x1b]52;c;eA==\x07done");
        assert_eq!(sanitize(&output), output);
    }
}
