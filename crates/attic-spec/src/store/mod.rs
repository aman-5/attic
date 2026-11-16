//! Authoritative store. Never attaches or migrates a catalog or source index.

use crate::{
    Result, SpecError,
    state::{self, Document, Operation, Status},
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

const SCHEMA: &str = include_str!("schema_v1.sql");
const MIGRATIONS: &[(u32, &str)] = &[(1, SCHEMA)];
const DOCUMENT_COLUMNS: &str = "spec_uid,spec_id,owner_root_path,canonical_rel_path,title,
    revision,state_version,status,status_reason,latest_run_id,latest_passing_run_id,
    verified_doc_sha256,verified_fingerprint,verify_attempts,attempts_doc_sha256,
    approved_run_id,approved_doc_sha256,approved_fingerprint,fencing_token,lease_id,
    lease_expires_at,lease_renewals,implemented_at,created_at,updated_at";

pub struct SpecStore {
    connection: Mutex<Connection>,
    read_only: bool,
}

impl SpecStore {
    /// Resolve from the global catalog, never from a managed repository index.
    pub fn path_for_catalog(catalog: &Path) -> PathBuf {
        catalog.with_file_name("spec_state.db")
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_migrations(path, MIGRATIONS)
    }

    fn open_with_migrations(path: &Path, migrations: &[(u32, &str)]) -> Result<Self> {
        let version = migrations.last().map(|m| m.0).ok_or_else(|| {
            SpecError::new(
                "ERR_SPEC_DB",
                "no authoritative store migrations configured",
            )
        })?;
        if migrations
            .iter()
            .enumerate()
            .any(|(i, m)| usize::try_from(m.0).ok() != Some(i + 1))
        {
            return Err(SpecError::new(
                "ERR_SPEC_DB",
                "migrations must be a contiguous forward-only sequence",
            ));
        }
        if let Some(parent) = path.parent() {
            attic_core::ensure_private_dir(parent)?;
        }
        let _migration_lock = MigrationLock::acquire(path)?;
        let mut connection = Connection::open(path)?;
        attic_core::set_private_file_permissions(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        validate_identity(&connection, version)?;
        let old: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if old < version {
            if old > 0 {
                backup_before_upgrade(&connection, path, old)?;
            }
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for &(target, sql) in migrations.iter().filter(|(target, _)| *target > old) {
                tx.execute_batch(sql)?;
                tx.pragma_update(None, "user_version", target)?;
            }
            tx.commit()?;
        }
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
            read_only: false,
        })
    }

    /// Missing stores stay missing. Hooks/status must not create data.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>> {
        if !path.try_exists()? {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let supported = MIGRATIONS.last().expect("nonempty migrations").0;
        validate_identity(&connection, supported)?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != supported {
            return Err(SpecError::new(
                "ERR_SPEC_DB_UNRECOGNIZED",
                "store needs a supported migration",
            ));
        }
        Ok(Some(Self {
            connection: Mutex::new(connection),
            read_only: true,
        }))
    }

    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| SpecError::new("ERR_SPEC_DB", "store mutex poisoned"))?;
        f(&connection)
    }

    pub fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        if self.read_only {
            return Err(SpecError::new("ERR_SPEC_DB", "read-only store"));
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| SpecError::new("ERR_SPEC_DB", "store mutex poisoned"))?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    pub fn documents(&self) -> Result<Vec<Document>> {
        self.read(|conn| {
            let mut query = conn.prepare(&format!(
                "SELECT {DOCUMENT_COLUMNS} FROM spec_documents ORDER BY owner_root_path,spec_id"
            ))?;
            Ok(query
                .query_map([], row_document)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Reconciliation has its own commit so a later refused operation cannot
    /// resurrect an expired lease or stale approval. Hashing is caller-owned
    /// and happens before entering this short transaction.
    pub fn reconcile(&self, expected: &Document, doc_hash: &str, now: i64) -> Result<Document> {
        let mut next = state::reconcile(expected, doc_hash, now)?;
        if next == *expected {
            return self.read(|conn| {
                let live = load(conn, &expected.spec_uid)?;
                if live.state_version != expected.state_version {
                    return Err(SpecError::new(
                        "ERR_STATE_CHANGED_DURING_APPROVAL",
                        "state changed during reconciliation",
                    ));
                }
                Ok(live)
            });
        }
        self.write(|tx| {
            if expected.approved_run_id.is_some() && next.approved_run_id.is_none() {
                void_approval(
                    tx,
                    &expected.spec_uid,
                    next.status_reason.as_deref().unwrap_or("DOC_CHANGED"),
                    now,
                )?;
            }
            save(
                tx,
                expected,
                &mut next,
                "reconcile",
                "RECONCILER",
                &serde_json::json!({}),
                now,
            )?;
            Ok(next)
        })
    }

    /// Reducing privilege does not require the old worker's lease token.
    /// The caller must commit reconciliation first and supply its returned row.
    pub fn reopen(
        &self,
        expected: &Document,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<Document> {
        validate_reason(reason)?;
        state::transition(expected.status, Operation::Reopen)?;
        let mut next = expected.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| SpecError::new("ERR_SPEC_DB", "revision counter exhausted"))?;
        next.leave_implementation(Status::UnderReview)?;
        next.clear_verification();
        next.status_reason = Some(reason.into());
        self.write(|tx| {
            void_approval(tx, &expected.spec_uid, reason, now)?;
            supersede_waves(tx, expected)?;
            save(
                tx,
                expected,
                &mut next,
                "reopen",
                actor,
                &serde_json::json!({"reason":reason}),
                now,
            )?;
            Ok(next)
        })
    }

    pub fn abandon(
        &self,
        expected: &Document,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<Document> {
        validate_reason(reason)?;
        if expected.status == Status::Abandoned {
            return self.read(|conn| {
                let live = load(conn, &expected.spec_uid)?;
                if live.state_version != expected.state_version {
                    return Err(SpecError::new(
                        "ERR_STATE_CHANGED_DURING_APPROVAL",
                        "state changed during abandon",
                    ));
                }
                Ok(live)
            });
        }
        let mut next = expected.clone();
        next.leave_implementation(state::transition(expected.status, Operation::Abandon)?)?;
        next.approved_run_id = None;
        next.approved_doc_sha256 = None;
        next.approved_fingerprint = None;
        next.status_reason = Some(reason.into());
        self.write(|tx| {
            void_approval(tx, &expected.spec_uid, reason, now)?;
            supersede_waves(tx, expected)?;
            save(
                tx,
                expected,
                &mut next,
                "abandon",
                actor,
                &serde_json::json!({"reason":reason}),
                now,
            )?;
            Ok(next)
        })
    }

    pub fn reregister(&self, expected: &Document, title: &str, now: i64) -> Result<Document> {
        if expected.status != Status::Abandoned {
            return self.read(|conn| load(conn, &expected.spec_uid));
        }
        let mut next = expected.clone();
        next.leave_implementation(Status::UnderReview)?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| SpecError::new("ERR_SPEC_DB", "revision counter exhausted"))?;
        next.clear_verification();
        next.status_reason = None;
        next.title = title.into();
        self.write(|tx| {
            void_approval(tx, &expected.spec_uid, "REREGISTERED", now)?;
            supersede_waves(tx, expected)?;
            save(
                tx,
                expected,
                &mut next,
                "register",
                "MCP",
                &serde_json::json!({}),
                now,
            )?;
            Ok(next)
        })
    }
}

fn validate_reason(reason: &str) -> Result<()> {
    if reason.trim().is_empty() || reason.chars().count() > 500 {
        return Err(SpecError::new(
            "ERR_INVALID_ARGUMENT",
            "a nonempty reason of at most 500 characters is required",
        ));
    }
    Ok(())
}

fn supersede_waves(tx: &Transaction<'_>, doc: &Document) -> Result<()> {
    tx.execute(
        "UPDATE spec_execution_waves SET wave_status='SUPERSEDED'
      WHERE spec_uid=?1 AND spec_revision=?2 AND wave_status IN ('PENDING','IN_PROGRESS')",
        params![doc.spec_uid, doc.revision],
    )?;
    Ok(())
}

fn validate_identity(conn: &Connection, supported: u32) -> Result<()> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > supported {
        return Err(SpecError::new(
            "ERR_SPEC_DB_TOO_NEW",
            "store was written by a newer build",
        ));
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if count == 0 && version == 0 {
        return Ok(());
    }
    let format: std::result::Result<String, _> =
        conn.query_row("SELECT value FROM spec_meta WHERE key='format'", [], |r| {
            r.get(0)
        });
    if format.ok().as_deref() != Some("attic-spec-state") || version == 0 {
        return Err(SpecError::new(
            "ERR_SPEC_DB_UNRECOGNIZED",
            "not an authoritative Attic spec store",
        ));
    }
    for table in [
        "spec_documents",
        "spec_verification_runs",
        "spec_citations",
        "spec_approvals",
        "spec_execution_waves",
        "spec_events",
        "spec_session_baselines",
        "spec_content_receipts",
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(SpecError::new(
                "ERR_SPEC_DB_UNRECOGNIZED",
                format!("authoritative store is missing required table {table}"),
            ));
        }
    }
    Ok(())
}

fn backup_before_upgrade(conn: &Connection, path: &Path, old: u32) -> Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let backup = path.with_file_name(format!("{name}.bak-v{old}-{}", uuid::Uuid::new_v4()));
    conn.execute("VACUUM INTO ?1", [backup.to_string_lossy().as_ref()])
        .map_err(|_| {
            SpecError::new(
                "ERR_SPEC_DB_BACKUP_FAILED",
                "could not create pre-upgrade backup",
            )
        })?;
    attic_core::set_private_file_permissions(&backup).map_err(|_| {
        SpecError::new(
            "ERR_SPEC_DB_BACKUP_FAILED",
            "could not protect pre-upgrade backup",
        )
    })?;
    Ok(backup)
}

struct MigrationLock {
    path: PathBuf,
}
impl MigrationLock {
    fn acquire(path: &Path) -> Result<Self> {
        let path = path.with_extension("migration-lock");
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(e)
                    if e.kind() == std::io::ErrorKind::AlreadyExists
                        && start.elapsed() < Duration::from_secs(5) =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(SpecError::busy());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn document_values(doc: &Document) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value;
    let text = |value: &Option<String>| value.clone().map(Value::Text).unwrap_or(Value::Null);
    let timestamp = |value: Option<i64>| value.map(Value::Integer).unwrap_or(Value::Null);
    vec![
        doc.spec_uid.clone().into(),
        doc.spec_id.clone().into(),
        doc.owner_root_path.clone().into(),
        doc.canonical_rel_path.clone().into(),
        doc.title.clone().into(),
        doc.revision.into(),
        doc.state_version.into(),
        doc.status.as_str().to_string().into(),
        text(&doc.status_reason),
        text(&doc.latest_run_id),
        text(&doc.latest_passing_run_id),
        text(&doc.verified_doc_sha256),
        text(&doc.verified_fingerprint),
        i64::from(doc.verify_attempts).into(),
        text(&doc.attempts_doc_sha256),
        text(&doc.approved_run_id),
        text(&doc.approved_doc_sha256),
        text(&doc.approved_fingerprint),
        doc.fencing_token.into(),
        text(&doc.lease_id),
        timestamp(doc.lease_expires_at),
        i64::from(doc.lease_renewals).into(),
        timestamp(doc.implemented_at),
        doc.created_at.into(),
        doc.updated_at.into(),
    ]
}

fn row_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    let status: String = row.get(7)?;
    let status = serde_json::from_value(serde_json::Value::String(status)).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Document {
        spec_uid: row.get(0)?,
        spec_id: row.get(1)?,
        owner_root_path: row.get(2)?,
        canonical_rel_path: row.get(3)?,
        title: row.get(4)?,
        revision: row.get(5)?,
        state_version: row.get(6)?,
        status,
        status_reason: row.get(8)?,
        latest_run_id: row.get(9)?,
        latest_passing_run_id: row.get(10)?,
        verified_doc_sha256: row.get(11)?,
        verified_fingerprint: row.get(12)?,
        verify_attempts: row.get(13)?,
        attempts_doc_sha256: row.get(14)?,
        approved_run_id: row.get(15)?,
        approved_doc_sha256: row.get(16)?,
        approved_fingerprint: row.get(17)?,
        fencing_token: row.get(18)?,
        lease_id: row.get(19)?,
        lease_expires_at: row.get(20)?,
        lease_renewals: row.get(21)?,
        implemented_at: row.get(22)?,
        created_at: row.get(23)?,
        updated_at: row.get(24)?,
    })
}

