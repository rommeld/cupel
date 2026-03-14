use std::ops::Range;
use std::sync::Arc;

use gpui::{Context, EventEmitter, Task};

// ---------------------------------------------------------------------------
// DiffHunkStatus — classification of a hunk
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffHunkStatus {
    /// Lines added (exist in buffer but not in base).
    Added,
    /// Lines modified (exist in both buffer and base, but differ).
    Modified,
    /// Lines deleted (exist in base but not in buffer).
    Deleted,
}

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
// InternalDiffHunk — storage type (anchors will replace line ranges later)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalDiffHunk {
    /// Line range in the current buffer (0-based).
    pub buffer_range: Range<u32>,
    /// Byte range in `base_text` that this hunk replaces.
    pub diff_base_byte_range: Range<usize>,
    /// Classification: Added, Modified, or Deleted.
    pub status: DiffHunkStatus,
    /// Word-level diff ranges within the buffer text (for Modified hunks ≤5 lines).
    pub buffer_word_diffs: Vec<Range<usize>>,
    /// Word-level diff ranges within the base text (for Modified hunks ≤5 lines).
    pub base_word_diffs: Vec<Range<usize>>,
}

impl InternalDiffHunk {
    /// Resolve this internal hunk into a public `DiffHunk`.
    pub fn to_diff_hunk(&self) -> DiffHunk {
        DiffHunk {
            buffer_range: self.buffer_range.clone(),
            diff_base_byte_range: self.diff_base_byte_range.clone(),
            status: self.status,
            secondary_status: SecondaryHunkStatus::default(),
            buffer_word_diffs: self.buffer_word_diffs.clone(),
            base_word_diffs: self.base_word_diffs.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// DiffHunk — public API type resolved from InternalDiffHunk
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    /// Line range in the current buffer (0-based).
    pub buffer_range: Range<u32>,
    /// Byte range in `base_text` that this hunk replaces.
    pub diff_base_byte_range: Range<usize>,
    /// Classification: Added, Modified, or Deleted.
    pub status: DiffHunkStatus,
    /// Staging status relative to a secondary diff.
    pub secondary_status: SecondaryHunkStatus,
    /// Word-level diff ranges within the buffer text.
    pub buffer_word_diffs: Vec<Range<usize>>,
    /// Word-level diff ranges within the base text.
    pub base_word_diffs: Vec<Range<usize>>,
}

// ---------------------------------------------------------------------------
// BufferDiffSnapshot — cheap-to-clone value type
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct BufferDiffSnapshot {
    hunks: Arc<Vec<InternalDiffHunk>>,
    pub base_text: Option<String>,
}

impl BufferDiffSnapshot {
    pub fn new(base_text: Option<String>) -> Self {
        Self {
            hunks: Arc::new(Vec::new()),
            base_text,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Return resolved DiffHunks whose buffer_range intersects the given line range.
    pub fn hunks_intersecting_range(&self, range: Range<u32>) -> Vec<DiffHunk> {
        self.hunks
            .iter()
            .filter(|h| ranges_overlap(&h.buffer_range, &range))
            .map(|h| h.to_diff_hunk())
            .collect()
    }

    /// Return all resolved DiffHunks.
    pub fn hunks(&self) -> Vec<DiffHunk> {
        self.hunks.iter().map(|h| h.to_diff_hunk()).collect()
    }

    /// Access the internal hunks directly.
    pub fn internal_hunks(&self) -> &[InternalDiffHunk] {
        &self.hunks
    }

    /// Set internal hunks (used by BufferDiff after recomputation).
    pub fn set_hunks(&mut self, hunks: Vec<InternalDiffHunk>) {
        self.hunks = Arc::new(hunks);
    }
}

// ---------------------------------------------------------------------------
// BufferDiff — GPUI entity that manages diff state for a single buffer
// ---------------------------------------------------------------------------

/// Event emitted when the diff snapshot changes.
#[derive(Clone, Debug)]
pub struct BufferDiffEvent;

/// Maximum line count for word-level diff computation.
const MAX_WORD_DIFF_LINE_COUNT: u32 = 5;

/// GPUI entity that holds base text and current text, recomputes hunks on
/// change, and exposes a cheap-to-clone `BufferDiffSnapshot`.
pub struct BufferDiff {
    snapshot: BufferDiffSnapshot,
    current_text: String,
    _recalculate_task: Option<Task<()>>,
}

impl EventEmitter<BufferDiffEvent> for BufferDiff {}

impl BufferDiff {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            snapshot: BufferDiffSnapshot::default(),
            current_text: String::new(),
            _recalculate_task: None,
        }
    }

    /// Replace the base text and schedule recomputation.
    pub fn set_base_text(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        self.snapshot.base_text = text;
        self.recalculate(cx);
    }

    /// Replace the current (buffer) text and schedule recomputation.
    pub fn set_current_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.current_text = text;
        self.recalculate(cx);
    }

    /// Get the current snapshot (O(1) Arc clone).
    pub fn snapshot(&self) -> BufferDiffSnapshot {
        self.snapshot.clone()
    }

    /// Get the base text.
    pub fn base_text(&self) -> Option<&str> {
        self.snapshot.base_text.as_deref()
    }

    /// Get the current text.
    pub fn current_text(&self) -> &str {
        &self.current_text
    }

    /// Compute new index text by applying staged hunks (replacing base text
    /// ranges with buffer text). Returns the new index content.
    pub fn stage_hunks(&self, hunk_indices: &[usize]) -> Option<String> {
        let base = self.snapshot.base_text.as_deref()?;
        let hunks = self.snapshot.internal_hunks();

        let mut ops: Vec<(Range<usize>, &str)> = hunk_indices
            .iter()
            .filter_map(|&idx| hunks.get(idx))
            .map(|hunk| {
                let buffer_slice = text_for_line_range(&self.current_text, &hunk.buffer_range);
                (hunk.diff_base_byte_range.clone(), buffer_slice)
            })
            .collect();

        ops.sort_by_key(|(range, _)| range.start);
        Some(apply_patch(base, &ops))
    }

    /// Compute new index text by removing staged hunks (restoring base text
    /// for those ranges). Returns the new index content.
    pub fn unstage_hunks(&self, hunk_indices: &[usize]) -> Option<String> {
        let base = self.snapshot.base_text.as_deref()?;
        let hunks = self.snapshot.internal_hunks();

        let mut ops: Vec<(Range<usize>, &str)> = hunk_indices
            .iter()
            .filter_map(|&idx| hunks.get(idx))
            .map(|hunk| {
                let base_slice = &base[hunk.diff_base_byte_range.clone()];
                (hunk.diff_base_byte_range.clone(), base_slice)
            })
            .collect();

        ops.sort_by_key(|(range, _)| range.start);
        Some(apply_patch(base, &ops))
    }

    fn recalculate(&mut self, cx: &mut Context<Self>) {
        // Drop any in-flight recalculation.
        self._recalculate_task = None;

        let base = self.snapshot.base_text.clone();
        let current = self.current_text.clone();

        let task = cx.spawn(async move |entity, cx| {
            // Compute on the async task (would use background_spawn in real Zed).
            let mut hunks = compute_diff_hunks(&current, base.as_deref());

            // Word-level diff for small Modified hunks (child 1.3).
            if let Some(base_text) = base.as_deref() {
                compute_word_diffs(&mut hunks, &current, base_text);
            }

            entity
                .update(cx, |this: &mut BufferDiff, cx| {
                    this.snapshot.set_hunks(hunks);
                    cx.emit(BufferDiffEvent);
                    cx.notify();
                })
                .ok();
        });

        self._recalculate_task = Some(task);
    }
}

/// Extract text for a line range from the full text.
fn text_for_line_range<'a>(text: &'a str, range: &Range<u32>) -> &'a str {
    let start = line_to_byte_offset(text, range.start as usize);
    let end = line_to_byte_offset(text, range.end as usize);
    &text[start..end]
}

