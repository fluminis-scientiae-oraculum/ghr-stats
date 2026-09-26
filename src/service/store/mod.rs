pub mod reader;
mod schema;
pub mod writer;

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::shared::error::Result;

pub fn open_writer(path: &Path) -> Result<Connection> {
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
    Ok(conn)
}

/// A WAL reader still writes the `-shm`/`-wal` sidecars, so it needs write access to the DB
/// directory; never `OPEN_READ_ONLY`. `None` (logged) lets the caller degrade.
pub fn open_reader(path: &Path) -> Option<Connection> {
    let opened = Connection::open(path).and_then(|c| {
        c.busy_timeout(Duration::from_secs(5))?;
        c.pragma_update(None, "query_only", true)?;
        Ok(c)
    });
    match opened {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!(error = %e, path = %path.display(), "open reader db");
            None
        }
    }
}

#[cfg(test)]
pub(crate) fn schema_for_test(conn: &mut Connection) {
    schema::migrate(conn).expect("migrate test db");
}
