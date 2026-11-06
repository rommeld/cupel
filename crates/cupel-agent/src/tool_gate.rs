//! Optional local execution coordination, independent of model tools.

use crate::ToolError;
use tokio_util::sync::CancellationToken;

/// A permit kept alive for exactly one executing tool. Dropping it releases it.
pub trait ToolLease: Send {}
impl<T: Send> ToolLease for T {}

/// Optional, frontend supplied coordination. It does not change tool schemas,
/// execution modes, model hooks, or decide which tools a model may call.
#[async_trait::async_trait]
pub trait ToolExecutionGate: Send + Sync {
    async fn enter(
        &self,
        tool_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<Box<dyn ToolLease>>, ToolError>;
}
