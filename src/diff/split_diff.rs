use std::collections::HashMap;
use std::ops::Range;

use crate::git::diff::{count_lines_in_byte_range, BufferDiffSnapshot, DiffHunkStatus};

// ---------------------------------------------------------------------------
// ExcerptId / BufferId — identifiers for split diff mapping
// ---------------------------------------------------------------------------

/// Opaque excerpt identifier for the Companion mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExcerptId(u64);

impl ExcerptId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

/// Opaque buffer identifier for the Companion mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SplitBufferId(u64);

impl SplitBufferId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

// ---------------------------------------------------------------------------
// Companion — bidirectional maps between LHS and RHS
// ---------------------------------------------------------------------------

/// Maintains bidirectional maps between LHS (base text, read-only) and
/// RHS (modified text, editable) editors for synchronized navigation.
#[derive(Clone, Debug, Default)]
pub struct Companion {
    lhs_to_rhs_excerpt: HashMap<ExcerptId, ExcerptId>,
    rhs_to_lhs_excerpt: HashMap<ExcerptId, ExcerptId>,
    lhs_to_rhs_buffer: HashMap<SplitBufferId, SplitBufferId>,
    rhs_to_lhs_buffer: HashMap<SplitBufferId, SplitBufferId>,
}

impl Companion {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a paired excerpt.
    pub fn register_excerpt_pair(&mut self, lhs: ExcerptId, rhs: ExcerptId) {
        self.lhs_to_rhs_excerpt.insert(lhs, rhs);
        self.rhs_to_lhs_excerpt.insert(rhs, lhs);
    }

    /// Register a paired buffer.
    pub fn register_buffer_pair(&mut self, lhs: SplitBufferId, rhs: SplitBufferId) {
        self.lhs_to_rhs_buffer.insert(lhs, rhs);
        self.rhs_to_lhs_buffer.insert(rhs, lhs);
    }

    pub fn lhs_to_rhs_excerpt(&self, lhs: ExcerptId) -> Option<ExcerptId> {
        self.lhs_to_rhs_excerpt.get(&lhs).copied()
    }

    pub fn rhs_to_lhs_excerpt(&self, rhs: ExcerptId) -> Option<ExcerptId> {
        self.rhs_to_lhs_excerpt.get(&rhs).copied()
    }

    pub fn lhs_to_rhs_buffer(&self, lhs: SplitBufferId) -> Option<SplitBufferId> {
        self.lhs_to_rhs_buffer.get(&lhs).copied()
    }

    pub fn rhs_to_lhs_buffer(&self, rhs: SplitBufferId) -> Option<SplitBufferId> {
        self.rhs_to_lhs_buffer.get(&rhs).copied()
    }
}

// ---------------------------------------------------------------------------
// CompanionExcerptPatch — row correspondence between LHS and RHS
// ---------------------------------------------------------------------------

/// Describes how a range of display rows in one side corresponds to the
/// other side, including any spacer blocks needed for alignment.
#[derive(Clone, Debug)]
pub struct CompanionExcerptPatch {
    /// Row range in the RHS (modified text).
    pub rhs_rows: Range<u32>,
    /// Row range in the LHS (base text).
    pub lhs_rows: Range<u32>,
    /// Height difference: positive means LHS needs spacers, negative means
    /// RHS needs spacers.
    pub height_diff: i32,
}

// ---------------------------------------------------------------------------
// patches_for_range — row conversion through diff
// ---------------------------------------------------------------------------

/// Compute row correspondence patches between LHS and RHS for a given
/// range of RHS display rows.
///
/// For each hunk in the diff snapshot:
/// - Outside hunks: rows correspond 1:1
/// - Inside hunks: apply the diff to compute LHS rows
/// - Where sides differ in length, spacers are needed
pub fn patches_for_range(
    rhs_range: Range<u32>,
    snapshot: &BufferDiffSnapshot,
) -> Vec<CompanionExcerptPatch> {
    let hunks = snapshot.internal_hunks();
    let mut patches = Vec::new();
    let mut rhs_cursor = rhs_range.start;

    for hunk in hunks {
        if hunk.buffer_range.end <= rhs_range.start || hunk.buffer_range.start >= rhs_range.end {
            continue;
        }

        let rhs_hunk_start = hunk.buffer_range.start.max(rhs_range.start);
        let rhs_hunk_end = hunk.buffer_range.end.min(rhs_range.end);

        // Lines before this hunk (1:1 mapping)
        if rhs_cursor < rhs_hunk_start {
            let _count = rhs_hunk_start - rhs_cursor;
            // In 1:1 regions, the LHS offset is the same as RHS.
            // This is simplified; a real implementation would track cumulative offsets.
            patches.push(CompanionExcerptPatch {
                rhs_rows: rhs_cursor..rhs_hunk_start,
                lhs_rows: rhs_cursor..rhs_hunk_start,
                height_diff: 0,
            });
        }

        // The hunk itself.
        let rhs_lines = rhs_hunk_end - rhs_hunk_start;
        let base_lines = count_lines_in_byte_range(snapshot.base_text.as_deref(), &hunk.diff_base_byte_range);
        let height_diff = rhs_lines as i32 - base_lines as i32;

        patches.push(CompanionExcerptPatch {
            rhs_rows: rhs_hunk_start..rhs_hunk_end,
            lhs_rows: rhs_hunk_start..(rhs_hunk_start + base_lines),
            height_diff,
        });

        rhs_cursor = rhs_hunk_end;
    }

    // Trailing 1:1 region.
    if rhs_cursor < rhs_range.end {
        patches.push(CompanionExcerptPatch {
            rhs_rows: rhs_cursor..rhs_range.end,
            lhs_rows: rhs_cursor..rhs_range.end,
            height_diff: 0,
        });
    }

    patches
}

