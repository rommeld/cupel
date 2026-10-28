//! The write lease covers a mutating leaf tool, never a parent awaiting a child.

use cupel_agent::types::ToolUpdateFn;
use cupel_agent::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio_util::sync::CancellationToken;

pub type WriteGate = Arc<tokio::sync::Mutex<()>>;
#[derive(Default)]
pub struct CheckoutGates {
    gates: BTreeMap<PathBuf, Weak<tokio::sync::Mutex<()>>>,
}
impl CheckoutGates {
    pub fn gate(&mut self, cwd: &Path) -> WriteGate {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let key = cwd
            .ancestors()
            .find(|path| path.join(".git").exists())
            .unwrap_or(&cwd)
            .to_path_buf();
        self.gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = self.gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        self.gates.insert(key, Arc::downgrade(&gate));
        gate
    }
}
pub struct CheckoutWriteTool {
    inner: Arc<dyn AgentTool>,
    gate: WriteGate,
}
impl CheckoutWriteTool {
    #[must_use]
    pub fn wrap(tool: Arc<dyn AgentTool>, gate: &WriteGate) -> Arc<dyn AgentTool> {
        if matches!(tool.name(), "bash" | "apply_patch") {
            Arc::new(Self {
                inner: tool,
                gate: Arc::clone(gate),
            })
        } else {
            tool
        }
    }
}
#[async_trait::async_trait]
impl AgentTool for CheckoutWriteTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn label(&self) -> &str {
        self.inner.label()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters(&self) -> Value {
        self.inner.parameters()
    }
    fn describe_call(&self, args: &Value) -> String {
        self.inner.describe_call(args)
    }
    fn reproducible(&self) -> bool {
        self.inner.reproducible()
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Sequential)
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let _lease = tokio::select! { biased; ()=cancel.cancelled()=>return Err("checkout write cancelled before execution".into()),lease=self.gate.lock()=>lease };
        if cancel.is_cancelled() {
            return Err("checkout write cancelled before execution".into());
        }
        self.inner
            .execute(tool_call_id, args, cancel, on_update)
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::checkout_write::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Writer {
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl AgentTool for Writer {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "test writer"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
            _: Option<ToolUpdateFn>,
        ) -> Result<AgentToolResult, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("done"))
        }
    }
    #[tokio::test]
    async fn actual_writers_serialize_and_cancelled_waiters_never_execute() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let writer = CheckoutWriteTool::wrap(
            Arc::new(Writer {
                active,
                maximum: Arc::clone(&maximum),
                calls: Arc::clone(&calls),
            }),
            &gate,
        );
        let mut jobs = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let tool = Arc::clone(&writer);
            jobs.spawn(async move {
                tool.execute(
                    "write",
                    serde_json::json!({}),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap()
            });
        }
        while let Some(result) = jobs.join_next().await {
            result.unwrap();
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        let _lease = gate.lock().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            writer
                .execute("cancel", serde_json::json!({}), cancel, None)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 8);
    }
}
