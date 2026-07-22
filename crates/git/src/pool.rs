//! App-managed Git mirrors and detached review worktrees.
//!
//! The pool deliberately keeps review checkout lifecycle separate from normal
//! workspace capture.  It never changes a user's files: a known clone is used
//! only when it already contains the pinned commits; otherwise an app-owned
//! bare mirror is populated with exact commit refspecs.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use localreview_domain::{GitSha, StoredPath};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{GitCommand, GitError, GitExecutor, GitOutput, GitRepository, ProcessGitExecutor};

const REGISTRY_FILENAME: &str = "managed-worktrees.json";
const REGISTRY_VERSION: u32 = 1;
const POOL_LOCK_FILENAME: &str = ".managed-worktrees.lock";
const POOL_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const POOL_LOCK_STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// A validated repository identity used to derive a cache location.  Segments
/// are intentionally restricted so neither a PR URL nor server metadata can
/// escape the configured application directories.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepositoryLocator {
    pub host: String,
    pub owner: String,
    pub repository: String,
}

impl RepositoryLocator {
    pub fn new(
        host: impl Into<String>,
        owner: impl Into<String>,
        repository: impl Into<String>,
    ) -> Result<Self, RepositoryPoolError> {
        let locator = Self {
            host: host.into().to_ascii_lowercase(),
            owner: owner.into(),
            repository: repository.into(),
        };
        for (label, value) in [
            ("host", locator.host.as_str()),
            ("owner", locator.owner.as_str()),
            ("repository", locator.repository.as_str()),
        ] {
            if !safe_component(value) {
                return Err(RepositoryPoolError::InvalidLocatorComponent {
                    label,
                    value: value.to_owned(),
                });
            }
        }
        Ok(locator)
    }

    #[must_use]
    pub fn normalized_identity(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.repository)
    }
}

/// The base and head commits returned by GitHub at an explicit open or refresh
/// boundary. These values are never silently re-resolved by the pool.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedGitRevision {
    pub base_sha: GitSha,
    pub head_sha: GitSha,
}

#[derive(Clone, Debug)]
pub struct RepositoryPool {
    cache_root: PathBuf,
    application_data_root: PathBuf,
}

impl RepositoryPool {
    #[must_use]
    pub fn new(cache_root: impl Into<PathBuf>, application_data_root: impl Into<PathBuf>) -> Self {
        Self {
            cache_root: cache_root.into(),
            application_data_root: application_data_root.into(),
        }
    }

    #[must_use]
    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    #[must_use]
    pub fn application_data_root(&self) -> &Path {
        &self.application_data_root
    }

    #[must_use]
    pub fn registry(&self) -> WorktreeRegistry {
        WorktreeRegistry::new(&self.application_data_root)
    }

    #[must_use]
    pub fn mirror_path(&self, repository: &RepositoryLocator) -> PathBuf {
        self.cache_root
            .join("git-mirrors")
            .join(&repository.host)
            .join(&repository.owner)
            .join(format!("{}.git", repository.repository))
    }

    pub fn worktree_path(&self, review_id: &str) -> Result<PathBuf, RepositoryPoolError> {
        validate_review_id(review_id)?;
        Ok(self
            .application_data_root
            .join("reviews")
            .join(review_id)
            .join("worktree"))
    }

    /// Prepares an isolated, detached worktree at a caller-provided pinned
    /// revision. Calling this method again with the same review id and exact
    /// pins returns the existing review checkout; changing pins requires an
    /// explicit new review or lifecycle action from the caller.
    pub fn prepare(
        &self,
        request: PrepareManagedWorktreeRequest,
    ) -> Result<PreparedManagedWorktree, RepositoryPoolError> {
        let _lock = self.acquire_lock()?;
        validate_review_id(&request.review_id)?;
        validate_clone_url(&request.clone_url)?;
        let target = self.worktree_path(&request.review_id)?;
        let registry = self.registry();
        if let Some(existing) = registry.get(&request.review_id)? {
            if existing.repository == request.repository
                && existing.revision == request.revision
                && PathBuf::from(existing.worktree_path.as_str()).is_dir()
            {
                return Ok(PreparedManagedWorktree { record: existing });
            }
            return Err(RepositoryPoolError::ReviewIdAlreadyManaged {
                review_id: request.review_id,
            });
        }
        if target.exists() {
            return Err(RepositoryPoolError::WorktreePathAlreadyExists { path: target });
        }

        let source = self.select_source(&request)?;
        self.ensure_pinned_objects(&source, &request.clone_url, &request.revision)?;
        let review_directory =
            target
                .parent()
                .ok_or_else(|| RepositoryPoolError::UnsafeManagedPath {
                    path: target.clone(),
                })?;
        fs::create_dir_all(review_directory).map_err(|source_error| RepositoryPoolError::File {
            path: review_directory.to_path_buf(),
            source: source_error,
        })?;
        require_git(
            source.path(),
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                OsString::from("--quiet"),
                target.clone().into_os_string(),
                OsString::from(request.revision.head_sha.as_str()),
            ],
        )?;