/// Apply a sorted list of (base_range, replacement) patches to base text.
/// Patches must be sorted by range.start and non-overlapping.
fn apply_patch(base: &str, ops: &[(Range<usize>, &str)]) -> String {
    let mut result = String::with_capacity(base.len());
    let mut cursor = 0;

    for (range, replacement) in ops {
        if range.start > cursor {
            result.push_str(&base[cursor..range.start]);
        }
        result.push_str(replacement);
        cursor = range.end;
    }

    if cursor < base.len() {
        result.push_str(&base[cursor..]);
    }
    result
}

// ---------------------------------------------------------------------------
// Word-level diff (child 1.3)
// ---------------------------------------------------------------------------

/// Compute word-level diffs for Modified hunks with ≤ MAX_WORD_DIFF_LINE_COUNT lines.
fn compute_word_diffs(hunks: &mut [InternalDiffHunk], current: &str, base: &str) {
    for hunk in hunks.iter_mut() {
        if hunk.status != DiffHunkStatus::Modified {
            continue;
        }

        let line_count = hunk.buffer_range.end - hunk.buffer_range.start;
        if line_count > MAX_WORD_DIFF_LINE_COUNT {
            continue;
        }

        let buf_start = line_to_byte_offset(current, hunk.buffer_range.start as usize);
        let buf_end = line_to_byte_offset(current, hunk.buffer_range.end as usize);
        let buf_slice = &current[buf_start..buf_end];

        let base_slice = &base[hunk.diff_base_byte_range.clone()];

        let (buf_diffs, base_diffs) = compute_word_diff_ranges(buf_slice, base_slice);

        // Offset word diff ranges to be relative to the full text.
        hunk.buffer_word_diffs = buf_diffs
            .into_iter()
            .map(|r| (r.start + buf_start)..(r.end + buf_start))
            .collect();
        hunk.base_word_diffs = base_diffs
            .into_iter()
            .map(|r| {
                (r.start + hunk.diff_base_byte_range.start)
                    ..(r.end + hunk.diff_base_byte_range.start)
            })
            .collect();
    }
}

