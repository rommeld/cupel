use gpui::{div, IntoElement, Styled};

use crate::git::diff::{BufferDiffSnapshot, DiffHunkStatus, SecondaryHunkStatus};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Gutter indicator bar width
// ---------------------------------------------------------------------------

/// Width of the gutter diff indicator bar in pixels.
const GUTTER_BAR_WIDTH_PX: f32 = 3.0;

// ---------------------------------------------------------------------------
// GutterIndicator — a single indicator for one hunk
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GutterIndicator {
    /// Start line (0-based).
    pub start_line: u32,
    /// End line (exclusive, 0-based).
    pub end_line: u32,
    /// Hunk status for coloring.
    pub status: DiffHunkStatus,
    /// Secondary status for staged/unstaged overlay.
    pub secondary_status: SecondaryHunkStatus,
}

// ---------------------------------------------------------------------------
// Build gutter indicators from a diff snapshot
// ---------------------------------------------------------------------------

/// Extract gutter indicators from a `BufferDiffSnapshot` for a visible
/// line range. Returns one indicator per hunk.
pub fn indicators_for_range(
    snapshot: &BufferDiffSnapshot,
    visible_range: std::ops::Range<u32>,
) -> Vec<GutterIndicator> {
    snapshot
        .hunks_intersecting_range(visible_range)
        .into_iter()
        .map(|hunk| GutterIndicator {
            start_line: hunk.buffer_range.start,
            end_line: hunk.buffer_range.end,
            status: hunk.status,
            secondary_status: hunk.secondary_status,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Render gutter indicator elements
// ---------------------------------------------------------------------------

/// Render a gutter indicator bar as a GPUI element.
///
/// The bar is a narrow colored strip:
/// - Added → green
/// - Modified → yellow/orange
/// - Deleted → red (rendered as a small triangle marker)
///
/// Staged hunks use full opacity; unstaged hunks use reduced opacity.
pub fn render_indicator(
    indicator: &GutterIndicator,
    line_height_px: f32,
    theme: &Theme,
) -> impl IntoElement {
    let color = match indicator.status {
        DiffHunkStatus::Added => theme.added,
        DiffHunkStatus::Modified => theme.modified,
        DiffHunkStatus::Deleted => theme.deleted,
    };

    let opacity = match indicator.secondary_status {
        SecondaryHunkStatus::NoSecondaryHunk => 1.0,       // fully staged
        SecondaryHunkStatus::HasSecondaryHunk => 0.5,      // fully unstaged
        SecondaryHunkStatus::OverlapsWithSecondaryHunk => 0.75, // partially staged
    };

    let line_count = indicator.end_line.saturating_sub(indicator.start_line).max(1);
    let height = line_count as f32 * line_height_px;

    let color_with_opacity = gpui::hsla(color.h, color.s, color.l, color.a * opacity);

    if indicator.status == DiffHunkStatus::Deleted {
        // Deleted hunks show a small triangle marker (approximated as a square)
        div()
            .w(gpui::px(GUTTER_BAR_WIDTH_PX * 2.0))
            .h(gpui::px(GUTTER_BAR_WIDTH_PX * 2.0))
            .bg(color_with_opacity)
    } else {
        // Added/Modified hunks show a vertical bar
        div()
            .w(gpui::px(GUTTER_BAR_WIDTH_PX))
            .h(gpui::px(height))
            .bg(color_with_opacity)
    }
}

// ---------------------------------------------------------------------------
// Scrollbar diff indicators
// ---------------------------------------------------------------------------

/// Compute scrollbar indicator positions as (proportion_start, proportion_end, status).
pub fn scrollbar_indicators(
    snapshot: &BufferDiffSnapshot,
    total_lines: u32,
) -> Vec<(f32, f32, DiffHunkStatus)> {
    if total_lines == 0 {
        return Vec::new();
    }

    let total = total_lines as f32;

    snapshot
        .internal_hunks()
        .iter()
        .map(|hunk| {
            let top = hunk.buffer_range.start as f32 / total;
            let bottom = hunk.buffer_range.end.max(hunk.buffer_range.start + 1) as f32 / total;
            (top, bottom, hunk.status)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::{compute_diff_hunks, BufferDiffSnapshot};

    #[test]
    fn test_indicators_for_added_lines() {
        let base = "line1\nline2\n";
        let current = "line1\nline2\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let indicators = indicators_for_range(&snapshot, 0..10);
        assert_eq!(indicators.len(), 1);
        assert_eq!(indicators[0].status, DiffHunkStatus::Added);
    }

    #[test]
    fn test_indicators_for_modified_lines() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let indicators = indicators_for_range(&snapshot, 0..10);
        assert_eq!(indicators.len(), 1);
        assert_eq!(indicators[0].status, DiffHunkStatus::Modified);
    }

    #[test]
    fn test_indicators_for_deleted_lines() {
        let base = "line1\nline2\nline3\n";
        let current = "line1\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let indicators = indicators_for_range(&snapshot, 0..10);
        assert_eq!(indicators.len(), 1);
        assert_eq!(indicators[0].status, DiffHunkStatus::Deleted);
    }

    #[test]
    fn test_scrollbar_indicators() {
        let base = "line1\noriginal\nline3\n";
        let current = "line1\nmodified\nline3\n";
        let hunks = compute_diff_hunks(current, Some(base));
        let mut snapshot = BufferDiffSnapshot::new(Some(base.to_string()));
        snapshot.set_hunks(hunks);

        let scroll_indicators = scrollbar_indicators(&snapshot, 3);
        assert_eq!(scroll_indicators.len(), 1);
        let (top, bottom, status) = &scroll_indicators[0];
        assert!(*top >= 0.0 && *top <= 1.0);
        assert!(*bottom >= 0.0 && *bottom <= 1.0);
        assert_eq!(*status, DiffHunkStatus::Modified);
    }

    #[test]
    fn test_empty_scrollbar() {
        let snapshot = BufferDiffSnapshot::new(None);
        let indicators = scrollbar_indicators(&snapshot, 0);
        assert!(indicators.is_empty());
    }
}
