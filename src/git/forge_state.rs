use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::git::types::{CheckRun, PullRequest, WorkflowRun};

// ---------------------------------------------------------------------------
// ForgeState — mutable, entity-owned GitHub state
// ---------------------------------------------------------------------------

/// Cached GitHub data owned by the `Repository` entity.
///
/// Updated by a background polling task. Each update generates a
/// `ForgeStateSnapshot` for cheap UI reads.
pub struct ForgeState {
    /// Embedded snapshot (cheap to clone via Arc-wrapped Vecs).
    snapshot: ForgeStateSnapshot,
    /// Polling interval (may increase due to rate-limit backoff).
    pub poll_interval: Duration,
    /// Base polling interval (from settings, before backoff).
    pub base_poll_interval: Duration,
    /// When the last successful poll completed.
    pub last_polled: Option<Instant>,
    /// Current backoff multiplier (resets to 1 after successful poll).
    backoff_multiplier: u32,
}

impl ForgeState {
    pub fn new(poll_interval_secs: u32) -> Self {
        let interval = Duration::from_secs(poll_interval_secs.max(15) as u64);
        Self {
            snapshot: ForgeStateSnapshot {
                is_loading: true,
                ..Default::default()
            },
            poll_interval: interval,
            base_poll_interval: interval,
            last_polled: None,
            backoff_multiplier: 1,
        }
    }

    /// Record a successful poll and reset backoff.
    pub fn record_success(&mut self) {
        self.last_polled = Some(Instant::now());
        self.snapshot.is_loading = false;
        self.snapshot.last_error = None;
        self.backoff_multiplier = 1;
        self.poll_interval = self.base_poll_interval;
    }

    /// Record a rate-limit error and apply exponential backoff.
    pub fn record_rate_limit(&mut self) {
        self.backoff_multiplier = (self.backoff_multiplier * 2).min(5);
        self.poll_interval = self
            .base_poll_interval
            .mul_f32(self.backoff_multiplier as f32)
            .min(Duration::from_secs(300));
        self.snapshot.last_error = Some("Rate limited — backing off".to_string());
    }

    /// Record a general error.
    pub fn record_error(&mut self, error: String) {
        self.snapshot.is_loading = false;
        self.snapshot.last_error = Some(error);
    }

    /// Whether enough time has elapsed since the last poll.
    pub fn should_poll(&self) -> bool {
        match self.last_polled {
            None => true,
            Some(last) => last.elapsed() >= self.poll_interval,
        }
    }

    /// Generate a cheap snapshot for rendering (O(1) Arc clones).
    pub fn snapshot(&self) -> ForgeStateSnapshot {
        self.snapshot.clone()
    }
}

// ---------------------------------------------------------------------------
// ForgeStateSnapshot — cheap clone for UI reads
// ---------------------------------------------------------------------------

/// Immutable snapshot of forge state for rendering. No async calls in render path.
/// Vec fields are wrapped in `Arc` for cheap cloning.
#[derive(Clone, Debug, Default)]
pub struct ForgeStateSnapshot {
    pub current_pr: Option<PullRequest>,
    pub open_prs: Arc<Vec<PullRequest>>,
    pub pr_checks: Arc<Vec<CheckRun>>,
    pub recent_runs: Arc<Vec<WorkflowRun>>,
    pub is_loading: bool,
    pub last_error: Option<String>,
}

impl ForgeStateSnapshot {
    /// Aggregate check conclusions into a single status for badge rendering.
    pub fn aggregate_checks_status(
        &self,
    ) -> Option<crate::git::types::ChecksStatus> {
        use crate::git::types::{CheckConclusion, CheckStatus, ChecksStatus};

        if self.pr_checks.is_empty() {
            return None;
        }

        let mut any_pending = false;
        let mut any_failure = false;

        for check in self.pr_checks.iter() {
            match check.status {
                Some(CheckStatus::Queued) | Some(CheckStatus::InProgress) => {
                    any_pending = true;
                }
                Some(CheckStatus::Completed) => match check.conclusion {
                    Some(CheckConclusion::Failure)
                    | Some(CheckConclusion::TimedOut)
                    | Some(CheckConclusion::ActionRequired) => {
                        any_failure = true;
                    }
                    _ => {}
                },
                None => {}
            }
        }

        Some(if any_failure {
            ChecksStatus::Failure
        } else if any_pending {
            ChecksStatus::Pending
        } else {
            ChecksStatus::Success
        })
    }
}
