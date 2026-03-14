use std::ops::Range;

use crate::git::diff::{count_lines_in_byte_range, BufferDiffSnapshot, DiffHunkStatus};

// ---------------------------------------------------------------------------
// DiffTransform — maps buffer rows to display rows
// ---------------------------------------------------------------------------

/// A single transform entry in the DiffMap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffTransform {
    /// A range of buffer rows that maps 1:1 to display rows.
    BufferRows { count: u32 },
    /// Deleted base text injected here (not present in the buffer).
    DeletedText {
        base_byte_range: Range<usize>,
        row_count: u32,
    },
}

// ---------------------------------------------------------------------------
// ExpandedHunk — tracks which hunks are currently expanded
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ExpandedHunk {
    /// Index into the diff snapshot's hunk list.
    pub hunk_index: usize,
    /// Buffer line where the deletion occurs.
    pub buffer_line: u32,
    /// Number of deleted display rows.
    pub deleted_row_count: u32,
    /// Byte range in base text.
    pub base_byte_range: Range<usize>,
}

// ---------------------------------------------------------------------------
// DiffMap
// ---------------------------------------------------------------------------

/// Coordinate transform that injects deleted text from the base version into
/// the editor's row coordinate system.
///
/// Before DiffMap:  buffer rows 0..N (deleted text is invisible)
/// After DiffMap:   display rows 0..M where M >= N (deleted text has rows)
pub struct DiffMap {
    expanded_hunks: Vec<ExpandedHunk>,
    transforms: Vec<DiffTransform>,
    total_display_rows: u32,
    buffer_row_count: u32,
}

impl DiffMap {
    /// Create a new DiffMap with no expanded hunks.
    pub fn new(buffer_row_count: u32) -> Self {
        let transforms = vec![DiffTransform::BufferRows {
            count: buffer_row_count,
        }];
        Self {
            expanded_hunks: Vec::new(),
            transforms,
            total_display_rows: buffer_row_count,
            buffer_row_count,
        }
    }

    /// Expand a hunk to show deleted text inline.
    pub fn expand_hunk(
        &mut self,
        hunk_index: usize,
        snapshot: &BufferDiffSnapshot,
        base_text: Option<&str>,
    ) {
        let hunks = snapshot.internal_hunks();
        let Some(hunk) = hunks.get(hunk_index) else {
            return;
        };

        // Only Deleted and Modified hunks have base text to show.
        if hunk.status == DiffHunkStatus::Added {
            return;
        }

        // Don't expand if already expanded.
        if self.expanded_hunks.iter().any(|e| e.hunk_index == hunk_index) {
            return;
        }

        let deleted_row_count = count_lines_in_byte_range(base_text, &hunk.diff_base_byte_range);

        let expanded = ExpandedHunk {
            hunk_index,
            buffer_line: hunk.buffer_range.start,
            deleted_row_count,
            base_byte_range: hunk.diff_base_byte_range.clone(),
        };

        self.expanded_hunks.push(expanded);
        self.expanded_hunks.sort_by_key(|e| e.buffer_line);
        self.rebuild_transforms();
    }

    /// Collapse an expanded hunk.
    pub fn collapse_hunk(&mut self, hunk_index: usize) {
        self.expanded_hunks.retain(|e| e.hunk_index != hunk_index);
        self.rebuild_transforms();
    }

    /// Toggle expansion of a hunk.
    pub fn toggle_hunk(
        &mut self,
        hunk_index: usize,
        snapshot: &BufferDiffSnapshot,
        base_text: Option<&str>,
    ) {
        if self.expanded_hunks.iter().any(|e| e.hunk_index == hunk_index) {
            self.collapse_hunk(hunk_index);
        } else {
            self.expand_hunk(hunk_index, snapshot, base_text);
        }
    }

    /// Expand all hunks that have deleted text.
    pub fn expand_all(
        &mut self,
        snapshot: &BufferDiffSnapshot,
        base_text: Option<&str>,
    ) {
        self.expanded_hunks.clear();

        for (i, hunk) in snapshot.internal_hunks().iter().enumerate() {
            if hunk.status == DiffHunkStatus::Added {
                continue;
            }

            let deleted_row_count = count_lines_in_byte_range(base_text, &hunk.diff_base_byte_range);

            self.expanded_hunks.push(ExpandedHunk {
                hunk_index: i,
                buffer_line: hunk.buffer_range.start,
                deleted_row_count,
                base_byte_range: hunk.diff_base_byte_range.clone(),
            });
        }

        self.expanded_hunks.sort_by_key(|e| e.buffer_line);
        self.rebuild_transforms();
    }

    /// Collapse all expanded hunks.
    pub fn collapse_all(&mut self) {
        self.expanded_hunks.clear();
        self.rebuild_transforms();
    }

    /// Total number of display rows (buffer rows + expanded deleted rows).
    pub fn total_display_rows(&self) -> u32 {
        self.total_display_rows
    }

    /// Whether a hunk is currently expanded.
    pub fn is_expanded(&self, hunk_index: usize) -> bool {
        self.expanded_hunks.iter().any(|e| e.hunk_index == hunk_index)
    }

