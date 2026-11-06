//! Compact token counts shared by the footer, transcript, and usage popup.

pub(crate) fn format(tokens: u64) -> String {
    let (scale, suffix) = match tokens {
        0..1_000 => return tokens.to_string(),
        1_000..1_000_000 => (1_000.0, "k"),
        1_000_000..1_000_000_000 => (1_000_000.0, "M"),
        1_000_000_000..1_000_000_000_000 => (1_000_000_000.0, "B"),
        _ => (1_000_000_000_000.0, "T"),
    };
    format!("{:.1}{suffix}", tokens as f64 / scale)
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_small_counts_and_rounds_larger_units_to_one_decimal() {
        for (tokens, expected) in [
            (0, "0"),
            (1, "1"),
            (999, "999"),
            (1_000, "1.0k"),
            (1_234, "1.2k"),
            (1_299, "1.3k"),
            (48_000, "48.0k"),
            (999_000, "999.0k"),
            (1_000_000, "1.0M"),
            (1_234_567, "1.2M"),
            (1_299_999, "1.3M"),
            (999_000_000, "999.0M"),
            (1_000_000_000, "1.0B"),
            (3_456_789_012, "3.5B"),
            (999_000_000_000, "999.0B"),
            (1_000_000_000_000, "1.0T"),
            (5_678_901_234_567, "5.7T"),
            (u64::MAX, "18446744.1T"),
        ] {
            assert_eq!(super::format(tokens), expected, "{tokens} tokens");
        }
    }
}
