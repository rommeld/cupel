//! Bounded, session-local state for managed shell commands. Nothing is persisted.

#[cfg(unix)]
mod child;
#[cfg(unix)]
mod process_session;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
#[cfg(unix)]
use tokio::sync::oneshot;
use tokio::sync::watch;
#[cfg(unix)]
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use crate::tools::bash::{OutputAccumulator, RunOutcome};

const MAX_LIVE: usize = 2;
const MAX_COMPLETED: usize = 8;
const TAIL_BYTES: usize = 64 * 1024;

/// The live jobs and retained results for one session, including unsaved sessions.
#[derive(Default)]
pub struct Jobs {
    // Workers own the state, not the Jobs handle: dropping the last handle
    // still requests shutdown, even when workers are running.
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    closed: bool,
    live: Vec<Live>,
    completed: VecDeque<Arc<Completed>>,
}

struct Live {
    record: Record,
    stop: CancellationToken,
    finished: CancellationToken,
    completion: watch::Sender<Option<Arc<Completed>>>,
    visibility: Visibility,
}

enum Visibility {
    Published,
    #[cfg(unix)]
    Pending {
        completion: oneshot::Sender<Completed>,
        output: PendingOutput,
        handoff: Handoff,
    },
    #[cfg(unix)]
    Discarded,
}

#[cfg(unix)]
enum Handoff {
    Available(CancellationToken),
    InlineClaimed,
}

#[cfg(unix)]
type PendingOutput = Arc<Mutex<Option<Box<OutputAccumulator>>>>;

#[cfg(unix)]
struct Spawned {
    id: String,
    stop: CancellationToken,
    finished: CancellationToken,
    started_at: Instant,
    interrupted: bool,
}

#[cfg(unix)]
pub(crate) enum YieldOutcome {
    Finished {
        outcome: RunOutcome,
        output: Box<OutputAccumulator>,
        cleanup_error: Option<String>,
    },
    Yielded(JobSnapshot),
}

#[cfg(unix)]
pub(crate) struct JobSnapshot {
    pub details: serde_json::Value,
    pub output: String,
}

#[cfg(unix)]
impl JobSnapshot {
    fn new(summary: Summary<'_>, capture: &Capture) -> Self {
        Self {
            details: serde_json::json!(summary),
            output: capture.output(),
        }
    }
}

struct Completed {
    record: Record,
    termination: Termination,
    evicted_job_id: Option<String>,
}

struct Termination {
    phase: Phase,
    exit: Exit,
    cleanup_error: Option<String>,
}

struct Record {
    id: String,
    command: String,
    capture: Capture,
}

#[derive(Default)]
struct Capture {
    tail: VecDeque<u8>,
    truncated: bool,
    incomplete: bool,
}

impl Capture {
    #[cfg(unix)]
    fn append(&mut self, bytes: &[u8]) {
        let excess = (self.tail.len() + bytes.len()).saturating_sub(TAIL_BYTES);
        self.truncated |= excess != 0;
        self.tail.drain(..excess.min(self.tail.len()));
        self.tail
            .extend(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]);
    }

    fn output(&self) -> String {
        let bytes: Vec<u8> = self.tail.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum Phase {
    Running,
    Stopping,
    #[cfg(unix)]
    Exited,
    #[cfg(unix)]
    Stopped,
    #[cfg(unix)]
    TimedOut,
    #[cfg(unix)]
    Failed,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Exit {
    exit_code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Summary<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    command_preview: &'a str,
    state: Phase,
    exit: Option<&'a Exit>,
    truncated: bool,
    capture_incomplete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evicted_job_id: Option<&'a str>,
}

impl Record {
    fn summary(&self, phase: Phase) -> Summary<'_> {
        Summary {
            id: Some(&self.id),
            command_preview: &self.command,
            state: phase,
            exit: None,
            truncated: self.capture.truncated,
            capture_incomplete: self.capture.incomplete,
            cleanup_error: None,
            evicted_job_id: None,
        }
    }
}

impl Live {
    fn is_published(&self) -> bool {
        matches!(self.visibility, Visibility::Published)
    }

    fn summary(&self) -> Summary<'_> {
        self.record.summary(if self.stop.is_cancelled() {
            Phase::Stopping
        } else {
            Phase::Running
        })
    }
}

