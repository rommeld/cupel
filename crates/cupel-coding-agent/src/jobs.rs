//! Bounded, session-local state for managed shell commands. Nothing is persisted.

#[cfg(unix)]
mod child;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
#[cfg(unix)]
use tokio::sync::oneshot;
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
    completed: VecDeque<Completed>,
}

struct Live {
    record: Record,
    stop: CancellationToken,
    finished: CancellationToken,
    visibility: Visibility,
}

enum Visibility {
    Published,
    #[cfg(unix)]
    Pending {
        completion: oneshot::Sender<Completed>,
        output: PendingOutput,
    },
    #[cfg(unix)]
    Discarded,
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
    phase: Phase,
    exit: Exit,
    evicted_job_id: Option<String>,
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
            exit: Some(&self.exit),
            evicted_job_id: self.evicted_job_id.as_deref(),
            ..self.record.summary(self.phase)
        }
    }

    #[cfg(unix)]
    fn inline_result(&self, output: &PendingOutput) -> Result<YieldOutcome, String> {
        let outcome = match self.phase {
            Phase::Exited => RunOutcome::Exited(self.exit.exit_code),
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
        Ok(YieldOutcome::Finished { outcome, output })
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
            },
            cancel,
            || child::ManagedChild::spawn(command),
        )?;
        let stop_on_drop = started.stop.drop_guard();
        let yield_at = started
            .started_at
            .checked_add(yield_after)
            .ok_or("yield_after_ms is too large")?;

        tokio::select! {
            biased;
            () = cancel.cancelled() => {},
            result = &mut completion => {
                return result
                    .map_err(|error| format!("Command ended without an inline result: {error}"))?
                    .inline_result(&output);
            }
            () = tokio::time::sleep_until(yield_at) => {
                let mut state = self.state.lock()
                    .map_err(|error| format!("Job state unavailable: {error}"))?;
                if !cancel.is_cancelled() {
                    if state.closed {
                        return Err("This job session has ended".to_string());
                    }
                    if let Some(job) = state.live.iter_mut().find(|job| job.record.id == started.id) {
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
                    // Completion removes the private job and sends its result under this same lock.
                    drop(state);
                    return completion.try_recv()
                        .map_err(|error| format!("Could not retrieve command result at handoff: {error}"))?
                        .inline_result(&output);
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
        let child = spawn_child()?;
        let started_at = Instant::now();
        let deadline = timeout.map(|timeout| started_at + timeout);
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
        let pending_output = match &visibility {
            Visibility::Pending { output, .. } if !interrupted => Some(Arc::clone(output)),
            _ => None,
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
            let (phase, exit) = child.run(deadline, &worker_stop, capture).await;
            if let Ok(mut state) = shared.lock() {
                state.complete(&worker_id, phase, exit);
            }
        });
        state.live.push(Live {
            record,
            stop: stop.clone(),
            finished: finished.clone(),
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
        let completed = state.completed.iter().map(Completed::summary);
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
        let (summary, record) = if let Some(job) = state.live_job(id) {
            (job.summary(), &job.record)
        } else if let Some(job) = state.completed_job(id) {
            (job.summary(), &job.record)
        } else {
            return Err(unknown_job());
        };
        let output = include_output.then(|| record.capture.output());
        Ok((serde_json::json!(summary), output))
    }

    /// Waits for completion or returns the current output snapshot at the deadline.
    pub(crate) async fn wait(
        &self,
        id: &str,
        duration: Duration,
        cancel: &CancellationToken,
    ) -> Result<(serde_json::Value, Option<String>), String> {
        let finished = {
            let state = self
                .state
                .lock()
                .map_err(|error| format!("Job state unavailable: {error}"))?;
            state.live_job(id).map(|job| job.finished.clone())
        };
        if let Some(finished) = finished
            && !duration.is_zero()
        {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err("Operation aborted".to_string()),
                () = finished.cancelled() => {},
                () = tokio::time::sleep(duration) => {},
            }
        }
        self.inspect(id, true)
    }

    /// Requests cleanup before awaiting it. Dropping the caller cannot undo the stop.
    pub(crate) async fn stop(&self, id: &str) -> Result<(), String> {
        let finished = {
            let state = self
                .state
                .lock()
                .map_err(|error| format!("Job state unavailable: {error}"))?;
            if let Some(job) = state.live_job(id) {
                job.stop.cancel();
                job.finished.clone()
            } else if state.completed_job(id).is_some() {
                return Ok(());
            } else {
                return Err(unknown_job());
            }
        };
        finished.cancelled().await;
        Ok(())
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
        self.completed.iter().find(|job| job.record.id == id)
    }

    #[cfg(unix)]
    fn complete(&mut self, id: &str, phase: Phase, exit: Exit) {
        let Some(index) = self.live.iter().position(|job| job.record.id == id) else {
            return;
        };
        let job = self.live.remove(index);
        if self.closed {
            return;
        }
        let completed = Completed {
            record: job.record,
            phase,
            exit,
            evicted_job_id: None,
        };
        match job.visibility {
            Visibility::Published => self.retain(completed),
            Visibility::Pending { completion, .. } => {
                let _ = completion.send(completed);
            }
            Visibility::Discarded => {}
        }
    }

    #[cfg(unix)]
    fn retain(&mut self, mut completed: Completed) {
        let evicted = if self.completed.len() == MAX_COMPLETED {
            self.completed.pop_front().map(|job| job.record.id)
        } else {
            None
        };
        completed.evicted_job_id = evicted;
        self.completed.push_back(completed);
    }
}

fn unknown_job() -> String {
    "Job is unknown or expired in this session; use jobs with action=list".to_string()
}

#[cfg(test)]
#[cfg(unix)]
#[path = "jobs/tests.rs"]
mod tests;
