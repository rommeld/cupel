use std::ops::Range;

// ---------------------------------------------------------------------------
// SecondaryHunkStatus — staging awareness for diff hunks
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondaryHunkStatus {
    /// Hunk does not exist in the secondary diff — fully staged.
    NoSecondaryHunk,
    /// Hunk exists identically in the secondary diff — fully unstaged.
    HasSecondaryHunk,
    /// Hunk partially overlaps the secondary diff — partially staged.
    OverlapsWithSecondaryHunk,
}

impl Default for SecondaryHunkStatus {
    fn default() -> Self {
        Self::HasSecondaryHunk
    }
}

// ---------------------------------------------------------------------------
// DiffHunk
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    /// Line range in the current buffer (0-based).
    pub buffer_range: Range<u32>,
    /// Byte range in `base_text` that this hunk replaces.
    pub diff_base_byte_range: Range<usize>,
    /// Staging status relative to a secondary diff.
    pub secondary_status: SecondaryHunkStatus,
}

// ---------------------------------------------------------------------------
// BufferDiffSnapshot — cheap-to-clone value type
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct BufferDiffSnapshot {
    pub hunks: Vec<DiffHunk>,
    pub base_text: Option<String>,
}

impl BufferDiffSnapshot {
    pub fn new(base_text: Option<String>) -> Self {
        Self {
            hunks: Vec::new(),
            base_text,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Return hunks whose buffer_range intersects the given line range.
    pub fn hunks_intersecting_range(&self, range: Range<u32>) -> Vec<&DiffHunk> {
        self.hunks
            .iter()
            .filter(|h| ranges_overlap(&h.buffer_range, &range))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Diff computation using imara-diff
// ---------------------------------------------------------------------------

/// Compute diff hunks between base and current text using imara-diff.
pub fn compute_diff_hunks(current: &str, base: Option<&str>) -> Vec<DiffHunk> {
    let base = match base {
        Some(b) => b,
        None => return vec![],
    };

    if base == current {
        return vec![];
    }

    let input = imara_diff::intern::InternedInput::new(base, current);
    let sink = HunkCollector {
        hunks: Vec::new(),
        base_text: base,
    };

    imara_diff::diff(imara_diff::Algorithm::Histogram, &input, sink)
}

struct HunkCollector<'a> {
    hunks: Vec<DiffHunk>,
    base_text: &'a str,
}

impl imara_diff::Sink for HunkCollector<'_> {
    type Out = Vec<DiffHunk>;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        let base_byte_start = line_to_byte_offset(self.base_text, before.start as usize);
        let base_byte_end = line_to_byte_offset(self.base_text, before.end as usize);

        self.hunks.push(DiffHunk {
            buffer_range: after,
            diff_base_byte_range: base_byte_start..base_byte_end,
            secondary_status: SecondaryHunkStatus::default(),
        });
    }

    fn finish(self) -> Self::Out {
        self.hunks
    }
}

/// Convert a line number (0-based) to a byte offset in the text.
fn line_to_byte_offset(text: &str, line: usize) -> usize {
    let mut current_line = 0;
    for (i, c) in text.char_indices() {
        if current_line == line {
            return i;
        }
        if c == '\n' {
            current_line += 1;
        }
    }
    text.len()
}

// ---------------------------------------------------------------------------
// Secondary status computation
// ---------------------------------------------------------------------------

/// Compare a hunk's buffer range against secondary diff hunks to determine
/// staging status.
pub fn compute_secondary_status(
    hunk: &DiffHunk,
    secondary_hunks: &[DiffHunk],
) -> SecondaryHunkStatus {
    let overlapping: Vec<_> = secondary_hunks
        .iter()
        .filter(|h| ranges_overlap(&h.buffer_range, &hunk.buffer_range))
        .collect();

    if overlapping.is_empty() {
        // Change is in index but not in HEAD → fully staged
        SecondaryHunkStatus::NoSecondaryHunk
    } else if overlapping
        .iter()
        .all(|h| h.buffer_range == hunk.buffer_range)
    {
        // Change exists in both diffs identically → fully unstaged
        SecondaryHunkStatus::HasSecondaryHunk
    } else {
        // Partial overlap → partially staged
        SecondaryHunkStatus::OverlapsWithSecondaryHunk
    }
}

/// Annotate hunks with secondary status computed against another set of hunks.
pub fn annotate_hunks_with_secondary(
    hunks: &mut [DiffHunk],
    secondary_hunks: &[DiffHunk],
) {
    for hunk in hunks.iter_mut() {
        hunk.secondary_status = compute_secondary_status(hunk, secondary_hunks);
    }
}

fn ranges_overlap(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start < b.end && b.start < a.end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_texts_no_hunks() {
        let hunks = compute_diff_hunks("hello\nworld\n", Some("hello\nworld\n"));
        assert!(hunks.is_empty());
    }

    #[test]
    fn test_none_base_no_hunks() {
        let hunks = compute_diff_hunks("hello\n", None);
        assert!(hunks.is_empty());
    }

    #[test]
    fn test_added_lines() {
        let base = "line1\nline2\n";
        let current = "line1\nline2\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        assert!(!hunks.is_empty());
    }

    #[test]
    fn test_deleted_lines() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        assert!(!hunks.is_empty());
    }

