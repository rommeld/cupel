//! The input editor, a small text buffer with a cursor and prompt history.
//!
//! Implementation note, the cursor is a character index into the buffer, not a
//! byte index. Rust strings are UTF 8, so byte indexing at arbitrary
//! positions panics on multi byte characters, converting at the edges keeps
//! all editing logic safely in char space.

use unicode_width::UnicodeWidthChar as _;

use crate::transcript;

#[derive(Default)]
pub struct InputState {
    buffer: String,
    cursor: usize,
    preferred_column: Option<usize>,
    history: Vec<String>,
    history_index: Option<usize>,
    stash: String,
}

/// The editor's shared wrapping and character-to-screen map. Rendering, vertical
/// movement and mouse hit testing must use the same rows, including soft wraps.
pub(crate) struct InputLayout {
    lines: Vec<String>,
    positions: Vec<(usize, usize)>,
}

impl InputLayout {
    pub fn new(text: &str, width: usize) -> Self {
        let mut lines = Vec::new();
        let mut positions = Vec::new();
        for logical in text.split('\n') {
            let chunks = transcript::wrap_line(logical, width.max(1));
            let mut column = 0;
            for chunk in chunks {
                column = 0;
                for c in chunk.chars() {
                    positions.push((lines.len(), column));
                    column += c.width().unwrap_or(0);
                }
                lines.push(chunk);
            }
            // This position is either before an explicit newline or at EOF.
            // Soft-wrap boundaries instead belong to the following row.
            positions.push((lines.len().saturating_sub(1), column));
        }
        Self { lines, positions }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor_position(&self, cursor: usize) -> (usize, usize) {
        self.positions
            .get(cursor)
            .copied()
            .or_else(|| self.positions.last().copied())
            .unwrap_or_default()
    }

    /// Snap to the nearest character boundary, never inside a wide character.
    /// Only positions belonging to this row are candidates, so clicking past a
    /// soft-wrapped row cannot accidentally jump to the next row.
    pub fn cursor_at(&self, row: usize, column: usize) -> usize {
        let row = row.min(self.lines.len().saturating_sub(1));
        self.positions
            .iter()
            .enumerate()
            .filter(|(_, (line, _))| *line == row)
            .min_by_key(|(_, (_, col))| col.abs_diff(column))
            .map_or(0, |(cursor, _)| cursor)
    }
}

impl InputState {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.buffer
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Cursor position as a character index (see module doc for why chars).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the char range `[start, end)` with `replacement`, the cursor
    /// lands after the inserted text. Used by autocomplete acceptance to
    /// swap the typed `@quer` for the completed `@path `.
    pub fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        let start_byte = self.byte_index(start);
        let end_byte = self.byte_index(end.max(start));
        self.buffer.replace_range(start_byte..end_byte, replacement);
        self.cursor = start + replacement.chars().count();
        self.preferred_column = None;
        self.history_index = None;
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(char_index)
            .map_or(self.buffer.len(), |(i, _)| i)
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.buffer.insert(at, c);
        self.cursor += 1;
        self.preferred_column = None;
        self.history_index = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_index(self.cursor);
        self.buffer.insert_str(at, s);
        self.cursor += s.chars().count();
        self.preferred_column = None;
        self.history_index = None;
    }

    /// Backspace, remove the char before the cursor.
    pub fn delete_back(&mut self) {
        self.preferred_column = None;
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_index(self.cursor);
        self.buffer.remove(at);
    }

    /// Delete, remove the char at the cursor.
    pub fn delete_forward(&mut self) {
        self.preferred_column = None;
        if self.cursor >= self.buffer.chars().count() {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.buffer.remove(at);
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.preferred_column = None;
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buffer.chars().count());
        self.preferred_column = None;
    }

