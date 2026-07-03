//! Streaming-JSON parsing.
//!
//! While a model streams a tool call, the argument JSON arrives in fragments:
//! `{"pat` ... `{"pattern": "fo` ... `{"pattern": "foo"}`. pi keeps the
//! *parsed-so-far* object updated on every delta (via the `partial-json` npm
//! package) so UIs can render arguments live. This module is the Rust
//! equivalent.
//!
//! Strategy: try a normal parse first. If the document is incomplete, *repair*
//! it by closing whatever is still open (strings, objects, arrays), trimming a
//! trailing comma or dangling key, then parse again. If everything fails,
//! return an empty object - the contract is "always return a usable value,
//! never fail", because the final complete JSON will arrive eventually.

use serde_json::Value;

/// Parse potentially-incomplete JSON. Always returns a `Value`; incomplete or
/// malformed input degrades to `{}` rather than an error.
#[must_use]
pub fn parse_streaming_json(partial: &str) -> Value {
    let trimmed = partial.trim();
    if trimmed.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    // Fast path: the JSON is already complete.
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value;
    }
    // Slow path: close open structures and retry.
    if let Some(completed) = complete_json(trimmed)
        && let Ok(value) = serde_json::from_str::<Value>(&completed)
    {
        return value;
    }
    Value::Object(serde_json::Map::new())
}

/// Attempt to turn a JSON *prefix* into a complete document by appending the
/// missing closers. Returns `None` when the input can't be a JSON prefix.
///
/// This is a single left-to-right scan that tracks:
/// - whether we are inside a string (and whether the last char was `\`),
/// - the stack of open containers (`{` / `[`).
fn complete_json(input: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for c in input.chars() {
        if in_string {
            if escaped {
                escaped = false; // The escaped char itself; no state change.
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            // A closer must match the innermost opener. The pop in the guard
            // is intentional: it must run for every closer.
            '}' | ']' if stack.pop() != Some(c) => return None,
            _ => {}
        }
    }

    let mut completed = input.to_string();

    // A dangling escape (`"abc\`) can't be completed meaningfully: drop the
    // backslash so the string can be closed.
    if escaped {
        completed.pop();
    }
    if in_string {
        completed.push('"');
    }

    // Trim trailing commas / colons so `{"a": 1,` and `{"a":` become valid
    // once closed. A dangling key like `{"a"` needs a `: null`.
    let tail_trimmed = completed.trim_end();
    if tail_trimmed.ends_with(',') {
        completed.truncate(completed.trim_end().len() - 1);
    } else if tail_trimmed.ends_with(':') {
        completed.push_str(" null");
    } else if stack.last() == Some(&'}') && tail_trimmed.ends_with('"') {
        // Inside an object and the last complete token is a string. It could
        // be a key missing its value, or a complete value. Distinguish by the
        // preceding significant character: after `{` or `,` it's a key.
        let before = tail_trimmed[..tail_trimmed.len() - 1]
            .rfind('"')
            .map(|open| tail_trimmed[..open].trim_end())
            .and_then(|prefix| prefix.chars().last());
        if matches!(before, Some('{' | ',')) {
            completed.push_str(": null");
        }
    }

    // Close remaining open containers innermost-first.
    while let Some(closer) = stack.pop() {
        completed.push(closer);
    }
    Some(completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn complete_json_passes_through() {
        assert_eq!(
            parse_streaming_json(r#"{"pattern": "foo"}"#),
            json!({"pattern": "foo"})
        );
    }

    #[test]
    fn empty_input_yields_empty_object() {
        assert_eq!(parse_streaming_json(""), json!({}));
        assert_eq!(parse_streaming_json("   "), json!({}));
    }

    #[test]
    fn completes_open_string_value() {
        assert_eq!(
            parse_streaming_json(r#"{"pattern": "fo"#),
            json!({"pattern": "fo"})
        );
    }

    #[test]
    fn completes_dangling_key() {
        assert_eq!(
            parse_streaming_json(r#"{"pattern""#),
            json!({"pattern": null})
        );
        assert_eq!(
            parse_streaming_json(r#"{"pattern":"#),
            json!({"pattern": null})
        );
    }

    #[test]
    fn completes_nested_structures() {
        assert_eq!(
            parse_streaming_json(r#"{"a": [1, 2, {"b": "c"#),
            json!({"a": [1, 2, {"b": "c"}]})
        );
    }

    #[test]
    fn trailing_comma_is_trimmed() {
        assert_eq!(parse_streaming_json(r#"{"a": 1,"#), json!({"a": 1}));
    }

    #[test]
    fn garbage_degrades_to_empty_object() {
        assert_eq!(parse_streaming_json("}{"), json!({}));
        assert_eq!(parse_streaming_json("not json"), json!({}));
    }

    #[test]
    fn dangling_escape_is_dropped() {
        assert_eq!(parse_streaming_json(r#"{"a": "b\"#), json!({"a": "b"}));
    }
}
