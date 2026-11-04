//! Catalog and lifecycle management for isolated per-repository SQLite stores.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::connection::{DbPool, open_db, open_db_with_pragmas, verify_connection};
use crate::error::StorageError;
use crate::migration::BASELINE_VERSION;
use crate::writer::{WriterQueue, WriterQueueHandle};

const CATALOG_VERSION: &str = "1";
const INDEX_DIRECTORY: &str = "repositories";
const DATABASE_FILENAME: &str = "index.db";
const CATALOG_SCHEMA_DDL: &str = "CREATE TABLE attic_store_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
INSERT INTO attic_store_meta (key, value) VALUES ('version', '1');
CREATE TABLE attic_repository_stores (
    stable_id TEXT PRIMARY KEY NOT NULL,
    root_key TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL,
    readiness TEXT NOT NULL,
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    owns_index INTEGER NOT NULL CHECK (owns_index IN (0, 1))
);";

/// Readiness state for a repository's independent index store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryReadiness {
    /// Registered but not initialized.
    Registered,
    /// Initialized and verified.
    Ready,
    /// Initialization was interrupted or is currently incomplete.
    Initializing,
    /// The store failed verification and is not safe to open.
    Corrupt,
    /// Deactivated; no new leases or writes are accepted.
    Inactive,
}

impl RepositoryReadiness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Ready => "ready",
            Self::Initializing => "initializing",
            Self::Corrupt => "corrupt",
            Self::Inactive => "inactive",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "registered" => Self::Registered,
            "ready" => Self::Ready,
            "initializing" => Self::Initializing,
            "corrupt" => Self::Corrupt,
            "inactive" => Self::Inactive,
            _ => Self::Corrupt,
        }
    }
}

/// Stable identity and derived paths for a registered repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRegistration {
    /// Stable identifier derived from the canonical repository root.
    pub stable_id: String,
    /// Canonical root path.
    pub root_path: PathBuf,
    /// Manager-owned per-repository index path.
    pub index_path: PathBuf,
    /// Current catalog readiness state.
    pub readiness: RepositoryReadiness,
}

/// Catalog readiness information for one registered repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryStatus {
    /// Stable identifier derived from the canonical repository root.
    pub stable_id: String,
    /// Canonical root path.
    pub root_path: PathBuf,
    /// Manager-owned per-repository index path.
    pub index_path: PathBuf,
    /// Current catalog readiness state.
    pub readiness: RepositoryReadiness,
}

/// Result of deactivating and removing a repository store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryRemoval {
    /// Store files and catalog entry were removed.
    Removed,
    /// Existing leases are draining; cleanup will finish when the last lease drops.
    PendingLeases,
}

struct RepositoryRecord {
    stable_id: String,
    readiness: RepositoryReadiness,
    active: bool,
    owns_index: bool,
}

struct RepositoryStore {
    stable_id: String,
    index_path: PathBuf,
    _writer_queue: WriterQueue,
    writer: WriterQueueHandle,
    readers: DbPool,
    active: AtomicBool,
    write_gate: Arc<Mutex<bool>>,
    leases: AtomicUsize,
}

struct CacheState {
    stores: HashMap<String, Arc<RepositoryStore>>,
    least_recently_used: VecDeque<String>,
}

struct ManagerInner {
    catalog: Mutex<Connection>,
    catalog_writer: Option<WriterQueueHandle>,
    operation: Mutex<()>,
    cache: Mutex<CacheState>,
    catalog_path: PathBuf,
    data_root: PathBuf,
    max_cached_stores: usize,
}

/// Manages a small catalog and bounded cache of per-repository index stores.
///
/// The catalog is opened at the explicit path supplied to [`Self::open`].
/// A catalog can coexist with the canonical storage schema; the manager adds
/// only its own metadata tables and keeps its version in `attic_store_meta`.
/// Index databases are always derived beneath `data_root/repositories/<id>/`
/// and never reuse or migrate a legacy flat database.
#[derive(Clone)]
pub struct RepositoryStoreManager {
    inner: Arc<ManagerInner>,
}

impl RepositoryStoreManager {
    /// Open or create a store manager using an explicit catalog path and data root.
    ///
    /// `max_cached_stores` bounds the number of open writer/read-pool pairs.
    /// Idle cached stores are closed before a new one is opened; leased stores
    /// are never evicted.
    pub fn open(
        catalog_path: impl AsRef<Path>,
        data_root: impl AsRef<Path>,
        max_cached_stores: usize,
    ) -> Result<Self, StorageError> {
        if max_cached_stores == 0 {
            return Err(StorageError::Worker(
                "repository store cache capacity must be greater than zero".into(),
            ));
        }

        fs::create_dir_all(data_root.as_ref())?;
        let data_root = data_root.as_ref().canonicalize()?;
        let index_root = data_root.join(INDEX_DIRECTORY);
        let catalog_path = absolute_file_path(catalog_path.as_ref())?;
        if catalog_path.starts_with(&index_root) {
            return Err(StorageError::Worker(
                "catalog path must not be inside the per-repository index directory".into(),
            ));
        }

        preflight_catalog(&catalog_path)?;
        let (catalog, catalog_readers) = open_db(&catalog_path)?;
        drop(catalog_readers);
        let violations = verify_connection(&catalog)?;
        if let Some(error) = violations.into_iter().next() {
            return Err(error);
        }
        initialize_catalog(&catalog, None)?;

        Self::from_catalog(catalog, None, catalog_path, data_root, max_cached_stores)
    }