pub fn load(conn: &Connection, uid: &str) -> Result<Document> {
    conn.query_row(
        &format!("SELECT {DOCUMENT_COLUMNS} FROM spec_documents WHERE spec_uid=?1"),
        [uid],
        row_document,
    )
    .optional()?
    .ok_or_else(|| SpecError::new("ERR_SPEC_NOT_FOUND", "spec not registered"))
}

pub fn insert(tx: &Transaction<'_>, doc: &Document) -> Result<()> {
    tx.execute(&format!("INSERT INTO spec_documents({DOCUMENT_COLUMNS})
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)"),
        rusqlite::params_from_iter(document_values(doc)))?;
    event(
        tx,
        doc,
        None,
        "register",
        "MCP",
        &serde_json::json!({}),
        doc.created_at,
    )
}

pub fn save(
    tx: &Transaction<'_>,
    expected: &Document,
    next: &mut Document,
    op: &str,
    actor: &str,
    detail: &serde_json::Value,
    now: i64,
) -> Result<()> {
    if expected.spec_uid != next.spec_uid
        || expected.spec_id != next.spec_id
        || expected.owner_root_path != next.owner_root_path
        || expected.canonical_rel_path != next.canonical_rel_path
        || expected.created_at != next.created_at
        || next.revision < expected.revision
        || next.fencing_token < expected.fencing_token
    {
        return Err(SpecError::new(
            "ERR_SPEC_ID_CONFLICT",
            "document identity and monotonic counters cannot be rebound",
        ));
    }
    next.state_version = expected
        .state_version
        .checked_add(1)
        .ok_or_else(|| SpecError::new("ERR_SPEC_DB", "state version exhausted"))?;
    next.updated_at = now;
    let mut values = document_values(next);
    values.extend([
        expected.state_version.into(),
        expected.revision.into(),
        expected.status.as_str().to_string().into(),
    ]);
    let count = tx.execute(
        "UPDATE spec_documents SET title=?5,revision=?6,state_version=?7,status=?8,
        status_reason=?9,latest_run_id=?10,latest_passing_run_id=?11,verified_doc_sha256=?12,
        verified_fingerprint=?13,verify_attempts=?14,attempts_doc_sha256=?15,approved_run_id=?16,
        approved_doc_sha256=?17,approved_fingerprint=?18,fencing_token=?19,lease_id=?20,
        lease_expires_at=?21,lease_renewals=?22,implemented_at=?23,updated_at=?25
        WHERE spec_uid=?1 AND spec_id=?2 AND owner_root_path=?3 AND canonical_rel_path=?4
        AND created_at=?24 AND state_version=?26 AND revision=?27 AND status=?28",
        rusqlite::params_from_iter(values),
    )?;
    if count != 1 {
        return Err(SpecError::new(
            "ERR_STATE_CHANGED_DURING_APPROVAL",
            "workflow state changed; retry from current status",
        ));
    }
    event(
        tx,
        next,
        Some(expected.status.as_str()),
        op,
        actor,
        detail,
        now,
    )
}

