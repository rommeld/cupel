//! Shared checkout admission for raw single-agent leaf tools.

use std::sync::{Arc, Weak};

use cupel_agent::{ToolError, ToolExecutionGate, ToolLease};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use crate::tools::checkout_write::WriteGate;

/// One agent's simultaneous leaf tools share a permit. Independent sessions
/// acquire the actual checkout mutex separately.
pub struct SoloToolGate {
    checkout: WriteGate,
    active: tokio::sync::Mutex<Weak<OwnedMutexGuard<()>>>,
}

impl SoloToolGate {
    #[must_use]
    pub fn new(checkout: &WriteGate) -> Self {
        Self {
            checkout: Arc::clone(checkout),
            active: tokio::sync::Mutex::new(Weak::new()),
        }
    }

    async fn acquire(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Arc<OwnedMutexGuard<()>>, ToolError> {
        let mut active = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err("checkout write cancelled before execution".into()),
            active = self.active.lock() => active,
        };
        if cancel.is_cancelled() {
            return Err("checkout write cancelled before execution".into());
        }
        if let Some(permit) = active.upgrade() {
            return Ok(permit);
        }
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err("checkout write cancelled before execution".into()),
            permit = Arc::clone(&self.checkout).lock_owned() => permit,
        };
        if cancel.is_cancelled() {
            return Err("checkout write cancelled before execution".into());
        }
        let permit = Arc::new(permit);
        *active = Arc::downgrade(&permit);
        Ok(permit)
    }
}

#[async_trait::async_trait]
impl ToolExecutionGate for SoloToolGate {
    async fn enter(
        &self,
        tool_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<Box<dyn ToolLease>>, ToolError> {
        if !matches!(tool_name, "bash" | "apply_patch") {
            return Ok(None);
        }
        let permit = self.acquire(cancel).await?;
        Ok(Some(Box::new(permit)))
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::solo_tool_gate::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;

    async fn pending<F>(mut future: Pin<&mut F>)
    where
        F: Future,
    {
        std::future::poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn one_agent_shares_until_the_final_leaf_settles() {
        let checkout = Arc::new(tokio::sync::Mutex::new(()));
        let owner = SoloToolGate::new(&checkout);
        let cancel = CancellationToken::new();
        let first = owner.acquire(&cancel).await.unwrap();
        let second = owner.acquire(&cancel).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        drop(first);
        assert!(checkout.try_lock().is_err());
        drop(second);
        assert!(checkout.try_lock().is_ok());
    }

    #[tokio::test]
    async fn other_sessions_cannot_join_the_group() {
        let checkout = Arc::new(tokio::sync::Mutex::new(()));
        let a = SoloToolGate::new(&checkout);
        let b = SoloToolGate::new(&checkout);
        let cancel = CancellationToken::new();
        let first = a.acquire(&cancel).await.unwrap();
        let mut second = Box::pin(b.acquire(&cancel));
        pending(second.as_mut()).await;
        drop(first);
        let second = second.await.unwrap();
        assert!(checkout.try_lock().is_err());
        drop(second);
        assert!(checkout.try_lock().is_ok());
    }

    #[tokio::test]
    async fn different_checkouts_remain_independent() {
        let a = Arc::new(tokio::sync::Mutex::new(()));
        let b = Arc::new(tokio::sync::Mutex::new(()));
        let first = SoloToolGate::new(&a);
        let second = SoloToolGate::new(&b);
        let cancel = CancellationToken::new();
        let _first = first.acquire(&cancel).await.unwrap();
        let _second = second.acquire(&cancel).await.unwrap();
        assert!(a.try_lock().is_err() && b.try_lock().is_err());
    }

    #[tokio::test]
    async fn both_admission_waits_are_cancellable() {
        let checkout = Arc::new(tokio::sync::Mutex::new(()));
        let owner = SoloToolGate::new(&checkout);
        let holder = checkout.lock().await;
        let global_cancel = CancellationToken::new();
        let mut global = Box::pin(owner.acquire(&global_cancel));
        pending(global.as_mut()).await;
        let local_cancel = CancellationToken::new();
        let mut local = Box::pin(owner.acquire(&local_cancel));
        pending(local.as_mut()).await;
        local_cancel.cancel();
        assert!(local.await.is_err());
        global_cancel.cancel();
        assert!(global.await.is_err());
        drop(holder);
        drop(owner.acquire(&CancellationToken::new()).await.unwrap());
        assert!(checkout.try_lock().is_ok());
    }

    #[tokio::test]
    async fn dropped_waiter_does_not_retain_admission() {
        let checkout = Arc::new(tokio::sync::Mutex::new(()));
        let owner = SoloToolGate::new(&checkout);
        let holder = checkout.lock().await;
        let cancel = CancellationToken::new();
        let mut waiter = Box::pin(owner.acquire(&cancel));
        pending(waiter.as_mut()).await;
        drop(waiter);
        drop(holder);
        drop(owner.acquire(&cancel).await.unwrap());
        assert!(checkout.try_lock().is_ok());
    }

    #[tokio::test]
    async fn a_panicked_leaf_releases_its_reference_only() {
        let checkout = Arc::new(tokio::sync::Mutex::new(()));
        let owner = SoloToolGate::new(&checkout);
        let cancel = CancellationToken::new();
        let first = owner.acquire(&cancel).await.unwrap();
        let second = owner.acquire(&cancel).await.unwrap();
        let task = tokio::spawn(async move {
            let _lease = first;
            panic!("test leaf panic");
        });
        assert!(task.await.unwrap_err().is_panic());
        assert!(checkout.try_lock().is_err());
        drop(second);
        assert!(checkout.try_lock().is_ok());
    }

    #[tokio::test]
    async fn only_mutating_leaves_acquire_a_permit() {
        let checkout = Arc::new(tokio::sync::Mutex::new(()));
        let owner = SoloToolGate::new(&checkout);
        let held = checkout.lock().await;
        for name in ["read", "grep", "ask_user"] {
            assert!(
                owner
                    .enter(name, &CancellationToken::new())
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        drop(held);
        let first = owner
            .enter("bash", &CancellationToken::new())
            .await
            .unwrap();
        let second = owner
            .enter("apply_patch", &CancellationToken::new())
            .await
            .unwrap();
        assert!(first.is_some() && second.is_some());
        drop(first);
        assert!(checkout.try_lock().is_err());
        drop(second);
        assert!(checkout.try_lock().is_ok());
    }
}