/// Compute word-level diff ranges between current and base text slices.
/// Returns (current_word_ranges, base_word_ranges) — byte offsets relative to
/// the input slices.
///
/// Uses character-level imara-diff comparison. Words are identified as
/// contiguous non-whitespace runs. Changed characters are merged into word
/// boundary-aligned ranges.
fn compute_word_diff_ranges(
    current: &str,
    base: &str,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    // Use byte-level diff (imara-diff supports &[u8] as TokenSource).
    let input =
        imara_diff::intern::InternedInput::new(base.as_bytes(), current.as_bytes());

    let collector = ByteDiffCollector {
        current_ranges: Vec::new(),
        base_ranges: Vec::new(),
    };

    let (cur_byte_ranges, base_byte_ranges) =
        imara_diff::diff(imara_diff::Algorithm::Histogram, &input, collector);

    // Expand byte ranges to word boundaries.
    let cur_word_ranges = expand_to_word_boundaries(current, &cur_byte_ranges);
    let base_word_ranges = expand_to_word_boundaries(base, &base_byte_ranges);

    (cur_word_ranges, base_word_ranges)
}

struct ByteDiffCollector {
    current_ranges: Vec<Range<usize>>,
    base_ranges: Vec<Range<usize>>,
}

impl imara_diff::Sink for ByteDiffCollector {
    type Out = (Vec<Range<usize>>, Vec<Range<usize>>);

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        if after.start < after.end {
            self.current_ranges
                .push(after.start as usize..after.end as usize);
        }
        if before.start < before.end {
            self.base_ranges
                .push(before.start as usize..before.end as usize);
        }
    }

    fn finish(self) -> Self::Out {
        (self.current_ranges, self.base_ranges)
    }
}

