use std::borrow::Cow;
use std::collections::HashMap;

use ra_ap_rustc_lexer::{FrontmatterAllowed, TokenKind};

use crate::suppression::{Allow, AttributeStyle, BlanketLint};

const ANCHOR_COUNT: usize = 9;
const INLINE_PREFIX_TOKENS: usize = 4;

type Tokens<'source> = &'source [&'source str];

struct Source<'source> {
    tokens: Vec<&'source str>,
    code: Vec<CodeLine>,
    comments: Vec<Comment<'source>>,
}

struct CodeLine {
    number: usize,
    start: usize,
}

struct Comment<'source> {
    text: &'source str,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Subject<'source> {
    Comment(&'source str),
    BlanketAllow(AttributeStyle, BlanketLint),
}

struct Located<'source> {
    subject: Subject<'source>,
    line: usize,
    anchors: [Option<Anchor<'source>>; ANCHOR_COUNT],
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub line: usize,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Comment,
    BlanketAllow,
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

pub(crate) fn added_findings(old: &str, new: &str) -> Vec<Finding> {
    let old = normalize_line_endings(old);
    let new = normalize_line_endings(new);
    let old = Source::lex(&old);
    let new = Source::lex(&new);
    let mut old = old.locate();
    let mut new = new.locate();

    for pass in 0..ANCHOR_COUNT {
        let mut available: HashMap<_, Vec<_>> = HashMap::new();
        for (index, finding) in old.iter().enumerate().rev() {
            if let Some(finding) = finding
                && let Some(anchor) = finding.anchors[pass]
            {
                available
                    .entry((finding.subject, anchor))
                    .or_default()
                    .push(index);
            }
        }
        for finding in &mut new {
            if let Some(candidate) = finding
                && let Some(anchor) = candidate.anchors[pass]
                && let Some(index) = available
                    .get_mut(&(candidate.subject, anchor))
                    .and_then(Vec::pop)
            {
                old[index] = None;
                *finding = None;
            }
        }
    }

    let mut findings: Vec<_> = new
        .into_iter()
        .flatten()
        .map(|finding| Finding {
            line: finding.line,
            kind: match finding.subject {
                Subject::Comment(_) => Kind::Comment,
                Subject::BlanketAllow(..) => Kind::BlanketAllow,
            },
        })
        .collect();
    findings.sort_by_key(|finding| finding.line);
    findings.dedup_by(|left, right| left.kind == Kind::BlanketAllow && left == right);
    findings
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
        let mut code: Vec<CodeLine> = Vec::new();
        let mut tokens = Vec::new();
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
                    if code.last().is_none_or(|current| current.number != line) {
                        code.push(CodeLine {
                            number: line,
                            start: tokens.len(),
                        });
                    }
                    tokens.push(text);
                }
            }
            offset = end;
            line = end_line;
        }
        Self {
            tokens,
            code,
            comments,
        }
    }

    fn locate(&self) -> Vec<Option<Located<'_>>> {
        let mut located = Vec::new();
        for comment in &self.comments {
            let start = self
                .code
                .partition_point(|line| line.number < comment.start);
            let end = self.code.partition_point(|line| line.number <= comment.end);
            let previous = start
                .checked_sub(1)
                .map_or(&[][..], |i| self.line_tokens(i));
            let inline = if self
                .code
                .get(start)
                .is_some_and(|line| line.number == comment.start)
            {
                self.line_tokens(start)
            } else {
                &[]
            };
            located.push(Some(Located {
                subject: Subject::Comment(comment.text),
                line: comment.start,
                anchors: anchors(previous, inline, self.line_tokens(end)),
            }));
        }

        for (index, &token) in self.tokens.iter().enumerate() {
            if token != "#" {
                continue;
            }
            let Some(attribute) = Allow::parse(&self.tokens[index..]) else {
                continue;
            };
            let start = self.code.partition_point(|line| line.start <= index) - 1;
            let after = index + attribute.len;
            let next_line = self.code.partition_point(|line| line.start <= after);
            let next = &self.tokens[after..self.token_start(next_line)];
            let previous = start
                .checked_sub(1)
                .map_or(&[][..], |i| self.line_tokens(i));
            for lint in attribute.blanket_lints() {
                located.push(Some(Located {
                    subject: Subject::BlanketAllow(attribute.style, lint),
                    line: self.code[start].number,
                    anchors: anchors(previous, &[], next),
                }));
            }
        }
        located
    }

    fn line_tokens(&self, index: usize) -> Tokens<'_> {
        &self.tokens[self.token_start(index)..self.token_start(index + 1)]
    }

    fn token_start(&self, index: usize) -> usize {
        self.code
            .get(index)
            .map_or(self.tokens.len(), |line| line.start)
    }
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