    /// Open against an already-running catalog writer, such as the server's
    /// shared canonical database writer.
    ///
    /// This avoids creating a second writer connection for `catalog.db`.
    /// The catalog path must already contain the canonical storage schema and
    /// be open through the supplied writer; catalog reads use a separate
    /// read-only connection. The manager does not run canonical migrations or
    /// modify SQLite's `user_version`.
    pub fn open_with_catalog_writer(
        catalog_path: impl AsRef<Path>,
        data_root: impl AsRef<Path>,
        max_cached_stores: usize,
        catalog_writer: WriterQueueHandle,
    ) -> Result<Self, StorageError> {
        if max_cached_stores == 0 {
            return Err(StorageError::Worker(
                "repository store cache capacity must be greater than zero".into(),
            ));
        }
        fs::create_dir_all(data_root.as_ref())?;
        let data_root = data_root.as_ref().canonicalize()?;
        let index_root = data_root.join(INDEX_DIRECTORY);
        let catalog_path = absolute_file_path(catalog_path.as_ref())?;
        if catalog_path.starts_with(&index_root) {
            return Err(StorageError::Worker(
                "catalog path must not be inside the per-repository index directory".into(),
            ));
        }
        preflight_catalog(&catalog_path)?;
        let catalog = Connection::open_with_flags(
            &catalog_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let violations = verify_connection(&catalog)?;
        if let Some(error) = violations.into_iter().next() {
            return Err(error);
        }
        if !is_canonical_storage_database(&catalog)? {
            return Err(StorageError::Worker(
                "shared catalog writer requires an initialized canonical database".into(),
            ));
        }
        initialize_catalog(&catalog, Some(&catalog_writer))?;

        Self::from_catalog(
            catalog,
            Some(catalog_writer),
            catalog_path,
            data_root,
            max_cached_stores,
        )
    }

    fn from_catalog(
        catalog: Connection,
        catalog_writer: Option<WriterQueueHandle>,
        catalog_path: PathBuf,
        data_root: PathBuf,
        max_cached_stores: usize,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            inner: Arc::new(ManagerInner {
                catalog: Mutex::new(catalog),
                catalog_writer,
                operation: Mutex::new(()),
                cache: Mutex::new(CacheState {
                    stores: HashMap::new(),
                    least_recently_used: VecDeque::new(),
                }),
                catalog_path,
                data_root,
                max_cached_stores,
            }),
        })
    }

    /// Return the catalog database path.
    pub fn catalog_path(&self) -> &Path {
        &self.inner.catalog_path
    }

    /// Register a repository root and return its stable identity and derived index path.
    ///
    /// The root must exist and be a directory. Canonicalization ensures aliases
    /// such as `root/.` map to the same stable identity.
    pub fn register_repository(
        &self,
        root: impl AsRef<Path>,
    ) -> Result<RepositoryRegistration, StorageError> {
        let _operation = self.lock_operation()?;
        let root_path = root.as_ref().canonicalize()?;
        if !root_path.is_dir() {
            return Err(StorageError::Worker(
                "repository root is not a directory".into(),
            ));
        }
        let root_key = normalized_root_key(&root_path);
        let stable_id = stable_repository_id(&root_key);
        let existing = {
            let catalog = self.lock_catalog()?;
            catalog
                .query_row(
                    "SELECT root_key, root_path, readiness, active
                   FROM attic_repository_stores WHERE stable_id = ?1",
                    params![stable_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, bool>(3)?,
                        ))
                    },
                )
                .optional()?
        };

        let readiness = if let Some((stored_key, _, status, active)) = existing {
            if stored_key != root_key {
                return Err(StorageError::Worker(
                    "stable repository identifier collision in store catalog".into(),
                ));
            }
            if !active {
                let store = self.cached_store(&stable_id)?;
                let restored = if store.is_some() {
                    RepositoryReadiness::Ready
                } else {
                    let record = self.load_record(&stable_id)?;
                    if record.owns_index {
                        RepositoryReadiness::Ready
                    } else {
                        RepositoryReadiness::Registered
                    }
                };
                let id = stable_id.clone();
                let readiness = restored.as_str().to_owned();
                self.execute_catalog_mutation(move |catalog| {
                    catalog.execute(
                        "UPDATE attic_repository_stores SET active = 1, readiness = ?2 WHERE stable_id = ?1",
                        params![id, readiness],
                    )?;
                    Ok(())
                })?;
                restored
            } else {
                RepositoryReadiness::parse(&status)
            }
        } else {
            let stored_path = root_path.to_string_lossy().into_owned();
            let inserted_id = stable_id.clone();
            let inserted_key = root_key.clone();
            self.execute_catalog_mutation(move |catalog| {
                catalog.execute(
                    "INSERT INTO attic_repository_stores
                        (stable_id, root_key, root_path, readiness, active, owns_index)
                     VALUES (?1, ?2, ?3, 'registered', 1, 0)",
                    params![inserted_id, inserted_key, stored_path],
                )?;
                Ok(())
            })?;
            RepositoryReadiness::Registered
        };

        Ok(RepositoryRegistration {
            index_path: self.index_path(&stable_id),
            stable_id,
            root_path,
            readiness,
        })
    }

    /// Return readiness information for every catalog entry.
    pub fn list_repositories(&self) -> Result<Vec<RepositoryStatus>, StorageError> {
        let catalog = self.lock_catalog()?;
        let mut statement = catalog.prepare(
            "SELECT stable_id, root_path, readiness, active
               FROM attic_repository_stores ORDER BY stable_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (stable_id, root_path, readiness, active) = row?;
            result.push(RepositoryStatus {
                index_path: self.index_path(&stable_id),
                stable_id,
                root_path: PathBuf::from(root_path),
                readiness: if active {
                    RepositoryReadiness::parse(&readiness)
                } else {
                    RepositoryReadiness::Inactive
                },
            });
        }
        Ok(result)
    }

    /// Ensure a registered store exists and passes integrity and schema checks.
    ///
    /// Only a new, absent manager-owned index receives the baseline migration.
    /// Existing files are verified without invoking migration code that could
    /// reset an unknown schema.
    pub fn ensure_repository_store(
        &self,
        stable_id: &str,
    ) -> Result<RepositoryReadiness, StorageError> {
        let _operation = self.lock_operation()?;
        let record = self.load_record(stable_id)?;
        if !record.active {
            return Ok(RepositoryReadiness::Inactive);
        }
        self.retire_inactive_store(stable_id)?;
        if let Some(store) = self.cached_store(stable_id)? {
            self.touch_lru(stable_id)?;
            return Ok(if store.active.load(Ordering::Acquire) {
                RepositoryReadiness::Ready
            } else {
                RepositoryReadiness::Inactive
            });
        }

        self.make_cache_room()?;
        let store = match self.open_repository_store(&record) {
            Ok(store) => store,
            Err(error) => {
                if !self.is_initializing_owned(stable_id)? {
                    self.set_readiness(stable_id, RepositoryReadiness::Corrupt)?;
                }
                return Err(error);
            }
        };
        self.set_readiness(stable_id, RepositoryReadiness::Ready)?;
        self.insert_cached_store(stable_id, store)?;
        Ok(RepositoryReadiness::Ready)
    }

    /// Acquire a lease with bounded readers and a coordinated single writer.
    ///
    /// The lease must remain alive while using either interface. Deactivation
    /// blocks new leases and serializes against writes already in progress.
    pub fn acquire(&self, stable_id: &str) -> Result<RepositoryStoreLease, StorageError> {
        let _operation = self.lock_operation()?;
        let record = self.load_record(stable_id)?;
        if !record.active {
            return Err(StorageError::Worker("repository store is inactive".into()));
        }
        self.retire_inactive_store(stable_id)?;
        if !self.cached_store(stable_id)?.is_some() {
            self.make_cache_room()?;
            let store = match self.open_repository_store(&record) {
                Ok(store) => store,
                Err(error) => {
                    if !self.is_initializing_owned(stable_id)? {
                        self.set_readiness(stable_id, RepositoryReadiness::Corrupt)?;
                    }
                    return Err(error);
                }
            };
            self.set_readiness(stable_id, RepositoryReadiness::Ready)?;
            self.insert_cached_store(stable_id, store)?;
        }

        let store = self
            .cached_store(stable_id)?
            .ok_or_else(|| StorageError::Worker("repository store cache lost entry".into()))?;
        if !store.active.load(Ordering::Acquire) {
            return Err(StorageError::Worker("repository store is inactive".into()));
        }
        store.leases.fetch_add(1, Ordering::AcqRel);
        self.touch_lru(stable_id)?;
        Ok(RepositoryStoreLease {
            manager: Arc::clone(&self.inner),
            store: Some(store),
        })
    }

    /// Deactivate a repository and remove its owned database files when leases drain.
    ///
    /// Only the exact `index.db`, `index.db-wal`, and `index.db-shm` files in
    /// the derived per-repository directory are eligible for cleanup.
    pub fn remove_repository(&self, stable_id: &str) -> Result<RepositoryRemoval, StorageError> {
        let _operation = self.lock_operation()?;
        let record = match self.try_load_record(stable_id)? {
            Some(record) => record,
            None => return Ok(RepositoryRemoval::Removed),
        };
        {
            let stable_id = stable_id.to_owned();
            self.execute_catalog_mutation(move |catalog| {
                catalog.execute(
                    "UPDATE attic_repository_stores
                    SET active = 0, readiness = 'inactive' WHERE stable_id = ?1",
                    params![stable_id],
                )?;
                Ok(())
            })?;
        }
        if let Some(store) = self.cached_store(stable_id)? {
            let mut gate = store
                .write_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *gate = false;
            store.active.store(false, Ordering::Release);
        }
        if self
            .cached_store(stable_id)?
            .is_some_and(|store| store.leases.load(Ordering::Acquire) != 0)
        {
            return Ok(RepositoryRemoval::PendingLeases);
        }
        self.finish_removal(&record)?;
        Ok(RepositoryRemoval::Removed)
    }

    fn open_repository_store(
        &self,
        record: &RepositoryRecord,
    ) -> Result<Arc<RepositoryStore>, StorageError> {
        if !record.active {
            return Err(StorageError::Worker("repository store is inactive".into()));
        }
        let path = self.index_path(&record.stable_id);
        let path_metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let path_exists = path_metadata.is_some();
        let resume_initialization =
            record.readiness == RepositoryReadiness::Initializing && record.owns_index;
        let initialize_new = !path_exists
            && !record.owns_index
            && record.readiness == RepositoryReadiness::Registered;

        self.verify_index_directories(&record.stable_id)?;
        if initialize_new || resume_initialization {
            self.prepare_index_directory(&record.stable_id)?;
            if initialize_new {
                self.set_initializing(record)?;
            }
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => drop(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if initialize_new {
                        self.set_unowned_corrupt(&record.stable_id)?;
                        return Err(StorageError::Worker(
                            "repository index appeared during initialization; refusing to migrate it"
                                .into(),
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            let metadata = path_metadata
                .ok_or_else(|| StorageError::Worker("repository index is missing".into()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StorageError::Worker(
                    "repository index is not a regular manager-owned file".into(),
                ));
            }
            if record.readiness != RepositoryReadiness::Ready || !record.owns_index {
                return Err(StorageError::Worker(
                    "repository index exists but is not recorded as initialized and manager-owned"
                        .into(),
                ));
            }
        }

        let (writer_connection, readers) = open_db_with_pragmas(&path, -2048, 0)?;
        if initialize_new || resume_initialization {
            crate::migrate_new_or_compatible_catalog(&writer_connection)?;
        }
        verify_store(&writer_connection)?;
        let writer_queue = WriterQueue::new(writer_connection)?;
        let writer = writer_queue.handle();
        Ok(Arc::new(RepositoryStore {
            stable_id: record.stable_id.clone(),
            index_path: path,
            _writer_queue: writer_queue,
            writer,
            readers,
            active: AtomicBool::new(true),
            write_gate: Arc::new(Mutex::new(true)),
            leases: AtomicUsize::new(0),
        }))
    }

    fn prepare_index_directory(&self, stable_id: &str) -> Result<(), StorageError> {
        let root = self.inner.data_root.join(INDEX_DIRECTORY);
        fs::create_dir_all(&root)?;
        ensure_real_directory(&root)?;
        let directory = root.join(stable_id);
        if !directory.exists() {
            fs::create_dir(&directory)?;
        }
        ensure_real_directory(&directory)
    }

    fn verify_index_directories(&self, stable_id: &str) -> Result<(), StorageError> {
        let root = self.inner.data_root.join(INDEX_DIRECTORY);
        ensure_real_directory_if_present(&root)?;
        let directory = root.join(stable_id);
        ensure_real_directory_if_present(&directory)?;
        Ok(())
    }

    fn set_initializing(&self, record: &RepositoryRecord) -> Result<(), StorageError> {
        let stable_id = record.stable_id.clone();
        self.execute_catalog_mutation(move |catalog| {
            catalog.execute(
                "UPDATE attic_repository_stores
                    SET readiness = 'initializing', owns_index = 1
                  WHERE stable_id = ?1 AND active = 1 AND readiness = 'registered'",
                params![stable_id],
            )?;
            Ok(())
        })
    }

    fn is_initializing_owned(&self, stable_id: &str) -> Result<bool, StorageError> {
        let record = self.load_record(stable_id)?;
        Ok(record.readiness == RepositoryReadiness::Initializing && record.owns_index)
    }

    fn set_unowned_corrupt(&self, stable_id: &str) -> Result<(), StorageError> {
        let stable_id = stable_id.to_owned();
        self.execute_catalog_mutation(move |catalog| {
            catalog.execute(
                "UPDATE attic_repository_stores
                    SET readiness = 'corrupt', owns_index = 0 WHERE stable_id = ?1",
                params![stable_id],
            )?;
            Ok(())
        })
    }

    fn load_record(&self, stable_id: &str) -> Result<RepositoryRecord, StorageError> {
        self.try_load_record(stable_id)?.ok_or_else(|| {
            StorageError::Worker(format!("repository store '{stable_id}' is not registered"))
        })
    }

    fn try_load_record(&self, stable_id: &str) -> Result<Option<RepositoryRecord>, StorageError> {
        let catalog = self.lock_catalog()?;
        let row = catalog
            .query_row(
                "SELECT root_key, root_path, readiness, active, owns_index
                   FROM attic_repository_stores WHERE stable_id = ?1",
                params![stable_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((root_key, root_path, _, _, _)) = &row
            && (stable_repository_id(root_key) != stable_id
                || normalized_root_key(Path::new(root_path)).as_str() != root_key.as_str())
        {
            return Err(StorageError::Worker(
                "repository store catalog identity does not match its root".into(),
            ));
        }
        Ok(
            row.map(|(_, _, readiness, active, owns_index)| RepositoryRecord {
                stable_id: stable_id.to_owned(),
                readiness: RepositoryReadiness::parse(&readiness),
                active,
                owns_index,
            }),
        )
    }

    fn set_readiness(
        &self,
        stable_id: &str,
        readiness: RepositoryReadiness,
    ) -> Result<(), StorageError> {
        let stable_id = stable_id.to_owned();
        let readiness = readiness.as_str().to_owned();
        self.execute_catalog_mutation(move |catalog| {
            catalog.execute(
                "UPDATE attic_repository_stores SET readiness = ?2 WHERE stable_id = ?1",
                params![stable_id, readiness],
            )?;
            Ok(())
        })
    }

    fn make_cache_room(&self) -> Result<(), StorageError> {
        let mut cache = self
            .inner
            .cache
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))?;
        while cache.stores.len() >= self.inner.max_cached_stores {
            let evictable = cache.least_recently_used.iter().find_map(|id| {
                cache
                    .stores
                    .get(id)
                    .filter(|store| store.leases.load(Ordering::Acquire) == 0)
                    .map(|_| id.clone())
            });
            let Some(id) = evictable else {
                return Err(StorageError::Worker(
                    "all cached repository stores are currently leased".into(),
                ));
            };
            let evicted = cache.stores.remove(&id);
            cache.least_recently_used.retain(|entry| entry != &id);
            drop(evicted);
        }
        Ok(())
    }

    fn retire_inactive_store(&self, stable_id: &str) -> Result<(), StorageError> {
        let mut cache = self
            .inner
            .cache
            .lock()
            .map_err(|error| StorageError::MutexPoisoned(error.to_string()))?;
        if cache.stores.get(stable_id).is_some_and(|store| {
            !store.active.load(Ordering::Acquire) && store.leases.load(Ordering::Acquire) == 0
        }) {
            cache.least_recently_used.retain(|id| id != stable_id);
            cache.stores.remove(stable_id);
        }
        Ok(())
    }

    fn insert_cached_store(
        &self,
        stable_id: &str,
        store: Arc<RepositoryStore>,
    ) -> Result<(), StorageError> {
        let mut cache = self
            .inner
            .cache
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))?;
        cache.stores.insert(stable_id.to_owned(), store);
        cache.least_recently_used.retain(|entry| entry != stable_id);
        cache.least_recently_used.push_back(stable_id.to_owned());
        Ok(())
    }

    fn cached_store(&self, stable_id: &str) -> Result<Option<Arc<RepositoryStore>>, StorageError> {
        let cache = self
            .inner
            .cache
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))?;
        Ok(cache.stores.get(stable_id).cloned())
    }

    fn touch_lru(&self, stable_id: &str) -> Result<(), StorageError> {
        let mut cache = self
            .inner
            .cache
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))?;
        if cache.stores.contains_key(stable_id) {
            cache.least_recently_used.retain(|entry| entry != stable_id);
            cache.least_recently_used.push_back(stable_id.to_owned());
        }
        Ok(())
    }

    fn finish_removal(&self, record: &RepositoryRecord) -> Result<(), StorageError> {
        if self
            .cached_store(&record.stable_id)?
            .is_some_and(|store| store.leases.load(Ordering::Acquire) != 0)
        {
            return Ok(());
        }

        let store = {
            let mut cache = self
                .inner
                .cache
                .lock()
                .map_err(|e| StorageError::MutexPoisoned(e.to_string()))?;
            cache
                .least_recently_used
                .retain(|entry| entry != &record.stable_id);
            cache.stores.remove(&record.stable_id)
        };
        drop(store);

        if record.owns_index {
            let index_path = self.index_path(&record.stable_id);
            remove_owned_database_files(&index_path)?;
        }
        let stable_id = record.stable_id.clone();
        self.execute_catalog_mutation(move |catalog| {
            catalog.execute(
                "DELETE FROM attic_repository_stores WHERE stable_id = ?1 AND active = 0",
                params![stable_id],
            )?;
            Ok(())
        })
    }

    fn index_path(&self, stable_id: &str) -> PathBuf {
        self.inner
            .data_root
            .join(INDEX_DIRECTORY)
            .join(stable_id)
            .join(DATABASE_FILENAME)
    }

    fn lock_operation(&self) -> Result<std::sync::MutexGuard<'_, ()>, StorageError> {
        self.inner
            .operation
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))
    }

    fn lock_catalog(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.inner
            .catalog
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))
    }

    fn execute_catalog_mutation<F>(&self, mutation: F) -> Result<(), StorageError>
    where
        F: FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static,
    {
        if let Some(writer) = &self.inner.catalog_writer {
            writer.send(mutation)
        } else {
            let catalog = self.lock_catalog()?;
            mutation(&catalog)
        }
    }
}

