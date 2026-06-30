pub mod reader;
mod schema;
pub mod writer;

use std::path::Path;

use rusqlite::Connection;

use crate::error::Result;

/// Owns the SQLite connection. Opened in WAL mode so the collector can write
/// while the TUI reads concurrently, without lock contention.
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// Apply the schema to an in-memory connection, for tests in sibling modules.
#[cfg(test)]
pub(crate) fn schema_for_test(conn: &mut Connection) {
    schema::migrate(conn).expect("migrate test db");
}