/// Expand byte-level change ranges to encompass full words (non-whitespace runs).
fn expand_to_word_boundaries(text: &str, ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut result: Vec<Range<usize>> = Vec::new();

    for range in ranges {
        // Expand start backward to word boundary.
        let mut start = range.start;
        while start > 0 && !bytes[start - 1].is_ascii_whitespace() {
            start -= 1;
        }

        // Expand end forward to word boundary.
        let mut end = range.end;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }

        // Merge with previous range if overlapping or adjacent.
        if let Some(last) = result.last_mut() {
            if start <= last.end {
                last.end = last.end.max(end);
                continue;
            }
        }
        result.push(start..end);
    }
    result
}

// ---------------------------------------------------------------------------
// Diff computation using imara-diff
// ---------------------------------------------------------------------------

/// Compute diff hunks between base and current text using imara-diff.
pub fn compute_diff_hunks(current: &str, base: Option<&str>) -> Vec<InternalDiffHunk> {
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
    hunks: Vec<InternalDiffHunk>,
    base_text: &'a str,
}

impl imara_diff::Sink for HunkCollector<'_> {
    type Out = Vec<InternalDiffHunk>;

    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        let base_byte_start = line_to_byte_offset(self.base_text, before.start as usize);
        let base_byte_end = line_to_byte_offset(self.base_text, before.end as usize);

        let status = if before.start == before.end {
            DiffHunkStatus::Added
        } else if after.start == after.end {
            DiffHunkStatus::Deleted
        } else {
            DiffHunkStatus::Modified
        };

        self.hunks.push(InternalDiffHunk {
            buffer_range: after,
            diff_base_byte_range: base_byte_start..base_byte_end,
            status,
            buffer_word_diffs: Vec::new(),
            base_word_diffs: Vec::new(),
        });
    }

    fn finish(self) -> Self::Out {
        self.hunks
    }
}