/// A live lease on a repository store.
pub struct RepositoryStoreLease {
    manager: Arc<ManagerInner>,
    store: Option<Arc<RepositoryStore>>,
}

impl RepositoryStoreLease {
    pub(crate) fn with_publication_fence<T>(
        &self,
        publication: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let store = self.store.as_ref().expect("live lease");
        let gate = store
            .write_gate
            .lock()
            .map_err(|error| StorageError::MutexPoisoned(error.to_string()))?;
        if !*gate {
            return Err(StorageError::QueueShutdown);
        }
        publication()
    }

    fn retain(&self) -> Arc<dyn Send + Sync> {
        let store = self.store.as_ref().expect("live lease");
        store.leases.fetch_add(1, Ordering::AcqRel);
        Arc::new(Self {
            manager: Arc::clone(&self.manager),
            store: Some(Arc::clone(store)),
        })
    }

    /// Clone the read pool, retaining this store until all clones are dropped.
    ///
    /// Prefer retaining the original lease for each request or background task.
    /// The returned pool additionally keeps an internal lease, so escaped
    /// clones cannot race physical removal or reopen a removed store.
    pub fn pool(&self) -> DbPool {
        self.store
            .as_ref()
            .expect("live lease")
            .readers
            .clone()
            .with_lease(self.retain())
    }

