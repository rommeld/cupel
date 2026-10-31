//! Short protocol handlers admit work, owned tasks finish it and settle on shutdown.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1 as acp;
use cupel_core::provider::Registry;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::session::{Notify, Runtime, directory, failure, invalid, prompt_text};

enum Slot {
    Loading(PathBuf),
    Idle(Box<Runtime>),
    Running {
        cwd: PathBuf,
        cancel: CancellationToken,
    },
}

impl Slot {
    fn cwd(&self) -> &std::path::Path {
        match self {
            Self::Loading(cwd) | Self::Running { cwd, .. } => cwd,
            Self::Idle(runtime) => &runtime.cwd,
        }
    }
}

#[derive(Default)]
struct State {
    initialized: bool,
    sessions: BTreeMap<String, Slot>,
}

pub struct Server {
    config: Config,
    registry: Arc<Registry>,
    state: Mutex<State>,
    tasks: Mutex<JoinSet<()>>,
    closing: CancellationToken,
}

pub struct Turn {
    id: String,
    text: String,
    cancel: CancellationToken,
    runtime: Box<Runtime>,
}

impl Server {
    #[must_use]
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            registry: Arc::new(cupel_core::default_registry()),
            state: Mutex::new(State::default()),
            tasks: Mutex::new(JoinSet::new()),
            closing: CancellationToken::new(),
        })
    }

    pub fn initialized(&self) -> acp::Result<acp::InitializeResponse> {
        let mut state = self.state.lock().expect("server state lock poisoned");
        if state.initialized {
            return Err(invalid("connection already initialized"));
        }
        state.initialized = true;
        Ok(
            acp::InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                .agent_capabilities(
                    acp::AgentCapabilities::new().load_session(self.config.home.is_some()),
                )
                .agent_info(acp::Implementation::new(
                    "cupel-acp",
                    env!("CARGO_PKG_VERSION"),
                )),
        )
    }

    pub fn spawn(&self, work: impl Future<Output = acp::Result<()>> + Send + 'static) {
        let mut tasks = self.tasks.lock().expect("task lock poisoned");
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                tracing::error!("ACP task failed: {error}");
            }
        }
        tasks.spawn(async move {
            if let Err(error) = work.await {
                tracing::warn!("ACP response delivery failed: {error}");
            }
        });
    }

    pub fn fresh_id() -> acp::Result<String> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|error| failure(format!("session id randomness failed: {error}")))?;
        Ok(bytes.iter().fold(String::from("acp-"), |mut id, byte| {
            let _ = write!(id, "{byte:02x}");
            id
        }))
    }

    pub async fn create(
        &self,
        id: &str,
        cwd: PathBuf,
        mcp_server: &[acp::McpServer],
        load: bool,
        notify: Notify,
    ) -> acp::Result<()> {
        if !mcp_server.is_empty() {
            return Err(invalid(
                "MCP server injection is not supported by cupel-acp",
            ));
        }
        let cwd = directory(&cwd)?;
        {
            let mut state = self.state.lock().expect("server state lock poisoned");
            if !state.initialized {
                return Err(invalid("initialize the connection first"));
            }
            if self.closing.is_cancelled() {
                return Err(failure("server is shutting down"));
            }
            if state.sessions.len() >= 32 {
                return Err(invalid("session limit reached; start a new connection"));
            }
            if state.sessions.contains_key(id) {
                return Err(invalid("session is already open on this connection"));
            }
            state
                .sessions
                .insert(id.to_owned(), Slot::Loading(cwd.clone()));
        }
        let built = Runtime::build(
            &self.config,
            Arc::clone(&self.registry),
            cwd,
            id,
            load,
            &notify,
        )
        .await;
        let mut state = self.state.lock().expect("server state lock poisoned");
        match built {
            Ok(runtime) => {
                state
                    .sessions
                    .insert(id.to_owned(), Slot::Idle(Box::new(runtime)));
                Ok(())
            }
            Err(error) => {
                state.sessions.remove(id);
                Err(error)
            }
        }
    }

    pub fn begin_prompt(&self, request: acp::PromptRequest) -> acp::Result<Turn> {
        let text = prompt_text(&request.prompt)?;
        let id = request.session_id.to_string();
        let cancel = self.closing.child_token();
        let runtime = {
            let mut state = self.state.lock().expect("server state lock poisoned");
            let slot = state
                .sessions
                .get(&id)
                .ok_or_else(|| invalid("unknown session"))?;
            if !matches!(slot, Slot::Idle(_)) {
                return Err(invalid("session is busy"));
            }
            let cwd = slot.cwd().to_path_buf();
            if state
                .sessions
                .values()
                .any(|other| matches!(other, Slot::Running { .. }) && other.cwd() == cwd)
            {
                return Err(invalid(
                    "another session is already working in this directory",
                ));
            }
            let previous = state.sessions.insert(
                id.clone(),
                Slot::Running {
                    cwd,
                    cancel: cancel.clone(),
                },
            );
            match previous {
                Some(Slot::Idle(runtime)) => runtime,
                _ => return Err(failure("session state changed during prompt admission")),
            }
        };
        Ok(Turn {
            id,
            text,
            cancel,
            runtime,
        })
    }

    pub async fn finish_prompt(
        &self,
        mut turn: Turn,
        notify: Notify,
    ) -> acp::Result<acp::PromptResponse> {
        let result = turn.runtime.prompt(&turn.text, &turn.cancel, &notify).await;
        self.state
            .lock()
            .expect("server state lock poisoned")
            .sessions
            .insert(turn.id, Slot::Idle(turn.runtime));
        result
    }

    pub fn cancel(&self, id: &acp::SessionId) {
        let state = self.state.lock().expect("server state lock poisoned");
        if let Some(Slot::Running { cancel, .. }) = state.sessions.get(id.0.as_ref()) {
            cancel.cancel();
        }
    }

    pub async fn shutdown(&self) -> acp::Result<()> {
        self.closing.cancel();
        let mut tasks = std::mem::take(&mut *self.tasks.lock().expect("task lock poisoned"));
        let mut task_error = None;
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                task_error = Some(failure(format!("ACP task failed: {error}")));
            }
        }
        let sessions = std::mem::take(
            &mut self
                .state
                .lock()
                .expect("server state lock poisoned")
                .sessions,
        );
        for slot in sessions.into_values() {
            if let Slot::Idle(mut runtime) = slot {
                runtime.recorder.end_session().await;
            }
        }
        task_error.map_or(Ok(()), Err)
    }
}
