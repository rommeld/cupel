//! Shared subprocess output and process-group handling for bash and hooks.

use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;

/// Bash uses this as a quiet interval; hooks use it as a fixed drain budget.
pub(crate) const EXIT_STDIO_GRACE: Duration = Duration::from_millis(500);

pub(crate) enum OutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

impl OutputChunk {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Stdout(bytes) | Self::Stderr(bytes) => bytes,
        }
    }
}

/// SIGKILL the child's whole process group. Shells out to `kill` because
/// direct syscalls need `unsafe`, which this workspace forbids.
pub(crate) fn kill_process_group(pid: u32) {
    let _ = std::process::Command::new("kill")
        // `--` keeps a negative PID (= process group) from being parsed as
        // an option, including by Linux's procps kill.
        .args(["-9", "--", &format!("-{pid}")])
        .output();
}

/// Read both pipes concurrently. Dropping the receiver stops the readers,
/// even if a background process still holds a pipe open.
pub(crate) fn output_chunks(child: &mut tokio::process::Child) -> mpsc::Receiver<OutputChunk> {
    let (tx, rx) = mpsc::channel(64);
    if let Some(stdout) = child.stdout.take() {
        spawn_reader(stdout, tx.clone(), OutputChunk::Stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_reader(stderr, tx.clone(), OutputChunk::Stderr);
    }
    rx
}

/// Copy one pipe until EOF or until the run stops listening. Closing our
/// end makes later writes fail with EPIPE; background servers should log
/// to a file rather than inherit the captured pipes.
fn spawn_reader(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: mpsc::Sender<OutputChunk>,
    chunk: fn(Vec<u8>) -> OutputChunk,
) {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = tokio::select! {
                read = pipe.read(&mut buffer) => read,
                () = tx.closed() => break,
            };
            match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk(buffer[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}