impl Completed {
    fn summary(&self) -> Summary<'_> {
        Summary {
            exit: Some(&self.termination.exit),
            cleanup_error: self.termination.cleanup_error.as_deref(),
            evicted_job_id: self.evicted_job_id.as_deref(),
            ..self.record.summary(self.termination.phase)
        }
    }

    fn output(&self) -> (serde_json::Value, Option<String>) {
        (
            serde_json::json!(self.summary()),
            Some(self.record.capture.output()),
        )
    }

    fn cleanup_result(&self) -> Result<(), String> {
        if let Some(error) = &self.termination.cleanup_error {
            return Err(format!(
                "Job {} cleanup was incomplete: {error}",
                self.record.id
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn inline_result(self, output: &PendingOutput) -> Result<YieldOutcome, String> {
        let outcome = match self.termination.phase {
            Phase::Exited => RunOutcome::Exited(self.termination.exit.exit_code),
            Phase::TimedOut => RunOutcome::TimedOut,
            Phase::Stopped | Phase::Stopping => RunOutcome::Aborted,
            Phase::Running | Phase::Failed => RunOutcome::Failed,
        };
        let mut output = output
            .lock()
            .map_err(|error| format!("Inline output unavailable: {error}"))?
            .take()
            .ok_or("Missing inline output")?;
        if self.record.capture.incomplete {
            output.mark_incomplete();
        }
        Ok(YieldOutcome::Finished {
            outcome,
            output,
            cleanup_error: self.termination.cleanup_error,
        })
    }
}

impl Jobs {
    /// Why an operation that requires a quiet checkout cannot run right now.
    /// Completed results don't prevent any operations.
    pub fn require_idle(&self) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|error| format!("Job state unavailable: {error}"))?;
        if state.live.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = state
            .live
            .iter()
            .filter(|job| job.is_published())
            .map(|job| job.record.id.as_str())
            .collect();
        let ids = if ids.is_empty() {
            String::new()
        } else {
            format!(" ({})", ids.join(", "))
        };
        Err(format!(
            "managed jobs are still live{ids}; wait for them or use the jobs tool to stop them"
        ))
    }

    /// Close admission, stop and await every live command, and discard retained results.
    /// This registry is not reused after shutdown.
    pub async fn shutdown(&self) {
        for finished in self.close() {
            finished.cancelled().await;
        }
    }

    pub(crate) fn close(&self) -> Vec<CancellationToken> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        state.closed = true;
        state.completed.clear();
        let mut finishing = Vec::with_capacity(state.live.len());
        for job in &state.live {
            job.stop.cancel();
            finishing.push(job.finished.clone());
        }
        finishing
    }

    #[cfg(unix)]
    pub(crate) async fn start(
        &self,
        command: tokio::process::Command,
        display_command: &str,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let started = self.spawn(
            display_command,
            timeout,
            Visibility::Published,
            cancel,
            || child::ManagedChild::spawn(command),
        )?;
        if started.interrupted {
            started.finished.cancelled().await;
            return Err("Operation aborted during background start".to_string());
        }
        Ok(started.id)
    }

    /// Keep the job private and stop it if the caller goes away before handoff.
    #[cfg(unix)]
    pub(crate) async fn start_yielding(
        &self,
        command: tokio::process::Command,
        display_command: &str,
        timeout: Option<Duration>,
        yield_after: Duration,
        cancel: &CancellationToken,
    ) -> Result<YieldOutcome, String> {
        let (sender, mut completion) = oneshot::channel();
        let output = Arc::new(Mutex::new(Some(Box::new(OutputAccumulator::new()))));
        let started = self.spawn(
            display_command,
            timeout,
            Visibility::Pending {
                completion: sender,
                output: Arc::clone(&output),
                handoff: Handoff::Available(CancellationToken::new()),
            },
            cancel,
            || child::ManagedChild::spawn(command),
        )?;
        let stop_on_drop = started.stop.drop_guard();
        let yield_at = started
            .started_at
            .checked_add(yield_after)
            .ok_or("yield_after_ms is too large")?;

        let mut try_handoff = true;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                result = &mut completion => {
                    return result
                        .map_err(|error| format!("Command ended without an inline result: {error}"))?
                        .inline_result(&output);
                }
                () = tokio::time::sleep_until(yield_at), if try_handoff => {
                    try_handoff = false;
                    let mut state = self.state.lock()
                        .map_err(|error| format!("Job state unavailable: {error}"))?;
                    if cancel.is_cancelled() {
                        break;
                    }
                    if state.closed {
                        return Err("This job session has ended".to_string());
                    }
                    if let Some(job) = state.live.iter_mut().find(|job| job.record.id == started.id)
                        && let Visibility::Pending { handoff: Handoff::Available(published), .. } = &job.visibility
                    {
                        published.cancel();
                        job.visibility = Visibility::Published;
                        stop_on_drop.disarm();
                        let snapshot = JobSnapshot::new(job.summary(), &job.record.capture);
                        drop(state);
                        // Dispose of the private spill without holding the registry lock.
                        if let Ok(mut output) = output.lock() {
                            output.take();
                        }
                        return Ok(YieldOutcome::Yielded(snapshot));
                    }
                }
            }
        }

        drop(stop_on_drop);
        started.finished.cancelled().await;
        Err("Operation aborted".to_string())
    }

    #[cfg(unix)]
    fn spawn(
        &self,
        display_command: &str,
        timeout: Option<Duration>,
        visibility: Visibility,
        cancel: &CancellationToken,
        spawn_child: impl FnOnce() -> Result<child::ManagedChild, String>,
    ) -> Result<Spawned, String> {
        use std::fmt::Write as _;

        let mut state = self
            .state
            .lock()
            .map_err(|error| format!("Job state unavailable: {error}"))?;
        if state.closed {
            return Err("This job session has ended".to_string());
        }
        if state.live.len() >= MAX_LIVE {
            return Err(format!(
                "At most {MAX_LIVE} managed jobs can be live; use jobs to list or stop them"
            ));
        }
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| format!("Cannot create job id: {error}"))?;
        let mut id = String::from("J");
        for byte in bytes {
            let _ = write!(id, "{byte:02x}");
        }
        let stop = CancellationToken::new();
        let finished = CancellationToken::new();
        let record = Record {
            id: id.clone(),
            command: display_command[..display_command.floor_char_boundary(256)].to_string(),
            capture: Capture {
                tail: VecDeque::with_capacity(TAIL_BYTES),
                ..Capture::default()
            },
        };
        if cancel.is_cancelled() {
            return Err("Operation aborted".to_string());
        }
        let deadline = timeout
            .map(|duration| {
                Instant::now()
                    .checked_add(duration)
                    .ok_or("timeout deadline is out of range")
            })
            .transpose()?;
        let child = spawn_child()?;
        let started_at = Instant::now();
        // If cancellation won during spawn, the worker still reaps the child
        // but no handle is exposed.
        let interrupted = cancel.is_cancelled();
        if interrupted {
            stop.cancel();
        }
        let shared = Arc::clone(&self.state);
        let worker_id = id.clone();
        let done = finished.clone().drop_guard();
        let worker_stop = stop.clone();
        let caller_cancel = cancel.clone();
        let (pending_output, handoff) = match &visibility {
            Visibility::Pending {
                output,
                handoff: Handoff::Available(published),
                ..
            } if !interrupted => (Some(Arc::clone(output)), Some(published.clone())),
            _ => (None, None),
        };
        tokio::spawn(async move {
            let _done = done;
            let capture = |update: child::CaptureUpdate<'_>| {
                if let child::CaptureUpdate::Bytes(bytes) = update
                    && let Some(output) = &pending_output
                    && let Ok(mut output) = output.lock()
                    && let Some(output) = output.as_mut()
                {
                    output.append(bytes);
                }
                if let Ok(mut state) = shared.lock()
                    && let Some(job) = state.live.iter_mut().find(|job| job.record.id == worker_id)
                {
                    let capture = &mut job.record.capture;
                    match update {
                        child::CaptureUpdate::Bytes(bytes) => capture.append(bytes),
                        child::CaptureUpdate::Incomplete => capture.incomplete = true,
                    }
                }
            };
            let claim_inline = || {
                if let Ok(mut state) = shared.lock()
                    && !state.closed
                    && !worker_stop.is_cancelled()
                    && let Some(job) = state.live.iter_mut().find(|job| job.record.id == worker_id)
                    && let Visibility::Pending { handoff, .. } = &mut job.visibility
                    && matches!(handoff, Handoff::Available(_))
                {
                    if caller_cancel.is_cancelled() {
                        worker_stop.cancel();
                        return false;
                    }
                    *handoff = Handoff::InlineClaimed;
                    return true;
                }
                false
            };
            let termination = child
                .run(
                    deadline,
                    &worker_stop,
                    capture,
                    handoff.as_ref(),
                    claim_inline,
                )
                .await;
            if let Some(error) = &termination.cleanup_error {
                tracing::warn!(job_id = %worker_id, "Managed job cleanup was incomplete: {error}");
            }
            if let Ok(mut state) = shared.lock() {
                state.complete(&worker_id, termination);
            }
        });
        state.live.push(Live {
            record,
            stop: stop.clone(),
            finished: finished.clone(),
            completion: watch::Sender::new(None),
            visibility: if interrupted {
                Visibility::Discarded
            } else {
                visibility
            },
        });
        Ok(Spawned {
            id,
            stop,
            finished,
            started_at,
            interrupted,
        })
    }

    pub(crate) fn list(&self) -> Result<serde_json::Value, String> {
        let state = self
            .state
            .lock()
            .map_err(|error| format!("Job state unavailable: {error}"))?;
        let live = state
            .live
            .iter()
            .filter(|job| job.is_published())
            .map(Live::summary);
        let completed = state.completed.iter().map(|job| job.summary());
        let jobs: Vec<_> = live.chain(completed).collect();
        Ok(serde_json::json!({ "jobs": jobs }))
    }

    pub(crate) fn inspect(
        &self,
        id: &str,
        include_output: bool,
    ) -> Result<(serde_json::Value, Option<String>), String> {
        let state = self
            .state
            .lock()
            .map_err(|error| format!("Job state unavailable: {error}"))?;
        state.inspect(id, include_output)
    }

    /// Waits for completion or returns the current output snapshot at the deadline.
    pub(crate) async fn wait(
        &self,
        id: &str,
        duration: Duration,
        cancel: &CancellationToken,
    ) -> Result<(serde_json::Value, Option<String>), String> {
        let mut completion = {
            let state = self
                .state
                .lock()
                .map_err(|error| format!("Job state unavailable: {error}"))?;
            match state.live_job(id) {
                Some(job) if !duration.is_zero() => job.completion.subscribe(),
                _ => return state.inspect(id, true),
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err("Operation aborted".to_string()),
            _ = completion.changed() => {},
            () = tokio::time::sleep(duration) => {},
        }
        let state = self
            .state
            .lock()
            .map_err(|error| format!("Job state unavailable: {error}"))?;
        if state.closed {
            return Err(unknown_job());
        }
        if let Some(completed) = completion.borrow().as_ref() {
            return Ok(completed.output());
        }
        state.inspect(id, true)
    }

    /// Requests cleanup before awaiting it. Dropping the caller cannot undo the stop.
    pub(crate) async fn stop(&self, id: &str) -> Result<(), String> {
        let mut completion = {
            let state = self
                .state
                .lock()
                .map_err(|error| format!("Job state unavailable: {error}"))?;
            if let Some(job) = state.live_job(id) {
                let completion = job.completion.subscribe();
                job.stop.cancel();
                completion
            } else if let Some(job) = state.completed_job(id) {
                return job.cleanup_result();
            } else {
                return Err(unknown_job());
            }
        };
        completion
            .changed()
            .await
            .map_err(|error| format!("Job {id} ended without a cleanup result: {error}"))?;
        completion
            .borrow()
            .as_ref()
            .ok_or_else(|| format!("Job {id} has no cleanup result"))?
            .cleanup_result()
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.close();
    }
}

