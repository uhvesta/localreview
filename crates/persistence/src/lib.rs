//! SQLite metadata, content-addressed blobs, and recoverable review history.
//!
//! Callers choose the platform application-support directory and this crate
//! keeps every durable artifact below it; no workspace files are modified.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use localreview_domain::{
    Annotation, AnnotationSet, AnnotationSetId, PromptExportRecord, Repository,
    RepositoryComparison, RepositoryId, ReviewSession, ReviewSessionId, ReviewSessionStatus,
    Workspace, WorkspaceId,
};
use rusqlite::{
    backup::Backup, params, Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const LATEST_SCHEMA_VERSION: i32 = 6;
/// The default number of SQLite snapshots retained under the private data
/// root. Callers that expose storage settings can provide a stricter policy
/// through [`BackupPolicy`] without changing historical records.
pub const DEFAULT_BACKUP_RETENTION: usize = 7;
pub const DEFAULT_BACKUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_BACKUP_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const BLOB_PAYLOAD_MARKER: &str = r#"{\"storage\":\"content_addressed_blob\",\"version\":1}"#;

#[derive(Clone, Debug)]
pub struct StateStore {
    root: Arc<PathBuf>,
    connection: Arc<Mutex<Connection>>,
    // This is deliberately an in-process test seam.  It lets both this crate
    // and the service crate prove that multi-record review promotions roll
    // back as one unit, without exposing a production failure mode.
    fail_next_atomic_commit: Arc<AtomicBool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupInfo {
    pub path: PathBuf,
    pub byte_len: u64,
    pub created_at: DateTime<Utc>,
}

/// The bounded retention policy for local SQLite snapshots. The newest
/// snapshot is always retained, even when a single database exceeds
/// `max_total_bytes`; silently removing the only recoverable snapshot would be
/// worse than temporarily exceeding a storage preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupPolicy {
    pub max_backups: usize,
    pub max_total_bytes: Option<u64>,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            max_backups: DEFAULT_BACKUP_RETENTION,
            max_total_bytes: Some(DEFAULT_BACKUP_MAX_TOTAL_BYTES),
        }
    }
}

/// Source-free aggregate storage information for diagnostics and backup
/// settings. It contains no database rows, captured source, or workspace
/// paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStorageReport {
    pub policy: BackupPolicy,
    pub retained_count: usize,
    pub retained_bytes: u64,
    pub newest_backup_at: Option<DateTime<Utc>>,
    pub oldest_backup_at: Option<DateTime<Utc>>,
    pub exceeds_size_preference: bool,
}

/// A source-free database health report suitable for diagnostics UI and
/// support bundles. It deliberately exposes neither SQL rows nor paths inside
/// reviewed workspaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
    pub healthy: bool,
    pub diagnostic: String,
    pub recoverable_backups: Vec<BackupInfo>,
}

/// The safe result of opening persisted application data at process startup.
/// A corrupt database is never silently reset or restored: callers receive a
/// bounded report and must opt into [`StateStore::restore_from_backup`].
#[derive(Debug)]
pub enum StartupState {
    Ready(StateStore),
    RequiresRecovery(RecoveryReport),
}

impl StartupState {
    #[must_use]
    pub fn recovery_report(&self) -> Option<&RecoveryReport> {
        match self {
            Self::Ready(_) => None,
            Self::RequiresRecovery(report) => Some(report),
        }
    }
}

/// A source-free recovery plan. `backup_file_name` values are intentionally
/// file names rather than paths, so an explicit restore cannot be redirected
/// outside LocalReview's private backup directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryReport {
    pub database_present: bool,
    pub diagnostic: String,
    pub recoverable_backups: Vec<RecoveryBackupInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryBackupInfo {
    pub backup_file_name: String,
    pub byte_len: u64,
    pub created_at: DateTime<Utc>,
}

/// The result of a caller-requested backup restore. The previous corrupt
/// database remains under the private `recovery/` directory for inspection;
/// it is never deleted by this operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRestoreResult {
    pub restored_backup_file_name: String,
    pub preserved_database_file_name: Option<String>,
}

/// A compact support payload that is safe to save or attach for diagnosis. It
/// intentionally omits source text, annotation bodies, workspace paths, and
/// raw database errors that can include SQL or OS-specific details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceDiagnostics {
    pub database_healthy: bool,
    pub integrity_diagnostic: String,
    pub recoverable_backup_count: usize,
    pub backup_storage: BackupStorageReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobReference {
    pub sha256: String,
    pub byte_len: u64,
}

#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
}

/// A generation whose large canonical documents have already been durably
/// written into the content-addressed store.  Inserting its small metadata
/// rows into SQLite is still transactional, so an uncommitted preparation can
/// only leave an unreachable blob which `gc_unreferenced_blobs` can reclaim.
#[derive(Clone, Debug)]
pub struct PreparedReviewGeneration {
    comparison: RepositoryComparison,
    files: Vec<PreparedReviewFile>,
}

#[derive(Clone, Debug)]
struct PreparedReviewFile {
    storage_id: String,
    path: String,
    blob: BlobReference,
}

/// The complete durable state transition for a promoted GitHub PR refresh.
/// Provider/worktree payloads remain generic so persistence does not depend on
/// GitHub or Git implementation types.
#[derive(Debug)]
pub struct GitHubPullRequestRefreshPromotion<'a, P, ActiveWorktree, RetiredWorktree> {
    pub workspace: &'a Workspace,
    pub repository: &'a Repository,
    pub github_pull_request_id: &'a str,
    pub github_pull_request: &'a P,
    pub active_worktree_id: &'a str,
    pub active_worktree: &'a ActiveWorktree,
    pub retired_worktree_id: &'a str,
    pub retired_worktree: &'a RetiredWorktree,
    pub session: &'a ReviewSession,
    pub generation: &'a PreparedReviewGeneration,
    pub annotations: &'a [Annotation],
}

/// The durable hand-off for a newly started remote review.  Repository state
/// is intentionally bundled with the capture generations and source-binding
/// setting so a crash cannot make one generation visible without the others.
#[derive(Debug)]
pub struct RemoteReviewReplacementPromotion<'a> {
    pub workspace_id: WorkspaceId,
    pub session: &'a ReviewSession,
    pub active_annotation_set: &'a AnnotationSet,
    pub generations: &'a [PreparedReviewGeneration],
    pub repositories: &'a [Repository],
    pub archived_at: DateTime<Utc>,
    pub setting: Option<(&'a str, &'a str)>,
}

#[derive(Clone, Debug)]
pub struct ClearedAnnotationSet {
    pub archived: AnnotationSet,
    pub active: AnnotationSet,
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("could not access application data at {path}: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("LocalReview's database at {path} needs recovery: {diagnostic}")]
    CorruptDatabase {
        path: PathBuf,
        diagnostic: String,
        recoverable_backups: Vec<RecoveryBackupInfo>,
    },
    #[error("backup policy must retain at least one backup")]
    InvalidBackupPolicy,
    #[error("backup file name is not a valid LocalReview backup")]
    InvalidBackupFileName,
    #[error("backup {backup_file_name} was not found in LocalReview's private backup directory")]
    BackupNotFound { backup_file_name: String },
    #[error(
        "backup {backup_file_name} cannot be restored because it failed validation: {diagnostic}"
    )]
    InvalidRecoveryBackup {
        backup_file_name: String,
        diagnostic: String,
    },
    #[error(
        "the active LocalReview database is healthy; refusing to overwrite it during recovery"
    )]
    RecoveryNotRequired,
    #[error("could not serialize durable record: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("state lock was poisoned")]
    LockPoisoned,
    #[error("review session {0} has no active annotation set")]
    NoActiveAnnotationSet(ReviewSessionId),
    #[error("review session {0} was not found")]
    ReviewSessionNotFound(ReviewSessionId),
    #[error("repository {0} was not found")]
    RepositoryNotFound(RepositoryId),
    #[error("workspace {0} was not found")]
    WorkspaceNotFound(WorkspaceId),
    #[error("invalid content-addressed blob hash")]
    InvalidBlobHash,
    #[error("content-addressed review blob {0} is missing")]
    MissingBlob(String),
    #[error("injected atomic commit failure")]
    InjectedAtomicCommitFailure,
}

impl StateStore {
    /// Opens (and, if required, migrates) the database. Existing databases are
    /// backed up through SQLite's online backup API before any schema change.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, PersistenceError> {
        let root = root.into();
        create_private_directory(&root)?;
        create_private_directory(&root.join("backups"))?;
        create_private_directory(&root.join("blobs"))?;
        let database_path = root.join("state.sqlite");
        let existed = database_path.exists();
        if existed {
            validate_database_for_open(&database_path).map_err(|diagnostic| {
                PersistenceError::CorruptDatabase {
                    path: database_path.clone(),
                    diagnostic,
                    recoverable_backups: recovery_backups_at(&root).unwrap_or_default(),
                }
            })?;
        }
        let mut connection = Connection::open(&database_path)?;
        set_private_file(&database_path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < LATEST_SCHEMA_VERSION {
            if existed && version > 0 {
                backup_connection(&connection, &root.join("backups"), "pre-migration")?;
            }
            migrate(&mut connection, version)?;
        }
        let store = Self {
            root: Arc::new(root),
            connection: Arc::new(Mutex::new(connection)),
            fail_next_atomic_commit: Arc::new(AtomicBool::new(false)),
        };
        store.rotate_backups(BackupPolicy::default())?;
        Ok(store)
    }

    /// Opens state without converting corruption into an empty database. This
    /// is the process-startup entry point: the desktop/CLI can render an
    /// explicit recovery action while all original bytes remain in place.
    pub fn open_for_startup(root: impl Into<PathBuf>) -> Result<StartupState, PersistenceError> {
        let root = root.into();
        match Self::open(root.clone()) {
            Ok(store) => {
                let integrity = store.integrity_report()?;
                if integrity.healthy {
                    Ok(StartupState::Ready(store))
                } else {
                    Ok(StartupState::RequiresRecovery(RecoveryReport {
                        database_present: true,
                        diagnostic: bounded_diagnostic(&integrity.diagnostic),
                        recoverable_backups: recovery_backups_at(&root)?,
                    }))
                }
            }
            Err(PersistenceError::CorruptDatabase {
                path,
                diagnostic,
                recoverable_backups,
            }) => Ok(StartupState::RequiresRecovery(RecoveryReport {
                database_present: path.exists(),
                diagnostic: bounded_diagnostic(&diagnostic),
                recoverable_backups,
            })),
            Err(error) => Err(error),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.as_ref()
    }

    #[must_use]
    pub fn blob_store(&self) -> BlobStore {
        BlobStore {
            root: self.root.join("blobs"),
        }
    }

    pub fn backup_now(&self) -> Result<BackupInfo, PersistenceError> {
        self.backup_now_with_policy(BackupPolicy::default())
    }

    pub fn backup_now_with_policy(
        &self,
        policy: BackupPolicy,
    ) -> Result<BackupInfo, PersistenceError> {
        validate_backup_policy(policy)?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| PersistenceError::LockPoisoned)?;
        let info = backup_connection(&connection, &self.root.join("backups"), "manual")?;
        drop(connection);
        self.rotate_backups(policy)?;
        Ok(info)
    }

    /// Creates a rotated backup only when the newest retained backup is older
    /// than `maximum_age`. Desktop code calls this from a low-frequency
    /// maintenance timer; checking is also safe after wake or reconnect.
    pub fn backup_if_due(
        &self,
        maximum_age: Duration,
    ) -> Result<Option<BackupInfo>, PersistenceError> {
        self.backup_if_due_with_policy(maximum_age, BackupPolicy::default())
    }

    pub fn backup_if_due_with_policy(
        &self,
        maximum_age: Duration,
        policy: BackupPolicy,
    ) -> Result<Option<BackupInfo>, PersistenceError> {
        validate_backup_policy(policy)?;
        let newest = self.list_backups()?.into_iter().next();
        let due = newest.map_or(true, |backup| {
            let Ok(age) = Utc::now().signed_duration_since(backup.created_at).to_std() else {
                return false;
            };
            age >= maximum_age
        });
        if !due {
            return Ok(None);
        }

        let connection = self
            .connection
            .lock()
            .map_err(|_| PersistenceError::LockPoisoned)?;
        let info = backup_connection(&connection, &self.root.join("backups"), "automatic")?;
        drop(connection);
        self.rotate_backups(policy)?;
        Ok(Some(info))
    }

