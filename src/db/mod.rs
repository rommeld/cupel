pub mod models;

use crate::errors::ServiceError;
use rusqlite::{Connection, Result};
use std::path::Path;
use tokio::sync::Mutex;

pub use models::SCHEMA_SQL;

pub struct DbPool(pub Mutex<Connection>);

impl DbPool {
    pub fn new(connection: Connection) -> Self {
        Self(Mutex::new(connection))
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
        let db = self.0.lock().await;
        f(&*db).map_err(|e| e.into())
    }
}
