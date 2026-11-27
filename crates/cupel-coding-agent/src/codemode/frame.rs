use std::io::{self, Read, Write};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::codemode::Failure;

pub(super) const SOURCE: u8 = 1;
pub(super) const READ: u8 = 2;
pub(super) const GREP: u8 = 3;
pub(super) const REPLY: u8 = 4;
pub(super) const EMIT: u8 = 5;
pub(super) const DONE: u8 = 6;
pub(super) const FAILURE: u8 = 7;
pub(super) const READY: u8 = 8;
pub(super) const FILE_REQUEST: u8 = 9;
pub(super) const FILE_REPLY: u8 = 10;

const GUEST_ERRORS: &[Failure] = &[
    Failure::Source,
    Failure::Compile,
    Failure::Script,
    Failure::Capability,
    Failure::Arguments,
    Failure::EmitValue,
    Failure::Memory,
    Failure::GuestTime,
    Failure::CallbackCount,
    Failure::CallbackBytes,
    Failure::EmitBytes,
];

pub(super) fn error_code(error: Failure) -> u8 {
    let index = GUEST_ERRORS.iter().position(|known| *known == error);
    index
        .and_then(|index| u8::try_from(index).ok())
        .unwrap_or(u8::MAX)
}

pub(super) fn decode_error(payload: &[u8]) -> Failure {
    match payload {
        [code] => GUEST_ERRORS
            .get(usize::from(*code))
            .copied()
            .unwrap_or(Failure::Worker),
        _ => Failure::Worker,
    }
}

pub(super) fn read(
    reader: &mut impl Read,
    limit: impl FnOnce(u8) -> Result<usize, Failure>,
) -> Result<(u8, Vec<u8>), Failure> {
    let mut header = [0; 5];
    reader
        .read_exact(&mut header)
        .map_err(|_error| Failure::Worker)?;
    let (tag, len) = decode_header(header, limit)?;
    let mut bytes = vec![0; len];
    reader
        .read_exact(&mut bytes)
        .map_err(|_error| Failure::Worker)?;
    Ok((tag, bytes))
}

pub(super) async fn read_async(
    reader: &mut (impl AsyncRead + Unpin),
    limit: impl FnOnce(u8) -> Result<usize, Failure>,
) -> Result<(u8, Vec<u8>), Failure> {
    let mut header = [0; 5];
    reader
        .read_exact(&mut header)
        .await
        .map_err(|_error| Failure::Worker)?;
    let (tag, len) = decode_header(header, limit)?;
    let mut bytes = vec![0; len];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_error| Failure::Worker)?;
    Ok((tag, bytes))
}

pub(super) fn write(writer: &mut impl Write, tag: u8, bytes: &[u8]) -> Result<(), Failure> {
    writer
        .write_all(&header(tag, bytes.len())?)
        .map_err(|_error| Failure::Worker)?;
    writer.write_all(bytes).map_err(|_error| Failure::Worker)?;
    writer.flush().map_err(|_error| Failure::Worker)
}

pub(super) async fn write_async(
    writer: &mut (impl AsyncWrite + Unpin),
    tag: u8,
    bytes: &[u8],
) -> Result<(), Failure> {
    writer
        .write_all(&header(tag, bytes.len())?)
        .await
        .map_err(|_error| Failure::Worker)?;
    writer
        .write_all(bytes)
        .await
        .map_err(|_error| Failure::Worker)?;
    writer.flush().await.map_err(|_error| Failure::Worker)
}

#[expect(
    clippy::big_endian_bytes,
    reason = "frame lengths use network byte order"
)]
fn header(tag: u8, len: usize) -> Result<[u8; 5], Failure> {
    let [a, b, c, d] = u32::try_from(len)
        .map_err(|_error| Failure::Worker)?
        .to_be_bytes();
    Ok([tag, a, b, c, d])
}

#[expect(
    clippy::big_endian_bytes,
    reason = "frame lengths use network byte order"
)]
fn decode_header(
    [tag, a, b, c, d]: [u8; 5],
    limit: impl FnOnce(u8) -> Result<usize, Failure>,
) -> Result<(u8, usize), Failure> {
    let len = u32::from_be_bytes([a, b, c, d]) as usize;
    if len > limit(tag)? {
        return Err(match tag {
            SOURCE => Failure::Source,
            READ | GREP => Failure::Arguments,
            REPLY => Failure::CallbackBytes,
            EMIT => Failure::EmitBytes,
            _ => Failure::Worker,
        });
    }
    Ok((tag, len))
}

pub(super) struct Buffer {
    bytes: Vec<u8>,
    remaining: usize,
}

impl Buffer {
    pub(super) fn new(remaining: usize) -> Self {
        Self {
            bytes: Vec::new(),
            remaining,
        }
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.remaining = self.remaining.checked_sub(buf.len()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::FileTooLarge, "encoded frame exceeds limit")
        })?;
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