    /// Runs SQLite's full integrity check and returns bounded, source-free
    /// recovery information. The caller can display the retained backups when
    /// corruption is detected without attempting an unsafe automatic restore.
    pub fn integrity_report(&self) -> Result<IntegrityReport, PersistenceError> {
        let diagnostic = self.with_connection(|connection| {
            connection
                .pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0))
                .map_err(PersistenceError::from)
        })?;
        let healthy = diagnostic.eq_ignore_ascii_case("ok");
        let recoverable_backups = if healthy {
            Vec::new()
        } else {
            self.list_backups()?
        };
        Ok(IntegrityReport {
            healthy,
            diagnostic,
            recoverable_backups,
        })
    }

    /// Returns source-free backup capacity information using the supplied
    /// policy. It does not delete anything; callers must explicitly invoke
    /// [`StateStore::apply_backup_policy`] to rotate retained snapshots.
    pub fn backup_storage_report(
        &self,
        policy: BackupPolicy,
    ) -> Result<BackupStorageReport, PersistenceError> {
        validate_backup_policy(policy)?;
        backup_storage_report(&self.list_backups()?, policy)
    }

    /// Applies an explicit backup retention policy. This is intentionally not
    /// folded into ordinary review actions, because removing historical
    /// recovery snapshots is a user-visible maintenance decision.
    pub fn apply_backup_policy(
        &self,
        policy: BackupPolicy,
    ) -> Result<BackupStorageReport, PersistenceError> {
        validate_backup_policy(policy)?;
        self.rotate_backups(policy)?;
        self.backup_storage_report(policy)
    }

    /// Builds a bounded, source-free JSON payload for support or settings UI.
    /// Source blobs, database rows, workspace paths, and annotation bodies are
    /// deliberately never read for this operation.
    pub fn diagnostics(
        &self,
        policy: BackupPolicy,
    ) -> Result<PersistenceDiagnostics, PersistenceError> {
        let integrity = self.integrity_report()?;
        Ok(PersistenceDiagnostics {
            database_healthy: integrity.healthy,
            integrity_diagnostic: bounded_diagnostic(&integrity.diagnostic),
            recoverable_backup_count: integrity.recoverable_backups.len(),
            backup_storage: self.backup_storage_report(policy)?,
        })
    }

    pub fn diagnostics_json(&self, policy: BackupPolicy) -> Result<Vec<u8>, PersistenceError> {
        serde_json::to_vec_pretty(&self.diagnostics(policy)?).map_err(PersistenceError::from)
    }

    pub fn list_backups(&self) -> Result<Vec<BackupInfo>, PersistenceError> {
        list_backups_at(&self.root.join("backups"))
    }

    /// Explicitly restores a validated SQLite backup selected by its exact
    /// file name. It refuses path traversal, validates the snapshot before it
    /// can replace anything, and preserves the previous database (and WAL
    /// sidecars) under `recovery/`.
    pub fn restore_from_backup(
        root: impl Into<PathBuf>,
        backup_file_name: &str,
    ) -> Result<RecoveryRestoreResult, PersistenceError> {
        let root = root.into();
        create_private_directory(&root)?;
        create_private_directory(&root.join("backups"))?;
        let backup_file_name = validate_backup_file_name(backup_file_name)?;
        let backup = list_backups_at(&root.join("backups"))?
            .into_iter()
            .find(|backup| {
                backup.path.file_name().and_then(|name| name.to_str()) == Some(&backup_file_name)
            })
            .ok_or_else(|| PersistenceError::BackupNotFound {
                backup_file_name: backup_file_name.clone(),
            })?;

        let database_path = root.join("state.sqlite");
        if database_path.exists() && validate_database_for_open(&database_path).is_ok() {
            return Err(PersistenceError::RecoveryNotRequired);
        }
        validate_database_for_open(&backup.path).map_err(|diagnostic| {
            PersistenceError::InvalidRecoveryBackup {
                backup_file_name: backup_file_name.clone(),
                diagnostic,
            }
        })?;

        let recovery_directory = root.join("recovery");
        create_private_directory(&recovery_directory)?;
        let temporary = root.join(format!(
            ".state.restore-{}-{}.sqlite",
            Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
            std::process::id()
        ));
        copy_validated_database(&backup.path, &temporary)?;
        validate_database_for_open(&temporary).map_err(|diagnostic| {
            PersistenceError::InvalidRecoveryBackup {
                backup_file_name: backup_file_name.clone(),
                diagnostic,
            }
        })?;

        let preserved_database_file_name =
            preserve_database_for_recovery(&root, &recovery_directory)?;
        fs::rename(&temporary, &database_path).map_err(|source| PersistenceError::File {
            path: database_path,
            source,
        })?;
        set_private_file(&root.join("state.sqlite"))?;
        Ok(RecoveryRestoreResult {
            restored_backup_file_name: backup_file_name,
            preserved_database_file_name,
        })
    }

    pub fn upsert_workspace(&self, workspace: &Workspace) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO workspace (id, display_name, source_json, default_base, pinned, archived_at, created_at, updated_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name, source_json=excluded.source_json,
                   default_base=excluded.default_base, pinned=excluded.pinned, archived_at=excluded.archived_at,
                   updated_at=excluded.updated_at, payload_json=excluded.payload_json",
                params![
                    workspace.id.to_string(), workspace.display_name, to_json(&workspace.source)?, workspace.default_base.as_str(),
                    workspace.pinned, timestamp(workspace.archived_at), timestamp(Some(workspace.created_at)), timestamp(Some(workspace.updated_at)), to_json(workspace)?
                ],
            )?;
            Ok(())
        })
    }

    /// Makes a discovered workspace and all of the repository records that
    /// describe that discovery visible as one durable unit. A process crash
    /// or write failure must not leave an empty workspace record that poisons
    /// a later open attempt.
    pub fn upsert_workspace_discovery(
        &self,
        workspace: &Workspace,
        repositories: &[Repository],
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            upsert_workspace_in_transaction(&transaction, workspace)?;
            for repository in repositories {
                upsert_repository_in_transaction(&transaction, repository)?;
            }
            self.commit_atomic(transaction)
        })
    }

    pub fn workspace(&self, id: WorkspaceId) -> Result<Option<Workspace>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM workspace WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    pub fn workspaces(&self) -> Result<Vec<Workspace>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM workspace ORDER BY pinned DESC, updated_at DESC",
                [],
            )
        })
    }

    pub fn upsert_repository(&self, repository: &Repository) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO workspace_repository (id, workspace_id, relative_path, enabled, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET relative_path=excluded.relative_path, enabled=excluded.enabled, payload_json=excluded.payload_json",
                params![repository.id.to_string(), repository.workspace_id.to_string(), repository.relative_path.as_str(), repository.enabled, to_json(repository)?],
            )?;
            Ok(())
        })
    }

    pub fn repositories(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Repository>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM workspace_repository WHERE workspace_id = ?1 ORDER BY relative_path",
                [workspace_id.to_string()],
            )
        })
    }

    pub fn repositories_for_id(
        &self,
        id: RepositoryId,
    ) -> Result<Option<Repository>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM workspace_repository WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    pub fn save_review_session(&self, session: &ReviewSession) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO review_session (id, workspace_id, status, started_at, refreshed_at, archived_at, completed_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET status=excluded.status, refreshed_at=excluded.refreshed_at,
                   archived_at=excluded.archived_at, completed_at=excluded.completed_at, payload_json=excluded.payload_json",
                params![
                    session.id.to_string(), session.workspace_id.to_string(), serde_json::to_string(&session.status)?,
                    timestamp(Some(session.started_at)), timestamp(session.refreshed_at), timestamp(session.archived_at),
                    timestamp(session.completed_at), to_json(session)?
                ],
            )?;
            Ok(())
        })
    }

    pub fn review_sessions(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ReviewSession>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM review_session WHERE workspace_id = ?1 ORDER BY started_at DESC",
                [workspace_id.to_string()],
            )
        })
    }

    pub fn review_sessions_for_id(
        &self,
        id: ReviewSessionId,
    ) -> Result<Option<ReviewSession>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM review_session WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    pub fn archive_review_session(
        &self,
        mut session: ReviewSession,
        archived_at: DateTime<Utc>,
    ) -> Result<ReviewSession, PersistenceError> {
        session.status = ReviewSessionStatus::Archived;
        session.archived_at = Some(archived_at);
        self.save_review_session(&session)?;
        Ok(session)
    }

    /// Moves an active set into immutable review history when its containing
    /// session is archived. A new review creates its own active set.
    pub fn archive_active_annotation_set(
        &self,
        session_id: ReviewSessionId,
        at: DateTime<Utc>,
    ) -> Result<Option<AnnotationSet>, PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let payload = transaction
                .query_row(
                    "SELECT payload_json FROM annotation_set WHERE review_session_id = ?1 AND active = 1 ORDER BY sequence DESC LIMIT 1",
                    params![session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(payload) = payload else {
                return Ok(None);
            };
            let mut archived: AnnotationSet = from_json(&payload)?;
            archived.active = false;
            archived.archived_at = Some(at);
            transaction.execute(
                "UPDATE annotation_set SET active = 0, archived_at = ?1, payload_json = ?2 WHERE id = ?3",
                params![timestamp(archived.archived_at), to_json(&archived)?, archived.id.to_string()],
            )?;
            transaction.commit()?;
            Ok(Some(archived))
        })
    }

    pub fn save_comparison(
        &self,
        comparison: &RepositoryComparison,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO repository_comparison (id, repository_id, captured_at, payload_json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET payload_json=excluded.payload_json, captured_at=excluded.captured_at",
                params![comparison.id.to_string(), comparison.repository_id.to_string(), timestamp(Some(comparison.captured_at)), to_json(comparison)?],
            )?;
            Ok(())
        })
    }

    /// Persists a captured comparison and records the review session that owns
    /// it. A comparison is immutable, while a session may have many captures
    /// after explicit refreshes.
    pub fn save_session_comparison(
        &self,
        session_id: ReviewSessionId,
        comparison: &RepositoryComparison,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO repository_comparison (id, repository_id, captured_at, payload_json) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET payload_json=excluded.payload_json, captured_at=excluded.captured_at",
                params![comparison.id.to_string(), comparison.repository_id.to_string(), timestamp(Some(comparison.captured_at)), to_json(comparison)?],
            )?;
            transaction.execute(
                "INSERT OR REPLACE INTO review_session_comparison (review_session_id, comparison_id) VALUES (?1, ?2)",
                params![session_id.to_string(), comparison.id.to_string()],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    /// Serializes canonical review documents into content-addressed files
    /// before their metadata is made visible in SQLite.  Unlike caches, this
    /// store lives under application support and is intentionally not placed
    /// in an OS-evictable directory.
    pub fn prepare_review_generation<T: Serialize>(
        &self,
        comparison: &RepositoryComparison,
        documents: &[(String, String, T)],
    ) -> Result<PreparedReviewGeneration, PersistenceError> {
        let blob_store = self.blob_store();
        let files = documents
            .iter()
            .map(|(file_id, path, payload)| {
                let encoded = to_json(payload)?;
                let blob = blob_store.put(encoded.as_bytes())?;
                Ok(PreparedReviewFile {
                    storage_id: format!("{}:{file_id}", comparison.id),
                    path: path.clone(),
                    blob,
                })
            })
            .collect::<Result<Vec<_>, PersistenceError>>()?;
        Ok(PreparedReviewGeneration {
            comparison: comparison.clone(),
            files,
        })
    }

    /// Atomically commits one repository's immutable review generation: its
    /// comparison metadata, blob references, history ownership, and live
    /// promotion.  Document bytes are never copied into SQLite.
    pub fn save_review_generation<T: Serialize>(
        &self,
        session_id: ReviewSessionId,
        comparison: &RepositoryComparison,
        documents: &[(String, String, T)],
    ) -> Result<(), PersistenceError> {
        let generation = self.prepare_review_generation(comparison, documents)?;
        self.save_prepared_review_generation(session_id, &generation)
    }

    pub fn save_prepared_review_generation(
        &self,
        session_id: ReviewSessionId,
        generation: &PreparedReviewGeneration,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            insert_review_generation(&transaction, session_id, generation)?;
            self.commit_atomic(transaction)
        })
    }

    /// Promotes every successfully prepared repository generation and all
    /// affected annotation rewrites in one transaction.  Local refresh builds
    /// every generation before entering this boundary so concurrent readers
    /// can observe either the complete prior review or the complete promoted
    /// review, never a repository-by-repository mixture.
    pub fn save_prepared_review_refresh_with_annotations(
        &self,
        session: &ReviewSession,
        generations: &[PreparedReviewGeneration],
        annotations: &[Annotation],
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            for generation in generations {
                insert_review_generation(&transaction, session.id, generation)?;
            }
            for annotation in annotations {
                upsert_annotation(&transaction, annotation)?;
            }
            upsert_review_session(&transaction, session)?;
            self.commit_atomic(transaction)
        })
    }

    /// Promotes a prepared pull-request refresh as one SQLite transaction.
    ///
    /// The Git worktree itself is intentionally prepared before this method is
    /// called and the retired checkout is intentionally removed afterwards.
    /// This is the durable hand-off point: after it commits, every reader sees
    /// the same workspace path, repository pin, provider record, ownership
    /// record, current comparison, and re-anchored annotations.  A failed
    /// commit leaves all of the previous records live.
    ///
    /// The previous checkout is recorded in `retired_managed_worktree` rather
    /// than being forgotten.  If post-commit filesystem cleanup is interrupted
    /// or finds a checkout that became dirty, startup recovery can retry only
    /// that app-owned record without ever treating it as the active review.
    pub fn promote_github_pull_request_refresh<
        P: Serialize,
        ActiveWorktree: Serialize,
        RetiredWorktree: Serialize,
    >(
        &self,
        promotion: GitHubPullRequestRefreshPromotion<'_, P, ActiveWorktree, RetiredWorktree>,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO workspace (id, display_name, source_json, default_base, pinned, archived_at, created_at, updated_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name, source_json=excluded.source_json,
                   default_base=excluded.default_base, pinned=excluded.pinned, archived_at=excluded.archived_at,
                   updated_at=excluded.updated_at, payload_json=excluded.payload_json",
                params![
                    promotion.workspace.id.to_string(), promotion.workspace.display_name, to_json(&promotion.workspace.source)?, promotion.workspace.default_base.as_str(),
                    promotion.workspace.pinned, timestamp(promotion.workspace.archived_at), timestamp(Some(promotion.workspace.created_at)), timestamp(Some(promotion.workspace.updated_at)), to_json(promotion.workspace)?
                ],
            )?;
            transaction.execute(
                "INSERT INTO workspace_repository (id, workspace_id, relative_path, enabled, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET relative_path=excluded.relative_path, enabled=excluded.enabled, payload_json=excluded.payload_json",
                params![promotion.repository.id.to_string(), promotion.repository.workspace_id.to_string(), promotion.repository.relative_path.as_str(), promotion.repository.enabled, to_json(promotion.repository)?],
            )?;
            transaction.execute(
                "INSERT INTO github_pull_request (id, workspace_id, payload_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET workspace_id=excluded.workspace_id, payload_json=excluded.payload_json",
                params![promotion.github_pull_request_id, promotion.workspace.id.to_string(), to_json(promotion.github_pull_request)?],
            )?;
            transaction.execute(
                "DELETE FROM managed_worktree WHERE id = ?1",
                params![promotion.retired_worktree_id],
            )?;
            transaction.execute(
                "INSERT INTO managed_worktree (id, workspace_id, payload_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET workspace_id=excluded.workspace_id, payload_json=excluded.payload_json",
                params![promotion.active_worktree_id, promotion.workspace.id.to_string(), to_json(promotion.active_worktree)?],
            )?;
            transaction.execute(
                "INSERT INTO retired_managed_worktree (id, workspace_id, payload_json, retired_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET workspace_id=excluded.workspace_id,
                   payload_json=excluded.payload_json, retired_at=excluded.retired_at",
                params![
                    promotion.retired_worktree_id,
                    promotion.workspace.id.to_string(),
                    to_json(promotion.retired_worktree)?,
                    timestamp(Some(Utc::now())),
                ],
            )?;
            insert_review_generation(&transaction, promotion.session.id, promotion.generation)?;
            for annotation in promotion.annotations {
                upsert_annotation(&transaction, annotation)?;
            }
            upsert_review_session(&transaction, promotion.session)?;
            self.commit_atomic(transaction)
        })
    }

    /// Replaces the active review only after every successful repository
    /// capture has been fully prepared.  Previous sessions and annotation sets
    /// are archived in the *same* transaction that creates the replacement
    /// session, set, and all its successful repository generations.
    pub fn replace_active_review(
        &self,
        workspace_id: WorkspaceId,
        session: &ReviewSession,
        active_annotation_set: &AnnotationSet,
        generations: &[PreparedReviewGeneration],
        archived_at: DateTime<Utc>,
    ) -> Result<(), PersistenceError> {
        self.replace_active_review_with_setting(
            workspace_id,
            session,
            active_annotation_set,
            generations,
            archived_at,
            None,
        )
    }

    /// Remote review manifests must become visible with the comparison/file
    /// generation that refers to them.  Keeping this application-setting write
    /// in the same SQLite transaction prevents a crash from promoting a
    /// source-window document whose opaque capture binding was never saved.
    pub fn replace_active_review_with_setting(
        &self,
        workspace_id: WorkspaceId,
        session: &ReviewSession,
        active_annotation_set: &AnnotationSet,
        generations: &[PreparedReviewGeneration],
        archived_at: DateTime<Utc>,
        setting: Option<(&str, &str)>,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let active_sessions = transaction
                .prepare(
                    "SELECT payload_json FROM review_session
                     WHERE workspace_id = ?1 AND status = ?2 ORDER BY started_at, id",
                )?
                .query_map(
                    params![
                        workspace_id.to_string(),
                        serde_json::to_string(&ReviewSessionStatus::Active)?
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for payload in active_sessions {
                let mut prior: ReviewSession = from_json(&payload)?;
                prior.status = ReviewSessionStatus::Archived;
                prior.archived_at = Some(archived_at);
                upsert_review_session(&transaction, &prior)?;
                archive_active_annotation_set_in_transaction(&transaction, prior.id, archived_at)?;
            }
            upsert_review_session(&transaction, session)?;
            insert_annotation_set(&transaction, active_annotation_set)?;
            for generation in generations {
                insert_review_generation(&transaction, session.id, generation)?;
            }
            if let Some((key, value_json)) = setting {
                upsert_application_setting(&transaction, key, value_json)?;
            }
            self.commit_atomic(transaction)
        })
    }

    /// Remote review promotion has repository-level state (resolved bases and
    /// scoped capture errors) that describes the same immutable generation as
    /// its comparison manifest.  Persisting it in the promotion transaction
    /// keeps a failed commit from advertising a new base/error beside the old
    /// live capture.
    pub fn replace_active_review_with_remote_state(
        &self,
        promotion: RemoteReviewReplacementPromotion<'_>,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let active_sessions = transaction
                .prepare(
                    "SELECT payload_json FROM review_session
                     WHERE workspace_id = ?1 AND status = ?2 ORDER BY started_at, id",
                )?
                .query_map(
                    params![
                        promotion.workspace_id.to_string(),
                        serde_json::to_string(&ReviewSessionStatus::Active)?
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            for payload in active_sessions {
                let mut prior: ReviewSession = from_json(&payload)?;
                prior.status = ReviewSessionStatus::Archived;
                prior.archived_at = Some(promotion.archived_at);
                upsert_review_session(&transaction, &prior)?;
                archive_active_annotation_set_in_transaction(
                    &transaction,
                    prior.id,
                    promotion.archived_at,
                )?;
            }
            for repository in promotion.repositories {
                upsert_repository_in_transaction(&transaction, repository)?;
            }
            upsert_review_session(&transaction, promotion.session)?;
            insert_annotation_set(&transaction, promotion.active_annotation_set)?;
            for generation in promotion.generations {
                insert_review_generation(&transaction, promotion.session.id, generation)?;
            }
            if let Some((key, value_json)) = promotion.setting {
                upsert_application_setting(&transaction, key, value_json)?;
            }
            self.commit_atomic(transaction)
        })
    }

    /// Atomically promotes all successful remote repository generations,
    /// updates the active-session timestamp, and persists the matching
    /// manifest-first source bindings. Failed sibling captures are omitted so
    /// their previously current immutable generations remain available.
    pub fn save_prepared_remote_refresh_with_setting(
        &self,
        session: &ReviewSession,
        generations: &[PreparedReviewGeneration],
        setting_key: &str,
        setting_value_json: &str,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            for generation in generations {
                insert_review_generation(&transaction, session.id, generation)?;
            }
            upsert_review_session(&transaction, session)?;
            upsert_application_setting(&transaction, setting_key, setting_value_json)?;
            self.commit_atomic(transaction)
        })
    }

    /// Atomic variant used by remote refreshes.  Each repository update is
    /// committed with the replacement comparison, its source binding, and the
    /// active-session timestamp; a failed commit leaves all four at their
    /// previous durable generation.
    pub fn save_prepared_remote_refresh_with_setting_and_repositories(
        &self,
        session: &ReviewSession,
        generations: &[PreparedReviewGeneration],
        repositories: &[Repository],
        setting_key: &str,
        setting_value_json: &str,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            for repository in repositories {
                upsert_repository_in_transaction(&transaction, repository)?;
            }
            for generation in generations {
                insert_review_generation(&transaction, session.id, generation)?;
            }
            upsert_review_session(&transaction, session)?;
            upsert_application_setting(&transaction, setting_key, setting_value_json)?;
            self.commit_atomic(transaction)
        })
    }

    pub fn comparisons_for_session(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Vec<RepositoryComparison>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT c.payload_json FROM repository_comparison c
                 INNER JOIN review_session_comparison m ON m.comparison_id = c.id
                 WHERE m.review_session_id = ?1 ORDER BY c.captured_at, c.id",
                [session_id.to_string()],
            )
        })
    }

    /// Returns exactly one current successful comparison per repository. The
    /// immutable session comparison history remains available separately for
    /// history/export, but the live review never mixes rows from generations.
    pub fn current_comparisons_for_session(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Vec<RepositoryComparison>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT c.payload_json FROM repository_comparison c
                 INNER JOIN review_session_current_comparison m ON m.comparison_id = c.id
                 WHERE m.review_session_id = ?1 ORDER BY c.repository_id",
                [session_id.to_string()],
            )
        })
    }

    /// Promotes a complete durable capture only after its review documents are
    /// stored. If refresh preparation fails, the previous selected comparison
    /// remains the live view for this repository.
    pub fn set_current_comparison(
        &self,
        session_id: ReviewSessionId,
        comparison: &RepositoryComparison,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO review_session_current_comparison (review_session_id, repository_id, comparison_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(review_session_id, repository_id) DO UPDATE SET comparison_id=excluded.comparison_id",
                params![session_id.to_string(), comparison.repository_id.to_string(), comparison.id.to_string()],
            )?;
            Ok(())
        })
    }

    /// Stores an opaque serialized canonical presentation document. The
    /// persistence layer deliberately stays independent from renderer types.
    pub fn save_review_file_payload<T: Serialize>(
        &self,
        comparison_id: impl ToString,
        file_id: impl ToString,
        path: &str,
        payload: &T,
    ) -> Result<(), PersistenceError> {
        let encoded = to_json(payload)?;
        let blob = self.blob_store().put(encoded.as_bytes())?;
        self.with_connection(|connection| {
            let comparison_id = comparison_id.to_string();
            let file_id = file_id.to_string();
            // A file identity can deliberately survive a refresh so viewed
            // state and selection follow a rename.  The immutable historical
            // document must not be overwritten when that happens, therefore
            // the database primary key is generation + stable file identity.
            let storage_id = format!("{comparison_id}:{file_id}");
            connection.execute(
                "INSERT INTO review_file (id, comparison_id, path, blob_hash, blob_byte_len, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET comparison_id=excluded.comparison_id, path=excluded.path,
                   blob_hash=excluded.blob_hash, blob_byte_len=excluded.blob_byte_len, payload_json=excluded.payload_json",
                params![
                    storage_id,
                    comparison_id,
                    path,
                    blob.sha256,
                    i64::try_from(blob.byte_len).unwrap_or(i64::MAX),
                    BLOB_PAYLOAD_MARKER,
                ],
            )?;
            Ok(())
        })
    }

    pub fn review_file_payload<T: DeserializeOwned>(
        &self,
        file_id: impl ToString,
    ) -> Result<Option<T>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    // Public callers hold the stable file id, while historical
                    // rows are keyed by comparison + id. The newest generation
                    // is the active one for direct UI lookup; history always
                    // uses the comparison-scoped query below.
                    "SELECT blob_hash, blob_byte_len, payload_json FROM review_file
                     WHERE id LIKE ?1 ORDER BY rowid DESC LIMIT 1",
                    params![format!("%:{}", file_id.to_string())],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            payload
                .map(|value| self.read_review_file_payload(value))
                .transpose()
        })
    }

    /// Loads one immutable captured document by its complete durable identity.
    /// Stable file IDs deliberately survive refreshes and remote review
    /// replacement, so history callers must include the owning comparison
    /// instead of accepting the newest row with the same logical file ID.
    pub fn review_file_payload_for_comparison<T: DeserializeOwned>(
        &self,
        comparison_id: impl ToString,
        file_id: impl ToString,
    ) -> Result<Option<T>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT blob_hash, blob_byte_len, payload_json FROM review_file
                     WHERE id = ?1 AND comparison_id = ?2",
                    params![
                        format!("{}:{}", comparison_id.to_string(), file_id.to_string()),
                        comparison_id.to_string(),
                    ],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            payload
                .map(|value| self.read_review_file_payload(value))
                .transpose()
        })
    }

    pub fn review_file_payloads_for_comparisons<T: DeserializeOwned>(
        &self,
        comparison_ids: &[String],
    ) -> Result<Vec<T>, PersistenceError> {
        if comparison_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_connection(|connection| {
            let placeholders = std::iter::repeat("?")
                .take(comparison_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "SELECT blob_hash, blob_byte_len, payload_json FROM review_file
                 WHERE comparison_id IN ({placeholders}) ORDER BY path, id"
            );
            let mut statement = connection.prepare(&query)?;
            let values = rusqlite::params_from_iter(comparison_ids.iter());
            let rows = statement.query_map(values, |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.map(|row| {
                row.map_err(PersistenceError::from)
                    .and_then(|payload| self.read_review_file_payload(payload))
            })
            .collect()
        })
    }

    pub fn save_annotation_set(&self, set: &AnnotationSet) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO annotation_set (id, review_session_id, sequence, active, archived_at, created_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET active=excluded.active, archived_at=excluded.archived_at, payload_json=excluded.payload_json",
                params![
                    set.id.to_string(), set.review_session_id.to_string(), set.sequence, set.active, timestamp(set.archived_at),
                    timestamp(Some(set.created_at)), to_json(set)?
                ],
            )?;
            Ok(())
        })
    }

    pub fn active_annotation_set(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Option<AnnotationSet>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM annotation_set WHERE review_session_id = ?1 AND active = 1 ORDER BY sequence DESC LIMIT 1",
                    params![session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    pub fn annotation_sets(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Vec<AnnotationSet>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM annotation_set WHERE review_session_id = ?1 ORDER BY sequence DESC",
                [session_id.to_string()],
            )
        })
    }

    pub fn annotation_set(
        &self,
        id: AnnotationSetId,
    ) -> Result<Option<AnnotationSet>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM annotation_set WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    /// Archives the active set before returning a fresh empty set. Annotation
    /// rows are preserved and therefore remain available to history and export.
    pub fn clear_active_annotation_set(
        &self,
        session_id: ReviewSessionId,
        at: DateTime<Utc>,
    ) -> Result<ClearedAnnotationSet, PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            let payload = transaction
                .query_row(
                    "SELECT payload_json FROM annotation_set WHERE review_session_id = ?1 AND active = 1 ORDER BY sequence DESC LIMIT 1",
                    params![session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .ok_or(PersistenceError::NoActiveAnnotationSet(session_id))?;
            let mut archived: AnnotationSet = from_json(&payload)?;
            archived.active = false;
            archived.archived_at = Some(at);
            transaction.execute(
                "UPDATE annotation_set SET active = 0, archived_at = ?1, payload_json = ?2 WHERE id = ?3",
                params![timestamp(archived.archived_at), to_json(&archived)?, archived.id.to_string()],
            )?;
            let active = AnnotationSet {
                id: AnnotationSetId::new(),
                review_session_id: session_id,
                sequence: archived.sequence.saturating_add(1),
                active: true,
                archived_at: None,
                created_at: at,
            };
            insert_annotation_set(&transaction, &active)?;
            transaction.commit()?;
            Ok(ClearedAnnotationSet { archived, active })
        })
    }

    pub fn save_annotation(&self, annotation: &Annotation) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO annotation (id, annotation_set_id, kind, state, publication_state, created_at, updated_at, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, state=excluded.state,
                   publication_state=excluded.publication_state, updated_at=excluded.updated_at, payload_json=excluded.payload_json",
                params![
                    annotation.id.to_string(), annotation.annotation_set_id.to_string(), serde_json::to_string(&annotation.kind)?,
                    serde_json::to_string(&annotation.state)?, serde_json::to_string(&annotation.publication_state)?,
                    timestamp(Some(annotation.created_at)), timestamp(Some(annotation.updated_at)), to_json(annotation)?
                ],
            )?;
            transaction.execute(
                "INSERT INTO annotation_revision (annotation_id, revised_at, payload_json) VALUES (?1, ?2, ?3)",
                params![annotation.id.to_string(), timestamp(Some(annotation.updated_at)), to_json(annotation)?],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn annotations(
        &self,
        set_id: AnnotationSetId,
    ) -> Result<Vec<Annotation>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM annotation WHERE annotation_set_id = ?1 ORDER BY created_at, id",
                [set_id.to_string()],
            )
        })
    }

    pub fn save_prompt_export(&self, export: &PromptExportRecord) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO prompt_export (id, review_session_id, annotation_set_id, created_at, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    export.id.to_string(), export.review_session_id.to_string(), export.annotation_set_id.to_string(),
                    timestamp(Some(export.created_at)), to_json(export)?
                ],
            )?;
            Ok(())
        })
    }

    pub fn prompt_exports(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Vec<PromptExportRecord>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM prompt_export WHERE review_session_id = ?1 ORDER BY created_at DESC, id",
                [session_id.to_string()],
            )
        })
    }

    /// Looks up one durable export without interpreting its scope.  Ownership
    /// is intentionally checked by the caller that has the workspace context;
    /// this low-level method must not silently substitute a different export.
    pub fn prompt_export(
        &self,
        export_id: localreview_domain::PromptExportId,
    ) -> Result<Option<PromptExportRecord>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM prompt_export WHERE id = ?1",
                    params![export_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    /// Persists lightweight per-session review chrome (viewed files, current
    /// location and mode). It is opaque JSON so UI evolution does not require a
    /// database migration for every optional UI field.
    pub fn review_session_ui_state<T: DeserializeOwned>(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Option<T>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM review_session_ui_state WHERE review_session_id = ?1",
                    params![session_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|value| from_json(&value)).transpose()
        })
    }

    pub fn save_review_session_ui_state<T: Serialize>(
        &self,
        session_id: ReviewSessionId,
        state: &T,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO review_session_ui_state (review_session_id, payload_json) VALUES (?1, ?2)
                 ON CONFLICT(review_session_id) DO UPDATE SET payload_json=excluded.payload_json",
                params![session_id.to_string(), to_json(state)?],
            )?;
            Ok(())
        })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>, PersistenceError> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT value_json FROM application_setting WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(Into::into)
        })
    }

    pub fn set_setting(&self, key: &str, value_json: &str) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO application_setting (key, value_json, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json, updated_at=excluded.updated_at",
                params![key, value_json, timestamp(Some(Utc::now()))],
            )?;
            Ok(())
        })
    }

    /// Stores the provider snapshot for one PR workspace.  The payload stays
    /// opaque to persistence so provider details do not leak into the generic
    /// review domain, while the workspace relation remains queryable and
    /// transactionally removed if the workspace is deleted.
    pub fn save_github_pull_request<T: Serialize>(
        &self,
        id: &str,
        workspace_id: WorkspaceId,
        payload: &T,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO github_pull_request (id, workspace_id, payload_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET workspace_id=excluded.workspace_id, payload_json=excluded.payload_json",
                params![id, workspace_id.to_string(), to_json(payload)?],
            )?;
            Ok(())
        })
    }

    pub fn github_pull_request_for_workspace<T: DeserializeOwned>(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<T>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM github_pull_request WHERE workspace_id = ?1 ORDER BY id LIMIT 1",
                    params![workspace_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|payload| from_json(&payload)).transpose()
        })
    }

    pub fn save_github_publication<T: Serialize>(
        &self,
        id: &str,
        review_session_id: ReviewSessionId,
        publication_attempt_id: &str,
        payload: &T,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO github_publication (id, review_session_id, publication_attempt_id, payload_json)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET review_session_id=excluded.review_session_id,
                   publication_attempt_id=excluded.publication_attempt_id, payload_json=excluded.payload_json",
                params![id, review_session_id.to_string(), publication_attempt_id, to_json(payload)?],
            )?;
            Ok(())
        })
    }

    pub fn github_publication_by_attempt<T: DeserializeOwned>(
        &self,
        publication_attempt_id: &str,
    ) -> Result<Option<T>, PersistenceError> {
        self.with_connection(|connection| {
            let payload = connection
                .query_row(
                    "SELECT payload_json FROM github_publication WHERE publication_attempt_id = ?1",
                    params![publication_attempt_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            payload.map(|payload| from_json(&payload)).transpose()
        })
    }

    pub fn github_publications_for_session<T: DeserializeOwned>(
        &self,
        review_session_id: ReviewSessionId,
    ) -> Result<Vec<T>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM github_publication WHERE review_session_id = ?1 ORDER BY id",
                [review_session_id.to_string()],
            )
        })
    }

    /// Mirrors the file-registry worktree record into SQLite so review
    /// history, diagnostics, and cleanup ownership remain queryable even if a
    /// registry-file recovery is required at startup.
    pub fn save_managed_worktree<T: Serialize>(
        &self,
        id: &str,
        workspace_id: WorkspaceId,
        payload: &T,
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO managed_worktree (id, workspace_id, payload_json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET workspace_id=excluded.workspace_id, payload_json=excluded.payload_json",
                params![id, workspace_id.to_string(), to_json(payload)?],
            )?;
            Ok(())
        })
    }

    pub fn remove_managed_worktree(&self, id: &str) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute("DELETE FROM managed_worktree WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Returns app-owned checkouts which were superseded durably but still
    /// need filesystem cleanup.  The payload is generic so persistence stays
    /// independent of the Git implementation.
    pub fn retired_managed_worktrees<T: DeserializeOwned>(
        &self,
    ) -> Result<Vec<T>, PersistenceError> {
        self.with_connection(|connection| {
            query_json_list(
                connection,
                "SELECT payload_json FROM retired_managed_worktree ORDER BY retired_at, id",
                [],
            )
        })
    }

    /// Marks a post-promotion worktree cleanup as complete.  Callers must do
    /// this only after Git has removed the clean app-owned checkout.
    pub fn complete_retired_managed_worktree(&self, id: &str) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM retired_managed_worktree WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    /// Commits the observed remote publication and all included annotation
    /// state together.  A process interruption can therefore only leave a
    /// pre-POST `prepared` attempt (which is reconciled), never a remote id
    /// paired with locally-unpublished comments.
    pub fn save_github_publication_and_annotations<T: Serialize>(
        &self,
        id: &str,
        review_session_id: ReviewSessionId,
        publication_attempt_id: &str,
        publication: &T,
        annotations: &[Annotation],
    ) -> Result<(), PersistenceError> {
        self.with_connection(|connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO github_publication (id, review_session_id, publication_attempt_id, payload_json)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET review_session_id=excluded.review_session_id,
                   publication_attempt_id=excluded.publication_attempt_id, payload_json=excluded.payload_json",
                params![id, review_session_id.to_string(), publication_attempt_id, to_json(publication)?],
            )?;
            for annotation in annotations {
                transaction.execute(
                    "INSERT INTO annotation (id, annotation_set_id, kind, state, publication_state, created_at, updated_at, payload_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(id) DO UPDATE SET annotation_set_id=excluded.annotation_set_id, kind=excluded.kind,
                       state=excluded.state, publication_state=excluded.publication_state, updated_at=excluded.updated_at,
                       payload_json=excluded.payload_json",
                    params![
                        annotation.id.to_string(), annotation.annotation_set_id.to_string(),
                        serde_json::to_string(&annotation.kind)?, serde_json::to_string(&annotation.state)?,
                        serde_json::to_string(&annotation.publication_state)?, timestamp(Some(annotation.created_at)),
                        timestamp(Some(annotation.updated_at)), to_json(annotation)?
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    /// Reclaims only content-addressed files which have no durable review-file
    /// metadata reference.  It is intentionally opt-in: normal history
    /// retention never removes captured review payloads.
    pub fn gc_unreferenced_blobs(&self) -> Result<usize, PersistenceError> {
        let referenced = self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT blob_hash FROM review_file WHERE blob_hash IS NOT NULL
                 UNION
                 SELECT blob_hash FROM review_snapshot",
            )?;
            let referenced = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<std::collections::BTreeSet<_>, _>>()
                .map_err(Into::into);
            referenced
        })?;
        let root = self.root.join("blobs");
        let mut removed = 0;
        for prefix in fs::read_dir(&root).map_err(|source| PersistenceError::File {
            path: root.clone(),
            source,
        })? {
            let prefix = prefix.map_err(|source| PersistenceError::File {
                path: root.clone(),
                source,
            })?;
            if !prefix
                .file_type()
                .map_err(|source| PersistenceError::File {
                    path: prefix.path(),
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            for entry in fs::read_dir(prefix.path()).map_err(|source| PersistenceError::File {
                path: prefix.path(),
                source,
            })? {
                let entry = entry.map_err(|source| PersistenceError::File {
                    path: prefix.path(),
                    source,
                })?;
                if !entry
                    .file_type()
                    .map_err(|source| PersistenceError::File {
                        path: entry.path(),
                        source,
                    })?
                    .is_file()
                {
                    continue;
                }
                let hash = format!("{prefix_name}{}", entry.file_name().to_string_lossy());
                if hash.len() == 64
                    && hash.bytes().all(|value| value.is_ascii_hexdigit())
                    && !referenced.contains(&hash)
                {
                    fs::remove_file(entry.path()).map_err(|source| PersistenceError::File {
                        path: entry.path(),
                        source,
                    })?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Test-only fault-injection seam for callers that need to assert review
    /// promotion rollback.  It is inert until explicitly invoked and is
    /// consumed by the next atomic review commit.
    #[doc(hidden)]
    pub fn inject_next_atomic_commit_failure_for_test(&self) {
        self.fail_next_atomic_commit.store(true, Ordering::SeqCst);
    }

    fn read_review_file_payload<T: DeserializeOwned>(
        &self,
        (blob_hash, blob_byte_len, payload_json): (Option<String>, Option<i64>, String),
    ) -> Result<T, PersistenceError> {
        let Some(sha256) = blob_hash else {
            // Databases created before schema v5 retain their JSON rows until
            // they are naturally rewritten by a later review capture.
            return from_json(&payload_json);
        };
        let byte_len = blob_byte_len
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(PersistenceError::MissingBlob(sha256.clone()))?;
        let reference = BlobReference { sha256, byte_len };
        let bytes = self
            .blob_store()
            .get(&reference)?
            .ok_or_else(|| PersistenceError::MissingBlob(reference.sha256.clone()))?;
        let text = String::from_utf8(bytes).map_err(|source| {
            PersistenceError::Serialize(serde_json::Error::io(io::Error::new(
                io::ErrorKind::InvalidData,
                source,
            )))
        })?;
        from_json(&text)
    }

    fn commit_atomic(&self, transaction: Transaction<'_>) -> Result<(), PersistenceError> {
        if self.fail_next_atomic_commit.swap(false, Ordering::SeqCst) {
            return Err(PersistenceError::InjectedAtomicCommitFailure);
        }
        transaction.commit()?;
        Ok(())
    }

    fn rotate_backups(&self, policy: BackupPolicy) -> Result<(), PersistenceError> {
        validate_backup_policy(policy)?;
        let backups = self.list_backups()?;
        let retained = retained_backups(&backups, policy);
        let retained_paths = retained
            .iter()
            .map(|backup| backup.path.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for backup in backups {
            if retained_paths.contains(&backup.path) {
                continue;
            }
            // Backups are only removed by this explicit bounded rotation
            // policy; normal review actions never permanently erase history.
            fs::remove_file(&backup.path).map_err(|source| PersistenceError::File {
                path: backup.path,
                source,
            })?;
        }
        Ok(())
    }

    fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, PersistenceError>,
    ) -> Result<T, PersistenceError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| PersistenceError::LockPoisoned)?;
        operation(&mut connection)
    }
}

impl BlobStore {
    pub fn put(&self, bytes: &[u8]) -> Result<BlobReference, PersistenceError> {
        let hash = hex::encode(Sha256::digest(bytes));
        let directory = self.root.join(&hash[..2]);
        create_private_directory(&directory)?;
        let target = directory.join(&hash[2..]);
        if !target.exists() {
            let temporary = directory.join(format!(".{}.{}.tmp", &hash[2..], std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(mut file) => {
                    set_private_file(&temporary)?;
                    file.write_all(bytes)
                        .map_err(|source| PersistenceError::File {
                            path: temporary.clone(),
                            source,
                        })?;
                    file.sync_all().map_err(|source| PersistenceError::File {
                        path: temporary.clone(),
                        source,
                    })?;
                    match fs::rename(&temporary, &target) {
                        Ok(()) => {}
                        Err(error) if target.exists() => {
                            let _ = fs::remove_file(&temporary);
                        }
                        Err(source) => {
                            return Err(PersistenceError::File {
                                path: target,
                                source,
                            })
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && target.exists() => {}
                Err(source) => {
                    return Err(PersistenceError::File {
                        path: temporary,
                        source,
                    })
                }
            }
        }
        Ok(BlobReference {
            sha256: hash,
            byte_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        })
    }

    pub fn get(&self, reference: &BlobReference) -> Result<Option<Vec<u8>>, PersistenceError> {
        if reference.sha256.len() != 64
            || !reference
                .sha256
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
        {
            return Err(PersistenceError::InvalidBlobHash);
        }
        let path = self
            .root
            .join(&reference.sha256[..2])
            .join(&reference.sha256[2..]);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(PersistenceError::File { path, source }),
        }
    }
}

fn validate_backup_policy(policy: BackupPolicy) -> Result<(), PersistenceError> {
    if policy.max_backups == 0 {
        return Err(PersistenceError::InvalidBackupPolicy);
    }
    Ok(())
}

fn list_backups_at(directory: &Path) -> Result<Vec<BackupInfo>, PersistenceError> {
    let mut backups = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(backups),
        Err(source) => {
            return Err(PersistenceError::File {
                path: directory.to_path_buf(),
                source,
            })
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| PersistenceError::File {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        // Never follow symlinks while enumerating recovery candidates. A
        // private directory is a defence-in-depth boundary, not a reason to
        // accept a path supplied by another process.
        let metadata = fs::symlink_metadata(&path).map_err(|source| PersistenceError::File {
            path: path.clone(),
            source,
        })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !metadata.file_type().is_file() || !is_valid_backup_file_name(file_name) {
            continue;
        }
        let modified = metadata
            .modified()
            .map_err(|source| PersistenceError::File {
                path: path.clone(),
                source,
            })?;
        backups.push(BackupInfo {
            path,
            byte_len: metadata.len(),
            created_at: DateTime::<Utc>::from(modified),
        });
    }
    backups.sort_by(|left, right| right.created_at.cmp(&left.created_at));
    Ok(backups)
}

fn recovery_backups_at(root: &Path) -> Result<Vec<RecoveryBackupInfo>, PersistenceError> {
    list_backups_at(&root.join("backups")).map(|backups| {
        backups
            .into_iter()
            .filter_map(|backup| {
                Some(RecoveryBackupInfo {
                    backup_file_name: backup.path.file_name()?.to_string_lossy().into_owned(),
                    byte_len: backup.byte_len,
                    created_at: backup.created_at,
                })
            })
            .collect()
    })
}

fn backup_storage_report(
    backups: &[BackupInfo],
    policy: BackupPolicy,
) -> Result<BackupStorageReport, PersistenceError> {
    validate_backup_policy(policy)?;
    let retained = retained_backups(backups, policy);
    let retained_bytes = retained
        .iter()
        .fold(0_u64, |total, backup| total.saturating_add(backup.byte_len));
    Ok(BackupStorageReport {
        policy,
        retained_count: retained.len(),
        retained_bytes,
        newest_backup_at: retained.first().map(|backup| backup.created_at),
        oldest_backup_at: retained.last().map(|backup| backup.created_at),
        exceeds_size_preference: policy
            .max_total_bytes
            .is_some_and(|maximum| retained_bytes > maximum),
    })
}

fn retained_backups(backups: &[BackupInfo], policy: BackupPolicy) -> Vec<BackupInfo> {
    let mut retained = Vec::new();
    let mut retained_bytes = 0_u64;
    for backup in backups.iter().take(policy.max_backups) {
        let exceeds_size = policy
            .max_total_bytes
            .is_some_and(|maximum| retained_bytes.saturating_add(backup.byte_len) > maximum);
        // Keep the newest snapshot even when it alone exceeds the preference.
        if exceeds_size && !retained.is_empty() {
            continue;
        }
        retained_bytes = retained_bytes.saturating_add(backup.byte_len);
        retained.push(backup.clone());
    }
    retained
}

fn is_valid_backup_file_name(file_name: &str) -> bool {
    let Some(stem) = file_name.strip_suffix(".sqlite") else {
        return false;
    };
    ["automatic-", "manual-", "pre-migration-"]
        .iter()
        .any(|prefix| stem.starts_with(prefix) && stem.len() > prefix.len())
        && stem
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn validate_backup_file_name(file_name: &str) -> Result<String, PersistenceError> {
    let path = Path::new(file_name);
    if path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(file_name)
        || !is_valid_backup_file_name(file_name)
    {
        return Err(PersistenceError::InvalidBackupFileName);
    }
    Ok(file_name.to_owned())
}

fn validate_database_for_open(path: &Path) -> Result<(), String> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(database_diagnostic)?;
    let report = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
        .map_err(database_diagnostic)?;
    if report.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(bounded_diagnostic(&report))
    }
}

fn database_diagnostic(error: rusqlite::Error) -> String {
    let known_corruption = matches!(
        &error,
        rusqlite::Error::SqliteFailure(details, _)
            if matches!(details.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    );
    if known_corruption {
        "SQLite reported corrupt or non-database content".into()
    } else {
        bounded_diagnostic(&error.to_string())
    }
}

fn bounded_diagnostic(value: &str) -> String {
    const LIMIT: usize = 512;
    let mut sanitized = value
        .chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .collect::<String>();
    if sanitized.len() > LIMIT {
        sanitized.truncate(LIMIT);
        sanitized.push('…');
    }
    sanitized
}

fn copy_validated_database(source_path: &Path, target_path: &Path) -> Result<(), PersistenceError> {
    let source = Connection::open_with_flags(
        source_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut target = Connection::open(target_path)?;
    set_private_file(target_path)?;
    let backup = Backup::new(&source, &mut target)?;
    backup.run_to_completion(64, Duration::from_millis(10), None)?;
    drop(backup);
    target
        .close()
        .map_err(|(_, error)| PersistenceError::Database(error))?;
    Ok(())
}

fn preserve_database_for_recovery(
    root: &Path,
    recovery_directory: &Path,
) -> Result<Option<String>, PersistenceError> {
    let database_path = root.join("state.sqlite");
    if !database_path.exists() {
        return Ok(None);
    }
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
    let preserved_name = format!("state.corrupt-{stamp}.sqlite");
    let preserved = recovery_directory.join(&preserved_name);
    fs::rename(&database_path, &preserved).map_err(|source| PersistenceError::File {
        path: database_path,
        source,
    })?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = root.join(format!("state.sqlite{suffix}"));
        if !sidecar.exists() {
            continue;
        }
        let target = recovery_directory.join(format!("{preserved_name}{suffix}"));
        fs::rename(&sidecar, &target).map_err(|source| PersistenceError::File {
            path: sidecar,
            source,
        })?;
    }
    Ok(Some(preserved_name))
}

fn backup_connection(
    connection: &Connection,
    directory: &Path,
    prefix: &str,
) -> Result<BackupInfo, PersistenceError> {
    create_private_directory(directory)?;
    let now = Utc::now();
    let path = directory.join(format!(
        "{prefix}-{}.sqlite",
        now.format("%Y%m%dT%H%M%S%.9fZ")
    ));
    let mut destination = Connection::open(&path)?;
    set_private_file(&path)?;
    let backup = Backup::new(connection, &mut destination)?;
    backup.run_to_completion(64, Duration::from_millis(10), None)?;
    drop(backup);
    drop(destination);
    let byte_len = fs::metadata(&path)
        .map_err(|source| PersistenceError::File {
            path: path.clone(),
            source,
        })?
        .len();
    Ok(BackupInfo {
        path,
        byte_len,
        created_at: now,
    })
}

fn migrate(connection: &mut Connection, from_version: i32) -> Result<(), PersistenceError> {
    let transaction = connection.transaction()?;
    if from_version < 1 {
        transaction.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS workspace (
              id TEXT PRIMARY KEY, display_name TEXT NOT NULL, source_json TEXT NOT NULL,
              default_base TEXT NOT NULL, pinned INTEGER NOT NULL, archived_at TEXT,
              created_at TEXT NOT NULL, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS workspace_source (
              workspace_id TEXT PRIMARY KEY REFERENCES workspace(id) ON DELETE CASCADE,
              source_kind TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS workspace_repository (
              id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspace(id) ON DELETE CASCADE,
              relative_path TEXT NOT NULL, enabled INTEGER NOT NULL, payload_json TEXT NOT NULL,
              UNIQUE(workspace_id, relative_path)
            );
            CREATE TABLE IF NOT EXISTS workspace_ui_state (
              workspace_id TEXT PRIMARY KEY REFERENCES workspace(id) ON DELETE CASCADE, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS review_session (
              id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspace(id) ON DELETE CASCADE,
              status TEXT NOT NULL, started_at TEXT NOT NULL, refreshed_at TEXT, archived_at TEXT,
              completed_at TEXT, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS repository_comparison (
              id TEXT PRIMARY KEY, repository_id TEXT NOT NULL REFERENCES workspace_repository(id) ON DELETE CASCADE,
              captured_at TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS review_file (
              id TEXT PRIMARY KEY, comparison_id TEXT NOT NULL REFERENCES repository_comparison(id) ON DELETE CASCADE,
              path TEXT NOT NULL, blob_hash TEXT, blob_byte_len INTEGER, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS review_snapshot (
              id TEXT PRIMARY KEY, comparison_id TEXT NOT NULL REFERENCES repository_comparison(id) ON DELETE CASCADE,
              blob_hash TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS annotation_set (
              id TEXT PRIMARY KEY, review_session_id TEXT NOT NULL REFERENCES review_session(id) ON DELETE CASCADE,
              sequence INTEGER NOT NULL, active INTEGER NOT NULL, archived_at TEXT, created_at TEXT NOT NULL,
              payload_json TEXT NOT NULL, UNIQUE(review_session_id, sequence)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_annotation_set_per_session
              ON annotation_set(review_session_id) WHERE active = 1;
            CREATE TABLE IF NOT EXISTS annotation (
              id TEXT PRIMARY KEY, annotation_set_id TEXT NOT NULL REFERENCES annotation_set(id) ON DELETE RESTRICT,
              kind TEXT NOT NULL, state TEXT NOT NULL, publication_state TEXT NOT NULL,
              created_at TEXT NOT NULL, updated_at TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS annotation_revision (
              id INTEGER PRIMARY KEY AUTOINCREMENT, annotation_id TEXT NOT NULL REFERENCES annotation(id) ON DELETE CASCADE,
              revised_at TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS prompt_export (
              id TEXT PRIMARY KEY, review_session_id TEXT NOT NULL REFERENCES review_session(id) ON DELETE CASCADE,
              annotation_set_id TEXT NOT NULL REFERENCES annotation_set(id) ON DELETE RESTRICT,
              created_at TEXT NOT NULL, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS github_pull_request (
              id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL REFERENCES workspace(id) ON DELETE CASCADE,
              payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS github_publication (
              id TEXT PRIMARY KEY, review_session_id TEXT NOT NULL REFERENCES review_session(id) ON DELETE CASCADE,
              publication_attempt_id TEXT NOT NULL UNIQUE, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS managed_repository_mirror (
              id TEXT PRIMARY KEY, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS managed_worktree (
              id TEXT PRIMARY KEY, workspace_id TEXT REFERENCES workspace(id) ON DELETE SET NULL,
              payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS retired_managed_worktree (
              id TEXT PRIMARY KEY, workspace_id TEXT REFERENCES workspace(id) ON DELETE SET NULL,
              payload_json TEXT NOT NULL, retired_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ssh_host_profile (
              id TEXT PRIMARY KEY, payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS application_setting (
              key TEXT PRIMARY KEY, value_json TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS review_file_blob_hash_idx ON review_file(blob_hash);
            ",
        )?;
        transaction.pragma_update(None, "user_version", 1)?;
    }
    if from_version < 2 {
        transaction.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS review_session_comparison (
              review_session_id TEXT NOT NULL REFERENCES review_session(id) ON DELETE CASCADE,
              comparison_id TEXT NOT NULL REFERENCES repository_comparison(id) ON DELETE CASCADE,
              PRIMARY KEY(review_session_id, comparison_id)
            );
            CREATE TABLE IF NOT EXISTS review_session_ui_state (
              review_session_id TEXT PRIMARY KEY REFERENCES review_session(id) ON DELETE CASCADE,
              payload_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS review_file_comparison_idx ON review_file(comparison_id);
            ",
        )?;
        transaction.pragma_update(None, "user_version", 2)?;
    }
    if from_version < 3 {
        transaction.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS review_session_current_comparison (
              review_session_id TEXT NOT NULL REFERENCES review_session(id) ON DELETE CASCADE,
              repository_id TEXT NOT NULL REFERENCES workspace_repository(id) ON DELETE CASCADE,
              comparison_id TEXT NOT NULL REFERENCES repository_comparison(id) ON DELETE CASCADE,
              PRIMARY KEY(review_session_id, repository_id)
            );
            ",
        )?;
        transaction.pragma_update(None, "user_version", 3)?;
    }
    if from_version == 0 {
        // Fresh databases receive the current table definition above, so no
        // ALTER is necessary; still advance directly to the latest version so
        // a reopen never mistakes the already-present column for an old DB.
        transaction.pragma_update(None, "user_version", LATEST_SCHEMA_VERSION)?;
    } else if from_version < 4 {
        // The unique attempt id is persisted before the outbound review POST.
        // This gives crash recovery a durable idempotency boundary even though
        // GitHub's REST endpoint itself does not offer an idempotency key.
        transaction.execute_batch(
            "
            ALTER TABLE github_publication ADD COLUMN publication_attempt_id TEXT;
            CREATE UNIQUE INDEX IF NOT EXISTS github_publication_attempt_unique
              ON github_publication(publication_attempt_id)
              WHERE publication_attempt_id IS NOT NULL;
            ",
        )?;
        transaction.pragma_update(None, "user_version", 4)?;
    }
    if from_version != 0 && from_version < 5 {
        // Keep the legacy JSON column for backward-compatible reads, but all
        // newly captured review documents use the content-addressed columns.
        transaction.execute_batch(
            "
            ALTER TABLE review_file ADD COLUMN blob_hash TEXT;
            ALTER TABLE review_file ADD COLUMN blob_byte_len INTEGER;
            CREATE INDEX IF NOT EXISTS review_file_blob_hash_idx ON review_file(blob_hash);
            ",
        )?;
        transaction.pragma_update(None, "user_version", 5)?;
    }
    if from_version != 0 && from_version < 6 {
        transaction.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS retired_managed_worktree (
              id TEXT PRIMARY KEY, workspace_id TEXT REFERENCES workspace(id) ON DELETE SET NULL,
              payload_json TEXT NOT NULL, retired_at TEXT NOT NULL
            );
            ",
        )?;
        transaction.pragma_update(None, "user_version", 6)?;
    }
    transaction.commit()?;
    Ok(())
}

fn insert_annotation_set(
    transaction: &Transaction<'_>,
    set: &AnnotationSet,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO annotation_set (id, review_session_id, sequence, active, archived_at, created_at, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            set.id.to_string(), set.review_session_id.to_string(), set.sequence, set.active, timestamp(set.archived_at),
            timestamp(Some(set.created_at)), to_json(set)?
        ],
    )?;
    Ok(())
}

fn upsert_review_session(
    transaction: &Transaction<'_>,
    session: &ReviewSession,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO review_session (id, workspace_id, status, started_at, refreshed_at, archived_at, completed_at, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET status=excluded.status, refreshed_at=excluded.refreshed_at,
           archived_at=excluded.archived_at, completed_at=excluded.completed_at, payload_json=excluded.payload_json",
        params![
            session.id.to_string(),
            session.workspace_id.to_string(),
            serde_json::to_string(&session.status)?,
            timestamp(Some(session.started_at)),
            timestamp(session.refreshed_at),
            timestamp(session.archived_at),
            timestamp(session.completed_at),
            to_json(session)?,
        ],
    )?;
    Ok(())
}

fn upsert_repository_in_transaction(
    transaction: &Transaction<'_>,
    repository: &Repository,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO workspace_repository (id, workspace_id, relative_path, enabled, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET relative_path=excluded.relative_path,
           enabled=excluded.enabled, payload_json=excluded.payload_json",
        params![
            repository.id.to_string(),
            repository.workspace_id.to_string(),
            repository.relative_path.as_str(),
            repository.enabled,
            to_json(repository)?,
        ],
    )?;
    Ok(())
}

fn upsert_workspace_in_transaction(
    transaction: &Transaction<'_>,
    workspace: &Workspace,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO workspace (id, display_name, source_json, default_base, pinned, archived_at, created_at, updated_at, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name,
           source_json=excluded.source_json, default_base=excluded.default_base,
           pinned=excluded.pinned, archived_at=excluded.archived_at,
           updated_at=excluded.updated_at, payload_json=excluded.payload_json",
        params![
            workspace.id.to_string(),
            workspace.display_name,
            to_json(&workspace.source)?,
            workspace.default_base.as_str(),
            workspace.pinned,
            timestamp(workspace.archived_at),
            timestamp(Some(workspace.created_at)),
            timestamp(Some(workspace.updated_at)),
            to_json(workspace)?,
        ],
    )?;
    Ok(())
}

fn archive_active_annotation_set_in_transaction(
    transaction: &Transaction<'_>,
    session_id: ReviewSessionId,
    at: DateTime<Utc>,
) -> Result<(), PersistenceError> {
    let active_sets = transaction
        .prepare(
            "SELECT payload_json FROM annotation_set
             WHERE review_session_id = ?1 AND active = 1 ORDER BY sequence DESC",
        )?
        .query_map(params![session_id.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for payload in active_sets {
        let mut set: AnnotationSet = from_json(&payload)?;
        set.active = false;
        set.archived_at = Some(at);
        transaction.execute(
            "UPDATE annotation_set SET active = 0, archived_at = ?1, payload_json = ?2 WHERE id = ?3",
            params![
                timestamp(set.archived_at),
                to_json(&set)?,
                set.id.to_string()
            ],
        )?;
    }
    Ok(())
}

fn upsert_application_setting(
    transaction: &Transaction<'_>,
    key: &str,
    value_json: &str,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO application_setting (key, value_json, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json, updated_at=excluded.updated_at",
        params![key, value_json, timestamp(Some(Utc::now()))],
    )?;
    Ok(())
}

fn insert_review_generation(
    transaction: &Transaction<'_>,
    session_id: ReviewSessionId,
    generation: &PreparedReviewGeneration,
) -> Result<(), PersistenceError> {
    let comparison = &generation.comparison;
    transaction.execute(
        "INSERT INTO repository_comparison (id, repository_id, captured_at, payload_json)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            comparison.id.to_string(),
            comparison.repository_id.to_string(),
            timestamp(Some(comparison.captured_at)),
            to_json(comparison)?,
        ],
    )?;
    transaction.execute(
        "INSERT INTO review_session_comparison (review_session_id, comparison_id) VALUES (?1, ?2)",
        params![session_id.to_string(), comparison.id.to_string()],
    )?;
    for file in &generation.files {
        transaction.execute(
            "INSERT INTO review_file (id, comparison_id, path, blob_hash, blob_byte_len, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file.storage_id,
                comparison.id.to_string(),
                file.path,
                file.blob.sha256,
                i64::try_from(file.blob.byte_len).unwrap_or(i64::MAX),
                BLOB_PAYLOAD_MARKER,
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO review_session_current_comparison (review_session_id, repository_id, comparison_id)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(review_session_id, repository_id) DO UPDATE SET comparison_id=excluded.comparison_id",
        params![
            session_id.to_string(),
            comparison.repository_id.to_string(),
            comparison.id.to_string(),
        ],
    )?;
    Ok(())
}

fn upsert_annotation(
    transaction: &Transaction<'_>,
    annotation: &Annotation,
) -> Result<(), PersistenceError> {
    transaction.execute(
        "INSERT INTO annotation (id, annotation_set_id, kind, state, publication_state, created_at, updated_at, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET annotation_set_id=excluded.annotation_set_id, kind=excluded.kind,
           state=excluded.state, publication_state=excluded.publication_state, updated_at=excluded.updated_at,
           payload_json=excluded.payload_json",
        params![
            annotation.id.to_string(),
            annotation.annotation_set_id.to_string(),
            serde_json::to_string(&annotation.kind)?,
            serde_json::to_string(&annotation.state)?,
            serde_json::to_string(&annotation.publication_state)?,
            timestamp(Some(annotation.created_at)),
            timestamp(Some(annotation.updated_at)),
            to_json(annotation)?,
        ],
    )?;
    transaction.execute(
        "INSERT INTO annotation_revision (annotation_id, revised_at, payload_json) VALUES (?1, ?2, ?3)",
        params![
            annotation.id.to_string(),
            timestamp(Some(annotation.updated_at)),
            to_json(annotation)?,
        ],
    )?;
    Ok(())
}

fn query_json_list<T: DeserializeOwned, P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    parameters: P,
) -> Result<Vec<T>, PersistenceError> {
    let mut statement = connection.prepare(query)?;
    let rows = statement.query_map(parameters, |row| row.get::<_, String>(0))?;
    rows.map(|row| {
        row.map_err(PersistenceError::from)
            .and_then(|payload| from_json(&payload))
    })
    .collect()
}

fn to_json<T: Serialize>(value: &T) -> Result<String, PersistenceError> {
    Ok(serde_json::to_string(value)?)
}

fn from_json<T: DeserializeOwned>(value: &str) -> Result<T, PersistenceError> {
    Ok(serde_json::from_str(value)?)
}

fn timestamp(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(|timestamp| timestamp.to_rfc3339())
}

fn create_private_directory(path: &Path) -> Result<(), PersistenceError> {
    fs::create_dir_all(path).map_err(|source| PersistenceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            PersistenceError::File {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> Result<(), PersistenceError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            PersistenceError::File {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use localreview_domain::{
        AnnotationId, AnnotationKind, AnnotationState, BaseReference, ComparisonId,
        ComparisonOptions, ContentFingerprint, GitSha, HeadState, PromptExportId, PromptScope,
        PublicationState, RepositoryId, StoredPath, WorkspaceSource,
    };
    use tempfile::TempDir;

    use super::*;

    fn workspace() -> Workspace {
        let now = Utc::now();
        Workspace {
            id: WorkspaceId::new(),
            display_name: "example".to_owned(),
            source: WorkspaceSource::LocalDirectory {
                root: StoredPath::from("/tmp/example"),
            },
            default_base: BaseReference::default(),
            pinned: false,
            archived_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn state_survives_reopen_and_clear_archives_annotations() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let now = Utc::now();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        store.save_review_session(&session).unwrap();
        let set = AnnotationSet {
            id: AnnotationSetId::new(),
            review_session_id: session.id,
            sequence: 1,
            active: true,
            archived_at: None,
            created_at: now,
        };
        store.save_annotation_set(&set).unwrap();
        let annotation = Annotation {
            id: AnnotationId::new(),
            annotation_set_id: set.id,
            kind: AnnotationKind::Comment,
            state: AnnotationState::Open,
            publication_state: PublicationState::LocalOnly,
            labels: vec![],
            body_markdown: "Keep this durable".to_owned(),
            anchor: None,
            created_at: now,
            updated_at: now,
        };
        store.save_annotation(&annotation).unwrap();
        let cleared = store.clear_active_annotation_set(session.id, now).unwrap();
        assert!(!cleared.archived.active);
        assert!(cleared.archived.archived_at.is_some());
        assert!(cleared.active.active);
        assert_eq!(
            store.annotations(cleared.archived.id).unwrap(),
            vec![annotation]
        );
        drop(store);
        let reopened = StateStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened.workspace(workspace.id).unwrap().unwrap(),
            workspace
        );
        assert_eq!(
            reopened.active_annotation_set(session.id).unwrap().unwrap(),
            cleared.active
        );
    }

    #[test]
    fn blob_storage_is_content_addressed_and_backup_is_readable() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let blob = store.blob_store();
        let first = blob.put(b"same source excerpt").unwrap();
        let second = blob.put(b"same source excerpt").unwrap();
        assert_eq!(first, second);
        assert_eq!(blob.get(&first).unwrap().unwrap(), b"same source excerpt");
        let backup = store.backup_now().unwrap();
        assert!(backup.path.exists());
        assert!(backup.byte_len > 0);
    }

    #[test]
    fn automatic_backup_is_due_bounded_and_integrity_is_source_free() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        store.upsert_workspace(&workspace()).unwrap();

        let first = store.backup_if_due(Duration::ZERO).unwrap().unwrap();
        assert!(first.path.exists());
        assert!(first
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("automatic-"));
        assert!(store
            .backup_if_due(Duration::from_secs(365 * 24 * 60 * 60))
            .unwrap()
            .is_none());

        let report = store.integrity_report().unwrap();
        assert!(report.healthy);
        assert_eq!(report.diagnostic, "ok");
        assert!(report.recoverable_backups.is_empty());
    }

    #[test]
    fn corrupt_database_requires_explicit_recovery_and_preserves_original_bytes() {
        let directory = TempDir::new().unwrap();
        let workspace = workspace();
        let store = StateStore::open(directory.path()).unwrap();
        store.upsert_workspace(&workspace).unwrap();
        let backup = store.backup_now().unwrap();
        let backup_file_name = backup
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        drop(store);

        let database_path = directory.path().join("state.sqlite");
        let corrupt = b"this is deliberately not a SQLite database";
        std::fs::write(&database_path, corrupt).unwrap();
        let startup = StateStore::open_for_startup(directory.path().to_path_buf()).unwrap();
        let StartupState::RequiresRecovery(report) = startup else {
            panic!("corrupt state must not be silently opened or reset");
        };
        assert!(report.database_present);
        assert_eq!(
            report
                .recoverable_backups
                .first()
                .map(|backup| backup.backup_file_name.as_str()),
            Some(backup_file_name.as_str())
        );
        assert_eq!(std::fs::read(&database_path).unwrap(), corrupt);

        let restored =
            StateStore::restore_from_backup(directory.path().to_path_buf(), &backup_file_name)
                .unwrap();
        assert_eq!(restored.restored_backup_file_name, backup_file_name);
        let preserved = restored.preserved_database_file_name.unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("recovery").join(preserved)).unwrap(),
            corrupt
        );
        let StartupState::Ready(reopened) =
            StateStore::open_for_startup(directory.path().to_path_buf()).unwrap()
        else {
            panic!("explicit validated restore must make the database ready");
        };
        assert_eq!(reopened.workspace(workspace.id).unwrap(), Some(workspace));
    }

    #[test]
    fn recovery_restore_rejects_traversal_unhealthy_backup_and_healthy_overwrite() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let backup = store.backup_now().unwrap();
        let backup_file_name = backup
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        drop(store);

        assert!(matches!(
            StateStore::restore_from_backup(directory.path().to_path_buf(), "../state.sqlite"),
            Err(PersistenceError::InvalidBackupFileName)
        ));
        assert!(matches!(
            StateStore::restore_from_backup(directory.path().to_path_buf(), &backup_file_name),
            Err(PersistenceError::RecoveryNotRequired)
        ));

        let database_path = directory.path().join("state.sqlite");
        std::fs::write(&database_path, b"not sqlite").unwrap();
        std::fs::write(&backup.path, b"not sqlite either").unwrap();
        assert!(matches!(
            StateStore::restore_from_backup(directory.path().to_path_buf(), &backup_file_name),
            Err(PersistenceError::InvalidRecoveryBackup { .. })
        ));
    }

    #[test]
    fn backup_policy_reports_size_and_rotates_only_when_explicitly_applied() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let first = store.backup_now().unwrap();
        let second = store.backup_now().unwrap();
        assert!(first.path.exists());
        assert!(second.path.exists());

        let policy = BackupPolicy {
            max_backups: 1,
            max_total_bytes: Some(1),
        };
        let before = store.backup_storage_report(policy).unwrap();
        assert_eq!(before.retained_count, 1);
        assert!(before.exceeds_size_preference);
        assert_eq!(store.list_backups().unwrap().len(), 2);
        let after = store.apply_backup_policy(policy).unwrap();
        assert_eq!(after.retained_count, 1);
        assert_eq!(store.list_backups().unwrap().len(), 1);
        assert!(matches!(
            store.backup_storage_report(BackupPolicy {
                max_backups: 0,
                max_total_bytes: None,
            }),
            Err(PersistenceError::InvalidBackupPolicy)
        ));
    }

    #[test]
    fn diagnostics_export_does_not_read_or_include_captured_source() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let diagnostics =
            String::from_utf8(store.diagnostics_json(BackupPolicy::default()).unwrap()).unwrap();
        assert!(diagnostics.contains("databaseHealthy"));
        assert!(!diagnostics.contains("/tmp/example"));
        assert!(!diagnostics.contains("payload_json"));
    }

    #[test]
    fn github_publication_attempt_migration_is_idempotent_for_fresh_and_legacy_databases() {
        for legacy_version in [1_i32, 2, 3] {
            let directory = TempDir::new().unwrap();
            let database = directory.path().join("state.sqlite");
            let connection = Connection::open(&database).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE github_publication (
                       id TEXT PRIMARY KEY,
                       review_session_id TEXT NOT NULL,
                       payload_json TEXT NOT NULL
                     );
                     -- The v2 migration adds an index over this existing v1
                     -- table; its unrelated payload shape is immaterial here.
                     CREATE TABLE review_file (
                       id TEXT PRIMARY KEY,
                       comparison_id TEXT NOT NULL,
                       path TEXT NOT NULL,
                       payload_json TEXT NOT NULL
                     );",
                )
                .unwrap();
            connection
                .pragma_update(None, "user_version", legacy_version)
                .unwrap();
            drop(connection);
            let store = StateStore::open(directory.path()).unwrap();
            drop(store);
            // Reopening is the regression: a successful first migration must
            // leave version 4 rather than trying to add the same column again.
            let reopened = StateStore::open(directory.path()).unwrap();
            drop(reopened);
            let connection = Connection::open(&database).unwrap();
            let version: i32 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(version, LATEST_SCHEMA_VERSION);
            let columns = connection
                .prepare("PRAGMA table_info(github_publication)")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(columns
                .iter()
                .any(|column| column == "publication_attempt_id"));
        }

        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        drop(store);
        let connection = Connection::open(directory.path().join("state.sqlite")).unwrap();
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, LATEST_SCHEMA_VERSION);
    }

    #[cfg(unix)]
    #[test]
    fn application_data_is_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        assert_eq!(
            fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.root().join("state.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let backup = store.backup_now().unwrap();
        assert_eq!(
            fs::metadata(backup.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn exports_are_records_and_do_not_affect_annotations() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let now = Utc::now();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        store.save_review_session(&session).unwrap();
        let set = AnnotationSet {
            id: AnnotationSetId::new(),
            review_session_id: session.id,
            sequence: 1,
            active: true,
            archived_at: None,
            created_at: now,
        };
        store.save_annotation_set(&set).unwrap();
        let export = PromptExportRecord {
            id: PromptExportId::new(),
            review_session_id: session.id,
            annotation_set_id: set.id,
            annotation_set_ids: vec![set.id],
            scope: PromptScope::AllActionable,
            annotation_ids: vec![],
            template_version: 1,
            rendered_markdown: Some("# Exact prompt\n".into()),
            title: Some("Review feedback".into()),
            annotation_count: Some(0),
            estimated_tokens: Some(4),
            created_at: now,
        };
        store.save_prompt_export(&export).unwrap();
        assert_eq!(
            store.active_annotation_set(session.id).unwrap().unwrap(),
            set
        );
    }

    #[test]
    fn remote_refresh_metadata_and_session_promotion_rollback_together() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let now = Utc::now();
        let repository = Repository {
            id: RepositoryId::new(),
            workspace_id: workspace.id,
            relative_path: StoredPath::from("repo"),
            worktree_path: StoredPath::from("/remote/repo"),
            git_common_dir: None,
            normalized_primary_remote: Some("origin".into()),
            enabled: true,
            base_override: None,
            current_branch: HeadState::Branch("feature".into()),
            last_resolved_base_sha: None,
            last_fetch_at: None,
            last_fetch_error: None,
            discovery_error: None,
            comparison_error: None,
        };
        store.upsert_repository(&repository).unwrap();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        store.save_review_session(&session).unwrap();
        store
            .set_setting("remote.workspace.test", r#"{"generation":1}"#)
            .unwrap();
        let mut promoted = session.clone();
        promoted.refreshed_at = Some(Utc::now());
        let mut captured_repository = repository.clone();
        captured_repository.last_resolved_base_sha =
            Some(GitSha::new("0123456789abcdef0123456789abcdef01234567").unwrap());
        captured_repository.discovery_error = Some("new capture warning".into());
        store.inject_next_atomic_commit_failure_for_test();
        assert!(matches!(
            store.save_prepared_remote_refresh_with_setting_and_repositories(
                &promoted,
                &[],
                &[captured_repository],
                "remote.workspace.test",
                r#"{"generation":2}"#,
            ),
            Err(PersistenceError::InjectedAtomicCommitFailure)
        ));
        assert_eq!(
            store.setting("remote.workspace.test").unwrap().as_deref(),
            Some(r#"{"generation":1}"#)
        );
        assert_eq!(
            store.review_sessions_for_id(session.id).unwrap().unwrap(),
            session
        );
        assert_eq!(
            store.repositories_for_id(repository.id).unwrap().unwrap(),
            repository
        );
    }

    #[test]
    fn generation_commit_keeps_historical_document_when_a_file_identity_survives_refresh() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let now = Utc::now();
        let repository = Repository {
            id: RepositoryId::new(),
            workspace_id: workspace.id,
            relative_path: StoredPath::from("repo"),
            worktree_path: StoredPath::from("/tmp/repo"),
            git_common_dir: None,
            normalized_primary_remote: None,
            enabled: true,
            base_override: None,
            current_branch: HeadState::Branch("feature".into()),
            last_resolved_base_sha: None,
            last_fetch_at: None,
            last_fetch_error: None,
            discovery_error: None,
            comparison_error: None,
        };
        store.upsert_repository(&repository).unwrap();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        store.save_review_session(&session).unwrap();
        let sha = GitSha::new("1234567890abcdef").unwrap();
        let comparison = |id| RepositoryComparison {
            id,
            repository_id: repository.id,
            requested_base: BaseReference::new("master").unwrap(),
            base_tip_sha: sha.clone(),
            merge_base_sha: sha.clone(),
            head_sha: Some(sha.clone()),
            head: HeadState::Branch("feature".into()),
            index_fingerprint: ContentFingerprint::from_bytes(b"index"),
            working_tree_fingerprint: ContentFingerprint::from_bytes(b"worktree"),
            untracked_files: vec![],
            options: ComparisonOptions::default(),
            captured_at: now,
        };
        let first = comparison(ComparisonId::new());
        let second = comparison(ComparisonId::new());
        let stable_file = "stable-file-id".to_owned();
        store
            .save_review_generation(
                session.id,
                &first,
                &[(
                    stable_file.clone(),
                    "src/lib.rs".into(),
                    "old payload".to_owned(),
                )],
            )
            .unwrap();
        store
            .save_review_generation(
                session.id,
                &second,
                &[(
                    stable_file.clone(),
                    "src/lib.rs".into(),
                    "new payload".to_owned(),
                )],
            )
            .unwrap();
        let first_payloads: Vec<String> = store
            .review_file_payloads_for_comparisons(&[first.id.to_string()])
            .unwrap();
        let second_payloads: Vec<String> = store
            .review_file_payloads_for_comparisons(&[second.id.to_string()])
            .unwrap();
        assert_eq!(first_payloads, vec!["old payload"]);
        assert_eq!(second_payloads, vec!["new payload"]);
        assert_eq!(
            store
                .review_file_payload_for_comparison::<String>(&first.id, &stable_file)
                .unwrap()
                .as_deref(),
            Some("old payload")
        );
        assert_eq!(
            store
                .review_file_payload_for_comparison::<String>(&second.id, &stable_file)
                .unwrap()
                .as_deref(),
            Some("new payload")
        );
        assert_eq!(
            store.current_comparisons_for_session(session.id).unwrap(),
            vec![second]
        );
    }

    #[test]
    fn review_documents_are_blob_backed_and_unreachable_prepared_blobs_are_recoverable() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let now = Utc::now();
        let repository = Repository {
            id: RepositoryId::new(),
            workspace_id: workspace.id,
            relative_path: StoredPath::from("repo"),
            worktree_path: StoredPath::from("/tmp/repo"),
            git_common_dir: None,
            normalized_primary_remote: None,
            enabled: true,
            base_override: None,
            current_branch: HeadState::Branch("feature".into()),
            last_resolved_base_sha: None,
            last_fetch_at: None,
            last_fetch_error: None,
            discovery_error: None,
            comparison_error: None,
        };
        store.upsert_repository(&repository).unwrap();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        store.save_review_session(&session).unwrap();
        let sha = GitSha::new("1234567890abcdef").unwrap();
        let comparison = RepositoryComparison {
            id: ComparisonId::new(),
            repository_id: repository.id,
            requested_base: BaseReference::new("master").unwrap(),
            base_tip_sha: sha.clone(),
            merge_base_sha: sha.clone(),
            head_sha: Some(sha),
            head: HeadState::Branch("feature".into()),
            index_fingerprint: ContentFingerprint::from_bytes(b"index"),
            working_tree_fingerprint: ContentFingerprint::from_bytes(b"worktree"),
            untracked_files: vec![],
            options: ComparisonOptions::default(),
            captured_at: now,
        };
        let large_payload = "canonical source\n".repeat(64 * 1024);
        store
            .save_review_generation(
                session.id,
                &comparison,
                &[("stable".into(), "src/lib.rs".into(), large_payload.clone())],
            )
            .unwrap();
        let database = Connection::open(store.root().join("state.sqlite")).unwrap();
        let (hash, byte_len, marker): (Option<String>, Option<i64>, String) = database
            .query_row(
                "SELECT blob_hash, blob_byte_len, payload_json FROM review_file",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let reference = BlobReference {
            sha256: hash.expect("metadata points at blob"),
            byte_len: u64::try_from(byte_len.unwrap()).unwrap(),
        };
        assert_eq!(marker, BLOB_PAYLOAD_MARKER);
        assert_eq!(
            store.blob_store().get(&reference).unwrap().unwrap(),
            serde_json::to_vec(&large_payload).unwrap()
        );
        assert_eq!(
            store
                .review_file_payloads_for_comparisons::<String>(&[comparison.id.to_string()])
                .unwrap(),
            vec![large_payload]
        );
        let orphan = store
            .blob_store()
            .put(b"unreachable prepared blob")
            .unwrap();
        assert!(store.blob_store().get(&orphan).unwrap().is_some());
        assert_eq!(store.gc_unreferenced_blobs().unwrap(), 1);
        assert!(store.blob_store().get(&orphan).unwrap().is_none());
        assert_eq!(
            store.blob_store().get(&reference).unwrap().unwrap(),
            serde_json::to_vec(&"canonical source\n".repeat(64 * 1024)).unwrap()
        );
        drop(database);
        drop(store);
        let reopened = StateStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .review_file_payloads_for_comparisons::<String>(&[comparison.id.to_string()])
                .unwrap(),
            vec!["canonical source\n".repeat(64 * 1024)]
        );
    }

    #[test]
    fn injected_generation_commit_failure_keeps_the_prior_current_generation_after_reopen() {
        let directory = TempDir::new().unwrap();
        let store = StateStore::open(directory.path()).unwrap();
        let workspace = workspace();
        store.upsert_workspace(&workspace).unwrap();
        let now = Utc::now();
        let repository = Repository {
            id: RepositoryId::new(),
            workspace_id: workspace.id,
            relative_path: StoredPath::from("repo"),
            worktree_path: StoredPath::from("/tmp/repo"),
            git_common_dir: None,
            normalized_primary_remote: None,
            enabled: true,
            base_override: None,
            current_branch: HeadState::Branch("feature".into()),
            last_resolved_base_sha: None,
            last_fetch_at: None,
            last_fetch_error: None,
            discovery_error: None,
            comparison_error: None,
        };
        store.upsert_repository(&repository).unwrap();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        store.save_review_session(&session).unwrap();
        let sha = GitSha::new("1234567890abcdef").unwrap();
        let comparison = |id| RepositoryComparison {
            id,
            repository_id: repository.id,
            requested_base: BaseReference::new("master").unwrap(),
            base_tip_sha: sha.clone(),
            merge_base_sha: sha.clone(),
            head_sha: Some(sha.clone()),
            head: HeadState::Branch("feature".into()),
            index_fingerprint: ContentFingerprint::from_bytes(b"index"),
            working_tree_fingerprint: ContentFingerprint::from_bytes(b"worktree"),
            untracked_files: vec![],
            options: ComparisonOptions::default(),
            captured_at: now,
        };
        let first = comparison(ComparisonId::new());
        store
            .save_review_generation(
                session.id,
                &first,
                &[("stable".into(), "src/lib.rs".into(), "old".to_owned())],
            )
            .unwrap();
        let second = comparison(ComparisonId::new());
        store.inject_next_atomic_commit_failure_for_test();
        assert!(matches!(
            store.save_review_generation(
                session.id,
                &second,
                &[("stable".into(), "src/lib.rs".into(), "new".to_owned())],
            ),
            Err(PersistenceError::InjectedAtomicCommitFailure)
        ));
        assert_eq!(
            store.current_comparisons_for_session(session.id).unwrap(),
            vec![first.clone()]
        );
        assert_eq!(
            store
                .review_file_payloads_for_comparisons::<String>(&[first.id.to_string()])
                .unwrap(),
            vec!["old"]
        );
        assert!(store
            .comparisons_for_session(session.id)
            .unwrap()
            .iter()
            .all(|comparison| comparison.id != second.id));
        drop(store);
        let reopened = StateStore::open(directory.path()).unwrap();
        assert_eq!(
            reopened
                .current_comparisons_for_session(session.id)
                .unwrap(),
            vec![first]
        );
    }

    #[test]
    fn v4_json_review_files_remain_readable_after_blob_metadata_migration() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("state.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE review_file (
                  id TEXT PRIMARY KEY, comparison_id TEXT NOT NULL, path TEXT NOT NULL,
                  payload_json TEXT NOT NULL
                );
                INSERT INTO review_file (id, comparison_id, path, payload_json)
                  VALUES ('comparison:file', 'comparison', 'src/lib.rs', '\"legacy payload\"');
                PRAGMA user_version = 4;
                ",
            )
            .unwrap();
        drop(connection);
        let store = StateStore::open(directory.path()).unwrap();
        assert_eq!(
            store
                .review_file_payloads_for_comparisons::<String>(&["comparison".into()])
                .unwrap(),
            vec!["legacy payload"]
        );
        let connection = Connection::open(&database).unwrap();
        let columns = connection
            .prepare("PRAGMA table_info(review_file)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "blob_hash"));
        assert!(columns.iter().any(|column| column == "blob_byte_len"));
    }
}
