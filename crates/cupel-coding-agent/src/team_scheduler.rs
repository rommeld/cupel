//! Root budgets and cycle-free, per-helper queues. No lock is held across inference.

use crate::crew::{Changes, Spend};
use crate::team_conversation::{Participant, TeamBody, TeamRuntime};
use crate::truncate::{TruncationOptions, truncate_head};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(pub u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessScope {
    ReadOnly,
    CanWrite,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskVisibility {
    Shared,
    Private,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    Queued,
    Running,
    Waiting,
    Cancelling,
    Succeeded,
    Failed(String),
    Cancelled,
    Blocked(String),
    Interrupted,
}

impl TaskState {
    #[must_use]
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed(_)
                | Self::Cancelled
                | Self::Blocked(_)
                | Self::Interrupted
        )
    }
    fn label(&self) -> &str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked(_) => "blocked",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub id: CallId,
    pub root: CallId,
    pub parent: Option<CallId>,
    pub actor: Participant,
    pub access: AccessScope,
    pub visibility: TaskVisibility,
    pub depth: u32,
    pub state: TaskState,
    pub task: String,
    pub summary: String,
    pub changes: Changes,
    pub usage: Spend,
    pub requests: u32,
    pub artifact: Option<std::path::PathBuf>,
    pub truncated: bool,
}

