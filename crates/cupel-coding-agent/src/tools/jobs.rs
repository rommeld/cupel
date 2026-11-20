//! The model-facing interface for a session's managed shell commands.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};

use crate::jobs::Jobs;

pub(crate) struct JobsTool(Arc<Jobs>);

impl JobsTool {
    pub(crate) fn new(jobs: Arc<Jobs>) -> Self {
        Self(jobs)
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Args {
    List,
    Status { id: String },
    Output { id: String },
    Stop { id: String },
}

#[async_trait::async_trait]
impl AgentTool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "List, inspect, or stop this session's managed bash jobs. Output is a combined stdout/stderr \
         tail (64 KiB), with truncation and incomplete capture reported separately. Completed jobs \
         can be evicted; handles do not survive session reset, reload, or restart."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "status", "output", "stop"]},
                "id": {"type": "string", "description": "Job ID from bash or jobs list; required except for list"}
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let args: Args = serde_json::from_value(args)?;
        let (details, output) = match args {
            Args::List => (self.0.list()?, None),
            Args::Status { id } => self.0.inspect(&id, false)?,
            Args::Output { id } => self.0.inspect(&id, true)?,
            Args::Stop { id } => {
                self.0.stop(&id).await?;
                return Ok(AgentToolResult::text(format!(
                    "Job {id} is no longer running. Use jobs status or output to inspect the retained result."
                )));
            }
        };
        let text = match output.as_deref() {
            Some("") => format!("{details}\n\n(no output captured)"),
            Some(output) => format!("{details}\n\n{output}"),
            None => details.to_string(),
        };
        let mut result = AgentToolResult::text(text);
        result.details = Some(details);
        Ok(result)
    }
}
