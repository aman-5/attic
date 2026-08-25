//! `ops_server_state` — singleton operational state (ADR-003 single-row
//! invariant).  Phase 2 uses the watcher epoch (recovery contract REC-W1) and
//! startup/shutdown timestamps.
//!
//! All functions are transaction-assuming primitives safe inside a
//! writer-queue closure.

use rusqlite::Connection;

use crate::error::StorageError;

const SINGLETON_ID: &str = "singleton";

/// Snapshot of the singleton server-state row.
#[derive(Debug, Clone)]
pub struct ServerState {
    /// Monotonically increasing watcher epoch; bumped once per startup.
    pub watcher_epoch: i64,
    /// Schema version recorded at last startup.
    pub schema_version: String,
    /// Server semver recorded at last startup.
    pub server_version: String,
    /// Last startup time (microseconds).
    pub last_startup_at: i64,
    /// Last clean-shutdown time; `None` means the previous stop was a crash.
    pub last_shutdown_at: Option<i64>,
    /// SHA-256 hex of startup configuration.
    pub config_hash: String,
}

/// Read the current singleton state, if the row exists yet.
pub fn get_server_state(conn: &Connection) -> Result<Option<ServerState>, StorageError> {
    use rusqlite::OptionalExtension;
    let row = conn
        .query_row(
            "SELECT watcher_epoch, schema_version, server_version,
                    last_startup_at, last_shutdown_at, config_hash
               FROM ops_server_state WHERE id = ?1",
            [SINGLETON_ID],
            |r| {
                Ok(ServerState {
                    watcher_epoch: r.get(0)?,
                    schema_version: r.get(1)?,
                    server_version: r.get(2)?,
                    last_startup_at: r.get(3)?,
                    last_shutdown_at: r.get(4)?,
                    config_hash: r.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Record this startup: bump the watcher epoch by one and refresh metadata.
///
/// Idempotency note: each call bumps the epoch exactly once — callers invoke
/// this exactly once per process start.  Returns the new epoch.
pub fn record_startup(
    conn: &Connection,
    schema_version: &str,
    server_version: &str,
    config_hash: &str,
    now_us: i64,
) -> Result<i64, StorageError> {
    let prior_epoch: i64 = match get_server_state(conn)? {
        Some(s) => s.watcher_epoch,
        None => 0,
    };
    let new_epoch = prior_epoch + 1;
    conn.execute(
        "INSERT INTO ops_server_state
             (id, watcher_epoch, schema_version, server_version,
              last_startup_at, last_shutdown_at, config_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
         ON CONFLICT(id) DO UPDATE SET
             watcher_epoch   = excluded.watcher_epoch,
             schema_version  = excluded.schema_version,
             server_version  = excluded.server_version,
             last_startup_at = excluded.last_startup_at,
             last_shutdown_at = NULL,
             config_hash     = excluded.config_hash",
        rusqlite::params![
            SINGLETON_ID,
            new_epoch,
            schema_version,
            server_version,
            now_us,
            config_hash
        ],
    )?;
    Ok(new_epoch)
}

/// Record a clean shutdown timestamp (crash detection marker for next start).
pub fn record_clean_shutdown(conn: &Connection, now_us: i64) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE ops_server_state SET last_shutdown_at = ?2 WHERE id = ?1",
        rusqlite::params![SINGLETON_ID, now_us],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn startup_bumps_epoch_and_shutdown_is_recorded() {
        let conn = migrated_conn();
        assert!(get_server_state(&conn).unwrap().is_none());

        let e1 = record_startup(&conn, "3", "0.1.0", "cfg", 100).unwrap();
        assert_eq!(e1, 1);
        let e2 = record_startup(&conn, "3", "0.1.0", "cfg", 200).unwrap();
        assert_eq!(e2, 2, "second start must bump the epoch");

        record_clean_shutdown(&conn, 300).unwrap();
        let st = get_server_state(&conn).unwrap().unwrap();
        assert_eq!(st.last_shutdown_at, Some(300));
        // A subsequent startup clears the clean-shutdown marker.
        let _ = record_startup(&conn, "3", "0.1.0", "cfg", 400).unwrap();
        let st = get_server_state(&conn).unwrap().unwrap();
        assert_eq!(st.last_shutdown_at, None);
        assert_eq!(st.watcher_epoch, 3);
    }
}