pub fn event(
    tx: &Transaction<'_>,
    doc: &Document,
    from: Option<&str>,
    op: &str,
    actor: &str,
    detail: &serde_json::Value,
    now: i64,
) -> Result<()> {
    tx.execute("INSERT INTO spec_events(spec_uid,op,actor,from_status,to_status,fencing_token,detail_json,at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![doc.spec_uid,op,actor,from,doc.status.as_str(),doc.fencing_token,serde_json::to_string(detail)?,now])?;
    Ok(())
}

pub fn void_approval(tx: &Transaction<'_>, uid: &str, reason: &str, now: i64) -> Result<()> {
    tx.execute("UPDATE spec_approvals SET voided_at=?2,void_reason=?3 WHERE spec_uid=?1 AND voided_at IS NULL",params![uid,now,reason])?;
    Ok(())
}

/// Historical approvals (including voided approvals) permanently pin their runs.
pub fn retain_runs(tx: &Transaction<'_>, doc: &Document, recent: u32) -> Result<()> {
    tx.execute("DELETE FROM spec_verification_runs WHERE spec_uid=?1
      AND run_id NOT IN (SELECT run_id FROM spec_verification_runs WHERE spec_uid=?1 ORDER BY checked_at DESC,run_id DESC LIMIT ?2)
      AND run_id NOT IN (SELECT run_id FROM spec_approvals WHERE spec_uid=?1)
      AND run_id IS NOT ?3 AND run_id IS NOT ?4 AND run_id IS NOT ?5
      AND run_id NOT IN (SELECT approved_run_id FROM spec_content_receipts WHERE spec_uid=?1)",
      params![doc.spec_uid,recent,doc.latest_run_id,doc.latest_passing_run_id,doc.approved_run_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("egm-tests");
        std::fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn upgrade_backs_up_before_applying_and_preserves_every_previous_backup() {
        let dir = fixture();
        let path = dir.path().join("spec_state.db");
        let store = SpecStore::open(&path).unwrap();
        let doc = Document::new("one", "root", "docs/designs/one.md", "one", 1);
        store.write(|tx| insert(tx, &doc)).unwrap();
        drop(store);
        let upgrades = [(1, SCHEMA), (2, "CREATE TABLE v2_marker(value TEXT);")];
        let store = SpecStore::open_with_migrations(&path, &upgrades).unwrap();
        assert_eq!(store.documents().unwrap()[0].spec_uid, doc.spec_uid);
        drop(store);
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".bak-v1-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        let backup =
            Connection::open_with_flags(&backups[0], OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            backup
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            backup
                .query_row("SELECT COUNT(*) FROM spec_documents", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let bytes = std::fs::read(&backups[0]).unwrap();
        assert_eq!(
            SpecStore::open(&path).err().unwrap().code,
            "ERR_SPEC_DB_TOO_NEW"
        );
        assert_eq!(std::fs::read(&backups[0]).unwrap(), bytes);
    }

    #[test]
    fn failing_upgrade_rolls_back_schema_and_data_and_keeps_backup() {
        let dir = fixture();
        let path = dir.path().join("spec_state.db");
        drop(SpecStore::open(&path).unwrap());
        let upgrades = [
            (1, SCHEMA),
            (2, "CREATE TABLE partial(value); INVALID SQL;"),
        ];
        assert!(SpecStore::open_with_migrations(&path, &upgrades).is_err());
        let store = SpecStore::open(&path).unwrap();
        store
            .read(|conn| {
                assert_eq!(
                    conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))?,
                    1
                );
                assert_eq!(
                    conn.query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE name='partial'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                Ok(())
            })
            .unwrap();
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".bak-v1-")
        }));
    }

    #[test]
    fn backup_failure_does_not_change_the_source_database() {
        let dir = fixture();
        let path = dir.path().join("spec_state.db");
        drop(SpecStore::open(&path).unwrap());
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let before = std::fs::read(&path).unwrap();
        let invalid_destination = dir.path().join("missing-parent").join("spec_state.db");
        assert_eq!(
            backup_before_upgrade(&conn, &invalid_destination, 1)
                .err()
                .unwrap()
                .code,
            "ERR_SPEC_DB_BACKUP_FAILED"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
