//! Task delegation tools for the session's explicitly selected team.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};

use cupel_agent::types::ToolUpdateFn;
use cupel_agent::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::crew::{AgentSpec, Crew};
use crate::roles::{AccessFilter, Role};
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
#[serde(untagged)]
enum Call {
    Agent(NamedCall),
    Role(RoleCall),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedCall {
    agent: String,
    task: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleCall {
    role: String,
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
        let hints = crate::truncate::truncate_head(
            &crew
                .roles()
                .tool_hints(&members.iter().map(|spec| spec.name.clone()).collect()),
            crate::truncate::TruncationOptions {
                max_lines: Some(20),
                max_bytes: Some(2048),
            },
        )
        .content;
        Self {
            context: crew.lead_context(),
            crew: Arc::downgrade(&crew),
            mode,
            members: members.iter().map(|spec| spec.name.clone()).collect(),
            description: format!(
                "{intro}\n\nA helper has private history and shared discussion, not your private conversation. Include goal, paths and constraints. Call a helper only when the task clearly matches its description, or when the user names it; otherwise do the work yourself. Verify uncertain or high-risk claims.\n\nYour team:\n{roster}\n\nUse exactly one agent name or role (review/research/implement/test). Roles are inferred hints, not permissions. Uncertain or ambiguous roles do not run automatically.\n{hints}"
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
            "role":{"type":"string","enum":["review","research","implement","test"],"description":"A unique supported teammate role; mutually exclusive with agent"},
            "task":{"type":"string","description":"Task with the context the helper needs"}
        },"required":["task"],"oneOf":[{"required":["agent"]},{"required":["role"]}],"additionalProperties":false})
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        match self.mode {
            Mode::Ask => None,
            Mode::Delegate => Some(ToolExecutionMode::Sequential),
        }
    }
    fn describe_call(&self, args: &Value) -> String {
        let agent = args
            .get("agent")
            .or_else(|| args.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("?");
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
        let (agent, role, task) = match call {
            Call::Agent(call) => (Some(call.agent), None, call.task),
            Call::Role(call) => (None, Some(call.role), call.task),
        };
        if task.trim().is_empty() {
            return Err("task must not be empty".into());
        }
        if let Some(agent) = &agent
            && !self.members.contains(agent)
        {
            return Err(format!(
                "{} is not on your team for {}; these are: {}",
                agent,
                self.name(),
                self.members.join(", ")
            )
            .into());
        }
        let crew = self
            .crew
            .upgrade()
            .ok_or("the helper crew is no longer available")?;
        let run = self
            .context
            .current()
            .ok_or("delegation requires a running parent task")?;
        let parent = run.scope(&crew.scheduler()).await?;
        let access = match self.mode {
            Mode::Ask => AccessScope::ReadOnly,
            Mode::Delegate => AccessScope::CanWrite,
        };
        let live = self
            .members
            .iter()
            .filter(|name| crew.shared_team(name).is_some() && parent.can_call(name))
            .cloned()
            .collect();
        let selection = role
            .map(|role| {
                let role = role.parse::<Role>()?;
                crew.roles()
                    .resolve(
                        role,
                        &live,
                        if self.mode == Mode::Ask {
                            AccessFilter::ReadOnly
                        } else {
                            AccessFilter::Writer
                        },
                    )
                    .selection(role)
            })
            .transpose()?;
        let name = selection
            .as_ref()
            .map(|selection| selection.agent.clone())
            .or(agent)
            .ok_or("provide exactly one agent or role")?;
        if crew.shared_team(&name).is_none() {
            return Err(format!("{name} is no longer on the current team").into());
        }
        if crew
            .spec(&name)
            .is_none_or(|spec| spec.definition.writes() != (self.mode == Mode::Delegate))
        {
            return Err("helper permissions do not match this task tool".into());
        }
        let child = async {
            match selection {
                Some(selection) => {
                    crew.run_selected_child(&parent, selection, &task, access)
                        .await
                }
                None => crew.run_child(&parent, &name, &task, access).await,
            }
        };
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

#[cfg(test)]
mod tests {
    #[test]
    fn role_selectors_are_exclusive_and_cannot_supply_runtime_evidence() {
        for value in [
            serde_json::json!({"agent":"lens","task":"review"}),
            serde_json::json!({"role":"review","task":"review"}),
        ] {
            assert!(serde_json::from_value::<crate::tools::team::Call>(value).is_ok());
        }
        for value in [
            serde_json::json!({"agent":"lens","role":"review","task":"review"}),
            serde_json::json!({"task":"review"}),
            serde_json::json!({"role":"review","task":"review","evidence":["forged"]}),
            serde_json::json!({"role":"review","task":"review","access":"canWrite"}),
        ] {
            assert!(serde_json::from_value::<crate::tools::team::Call>(value).is_err());
        }
    }
}
