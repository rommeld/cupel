use std::borrow::Cow;
use std::collections::HashMap;

use ra_ap_rustc_lexer::{FrontmatterAllowed, TokenKind};

const ANCHOR_COUNT: usize = 9;
const INLINE_PREFIX_TOKENS: usize = 4;

type Tokens<'source> = &'source [&'source str];

struct Source<'source> {
    code: Vec<CodeLine<'source>>,
    comments: Vec<Comment<'source>>,
}

struct CodeLine<'source> {
    number: usize,
    tokens: Vec<&'source str>,
}

struct Comment<'source> {
    text: &'source str,
    start: usize,
    end: usize,
}

struct LocatedComment<'source> {
    text: &'source str,
    line: usize,
    anchors: [Option<Anchor<'source>>; ANCHOR_COUNT],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Anchor<'source> {
    Surrounding {
        previous: Tokens<'source>,
        inline: Tokens<'source>,
        next: Tokens<'source>,
    },
    PreviousAndInline(Tokens<'source>, Tokens<'source>),
    InlineAndNext(Tokens<'source>, Tokens<'source>),
    Neighbors(Tokens<'source>, Tokens<'source>),
    Inline(Tokens<'source>),
    Next(Tokens<'source>),
    Previous(Tokens<'source>),
    TextOnly,
}

pub(crate) fn added_comment_lines(old: &str, new: &str) -> Vec<usize> {
    let old = normalize_line_endings(old);
    let new = normalize_line_endings(new);
    let old = Source::lex(&old);
    let new = Source::lex(&new);
    let mut old = old.locate_comments();
    let mut new = new.locate_comments();

    for pass in 0..ANCHOR_COUNT {
        let mut available: HashMap<_, Vec<_>> = HashMap::new();
        for (index, comment) in old.iter().enumerate().rev() {
            if let Some(comment) = comment
                && let Some(anchor) = comment.anchors[pass]
            {
                available
                    .entry((comment.text, anchor))
                    .or_default()
                    .push(index);
            }
        }
        for comment in &mut new {
            if let Some(candidate) = comment
                && let Some(anchor) = candidate.anchors[pass]
                && let Some(index) = available
                    .get_mut(&(candidate.text, anchor))
                    .and_then(Vec::pop)
            {
                old[index] = None;
                *comment = None;
            }
        }
    }

    new.into_iter()
        .flatten()
        .map(|comment| comment.line)
        .collect()
}

fn normalize_line_endings(source: &str) -> Cow<'_, str> {
    if source.contains("\r\n") {
        Cow::Owned(source.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(source)
    }
}

impl<'source> Source<'source> {
    fn lex(source: &'source str) -> Self {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let mut code: Vec<CodeLine<'source>> = Vec::new();
        let mut comments = Vec::new();
        let mut offset = ra_ap_rustc_lexer::strip_shebang(source).unwrap_or_default();
        let mut line = 1 + source[..offset].matches('\n').count();
        for token in ra_ap_rustc_lexer::tokenize(&source[offset..], FrontmatterAllowed::Yes) {
            let end = offset + token.len as usize;
            let text = &source[offset..end];
            let end_line = line + text.matches('\n').count();
            match token.kind {
                TokenKind::LineComment { doc_style: None }
                | TokenKind::BlockComment {
                    doc_style: None, ..
                } => {
                    comments.push(Comment {
                        text,
                        start: line,
                        end: end_line,
                    });
                }
                TokenKind::LineComment { .. }
                | TokenKind::BlockComment { .. }
                | TokenKind::Whitespace => {}
                _ => {
                    if let Some(current) = code.last_mut()
                        && current.number == line
                    {
                        current.tokens.push(text);
                    } else {
                        code.push(CodeLine {
                            number: line,
                            tokens: vec![text],
                        });
                    }
                }
            }
            offset = end;
            line = end_line;
        }
        Self { code, comments }
    }

    fn locate_comments(&self) -> Vec<Option<LocatedComment<'_>>> {
        self.comments
            .iter()
            .map(|comment| {
                let start = self
                    .code
                    .partition_point(|line| line.number < comment.start);
                let end = self.code.partition_point(|line| line.number <= comment.end);
                let previous = start.checked_sub(1).and_then(|index| self.code.get(index));
                let inline = self
                    .code
                    .get(start)
                    .filter(|line| line.number == comment.start);
                let next = self.code.get(end);
                Some(LocatedComment {
                    text: comment.text,
                    line: comment.start,
                    anchors: anchors(tokens(previous), tokens(inline), tokens(next)),
                })
            })
            .collect()
    }
}

fn tokens<'source>(line: Option<&'source CodeLine<'source>>) -> Tokens<'source> {
    line.map_or(&[], |line| line.tokens.as_slice())
}

fn anchors<'source>(
    previous: Tokens<'source>,
    inline: Tokens<'source>,
    next: Tokens<'source>,
) -> [Option<Anchor<'source>>; ANCHOR_COUNT] {
    let prefix = inline.get(..INLINE_PREFIX_TOKENS).unwrap_or(inline);
    let has_neighbors = !previous.is_empty() && !next.is_empty();
    [
        has_neighbors.then_some(Anchor::Surrounding {
            previous,
            inline: prefix,
            next,
        }),
        has_neighbors.then_some(Anchor::Neighbors(previous, next)),
        (!inline.is_empty()).then_some(Anchor::Inline(inline)),
        (!previous.is_empty() && !prefix.is_empty())
            .then_some(Anchor::PreviousAndInline(previous, prefix)),
        (!prefix.is_empty() && !next.is_empty()).then_some(Anchor::InlineAndNext(prefix, next)),
        (!prefix.is_empty()).then_some(Anchor::Inline(prefix)),
        (!next.is_empty()).then_some(Anchor::Next(next)),
        (!previous.is_empty()).then_some(Anchor::Previous(previous)),
        Some(Anchor::TextOnly),
    ]
}

#[cfg(test)]
#[path = "compare/tests.rs"]
mod tests;
