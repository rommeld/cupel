//! Compact token counts shared by the footer, transcript, and usage popup.

pub(crate) fn format(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_small_counts_and_rounds_thousands_to_one_decimal() {
        for (tokens, expected) in [
            (0, "0"),
            (1, "1"),
            (999, "999"),
            (1_000, "1.0k"),
            (1_234, "1.2k"),
            (1_299, "1.3k"),
            (48_000, "48.0k"),
            (1_000_000, "1000.0k"),
        ] {
            assert_eq!(super::format(tokens), expected, "{tokens} tokens");
        }
    }
}