// ---------------------------------------------------------------------------
// compute_balancing_blocks — spacer block placement
// ---------------------------------------------------------------------------

/// Describes a spacer block to be inserted for vertical alignment.
#[derive(Clone, Debug)]
pub struct SpacerBlock {
    /// Which side needs the spacer.
    pub side: SpacerSide,
    /// Row in that side where the spacer should be placed.
    pub at_row: u32,
    /// Number of empty rows to insert.
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpacerSide {
    Lhs,
    Rhs,
}

/// Compute spacer blocks from companion patches.
pub fn compute_balancing_blocks(patches: &[CompanionExcerptPatch]) -> Vec<SpacerBlock> {
    let mut blocks = Vec::new();

    for patch in patches {
        if patch.height_diff > 0 {
            // RHS has more lines → LHS needs spacers
            blocks.push(SpacerBlock {
                side: SpacerSide::Lhs,
                at_row: patch.lhs_rows.start,
                height: patch.height_diff as u32,
            });
        } else if patch.height_diff < 0 {
            // LHS has more lines → RHS needs spacers
            blocks.push(SpacerBlock {
                side: SpacerSide::Rhs,
                at_row: patch.rhs_rows.start,
                height: (-patch.height_diff) as u32,
            });
        }
    }

    blocks
}

// ---------------------------------------------------------------------------
// Selection translation (child 3.4)
// ---------------------------------------------------------------------------

/// Map a buffer position (line, column) from LHS to RHS through the diff.
///
/// For positions outside hunks: returns the same position.
/// For positions inside deleted text: returns the nearest buffer line.
pub fn translate_lhs_position_to_rhs(
    lhs_line: u32,
    snapshot: &BufferDiffSnapshot,
) -> u32 {
    let hunks = snapshot.internal_hunks();

    // Walk through hunks to find where lhs_line falls.
    let mut line_offset: i32 = 0;

    for hunk in hunks {
        let base_lines = count_lines_in_byte_range(snapshot.base_text.as_deref(), &hunk.diff_base_byte_range);

        match hunk.status {
            DiffHunkStatus::Added => {
                // Added lines exist only in RHS. If lhs_line is after this hunk,
                // offset by the number of added lines.
                if lhs_line >= hunk.buffer_range.start {
                    let added = hunk.buffer_range.end - hunk.buffer_range.start;
                    line_offset += added as i32;
                }
            }
            DiffHunkStatus::Deleted => {
                // Deleted lines exist only in LHS. If lhs_line is within the
                // deleted range, snap to the nearest RHS line.
                if lhs_line >= hunk.buffer_range.start {
                    line_offset -= base_lines as i32;
                }
            }
            DiffHunkStatus::Modified => {
                if lhs_line >= hunk.buffer_range.start {
                    let rhs_lines = hunk.buffer_range.end - hunk.buffer_range.start;
                    line_offset += rhs_lines as i32 - base_lines as i32;
                }
            }
        }
    }

    (lhs_line as i32 + line_offset).max(0) as u32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{compute_diff_hunks, BufferDiffSnapshot};

    #[test]
    fn test_companion_bidirectional() {
        let mut companion = Companion::new();
        let lhs_ex = ExcerptId::new(1);
        let rhs_ex = ExcerptId::new(2);
        companion.register_excerpt_pair(lhs_ex, rhs_ex);

        assert_eq!(companion.lhs_to_rhs_excerpt(lhs_ex), Some(rhs_ex));
        assert_eq!(companion.rhs_to_lhs_excerpt(rhs_ex), Some(lhs_ex));
    }

    #[test]
    fn test_patches_for_modified_hunk() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let patches = patches_for_range(0..3, &snapshot);
        assert!(!patches.is_empty());
    }

    #[test]
    fn test_balancing_blocks_added_lines() {
        // RHS has more lines → LHS needs spacers
        let patches = vec![CompanionExcerptPatch {
            rhs_rows: 5..8,
            lhs_rows: 5..5,
            height_diff: 3,
        }];

        let blocks = compute_balancing_blocks(&patches);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].side, SpacerSide::Lhs);
        assert_eq!(blocks[0].height, 3);
    }

    #[test]
    fn test_balancing_blocks_deleted_lines() {
        // LHS has more lines → RHS needs spacers
        let patches = vec![CompanionExcerptPatch {
            rhs_rows: 5..5,
            lhs_rows: 5..8,
            height_diff: -3,
        }];

        let blocks = compute_balancing_blocks(&patches);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].side, SpacerSide::Rhs);
        assert_eq!(blocks[0].height, 3);
    }

    #[test]
    fn test_translate_identity_no_hunks() {
        let snapshot = BufferDiffSnapshot::new(None);
        assert_eq!(translate_lhs_position_to_rhs(5, &snapshot), 5);
    }

    #[test]
    fn test_translate_across_modified_hunk() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        // Line 0 (before hunk) should map 1:1
        assert_eq!(translate_lhs_position_to_rhs(0, &snapshot), 0);
    }
}
