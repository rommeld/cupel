//! Program Status Protocol revision 0.4 (OSC 7501).
//!
//! Frontends opt in only for terminal output. Nothing is emitted by agent or ACP
//! code. Reports are complete replacements; output failures are best-effort.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Write};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    Question,
    Auth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked(BlockedKind),
    Done,
    Error,
}

/// An owned session record, never the terminal's root record. Hashing keeps even
/// externally supplied transcript ids within the protocol's id grammar and limits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordId(String);

impl RecordId {
    #[must_use]
    pub fn session(session: &str, cwd: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(session.as_bytes());
        hash.update([0]);
        hash.update(cwd.as_bytes());
        let digest =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&hash.finalize()[..16]);
        Self(format!("cupel-{}/{digest}", std::process::id()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: RecordId,
    pub state: State,
    pub title: Option<String>,
    pub message: Option<String>,
}

fn text(value: &str, limit: usize) -> String {
    let mut filtered = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        if filtered.len() + character.len_utf8() > limit {
            break;
        }
        filtered.push(character);
    }
    base64::engine::general_purpose::STANDARD.encode(filtered)
}

impl Record {
    fn encode(&self) -> String {
        let state = match self.state {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked(BlockedKind::Question) => "blocked:kind=question",
            State::Blocked(BlockedKind::Auth) => "blocked:kind=auth",
            State::Done => "done",
            State::Error => "error",
        };
        let mut report = format!("\x1b]7501;state={state}:id={}:app=cupel", self.id.0);
        if let Some(title) = &self.title {
            report.push_str(":title=");
            report.push_str(&text(title, 192));
        }
        if let Some(message) = &self.message {
            report.push_str(":msg=");
            report.push_str(&text(message, 2048));
        }
        report.push_str("\x1b\\");
        report
    }
}

fn clear(id: &RecordId) -> String {
    format!("\x1b]7501;state=clear:id={}:app=cupel\x1b\\", id.0)
}

/// Deduplicates reports, removes records no longer owned by a frontend, and
/// flushes outside rendering. A failed terminal must not stop agent work.
pub struct Reporter {
    enabled: bool,
    previous: BTreeMap<RecordId, String>,
}

impl Reporter {
    #[must_use]
    pub fn for_stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }

    #[must_use]
    pub fn new(terminal_attached: bool) -> Self {
        Self {
            enabled: terminal_attached,
            previous: BTreeMap::new(),
        }
    }

    pub fn sync(&mut self, writer: &mut impl Write, records: impl IntoIterator<Item = Record>) {
        if !self.enabled {
            return;
        }
        let current: BTreeMap<_, _> = records
            .into_iter()
            .map(|record| (record.id.clone(), record.encode()))
            .collect();
        let mut output = String::new();
        for id in self.previous.keys().filter(|id| !current.contains_key(*id)) {
            output.push_str(&clear(id));
        }
        for (id, report) in &current {
            if self.previous.get(id) != Some(report) {
                output.push_str(report);
            }
        }
        if !output.is_empty()
            && writer
                .write_all(output.as_bytes())
                .and_then(|()| writer.flush())
                .is_err()
        {
            self.enabled = false;
        }
        self.previous = current;
    }

    /// Restart clears only our own records, never another program's root/children.
    pub fn clear(&mut self, writer: &mut impl Write) {
        self.sync(writer, []);
    }
}

#[cfg(test)]
mod tests {
    use crate::program_status::{BlockedKind, Record, RecordId, Reporter, State};
    use base64::Engine as _;

    fn record(state: State) -> Record {
        Record {
            id: RecordId("cupel/session-1".into()),
            state,
            title: Some("main".into()),
            message: Some("hi".into()),
        }
    }

    #[test]
    fn exact_bytes_and_replacement_metadata() {
        let mut reporter = Reporter::new(true);
        let mut output = Vec::new();
        reporter.sync(&mut output, [record(State::Blocked(BlockedKind::Question))]);
        assert_eq!(output, b"\x1b]7501;state=blocked:kind=question:id=cupel/session-1:app=cupel:title=bWFpbg==:msg=aGk=\x1b\\");
        output.clear();
        let mut done = record(State::Done);
        done.message = None;
        reporter.sync(&mut output, [done.clone()]);
        assert_eq!(
            output,
            b"\x1b]7501;state=done:id=cupel/session-1:app=cupel:title=bWFpbg==\x1b\\"
        );
        output.clear();
        reporter.sync(&mut output, [done]);
        assert!(output.is_empty(), "unchanged records do not write or flush");
        reporter.clear(&mut output);
        assert_eq!(
            output,
            b"\x1b]7501;state=clear:id=cupel/session-1:app=cupel\x1b\\"
        );
    }

