//! Unix process supervision for Bash calls that can return a managed job ID.

use std::io;
use std::os::unix::process::ExitStatusExt as _;

use process_wrap::tokio::ChildWrapper;
use rustix::process::{self, Pid};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::jobs::{Exit, Phase, Termination, process_session};
use crate::process::EXIT_STDIO_GRACE;

pub(super) enum CaptureUpdate<'a> {
    Bytes(&'a [u8]),
    Incomplete,
}

pub(super) struct ManagedChild {
    child: Box<dyn ChildWrapper>,
    session: Option<Pid>,
    child_exit: tokio::signal::unix::Signal,
}

impl ManagedChild {
    #[cfg(test)]
    pub(super) fn leader(&self) -> Option<Pid> {
        self.session
    }

    pub(super) fn spawn(command: Command) -> Result<Self, String> {
        // Without a non-reaping wait, an exited leader's group ID could be
        // recycled before cleanup. Reject starts on those platforms.
        if cfg!(any(
            target_os = "cygwin",
            target_os = "horizon",
            target_os = "openbsd",
            target_os = "redox"
        )) {
            return Err("Managed jobs are unsupported on this Unix platform".to_string());
        }
        // Subscribe before spawn, so even immediate exits wake the supervisor.
        let child_exit = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
            .map_err(|error| format!("Cannot monitor background shell exit: {error}"))?;
        let child = process_session::spawn(command)
            .map_err(|error| format!("Failed to start background shell: {error}"))?;
        let session = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        Ok(Self {
            child,
            session,
            child_exit,
        })
    }

    pub(super) async fn run(
        mut self,
        deadline: Option<Instant>,
        stop: &CancellationToken,
        capture: impl Fn(CaptureUpdate<'_>),
        handoff: Option<&CancellationToken>,
        claim_inline: impl FnOnce() -> bool,
    ) -> Termination {
        // No queued payload or reader tasks. A single supervisor drains both pipes
        // with two fixed 8 KiB buffers and writes directly to the 64 KiB tail.
        let mut pipes = Pipes {
            stdout: Pipe::new(self.child.stdout().take()),
            stderr: Pipe::new(self.child.stderr().take()),
        };
        if pipes.stdout.reader.is_none() || pipes.stderr.reader.is_none() {
            capture(CaptureUpdate::Incomplete);
        }
        let mut phase = tokio::select! {
            biased;
            () = stop.cancelled() => Phase::Stopped,
            () = wait_for_deadline(deadline) => Phase::TimedOut,
            result = self.observe_exit() => match result {
                Ok(()) => Phase::Exited,
                Err(error) => {
                    tracing::warn!("Cannot observe background shell exit: {error}");
                    Phase::Failed
                }
            },
            never = async {
                pipes.drain(&capture).await;
                std::future::pending::<!>().await
            } => match never {},
        };
        let mut inline = false;
        if matches!(phase, Phase::Exited)
            && let Some(handoff) = handoff
        {
            tokio::select! {
                biased;
                () = stop.cancelled() => phase = Phase::Stopped,
                () = wait_for_deadline(deadline) => phase = Phase::TimedOut,
                () = handoff.cancelled() => {},
                () = pipes.drain_until_quiet(&capture) => {
                    inline = claim_inline();
                    if !inline && stop.is_cancelled() {
                        phase = Phase::Stopped;
                    }
                },
            }
        }
        let mut cleanup_error = if inline {
            None
        } else {
            stop.cancel(); // Keep the job live (stopping) until cleanup and capture end.
            let result = match self.session {
                Some(session) => process_session::kill(session).await,
                None => Err(io::Error::other("missing process session")),
            };
            if result.is_err() {
                // Fall back to signaling the leader. A zombie keeps its original status.
                let _ = self.child.start_kill();
            }
            result.err().map(|error| error.to_string())
        };
        self.session = None;
        let status = match self.child.wait().await {
            Ok(status) => Some(status),
            Err(error) => {
                cleanup_error
                    .get_or_insert_with(|| format!("cannot reap background shell: {error}"));
                phase = Phase::Failed;
                None
            }
        };
        // Fixed budget, not a quiet interval: an escaped descendant could hold a
        // pipe open or continue producing output indefinitely after its parent dies.
        if !inline
            && tokio::time::timeout(EXIT_STDIO_GRACE, pipes.drain(&capture))
                .await
                .is_err()
        {
            capture(CaptureUpdate::Incomplete);
        }
        let exit = Exit {
            exit_code: status.and_then(|status| status.code()),
            signal: status.and_then(|status| status.signal()),
        };
        Termination {
            phase,
            exit,
            cleanup_error,
        }
    }

    #[cfg(not(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox"
    )))]
    async fn observe_exit(&mut self) -> io::Result<()> {
        use rustix::io::Errno;
        use rustix::process::{WaitId, WaitIdOptions};

        let leader = self
            .session
            .ok_or_else(|| io::Error::other("missing process session"))?;
        let flags = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        loop {
            // The unreaped leader pins the session ID until cleanup finishes.
            match process::waitid(WaitId::Pid(leader), flags) {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
            if self.child_exit.recv().await.is_none() {
                return Err(io::Error::other("SIGCHLD stream closed"));
            }
        }
    }

    #[cfg(any(
        target_os = "cygwin",
        target_os = "horizon",
        target_os = "openbsd",
        target_os = "redox"
    ))]
    async fn observe_exit(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && let Err(error) = process_session::kill_group(session)
        {
            tracing::warn!(
                ?session,
                "Cannot stop background process group on drop: {error}"
            );
        }
    }
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

