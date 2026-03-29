use rusqlite::Connection;
use tokio::sync::Mutex;

pub struct DbPool(pub Mutex<Connection>);
