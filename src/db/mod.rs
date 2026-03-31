pub mod models;

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
}
