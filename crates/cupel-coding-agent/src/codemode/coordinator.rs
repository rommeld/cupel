use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use serde_json::Value;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use crate::codemode::{
    ARGUMENT_BYTES, CALL_TIME, CALLBACK_BYTES, CALLBACKS, CLEANUP_TIME, Callback, EMIT_BYTES,
    Failure, GUEST_TIME, Request, SOURCE_BYTES, WORKER_ARGUMENT, frame,
};

/// Share one coordinator across all preview calls in the process.
#[derive(Clone)]
pub struct Coordinator {
    binary: PathBuf,
    gate: Arc<Semaphore>,
}

impl Coordinator {
    pub(super) fn binary(&self) -> &std::path::Path {
        &self.binary
    }

    /// `binary` must be the trusted, absolute path of the Cupel executable.
    #[must_use]
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            gate: Arc::new(Semaphore::new(1)),
        }
    }

    /// The broker must return only after its callback has stopped, even on
    /// cancellation. Return [`Failure::Cleanup`] if it cannot confirm that.
    pub async fn run<F>(
        &self,
        source: String,
        cancel: &CancellationToken,
        broker: impl FnMut(Request, CancellationToken) -> F + Send + 'static,
    ) -> Result<Vec<Value>, Failure>
    where
        F: Future<Output = Result<Value, Failure>> + Send + 'static,
    {
        let deadline = Instant::now() + CALL_TIME;
        if source.len() > SOURCE_BYTES {
            return Err(Failure::Source);
        }
        let call_cancel = cancel.child_token();
        let _cancel_on_drop = call_cancel.clone().drop_guard();
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator
                .supervise(source, call_cancel, deadline, broker)
                .await
        })
        .await
        .map_err(|_error| Failure::Cleanup)?
    }

    async fn supervise<F>(
        &self,
        source: String,
        cancel: CancellationToken,
        deadline: Instant,
        broker: impl FnMut(Request, CancellationToken) -> F,
    ) -> Result<Vec<Value>, Failure>
    where
        F: Future<Output = Result<Value, Failure>>,
    {
        let permit = until(&cancel, deadline, Failure::Deadline, async {
            self.gate.acquire().await.map_err(|_error| Failure::Cleanup)
        })
        .await?;
        let mut admission = Admission {
            gate: &self.gate,
            _permit: permit,
            confirmed: false,
        };
        let result = self.run_admitted(source, &cancel, deadline, broker).await;
        admission.confirmed = result != Err(Failure::Cleanup);
        result
    }

    async fn run_admitted<F>(
        &self,
        source: String,
        cancel: &CancellationToken,
        deadline: Instant,
        broker: impl FnMut(Request, CancellationToken) -> F,
    ) -> Result<Vec<Value>, Failure>
    where
        F: Future<Output = Result<Value, Failure>>,
    {
        check_cancel(cancel, deadline, Failure::Deadline)?;
        if !self.binary.is_absolute() {
            return Err(Failure::Worker);
        }
        let mut child = Command::new(&self.binary)
            .arg(WORKER_ARGUMENT)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_error| Failure::Worker)?;
        let result = match (child.stdin.take(), child.stdout.take()) {
            (Some(input), Some(output)) => {
                let mut guest = Guest {
                    child: &mut child,
                    input,
                    output,
                    cancel,
                    deadline,
                    execution_deadline: deadline,
                };
                guest.exchange(&source, broker).await
            }
            _ => Err(Failure::Worker),
        };
        if let Err(error) = result {
            let _ = child.start_kill();
            if !matches!(time::timeout(CLEANUP_TIME, child.wait()).await, Ok(Ok(_))) {
                return Err(Failure::Cleanup);
            }
            return Err(error);
        }
        result
    }
}

struct Admission<'a> {
    gate: &'a Semaphore,
    _permit: SemaphorePermit<'a>,
    confirmed: bool,
}

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if !self.confirmed {
            self.gate.close();
        }
    }
}

struct Guest<'a> {
    child: &'a mut Child,
    input: ChildStdin,
    output: ChildStdout,
    cancel: &'a CancellationToken,
    deadline: Instant,
    execution_deadline: Instant,
}

