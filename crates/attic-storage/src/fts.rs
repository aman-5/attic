//! S5 — FTS5 external-content table helpers.
//!
//! The two FTS5 tables (`fts_retrieval_units`, `fts_symbol_identities`) are
//! declared as **external-content** tables that mirror columns from the base
//! tables.  Callers are responsible for keeping them in sync by calling the
//! insert/delete helpers below whenever the base rows change.
//!
//! **IMPORTANT**: Secrets must never be indexed in FTS tables.  The
//! `fts_retrieval_units` table indexes only the `body` column of retrieval
//! units that have already passed the secret-scan gate.

use rusqlite::Connection;

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// fts_retrieval_units
// ---------------------------------------------------------------------------

/// Insert a row into the `fts_retrieval_units` external-content FTS5 table.
///
/// `rowid` must match the integer primary key of the corresponding
/// `ret_retrieval_units` row.  `body` is the text to be indexed.
pub fn fts_retrieval_unit_insert(
    conn: &Connection,
    rowid: i64,
    body: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_retrieval_units(rowid, body) VALUES (?1, ?2)",
        rusqlite::params![rowid, body],
    )?;
    Ok(())
}

/// Remove a row from the `fts_retrieval_units` FTS5 table using the
/// external-content `'delete'` protocol.
///
/// `old_body` must be the **current** body text of the row being deleted
/// (required by the FTS5 external-content delete protocol).
pub fn fts_retrieval_unit_delete(
    conn: &Connection,
    rowid: i64,
    old_body: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_retrieval_units(fts_retrieval_units, rowid, body)
         VALUES ('delete', ?1, ?2)",
        rusqlite::params![rowid, old_body],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// fts_symbol_identities
// ---------------------------------------------------------------------------

/// Insert a row into the `fts_symbol_identities` external-content FTS5 table.
///
/// `rowid` must match the integer primary key of the corresponding
/// `sym_symbol_identities` row.  `qualified_name` is the text to be indexed.
pub fn fts_symbol_identity_insert(
    conn: &Connection,
    rowid: i64,
    qualified_name: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_symbol_identities(rowid, qualified_name) VALUES (?1, ?2)",
        rusqlite::params![rowid, qualified_name],
    )?;
    Ok(())
}

/// Remove a row from the `fts_symbol_identities` FTS5 table using the
/// external-content `'delete'` protocol.
///
/// `old_qualified_name` must be the **current** qualified name of the row
/// being deleted.
pub fn fts_symbol_identity_delete(
    conn: &Connection,
    rowid: i64,
    old_qualified_name: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_symbol_identities(fts_symbol_identities, rowid, qualified_name)
         VALUES ('delete', ?1, ?2)",
        rusqlite::params![rowid, old_qualified_name],
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
    fn fts_retrieval_unit_insert_and_search() {
        let conn = migrated_conn();

        // Insert a dummy retrieval unit into the base table first so we have a rowid.
        conn.execute_batch(
            "INSERT INTO ret_retrieval_units
               (id, file_occurrence_id, index_generation_id, body,
                lexical_state, semantic_state)
             VALUES
               ('ru-1', 'fo-1', 'ig-1', 'hello world',
                'pending', 'pending');",
        )
        .unwrap();

        let rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM ret_retrieval_units WHERE id = 'ru-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        fts_retrieval_unit_insert(&conn, rowid, "hello world").unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_retrieval_units WHERE fts_retrieval_units MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn fts_retrieval_unit_delete_removes_from_index() {
        let conn = migrated_conn();

        conn.execute_batch(
            "INSERT INTO ret_retrieval_units
               (id, file_occurrence_id, index_generation_id, body,
                lexical_state, semantic_state)
             VALUES
               ('ru-2', 'fo-2', 'ig-2', 'unique phrase xyz',
                'pending', 'pending');",
        )
        .unwrap();

        let rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM ret_retrieval_units WHERE id = 'ru-2'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        fts_retrieval_unit_insert(&conn, rowid, "unique phrase xyz").unwrap();
        fts_retrieval_unit_delete(&conn, rowid, "unique phrase xyz").unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_retrieval_units WHERE fts_retrieval_units MATCH 'unique'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 0);
    }
}
