//! The input editor: a small text buffer with a cursor and prompt history.
//!
//! pi ships a full editor component (kill ring, word navigation, undo,
//! bracketed paste). This is the deliberately-minimal core of that: insert,
//! delete, horizontal movement, Alt+Enter newlines, and Up/Down history.
//! Each of pi's extras can be layered on later without changing the shape.
//!
//! Implementation note: the cursor is a CHAR index into the buffer, not a
//! byte index. Rust strings are UTF-8, so byte-indexing at arbitrary
//! positions panics on multi-byte characters; converting at the edges keeps
//! all editing logic safely in char space.

#[derive(Default)]
pub struct InputState {
    buffer: String,
    /// Cursor position in CHARS from the start of the buffer.
    cursor: usize,
    /// Previously submitted prompts, oldest first.
    history: Vec<String>,
    /// Current position while browsing history (`None` = editing new input).
    history_index: Option<usize>,
    /// What was being typed before history browsing started, restored when
    /// the user navigates past the newest entry.
    stash: String,
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

    /// Cursor position as (line, column) in display terms, for placing the
    /// terminal cursor.
    #[must_use]
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before: String = self.buffer.chars().take(self.cursor).collect();
        let line = before.matches('\n').count();
        let col = before
            .rsplit('\n')
            .next()
            .map_or(0, |last| last.chars().count());
        (line, col)
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
        self.history_index = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        let at = self.byte_index(self.cursor);
        self.buffer.insert_str(at, s);
        self.cursor += s.chars().count();
        self.history_index = None;
    }

    /// Backspace: remove the char BEFORE the cursor.
    pub fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_index(self.cursor);
        self.buffer.remove(at);
    }

    /// Delete: remove the char AT the cursor.
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.buffer.chars().count() {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.buffer.remove(at);
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.buffer.chars().count());
    }

    /// Home: start of the current line (not the whole buffer).
    pub fn move_home(&mut self) {
        while self.cursor > 0 && self.buffer.chars().nth(self.cursor - 1) != Some('\n') {
            self.cursor -= 1;
        }
    }

    /// End: end of the current line.
    pub fn move_end(&mut self) {
        let total = self.buffer.chars().count();
        while self.cursor < total && self.buffer.chars().nth(self.cursor) != Some('\n') {
            self.cursor += 1;
        }
    }

    /// Take the buffer for submission, recording it in history.
    pub fn submit(&mut self) -> String {
        let text = core::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_index = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    /// Up arrow: step back through history. The in-progress input is stashed
    /// on the first step so it isn't lost.
    pub fn history_prev(&mut self) {
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

    /// Down arrow: step forward; past the newest entry restores the stash.
    pub fn history_next(&mut self) {
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
    fn home_end_work_per_line() {
        let mut input = InputState::default();
        input.insert_str("first\nsecond");
        input.move_home();
        let (line, col) = input.cursor_line_col();
        assert_eq!((line, col), (1, 0));
        input.move_end();
        let (line, col) = input.cursor_line_col();
        assert_eq!((line, col), (1, 6));
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