        let record = ManagedWorktree {
            review_id: request.review_id,
            repository: request.repository,
            worktree_path: StoredPath::new(&target),
            source,
            revision: request.revision,
            created_at: Utc::now(),
        };
        registry.upsert(&record)?;
        Ok(PreparedManagedWorktree { record })
    }

    /// Returns whether Git detected modifications, staged content, or
    /// untracked files. A dirty app-managed checkout is never silently removed.
    pub fn is_dirty(&self, record: &ManagedWorktree) -> Result<bool, RepositoryPoolError> {
        let path = PathBuf::from(record.worktree_path.as_str());
        if !path.is_dir() {
            return Err(RepositoryPoolError::MissingManagedWorktree { path });
        }
        let output = run_git(
            &path,
            [
                OsString::from("status"),
                OsString::from("--porcelain=v1"),
                OsString::from("-z"),
                OsString::from("--untracked-files=all"),
            ],
        )?;
        if !output.success() {
            return Err(command_error(&path, &["status", "--porcelain=v1", "-z"], output).into());
        }
        Ok(!output.stdout.is_empty())
    }

    /// Removes a clean review worktree, prunes the owning Git metadata, then
    /// removes its registry entry. The shared mirror intentionally remains.
    pub fn delete(&self, review_id: &str) -> Result<DeletedManagedWorktree, RepositoryPoolError> {
        let _lock = self.acquire_lock()?;
        validate_review_id(review_id)?;
        let registry = self.registry();
        let record = registry.get(review_id)?.ok_or_else(|| {
            RepositoryPoolError::UnknownManagedWorktree {
                review_id: review_id.to_owned(),
            }
        })?;
        let path = PathBuf::from(record.worktree_path.as_str());
        self.ensure_managed_path(&path)?;
        let source_path = record.source.path().to_path_buf();
        if path.exists() {
            if self.is_dirty(&record)? {
                return Err(RepositoryPoolError::DirtyManagedWorktree { path });
            }
            require_git(
                &source_path,
                [
                    OsString::from("worktree"),
                    OsString::from("remove"),
                    path.clone().into_os_string(),
                ],
            )?;
        }
        if source_path.exists() {
            require_git(
                &source_path,
                [OsString::from("worktree"), OsString::from("prune")],
            )?;
        }
        registry.remove(review_id)?;
        remove_empty_review_directory(&path)?;
        let mirror_retained = matches!(record.source, RepositoryObjectSource::ManagedMirror { .. });
        Ok(DeletedManagedWorktree {
            record,
            mirror_retained,
        })
    }

    /// Inspects durable registrations against Git's own worktree metadata. It
    /// does not mutate either source, making it safe to run at application
    /// startup before presenting recovery choices.
    pub fn inspect_orphans(&self) -> Result<WorktreeOrphanReport, RepositoryPoolError> {
        let registry = self.registry();
        let records = registry.list()?;
        let mut missing_registrations = Vec::new();
        let mut source_paths = BTreeSet::new();
        let registered_paths = records
            .iter()
            .map(|record| comparable_path(Path::new(record.worktree_path.as_str())))
            .collect::<BTreeSet<_>>();
        for record in &records {
            let path = PathBuf::from(record.worktree_path.as_str());
            if !path.is_dir() {
                missing_registrations.push(record.clone());
            }
            source_paths.insert(record.source.path().to_path_buf());
        }
        source_paths.extend(self.managed_mirror_paths()?);

        let mut unregistered = Vec::new();
        for source_path in source_paths {
            if !source_path.exists() {
                continue;
            }
            for path in listed_worktrees(&source_path)? {
                if self.is_managed_path(&path)
                    && !registered_paths.contains(&comparable_path(&path))
                {
                    unregistered.push(UnregisteredManagedWorktree {
                        source_path: StoredPath::new(&source_path),
                        worktree_path: StoredPath::new(path),
                    });
                }
            }
        }
        unregistered.sort_by(|left, right| left.worktree_path.cmp(&right.worktree_path));
        Ok(WorktreeOrphanReport {
            missing_registrations,
            unregistered_worktrees: unregistered,
        })
    }

    /// Repairs only recoverable orphan records. Missing registered paths are
    /// pruned and deregistered. Unregistered paths are removed only when clean;
    /// dirty paths remain in the report for a human decision.
    pub fn repair_orphans(&self) -> Result<WorktreeRepairReport, RepositoryPoolError> {
        let _lock = self.acquire_lock()?;
        let report = self.inspect_orphans()?;
        let registry = self.registry();
        let mut repaired_missing_registrations = Vec::new();
        for record in report.missing_registrations {
            let source = record.source.path();
            if source.exists() {
                require_git(
                    source,
                    [OsString::from("worktree"), OsString::from("prune")],
                )?;
            }
            registry.remove(&record.review_id)?;
            repaired_missing_registrations.push(record.review_id);
        }

        let mut removed_unregistered = Vec::new();
        let mut dirty_unregistered = Vec::new();
        for orphan in report.unregistered_worktrees {
            let path = PathBuf::from(orphan.worktree_path.as_str());
            let dirty = if path.is_dir() {
                let status = run_git(
                    &path,
                    [
                        OsString::from("status"),
                        OsString::from("--porcelain=v1"),
                        OsString::from("-z"),
                        OsString::from("--untracked-files=all"),
                    ],
                )?;
                !status.success() || !status.stdout.is_empty()
            } else {
                false
            };
            if dirty {
                dirty_unregistered.push(orphan);
                continue;
            }
            let source = PathBuf::from(orphan.source_path.as_str());
            require_git(
                &source,
                [
                    OsString::from("worktree"),
                    OsString::from("remove"),
                    path.into_os_string(),
                ],
            )?;
            require_git(
                &source,
                [OsString::from("worktree"), OsString::from("prune")],
            )?;
            removed_unregistered.push(orphan);
        }
        Ok(WorktreeRepairReport {
            repaired_missing_registrations,
            removed_unregistered,
            dirty_unregistered,
        })
    }

    /// Removes an unused app-owned mirror. Known clones are never candidates,
    /// and the method refuses to remove a mirror referenced by an active record.
    pub fn remove_unused_mirror(
        &self,
        repository: &RepositoryLocator,
    ) -> Result<(), RepositoryPoolError> {
        let _lock = self.acquire_lock()?;
        let mirror = self.mirror_path(repository);
        self.ensure_mirror_path(&mirror)?;
        if self
            .registry()
            .list()?
            .iter()
            .any(|record| record.source.path() == mirror)
        {
            return Err(RepositoryPoolError::MirrorInUse { path: mirror });
        }
        if !mirror.exists() {
            return Ok(());
        }
        // A bare repository reports itself as the first entry of
        // `git worktree list --porcelain`. Only linked worktrees make the
        // mirror busy.
        let comparable_mirror = comparable_path(&mirror);
        let managed_worktrees = listed_worktrees(&mirror)?
            .into_iter()
            .filter(|path| comparable_path(path) != comparable_mirror)
            .collect::<Vec<_>>();
        if !managed_worktrees.is_empty() {
            return Err(RepositoryPoolError::MirrorHasWorktrees {
                path: mirror,
                worktrees: managed_worktrees,
            });
        }
        fs::remove_dir_all(&mirror).map_err(|source| RepositoryPoolError::File {
            path: mirror,
            source,
        })
    }

    fn select_source(
        &self,
        request: &PrepareManagedWorktreeRequest,
    ) -> Result<RepositoryObjectSource, RepositoryPoolError> {
        for candidate in &request.known_clones {
            if self.known_clone_is_usable(candidate, &request.clone_url, &request.revision)? {
                return Ok(RepositoryObjectSource::KnownClone {
                    path: StoredPath::new(candidate),
                });
            }
        }
        let mirror = self.ensure_mirror(&request.repository, &request.clone_url)?;
        Ok(RepositoryObjectSource::ManagedMirror {
            path: StoredPath::new(mirror),
        })
    }

    fn acquire_lock(&self) -> Result<PoolLock, RepositoryPoolError> {
        fs::create_dir_all(&self.application_data_root).map_err(|source| {
            RepositoryPoolError::File {
                path: self.application_data_root.clone(),
                source,
            }
        })?;
        let path = self.application_data_root.join(POOL_LOCK_FILENAME);
        let deadline = Instant::now() + POOL_LOCK_TIMEOUT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "pid={}", std::process::id());
                    return Ok(PoolLock { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > POOL_LOCK_STALE_AFTER);
                    if stale {
                        // The lock contains no data and only guards our own
                        // app paths. A crashed process must not leave the PR
                        // lifecycle permanently unavailable.
                        match fs::remove_file(&path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(source) => {
                                return Err(RepositoryPoolError::File {
                                    path: path.clone(),
                                    source,
                                });
                            }
                        }
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(RepositoryPoolError::LockTimeout { path });
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(source) => {
                    return Err(RepositoryPoolError::File {
                        path: path.clone(),
                        source,
                    });
                }
            }
        }
    }

    fn known_clone_is_usable(
        &self,
        candidate: &Path,
        clone_url: &str,
        revision: &PinnedGitRevision,
    ) -> Result<bool, RepositoryPoolError> {
        if !candidate.is_dir() {
            return Ok(false);
        }
        let identity = match GitRepository::open(candidate).inspect() {
            Ok(identity) => identity,
            Err(_) => return Ok(false),
        };
        let Some(remote) = identity.primary_remote else {
            return Ok(false);
        };
        if !equivalent_remotes(&remote, clone_url) {
            return Ok(false);
        }
        let bare = run_git(
            candidate,
            [
                OsString::from("rev-parse"),
                OsString::from("--is-bare-repository"),
            ],
        )?;
        if !bare.success() || bare.stdout_trimmed() != "false" {
            return Ok(false);
        }
        let git_dir = run_git(
            candidate,
            [
                OsString::from("rev-parse"),
                OsString::from("--path-format=absolute"),
                OsString::from("--git-dir"),
            ],
        )?;
        let common_dir = run_git(
            candidate,
            [
                OsString::from("rev-parse"),
                OsString::from("--path-format=absolute"),
                OsString::from("--git-common-dir"),
            ],
        )?;
        if !git_dir.success()
            || !common_dir.success()
            || git_dir.stdout_trimmed() != common_dir.stdout_trimmed()
        {
            return Ok(false);
        }
        Ok(
            has_commit(candidate, &revision.base_sha)?
                && has_commit(candidate, &revision.head_sha)?,
        )
    }

    fn ensure_mirror(
        &self,
        repository: &RepositoryLocator,
        clone_url: &str,
    ) -> Result<PathBuf, RepositoryPoolError> {
        let mirror = self.mirror_path(repository);
        self.ensure_mirror_path(&mirror)?;
        if mirror.exists() {
            let bare = run_git(
                &mirror,
                [
                    OsString::from("rev-parse"),
                    OsString::from("--is-bare-repository"),
                ],
            )?;
            if !bare.success() || bare.stdout_trimmed() != "true" {
                return Err(RepositoryPoolError::UnusableMirror { path: mirror });
            }
            let remote = require_git(
                &mirror,
                [
                    OsString::from("remote"),
                    OsString::from("get-url"),
                    OsString::from("origin"),
                ],
            )?
            .stdout_trimmed();
            if !equivalent_remotes(&remote, clone_url) {
                return Err(RepositoryPoolError::MirrorRemoteMismatch { path: mirror });
            }
            return Ok(mirror);
        }
        let parent = mirror
            .parent()
            .ok_or_else(|| RepositoryPoolError::UnsafeMirrorPath {
                path: mirror.clone(),
            })?;
        fs::create_dir_all(parent).map_err(|source| RepositoryPoolError::File {
            path: parent.to_path_buf(),
            source,
        })?;
        require_git(
            parent,
            [
                OsString::from("init"),
                OsString::from("--bare"),
                mirror.clone().into_os_string(),
            ],
        )?;
        require_git(
            &mirror,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                OsString::from(clone_url),
            ],
        )?;
        Ok(mirror)
    }

    fn ensure_pinned_objects(
        &self,
        source: &RepositoryObjectSource,
        clone_url: &str,
        revision: &PinnedGitRevision,
    ) -> Result<(), RepositoryPoolError> {
        let path = source.path();
        if has_commit(path, &revision.base_sha)? && has_commit(path, &revision.head_sha)? {
            return Ok(());
        }
        if matches!(source, RepositoryObjectSource::KnownClone { .. }) {
            // Do not fetch into a user-owned clone. Source selection only
            // chooses it if both pinned commits already exist.
            return Err(RepositoryPoolError::KnownCloneMissingPinnedCommit {
                path: path.to_path_buf(),
            });
        }
        let mut arguments = vec![
            OsString::from("fetch"),
            OsString::from("--no-tags"),
            OsString::from("origin"),
        ];
        let base_ref = format!(
            "{}:refs/localreview/base/{}",
            revision.base_sha.as_str(),
            revision.base_sha.as_str()
        );
        arguments.push(OsString::from(base_ref));
        if revision.head_sha != revision.base_sha {
            arguments.push(OsString::from(format!(
                "{}:refs/localreview/head/{}",
                revision.head_sha.as_str(),
                revision.head_sha.as_str()
            )));
        }
        require_git(path, arguments)?;
        if has_commit(path, &revision.base_sha)? && has_commit(path, &revision.head_sha)? {
            Ok(())
        } else {
            Err(RepositoryPoolError::PinnedCommitUnavailable {
                source_path: path.to_path_buf(),
                base_sha: revision.base_sha.clone(),
                head_sha: revision.head_sha.clone(),
                clone_url: clone_url.to_owned(),
            })
        }
    }

    fn is_managed_path(&self, path: &Path) -> bool {
        let reviews_root = self.application_data_root.join("reviews");
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return false;
        }
        match (fs::canonicalize(path), fs::canonicalize(&reviews_root)) {
            (Ok(canonical_path), Ok(canonical_root)) => canonical_path.starts_with(canonical_root),
            // New app-generated worktree paths do not exist yet. Their
            // components are separately validated before creation, so a
            // lexical check is appropriate only while canonicalization is
            // impossible. Existing deletion targets always take the branch
            // above, which rejects symlink escapes.
            _ => path.starts_with(&reviews_root),
        }
    }

    fn ensure_managed_path(&self, path: &Path) -> Result<(), RepositoryPoolError> {
        if self.is_managed_path(path) {
            Ok(())
        } else {
            Err(RepositoryPoolError::UnsafeManagedPath {
                path: path.to_path_buf(),
            })
        }
    }

    fn ensure_mirror_path(&self, path: &Path) -> Result<(), RepositoryPoolError> {
        if path.starts_with(self.cache_root.join("git-mirrors")) {
            Ok(())
        } else {
            Err(RepositoryPoolError::UnsafeMirrorPath {
                path: path.to_path_buf(),
            })
        }
    }

    fn managed_mirror_paths(&self) -> Result<Vec<PathBuf>, RepositoryPoolError> {
        let root = self.cache_root.join("git-mirrors");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut mirrors = Vec::new();
        for host in fs::read_dir(&root).map_err(|source| RepositoryPoolError::File {
            path: root.clone(),
            source,
        })? {
            let host = host.map_err(|source| RepositoryPoolError::File {
                path: root.clone(),
                source,
            })?;
            if !host
                .file_type()
                .map_err(|source| RepositoryPoolError::File {
                    path: host.path(),
                    source,
                })?
                .is_dir()
            {
                continue;
            }
            for owner in fs::read_dir(host.path()).map_err(|source| RepositoryPoolError::File {
                path: host.path(),
                source,
            })? {
                let owner = owner.map_err(|source| RepositoryPoolError::File {
                    path: host.path(),
                    source,
                })?;
                if !owner
                    .file_type()
                    .map_err(|source| RepositoryPoolError::File {
                        path: owner.path(),
                        source,
                    })?
                    .is_dir()
                {
                    continue;
                }
                for mirror in
                    fs::read_dir(owner.path()).map_err(|source| RepositoryPoolError::File {
                        path: owner.path(),
                        source,
                    })?
                {
                    let mirror = mirror.map_err(|source| RepositoryPoolError::File {
                        path: owner.path(),
                        source,
                    })?;
                    if mirror
                        .file_type()
                        .map_err(|source| RepositoryPoolError::File {
                            path: mirror.path(),
                            source,
                        })?
                        .is_dir()
                        && mirror.file_name().to_string_lossy().ends_with(".git")
                    {
                        mirrors.push(mirror.path());
                    }
                }
            }
        }
        mirrors.sort();
        Ok(mirrors)
    }
}

