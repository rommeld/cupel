use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

use crate::source_observations::format_revision;
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy};

const BUFFER_BYTES: usize = 64 * 1024;
const BINARY_PREFIX_BYTES: usize = 8 * 1024;

pub(super) struct TextRead {
    pub content: String,
    pub revision: String,
    pub returned_lines: usize,
    pub total_lines: usize,
    pub truncated_by: Option<TruncatedBy>,
    pub oversized_first_line: Option<usize>,
}

pub(super) fn live_file(path: &Path) -> io::Result<std::fs::File> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::other("not a regular text file"));
    }
    std::fs::File::open(path)
}

struct Window {
    start: usize,
    end: usize,
    total: usize,
    returned: usize,
    bytes: Vec<u8>,
    committed: usize,
    line_bytes: usize,
    terminated: bool,
    truncated_by: Option<TruncatedBy>,
    oversized_first_line: Option<usize>,
}

impl Window {
    fn in_range(&self) -> bool {
        self.total >= self.start && self.total < self.end
    }

    fn append(&mut self, fragment: &[u8]) {
        if self.in_range() && self.truncated_by.is_none() {
            if self.returned == DEFAULT_MAX_LINES {
                self.truncated_by = Some(TruncatedBy::Lines);
            } else {
                let separator = self.line_bytes == 0 && self.returned != 0;
                let size = self.bytes.len() + usize::from(separator) + fragment.len();
                if size > DEFAULT_MAX_BYTES {
                    self.bytes.truncate(self.committed);
                    self.truncated_by = Some(TruncatedBy::Bytes);
                } else {
                    if separator {
                        self.bytes.push(b'\n');
                    }
                    self.bytes.extend_from_slice(fragment);
                }
            }
        }
        self.line_bytes = self.line_bytes.saturating_add(fragment.len());
    }

    fn finish_line(&mut self, terminated: bool) {
        if self.in_range() {
            if self.truncated_by.is_none() {
                self.returned += 1;
                self.committed = self.bytes.len();
                self.terminated = terminated;
            } else if self.total == self.start && self.returned == 0 {
                self.oversized_first_line = Some(self.line_bytes);
            }
        }
        self.total += 1;
        self.line_bytes = 0;
    }
}

pub(super) fn scan(
    mut input: impl Read,
    start: usize,
    limit: Option<usize>,
    sniff_binary: bool,
    cancel: &CancellationToken,
) -> io::Result<TextRead> {
    let mut window = Window {
        start,
        end: start.saturating_add(limit.unwrap_or(usize::MAX)),
        total: 0,
        returned: 0,
        bytes: Vec::new(),
        committed: 0,
        line_bytes: 0,
        terminated: false,
        truncated_by: None,
        oversized_first_line: None,
    };
    let mut digest = Sha256::new();
    let mut buffer = vec![0; BUFFER_BYTES + 3];
    let mut carried = 0;
    let mut sniff_remaining = if sniff_binary { BINARY_PREFIX_BYTES } else { 0 };
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::other("Operation aborted"));
        }
        let size = input.read(&mut buffer[carried..])?;
        if size == 0 {
            break;
        }
        let end = carried + size;
        let bytes = &buffer[carried..end];
        let sniff = sniff_remaining.min(size);
        if bytes[..sniff].contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "binary file contains NUL bytes",
            ));
        }
        sniff_remaining -= sniff;
        digest.update(bytes);
        let valid = match std::str::from_utf8(&buffer[..end]) {
            Ok(_) => end,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
        };
        for fragment in bytes.split_inclusive(|&byte| byte == b'\n') {
            if let Some(line) = fragment.strip_suffix(b"\n") {
                window.append(line);
                window.finish_line(true);
            } else {
                window.append(fragment);
            }
        }
        buffer.copy_within(valid..end, 0);
        carried = end - valid;
    }
    if carried != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete UTF-8 at end of file",
        ));
    }
    if window.line_bytes != 0 || window.total == 0 {
        window.finish_line(false);
    }
    if window.truncated_by.is_none() && window.terminated {
        if window.bytes.len() == DEFAULT_MAX_BYTES {
            window.truncated_by = Some(TruncatedBy::Bytes);
        } else {
            window.bytes.push(b'\n');
        }
    }
    let content = String::from_utf8(window.bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(TextRead {
        content,
        revision: format_revision(&digest.finalize()),
        total_lines: window.total,
        returned_lines: window.returned,
        truncated_by: window.truncated_by,
        oversized_first_line: window.oversized_first_line,
    })
}
