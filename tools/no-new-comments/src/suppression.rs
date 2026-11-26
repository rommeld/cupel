#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AttributeStyle {
    Inner,
    Outer,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BlanketLint {
    Warnings,
    ClippyAll,
}

pub(crate) struct Allow<'source> {
    pub style: AttributeStyle,
    pub len: usize,
    pub arguments: &'source [&'source str],
}

impl<'source> Allow<'source> {
    pub(crate) fn parse(tokens: &'source [&'source str]) -> Option<Self> {
        let after_hash = tokens.strip_prefix(&["#"])?;
        let (style, after_style) = match after_hash.strip_prefix(&["!"]) {
            Some(rest) => (AttributeStyle::Inner, rest),
            None => (AttributeStyle::Outer, after_hash),
        };
        let ["[", "allow" | "r#allow", "(", arguments @ ..] = after_style else {
            return None;
        };
        let close = arguments
            .iter()
            .position(|token| matches!(*token, ")" | "]" | "(" | "#"))?;
        if !arguments[close..].starts_with(&[")", "]"]) {
            return None;
        }
        Some(Self {
            style,
            len: tokens.len() - arguments.len() + close + 2,
            arguments: &arguments[..close],
        })
    }

    pub(crate) fn blanket_lints(&self) -> impl Iterator<Item = BlanketLint> {
        [BlanketLint::Warnings, BlanketLint::ClippyAll]
            .into_iter()
            .filter(|lint| {
                self.arguments
                    .split(|&token| token == ",")
                    .any(|argument| match lint {
                        BlanketLint::Warnings => matches!(argument, ["warnings" | "r#warnings"]),
                        BlanketLint::ClippyAll => {
                            matches!(argument, ["clippy" | "r#clippy", ":", ":", "all" | "r#all"])
                        }
                    })
            })
    }
}