    /// Alias for [`Self::pool`] for existing reader-oriented consumers.
    pub fn readers(&self) -> DbPool {
        self.pool()
    }

    /// Clone the coordinated writer, retaining a lease and deactivation fence.
    ///
    /// Every send holds the store's publication gate through transaction
    /// completion. Removal waits for earlier sends, rejects later sends, and
    /// cannot remove or re-add the store until all cloned handles are dropped.
    pub fn writer_handle(&self) -> WriterQueueHandle {
        let store = self.store.as_ref().expect("live lease");
        store
            .writer
            .clone()
            .with_lease(Arc::clone(&store.write_gate), self.retain())
    }

    /// Return the stable repository identifier.
    pub fn repository_id(&self) -> &str {
        &self.store.as_ref().expect("live lease").stable_id
    }

    /// Return the manager-owned index database path.
    pub fn index_path(&self) -> &Path {
        &self.store.as_ref().expect("live lease").index_path
    }

    /// Execute a read against a bounded read-only connection.
    pub fn with_reader<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> Result<T, StorageError>,
    {
        self.store
            .as_ref()
            .expect("live lease")
            .readers
            .with_reader(f)
    }

    /// Return a borrow-scoped writer facade that checks deactivation on every send.
    pub fn writer(&self) -> RepositoryWriter<'_> {
        RepositoryWriter {
            store: self.store.as_ref().expect("live lease"),
        }
    }

    /// Clone the store's commit-generation counter for read-side coordination.
    pub fn generation(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.store.as_ref().expect("live lease").writer.generation()
    }
}