#[derive(Clone, Debug)]
pub struct PrepareManagedWorktreeRequest {
    pub review_id: String,
    pub repository: RepositoryLocator,
    /// Normally `https://github.com/<owner>/<repo>.git`; local paths are also
    /// accepted for hermetic tests and future enterprise-compatible transports.
    pub clone_url: String,
    pub revision: PinnedGitRevision,
    /// Candidates supplied by the service's known-workspace registry.
    pub known_clones: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedManagedWorktree {
    pub record: ManagedWorktree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletedManagedWorktree {
    pub record: ManagedWorktree,
    pub mirror_retained: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepositoryObjectSource {
    KnownClone { path: StoredPath },
    ManagedMirror { path: StoredPath },
}

impl RepositoryObjectSource {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::KnownClone { path } | Self::ManagedMirror { path } => Path::new(path.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedWorktree {
    pub review_id: String,
    pub repository: RepositoryLocator,
    pub worktree_path: StoredPath,
    pub source: RepositoryObjectSource,
    pub revision: PinnedGitRevision,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnregisteredManagedWorktree {
    pub source_path: StoredPath,
    pub worktree_path: StoredPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeOrphanReport {
    pub missing_registrations: Vec<ManagedWorktree>,
    pub unregistered_worktrees: Vec<UnregisteredManagedWorktree>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRepairReport {
    pub repaired_missing_registrations: Vec<String>,
    pub removed_unregistered: Vec<UnregisteredManagedWorktree>,
    pub dirty_unregistered: Vec<UnregisteredManagedWorktree>,
}

#[derive(Debug)]
struct PoolLock {
    path: PathBuf,
}

impl Drop for PoolLock {
    fn drop(&mut self) {
        // If another process removed a stale lock after this process was
        // suspended, failing closed here is impossible. The acquisition
        // timeout plus short critical sections keep that edge observable
        // without risking an unrelated deletion.
        let _ = fs::remove_file(&self.path);
    }
}

/// A small, durable metadata API. Persistence integration can mirror these
/// records into SQLite later; keeping the registry file adjacent to application
/// data lets startup repair work before higher-level services are available.
#[derive(Clone, Debug)]
pub struct WorktreeRegistry {
    path: PathBuf,
}

impl WorktreeRegistry {
    #[must_use]
    pub fn new(application_data_root: impl AsRef<Path>) -> Self {
        Self {
            path: application_data_root.as_ref().join(REGISTRY_FILENAME),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Result<Vec<ManagedWorktree>, RepositoryPoolError> {
        Ok(self.read()?.worktrees)
    }

    pub fn get(&self, review_id: &str) -> Result<Option<ManagedWorktree>, RepositoryPoolError> {
        Ok(self
            .read()?
            .worktrees
            .into_iter()
            .find(|record| record.review_id == review_id))
    }

    pub fn upsert(&self, record: &ManagedWorktree) -> Result<(), RepositoryPoolError> {
        let mut state = self.read()?;
        if let Some(existing) = state
            .worktrees
            .iter_mut()
            .find(|existing| existing.review_id == record.review_id)
        {
            *existing = record.clone();
        } else {
            state.worktrees.push(record.clone());
        }
        state
            .worktrees
            .sort_by(|left, right| left.review_id.cmp(&right.review_id));
        self.write(&state)
    }

    pub fn remove(&self, review_id: &str) -> Result<Option<ManagedWorktree>, RepositoryPoolError> {
        let mut state = self.read()?;
        let index = state
            .worktrees
            .iter()
            .position(|record| record.review_id == review_id);
        let removed = index.map(|index| state.worktrees.remove(index));
        if removed.is_some() {
            self.write(&state)?;
        }
        Ok(removed)
    }

    fn read(&self) -> Result<RegistryFile, RepositoryPoolError> {
        if !self.path.exists() {
            return Ok(RegistryFile::default());
        }
        let contents = fs::read(&self.path).map_err(|source| RepositoryPoolError::File {
            path: self.path.clone(),
            source,
        })?;
        let state = serde_json::from_slice::<RegistryFile>(&contents).map_err(|source| {
            RepositoryPoolError::RegistryDecode {
                path: self.path.clone(),
                source,
            }
        })?;
        if state.version != REGISTRY_VERSION {
            return Err(RepositoryPoolError::UnsupportedRegistryVersion {
                path: self.path.clone(),
                version: state.version,
            });
        }
        Ok(state)
    }

    fn write(&self, state: &RegistryFile) -> Result<(), RepositoryPoolError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| RepositoryPoolError::File {
                path: self.path.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "registry has no parent"),
            })?;
        fs::create_dir_all(parent).map_err(|source| RepositoryPoolError::File {
            path: parent.to_path_buf(),
            source,
        })?;
        let bytes =
            serde_json::to_vec_pretty(state).map_err(RepositoryPoolError::RegistryEncode)?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, bytes).map_err(|source| RepositoryPoolError::File {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| RepositoryPoolError::File {
            path: self.path.clone(),
            source,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default = "registry_version")]
    version: u32,
    #[serde(default)]
    worktrees: Vec<ManagedWorktree>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            worktrees: Vec::new(),
        }
    }
}

fn registry_version() -> u32 {
    REGISTRY_VERSION
}

#[derive(Debug, Error)]
pub enum RepositoryPoolError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("could not access {path}: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid {label} component {value:?}")]
    InvalidLocatorComponent { label: &'static str, value: String },
    #[error("invalid managed review id {review_id:?}")]
    InvalidReviewId { review_id: String },
    #[error("invalid clone URL")]
    InvalidCloneUrl,
    #[error("review id {review_id} is already managed with a different checkout")]
    ReviewIdAlreadyManaged { review_id: String },
    #[error("managed worktree path already exists: {path}")]
    WorktreePathAlreadyExists { path: PathBuf },
    #[error("managed worktree is dirty and was not removed: {path}")]
    DirtyManagedWorktree { path: PathBuf },
    #[error("managed worktree is missing: {path}")]
    MissingManagedWorktree { path: PathBuf },
    #[error("unknown managed worktree review {review_id}")]
    UnknownManagedWorktree { review_id: String },
    #[error("refusing an operation outside the managed review root: {path}")]
    UnsafeManagedPath { path: PathBuf },
    #[error("refusing an operation outside the managed mirror root: {path}")]
    UnsafeMirrorPath { path: PathBuf },
    #[error("existing mirror is not a usable bare repository: {path}")]
    UnusableMirror { path: PathBuf },
    #[error("existing mirror has a different origin remote: {path}")]
    MirrorRemoteMismatch { path: PathBuf },
    #[error("known clone unexpectedly lacks a pinned commit: {path}")]
    KnownCloneMissingPinnedCommit { path: PathBuf },
    #[error(
        "pinned commits are unavailable from {clone_url} in {source_path}: {base_sha} / {head_sha}"
    )]
    PinnedCommitUnavailable {
        source_path: PathBuf,
        base_sha: GitSha,
        head_sha: GitSha,
        clone_url: String,
    },
    #[error("mirror still has active managed worktrees: {path}")]
    MirrorInUse { path: PathBuf },
    #[error("mirror has Git worktrees that must be handled first: {path}")]
    MirrorHasWorktrees {
        path: PathBuf,
        worktrees: Vec<PathBuf>,
    },
    #[error("could not decode worktree registry {path}: {source}")]
    RegistryDecode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not encode worktree registry: {0}")]
    RegistryEncode(#[source] serde_json::Error),
    #[error("unsupported worktree registry version {version} at {path}")]
    UnsupportedRegistryVersion { path: PathBuf, version: u32 },
    #[error("timed out waiting for the managed-worktree lifecycle lock: {path}")]
    LockTimeout { path: PathBuf },
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_review_id(review_id: &str) -> Result<(), RepositoryPoolError> {
    if safe_component(review_id) {
        Ok(())
    } else {
        Err(RepositoryPoolError::InvalidReviewId {
            review_id: review_id.to_owned(),
        })
    }
}

fn validate_clone_url(clone_url: &str) -> Result<(), RepositoryPoolError> {
    if clone_url.is_empty() || clone_url.len() > 4_096 || clone_url.contains('\0') {
        Err(RepositoryPoolError::InvalidCloneUrl)
    } else {
        Ok(())
    }
}

fn equivalent_remotes(left: &str, right: &str) -> bool {
    let left_normalized = crate::normalize_remote_url(left);
    let right_normalized = crate::normalize_remote_url(right);
    left_normalized == right_normalized || left.trim_end_matches('/') == right.trim_end_matches('/')
}

fn comparable_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn run_git(
    working_directory: &Path,
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<GitOutput, GitError> {
    let command = GitCommand::new(working_directory, arguments);
    ProcessGitExecutor.execute(&command)
}

fn require_git(
    working_directory: &Path,
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<GitOutput, GitError> {
    let command = GitCommand::new(working_directory, arguments);
    let display = command.display();
    let output = ProcessGitExecutor.execute(&command)?;
    if output.success() {
        Ok(output)
    } else {
        Err(GitError::CommandFailed {
            command: display,
            stderr: output.stderr_trimmed(),
        })
    }
}

fn command_error(working_directory: &Path, arguments: &[&str], output: GitOutput) -> GitError {
    GitError::CommandFailed {
        command: GitCommand::new(working_directory, arguments.iter().copied()).display(),
        stderr: output.stderr_trimmed(),
    }
}

fn has_commit(path: &Path, sha: &GitSha) -> Result<bool, GitError> {
    let output = run_git(
        path,
        [
            OsString::from("cat-file"),
            OsString::from("-e"),
            OsString::from(format!("{}^{{commit}}", sha.as_str())),
        ],
    )?;
    Ok(output.success())
}

fn listed_worktrees(source: &Path) -> Result<Vec<PathBuf>, RepositoryPoolError> {
    let output = require_git(
        source,
        [
            OsString::from("worktree"),
            OsString::from("list"),
            OsString::from("--porcelain"),
        ],
    )?;
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            paths.push(PathBuf::from(path));
        }
    }
    Ok(paths)
}

fn remove_empty_review_directory(worktree_path: &Path) -> Result<(), RepositoryPoolError> {
    let Some(review_directory) = worktree_path.parent() else {
        return Ok(());
    };
    if review_directory.is_dir()
        && fs::read_dir(review_directory)
            .map_err(|source| RepositoryPoolError::File {
                path: review_directory.to_path_buf(),
                source,
            })?
            .next()
            .is_none()
    {
        fs::remove_dir(review_directory).map_err(|source| RepositoryPoolError::File {
            path: review_directory.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        process::Command,
        sync::{Arc, Barrier},
        thread,
    };

    use tempfile::TempDir;

    use super::*;

    fn git(path: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    struct Fixture {
        _temporary: TempDir,
        remote: PathBuf,
        known_clone: PathBuf,
        pool: RepositoryPool,
        revision: PinnedGitRevision,
        locator: RepositoryLocator,
    }

    fn fixture() -> Fixture {
        let temporary = TempDir::new().unwrap();
        let remote = temporary.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare"]);
        let seed = temporary.path().join("seed");
        fs::create_dir(&seed).unwrap();
        git(&seed, &["init", "-b", "main"]);
        git(&seed, &["config", "user.email", "test@example.invalid"]);
        git(&seed, &["config", "user.name", "Test User"]);
        fs::write(seed.join("review.txt"), "base\n").unwrap();
        git(&seed, &["add", "review.txt"]);
        git(&seed, &["commit", "-m", "base"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "origin", "main"]);
        let base_sha = GitSha::new(git(&seed, &["rev-parse", "HEAD"]).trim()).unwrap();
        git(&seed, &["switch", "-c", "feature"]);
        fs::write(seed.join("review.txt"), "head\n").unwrap();
        git(&seed, &["commit", "-am", "head"]);
        git(&seed, &["push", "origin", "feature"]);
        let head_sha = GitSha::new(git(&seed, &["rev-parse", "HEAD"]).trim()).unwrap();
        let known_clone = temporary.path().join("known");
        git(
            temporary.path(),
            &[
                "clone",
                remote.to_str().unwrap(),
                known_clone.to_str().unwrap(),
            ],
        );
        // `git clone` checked out the remote's HEAD (unset for this fixture),
        // so explicitly materialize the branch in the known clone.
        git(&known_clone, &["switch", "--detach", head_sha.as_str()]);
        Fixture {
            remote,
            known_clone,
            pool: RepositoryPool::new(
                temporary.path().join("cache"),
                temporary.path().join("data"),
            ),
            revision: PinnedGitRevision { base_sha, head_sha },
            locator: RepositoryLocator::new("github.com", "octo", "review").unwrap(),
            _temporary: temporary,
        }
    }

    fn request(fixture: &Fixture, review_id: &str) -> PrepareManagedWorktreeRequest {
        PrepareManagedWorktreeRequest {
            review_id: review_id.to_owned(),
            repository: fixture.locator.clone(),
            clone_url: fixture.remote.to_string_lossy().into_owned(),
            revision: fixture.revision.clone(),
            known_clones: Vec::new(),
        }
    }

    #[test]
    fn falls_back_to_a_bare_mirror_and_pins_a_detached_worktree() {
        let fixture = fixture();
        let prepared = fixture.pool.prepare(request(&fixture, "review-1")).unwrap();
        assert!(matches!(
            prepared.record.source,
            RepositoryObjectSource::ManagedMirror { .. }
        ));
        let worktree = PathBuf::from(prepared.record.worktree_path.as_str());
        assert_eq!(
            git(&worktree, &["rev-parse", "HEAD"]).trim(),
            fixture.revision.head_sha.as_str()
        );
        assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
        assert_eq!(
            fixture.pool.registry().list().unwrap(),
            vec![prepared.record.clone()]
        );

        let deleted = fixture.pool.delete("review-1").unwrap();
        assert!(deleted.mirror_retained);
        assert!(!worktree.exists());
        assert!(fixture.pool.mirror_path(&fixture.locator).exists());
    }

    #[test]
    fn reuses_a_healthy_known_clone_without_creating_a_mirror() {
        let fixture = fixture();
        let mut request = request(&fixture, "review-known");
        request.known_clones.push(fixture.known_clone.clone());
        let prepared = fixture.pool.prepare(request).unwrap();
        assert!(matches!(
            prepared.record.source,
            RepositoryObjectSource::KnownClone { .. }
        ));
        assert!(!fixture.pool.mirror_path(&fixture.locator).exists());
        fixture.pool.delete("review-known").unwrap();
    }

    #[test]
    fn dirty_worktrees_refuse_deletion_until_cleaned() {
        let fixture = fixture();
        let prepared = fixture
            .pool
            .prepare(request(&fixture, "review-dirty"))
            .unwrap();
        let worktree = PathBuf::from(prepared.record.worktree_path.as_str());
        fs::write(worktree.join("unexpected.txt"), "do not erase\n").unwrap();
        assert!(matches!(
            fixture.pool.delete("review-dirty"),
            Err(RepositoryPoolError::DirtyManagedWorktree { .. })
        ));
        assert!(worktree.exists());
        fs::remove_file(worktree.join("unexpected.txt")).unwrap();
        fixture.pool.delete("review-dirty").unwrap();
    }

    #[test]
    fn orphan_inspection_and_repair_remove_only_clean_unregistered_worktrees() {
        let fixture = fixture();
        let prepared = fixture
            .pool
            .prepare(request(&fixture, "review-orphan"))
            .unwrap();
        fixture.pool.registry().remove("review-orphan").unwrap();
        let orphan_report = fixture.pool.inspect_orphans().unwrap();
        assert_eq!(orphan_report.unregistered_worktrees.len(), 1);
        let repaired = fixture.pool.repair_orphans().unwrap();
        assert_eq!(repaired.removed_unregistered.len(), 1);
        assert!(!Path::new(prepared.record.worktree_path.as_str()).exists());
    }

    #[test]
    fn registry_records_missing_worktrees_for_safe_startup_repair() {
        let fixture = fixture();
        let prepared = fixture
            .pool
            .prepare(request(&fixture, "review-missing"))
            .unwrap();
        let worktree = PathBuf::from(prepared.record.worktree_path.as_str());
        // Simulate interrupted external cleanup: Git unregisters it, then the
        // durable registry remains until startup repair runs.
        let source = prepared.record.source.path().to_path_buf();
        git(
            &source,
            &["worktree", "remove", "--force", worktree.to_str().unwrap()],
        );
        let report = fixture.pool.inspect_orphans().unwrap();
        assert_eq!(report.missing_registrations.len(), 1);
        let repaired = fixture.pool.repair_orphans().unwrap();
        assert_eq!(
            repaired.repaired_missing_registrations,
            vec!["review-missing"]
        );
        assert!(fixture.pool.registry().list().unwrap().is_empty());
    }

    #[test]
    fn invalid_components_cannot_escape_app_directories() {
        assert!(RepositoryLocator::new("github.com", "..", "repo").is_err());
        let pool = RepositoryPool::new("/cache", "/data");
        assert!(pool.worktree_path("../../not-a-review").is_err());
    }

    #[test]
    fn unused_bare_mirror_can_be_removed_after_its_worktree_is_deleted() {
        let fixture = fixture();
        fixture
            .pool
            .prepare(request(&fixture, "review-cleanup"))
            .unwrap();
        fixture.pool.delete("review-cleanup").unwrap();
        let mirror = fixture.pool.mirror_path(&fixture.locator);
        assert!(mirror.exists());
        fixture.pool.remove_unused_mirror(&fixture.locator).unwrap();
        assert!(!mirror.exists());
    }

    #[test]
    fn concurrent_prepare_for_one_review_serializes_registry_and_worktree_creation() {
        let fixture = fixture();
        let pool = Arc::new(fixture.pool.clone());
        let barrier = Arc::new(Barrier::new(2));
        let left_pool = Arc::clone(&pool);
        let left_barrier = Arc::clone(&barrier);
        let left_request = request(&fixture, "review-concurrent");
        let right_pool = Arc::clone(&pool);
        let right_barrier = Arc::clone(&barrier);
        let right_request = request(&fixture, "review-concurrent");
        let left = thread::spawn(move || {
            left_barrier.wait();
            left_pool.prepare(left_request)
        });
        let right = thread::spawn(move || {
            right_barrier.wait();
            right_pool.prepare(right_request)
        });
        let results = [left.join().unwrap(), right.join().unwrap()];
        // `prepare` is idempotent for identical pinned input. Both callers
        // may safely receive the same checkout, but only one registry record
        // and one worktree may be created.
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 2);
        assert_eq!(pool.registry().list().unwrap().len(), 1);
        let paths = results
            .iter()
            .map(|result| result.as_ref().unwrap().record.worktree_path.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(paths.len(), 1);
        pool.delete("review-concurrent").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_path_check_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temporary = TempDir::new().unwrap();
        let data = temporary.path().join("data");
        let reviews = data.join("reviews");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&reviews).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("victim"), "keep\n").unwrap();
        symlink(&outside, reviews.join("escaped")).unwrap();
        let pool = RepositoryPool::new(temporary.path().join("cache"), &data);
        assert!(matches!(
            pool.ensure_managed_path(&reviews.join("escaped/victim")),
            Err(RepositoryPoolError::UnsafeManagedPath { .. })
        ));
    }
}
