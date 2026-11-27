use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;

use serde_json::Value;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use crate::codemode::{
    ARGUMENT_BYTES, CALL_TIME, CALLBACK_BYTES, CALLBACKS, CLEANUP_TIME, Callback, EMIT_BYTES,
    Failure, GUEST_TIME, Request, SOURCE_BYTES, WORKER_ARGUMENT, frame,
};

static PROCESS_SLOTS: Semaphore = Semaphore::const_new(2);
static PREVIEW_SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Copy)]
enum AdmissionMode {
    Preview,
    V1Qualification,
}

#[derive(Clone)]
pub struct Coordinator {
    binary: PathBuf,
    mode: AdmissionMode,
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
            mode: AdmissionMode::Preview,
        }
    }

    /// Uses both process slots for qualification without changing the preview limit.
    #[must_use]
    pub fn for_v1_qualification(binary: PathBuf) -> Self {
        Self {
            binary,
            mode: AdmissionMode::V1Qualification,
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
        let budget = Budget::starting_at(Instant::now());
        if source.len() > SOURCE_BYTES {
            return Err(Failure::Source);
        }
        let call_cancel = cancel.child_token();
        let _cancel_on_drop = call_cancel.clone().drop_guard();
        let coordinator = self.clone();
        let supervisor = tokio::spawn(async move {
            coordinator
                .supervise(source, call_cancel, budget, broker)
                .await
        });
        match time::timeout_at(budget.return_by, supervisor).await {
            Ok(Ok(result)) => result,
            _ => {
                close_admission();
                Err(Failure::Cleanup)
            }
        }
    }

    async fn supervise<F>(
        &self,
        source: String,
        cancel: CancellationToken,
        budget: Budget,
        broker: impl FnMut(Request, CancellationToken) -> F,
    ) -> Result<Vec<Value>, Failure>
    where
        F: Future<Output = Result<Value, Failure>>,
    {
        let mut admission = until(
            &cancel,
            budget.work_until,
            Failure::Deadline,
            Admission::acquire(self.mode),
        )
        .await?;
        let result = self.run_admitted(source, &cancel, budget, broker).await;
        admission.confirmed = result != Err(Failure::Cleanup);
        result
    }

    async fn run_admitted<F>(
        &self,
        source: String,
        cancel: &CancellationToken,
        budget: Budget,
        broker: impl FnMut(Request, CancellationToken) -> F,
    ) -> Result<Vec<Value>, Failure>
    where
        F: Future<Output = Result<Value, Failure>>,
    {
        check_cancel(cancel, budget.work_until, Failure::Deadline)?;
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
                    budget,
                    execution_deadline: budget.work_until,
                };
                guest.exchange(&source, broker).await
            }
            _ => Err(Failure::Worker),
        };
        if let Err(error) = result {
            stop_and_reap(&mut child, cleanup_until(budget.return_by)).await?;
            return Err(error);
        }
        result
    }
}

#[derive(Clone, Copy)]
struct Budget {
    work_until: Instant,
    return_by: Instant,
}

impl Budget {
    fn starting_at(started: Instant) -> Self {
        let return_by = started + CALL_TIME;
        Self {
            work_until: return_by - CLEANUP_TIME,
            return_by,
        }
    }
}

struct Admission {
    _process: SemaphorePermit<'static>,
    _preview: Option<SemaphorePermit<'static>>,
    confirmed: bool,
}

impl Admission {
    async fn acquire(mode: AdmissionMode) -> Result<Self, Failure> {
        let preview = match mode {
            AdmissionMode::Preview => Some(
                PREVIEW_SLOT
                    .acquire()
                    .await
                    .map_err(|_error| Failure::Cleanup)?,
            ),
            AdmissionMode::V1Qualification => None,
        };
        let process = PROCESS_SLOTS
            .acquire()
            .await
            .map_err(|_error| Failure::Cleanup)?;
        Ok(Self {
            _process: process,
            _preview: preview,
            confirmed: false,
        })
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if !self.confirmed {
            close_admission();
        }
    }
}

fn close_admission() {
    PROCESS_SLOTS.close();
    PREVIEW_SLOT.close();
}

pub(super) fn cleanup_until(return_by: Instant) -> Instant {
    (Instant::now() + CLEANUP_TIME).min(return_by)
}

pub(super) async fn stop_and_reap(child: &mut Child, stop_by: Instant) -> Result<(), Failure> {
    let _ = child.start_kill();
    match time::timeout_at(stop_by, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        _ => Err(Failure::Cleanup),
    }
}

struct Guest<'a> {
    child: &'a mut Child,
    input: ChildStdin,
    output: ChildStdout,
    cancel: &'a CancellationToken,
    budget: Budget,
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
            self.budget.work_until,
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
                        return_by: self.budget.return_by,
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
            () = time::sleep_until(self.budget.work_until) => Failure::Deadline,
            result = &mut active => {
                self.execution_deadline += started.elapsed();
                return result;
            }
        };
        cancel.cancel();
        let stop_by = cleanup_until(self.budget.return_by);
        let (reaped, callback) = tokio::join!(
            stop_and_reap(self.child, stop_by),
            time::timeout_at(stop_by, active)
        );
        reaped?;
        match callback {
            Err(_) | Ok(Err(Failure::Cleanup)) => Err(Failure::Cleanup),
            Ok(_) => Err(result),
        }
    }

    fn guest_limit(&self) -> (Instant, Failure) {
        if self.execution_deadline < self.budget.work_until {
            (self.execution_deadline, Failure::GuestTime)
        } else {
            (self.budget.work_until, Failure::Deadline)
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