    #[test]
    fn test_modified_lines() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn test_hunks_intersecting_range() {
        let snapshot = BufferDiffSnapshot {
            hunks: vec![
                DiffHunk {
                    buffer_range: 0..3,
                    diff_base_byte_range: 0..10,
                    secondary_status: SecondaryHunkStatus::default(),
                },
                DiffHunk {
                    buffer_range: 5..8,
                    diff_base_byte_range: 10..20,
                    secondary_status: SecondaryHunkStatus::default(),
                },
                DiffHunk {
                    buffer_range: 10..12,
                    diff_base_byte_range: 20..30,
                    secondary_status: SecondaryHunkStatus::default(),
                },
            ],
            base_text: None,
        };

        let result = snapshot.hunks_intersecting_range(4..9);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].buffer_range, 5..8);

        let result = snapshot.hunks_intersecting_range(0..12);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_ranges_overlap() {
        assert!(ranges_overlap(&(0..5), &(3..8)));
        assert!(ranges_overlap(&(3..8), &(0..5)));
        assert!(!ranges_overlap(&(0..3), &(3..6))); // adjacent, not overlapping
        assert!(!ranges_overlap(&(0..3), &(5..8)));
        assert!(ranges_overlap(&(0..5), &(0..5))); // identical
    }

    #[test]
    fn test_secondary_status_no_overlap() {
        let hunk = DiffHunk {
            buffer_range: 0..5,
            diff_base_byte_range: 0..10,
            secondary_status: SecondaryHunkStatus::default(),
        };
        let secondary = vec![DiffHunk {
            buffer_range: 10..15,
            diff_base_byte_range: 0..10,
            secondary_status: SecondaryHunkStatus::default(),
        }];

        assert_eq!(
            compute_secondary_status(&hunk, &secondary),
            SecondaryHunkStatus::NoSecondaryHunk
        );
    }

    #[test]
    fn test_secondary_status_exact_match() {
        let hunk = DiffHunk {
            buffer_range: 0..5,
            diff_base_byte_range: 0..10,
            secondary_status: SecondaryHunkStatus::default(),
        };
        let secondary = vec![DiffHunk {
            buffer_range: 0..5,
            diff_base_byte_range: 0..20,
            secondary_status: SecondaryHunkStatus::default(),
        }];

        assert_eq!(
            compute_secondary_status(&hunk, &secondary),
            SecondaryHunkStatus::HasSecondaryHunk
        );
    }

    #[test]
    fn test_secondary_status_partial_overlap() {
        let hunk = DiffHunk {
            buffer_range: 0..5,
            diff_base_byte_range: 0..10,
            secondary_status: SecondaryHunkStatus::default(),
        };
        let secondary = vec![DiffHunk {
            buffer_range: 3..8,
            diff_base_byte_range: 0..20,
            secondary_status: SecondaryHunkStatus::default(),
        }];

        assert_eq!(
            compute_secondary_status(&hunk, &secondary),
            SecondaryHunkStatus::OverlapsWithSecondaryHunk
        );
    }

    #[test]
    fn test_secondary_status_empty_secondary() {
        let hunk = DiffHunk {
            buffer_range: 0..5,
            diff_base_byte_range: 0..10,
            secondary_status: SecondaryHunkStatus::default(),
        };

        assert_eq!(
            compute_secondary_status(&hunk, &[]),
            SecondaryHunkStatus::NoSecondaryHunk
        );
    }

    #[test]
    fn test_annotate_hunks() {
        let mut hunks = vec![
            DiffHunk {
                buffer_range: 0..5,
                diff_base_byte_range: 0..10,
                secondary_status: SecondaryHunkStatus::default(),
            },
            DiffHunk {
                buffer_range: 10..15,
                diff_base_byte_range: 10..20,
                secondary_status: SecondaryHunkStatus::default(),
            },
        ];
        let secondary = vec![DiffHunk {
            buffer_range: 0..5,
            diff_base_byte_range: 0..10,
            secondary_status: SecondaryHunkStatus::default(),
        }];

        annotate_hunks_with_secondary(&mut hunks, &secondary);

        assert_eq!(hunks[0].secondary_status, SecondaryHunkStatus::HasSecondaryHunk);
        assert_eq!(hunks[1].secondary_status, SecondaryHunkStatus::NoSecondaryHunk);
    }
}
