//! S3 — `core_repositories` CRUD operations.

use rusqlite::Connection;

use attic_core::RepositoryId;

use crate::error::StorageError;

/// Insert or update a repository record.
///
/// Uses `ON CONFLICT(id) DO UPDATE` so this is safe to call on every startup
/// even if the repository was previously registered.
pub fn upsert_repository(
    conn: &Connection,
    id: &RepositoryId,
    root_path: &str,
    name: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO core_repositories (id, root_path, name)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET
             root_path = excluded.root_path,
             name      = excluded.name,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![id.to_string_repr(), root_path, name],
    )?;
    Ok(())
}

/// Return the `root_path` of a repository by ID, or `None` if not found.
pub fn get_repository_path(
    conn: &Connection,
    id: &RepositoryId,
) -> Result<Option<String>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT root_path FROM core_repositories WHERE id = ?1",
    )?;
    let mut rows = stmt.query(rusqlite::params![id.to_string_repr()])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
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
    fn upsert_and_get_repository() {
        let conn = migrated_conn();
        let id = RepositoryId::new_v4();

        upsert_repository(&conn, &id, "/home/user/project", "my-project").unwrap();

        let path = get_repository_path(&conn, &id).unwrap();
        assert_eq!(path, Some("/home/user/project".to_owned()));
    }

    #[test]
    fn upsert_updates_existing_record() {
        let conn = migrated_conn();
        let id = RepositoryId::new_v4();

        upsert_repository(&conn, &id, "/old/path", "old-name").unwrap();
        upsert_repository(&conn, &id, "/new/path", "new-name").unwrap();

        let path = get_repository_path(&conn, &id).unwrap();
        assert_eq!(path, Some("/new/path".to_owned()));
    }

    #[test]
    fn get_repository_path_returns_none_for_unknown_id() {
        let conn = migrated_conn();
        let id = RepositoryId::new_v4();
        assert_eq!(get_repository_path(&conn, &id).unwrap(), None);
    }
}
