//! Task delegation tools for the session's explicitly selected team.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};

use cupel_agent::types::ToolUpdateFn;
use cupel_agent::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::crew::{AgentSpec, Crew};
use crate::team_scheduler::{AccessScope, CallContext};

pub const TEAM_TOOL_NAMES: [&str; 2] = ["ask", "delegate"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ask,
    Delegate,
}

#[must_use]
pub fn team_tools(crew: &Arc<Crew>, team: &BTreeSet<String>) -> Vec<Arc<dyn AgentTool>> {
    scoped_team_tools(crew, team, crew.lead_context(), AccessScope::CanWrite, None)
}

#[must_use]
pub fn scoped_team_tools(
    crew: &Arc<Crew>,
    team: &BTreeSet<String>,
    context: Arc<CallContext>,
    access: AccessScope,
    caller: Option<&str>,
) -> Vec<Arc<dyn AgentTool>> {
    let (writers, readers): (Vec<_>, Vec<_>) = team
        .iter()
        .filter_map(|name| crew.spec(name))
        .filter(|spec| spec.definition.description().is_some())
        .filter(|spec| {
            caller != Some(spec.name.as_str())
                && (access == AccessScope::CanWrite || !spec.definition.writes())
        })
        .partition(|spec| spec.definition.writes());
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    if !readers.is_empty() {
        tools.push(Arc::new(
            TeamTool::new(Arc::clone(crew), Mode::Ask, &readers).with_context(Arc::clone(&context)),
        ));
    }
    if !writers.is_empty() {
        tools.push(Arc::new(
            TeamTool::new(Arc::clone(crew), Mode::Delegate, &writers).with_context(context),
        ));
    }
    tools
}

pub struct TeamTool {
    crew: Weak<Crew>,
    context: Arc<CallContext>,
    mode: Mode,
    members: Vec<String>,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    agent: String,
    task: String,
}

impl TeamTool {
    #[must_use]
    pub fn new(crew: Arc<Crew>, mode: Mode, members: &[&AgentSpec]) -> Self {
        let intro = match mode {
            Mode::Ask => {
                "Give a task to a read-only helper agent on your team and get its answer back. Several asks in one turn run at the same time."
            }
            Mode::Delegate => {
                "Hand a task to a helper agent on your team that can change files. Delegated tasks run one at a time. Read the reported changed files again before editing them."
            }
        };
        let roster = members
            .iter()
            .map(|spec| {
                let access = if spec.definition.writes() {
                    "can write"
                } else {
                    "read-only"
                };
                format!(
                    "- {}: {} [{access} · {}/{} · thinking {}]",
                    spec.name,
                    spec.definition.description().unwrap_or_default(),
                    spec.model.provider.as_str(),
                    spec.model.id,
                    spec.definition.preset.thinking_level.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            context: crew.lead_context(),
            crew: Arc::downgrade(&crew),
            mode,
            members: members.iter().map(|spec| spec.name.clone()).collect(),
            description: format!(
                "{intro}\n\nA helper has its own context, not this conversation. Include the goal, file paths and constraints in the task. Call a helper only when the task clearly matches its description, or when the user names it; otherwise do the work yourself. Verify uncertain or high-risk claims.\n\nYour team:\n{roster}"
            ),
        }
    }

    #[must_use]
    pub fn with_context(mut self, context: Arc<CallContext>) -> Self {
        self.context = context;
        self
    }
}

#[async_trait::async_trait]
impl AgentTool for TeamTool {
    fn name(&self) -> &str {
        match self.mode {
            Mode::Ask => "ask",
            Mode::Delegate => "delegate",
        }
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object","properties":{
            "agent":{"type":"string","enum":self.members,"description":"Who gets the task"},
            "task":{"type":"string","description":"Task with the context the helper needs"}
        },"required":["agent","task"],"additionalProperties":false})
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        match self.mode {
            Mode::Ask => None,
            Mode::Delegate => Some(ToolExecutionMode::Sequential),
        }
    }
    fn describe_call(&self, args: &Value) -> String {
        let agent = args.get("agent").and_then(Value::as_str).unwrap_or("?");
        let task = args.get("task").and_then(Value::as_str).unwrap_or_default();
        format!(
            "{} {agent}: {}",
            self.name(),
            task.lines().next().unwrap_or_default()
        )
    }
    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let call: Call = serde_json::from_value(args)?;
        if call.task.trim().is_empty() {
            return Err("task must not be empty".into());
        }
        if !self.members.contains(&call.agent) {
            return Err(format!(
                "{} is not on your team for {}; these are: {}",
                call.agent,
                self.name(),
                self.members.join(", ")
            )
            .into());
        }
        let crew = self
            .crew
            .upgrade()
            .ok_or("the helper crew is no longer available")?;
        if crew.shared_team(&call.agent).is_none() {
            return Err(format!("{} is no longer on the current team", call.agent).into());
        }
        let run = self
            .context
            .current()
            .ok_or("delegation requires a running parent task")?;
        let parent = run.scope(&crew.scheduler()).await?;
        let access = match self.mode {
            Mode::Ask => AccessScope::ReadOnly,
            Mode::Delegate => AccessScope::CanWrite,
        };
        let child = crew.run_child(&parent, &call.agent, &call.task, access);
        tokio::pin!(child);
        let reply = tokio::select! { biased; ()=cancel.cancelled()=>return Err("delegation caller stopped; child cleanup continues".into()),result=&mut child=>result? };
        if let Some(update) = on_update {
            update(AgentToolResult::text(format!(
                "{} tool calls",
                reply.tool_calls
            )));
        }
        Ok(AgentToolResult::text(reply.for_lead()))
    }
}