impl TaskSnapshot {
    #[must_use]
    pub fn display(&self) -> String {
        let reason = match &self.state {
            TaskState::Failed(reason) | TaskState::Blocked(reason) => reason.as_str(),
            TaskState::Interrupted => "previous process ended; changes and usage may be incomplete",
            _ => "",
        };
        format!(
            "call={} root={} parent={} {} depth={} {} {}\n{}\n{}\n{}{}",
            self.id.0,
            self.root.0,
            self.parent
                .map_or_else(|| "-".into(), |id| id.0.to_string()),
            self.actor,
            self.depth,
            self.state.label(),
            reason,
            self.changes,
            self.summary,
            self.artifact
                .as_ref()
                .map_or_else(String::new, |path| format!(
                    "full answer: {}\n",
                    path.display()
                )),
            if self.usage.unknown_usage {
                "[usage incomplete]"
            } else {
                ""
            }
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TeamLimits {
    pub depth: u32,
    pub helper_calls: u32,
    pub requests: u32,
    pub duration: Duration,
}
impl Default for TeamLimits {
    fn default() -> Self {
        Self {
            depth: 3,
            helper_calls: 16,
            requests: 64,
            duration: Duration::from_secs(600),
        }
    }
}

struct RootBudget {
    limits: TeamLimits,
    calls: AtomicU32,
    requests: AtomicU32,
    deadline: Instant,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct CallScope {
    id: CallId,
    root: CallId,
    parent: Option<CallId>,
    actor: Participant,
    access: AccessScope,
    visibility: TaskVisibility,
    depth: u32,
    ancestry: Vec<String>,
    cancel: CancellationToken,
    budget: Arc<RootBudget>,
    requests: Arc<AtomicU32>,
    ledger: Arc<Mutex<(Spend, u32)>>,
}

impl CallScope {
    #[must_use]
    pub fn id(&self) -> CallId {
        self.id
    }
    #[must_use]
    pub fn access(&self) -> AccessScope {
        self.access
    }
    #[must_use]
    pub fn visibility(&self) -> TaskVisibility {
        self.visibility
    }
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
    #[must_use]
    pub fn request_problem(&self) -> Option<String> {
        if Instant::now() >= self.budget.deadline {
            self.budget.cancel.cancel();
            return Some("root task deadline exceeded".into());
        }
        if self.cancel.is_cancelled() {
            return Some("task subtree was stopped".into());
        }
        if self
            .budget
            .requests
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.budget.limits.requests).then(|| used + 1)
            })
            .is_err()
        {
            return Some("root provider-request budget exhausted".into());
        }
        self.requests.fetch_add(1, Ordering::SeqCst);
        None
    }
    pub fn observe_usage(&self, usage: Option<&cupel_core::types::Usage>) {
        let mut ledger = self.ledger.lock().expect("task usage poisoned");
        ledger.1 += 1;
        if let Some(usage) = usage {
            ledger.0.add_usage(usage);
        } else {
            ledger.0.unknown_usage = true;
        }
    }
    #[must_use]
    pub fn usage(&self) -> Spend {
        let (mut spend, observations) = *self.ledger.lock().expect("task usage poisoned");
        let requests = self.requests.load(Ordering::SeqCst);
        spend.calls = u32::from(requests > 0);
        spend.unknown_usage |= observations < requests;
        spend
    }
    #[must_use]
    pub fn facts(&self) -> String {
        format!(
            "[Task {} root {}; depth {}/{}; {}; root calls {}/{}, requests {}/{}; shared cancellation and deadline apply]",
            self.id.0,
            self.root.0,
            self.depth,
            self.budget.limits.depth,
            if self.access == AccessScope::ReadOnly {
                "read-only subtree"
            } else {
                "can write"
            },
            self.budget.calls.load(Ordering::SeqCst),
            self.budget.limits.helper_calls,
            self.budget.requests.load(Ordering::SeqCst),
            self.budget.limits.requests
        )
    }
}

struct Node {
    snapshot: TaskSnapshot,
    scope: Option<CallScope>,
}
#[derive(Default)]
struct State {
    initialized: bool,
    next: u64,
    nodes: BTreeMap<CallId, Node>,
    queues: BTreeMap<String, VecDeque<CallId>>,
}
pub struct TeamScheduler {
    state: Mutex<State>,
    initializing: tokio::sync::Mutex<()>,
    journal: Mutex<Option<Arc<TeamRuntime>>>,
    changed: Notify,
    limits: TeamLimits,
    cwd: Mutex<Option<std::path::PathBuf>>,
}

impl TeamScheduler {
    #[must_use]
    pub fn new(limits: TeamLimits) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            initializing: tokio::sync::Mutex::new(()),
            journal: Mutex::new(None),
            changed: Notify::new(),
            limits,
            cwd: Mutex::new(None),
        })
    }
    pub fn attach(&self, journal: Arc<TeamRuntime>) {
        *self.journal.lock().expect("task journal poisoned") = Some(journal);
    }
    pub fn set_cwd(&self, cwd: std::path::PathBuf) {
        *self.cwd.lock().expect("scheduler cwd poisoned") = Some(cwd);
    }
    #[must_use]
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd.lock().expect("scheduler cwd poisoned").clone()
    }
    fn journal(&self) -> Option<Arc<TeamRuntime>> {
        self.journal.lock().expect("task journal poisoned").clone()
    }
    pub fn reserve_after(&self, through: CallId) {
        let mut state = self.state.lock().expect("scheduler poisoned");
        state.next = state.next.max(through.0.saturating_add(1));
    }

    pub async fn initialize(&self) -> Result<(), String> {
        let _initializing = self.initializing.lock().await;
        if self.state.lock().expect("scheduler poisoned").initialized {
            return Ok(());
        }
        let page = match self.journal() {
            Some(journal) => Some(journal.inspect(None).await?),
            None => None,
        };
        let mut state = self.state.lock().expect("scheduler poisoned");
        if let Some(page) = page {
            for entry in page.entries {
                if let TeamBody::Scheduled { task } = entry.body {
                    state.next = state
                        .next
                        .max(task.id.0.checked_add(1).ok_or("task IDs exhausted")?);
                    state.nodes.insert(
                        task.id,
                        Node {
                            snapshot: *task,
                            scope: None,
                        },
                    );
                }
            }
            for node in state.nodes.values_mut() {
                if !node.snapshot.state.terminal() {
                    node.snapshot.state = TaskState::Interrupted;
                    node.snapshot.changes = node.snapshot.changes.merge(&Changes::Unknown);
                    node.snapshot.usage.unknown_usage = true;
                }
            }
        }
        state.initialized = true;
        Ok(())
    }

    pub async fn root(
        self: &Arc<Self>,
        actor: Participant,
        access: AccessScope,
        task: &str,
        cancel: CancellationToken,
        visibility: TaskVisibility,
    ) -> Result<CallScope, String> {
        self.initialize().await?;
        if task.trim().is_empty() {
            return Err("task must not be empty".into());
        }
        let helper = match &actor {
            Participant::Helper(name) => Some(name.clone()),
            Participant::Lead => None,
            Participant::User => return Err("a user is not a model task".into()),
        };
        if helper.is_some() && (self.limits.depth == 0 || self.limits.helper_calls == 0) {
            return Err("root helper limit is zero".into());
        }
        let budget = Arc::new(RootBudget {
            limits: self.limits,
            calls: AtomicU32::new(u32::from(helper.is_some())),
            requests: AtomicU32::new(0),
            deadline: Instant::now() + self.limits.duration,
            cancel: cancel.clone(),
        });
        let scope = {
            let mut state = self.state.lock().expect("scheduler poisoned");
            let id = allocate(&mut state)?;
            let scope = CallScope {
                id,
                root: id,
                parent: None,
                actor,
                access,
                visibility,
                depth: u32::from(helper.is_some()),
                ancestry: helper.iter().cloned().collect(),
                cancel,
                budget: Arc::clone(&budget),
                requests: Arc::new(AtomicU32::new(0)),
                ledger: Arc::new(Mutex::new((Spend::default(), 0))),
            };
            insert(&mut state, &scope, task);
            scope
        };
        tokio::spawn(async move {
            tokio::select! { () = budget.cancel.cancelled() => {}, () = tokio::time::sleep_until(budget.deadline) => budget.cancel.cancel() }
        });
        self.publish(scope.id).await;
        self.changed.notify_waiters();
        Ok(scope)
    }

    fn validate_members(&self, parent: &CallScope, name: &str) -> Result<(), String> {
        if parent.visibility == TaskVisibility::Private {
            return Err("an off-team private call cannot delegate".into());
        }
        if let Some(journal) = self.journal() {
            if !journal.contains_member(name) {
                return Err(format!("{name} is no longer on the team"));
            }
            if let Participant::Helper(caller) = &parent.actor
                && !journal.contains_member(caller)
            {
                return Err(format!("caller {caller} is no longer on the team"));
            }
        }
        Ok(())
    }

    pub async fn child(
        self: &Arc<Self>,
        parent: &CallScope,
        name: &str,
        task: &str,
        access: AccessScope,
    ) -> Result<CallScope, String> {
        if task.trim().is_empty() {
            return Err("task must not be empty".into());
        }
        self.validate_members(parent, name)?;
        if parent.cancel.is_cancelled() {
            return Err("parent task was stopped".into());
        }
        if Instant::now() >= parent.budget.deadline {
            parent.budget.cancel.cancel();
            return Err("root task deadline exceeded".into());
        }
        if parent.depth >= parent.budget.limits.depth {
            return Err("root delegation depth exhausted".into());
        }
        if access == AccessScope::CanWrite && parent.access == AccessScope::ReadOnly {
            return Err("a read-only subtree cannot delegate to a writer".into());
        }
        if parent.ancestry.iter().any(|ancestor| ancestor == name) {
            return Err(format!("ancestor call refused: {name}"));
        }
        let (scope, cycle) = {
            let mut state = self.state.lock().expect("scheduler poisoned");
            if state.nodes.get(&parent.id).is_none_or(|node| {
                !matches!(node.snapshot.state, TaskState::Running | TaskState::Waiting)
            }) {
                return Err("parent task does not own an execution lease".into());
            }
            parent
                .budget
                .calls
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                    (used < parent.budget.limits.helper_calls).then(|| used + 1)
                })
                .map_err(|_used| "root helper-call budget exhausted")?;
            let id = allocate(&mut state)?;
            let mut ancestry = parent.ancestry.clone();
            ancestry.push(name.into());
            let scope = CallScope {
                id,
                root: parent.root,
                parent: Some(parent.id),
                actor: Participant::Helper(name.into()),
                access,
                visibility: parent.visibility,
                depth: parent.depth + 1,
                ancestry,
                cancel: parent.cancel.child_token(),
                budget: Arc::clone(&parent.budget),
                requests: Arc::new(AtomicU32::new(0)),
                ledger: Arc::new(Mutex::new((Spend::default(), 0))),
            };
            insert(&mut state, &scope, task);
            let cycle = cycle_from(&state, id);
            if cycle {
                if let Some(node) = state.nodes.get_mut(&id) {
                    node.snapshot.state = TaskState::Blocked("dependency cycle".into());
                    node.scope = None;
                }
                remove(&mut state, id);
            }
            refresh_waiting(&mut state);
            (scope, cycle)
        };
        self.publish(scope.id).await;
        self.changed.notify_waiters();
        if cycle {
            return Err("dependency cycle refused before waiting".into());
        }
        Ok(scope)
    }

    pub async fn acquire(&self, scope: &CallScope) -> Result<(), String> {
        loop {
            let ready = self.changed.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if scope.cancel.is_cancelled() {
                return Err("queued task was stopped".into());
            }
            let Participant::Helper(name) = &scope.actor else {
                return Ok(());
            };
            if let Some(parent) = scope.parent.and_then(|id| self.scope(id)) {
                self.validate_members(&parent, name)?;
            }
            let acquired = {
                let mut state = self.state.lock().expect("scheduler poisoned");
                let head = state
                    .queues
                    .get(name)
                    .and_then(|queue| queue.front())
                    .copied();
                let node = state
                    .nodes
                    .get_mut(&scope.id)
                    .ok_or("task disappeared from queue")?;
                if node.snapshot.state.terminal() {
                    return Err("task already ended".into());
                }
                if head == Some(scope.id) && node.snapshot.state == TaskState::Queued {
                    node.snapshot.state = TaskState::Running;
                    true
                } else {
                    false
                }
            };
            if acquired {
                self.publish(scope.id).await;
                return Ok(());
            }
            tokio::select! { biased; () = scope.cancel.cancelled() => return Err("queued task was stopped".into()), () = ready => {} }
        }
    }

    #[must_use]
    pub fn scope(&self, id: CallId) -> Option<CallScope> {
        self.state
            .lock()
            .expect("scheduler poisoned")
            .nodes
            .get(&id)
            .and_then(|node| node.scope.clone())
    }

    pub fn stop_helpers(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for node in self
            .state
            .lock()
            .expect("scheduler poisoned")
            .nodes
            .values()
        {
            if let Some(scope) = &node.scope
                && let Participant::Helper(name) = &scope.actor
                && !scope.cancel.is_cancelled()
            {
                scope.cancel.cancel();
                names.insert(name.clone());
            }
        }
        self.changed.notify_waiters();
        names.into_iter().collect()
    }

    pub async fn cancel(&self, id: CallId) -> Result<(), String> {
        let scope = self.scope(id).ok_or("task is not active")?;
        scope.cancel.cancel();
        if let Some(node) = self
            .state
            .lock()
            .expect("scheduler poisoned")
            .nodes
            .get_mut(&id)
        {
            node.snapshot.state = TaskState::Cancelling;
        }
        self.publish(id).await;
        self.changed.notify_waiters();
        Ok(())
    }

    pub async fn finish(
        &self,
        scope: &CallScope,
        result: TaskState,
        answer: &str,
        changes: Changes,
    ) {
        let children = {
            let state = self.state.lock().expect("scheduler poisoned");
            descendants(&state, scope.id)
        };
        for child in &children {
            if let Some(scope) = self.scope(*child) {
                scope.cancel.cancel();
            }
        }
        loop {
            let ready = self.changed.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if children.iter().all(|id| self.scope(*id).is_none()) {
                break;
            }
            ready.await;
        }
        {
            let mut state = self.state.lock().expect("scheduler poisoned");
            let changes = children
                .iter()
                .filter_map(|id| state.nodes.get(id))
                .fold(changes, |changes, child| {
                    changes.merge(&child.snapshot.changes)
                });
            if let Some(node) = state.nodes.get_mut(&scope.id) {
                if node.snapshot.state.terminal() {
                    return;
                }
                node.snapshot.state = result;
                node.snapshot.summary = excerpt(answer);
                node.snapshot.truncated = node.snapshot.summary != answer;
                node.snapshot.changes = changes;
                node.snapshot.usage = scope.usage();
                node.snapshot.requests = scope.requests.load(Ordering::SeqCst);
            }
        }
        self.publish_answer(scope.id, Some(answer)).await;
        {
            let mut state = self.state.lock().expect("scheduler poisoned");
            if let Some(node) = state.nodes.get_mut(&scope.id) {
                node.scope = None;
            }
            remove(&mut state, scope.id);
            refresh_waiting(&mut state);
        }
        if scope.parent.is_none() {
            scope.budget.cancel.cancel();
        }
        self.changed.notify_waiters();
    }

    pub async fn publish(&self, id: CallId) {
        self.publish_answer(id, None).await;
    }
    async fn publish_answer(&self, id: CallId, answer: Option<&str>) {
        let snapshot = self.snapshots().into_iter().find(|node| node.id == id);
        if let (Some(journal), Some(snapshot)) = (self.journal(), snapshot) {
            match journal.scheduled(snapshot, answer) {
                Ok(receipt) => match journal.receive(receipt).await {
                    Ok(entry) => {
                        if let TeamBody::Scheduled { task } = entry.body
                            && let Some(node) = self
                                .state
                                .lock()
                                .expect("scheduler poisoned")
                                .nodes
                                .get_mut(&id)
                        {
                            node.snapshot.artifact = task.artifact;
                        }
                    }
                    Err(problem) => journal.warn(problem),
                },
                Err(problem) => journal.warn(problem),
            }
        }
    }
    #[must_use]
    pub fn snapshots(&self) -> Vec<TaskSnapshot> {
        self.state
            .lock()
            .expect("scheduler poisoned")
            .nodes
            .values()
            .map(|node| {
                let mut snapshot = node.snapshot.clone();
                if let Some(scope) = &node.scope {
                    snapshot.usage = scope.usage();
                    snapshot.requests = scope.requests.load(Ordering::SeqCst);
                }
                snapshot
            })
            .collect()
    }
    #[must_use]
    pub fn describe(&self) -> String {
        self.describe_root(None)
    }
    #[must_use]
    pub fn describe_root(&self, root: Option<CallId>) -> String {
        let state = self.state.lock().expect("scheduler poisoned");
        let mut out = Vec::new();
        for node in state
            .nodes
            .values()
            .filter(|node| root.is_none_or(|root| node.snapshot.root == root))
        {
            let mut total = node
                .scope
                .as_ref()
                .map_or(node.snapshot.usage, CallScope::usage);
            for child in descendants(&state, node.snapshot.id)
                .iter()
                .filter_map(|id| state.nodes.get(id))
            {
                total.add(
                    &child
                        .scope
                        .as_ref()
                        .map_or(child.snapshot.usage, CallScope::usage),
                );
            }
            out.push(format!(
                "{}{}\nown ${:.6}, subtree ${:.6}{}",
                "  ".repeat(node.snapshot.depth as usize),
                node.snapshot.display(),
                node.scope
                    .as_ref()
                    .map_or(node.snapshot.usage, CallScope::usage)
                    .cost,
                total.cost,
                if total.unknown_usage {
                    " (usage incomplete)"
                } else {
                    ""
                }
            ));
        }
        out.join("\n")
    }
    pub async fn wait_for_idle(&self) {
        loop {
            let ready = self.changed.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if self
                .state
                .lock()
                .expect("scheduler poisoned")
                .nodes
                .values()
                .all(|node| {
                    node.scope.is_none() || !matches!(node.snapshot.actor, Participant::Helper(_))
                })
            {
                return;
            }
            ready.await;
        }
    }
}

