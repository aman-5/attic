//! Persisted store identity + monotonic version (`ops_store_version`).
//!
//! `epoch` changes only when the database file is recreated; `version` is
//! bumped by the writer inside every transaction that changes index-visible
//! tables (see `writer::flush_batch`). Reading both inside one read
//! transaction gives a consistent "which data am I looking at" stamp.

use rusqlite::{Connection, OptionalExtension};

use crate::error::StorageError;

/// `(epoch, version)` of one SQLite store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StoreVersion {
    /// Random id fixed at database creation.
    pub epoch: String,
    /// Monotonic count of committed index-visible write transactions.
    pub version: i64,
}

/// Read the store version, or `None` for a store without the table.
pub fn read_store_version(conn: &Connection) -> Result<Option<StoreVersion>, StorageError> {
    let row = conn
        .query_row(
            "SELECT epoch, version FROM ops_store_version WHERE id = 'singleton'",
            [],
            |r| {
                Ok(StoreVersion {
                    epoch: r.get(0)?,
                    version: r.get(1)?,
                })
            },
        )
        .optional();
    match row {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Run `f` inside one deferred read transaction after checking that the
/// store still has `expected`. The version check and every read in `f`
/// observe the same WAL snapshot, so `f` can never see rows newer than the
/// version it was admitted at.
pub fn read_at_version<T>(
    conn: &Connection,
    expected: &StoreVersion,
    f: impl FnOnce(&Connection) -> Result<T, StorageError>,
) -> Result<Result<T, StoreVersion>, StorageError> {
    conn.execute_batch("BEGIN DEFERRED;")?;
    let outcome = (|| {
        let current = read_store_version(conn)?.unwrap_or(StoreVersion {
            epoch: String::new(),
            version: 0,
        });
        if &current != expected {
            return Ok(Err(current));
        }
        f(conn).map(Ok)
    })();
    let _ = conn.execute_batch("COMMIT;");
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WriterQueue, open_db_with_pragmas, run_migrations};

    #[test]
    fn version_moves_with_index_writes_but_not_ops_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let (conn, pool) = open_db_with_pragmas(&path, -2048, 0).unwrap();
        run_migrations(&conn).unwrap();
        let queue = WriterQueue::new(conn).unwrap();
        let writer = queue.handle();
        let v0 = pool
            .with_reader(read_store_version)
            .unwrap()
            .expect("baseline creates ops_store_version");
        writer
            .send(|c| {
                c.execute(
                    "INSERT INTO ops_server_state (schema_version, server_version, last_startup_at, config_hash)
                     VALUES ('1', '1', 0, 'x')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let v1 = pool.with_reader(read_store_version).unwrap().unwrap();
        assert_eq!(v0, v1, "ops_ writes must not move the store version");
        writer
            .send(|c| {
                c.execute(
                    "INSERT INTO core_repositories (id, root_path, display_name, created_at, updated_at)
                     VALUES ('r', '/r', 'r', 0, 0)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let v2 = pool.with_reader(read_store_version).unwrap().unwrap();
        assert_eq!(v2.epoch, v0.epoch);
        assert_eq!(v2.version, v0.version + 1);
        let stale = pool
            .with_reader(|c| read_at_version(c, &v0, |_| Ok(())))
            .unwrap();
        assert_eq!(stale, Err(v2.clone()));
        let fresh = pool
            .with_reader(|c| read_at_version(c, &v2, |_| Ok(7)))
            .unwrap();
        assert_eq!(fresh, Ok(7));
    }
}