impl State {
    fn live_job(&self, id: &str) -> Option<&Live> {
        self.live
            .iter()
            .find(|job| job.is_published() && job.record.id == id)
    }

    fn completed_job(&self, id: &str) -> Option<&Completed> {
        self.completed
            .iter()
            .map(Arc::as_ref)
            .find(|job| job.record.id == id)
    }

    fn inspect(
        &self,
        id: &str,
        include_output: bool,
    ) -> Result<(serde_json::Value, Option<String>), String> {
        let (summary, record) = if let Some(job) = self.live_job(id) {
            (job.summary(), &job.record)
        } else if let Some(job) = self.completed_job(id) {
            (job.summary(), &job.record)
        } else {
            return Err(unknown_job());
        };
        let output = include_output.then(|| record.capture.output());
        Ok((serde_json::json!(summary), output))
    }

    #[cfg(unix)]
    fn complete(&mut self, id: &str, termination: Termination) {
        let Some(index) = self.live.iter().position(|job| job.record.id == id) else {
            return;
        };
        let job = self.live.remove(index);
        let completed = Completed {
            record: job.record,
            termination,
            evicted_job_id: None,
        };
        match job.visibility {
            Visibility::Published => {
                let completed = if self.closed {
                    Arc::new(completed)
                } else {
                    self.retain(completed)
                };
                let _ = job.completion.send(Some(completed));
            }
            Visibility::Pending { completion, .. } if !self.closed => {
                let _ = completion.send(completed);
            }
            Visibility::Discarded | Visibility::Pending { .. } => {}
        }
    }

    #[cfg(unix)]
    fn retain(&mut self, mut completed: Completed) -> Arc<Completed> {
        let evicted = if self.completed.len() == MAX_COMPLETED {
            self.completed.pop_front().map(|job| job.record.id.clone())
        } else {
            None
        };
        completed.evicted_job_id = evicted;
        let completed = Arc::new(completed);
        self.completed.push_back(Arc::clone(&completed));
        completed
    }
}

fn unknown_job() -> String {
    "Job is unknown or expired in this session; use jobs with action=list".to_string()
}

#[cfg(test)]
#[cfg(unix)]
#[path = "jobs/tests.rs"]
mod tests;