fn excerpt(text: &str) -> String {
    truncate_head(
        text,
        TruncationOptions {
            max_lines: Some(30),
            max_bytes: Some(1024),
        },
    )
    .content
}
fn allocate(state: &mut State) -> Result<CallId, String> {
    let id = CallId(state.next);
    state.next = state.next.checked_add(1).ok_or("task IDs exhausted")?;
    Ok(id)
}
fn insert(state: &mut State, scope: &CallScope, task: &str) {
    state.nodes.insert(
        scope.id,
        Node {
            snapshot: TaskSnapshot {
                id: scope.id,
                root: scope.root,
                parent: scope.parent,
                actor: scope.actor.clone(),
                access: scope.access,
                visibility: scope.visibility,
                depth: scope.depth,
                state: if matches!(scope.actor, Participant::Helper(_)) {
                    TaskState::Queued
                } else {
                    TaskState::Running
                },
                task: excerpt(task),
                summary: String::new(),
                changes: Changes::ReadOnly,
                usage: Spend::default(),
                requests: 0,
                artifact: None,
                truncated: false,
            },
            scope: Some(scope.clone()),
        },
    );
    if let Participant::Helper(name) = &scope.actor {
        state
            .queues
            .entry(name.clone())
            .or_default()
            .push_back(scope.id);
    }
}
fn remove(state: &mut State, id: CallId) {
    for queue in state.queues.values_mut() {
        queue.retain(|item| *item != id);
    }
    state.queues.retain(|_, queue| !queue.is_empty());
}
fn descendants(state: &State, ancestor: CallId) -> Vec<CallId> {
    state
        .nodes
        .keys()
        .filter(|id| {
            let mut cursor = **id;
            let mut seen = BTreeSet::new();
            while seen.insert(cursor) {
                if let Some(parent) = state
                    .nodes
                    .get(&cursor)
                    .and_then(|node| node.snapshot.parent)
                {
                    if parent == ancestor {
                        return true;
                    }
                    cursor = parent;
                } else {
                    break;
                }
            }
            false
        })
        .copied()
        .collect()
}
fn refresh_waiting(state: &mut State) {
    let parents = state
        .nodes
        .values()
        .filter(|node| node.scope.is_some())
        .filter_map(|node| node.snapshot.parent)
        .collect::<BTreeSet<_>>();
    for node in state.nodes.values_mut() {
        if matches!(node.snapshot.state, TaskState::Running | TaskState::Waiting) {
            node.snapshot.state = if parents.contains(&node.snapshot.id) {
                TaskState::Waiting
            } else {
                TaskState::Running
            };
        }
    }
}
fn cycle_from(state: &State, start: CallId) -> bool {
    let mut edges: BTreeMap<CallId, Vec<CallId>> = BTreeMap::new();
    for node in state.nodes.values().filter(|node| node.scope.is_some()) {
        if let Some(parent) = node.snapshot.parent {
            edges.entry(parent).or_default().push(node.snapshot.id);
        }
    }
    for queue in state.queues.values() {
        for (previous, next) in queue.iter().zip(queue.iter().skip(1)) {
            edges.entry(*next).or_default().push(*previous);
        }
    }
    let mut visits = vec![start];
    let mut seen = BTreeSet::new();
    while let Some(id) = visits.pop() {
        for next in edges.get(&id).into_iter().flatten() {
            if *next == start {
                return true;
            }
            if seen.insert(*next) {
                visits.push(*next);
            }
        }
    }
    false
}