impl Guest<'_> {
    async fn exchange<F>(
        &mut self,
        source: &str,
        mut broker: impl FnMut(Request, CancellationToken) -> F,
    ) -> Result<Vec<Value>, Failure>
    where
        F: Future<Output = Result<Value, Failure>>,
    {
        until(
            self.cancel,
            self.deadline,
            Failure::Deadline,
            frame::read_async(&mut self.output, |tag| match tag {
                frame::READY => Ok(0),
                _ => Err(Failure::Worker),
            }),
        )
        .await?;
        self.execution_deadline = Instant::now() + GUEST_TIME;
        let mut callback_count = 0;
        let mut callback_bytes = CALLBACK_BYTES;
        let mut emit_bytes = EMIT_BYTES;
        let mut emits = Vec::new();
        self.write(frame::SOURCE, source.as_bytes()).await?;
        loop {
            let (deadline, limit) = self.guest_limit();
            let frame = until(
                self.cancel,
                deadline,
                limit,
                frame::read_async(&mut self.output, |tag| match tag {
                    frame::READ | frame::GREP => Ok(ARGUMENT_BYTES),
                    frame::EMIT => Ok(emit_bytes),
                    frame::FAILURE => Ok(1),
                    frame::DONE => Ok(0),
                    _ => Err(Failure::Worker),
                }),
            )
            .await;
            let (tag, payload) = match frame {
                Err(Failure::Worker) => {
                    self.wait().await?;
                    return Err(Failure::Worker);
                }
                other => other?,
            };
            match tag {
                frame::READ | frame::GREP => {
                    if callback_count == CALLBACKS {
                        return Err(Failure::CallbackCount);
                    }
                    callback_count += 1;
                    let arguments: Value =
                        serde_json::from_slice(&payload).map_err(|_error| Failure::Arguments)?;
                    if !arguments.is_object() {
                        return Err(Failure::Arguments);
                    }
                    let callback = if tag == frame::READ {
                        Callback::Read
                    } else {
                        Callback::Grep
                    };
                    let request = Request {
                        callback,
                        arguments,
                    };
                    let value = self.callback(request, &mut broker).await?;
                    let mut buffer = frame::Buffer::new(callback_bytes);
                    serde_json::to_writer(&mut buffer, &value)
                        .map_err(|_error| Failure::CallbackBytes)?;
                    let bytes = buffer.into_bytes();
                    callback_bytes -= bytes.len();
                    self.write(frame::REPLY, &bytes).await?;
                }
                frame::EMIT => {
                    emit_bytes -= payload.len();
                    emits.push(serde_json::from_slice(&payload).map_err(|_error| Failure::Worker)?);
                }
                frame::FAILURE => return Err(frame::decode_error(&payload)),
                frame::DONE => {
                    self.wait().await?;
                    return Ok(emits);
                }
                _ => return Err(Failure::Worker),
            }
        }
    }

    async fn callback<F>(
        &mut self,
        request: Request,
        broker: &mut impl FnMut(Request, CancellationToken) -> F,
    ) -> Result<Value, Failure>
    where
        F: Future<Output = Result<Value, Failure>>,
    {
        let (deadline, limit) = self.guest_limit();
        check_cancel(self.cancel, deadline, limit)?;
        let started = Instant::now();
        let cancel = self.cancel.child_token();
        let mut active = std::pin::pin!(broker(request, cancel.clone()));
        let result = tokio::select! {
            biased;
            () = self.cancel.cancelled() => Failure::Cancelled,
            () = time::sleep_until(self.deadline) => Failure::Deadline,
            result = &mut active => {
                self.execution_deadline += started.elapsed();
                return result;
            }
        };
        cancel.cancel();
        let _ = self.child.start_kill();
        match time::timeout(CLEANUP_TIME, active).await {
            Err(_) | Ok(Err(Failure::Cleanup)) => Err(Failure::Cleanup),
            Ok(_) => Err(result),
        }
    }

    fn guest_limit(&self) -> (Instant, Failure) {
        if self.execution_deadline < self.deadline {
            (self.execution_deadline, Failure::GuestTime)
        } else {
            (self.deadline, Failure::Deadline)
        }
    }

    async fn write(&mut self, tag: u8, data: &[u8]) -> Result<(), Failure> {
        let (deadline, limit) = self.guest_limit();
        until(
            self.cancel,
            deadline,
            limit,
            frame::write_async(&mut self.input, tag, data),
        )
        .await
    }

    async fn wait(&mut self) -> Result<(), Failure> {
        let (deadline, limit) = self.guest_limit();
        until(self.cancel, deadline, limit, async {
            let status = self.child.wait().await.map_err(|_error| Failure::Worker)?;
            if status.success() {
                Ok(())
            } else if status.code() == Some(monty_types::OOM_EXIT_CODE) {
                Err(Failure::Memory)
            } else {
                Err(Failure::Worker)
            }
        })
        .await
    }
}

fn check_cancel(
    cancel: &CancellationToken,
    deadline: Instant,
    expired: Failure,
) -> Result<(), Failure> {
    if cancel.is_cancelled() {
        Err(Failure::Cancelled)
    } else if Instant::now() >= deadline {
        Err(expired)
    } else {
        Ok(())
    }
}

async fn until<T>(
    cancel: &CancellationToken,
    deadline: Instant,
    expired: Failure,
    action: impl Future<Output = Result<T, Failure>>,
) -> Result<T, Failure> {
    check_cancel(cancel, deadline, expired)?;
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Failure::Cancelled),
        () = time::sleep_until(deadline) => Err(expired),
        result = action => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expired_queue_entry_never_spawns() {
        let coordinator = Coordinator::new(PathBuf::new());
        let permit = coordinator.gate.acquire().await.unwrap();
        let result = coordinator
            .supervise(
                String::new(),
                CancellationToken::new(),
                Instant::now() + std::time::Duration::from_millis(10),
                |_, _| async { panic!("an expired call reached the broker") },
            )
            .await;
        assert_eq!(
            result,
            Err(Failure::Deadline),
            "the missing worker would return a different error"
        );
        drop(permit);
        assert_eq!(coordinator.gate.available_permits(), 1);
    }
}
