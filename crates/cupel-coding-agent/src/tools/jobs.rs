//! The model-facing interface for a session's managed shell commands.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};

use crate::jobs::Jobs;

const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 60_000;

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
    Wait { id: String, wait_ms: Option<u64> },
    Stop { id: String },
}

#[async_trait::async_trait]
impl AgentTool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }

    fn description(&self) -> &str {
        "List, inspect, wait for, or stop this session's managed bash jobs. Wait returns the status \
         and output on completion or expiry. Expiring or canceling a wait does not stop the job. \
         Output is a combined stdout/stderr tail (64 KiB), with truncation and incomplete capture \
         reported separately. Completed jobs can be evicted; handles do not survive session reset, \
         reload, or restart."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "status", "output", "wait", "stop"]},
                "id": {"type": "string", "description": "Job ID from bash or jobs list; required except for list"},
                "wait_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_WAIT_MS,
                    "default": DEFAULT_WAIT_MS,
                    "description": "For wait only: milliseconds to wait for completion (0 returns immediately)"
                }
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
            Args::Wait { id, wait_ms } => {
                let wait_ms = wait_ms.unwrap_or(DEFAULT_WAIT_MS);
                if wait_ms > MAX_WAIT_MS {
                    return Err(format!("wait_ms cannot exceed {MAX_WAIT_MS} milliseconds").into());
                }
                self.0
                    .wait(&id, Duration::from_millis(wait_ms), &cancel)
                    .await?
            }
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
