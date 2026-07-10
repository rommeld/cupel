//! Tool output flows straight into the model's context window, so every tool
//! caps its output. Two independent limits apply - whichever is hit first
//! wins:
//! - line limit (default 2000 lines)
//! - byte limit (default 50 KB)
//!
//! Truncation never returns partial lines (except the tail-truncation edge
//! case where a single line exceeds the whole byte budget).

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Max characters per grep match line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which limit cut the content off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug, Clone)]
pub struct TruncationResult {
    /// The (possibly truncated) content.
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Tail truncation only: the first output line is a partial line.
    pub last_line_partial: bool,
    /// Head truncation only: the FIRST line alone exceeded the byte limit,
    /// so nothing could be kept.
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

/// Split for counting: a trailing newline does not create a phantom line.
fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let trimmed = content.strip_suffix('\n').unwrap_or(content);
    trimmed.split('\n').collect()
}

/// Format bytes human-readably: `512B`, `50.0KB`, `1.2MB`.
#[must_use]
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn untruncated(
    content: &str,
    total_lines: usize,
    total_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
) -> TruncationResult {
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes,
        output_lines: total_lines,
        output_bytes: total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the FIRST lines/bytes that fit. Suitable for file reads and search
/// output where the beginning matters.
#[must_use]
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return untruncated(content, total_lines, total_bytes, max_lines, max_bytes);
    }

    // The very first line exceeding the budget means we can keep nothing
    // (partial lines are never returned).
    if lines.first().is_some_and(|line| line.len() > max_bytes) {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut kept_bytes = 0_usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate().take(max_lines) {
        // +1 for the joining newline (except before the first line).
        let line_bytes = line.len() + usize::from(i > 0);
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        kept_bytes += line_bytes;
    }
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = kept.join("\n");
    TruncationResult {
        output_bytes: output.len(),
        output_lines: kept.len(),
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Keep the LAST lines/bytes that fit. Suitable for command output where the
/// end matters (errors, final results). Used by the future bash tool.
#[must_use]
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return untruncated(content, total_lines, total_bytes, max_lines, max_bytes);
    }

    let mut kept: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
    let mut kept_bytes = 0_usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    let mut partial: Option<String> = None;

    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if kept_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: not even one full line fits - keep the line's TAIL,
            // cut at a char boundary (Rust: byte slicing must respect UTF-8).
            if kept.is_empty() {
                let mut start = line.len().saturating_sub(max_bytes);
                while !line.is_char_boundary(start) {
                    start += 1;
                }
                partial = Some(line[start..].to_string());
                last_line_partial = true;
            }
            break;
        }
        kept.push_front(line);
        kept_bytes += line_bytes;
    }
    if kept.len() >= max_lines && kept_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output = partial.unwrap_or_else(|| kept.into_iter().collect::<Vec<_>>().join("\n"));
    TruncationResult {
        output_bytes: output.len(),
        output_lines: if last_line_partial {
            1
        } else {
            output.split('\n').count().min(max_lines)
        },
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a single line to `max_chars`, appending `... [truncated]`.
/// Counts CHARACTERS (not bytes) like pi, so multi-byte text truncates
/// consistently.
#[must_use]
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let prefix: String = line.chars().take(max_chars).collect();
    (format!("{prefix}... [truncated]"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_no_truncation_when_within_limits() {
        let result = truncate_head("a\nb\nc", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.content, "a\nb\nc");
        assert_eq!(result.total_lines, 3);
    }

    #[test]
    fn head_line_limit() {
        let result = truncate_head(
            "1\n2\n3\n4\n5",
            TruncationOptions {
                max_lines: Some(2),
                max_bytes: None,
            },
        );
        assert_eq!(result.content, "1\n2");
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
    }

    #[test]
    fn head_byte_limit_keeps_complete_lines() {
        let result = truncate_head(
            "aaaa\nbbbb\ncccc",
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(10),
            },
        );
        // "aaaa" (4) + "\nbbbb" (5) = 9 fits; adding "\ncccc" would exceed.
        assert_eq!(result.content, "aaaa\nbbbb");
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    }

    #[test]
    fn head_first_line_too_big() {
        let result = truncate_head(
            "aaaaaaaaaa\nb",
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(5),
            },
        );
        assert_eq!(result.content, "");
        assert!(result.first_line_exceeds_limit);
    }

    #[test]
    fn tail_keeps_last_lines() {
        let result = truncate_tail(
            "1\n2\n3\n4\n5",
            TruncationOptions {
                max_lines: Some(2),
                max_bytes: None,
            },
        );
        assert_eq!(result.content, "4\n5");
    }

    #[test]
    fn tail_partial_line_respects_utf8() {
        // One long line of multi-byte chars, tiny budget.
        let content = "éééééééééé"; // 10 chars, 20 bytes
        let result = truncate_tail(
            content,
            TruncationOptions {
                max_lines: None,
                max_bytes: Some(5),
            },
        );
        assert!(result.last_line_partial);
        // 5 bytes budget -> 2 chars (4 bytes) after boundary adjustment.
        assert_eq!(result.content, "éé");
    }

    #[test]
    fn line_truncation_counts_chars() {
        let (text, was_truncated) = truncate_line("abcdef", 4);
        assert!(was_truncated);
        assert_eq!(text, "abcd... [truncated]");
        let (text, was_truncated) = truncate_line("abc", 4);
        assert!(!was_truncated);
        assert_eq!(text, "abc");
    }

    #[test]
    fn format_size_examples() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(51200), "50.0KB");
    }
}
