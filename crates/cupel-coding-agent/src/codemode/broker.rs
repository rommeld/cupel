use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::agent_loop::ToolPreflight;
use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};

use crate::codemode::files::FileWorker;
use crate::codemode::{Callback, Coordinator, Failure, Request};
use crate::tools::grep::{GrepArgs, GrepTool};
use crate::tools::read::{ReadArgs, ReadTool};

pub(crate) const NAME: &str = "codemode";

pub(crate) struct CodemodeTool {
    broker: Arc<Broker>,
    coordinator: Coordinator,
}

impl CodemodeTool {
    pub(crate) fn new(
        coordinator: Coordinator,
        cwd: PathBuf,
        read: Arc<ReadTool>,
        grep: Arc<GrepTool>,
    ) -> Self {
        Self {
            broker: Arc::new(Broker {
                cwd,
                binary: coordinator.binary().to_path_buf(),
                read,
                grep,
            }),
            coordinator,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    code: String,
}

#[async_trait::async_trait]
impl AgentTool for CodemodeTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &'static str {
        "Run a bounded Monty Python script for repository exploration. Only read, grep, and emit \
         are available. Use keyword arguments matching the direct tools. Use emit to return \
         JSON-compatible values; print and unselected data are not returned. For example:\n\
         files = grep(pattern='Widget', outputMode='files')['files']\n\
         page = read(path=files[0]['path'])\n\
         emit({'path': page['path'], 'lines': [s for s in page['text'].splitlines() if 'Widget' in s], \
         'observation': page['observation']})"
    }

    fn parameters(&self) -> Value {
        json!({"type":"object", "properties":{"code":{"type":"string"}}, "required":["code"], "additionalProperties":false})
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        Err("codemode requires session preflight".into())
    }

    async fn execute_with_preflight(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
        preflight: &ToolPreflight,
    ) -> Result<AgentToolResult, ToolError> {
        let Arguments { code } =
            serde_json::from_value(args).map_err(|_error| Failure::Arguments)?;
        let preflight = preflight.clone();
        let broker = Arc::clone(&self.broker);
        let id = tool_call_id.to_owned();
        let mut sequence = 0;
        let result = self
            .coordinator
            .run(code, &cancel, move |request, cancel| {
                sequence += 1;
                let id = format!("{id}:callback:{sequence}");
                let preflight = preflight.clone();
                let broker = Arc::clone(&broker);
                let on_update = on_update.as_ref().map(Arc::clone);
                async move {
                    let name = match request.callback {
                        Callback::Read => "read",
                        Callback::Grep => "grep",
                    };
                    let start = Instant::now();
                    let result = broker.dispatch(request, &preflight, id, cancel).await;
                    if let Some(update) = on_update {
                        let outcome = match &result {
                            Ok(_) => "ok",
                            Err(Failure::Cancelled) => "cancelled",
                            Err(Failure::Veto) => "blocked",
                            Err(_) => "failed",
                        };
                        update(AgentToolResult::text(format!(
                            "{name} {outcome} ({} ms)",
                            start.elapsed().as_millis()
                        )));
                    }
                    result
                }
            })
            .await?;
        Ok(AgentToolResult::text(serde_json::to_string(&result)?))
    }
}

struct Broker {
    binary: PathBuf,
    cwd: PathBuf,
    read: Arc<ReadTool>,
    grep: Arc<GrepTool>,
}

enum Selected<'a> {
    Read(ReadArgs),
    Grep {
        cwd: &'a std::path::Path,
        root: &'a std::path::Path,
        args: GrepArgs,
    },
}

impl Broker {
    async fn dispatch(
        &self,
        request: Request,
        preflight: &ToolPreflight,
        id: String,
        cancel: CancellationToken,
    ) -> Result<Value, Failure> {
        let tool: &dyn AgentTool = match request.callback {
            Callback::Read => self.read.as_ref(),
            Callback::Grep => self.grep.as_ref(),
        };
        let arguments = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Failure::Cancelled),
            result = preflight.check_selected(tool, id, request.arguments, &cancel) => result.map_err(|_reason| Failure::Veto)?,
        };
        let selected = match request.callback {
            Callback::Read => Selected::Read(self.read.callback_args(arguments)?),
            Callback::Grep => {
                let (cwd, root) = self.grep.worker_paths().ok_or(Failure::Callback)?;
                let args =
                    serde_json::from_value(arguments).map_err(|_error| Failure::Arguments)?;
                Selected::Grep { cwd, root, args }
            }
        };
        if cancel.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        let mut worker = FileWorker::spawn(&self.binary, &self.cwd)?;
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Failure::Cancelled),
            result = self.run_selected(selected, &cancel, &mut worker) => result,
        };
        worker.stop(request.return_by).await?;
        result
    }

    async fn run_selected(
        &self,
        selected: Selected<'_>,
        cancel: &CancellationToken,
        worker: &mut FileWorker,
    ) -> Result<Value, Failure> {
        match selected {
            Selected::Read(args) => {
                self.read
                    .callback(&args, cancel, worker)
                    .await
                    .map_err(|error| {
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::FileTooLarge)
                        {
                            Failure::CallbackBytes
                        } else {
                            Failure::Callback
                        }
                    })
            }
            Selected::Grep { cwd, root, args } => worker.grep(cwd, root, args).await,
        }
    }
}