#[derive(Default)]
pub struct CallContext(Mutex<Option<Arc<RunContext>>>);
impl CallContext {
    pub fn set(&self, run: Arc<RunContext>) {
        *self.0.lock().expect("call context poisoned") = Some(run);
    }
    pub fn clear(&self) {
        self.0.lock().expect("call context poisoned").take();
    }
    #[must_use]
    pub fn current(&self) -> Option<Arc<RunContext>> {
        self.0.lock().expect("call context poisoned").clone()
    }
}
pub struct RunContext {
    seed: (Participant, AccessScope, String),
    cancel: CancellationToken,
    scope: tokio::sync::Mutex<Option<CallScope>>,
    before: tokio::sync::Mutex<Observation>,
}
enum Observation {
    Pending,
    Snapshot(Option<crate::crew::WorkingTree>),
}
impl RunContext {
    #[must_use]
    pub fn root(
        actor: Participant,
        access: AccessScope,
        task: String,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            seed: (actor, access, task),
            cancel,
            scope: tokio::sync::Mutex::new(None),
            before: tokio::sync::Mutex::new(Observation::Pending),
        })
    }
    #[must_use]
    pub fn ready(scope: CallScope) -> Arc<Self> {
        Arc::new(Self {
            seed: (scope.actor.clone(), scope.access, String::new()),
            cancel: scope.cancel.clone(),
            scope: tokio::sync::Mutex::new(Some(scope)),
            before: tokio::sync::Mutex::new(Observation::Pending),
        })
    }
    pub async fn scope(&self, scheduler: &Arc<TeamScheduler>) -> Result<CallScope, String> {
        let mut scope = self.scope.lock().await;
        if let Some(scope) = &*scope {
            return Ok(scope.clone());
        }
        let new = scheduler
            .root(
                self.seed.0.clone(),
                self.seed.1,
                &self.seed.2,
                self.cancel.clone(),
                TaskVisibility::Shared,
            )
            .await?;
        *scope = Some(new.clone());
        Ok(new)
    }
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }
    pub async fn observe_start(&self, scheduler: &TeamScheduler) {
        if self.seed.0 == Participant::Lead && self.seed.1 == AccessScope::CanWrite {
            let mut before = self.before.lock().await;
            if matches!(*before, Observation::Pending)
                && let Some(cwd) = scheduler.cwd()
            {
                *before = Observation::Snapshot(
                    tokio::select! {biased;()=self.cancel.cancelled()=>None,snapshot=crate::crew::snapshot(&cwd)=>snapshot},
                );
            }
        }
    }
    pub async fn observed_changes(&self, cwd: &std::path::Path) -> Changes {
        if self
            .scope
            .lock()
            .await
            .as_ref()
            .is_none_or(|scope| scope.usage().calls == 0)
        {
            return Changes::ReadOnly;
        }
        match std::mem::replace(&mut *self.before.lock().await, Observation::Pending) {
            Observation::Snapshot(before) => crate::crew::observe_changes(before, cwd).await,
            Observation::Pending => Changes::ReadOnly,
        }
    }
}
impl Drop for RunContext {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use crate::team_scheduler::*;