    #[test]
    fn state_changes_drop_old_kind_title_and_message() {
        let mut reporter = Reporter::new(true);
        let mut output = Vec::new();
        reporter.sync(&mut output, [record(State::Blocked(BlockedKind::Auth))]);
        assert!(String::from_utf8_lossy(&output).contains("state=blocked:kind=auth:"));
        output.clear();
        let mut next = record(State::Working);
        next.title = None;
        next.message = None;
        reporter.sync(&mut output, [next]);
        assert_eq!(
            output,
            b"\x1b]7501;state=working:id=cupel/session-1:app=cupel\x1b\\"
        );
    }

    #[test]
    fn flushes_only_changes_and_ignores_flush_failure() {
        #[derive(Default)]
        struct Output {
            flushes: usize,
            fail: bool,
        }
        impl std::io::Write for Output {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                if self.fail {
                    Err(std::io::Error::other("terminal unavailable"))
                } else {
                    Ok(())
                }
            }
        }
        let mut output = Output::default();
        let mut reporter = Reporter::new(true);
        reporter.sync(&mut output, [record(State::Working)]);
        assert_eq!(output.flushes, 1);
        reporter.sync(&mut output, [record(State::Working)]);
        assert_eq!(output.flushes, 1);
        output.fail = true;
        reporter.sync(&mut output, [record(State::Done)]);
        assert_eq!(output.flushes, 2);
        reporter.clear(&mut output);
        assert_eq!(output.flushes, 2);
    }

    #[test]
    fn utf8_controls_and_limits() {
        let mut report = record(State::Working);
        report.title = Some("é\n\t\x1b\x7f\u{80}\u{9f}猫".into());
        report.message = Some("hello: 世界".into());
        let encoded = report.encode();
        assert!(encoded.contains(":title=w6nnjKs=:msg=aGVsbG86IOS4lueVjA=="));
        assert!(!encoded.contains("progress="));

        report.title = Some("猫".repeat(200));
        report.message = Some("é".repeat(3000));
        let encoded = report.encode();
        assert!(encoded.len() < 3300);
        for (key, decoded_limit, encoded_limit) in [("title", 192, 256), ("msg", 2048, 2732)] {
            let payload = encoded
                .split(&format!(":{key}="))
                .nth(1)
                .unwrap()
                .split([':', '\x1b'])
                .next()
                .unwrap();
            assert!(payload.len() <= encoded_limit);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap();
            assert!(bytes.len() <= decoded_limit);
            assert!(String::from_utf8(bytes).is_ok());
        }
    }

    #[test]
    fn ids_are_stable_valid_and_session_specific() {
        let first = RecordId::session("untrusted/id\n", "/project");
        assert_eq!(first, RecordId::session("untrusted/id\n", "/project"));
        assert_ne!(first, RecordId::session("other", "/project"));
        assert_ne!(first, RecordId::session("untrusted/id\n", "/worktree"));
        assert!(first.0.len() <= 128);
        assert!(first.0.split('/').count() <= 8);
        assert!(first.0.split('/').all(|part| {
            part.len() <= 32
                && !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
        }));
    }

    #[test]
    fn redirected_output_is_escape_free_and_write_failures_are_ignored() {
        let mut output = Vec::new();
        let mut disabled = Reporter::new(false);
        disabled.sync(&mut output, [record(State::Working)]);
        disabled.clear(&mut output);
        assert!(output.is_empty());

        let mut reporter = Reporter::new(true);
        reporter.sync(&mut std::io::sink(), [record(State::Working)]);
        reporter.sync(
            &mut std::io::Cursor::new(&mut [0_u8; 0][..]),
            [record(State::Error)],
        );
        reporter.sync(&mut output, [record(State::Done)]);
        assert!(
            output.is_empty(),
            "failed status output does not retry forever"
        );
    }
}