impl Drop for RepositoryStoreLease {
    fn drop(&mut self) {
        let Some(store) = self.store.take() else {
            return;
        };
        let last_lease = store.leases.fetch_sub(1, Ordering::AcqRel) == 1;
        let stable_id = store.stable_id.clone();
        drop(store);
        if last_lease {
            let manager = RepositoryStoreManager {
                inner: Arc::clone(&self.manager),
            };
            if let Ok(_operation) = manager.lock_operation()
                && let Ok(Some(record)) = manager.try_load_record(&stable_id)
                && !record.active
                && let Err(error) = manager.finish_removal(&record)
            {
                tracing::error!(
                    repository_id = stable_id,
                    "repository cleanup failed: {error}"
                );
            }
        }
    }
}

/// A writer facade that cannot outlive its lease and rejects writes after deactivation.
pub struct RepositoryWriter<'a> {
    store: &'a RepositoryStore,
}

impl RepositoryWriter<'_> {
    /// Submit a serialized mutation and wait for its transaction to finish.
    pub fn send<F>(&self, f: F) -> Result<(), StorageError>
    where
        F: FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static,
    {
        let _gate = self
            .store
            .write_gate
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))?;
        if !self.store.active.load(Ordering::Acquire) {
            return Err(StorageError::Worker(
                "repository store is inactive; publication rejected".into(),
            ));
        }
        self.store.writer.send(f)
    }
}

fn initialize_catalog(
    conn: &Connection,
    writer: Option<&WriterQueueHandle>,
) -> Result<(), StorageError> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'attic_store_meta')",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if tables != 0 && !is_canonical_storage_database(conn)? {
            return Err(StorageError::Worker(
                "refusing to initialize a catalog over an unrelated database schema".into(),
            ));
        }
        if let Some(writer) = writer {
            writer.send(|catalog| {
                catalog.execute_batch(CATALOG_SCHEMA_DDL)?;
                Ok(())
            })?;
        } else {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            if let Err(error) = conn.execute_batch(CATALOG_SCHEMA_DDL) {
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(error.into());
            }
            conn.execute_batch("COMMIT;")?;
        }
    } else {
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM attic_store_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if version.as_deref() != Some(CATALOG_VERSION) {
            return Err(StorageError::Worker(
                "unsupported or corrupt repository store catalog version".into(),
            ));
        }
        let has_repositories: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
              WHERE type = 'table' AND name = 'attic_repository_stores')",
            [],
            |row| row.get(0),
        )?;
        if !has_repositories {
            return Err(StorageError::Worker(
                "repository store catalog is missing its repository table".into(),
            ));
        }
    }
    Ok(())
}

