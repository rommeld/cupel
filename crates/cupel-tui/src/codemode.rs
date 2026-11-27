mod coordinator;
mod frame;
mod worker;

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

pub use coordinator::Coordinator;
pub use worker::worker_main;

pub const WORKER_ARGUMENT: &str = "--internal-codemode-worker";

const SOURCE_BYTES: usize = 16 * 1024;
const GUEST_HEAP_BYTES: usize = 16 * 1024 * 1024;
const ALLOCATOR_BYTES: usize = 20 * 1024 * 1024;
const CALLBACKS: usize = 16;
const CALLBACK_BYTES: usize = 256 * 1024;
const ARGUMENT_BYTES: usize = SOURCE_BYTES;
const EMIT_BYTES: usize = 16 * 1024;
const GUEST_TIME: Duration = Duration::from_secs(1);
const CALL_TIME: Duration = Duration::from_secs(5);
const CLEANUP_TIME: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Callback {
    Read,
    Grep,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum Failure {
    #[error("codemode source exceeds 16 KiB")]
    Source,
    #[error("codemode script could not be compiled")]
    Compile,
    #[error("codemode script failed")]
    Script,
    #[error("codemode capability unavailable; use only read, grep, and emit")]
    Capability,
    #[error("invalid codemode callback arguments")]
    Arguments,
    #[error("codemode emit requires a JSON-compatible value")]
    EmitValue,
    #[error("codemode exceeded the 16 MiB guest memory limit or worker allocation ceiling")]
    Memory,
    #[error("codemode exceeded the one-second guest execution limit")]
    GuestTime,
    #[error("codemode exceeded the 16 host callback limit")]
    CallbackCount,
    #[error("codemode exceeded the 256 KiB callback result limit")]
    CallbackBytes,
    #[error("codemode exceeded the 16 KiB emit limit")]
    EmitBytes,
    #[error("codemode exceeded the five-second call deadline")]
    Deadline,
    #[error("codemode cancelled")]
    Cancelled,
    #[error("codemode callback failed")]
    Callback,
    #[error("codemode worker failed")]
    Worker,
    #[error("codemode worker cleanup could not be confirmed; preview disabled")]
    Cleanup,
}

/// The broker must validate the selected tool's arguments before dispatch.
#[derive(Debug)]
pub struct Request {
    pub callback: Callback,
    pub arguments: Value,
}
