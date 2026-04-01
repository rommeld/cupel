pub mod models;

use crate::errors::ServiceError;
use rusqlite::{Connection, Result};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

pub use models::SCHEMA_SQL;

pub struct DbPool(pub Arc<Mutex<Connection>>);

impl DbPool {
    pub fn new(connection: Connection) -> Self {
        Self(Arc::new(Mutex::new(connection)))
    }

    pub async fn connect_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self::new(conn))
    }

    pub async fn connect<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self::new(conn))
    }

    pub async fn execute<F, T>(&self, f: F) -> Result<T, ServiceError>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            let guard = db.blocking_lock();
            f(&guard).map_err(ServiceError::from)
        })
        .await
        .map_err(|e| ServiceError::Internal(format!("Task join error: {}", e)))?
    }

    pub async fn execute_async<F, T>(&self, f: F) -> Result<T, ServiceError>
    where
        F: FnOnce(&Connection) -> Result<T, ServiceError> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            let guard = db.blocking_lock();
            f(&guard)
        })
        .await
        .map_err(|e| ServiceError::Internal(format!("Task join error: {}", e)))?
    }
}