fn verify_store(conn: &Connection) -> Result<(), StorageError> {
    let violations = verify_connection(conn)?;
    if !violations.is_empty() {
        return Err(violations.into_iter().next().expect("non-empty violations"));
    }
    let mut statement = conn.prepare("SELECT id FROM core_schema_migrations")?;
    let versions = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if versions.len() != 1 || versions[0] != BASELINE_VERSION {
        return Err(StorageError::Worker(
            "repository index does not match the canonical storage schema".into(),
        ));
    }
    let required_tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type = 'table' AND name IN ('core_repositories', 'core_retrieval_units')",
        [],
        |row| row.get(0),
    )?;
    if required_tables != 2 {
        return Err(StorageError::Worker(
            "repository index is missing canonical schema tables".into(),
        ));
    }
    Ok(())
}

fn stable_repository_id(root_key: &str) -> String {
    let hash = blake3::hash(root_key.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn normalized_root_key(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            value = format!(r"\\{rest}");
        } else if let Some(rest) = value.strip_prefix(r"\\?\") {
            value = rest.to_owned();
        }
        value = value.to_lowercase();
    }
    while value.len() > 1 && value.ends_with('\\') {
        value.pop();
    }
    value
}

fn absolute_file_path(path: &Path) -> Result<PathBuf, StorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StorageError::Worker(
                    "catalog path must be a regular file, not a symlink".into(),
                ));
            }
            return Ok(path.canonicalize()?);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let canonical_parent = parent.canonicalize()?;
    let filename = path
        .file_name()
        .ok_or_else(|| StorageError::Worker("catalog path must include a filename".into()))?;
    Ok(canonical_parent.join(filename))
}

fn preflight_catalog(path: &Path) -> Result<(), StorageError> {
    if !path.exists() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'attic_store_meta')",
        [],
        |row| row.get(0),
    )?;
    if !has_meta {
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if tables != 0 && !is_canonical_storage_database(&conn)? {
            return Err(StorageError::Worker(
                "refusing to open a catalog path containing a non-catalog database".into(),
            ));
        }
    }
    Ok(())
}

fn is_canonical_storage_database(conn: &Connection) -> Result<bool, StorageError> {
    let has_migrations: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'core_schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    if !has_migrations {
        return Ok(false);
    }
    let mut statement = conn.prepare("SELECT id FROM core_schema_migrations")?;
    let versions = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if versions.len() != 1 || versions[0] != BASELINE_VERSION {
        return Ok(false);
    }
    let required_tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type = 'table' AND name IN ('core_repositories', 'core_retrieval_units')",
        [],
        |row| row.get(0),
    )?;
    Ok(required_tables == 2)
}