/// Convert a line number (0-based) to a byte offset in the text.
pub fn line_to_byte_offset(text: &str, line: usize) -> usize {
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

/// Count the number of lines in a byte range of text.
/// Returns at least 1 for non-empty ranges.
pub fn count_lines_in_byte_range(text: Option<&str>, range: &Range<usize>) -> u32 {
    let Some(text) = text else {
        return 0;
    };
    let slice = &text[range.start.min(text.len())..range.end.min(text.len())];
    slice.lines().count().max(1) as u32
}

// ---------------------------------------------------------------------------
// Secondary status computation
// ---------------------------------------------------------------------------

/// Compare a hunk's buffer range against secondary diff hunks to determine
/// staging status.
pub fn compute_secondary_status(
    buffer_range: &Range<u32>,
    secondary_hunks: &[InternalDiffHunk],
) -> SecondaryHunkStatus {
    let overlapping: Vec<_> = secondary_hunks
        .iter()
        .filter(|h| ranges_overlap(&h.buffer_range, buffer_range))
        .collect();

    if overlapping.is_empty() {
        // Change is in index but not in HEAD → fully staged
        SecondaryHunkStatus::NoSecondaryHunk
    } else if overlapping
        .iter()
        .all(|h| h.buffer_range == *buffer_range)
    {
        // Change exists in both diffs identically → fully unstaged
        SecondaryHunkStatus::HasSecondaryHunk
    } else {
        // Partial overlap → partially staged
        SecondaryHunkStatus::OverlapsWithSecondaryHunk
    }
}

/// Annotate resolved DiffHunks with secondary status computed against
/// internal hunks from another diff.
pub fn annotate_hunks_with_secondary(
    hunks: &mut [DiffHunk],
    secondary_hunks: &[InternalDiffHunk],
) {
    for hunk in hunks.iter_mut() {
        hunk.secondary_status = compute_secondary_status(&hunk.buffer_range, secondary_hunks);
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
        assert_eq!(hunks[0].status, DiffHunkStatus::Added);
    }

    #[test]
    fn test_deleted_lines() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        assert!(!hunks.is_empty());
        assert_eq!(hunks[0].status, DiffHunkStatus::Deleted);
    }

    #[test]
    fn test_modified_lines() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].status, DiffHunkStatus::Modified);
    }

    #[test]
    fn test_hunks_intersecting_range() {
        let mut snapshot = BufferDiffSnapshot::new(None);
        snapshot.set_hunks(vec![
            InternalDiffHunk {
                buffer_range: 0..3,
                diff_base_byte_range: 0..10,
                status: DiffHunkStatus::Modified,
                buffer_word_diffs: Vec::new(),
                base_word_diffs: Vec::new(),
            },
            InternalDiffHunk {
                buffer_range: 5..8,
                diff_base_byte_range: 10..20,
                status: DiffHunkStatus::Modified,
                buffer_word_diffs: Vec::new(),
                base_word_diffs: Vec::new(),
            },
            InternalDiffHunk {
                buffer_range: 10..12,
                diff_base_byte_range: 20..30,
                status: DiffHunkStatus::Added,
                buffer_word_diffs: Vec::new(),
                base_word_diffs: Vec::new(),
            },
        ]);

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

    fn make_internal_hunk(buffer_range: Range<u32>) -> InternalDiffHunk {
        InternalDiffHunk {
            buffer_range: buffer_range.clone(),
            diff_base_byte_range: 0..10,
            status: DiffHunkStatus::Modified,
            buffer_word_diffs: Vec::new(),
            base_word_diffs: Vec::new(),
        }
    }

    #[test]
    fn test_secondary_status_no_overlap() {
        let secondary = vec![make_internal_hunk(10..15)];
        assert_eq!(
            compute_secondary_status(&(0..5), &secondary),
            SecondaryHunkStatus::NoSecondaryHunk
        );
    }

    #[test]
    fn test_secondary_status_exact_match() {
        let secondary = vec![make_internal_hunk(0..5)];
        assert_eq!(
            compute_secondary_status(&(0..5), &secondary),
            SecondaryHunkStatus::HasSecondaryHunk
        );
    }

    #[test]
    fn test_secondary_status_partial_overlap() {
        let secondary = vec![make_internal_hunk(3..8)];
        assert_eq!(
            compute_secondary_status(&(0..5), &secondary),
            SecondaryHunkStatus::OverlapsWithSecondaryHunk
        );
    }

    #[test]
    fn test_secondary_status_empty_secondary() {
        assert_eq!(
            compute_secondary_status(&(0..5), &[]),
            SecondaryHunkStatus::NoSecondaryHunk
        );
    }

    #[test]
    fn test_annotate_hunks() {
        let hunks = vec![
            InternalDiffHunk {
                buffer_range: 0..5,
                diff_base_byte_range: 0..10,
                status: DiffHunkStatus::Modified,
                buffer_word_diffs: Vec::new(),
                base_word_diffs: Vec::new(),
            },
            InternalDiffHunk {
                buffer_range: 10..15,
                diff_base_byte_range: 10..20,
                status: DiffHunkStatus::Modified,
                buffer_word_diffs: Vec::new(),
                base_word_diffs: Vec::new(),
            },
        ];
        let secondary_internal = vec![make_internal_hunk(0..5)];

        let mut resolved: Vec<DiffHunk> = hunks.iter().map(|h| h.to_diff_hunk()).collect();
        annotate_hunks_with_secondary(&mut resolved, &secondary_internal);

        assert_eq!(resolved[0].secondary_status, SecondaryHunkStatus::HasSecondaryHunk);
        assert_eq!(resolved[1].secondary_status, SecondaryHunkStatus::NoSecondaryHunk);
    }

    #[test]
    fn test_snapshot_arc_clone_is_cheap() {
        let mut snapshot = BufferDiffSnapshot::new(Some("base".to_string()));
        snapshot.set_hunks(vec![make_internal_hunk(0..5)]);

        let clone = snapshot.clone();
        assert_eq!(clone.hunk_count(), 1);
        assert_eq!(
            Arc::as_ptr(&snapshot.hunks),
            Arc::as_ptr(&clone.hunks),
            "clone should share the same Arc"
        );
    }

    // -- Word diff tests (child 1.3) -----------------------------------------

    #[test]
    fn test_word_diff_small_modified_hunk() {
        let base = "hello world\n";
        let current = "hello rust\n";
        let mut hunks = compute_diff_hunks(current, Some(base));
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].status, DiffHunkStatus::Modified);

        compute_word_diffs(&mut hunks, current, base);

        assert!(
            !hunks[0].buffer_word_diffs.is_empty(),
            "word diffs should be populated for small modified hunk"
        );
        assert!(
            !hunks[0].base_word_diffs.is_empty(),
            "base word diffs should be populated"
        );
    }

    #[test]
    fn test_word_diff_skipped_for_large_hunk() {
        let base = "line1\nline2\nline3\nline4\nline5\nline6\n";
        let current = "LINE1\nLINE2\nLINE3\nLINE4\nLINE5\nLINE6\n";
        let mut hunks = compute_diff_hunks(current, Some(base));
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].status, DiffHunkStatus::Modified);

        compute_word_diffs(&mut hunks, current, base);

        assert!(
            hunks[0].buffer_word_diffs.is_empty(),
            "word diffs should be empty for >5-line modified hunk"
        );
    }

    #[test]
    fn test_word_diff_skipped_for_added() {
        let base = "line1\n";
        let current = "line1\nline2\n";
        let mut hunks = compute_diff_hunks(current, Some(base));
        let added = hunks.iter().find(|h| h.status == DiffHunkStatus::Added);
        assert!(added.is_some());

        compute_word_diffs(&mut hunks, current, base);

        for hunk in &hunks {
            if hunk.status == DiffHunkStatus::Added {
                assert!(hunk.buffer_word_diffs.is_empty());
            }
        }
    }

    #[test]
    fn test_word_diff_skipped_for_deleted() {
        let base = "line1\nline2\n";
        let current = "line1\n";
        let mut hunks = compute_diff_hunks(current, Some(base));
        let deleted = hunks.iter().find(|h| h.status == DiffHunkStatus::Deleted);
        assert!(deleted.is_some());

        compute_word_diffs(&mut hunks, current, base);

        for hunk in &hunks {
            if hunk.status == DiffHunkStatus::Deleted {
                assert!(hunk.buffer_word_diffs.is_empty());
            }
        }
    }

    #[test]
    fn test_expand_to_word_boundaries() {
        let text = "hello world foo";
        // Change byte 7 ('o' in 'world') should expand to cover "world" (6..11)
        let ranges = expand_to_word_boundaries(text, &[7..8]);
        assert_eq!(ranges, vec![6..11]);
    }

    #[test]
    fn test_apply_patch() {
        let base = "hello world foo bar";
        let ops = vec![(6..11, "rust")];
        let result = apply_patch(base, &ops);
        assert_eq!(result, "hello rust foo bar");
    }

    #[test]
    fn test_stage_hunks_produces_correct_index() {
        // Base: "line1\noriginal\nline3\n"
        // Current: "line1\nmodified\nline3\n"
        // Staging the hunk should produce current content as new index
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        assert_eq!(hunks.len(), 1);

        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        // Simulate what BufferDiff.stage_hunks would do
        let internal = snapshot.internal_hunks();
        let hunk = &internal[0];
        let buf_slice = text_for_line_range(current, &hunk.buffer_range);
        let ops = vec![(hunk.diff_base_byte_range.clone(), buf_slice)];
        let new_index = apply_patch(base, &ops);

        assert_eq!(new_index, current);
    }
}
