//! Phase 2 — `run_on_writer` atomicity: a failing `f(conn)` must never be
//! observed as a committed mutation (see attic-incremental/src/lib.rs).

mod common;

use common::*;

fn repo_count(fx: &Fixture, id: &str) -> i64 {
    fx.sql_count(&format!(
        "SELECT COUNT(*) FROM core_repositories WHERE id = '{id}'"
    ))
}

fn insert_repo(conn: &rusqlite::Connection, id: &str) -> Result<(), attic_storage::StorageError> {
    conn.execute(
        "INSERT INTO core_repositories \
             (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
             VALUES (?1, ?1, ?1, 1, 1, 0, 0)",
        rusqlite::params![id],
    )?;
    Ok(())
}

/// (a) A successful callback commits its mutation.
#[test]
fn successful_callback_commits() {
    let fx = Fixture::new(&[]);

    let result: Result<(), _> =
        attic_incremental::run_on_writer(&fx.writer, |conn| insert_repo(conn, "wa-ok"));

    assert!(result.is_ok(), "expected Ok, got {result:?}");
    assert_eq!(repo_count(&fx, "wa-ok"), 1);
}

/// (b) An error before any write causes the caller to see `Err` and leaves
/// no partial state behind.
#[test]
fn error_before_any_write_yields_err_and_no_row() {
    let fx = Fixture::new(&[]);

    let result: Result<(), _> = attic_incremental::run_on_writer(&fx.writer, |_conn| {
        Err(attic_storage::StorageError::Worker(
            "intentional failure before any write".into(),
        ))
    });

    assert!(result.is_err(), "expected Err, got {result:?}");
    assert_eq!(repo_count(&fx, "wa-should-not-exist"), 0);
}

/// (c) Write A followed by a failing write B (same transaction) rolls back
/// both — A must not survive just because it ran before the failure.
#[test]
fn write_then_failing_write_rolls_back_both() {
    let fx = Fixture::new(&[]);

    let result: Result<(), _> = attic_incremental::run_on_writer(&fx.writer, |conn| {
        insert_repo(conn, "wa-a")?; // write A: succeeds
        insert_repo(conn, "wa-a")?; // write B: duplicate PK -> fails
        Ok(())
    });

    assert!(
        result.is_err(),
        "expected Err from duplicate-PK failure, got {result:?}"
    );
    assert_eq!(
        repo_count(&fx, "wa-a"),
        0,
        "write A must be rolled back along with failing write B"
    );
    assert!(
        matches!(
            result,
            Err(attic_incremental::IncrementalError::Storage(
                attic_storage::StorageError::Sqlite(_)
            ))
        ),
        "the original StorageError variant must survive end-to-end, got {result:?}"
    );
}

/// (d) After a rollback, a subsequent retry sees pre-transaction state: the
/// writer connection is not poisoned by the earlier failure, and a clean
/// retry with the same id succeeds.
#[test]
fn after_rollback_retry_sees_pre_transaction_state() {
    let fx = Fixture::new(&[]);

    let failed: Result<(), _> = attic_incremental::run_on_writer(&fx.writer, |conn| {
        insert_repo(conn, "wa-retry")?;
        Err(attic_storage::StorageError::Worker("forced failure".into()))
    });
    assert!(failed.is_err());
    assert_eq!(
        repo_count(&fx, "wa-retry"),
        0,
        "failed attempt must leave no trace"
    );

    let retried: Result<(), _> =
        attic_incremental::run_on_writer(&fx.writer, |conn| insert_repo(conn, "wa-retry"));
    assert!(retried.is_ok(), "expected Ok on retry, got {retried:?}");
    assert_eq!(repo_count(&fx, "wa-retry"), 1);
}