fn ensure_real_directory(path: &Path) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StorageError::Worker(format!(
            "manager data path is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_real_directory_if_present(path: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(_) => ensure_real_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_owned_database_files(index_path: &Path) -> Result<(), StorageError> {
    let parent = index_path
        .parent()
        .ok_or_else(|| StorageError::Worker("index path has no parent directory".into()))?;
    match fs::symlink_metadata(parent) {
        Ok(_) => ensure_real_directory(parent)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let suffix_path = |suffix: &str| {
        let mut name = index_path
            .file_name()
            .expect("index path has a filename")
            .to_os_string();
        name.push(suffix);
        parent.join(name)
    };
    for path in [
        index_path.to_path_buf(),
        suffix_path("-wal"),
        suffix_path("-shm"),
    ] {
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(StorageError::Worker(format!(
                        "refusing to remove non-file repository store entry: {}",
                        path.display()
                    )));
                }
                fs::remove_file(path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use tempfile::TempDir;

    fn manager(dir: &TempDir, max_cached_stores: usize) -> RepositoryStoreManager {
        RepositoryStoreManager::open(
            dir.path().join("catalog.db"),
            dir.path().join("attic-data"),
            max_cached_stores,
        )
        .expect("open manager")
    }

    fn repository(dir: &TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        fs::create_dir_all(&root).expect("create repository");
        root
    }

    #[test]
    fn status_metadata_does_not_wait_for_store_verification_lock() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 1);
        manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        let operation = manager.lock_operation().unwrap();
        let reader = manager.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            sender.send(reader.list_repositories()).unwrap();
        });
        let response = receiver.recv_timeout(std::time::Duration::from_secs(1));
        drop(operation);
        thread.join().unwrap();
        assert_eq!(
            response
                .expect("status must not await integrity verification")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn canonical_root_has_stable_id_across_registration_and_reopen() {
        let dir = TempDir::new().unwrap();
        let root = repository(&dir, "repo");
        let first_manager = manager(&dir, 2);
        let first = first_manager.register_repository(&root).unwrap();
        let alias = first_manager.register_repository(root.join(".")).unwrap();
        assert_eq!(first.stable_id, alias.stable_id);

        drop(first_manager);
        let reopened = manager(&dir, 2);
        let again = reopened.register_repository(&root).unwrap();
        assert_eq!(first.stable_id, again.stable_id);
        assert_eq!(first.index_path, again.index_path);
    }

    #[test]
    fn repositories_receive_independent_initialized_stores() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 2);
        let first = manager
            .register_repository(repository(&dir, "one"))
            .unwrap();
        let second = manager
            .register_repository(repository(&dir, "two"))
            .unwrap();
        assert_ne!(first.index_path, second.index_path);

        for registration in [&first, &second] {
            let lease = manager.acquire(&registration.stable_id).unwrap();
            lease
                .writer()
                .send(|conn| {
                    conn.execute_batch("CREATE TABLE store_marker (value TEXT NOT NULL);")?;
                    conn.execute("INSERT INTO store_marker VALUES ('isolated')", [])?;
                    Ok(())
                })
                .unwrap();
            let count = lease
                .with_reader(|conn| {
                    conn.query_row("SELECT COUNT(*) FROM store_marker", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(Into::into)
                })
                .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn removal_waits_for_leases_then_cleans_only_owned_database_group() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 1);
        let registration = manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        let lease = manager.acquire(&registration.stable_id).unwrap();
        assert_eq!(
            manager.remove_repository(&registration.stable_id).unwrap(),
            RepositoryRemoval::PendingLeases
        );
        assert!(manager.acquire(&registration.stable_id).is_err());
        assert!(lease.writer().send(|_| Ok(())).is_err());

        drop(lease);
        assert!(!registration.index_path.exists());
        assert!(manager.list_repositories().unwrap().is_empty());
        let readded = manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        assert_eq!(registration.stable_id, readded.stable_id);
        assert_eq!(
            manager.ensure_repository_store(&readded.stable_id).unwrap(),
            RepositoryReadiness::Ready
        );
    }

    #[test]
    fn readd_cancels_pending_cleanup_without_losing_index() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 2);
        let root = repository(&dir, "repo");
        let registration = manager.register_repository(&root).unwrap();
        let lease = manager.acquire(&registration.stable_id).unwrap();
        assert_eq!(
            manager.remove_repository(&registration.stable_id).unwrap(),
            RepositoryRemoval::PendingLeases
        );
        let readded = manager.register_repository(&root).unwrap();
        assert_eq!(registration.stable_id, readded.stable_id);
        drop(lease);
        assert!(registration.index_path.exists());
        assert!(manager.acquire(&registration.stable_id).is_ok());
    }

    #[test]
    fn twenty_to_nineteen_preserves_other_store_identities() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 2);
        let mut registrations = Vec::new();
        for i in 0..20 {
            let registration = manager
                .register_repository(repository(&dir, &format!("repo{i}")))
                .unwrap();
            manager
                .ensure_repository_store(&registration.stable_id)
                .unwrap();
            registrations.push(registration);
        }
        manager
            .remove_repository(&registrations[7].stable_id)
            .unwrap();
        let remaining = manager.list_repositories().unwrap();
        assert_eq!(remaining.len(), 19);
        for (i, registration) in registrations.iter().enumerate() {
            assert_eq!(registration.index_path.exists(), i != 7);
            assert_eq!(
                remaining
                    .iter()
                    .any(|status| status.stable_id == registration.stable_id),
                i != 7
            );
        }
    }

    #[test]
    fn unrecognized_existing_index_is_not_migrated_or_marked_ready() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 1);
        let registration = manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        fs::create_dir_all(registration.index_path.parent().unwrap()).unwrap();
        fs::write(&registration.index_path, b"not a sqlite database").unwrap();

        assert!(
            manager
                .ensure_repository_store(&registration.stable_id)
                .is_err()
        );
        assert_eq!(
            manager.list_repositories().unwrap()[0].readiness,
            RepositoryReadiness::Corrupt
        );
        assert_eq!(
            fs::read(&registration.index_path).unwrap(),
            b"not a sqlite database"
        );
    }

    #[test]
    fn initializing_store_recovers_after_restart_before_and_after_file_creation() {
        let dir = TempDir::new().unwrap();
        let manager_instance = manager(&dir, 2);
        let missing_file = manager_instance
            .register_repository(repository(&dir, "missing-file"))
            .unwrap();
        let empty_file = manager_instance
            .register_repository(repository(&dir, "empty-file"))
            .unwrap();

        for registration in [&missing_file, &empty_file] {
            let record = manager_instance
                .load_record(&registration.stable_id)
                .unwrap();
            manager_instance.set_initializing(&record).unwrap();
        }
        fs::create_dir_all(empty_file.index_path.parent().unwrap()).unwrap();
        fs::File::create(&empty_file.index_path).unwrap();
        drop(manager_instance);

        let reopened = manager(&dir, 2);
        for registration in [&missing_file, &empty_file] {
            assert_eq!(
                reopened
                    .ensure_repository_store(&registration.stable_id)
                    .unwrap(),
                RepositoryReadiness::Ready
            );
        }
    }

    #[test]
    fn shared_canonical_catalog_keeps_schema_and_uses_shared_writer() {
        let dir = TempDir::new().unwrap();
        let catalog_path = dir.path().join("catalog.db");
        let (catalog_connection, catalog_readers) = open_db(&catalog_path).unwrap();
        run_migrations(&catalog_connection).unwrap();
        let catalog_queue = WriterQueue::new(catalog_connection).unwrap();
        let manager = RepositoryStoreManager::open_with_catalog_writer(
            &catalog_path,
            dir.path().join("attic-data"),
            1,
            catalog_queue.handle(),
        )
        .unwrap();
        manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();

        let (migration_count, catalog_count) = catalog_readers
            .with_reader(|conn| {
                let migrations =
                    conn.query_row("SELECT COUNT(*) FROM core_schema_migrations", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                let stores =
                    conn.query_row("SELECT COUNT(*) FROM attic_repository_stores", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                Ok((migrations, stores))
            })
            .unwrap();
        assert_eq!(migration_count, 1);
        assert_eq!(catalog_count, 1);
    }

    #[test]
    fn cloned_pool_and_writer_retain_lease_and_fence_late_publication() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 1);
        let registration = manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        let lease = manager.acquire(&registration.stable_id).unwrap();
        let readers = lease.pool();
        let writer = lease.writer_handle();
        drop(lease);
        writer.send(|_| Ok(())).unwrap();
        assert_eq!(
            manager.remove_repository(&registration.stable_id).unwrap(),
            RepositoryRemoval::PendingLeases
        );
        assert!(registration.index_path.exists());
        assert!(matches!(
            writer.send(|_| Ok(())),
            Err(StorageError::QueueShutdown)
        ));
        drop(writer);
        assert!(registration.index_path.exists());
        readers
            .with_reader(|conn| {
                conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
                Ok(())
            })
            .unwrap();
        manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        assert!(manager.acquire(&registration.stable_id).is_err());
        drop(readers);
        assert!(registration.index_path.exists());
        assert!(manager.acquire(&registration.stable_id).is_ok());
        assert!(
            manager
                .register_repository(repository(&dir, "repo"))
                .is_ok()
        );
    }

    #[test]
    fn explicit_catalog_initialization_never_resets_unknown_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE preserve_me (value TEXT); INSERT INTO preserve_me VALUES ('original');",
        )
        .unwrap();
        assert!(crate::migrate_new_or_compatible_catalog(&conn).is_err());
        let original: String = conn
            .query_row("SELECT value FROM preserve_me", [], |row| row.get(0))
            .unwrap();
        assert_eq!(original, "original");
        let migration_table_created: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'core_schema_migrations')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!migration_table_created);
    }

    #[test]
    fn metadata_projection_keeps_repository_id_and_excludes_bulk_content() {
        let dir = TempDir::new().unwrap();
        let manager = manager(&dir, 1);
        let registration = manager
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        let lease = manager.acquire(&registration.stable_id).unwrap();
        let repository_id = lease.repository_id().to_owned();
        let root_path = registration.root_path.to_string_lossy().into_owned();
        let indexed_id = repository_id.clone();
        lease.writer().send(move |conn| {
            conn.execute(
                "INSERT INTO core_repositories (id, root_path, display_name, created_at, updated_at)
                 VALUES (?1, ?2, 'repo', 0, 0)",
                params![indexed_id, root_path],
            )?;
            conn.execute(
                "INSERT INTO core_source_revisions
                    (id, repository_id, working_tree_manifest_hash, discovery_policy_hash, captured_at)
                 VALUES ('revision', ?1, 'manifest', 'policy', 1)",
                params![indexed_id],
            )?;
            conn.execute(
                "INSERT INTO core_file_identities (id, repository_id, stable_id_basis)
                 VALUES ('manifest-file', ?1, 'package.json'),
                        ('source-file', ?1, 'src/lib.rs')",
                params![indexed_id],
            )?;
            conn.execute_batch(
                "INSERT INTO core_file_occurrences
                    (id, occurrence_seq, file_identity_id, source_revision_id, path,
                     content_hash, size_bytes, file_type, discovery_class,
                     security_state, existence_state)
                 VALUES ('manifest-occurrence', 1, 'manifest-file', 'revision',
                         'package.json', 'hash', 10, 'CONFIG', 'NORMAL', 'SAFE', 'ACTIVE'),
                        ('source-occurrence', 2, 'source-file', 'revision',
                         'src/lib.rs', 'hash', 20, 'SOURCE', 'NORMAL', 'SAFE', 'ACTIVE');",
            )?;
            Ok(())
        }).unwrap();
        let (catalog, catalog_readers) = open_db(dir.path().join("projection.db")).unwrap();
        crate::migrate_new_or_compatible_catalog(&catalog).unwrap();
        let catalog_queue = WriterQueue::new(catalog).unwrap();
        assert_eq!(
            crate::export_repository_catalog(&lease, &catalog_queue.handle(), &[]).unwrap(),
            1
        );
        let projected_paths: Vec<String> = (0..2_001)
            .map(|index| format!("packages/{index}/package.json"))
            .collect();
        let projection_repository_id = repository_id.clone();
        lease
            .writer()
            .send(move |conn| {
                for (index, path) in projected_paths.iter().enumerate() {
                    let identity = format!("manifest-file-{index}");
                    let occurrence = format!("manifest-occurrence-{index}");
                    conn.execute(
                        "INSERT INTO core_file_identities (id, repository_id, stable_id_basis)
                         VALUES (?1, ?2, ?3)",
                        params![identity, projection_repository_id, path],
                    )?;
                    conn.execute(
                        "INSERT INTO core_file_occurrences
                            (id, occurrence_seq, file_identity_id, source_revision_id, path,
                             content_hash, size_bytes, file_type, discovery_class,
                             security_state, existence_state)
                         VALUES (?1, ?2, ?3, 'revision', ?4, 'hash', 10,
                                 'CONFIG', 'NORMAL', 'SAFE', 'ACTIVE')",
                        params![occurrence, index as i64 + 3, identity, path],
                    )?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            crate::export_repository_catalog(
                &lease,
                &catalog_queue.handle(),
                &(0..2_001)
                    .map(|index| format!("packages/{index}/package.json"))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            2_002
        );
        catalog_readers
            .with_reader(|conn| {
                let projected_id: String =
                    conn.query_row("SELECT id FROM core_repositories", [], |row| row.get(0))?;
                assert_eq!(projected_id, repository_id);
                let paths: Vec<String> = conn
                    .prepare("SELECT path FROM core_file_occurrences")?
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                assert_eq!(paths.len(), 2_002);
                for table in ["core_retrieval_units", "index_analysis_cache"] {
                    let count: i64 =
                        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?;
                    assert_eq!(count, 0);
                }
                Ok(())
            })
            .unwrap();
        manager.remove_repository(&registration.stable_id).unwrap();
        assert!(crate::export_repository_catalog(&lease, &catalog_queue.handle(), &[]).is_err());
    }

    #[test]
    fn interrupted_owned_store_initialization_resumes_without_resetting_unknown_schema() {
        let dir = TempDir::new().unwrap();
        let manager_instance = manager(&dir, 1);
        let registration = manager_instance
            .register_repository(repository(&dir, "repo"))
            .unwrap();
        let record = manager_instance
            .load_record(&registration.stable_id)
            .unwrap();
        manager_instance.set_initializing(&record).unwrap();
        fs::create_dir_all(registration.index_path.parent().unwrap()).unwrap();

        let connection = Connection::open(&registration.index_path).unwrap();
        connection
            .execute_batch("CREATE TABLE unrelated_user_data (value TEXT);")
            .unwrap();
        drop(connection);
        drop(manager_instance);

        let reopened = manager(&dir, 1);
        assert!(
            reopened
                .ensure_repository_store(&registration.stable_id)
                .is_err(),
            "unknown schemas must not be reset while resuming initialization"
        );
        assert_eq!(
            reopened.list_repositories().unwrap()[0].readiness,
            RepositoryReadiness::Initializing,
            "owned initialization failures remain retryable and are not mislabeled corrupt"
        );
        let preserved = Connection::open(&registration.index_path).unwrap();
        let exists: bool = preserved
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'unrelated_user_data')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "unknown data must remain unchanged");
    }
}