    async fn root(scheduler: &Arc<TeamScheduler>) -> CallScope {
        scheduler
            .root(
                Participant::Lead,
                AccessScope::CanWrite,
                "root",
                CancellationToken::new(),
                TaskVisibility::Shared,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn queues_reject_cross_branch_wait_cycles_before_blocking() {
        let scheduler = TeamScheduler::new(TeamLimits::default());
        let root = root(&scheduler).await;
        let a = scheduler
            .child(&root, "a", "a task", AccessScope::CanWrite)
            .await
            .unwrap();
        let b = scheduler
            .child(&root, "b", "b task", AccessScope::CanWrite)
            .await
            .unwrap();
        scheduler.acquire(&a).await.unwrap();
        scheduler.acquire(&b).await.unwrap();
        let queued = scheduler
            .child(&a, "b", "wait for b", AccessScope::ReadOnly)
            .await
            .unwrap();
        let error = scheduler
            .child(&b, "a", "wait for a", AccessScope::ReadOnly)
            .await
            .err()
            .unwrap();
        assert!(error.contains("dependency cycle"));
        scheduler
            .finish(
                &queued,
                TaskState::Cancelled,
                "cancelled",
                Changes::ReadOnly,
            )
            .await;
        scheduler
            .finish(&a, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
        scheduler
            .finish(&b, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
        scheduler
            .finish(&root, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
        scheduler.wait_for_idle().await;
    }

    #[tokio::test]
    async fn ancestry_access_depth_and_root_budgets_cannot_be_reset() {
        let scheduler = TeamScheduler::new(TeamLimits {
            depth: 2,
            helper_calls: 2,
            requests: 2,
            duration: Duration::from_secs(30),
        });
        let root = root(&scheduler).await;
        let a = scheduler
            .child(&root, "a", "read", AccessScope::ReadOnly)
            .await
            .unwrap();
        scheduler.acquire(&a).await.unwrap();
        assert!(
            scheduler
                .child(&a, "writer", "write", AccessScope::CanWrite)
                .await
                .is_err()
        );
        assert!(
            scheduler
                .child(&a, "a", "cycle", AccessScope::ReadOnly)
                .await
                .is_err()
        );
        let b = scheduler
            .child(&a, "b", "read", AccessScope::ReadOnly)
            .await
            .unwrap();
        scheduler.acquire(&b).await.unwrap();
        assert!(
            scheduler
                .child(&b, "c", "deep", AccessScope::ReadOnly)
                .await
                .is_err()
        );
        assert!(
            scheduler
                .child(&root, "d", "too many", AccessScope::ReadOnly)
                .await
                .is_err()
        );
        assert!(root.request_problem().is_none());
        assert!(b.request_problem().is_none());
        assert!(a.request_problem().unwrap().contains("request budget"));
        scheduler
            .finish(&b, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
        scheduler
            .finish(&a, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
        scheduler
            .finish(&root, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
    }

    #[tokio::test]
    async fn cancelling_a_queued_call_leaves_other_queue_owners_intact() {
        let scheduler = TeamScheduler::new(TeamLimits::default());
        let root = root(&scheduler).await;
        let first = scheduler
            .child(&root, "a", "one", AccessScope::ReadOnly)
            .await
            .unwrap();
        scheduler.acquire(&first).await.unwrap();
        let second = scheduler
            .child(&root, "a", "two", AccessScope::ReadOnly)
            .await
            .unwrap();
        scheduler.cancel(second.id).await.unwrap();
        assert!(scheduler.acquire(&second).await.is_err());
        assert!(!first.cancel.is_cancelled());
        scheduler
            .finish(
                &second,
                TaskState::Cancelled,
                "cancelled",
                Changes::ReadOnly,
            )
            .await;
        scheduler
            .finish(&first, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
        scheduler
            .finish(&root, TaskState::Succeeded, "done", Changes::ReadOnly)
            .await;
    }

    #[tokio::test]
    async fn deadline_cancels_the_entire_root_and_resumption_marks_interrupted_work() {
        let (journal, _) = TeamRuntime::new(None);
        journal.set_members(["a".into()].into_iter().collect());
        let scheduler = TeamScheduler::new(TeamLimits {
            duration: Duration::from_millis(15),
            ..TeamLimits::default()
        });
        scheduler.attach(Arc::clone(&journal));
        let root = root(&scheduler).await;
        let child = scheduler
            .child(&root, "a", "task", AccessScope::ReadOnly)
            .await
            .unwrap();
        scheduler.acquire(&child).await.unwrap();
        assert!(child.request_problem().is_none());
        child.observe_usage(Some(&cupel_core::types::Usage {
            input: 12,
            output: 3,
            ..cupel_core::types::Usage::default()
        }));
        scheduler.publish(child.id).await;
        let resumed = TeamScheduler::new(TeamLimits::default());
        resumed.attach(journal);
        resumed.initialize().await.unwrap();
        let saved = resumed
            .snapshots()
            .into_iter()
            .find(|node| node.id == child.id)
            .unwrap();
        assert_eq!(saved.state, TaskState::Interrupted);
        assert_eq!(saved.usage.input, 12);
        assert!(saved.usage.unknown_usage);
        tokio::time::timeout(Duration::from_secs(1), child.cancel.cancelled())
            .await
            .unwrap();
        assert!(root.cancel.is_cancelled());
        scheduler
            .finish(&child, TaskState::Cancelled, "deadline", Changes::ReadOnly)
            .await;
        scheduler
            .finish(&root, TaskState::Cancelled, "deadline", Changes::ReadOnly)
            .await;
    }
}