    /// Jump over whitespace and then the preceding whitespace-delimited word.
    pub fn move_word_left(&mut self) {
        let before = &self.buffer[..self.byte_index(self.cursor)];
        let mut chars = before.chars().rev().peekable();
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
            self.cursor -= 1;
        }
        while chars.peek().is_some_and(|c| !c.is_whitespace()) {
            chars.next();
            self.cursor -= 1;
        }
        self.preferred_column = None;
    }

    /// Jump over the current word and following whitespace to the next word.
    pub fn move_word_right(&mut self) {
        let after = &self.buffer[self.byte_index(self.cursor)..];
        let mut chars = after.chars().peekable();
        while chars.peek().is_some_and(|c| !c.is_whitespace()) {
            chars.next();
            self.cursor += 1;
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
            self.cursor += 1;
        }
        self.preferred_column = None;
    }

    /// Move by visual rows, retaining the desired display column across short
    /// rows. Other movement and edits reset that column.
    pub fn move_vertical(&mut self, rows: isize, width: usize) {
        let layout = InputLayout::new(&self.buffer, width);
        let (row, column) = layout.cursor_position(self.cursor);
        let column = *self.preferred_column.get_or_insert(column);
        let target = row
            .saturating_add_signed(rows)
            .min(layout.lines.len().saturating_sub(1));
        if target != row {
            self.cursor = layout.cursor_at(target, column);
        }
    }

    /// Place the cursor at a visual row and display column within the input.
    pub fn move_to_position(&mut self, row: usize, column: usize, width: usize) {
        self.cursor = InputLayout::new(&self.buffer, width).cursor_at(row, column);
        self.preferred_column = None;
    }

    /// Home, start of the current line (not the whole buffer).
    pub fn move_home(&mut self) {
        self.preferred_column = None;
        while self.cursor > 0 && self.buffer.chars().nth(self.cursor - 1) != Some('\n') {
            self.cursor -= 1;
        }
    }

    /// End, end of the current line.
    pub fn move_end(&mut self) {
        self.preferred_column = None;
        let total = self.buffer.chars().count();
        while self.cursor < total && self.buffer.chars().nth(self.cursor) != Some('\n') {
            self.cursor += 1;
        }
    }

    /// Take the buffer for submission, recording it in history.
    pub fn submit(&mut self) -> String {
        let text = core::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.preferred_column = None;
        self.history_index = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    /// Shift+Up, step back through history. The in progress input is stashed
    /// on the first step so it isn't lost.
    pub fn history_prev(&mut self) {
        self.preferred_column = None;
        if self.history.is_empty() {
            return;
        }
        let next_index = match self.history_index {
            None => {
                self.stash = self.buffer.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_index = Some(next_index);
        self.buffer = self.history[next_index].clone();
        self.cursor = self.buffer.chars().count();
    }

    /// Shift+Down, step forward, past the newest entry restores the stash.
    pub fn history_next(&mut self) {
        self.preferred_column = None;
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.buffer = self.history[index + 1].clone();
        } else {
            self.history_index = None;
            self.buffer = core::mem::take(&mut self.stash);
        }
        self.cursor = self.buffer.chars().count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_word_wrap_and_soft_boundaries() {
        let layout = InputLayout::new("hello world", 10);
        assert_eq!(layout.lines, ["hello ", "world"]);
        assert_eq!(layout.cursor_position(11), (1, 5));
        assert_eq!(layout.cursor_position(6), (1, 0));
        assert_eq!(layout.cursor_position(3), (0, 3));
        assert_eq!(layout.cursor_at(0, 99), 5, "stay on the clicked row");
        assert_eq!(layout.cursor_at(1, 99), 11);
    }

    #[test]
    fn layout_handles_hard_splits_newlines_and_wide_chars() {
        assert_eq!(
            InputLayout::new("abcdefghij", 4).cursor_position(10),
            (2, 2)
        );
        assert_eq!(InputLayout::new("ab\nxyz", 4).cursor_position(6), (1, 3));
        assert_eq!(InputLayout::new("ab\n", 4).cursor_position(3), (1, 0));
        let layout = InputLayout::new("日本語", 4);
        assert_eq!(layout.cursor_position(3), (1, 2));
        assert_eq!(layout.cursor_position(2), (1, 0));
        assert_eq!(layout.cursor_at(0, 1), 0, "snap inside a wide char");
        assert_eq!(layout.cursor_at(0, 2), 1);
        assert_eq!(InputLayout::new("", 0).cursor_position(0), (0, 0));
        assert_eq!(InputLayout::new("", 0).cursor_at(99, 99), 0);
    }

    #[test]
    fn layout_round_trips_visible_character_boundaries() {
        for text in ["", "\n\n", "hello world\n\nnext\n", "hé🙂 日本語\nlast"] {
            for width in [1, 4, 10, 80] {
                let layout = InputLayout::new(text, width);
                for cursor in 0..=text.chars().count() {
                    let (row, column) = layout.cursor_position(cursor);
                    assert_eq!(
                        layout.cursor_at(row, column),
                        cursor,
                        "text={text:?}, width={width}, row={row}, column={column}"
                    );
                }
            }
        }
    }

    #[test]
    fn word_jumps_are_char_safe_and_cross_whitespace() {
        let mut input = InputState::default();
        input.insert_str("  héllo\t日本語\n🙂  ");
        for expected in [12, 8, 2, 0, 0] {
            input.move_word_left();
            assert_eq!(input.cursor(), expected);
        }
        for expected in [2, 8, 12, 15, 15] {
            input.move_word_right();
            assert_eq!(input.cursor(), expected);
        }
        assert_eq!(input.text(), "  héllo\t日本語\n🙂  ");
    }

    #[test]
    fn vertical_motion_preserves_display_column_across_short_and_empty_rows() {
        let mut input = InputState::default();
        input.insert_str("abcdef\nx\n\n日本語🙂");
        input.move_to_position(0, 5, 20);
        assert_eq!(input.cursor(), 5);
        for (movement, expected) in [(1, 8), (1, 9), (1, 12), (-1, 9), (-1, 8), (-1, 5)] {
            input.move_vertical(movement, 20);
            assert_eq!(input.cursor(), expected);
        }
        input.move_vertical(-1, 20);
        assert_eq!(input.cursor(), 5, "top boundary is a no-op");
    }

    #[test]
    fn vertical_motion_uses_wrapped_rows_and_clamps_at_bottom() {
        let mut input = InputState::default();
        input.insert_str("hello world");
        input.move_to_position(0, 3, 10);
        input.move_vertical(1, 10);
        assert_eq!(input.cursor(), 9);
        input.move_vertical(1, 10);
        assert_eq!(input.cursor(), 9);
        input.move_vertical(-1, 10);
        assert_eq!(input.cursor(), 3);
    }

    #[test]
    fn horizontal_motion_edits_and_clicks_reset_preferred_column() {
        let mut input = InputState::default();
        input.insert_str("abcdef\nx\nabcdef");
        input.move_to_position(0, 5, 20);
        input.move_vertical(1, 20);
        input.move_left();
        input.move_vertical(1, 20);
        assert_eq!(input.cursor(), 9, "left reset desired column to zero");

        input.move_to_position(0, 5, 20);
        input.move_vertical(1, 20);
        input.insert('y');
        input.move_vertical(1, 20);
        assert_eq!(input.cursor(), 12, "edit reset desired column to two");

        input.move_to_position(0, 5, 20);
        input.move_vertical(1, 20);
        input.move_to_position(1, 1, 20);
        input.move_vertical(1, 20);
        assert_eq!(input.cursor(), 11, "click reset desired column to one");
    }

    #[test]
    fn insert_and_delete_are_char_safe() {
        let mut input = InputState::default();
        for c in "héllo".chars() {
            input.insert(c);
        }
        input.move_left();
        input.move_left();
        input.delete_back(); // removes the second 'l'
        assert_eq!(input.text(), "hélo");
        input.delete_forward(); // removes the remaining 'l'
        assert_eq!(input.text(), "héo");
    }

    #[test]
    fn history_round_trip_preserves_stash() {
        let mut input = InputState::default();
        input.insert_str("one");
        assert_eq!(input.submit(), "one");
        input.insert_str("two");
        assert_eq!(input.submit(), "two");

        input.insert_str("draft");
        input.history_prev();
        assert_eq!(input.text(), "two");
        input.history_prev();
        assert_eq!(input.text(), "one");
        input.history_next();
        assert_eq!(input.text(), "two");
        input.history_next();
        assert_eq!(input.text(), "draft"); // stash restored
    }

    #[test]
    fn replace_range_is_char_safe_and_places_cursor() {
        let mut input = InputState::default();
        input.insert_str("sée @qué tail");
        // Replace the "@qué" token (chars 4..8) with a completion.
        input.replace_range(4, 8, "@querétaro.rs ");
        assert_eq!(input.text(), "sée @querétaro.rs  tail");
    }

    #[test]
    fn duplicate_history_entries_are_skipped() {
        let mut input = InputState::default();
        input.insert_str("same");
        input.submit();
        input.insert_str("same");
        input.submit();
        input.history_prev();
        input.history_prev(); // would go past if "same" were stored twice
        assert_eq!(input.text(), "same");
    }
}