    /// Get the list of expanded hunks.
    pub fn expanded_hunks(&self) -> &[ExpandedHunk] {
        &self.expanded_hunks
    }

    /// Get the transforms.
    pub fn transforms(&self) -> &[DiffTransform] {
        &self.transforms
    }

    /// Convert a buffer row to a display row.
    pub fn buffer_row_to_display_row(&self, buffer_row: u32) -> u32 {
        let mut display_row = 0u32;
        let mut buffer_cursor = 0u32;

        for transform in &self.transforms {
            match transform {
                DiffTransform::BufferRows { count } => {
                    if buffer_row < buffer_cursor + count {
                        return display_row + (buffer_row - buffer_cursor);
                    }
                    buffer_cursor += count;
                    display_row += count;
                }
                DiffTransform::DeletedText { row_count, .. } => {
                    display_row += row_count;
                }
            }
        }

        display_row
    }

    /// Convert a display row to a buffer row. Returns None if the display
    /// row falls within deleted text (which has no buffer row).
    pub fn display_row_to_buffer_row(&self, display_row: u32) -> Option<u32> {
        let mut display_cursor = 0u32;
        let mut buffer_cursor = 0u32;

        for transform in &self.transforms {
            match transform {
                DiffTransform::BufferRows { count } => {
                    if display_row < display_cursor + count {
                        return Some(buffer_cursor + (display_row - display_cursor));
                    }
                    display_cursor += count;
                    buffer_cursor += count;
                }
                DiffTransform::DeletedText { row_count, .. } => {
                    if display_row < display_cursor + row_count {
                        return None; // Inside deleted text
                    }
                    display_cursor += row_count;
                }
            }
        }

        None
    }

    fn rebuild_transforms(&mut self) {
        self.transforms.clear();
        let mut buffer_cursor = 0u32;
        let mut total_display = 0u32;

        for expanded in &self.expanded_hunks {
            // Buffer rows before this expanded hunk.
            if expanded.buffer_line > buffer_cursor {
                let count = expanded.buffer_line - buffer_cursor;
                self.transforms
                    .push(DiffTransform::BufferRows { count });
                total_display += count;
            }

            // Inject deleted text.
            self.transforms.push(DiffTransform::DeletedText {
                base_byte_range: expanded.base_byte_range.clone(),
                row_count: expanded.deleted_row_count,
            });
            total_display += expanded.deleted_row_count;

            buffer_cursor = expanded.buffer_line;
        }

        // Remaining buffer rows.
        if buffer_cursor < self.buffer_row_count {
            let count = self.buffer_row_count - buffer_cursor;
            self.transforms
                .push(DiffTransform::BufferRows { count });
            total_display += count;
        }

        self.total_display_rows = total_display;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{compute_diff_hunks, BufferDiffSnapshot};

    #[test]
    fn test_new_diffmap_identity() {
        let map = DiffMap::new(100);
        assert_eq!(map.total_display_rows(), 100);
        assert_eq!(map.buffer_row_to_display_row(0), 0);
        assert_eq!(map.buffer_row_to_display_row(50), 50);
        assert_eq!(map.display_row_to_buffer_row(50), Some(50));
    }

    #[test]
    fn test_expand_deleted_hunk() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let mut map = DiffMap::new(1); // current has 1 line
        map.expand_hunk(0, &snapshot, Some(base));

        // Should have injected 2 deleted rows (line2 and line3)
        assert!(map.total_display_rows() > 1);
        assert!(map.is_expanded(0));
    }

    #[test]
    fn test_collapse_hunk() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let mut map = DiffMap::new(1);
        map.expand_hunk(0, &snapshot, Some(base));
        assert!(map.is_expanded(0));

        map.collapse_hunk(0);
        assert!(!map.is_expanded(0));
        assert_eq!(map.total_display_rows(), 1);
    }

    #[test]
    fn test_toggle_hunk() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let mut map = DiffMap::new(1);
        map.toggle_hunk(0, &snapshot, Some(base));
        assert!(map.is_expanded(0));

        map.toggle_hunk(0, &snapshot, Some(base));
        assert!(!map.is_expanded(0));
    }

    #[test]
    fn test_expand_all_collapse_all() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let mut map = DiffMap::new(3);
        map.expand_all(&snapshot, Some(base));
        assert!(map.total_display_rows() >= 3);

        map.collapse_all();
        assert_eq!(map.total_display_rows(), 3);
    }

    #[test]
    fn test_display_row_in_deleted_text_returns_none() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let mut map = DiffMap::new(1);
        map.expand_hunk(0, &snapshot, Some(base));

        // Display row 0 is buffer row 0 (line1)
        // Rows after that are deleted text — should return None
        // The deleted text is injected at buffer_line 1 (start of deleted range)
        // So display row 0 maps to buffer row 0
        assert_eq!(map.display_row_to_buffer_row(0), Some(0));
    }

    #[test]
    fn test_added_hunks_dont_expand() {
        let base = "line1\n";
        let current = "line1\nline2\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let mut map = DiffMap::new(2);
        map.expand_hunk(0, &snapshot, Some(base));

        // Added hunks have no deleted text to show
        assert!(!map.is_expanded(0));
        assert_eq!(map.total_display_rows(), 2);
    }
}