struct Pipes<Out, Err> {
    stdout: Pipe<Out>,
    stderr: Pipe<Err>,
}

impl<Out: AsyncRead + Unpin, Err: AsyncRead + Unpin> Pipes<Out, Err> {
    async fn drain(&mut self, capture: &impl Fn(CaptureUpdate<'_>)) {
        while self.read_chunk(capture).await {}
    }

    async fn drain_until_quiet(&mut self, capture: &impl Fn(CaptureUpdate<'_>)) {
        while let Ok(true) = tokio::time::timeout(EXIT_STDIO_GRACE, self.read_chunk(capture)).await
        {
        }
    }

    async fn read_chunk(&mut self, capture: &impl Fn(CaptureUpdate<'_>)) -> bool {
        while self.stdout.reader.is_some() || self.stderr.reader.is_some() {
            let captured = tokio::select! {
                captured = self.stdout.read(capture), if self.stdout.reader.is_some() => captured,
                captured = self.stderr.read(capture), if self.stderr.reader.is_some() => captured,
            };
            if captured {
                return true;
            }
        }
        false
    }
}

struct Pipe<R> {
    reader: Option<R>,
    buffer: [u8; 8192],
}

impl<R: AsyncRead + Unpin> Pipe<R> {
    fn new(reader: Option<R>) -> Self {
        Self {
            reader,
            buffer: [0; 8192],
        }
    }

    async fn read(&mut self, capture: &impl Fn(CaptureUpdate<'_>)) -> bool {
        if let Some(reader) = &mut self.reader {
            match reader.read(&mut self.buffer).await {
                Ok(0) => self.reader = None,
                Ok(n) => {
                    capture(CaptureUpdate::Bytes(&self.buffer[..n]));
                    return true;
                }
                Err(error) => {
                    tracing::warn!("Cannot read background shell output: {error}");
                    capture(CaptureUpdate::Incomplete);
                    self.reader = None;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::ReadBuf;

    use super::*;

    struct BrokenPipe;

    impl AsyncRead for BrokenPipe {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }
    }

    #[tokio::test]
    async fn capture_error_does_not_discard_the_other_pipe() {
        let mut pipes = Pipes {
            stdout: Pipe::new(Some(BrokenPipe)),
            stderr: Pipe::new(Some(&b"stderr still captured"[..])),
        };
        let incomplete = Cell::new(false);
        let seen = Cell::new(0);
        pipes
            .drain(&|update| match update {
                CaptureUpdate::Incomplete => incomplete.set(true),
                CaptureUpdate::Bytes(bytes) => seen.set(seen.get() + bytes.len()),
            })
            .await;
        assert!(incomplete.get(), "a read error is not EOF");
        assert_eq!(seen.get(), b"stderr still captured".len());
    }
}
