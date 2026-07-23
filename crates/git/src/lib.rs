//! Typed, shell-free Git operations for local workspaces.
//!
//! This crate never writes refs, configuration, the index, or a worktree while
//! discovering or capturing a review. It uses the Git executable because that
//! gives the same revision and ignore semantics users see in their terminal.

use std::{
    collections::{BTreeSet, VecDeque},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use chrono::Utc;
use localreview_domain::{
    is_safe_repository_relative_path, BaseReference, ComparisonId, ComparisonOptions,
    ContentFingerprint, GitSha, HeadState, RepositoryComparison, RepositoryId,
    ReviewFileClassification, StoredPath, UntrackedFile,
};
use thiserror::Error;

mod pool;

pub use pool::*;

/// Capturing an untracked file retains its bytes in the immutable comparison
/// input. This conservative ceiling keeps a single pathological file from
/// exhausting the desktop process; callers can report the scoped capture error
/// and offer a configured higher limit in a later capture policy.
pub const DEFAULT_MAX_UNTRACKED_CAPTURE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_COHERENT_CAPTURE_ATTEMPTS: usize = 3;
/// Blame is explicitly on-demand. A small, fixed ceiling prevents a selection
/// that spans a generated file from turning a UI click into a huge process
/// response.
pub const MAX_BLAME_LINES: u32 = 500;
pub const MAX_BLAME_SOURCE_LINE_BYTES: usize = 16 * 1024;
pub const DEFAULT_MAX_COMMIT_CONTEXT_ENTRIES: usize = 250;
pub const MAX_COMMIT_CONTEXT_ENTRIES: usize = 2_000;
pub const MAX_COMMIT_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct GitCommand {
    pub working_directory: PathBuf,
    pub arguments: Vec<OsString>,
}

impl GitCommand {
    #[must_use]
    pub fn new(
        working_directory: impl Into<PathBuf>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            working_directory: working_directory.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn display(&self) -> String {
        let arguments = self
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        format!("git -C {} {arguments}", self.working_directory.display())
    }
}

#[derive(Clone, Debug)]
pub struct GitOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    #[must_use]
    pub fn success(&self) -> bool {
        self.status.success()
    }

    #[must_use]
    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    #[must_use]
    pub fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
}

pub trait GitExecutor: Send + Sync {
    fn execute(&self, command: &GitCommand) -> Result<GitOutput, GitError>;
}

/// A selected, immutable range of lines in one captured Git revision.
/// `revision` is intentionally a parsed SHA rather than a revision expression
/// such as `HEAD~3`, so callers cannot make an on-demand blame result drift as
/// refs move after capture.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitBlameRequest {
    pub revision: GitSha,
    pub path: StoredPath,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitBlameLine {
    pub revision: GitSha,
    pub original_line: u32,
    pub final_line: u32,
    pub source_path: StoredPath,
    pub author_name: String,
    pub author_email: String,
    pub author_time: String,
    pub summary: String,
    /// The selected source line, bounded independently of the number of
    /// selected lines. This is a convenience only; canonical source remains
    /// the immutable review document.
    pub source: String,
    pub source_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitBlameResult {
    pub request: GitBlameRequest,
    pub lines: Vec<GitBlameLine>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitCommitSummary {
    pub sha: GitSha,
    pub parent_shas: Vec<GitSha>,
    pub author_name: String,
    pub author_email: String,
    /// Git's ISO-8601 author timestamp. Keeping the wire value exact avoids
    /// locale-sensitive parsing in this read-only metadata layer.
    pub authored_at: String,
    pub subject: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitCommitDetails {
    pub summary: GitCommitSummary,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
    pub body: String,
    pub body_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitCommitRange {
    pub merge_base: GitSha,
    pub head: GitSha,
}

/// A bounded, presentation-neutral plan for a commit-context panel. The plan
/// never changes the canonical aggregate comparison; it only asks Git for
/// explanatory metadata between two already captured SHAs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitCommitContextRequest {
    pub max_entries: usize,
    pub include_merge_commits: bool,
    pub author_contains: Option<String>,
    pub subject_contains: Option<String>,
    pub selected_commit: Option<GitSha>,
}

impl Default for GitCommitContextRequest {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_COMMIT_CONTEXT_ENTRIES,
            include_merge_commits: true,
            author_contains: None,
            subject_contains: None,
            selected_commit: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitCommitContext {
    pub range: GitCommitRange,
    pub commits: Vec<GitCommitSummary>,
    /// Indicates there were more unfiltered commits after `max_entries`.
    pub truncated: bool,
    pub selected_commit: Option<GitCommitDetails>,
}

#[derive(Clone, Debug, Default)]
pub struct ProcessGitExecutor;

impl GitExecutor for ProcessGitExecutor {
    fn execute(&self, command: &GitCommand) -> Result<GitOutput, GitError> {
        let output = Command::new("git")
            .current_dir(&command.working_directory)
            // Read-only status/diff commands otherwise may refresh the index
            // stat cache. A recursive worktree watcher can mistake that
            // administrative write for source newer than the capture it just
            // produced. Mutating commands such as explicit fetch still take
            // their required locks; this disables only optional lock-taking.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(&command.arguments)
            .output()
            .map_err(|source| GitError::Spawn {
                command: command.display(),
                source,
            })?;
        Ok(GitOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[derive(Debug, Error)]
pub enum GitError {
    #[error("could not run {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("Git command failed ({command}): {stderr}")]
    CommandFailed { command: String, stderr: String },
    #[error("{path} is not a usable Git worktree")]
    NotARepository { path: PathBuf },
    #[error("could not parse Git output: {0}")]
    Parse(String),
    #[error("could not access {path}: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("repository-relative path is unsafe: {path}")]
    UnsafeRepositoryPath { path: StoredPath },
    #[error("Git line range must be one-based, non-empty, and contain at most {limit} lines")]
    InvalidBlameRange { limit: u32 },
    #[error("commit-context entry limit must be between 1 and {limit}")]
    InvalidCommitContextLimit { limit: usize },
    #[error("selected commit {commit} is not in the captured comparison range")]
    CommitOutsideComparisonRange { commit: GitSha },
    #[error("Git output exceeded the bounded {operation} response limit ({limit} bytes)")]
    OutputTooLarge {
        operation: &'static str,
        limit: usize,
    },
    #[error("repository has no HEAD commit, so a local comparison cannot be captured")]
    UnbornHead,
    #[error("untracked file exceeds the capture limit ({byte_len} bytes > {limit}): {path}")]
    UntrackedFileTooLarge {
        path: PathBuf,
        byte_len: u64,
        limit: u64,
    },
    #[error(
        "repository changed while the review snapshot was being captured after {attempts} attempts"
    )]
    ConcurrentModification { attempts: usize },
}

#[derive(Clone, Debug)]
pub struct GitRepository<E = ProcessGitExecutor> {
    root: PathBuf,
    executor: E,
}

impl GitRepository<ProcessGitExecutor> {
    #[must_use]
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            executor: ProcessGitExecutor,
        }
    }
}

impl<E: GitExecutor> GitRepository<E> {
    #[must_use]
    pub fn with_executor(root: impl Into<PathBuf>, executor: E) -> Self {
        Self {
            root: root.into(),
            executor,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn inspect(&self) -> Result<RepositoryIdentity, GitError> {
        self.require_success(self.command(["rev-parse", "--is-inside-work-tree"]))?;
        let worktree = PathBuf::from(
            self.require_success(self.command(["rev-parse", "--show-toplevel"]))?
                .stdout_trimmed(),
        );
        let common_dir = PathBuf::from(
            self.require_success(self.command([
                "rev-parse",
                "--path-format=absolute",
                "--git-common-dir",
            ]))?
            .stdout_trimmed(),
        );

        let branch = match self.execute(self.command(["symbolic-ref", "-q", "--short", "HEAD"]))? {
            output if output.success() => HeadState::Branch(output.stdout_trimmed()),
            _ => match self.execute(self.command(["rev-parse", "--verify", "HEAD"]))? {
                output if output.success() => {
                    HeadState::Detached(parse_sha(&output.stdout_trimmed())?)
                }
                _ => HeadState::Unborn,
            },
        };
        let origin_url = match self.execute(self.command(["remote", "get-url", "origin"]))? {
            output if output.success() => Some(normalize_remote_url(&output.stdout_trimmed())),
            _ => None,
        };
        Ok(RepositoryIdentity {
            worktree,
            common_dir: Some(common_dir),
            primary_remote: origin_url,
            head: branch,
        })
    }

    /// Returns the locally recorded default branch for `origin` without
    /// fetching or changing any ref. A missing `origin/HEAD` is normal and is
    /// represented as `None` so setup can offer guidance only when Git has an
    /// authoritative suggestion.
    pub fn primary_remote_head(&self) -> Result<Option<String>, GitError> {
        let output = self.execute(self.command([
            "symbolic-ref",
            "-q",
            "--short",
            "refs/remotes/origin/HEAD",
        ]))?;
        if !output.success() {
            return Ok(None);
        }
        let reference = output.stdout_trimmed();
        if reference.starts_with("origin/")
            && reference.len() <= 512
            && !reference.chars().any(char::is_control)
        {
            Ok(Some(reference))
        } else {
            Err(GitError::Parse(
                "origin/HEAD did not resolve to a safe remote branch".into(),
            ))
        }
    }

    /// Resolves GitHub Linguist's per-path language override using Git's own
    /// `.gitattributes` matcher. `check-attr -z` keeps repository-relative
    /// paths containing whitespace or non-ASCII characters unambiguous.
    /// Boolean/unset values are not language names and therefore return None.
    pub fn linguist_language(&self, path: &StoredPath) -> Result<Option<String>, GitError> {
        const MAX_ATTRIBUTE_OUTPUT_BYTES: usize = 4 * 1024;
        const MAX_LANGUAGE_BYTES: usize = 128;
        validate_repository_relative_path(path)?;
        let output = self.require_success(GitCommand::new(
            &self.root,
            [
                OsString::from("check-attr"),
                OsString::from("-z"),
                OsString::from("linguist-language"),
                OsString::from("--"),
                OsString::from(path.as_str()),
            ],
        ))?;
        if output.stdout.len() > MAX_ATTRIBUTE_OUTPUT_BYTES {
            return Err(GitError::OutputTooLarge {
                operation: "linguist-language attribute",
                limit: MAX_ATTRIBUTE_OUTPUT_BYTES,
            });
        }
        let fields = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let [reported_path, attribute, value] = fields.as_slice() else {
            return Err(GitError::Parse(
                "git check-attr returned an incomplete linguist-language record".into(),
            ));
        };
        if *reported_path != path.as_str().as_bytes() || *attribute != b"linguist-language" {
            return Err(GitError::Parse(
                "git check-attr returned a mismatched linguist-language record".into(),
            ));
        }
        if matches!(*value, b"unspecified" | b"unset" | b"set") {
            return Ok(None);
        }
        if value.is_empty() || value.len() > MAX_LANGUAGE_BYTES {
            return Ok(None);
        }
        let value = std::str::from_utf8(value)
            .map_err(|_| GitError::Parse("linguist-language was not UTF-8".into()))?
            .trim();
        if value.is_empty() || value.chars().any(char::is_control) {
            Ok(None)
        } else {
            Ok(Some(value.to_owned()))
        }
    }

    pub fn resolve_comparison(
        &self,
        repository_id: RepositoryId,
        comparison_id: ComparisonId,
        requested_base: BaseReference,
        options: ComparisonOptions,
    ) -> Result<ResolvedLocalComparison, GitError> {
        options
            .validate()
            .map_err(|error| GitError::Parse(error.to_string()))?;
        let identity = self.inspect()?;
        let head_sha = match identity.head {
            HeadState::Branch(_) | HeadState::Detached(_) => parse_sha(
                &self
                    .require_success(self.command(["rev-parse", "--verify", "HEAD"]))?
                    .stdout_trimmed(),
            )?,
            HeadState::Unborn => return Err(GitError::UnbornHead),
        };
        let base_tip_sha = parse_sha(
            &self
                .require_success(self.command(["rev-parse", "--verify", requested_base.as_str()]))?
                .stdout_trimmed(),
        )?;
        let merge_base_sha = parse_sha(
            &self
                .require_success(self.command(["merge-base", requested_base.as_str(), "HEAD"]))?
                .stdout_trimmed(),
        )?;
        Ok(ResolvedLocalComparison {
            repository_id,
            comparison_id,
            requested_base,
            base_tip_sha,
            merge_base_sha,
            head_sha,
            head: identity.head,
            options,
        })
    }

    /// Captures immutable inputs to a synthetic local review target. The method
    /// only invokes read-only Git commands and reads non-ignored untracked files.
    pub fn capture_local_comparison(
        &self,
        resolved: ResolvedLocalComparison,
    ) -> Result<CapturedLocalComparison, GitError> {
        for _attempt in 1..=MAX_COHERENT_CAPTURE_ATTEMPTS {
            let captured = self.capture_local_comparison_once(&resolved)?;
            if self.capture_still_current(&resolved, &captured)? {
                return Ok(captured);
            }
        }
        Err(GitError::ConcurrentModification {
            attempts: MAX_COHERENT_CAPTURE_ATTEMPTS,
        })
    }

    /// Captures one candidate generation. The public entry point validates a
    /// second observation before exposing it so Git's patch bytes, raw
    /// manifest, and retained file bytes cannot silently describe different
    /// working-tree moments.
    fn capture_local_comparison_once(
        &self,
        resolved: &ResolvedLocalComparison,
    ) -> Result<CapturedLocalComparison, GitError> {
        let status_bytes = self
            .require_success(self.command([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ]))?
            .stdout;
        let statuses = parse_porcelain_v1_z(&status_bytes)?;
        let captured_untracked_files = statuses
            .iter()
            .filter(|entry| entry.kind == WorkingTreeChangeKind::Untracked)
            .map(|entry| self.capture_untracked_file(&entry.path))
            .collect::<Result<Vec<_>, _>>()?;
        let untracked_files = captured_untracked_files
            .iter()
            .map(|file| file.metadata.clone())
            .collect::<Vec<_>>();

        let committed_patch = self.diff_bytes(
            &[format!("{}..HEAD", resolved.merge_base_sha.as_str())],
            &resolved.options,
        )?;
        let staged_patch = self.diff_bytes(&["--cached".to_owned()], &resolved.options)?;
        let unstaged_patch = self.diff_bytes(&[], &resolved.options)?;
        // The three layer patches explain where local changes came from, but
        // they cannot be concatenated to represent the reviewed target: one
        // path may have a committed change, an index change, and a different
        // working-tree change.  Capture the final tracked state directly from
        // the merge base for the canonical review diff.
        let working_tree_patch = self.diff_bytes(
            &[resolved.merge_base_sha.as_str().to_owned()],
            &resolved.options,
        )?;
        let captured_tracked_files =
            self.capture_tracked_files(&resolved.merge_base_sha, &resolved.options)?;
        let index_fingerprint = ContentFingerprint::from_bytes(&staged_patch);
        let working_tree_fingerprint = fingerprint_snapshot(&working_tree_patch, &untracked_files);
        let comparison = RepositoryComparison {
            id: resolved.comparison_id,
            repository_id: resolved.repository_id,
            requested_base: resolved.requested_base.clone(),
            base_tip_sha: resolved.base_tip_sha.clone(),
            merge_base_sha: resolved.merge_base_sha.clone(),
            head_sha: Some(resolved.head_sha.clone()),
            head: resolved.head.clone(),
            index_fingerprint,
            working_tree_fingerprint,
            untracked_files,
            options: resolved.options.clone(),
            captured_at: Utc::now(),
        };
        Ok(CapturedLocalComparison {
            comparison,
            working_tree_patch,
            committed_patch,
            staged_patch,
            unstaged_patch,
            captured_tracked_files,
            captured_untracked_files,
            status: statuses,
        })
    }

    /// Re-observes every mutable input that contributes to a capture. A
    /// mismatch causes a bounded retry while the previous successful review
    /// remains active. This deliberately compares bytes rather than only
    /// mtimes/status codes: same-sized edits and untracked-file rewrites are
    /// detected as well.
    fn capture_still_current(
        &self,
        resolved: &ResolvedLocalComparison,
        captured: &CapturedLocalComparison,
    ) -> Result<bool, GitError> {
        let head = self
            .require_success(self.command(["rev-parse", "--verify", "HEAD"]))?
            .stdout_trimmed();
        if head != resolved.head_sha.as_str() {
            return Ok(false);
        }

        let status_bytes = self
            .require_success(self.command([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ]))?
            .stdout;
        let statuses = parse_porcelain_v1_z(&status_bytes)?;
        if statuses != captured.status {
            return Ok(false);
        }

        let untracked = statuses
            .iter()
            .filter(|entry| entry.kind == WorkingTreeChangeKind::Untracked)
            .map(|entry| self.capture_untracked_file(&entry.path))
            .collect::<Result<Vec<_>, _>>()?;
        if untracked != captured.captured_untracked_files {
            return Ok(false);
        }

        let committed = self.diff_bytes(
            &[format!("{}..HEAD", resolved.merge_base_sha.as_str())],
            &resolved.options,
        )?;
        let staged = self.diff_bytes(&["--cached".to_owned()], &resolved.options)?;
        let unstaged = self.diff_bytes(&[], &resolved.options)?;
        let aggregate = self.diff_bytes(
            &[resolved.merge_base_sha.as_str().to_owned()],
            &resolved.options,
        )?;
        if committed != captured.committed_patch
            || staged != captured.staged_patch
            || unstaged != captured.unstaged_patch
            || aggregate != captured.working_tree_patch
        {
            return Ok(false);
        }

        let tracked = self.capture_tracked_files(&resolved.merge_base_sha, &resolved.options)?;
        Ok(tracked == captured.captured_tracked_files)
    }

    pub fn status(&self) -> Result<Vec<WorkingTreeChange>, GitError> {
        let bytes = self
            .require_success(self.command([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ]))?
            .stdout;
        parse_porcelain_v1_z(&bytes)
    }

    /// Fetches a named remote only when the caller explicitly requested a
    /// refresh. Capture/discovery never calls this method implicitly.
    pub fn fetch_remote(&self, remote: &str) -> Result<(), GitError> {
        if remote.trim().is_empty() || remote.contains('\0') {
            return Err(GitError::Parse("remote name is invalid".into()));
        }
        self.require_success(GitCommand::new(
            &self.root,
            ["fetch", "--prune", "--no-tags", remote],
        ))?;
        Ok(())
    }

    /// Returns the direct commit divergence between a verified base revision
    /// and `HEAD`.  This is deliberately separate from capture: review
    /// setup may display it without producing a new immutable generation.
    pub fn ahead_behind(&self, base: &BaseReference) -> Result<AheadBehind, GitError> {
        let range = format!("{}...HEAD", base.as_str());
        let output = self.require_success(GitCommand::new(
            &self.root,
            ["rev-list", "--left-right", "--count", range.as_str()],
        ))?;
        let values = output.stdout_trimmed();
        let mut values = values.split_ascii_whitespace();
        let behind = values
            .next()
            .ok_or_else(|| GitError::Parse("ahead/behind output is missing behind count".into()))?
            .parse::<u32>()
            .map_err(|_| {
                GitError::Parse("ahead/behind output has an invalid behind count".into())
            })?;
        let ahead = values
            .next()
            .ok_or_else(|| GitError::Parse("ahead/behind output is missing ahead count".into()))?
            .parse::<u32>()
            .map_err(|_| {
                GitError::Parse("ahead/behind output has an invalid ahead count".into())
            })?;
        if values.next().is_some() {
            return Err(GitError::Parse(
                "ahead/behind output has unexpected fields".into(),
            ));
        }
        Ok(AheadBehind { ahead, behind })
    }

    /// Reads a blob from an immutable commit. `None` means the path does not
    /// exist at that revision (for example, an added or deleted file); command
    /// construction remains typed and no revision expression is accepted.
    pub fn read_blob_at(
        &self,
        revision: &GitSha,
        relative_path: &StoredPath,
    ) -> Result<Option<Vec<u8>>, GitError> {
        validate_repository_relative_path(relative_path)?;
        let object = format!("{}:{}", revision.as_str(), relative_path.as_str());
        let exists = self.execute(GitCommand::new(
            &self.root,
            ["cat-file", "-e", object.as_str()],
        ))?;
        if !exists.success() {
            return Ok(None);
        }
        Ok(Some(
            self.require_success(GitCommand::new(
                &self.root,
                ["show", "--no-ext-diff", "--format=", object.as_str()],
            ))?
            .stdout,
        ))
    }

    /// Reads the final local worktree state without following symlinks outside
    /// the repository. `None` represents a deleted or absent path.
    pub fn read_worktree_file(
        &self,
        relative_path: &StoredPath,
    ) -> Result<Option<Vec<u8>>, GitError> {
        validate_repository_relative_path(relative_path)?;
        let path = self.root.join(relative_path.as_str());
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(GitError::File {
                    path: path.clone(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(GitError::UnsafeRepositoryPath {
                path: relative_path.clone(),
            });
        }
        fs::read(&path)
            .map(Some)
            .map_err(|source| GitError::File { path, source })
    }

    /// Reads attribution for a selected line range at an immutable captured
    /// revision. This never uses `HEAD`, a branch name, shell interpolation, or
    /// a worktree read, so an already-open review remains reproducible while
    /// the developer continues editing locally.
    pub fn blame_at(&self, request: GitBlameRequest) -> Result<GitBlameResult, GitError> {
        validate_repository_relative_path(&request.path)?;
        let line_count = request
            .end_line
            .checked_sub(request.start_line)
            .and_then(|count| count.checked_add(1))
            .filter(|count| request.start_line > 0 && *count <= MAX_BLAME_LINES)
            .ok_or(GitError::InvalidBlameRange {
                limit: MAX_BLAME_LINES,
            })?;
        let range = format!("{},{}", request.start_line, request.end_line);
        let output = self.require_success(GitCommand::new(
            &self.root,
            [
                "blame",
                "--line-porcelain",
                "--no-progress",
                "-L",
                range.as_str(),
                request.revision.as_str(),
                "--",
                request.path.as_str(),
            ],
        ))?;
        let lines = parse_blame_porcelain(&output.stdout, &request.path)?;
        if lines.len() > usize::try_from(line_count).unwrap_or(usize::MAX) {
            return Err(GitError::Parse(
                "blame returned more lines than the selected range".into(),
            ));
        }
        Ok(GitBlameResult { request, lines })
    }

    /// Returns commit metadata for an immutable object ID. The commit body is
    /// bounded after parsing so it can be safely displayed or forwarded by a
    /// desktop command without exposing a pathological commit message.
    pub fn commit_details(&self, revision: &GitSha) -> Result<GitCommitDetails, GitError> {
        let output = self.require_success(GitCommand::new(
            &self.root,
            [
                "show",
                "--no-patch",
                "--no-notes",
                "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x00%cn%x00%ce%x00%cI%x00%B",
                revision.as_str(),
            ],
        ))?;
        parse_commit_details(&output.stdout)
    }

    /// Produces a bounded commit-context projection for `merge_base..head`.
    /// It is intentionally separate from `capture_local_comparison`: selecting
    /// or filtering commits cannot alter the aggregate worktree comparison,
    /// retained sources, or annotation anchors.
    pub fn commit_context(
        &self,
        range: GitCommitRange,
        request: GitCommitContextRequest,
    ) -> Result<GitCommitContext, GitError> {
        if request.max_entries == 0 || request.max_entries > MAX_COMMIT_CONTEXT_ENTRIES {
            return Err(GitError::InvalidCommitContextLimit {
                limit: MAX_COMMIT_CONTEXT_ENTRIES,
            });
        }
        let range_spec = format!("{}..{}", range.merge_base.as_str(), range.head.as_str());
        let count = request.max_entries.saturating_add(1).to_string();
        let mut arguments = vec![
            OsString::from("log"),
            OsString::from("-z"),
            OsString::from("--no-decorate"),
            OsString::from("--no-notes"),
            OsString::from("--topo-order"),
            OsString::from("--reverse"),
            OsString::from(format!("--max-count={count}")),
            OsString::from("--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%s%x00%x1e"),
        ];
        if !request.include_merge_commits {
            arguments.push(OsString::from("--no-merges"));
        }
        arguments.push(OsString::from(range_spec));
        let output = self.require_success(GitCommand::new(&self.root, arguments))?;
        let mut commits = parse_commit_summaries(&output.stdout)?;
        let truncated = commits.len() > request.max_entries;
        commits.truncate(request.max_entries);
        let author_query = request
            .author_contains
            .as_deref()
            .map(str::to_ascii_lowercase);
        let subject_query = request
            .subject_contains
            .as_deref()
            .map(str::to_ascii_lowercase);
        commits.retain(|commit| {
            let author_matches = match &author_query {
                Some(query) => {
                    commit.author_name.to_ascii_lowercase().contains(query)
                        || commit.author_email.to_ascii_lowercase().contains(query)
                }
                None => true,
            };
            let subject_matches = match &subject_query {
                Some(query) => commit.subject.to_ascii_lowercase().contains(query),
                None => true,
            };
            author_matches && subject_matches
        });
        let selected_commit = request
            .selected_commit
            .as_ref()
            .map(|selected| {
                if !self.commit_is_in_range(selected, &range)? {
                    return Err(GitError::CommitOutsideComparisonRange {
                        commit: selected.clone(),
                    });
                }
                self.commit_details(selected)
            })
            .transpose()?;
        Ok(GitCommitContext {
            range,
            commits,
            truncated,
            selected_commit,
        })
    }

    fn commit_is_in_range(
        &self,
        candidate: &GitSha,
        range: &GitCommitRange,
    ) -> Result<bool, GitError> {
        let after_base = self.execute(GitCommand::new(
            &self.root,
            [
                "merge-base",
                "--is-ancestor",
                range.merge_base.as_str(),
                candidate.as_str(),
            ],
        ))?;
        if !after_base.success() {
            return Ok(false);
        }
        let before_head = self.execute(GitCommand::new(
            &self.root,
            [
                "merge-base",
                "--is-ancestor",
                candidate.as_str(),
                range.head.as_str(),
            ],
        ))?;
        Ok(before_head.success() && candidate != &range.merge_base)
    }

    fn capture_untracked_file(
        &self,
        relative_path: &StoredPath,
    ) -> Result<CapturedUntrackedFile, GitError> {
        validate_repository_relative_path(relative_path)?;
        let path = self.root.join(relative_path.as_str());
        let metadata = fs::symlink_metadata(&path).map_err(|source| GitError::File {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(GitError::UnsafeRepositoryPath {
                path: relative_path.clone(),
            });
        }
        let byte_len = metadata.len();
        if byte_len > DEFAULT_MAX_UNTRACKED_CAPTURE_BYTES {
            return Err(GitError::UntrackedFileTooLarge {
                path,
                byte_len,
                limit: DEFAULT_MAX_UNTRACKED_CAPTURE_BYTES,
            });
        }
        let content = fs::read(&path).map_err(|source| GitError::File {
            path: path.clone(),
            source,
        })?;
        Ok(CapturedUntrackedFile {
            metadata: UntrackedFile {
                path: relative_path.clone(),
                fingerprint: ContentFingerprint::from_bytes(&content),
                byte_len: u64::try_from(content.len()).unwrap_or(u64::MAX),
                binary: content.contains(&0),
            },
            content,
        })
    }

    fn capture_tracked_files(
        &self,
        merge_base: &GitSha,
        options: &ComparisonOptions,
    ) -> Result<Vec<CapturedTrackedFile>, GitError> {
        let mut arguments = vec![
            OsString::from("diff"),
            // `--raw -z` is the authoritative file manifest.  Unlike the
            // human patch header it never quotes paths, and it records pure
            // renames, copies, mode changes, binary files and gitlinks even
            // when there are no textual hunks to render.
            OsString::from("--raw"),
            OsString::from("-z"),
            OsString::from("--find-renames"),
            OsString::from("--find-copies"),
            OsString::from("--find-copies-harder"),
            OsString::from("--no-ext-diff"),
        ];
        if options.ignore_all_whitespace {
            arguments.push(OsString::from("--ignore-all-space"));
        }
        if options.ignore_space_at_eol {
            arguments.push(OsString::from("--ignore-space-at-eol"));
        }
        if options.ignore_cr_at_eol {
            arguments.push(OsString::from("--ignore-cr-at-eol"));
        }
        arguments.push(OsString::from(merge_base.as_str()));
        if !options.path_filters.is_empty() {
            arguments.push(OsString::from("--"));
            arguments.extend(
                options
                    .path_filters
                    .iter()
                    .map(|path| OsString::from(path.as_str())),
            );
        }
        let raw = self
            .require_success(GitCommand::new(&self.root, arguments))?
            .stdout;
        parse_raw_diff_z(&raw)?
            .into_iter()
            .map(|entry| {
                let is_gitlink = entry.old_mode == 0o160000 || entry.new_mode == 0o160000;
                // A gitlink is a directory in a worktree, not a source file.
                // Never attempt to follow it; retain a first-class no-hunk
                // record instead so one submodule cannot sink the repository.
                let content = if is_gitlink || entry.kind == CapturedTrackedFileKind::Deleted {
                    None
                } else {
                    self.read_worktree_file(&entry.path)?
                };
                let binary = content.as_ref().is_some_and(|bytes| bytes.contains(&0));
                let lfs_pointer = content.as_ref().is_some_and(|bytes| is_lfs_pointer(bytes));
                let classification =
                    classify_review_file(&entry.path, content.as_deref(), is_gitlink)?;
                Ok(CapturedTrackedFile {
                    path: entry.path,
                    old_path: entry.old_path,
                    content,
                    kind: if is_gitlink {
                        CapturedTrackedFileKind::Submodule
                    } else {
                        entry.kind
                    },
                    old_mode: entry.old_mode,
                    new_mode: entry.new_mode,
                    binary,
                    lfs_pointer,
                    classification,
                })
            })
            .collect()
    }

    fn diff_bytes(
        &self,
        prefixes: &[String],
        options: &ComparisonOptions,
    ) -> Result<Vec<u8>, GitError> {
        let mut arguments = vec![
            OsString::from("diff"),
            OsString::from("--binary"),
            OsString::from("--find-renames"),
            OsString::from("--find-copies"),
            OsString::from("--find-copies-harder"),
            OsString::from("--no-ext-diff"),
        ];
        if options.ignore_all_whitespace {
            arguments.push(OsString::from("--ignore-all-space"));
        }
        if options.ignore_space_at_eol {
            arguments.push(OsString::from("--ignore-space-at-eol"));
        }
        if options.ignore_cr_at_eol {
            arguments.push(OsString::from("--ignore-cr-at-eol"));
        }
        arguments.extend(prefixes.iter().map(OsString::from));
        if !options.path_filters.is_empty() {
            arguments.push(OsString::from("--"));
            arguments.extend(
                options
                    .path_filters
                    .iter()
                    .map(|path| OsString::from(path.as_str())),
            );
        }
        Ok(self
            .require_success(GitCommand::new(&self.root, arguments))?
            .stdout)
    }

    fn command<const N: usize>(&self, arguments: [&str; N]) -> GitCommand {
        GitCommand::new(&self.root, arguments)
    }

    fn execute(&self, command: GitCommand) -> Result<GitOutput, GitError> {
        self.executor.execute(&command)
    }

    fn require_success(&self, command: GitCommand) -> Result<GitOutput, GitError> {
        let display = command.display();
        let output = self.execute(command)?;
        if output.success() {
            Ok(output)
        } else {
            Err(GitError::CommandFailed {
                command: display,
                stderr: output.stderr_trimmed(),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryIdentity {
    pub worktree: PathBuf,
    pub common_dir: Option<PathBuf>,
    pub primary_remote: Option<String>,
    pub head: HeadState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AheadBehind {
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug)]
pub struct ResolvedLocalComparison {
    pub repository_id: RepositoryId,
    pub comparison_id: ComparisonId,
    pub requested_base: BaseReference,
    pub base_tip_sha: GitSha,
    pub merge_base_sha: GitSha,
    pub head_sha: GitSha,
    pub head: HeadState,
    pub options: ComparisonOptions,
}

#[derive(Clone, Debug)]
pub struct CapturedLocalComparison {
    pub comparison: RepositoryComparison,
    /// Canonical aggregate patch from the selected merge base to the current
    /// tracked working-tree state. This is the patch diff renderers consume.
    pub working_tree_patch: Vec<u8>,
    /// Layer patches are retained for diagnostics and status presentation only.
    pub committed_patch: Vec<u8>,
    pub staged_patch: Vec<u8>,
    pub unstaged_patch: Vec<u8>,
    /// Immutable final bytes for every tracked path represented by the
    /// aggregate patch. `None` is a captured deletion.
    pub captured_tracked_files: Vec<CapturedTrackedFile>,
    /// Immutable bytes read during capture. Consumers must persist or render
    /// these bytes rather than rereading a potentially changed working tree.
    pub captured_untracked_files: Vec<CapturedUntrackedFile>,
    pub status: Vec<WorkingTreeChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedUntrackedFile {
    pub metadata: UntrackedFile,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedTrackedFile {
    /// Current path for an added/modified/renamed/copy record; the old path
    /// for a deletion. Paths are supplied by Git's NUL-delimited raw format.
    pub path: StoredPath,
    pub old_path: Option<StoredPath>,
    pub content: Option<Vec<u8>>,
    pub kind: CapturedTrackedFileKind,
    pub old_mode: u32,
    pub new_mode: u32,
    pub binary: bool,
    pub lfs_pointer: bool,
    /// Capture-time classification so callers do not infer semantic file
    /// categories from a rendered patch (which can omit binary and gitlink
    /// records entirely).
    pub classification: ReviewFileClassification,
}

/// File-level status produced by `git diff --raw -z`. This deliberately lives
/// in the Git crate, rather than inferring it from hunk text, because Git may
/// represent a valid change without any textual hunk at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturedTrackedFileKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    ModeChanged,
    TypeChanged,
    Submodule,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryConfig {
    pub max_depth: usize,
    pub excluded_directory_names: BTreeSet<OsString>,
    pub excluded_relative_prefixes: Vec<PathBuf>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            max_depth: 4,
            excluded_directory_names: [".git", "node_modules", ".cache", "target", "build", "dist"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            excluded_relative_prefixes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredRepository {
    pub relative_path: PathBuf,
    pub identity: RepositoryIdentity,
}

/// Discovers top-level working trees below `workspace_root`. Once a repository
/// is found the walker does not descend into it, preventing nested dependencies
/// from accidentally becoming workspace repositories.
pub fn discover_repositories(
    workspace_root: impl AsRef<Path>,
    config: &DiscoveryConfig,
) -> Result<Vec<DiscoveredRepository>, GitError> {
    let workspace_root = workspace_root.as_ref();
    let mut pending = VecDeque::from([(workspace_root.to_path_buf(), 0_usize)]);
    let mut discovered = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some((directory, depth)) = pending.pop_front() {
        let metadata = fs::symlink_metadata(&directory).map_err(|source| GitError::File {
            path: directory.clone(),
            source,
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let git_marker = directory.join(".git");
        if git_marker.is_dir() || git_marker.is_file() {
            let repository = GitRepository::open(&directory);
            let identity = repository.inspect()?;
            let dedupe_key = identity
                .common_dir
                .as_ref()
                .and_then(|path| path.canonicalize().ok())
                .unwrap_or_else(|| identity.worktree.clone());
            if seen.insert(dedupe_key) {
                let relative_path = directory
                    .strip_prefix(workspace_root)
                    .unwrap_or(&directory)
                    .to_path_buf();
                discovered.push(DiscoveredRepository {
                    relative_path,
                    identity,
                });
            }
            continue;
        }
        if depth >= config.max_depth {
            continue;
        }
        let entries = fs::read_dir(&directory).map_err(|source| GitError::File {
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| GitError::File {
                path: directory.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| GitError::File {
                path: entry.path(),
                source,
            })?;
            if !file_type.is_dir()
                || file_type.is_symlink()
                || config.excluded_directory_names.contains(&entry.file_name())
            {
                continue;
            }
            let candidate = entry.path();
            let relative = candidate.strip_prefix(workspace_root).unwrap_or(&candidate);
            if config
                .excluded_relative_prefixes
                .iter()
                .any(|prefix| relative.starts_with(prefix))
            {
                continue;
            }
            pending.push_back((candidate, depth + 1));
        }
    }
    discovered.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(discovered)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkingTreeChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Unmerged,
    TypeChanged,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingTreeChange {
    pub index_status: char,
    pub worktree_status: char,
    pub kind: WorkingTreeChangeKind,
    pub path: StoredPath,
    pub original_path: Option<StoredPath>,
}

/// Parses the documented NUL-delimited porcelain v1 format. The output itself
/// contains no locale-dependent prose and is suitable for durable capture.
pub fn parse_porcelain_v1_z(bytes: &[u8]) -> Result<Vec<WorkingTreeChange>, GitError> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 3 || field[2] != b' ' {
            return Err(GitError::Parse("invalid porcelain v1 record".to_owned()));
        }
        let index_status = char::from(field[0]);
        let worktree_status = char::from(field[1]);
        let path = StoredPath::from(String::from_utf8_lossy(&field[3..]).into_owned());
        let kind = change_kind(index_status, worktree_status);
        let original_path =
            if matches!(index_status, 'R' | 'C') || matches!(worktree_status, 'R' | 'C') {
                let original = fields.next().ok_or_else(|| {
                    GitError::Parse("rename record missing original path".to_owned())
                })?;
                Some(StoredPath::from(
                    String::from_utf8_lossy(original).into_owned(),
                ))
            } else {
                None
            };
        entries.push(WorkingTreeChange {
            index_status,
            worktree_status,
            kind,
            path,
            original_path,
        });
    }
    Ok(entries)
}

#[must_use]
pub fn normalize_remote_url(raw: &str) -> String {
    let without_scheme = raw.trim().trim_end_matches('/').trim_end_matches(".git");
    let rest = without_scheme
        .split_once("://")
        .map_or(without_scheme, |(_, rest)| rest)
        .strip_prefix("git@")
        .unwrap_or(without_scheme);
    let rest = rest
        .rsplit_once('@')
        .map_or(rest, |(_, remainder)| remainder);
    let normalized = if let Some((host, path)) = rest.split_once(':') {
        format!("{host}/{path}")
    } else {
        rest.to_owned()
    };
    let mut parts = normalized.splitn(2, '/');
    let host = parts.next().unwrap_or_default().to_ascii_lowercase();
    parts
        .next()
        .map_or(host.clone(), |path| format!("{host}/{path}"))
}

fn change_kind(index_status: char, worktree_status: char) -> WorkingTreeChangeKind {
    let status = if index_status != ' ' {
        index_status
    } else {
        worktree_status
    };
    match status {
        '?' => WorkingTreeChangeKind::Untracked,
        'A' => WorkingTreeChangeKind::Added,
        'M' => WorkingTreeChangeKind::Modified,
        'D' => WorkingTreeChangeKind::Deleted,
        'R' => WorkingTreeChangeKind::Renamed,
        'C' => WorkingTreeChangeKind::Copied,
        'U' => WorkingTreeChangeKind::Unmerged,
        'T' => WorkingTreeChangeKind::TypeChanged,
        _ => WorkingTreeChangeKind::Unknown,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawDiffEntry {
    path: StoredPath,
    old_path: Option<StoredPath>,
    kind: CapturedTrackedFileKind,
    old_mode: u32,
    new_mode: u32,
}

/// Parses Git's raw NUL-delimited format.  The header is ASCII and paths are
/// separate fields, so names containing spaces, tabs, quotes, and UTF-8 are
/// unambiguous.  Git itself permits non-UTF-8 Unix path bytes; those cannot be
/// represented by the cross-platform JSON `StoredPath` contract and are
/// lossily displayed, but they are never reparsed from a quoted patch header.
fn parse_raw_diff_z(bytes: &[u8]) -> Result<Vec<RawDiffEntry>, GitError> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut result = Vec::new();
    while let Some(header) = fields.next() {
        let header = std::str::from_utf8(header)
            .map_err(|_| GitError::Parse("raw diff header was not ASCII".into()))?;
        let mut parts = header.split_whitespace();
        let old_mode = parts
            .next()
            .and_then(|value| value.strip_prefix(':'))
            .and_then(|value| u32::from_str_radix(value, 8).ok())
            .ok_or_else(|| GitError::Parse("raw diff old mode was invalid".into()))?;
        let new_mode = parts
            .next()
            .and_then(|value| u32::from_str_radix(value, 8).ok())
            .ok_or_else(|| GitError::Parse("raw diff new mode was invalid".into()))?;
        let _old_sha = parts
            .next()
            .ok_or_else(|| GitError::Parse("raw diff old object was missing".into()))?;
        let _new_sha = parts
            .next()
            .ok_or_else(|| GitError::Parse("raw diff new object was missing".into()))?;
        let status = parts
            .next()
            .and_then(|value| value.chars().next())
            .ok_or_else(|| GitError::Parse("raw diff status was missing".into()))?;
        if parts.next().is_some() {
            return Err(GitError::Parse("raw diff header had extra fields".into()));
        }
        let first_path = fields
            .next()
            .ok_or_else(|| GitError::Parse("raw diff path was missing".into()))?;
        let (path, old_path, kind) = match status {
            'R' | 'C' => {
                let second_path = fields.next().ok_or_else(|| {
                    GitError::Parse("raw diff rename/copy target was missing".into())
                })?;
                (
                    stored_path_from_git_bytes(second_path),
                    Some(stored_path_from_git_bytes(first_path)),
                    if status == 'R' {
                        CapturedTrackedFileKind::Renamed
                    } else {
                        CapturedTrackedFileKind::Copied
                    },
                )
            }
            'A' => (
                stored_path_from_git_bytes(first_path),
                None,
                CapturedTrackedFileKind::Added,
            ),
            'D' => (
                stored_path_from_git_bytes(first_path),
                None,
                CapturedTrackedFileKind::Deleted,
            ),
            'T' => (
                stored_path_from_git_bytes(first_path),
                None,
                CapturedTrackedFileKind::TypeChanged,
            ),
            'M' => (
                stored_path_from_git_bytes(first_path),
                None,
                if old_mode != new_mode {
                    CapturedTrackedFileKind::ModeChanged
                } else {
                    CapturedTrackedFileKind::Modified
                },
            ),
            other => {
                return Err(GitError::Parse(format!(
                    "unsupported raw diff status {other:?}"
                )));
            }
        };
        result.push(RawDiffEntry {
            path,
            old_path,
            kind,
            old_mode,
            new_mode,
        });
    }
    Ok(result)
}

fn stored_path_from_git_bytes(value: &[u8]) -> StoredPath {
    StoredPath::from(String::from_utf8_lossy(value).into_owned())
}

fn is_lfs_pointer(bytes: &[u8]) -> bool {
    bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
        && bytes
            .windows(b"oid sha256:".len())
            .any(|window| window == b"oid sha256:")
}

/// Classifies a captured repository file without reading it again. The path is
/// validated first because this helper is also used by remote/CLI adapters.
/// Heuristics are deliberately conservative and only supply review hints;
/// they never suppress a file from the canonical comparison.
pub fn classify_review_file(
    relative_path: &StoredPath,
    content: Option<&[u8]>,
    submodule: bool,
) -> Result<ReviewFileClassification, GitError> {
    validate_repository_relative_path(relative_path)?;
    let components = Path::new(relative_path.as_str())
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    let filename = components.last().map_or("", String::as_str);
    let binary = content.is_some_and(|bytes| bytes.contains(&0));
    let lfs_pointer = content.is_some_and(is_lfs_pointer);
    let generated_by_path = components.iter().any(|component| {
        matches!(
            component.as_str(),
            "generated"
                | "__generated__"
                | "gen"
                | "dist"
                | "build"
                | "target"
                | ".next"
                | "coverage"
        )
    }) || filename.ends_with(".generated.rs")
        || filename.ends_with(".generated.ts")
        || filename.ends_with(".generated.tsx")
        || filename.ends_with(".generated.js")
        || filename.ends_with(".generated.go")
        || filename.ends_with(".g.dart")
        || filename.ends_with(".pb.go")
        || filename.ends_with(".min.js")
        || filename.ends_with(".min.css")
        || filename.contains("_generated.")
        || filename.contains(".designer.");
    let generated_by_header = content.is_some_and(|bytes| {
        let prefix = &bytes[..bytes.len().min(16 * 1024)];
        let header = String::from_utf8_lossy(prefix).to_ascii_lowercase();
        header.contains("@generated")
            || header.contains("code generated")
            || header.contains("do not edit")
            || header.contains("automatically generated")
    });
    let vendored = components.iter().any(|component| {
        matches!(
            component.as_str(),
            "vendor"
                | "vendors"
                | "third_party"
                | "third-party"
                | "node_modules"
                | "pods"
                | "carthage"
                | "external"
                | "deps"
        )
    });
    let lockfile = matches!(
        filename,
        "cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "bun.lockb"
            | "poetry.lock"
            | "pipfile.lock"
            | "uv.lock"
            | "gemfile.lock"
            | "composer.lock"
            | "podfile.lock"
            | "mix.lock"
            | "flake.lock"
            | "go.sum"
            | "gradle.lockfile"
    ) || filename.ends_with(".lock");
    Ok(ReviewFileClassification {
        generated: generated_by_path || generated_by_header,
        vendored,
        lockfile,
        binary,
        lfs_pointer,
        submodule,
    })
}

#[derive(Default)]
struct ParsedBlameLine {
    revision: Option<GitSha>,
    original_line: Option<u32>,
    final_line: Option<u32>,
    source_path: Option<StoredPath>,
    author_name: String,
    author_email: String,
    author_time: String,
    summary: String,
}

fn parse_blame_porcelain(
    bytes: &[u8],
    requested_path: &StoredPath,
) -> Result<Vec<GitBlameLine>, GitError> {
    let mut result = Vec::new();
    let mut current: Option<ParsedBlameLine> = None;
    for raw_line in bytes.split(|byte| *byte == b'\n') {
        if raw_line.is_empty() {
            continue;
        }
        if raw_line.first() == Some(&b'\t') {
            let source = &raw_line[1..];
            let current = current.take().ok_or_else(|| {
                GitError::Parse("blame source line appeared before a header".into())
            })?;
            let revision = current
                .revision
                .ok_or_else(|| GitError::Parse("blame header had no revision".into()))?;
            let original_line = current
                .original_line
                .ok_or_else(|| GitError::Parse("blame header had no original line".into()))?;
            let final_line = current
                .final_line
                .ok_or_else(|| GitError::Parse("blame header had no final line".into()))?;
            let (source, source_truncated) =
                truncate_utf8_lossy(source, MAX_BLAME_SOURCE_LINE_BYTES);
            result.push(GitBlameLine {
                revision,
                original_line,
                final_line,
                source_path: current
                    .source_path
                    .unwrap_or_else(|| requested_path.clone()),
                author_name: current.author_name,
                author_email: current.author_email,
                author_time: current.author_time,
                summary: current.summary,
                source,
                source_truncated,
            });
            continue;
        }
        if let Some(header) = parse_blame_header(raw_line)? {
            if current.is_some() {
                return Err(GitError::Parse(
                    "blame entry was missing its source line".into(),
                ));
            }
            current = Some(header);
            continue;
        }
        let separator = raw_line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| GitError::Parse("blame metadata line was malformed".into()))?;
        let (key, value_with_separator) = raw_line.split_at(separator);
        let value = &value_with_separator[1..];
        let current = current
            .as_mut()
            .ok_or_else(|| GitError::Parse("blame metadata appeared before a header".into()))?;
        let value = String::from_utf8_lossy(value).into_owned();
        match key {
            b"author" => current.author_name = value,
            b"author-mail" => {
                current.author_email = value
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            }
            b"author-time" => current.author_time = value,
            b"summary" => current.summary = value,
            b"filename" => current.source_path = Some(decode_git_quoted_path(&value)),
            _ => {}
        }
    }
    if current.is_some() {
        return Err(GitError::Parse(
            "blame output ended before an entry source line".into(),
        ));
    }
    Ok(result)
}

fn parse_blame_header(raw: &[u8]) -> Result<Option<ParsedBlameLine>, GitError> {
    let Ok(raw) = std::str::from_utf8(raw) else {
        return Ok(None);
    };
    let mut fields = raw.split_whitespace();
    let Some(revision) = fields.next() else {
        return Ok(None);
    };
    // Metadata keys such as `author` do not begin with a valid object id.
    let revision = revision.strip_prefix('^').unwrap_or(revision);
    let Ok(revision) = GitSha::new(revision) else {
        return Ok(None);
    };
    let original_line = fields
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| GitError::Parse("blame header original line was invalid".into()))?;
    let final_line = fields
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| GitError::Parse("blame header final line was invalid".into()))?;
    Ok(Some(ParsedBlameLine {
        revision: Some(revision),
        original_line: Some(original_line),
        final_line: Some(final_line),
        ..ParsedBlameLine::default()
    }))
}

fn parse_commit_summaries(bytes: &[u8]) -> Result<Vec<GitCommitSummary>, GitError> {
    const FIELDS_PER_COMMIT: usize = 6;
    bytes
        .split(|byte| *byte == 0x1e)
        .filter(|record| record.iter().any(|byte| *byte != 0))
        .map(|record| {
            let record = if record.last() == Some(&0) {
                &record[..record.len() - 1]
            } else {
                record
            };
            let record = if record.first() == Some(&0) {
                &record[1..]
            } else {
                record
            };
            let fields = record.split(|byte| *byte == 0).collect::<Vec<_>>();
            if fields.len() != FIELDS_PER_COMMIT {
                return Err(GitError::Parse(format!(
                    "commit list output had an incomplete metadata record ({} fields)",
                    fields.len()
                )));
            }
            Ok(GitCommitSummary {
                sha: parse_sha_from_bytes(fields[0])?,
                parent_shas: parse_parent_shas(fields[1])?,
                author_name: String::from_utf8_lossy(fields[2]).into_owned(),
                author_email: String::from_utf8_lossy(fields[3]).into_owned(),
                authored_at: String::from_utf8_lossy(fields[4]).into_owned(),
                subject: String::from_utf8_lossy(fields[5]).into_owned(),
            })
        })
        .collect()
}

fn parse_commit_details(bytes: &[u8]) -> Result<GitCommitDetails, GitError> {
    let mut fields = bytes.splitn(10, |byte| *byte == 0);
    let required = (0..9)
        .map(|_| {
            fields
                .next()
                .ok_or_else(|| GitError::Parse("commit detail output was incomplete".into()))
        })
        .collect::<Result<Vec<_>, GitError>>()?;
    let body = fields.next().unwrap_or_default();
    let (body, body_truncated) = truncate_utf8_lossy(body, MAX_COMMIT_MESSAGE_BYTES);
    Ok(GitCommitDetails {
        summary: GitCommitSummary {
            sha: parse_sha_from_bytes(required[0])?,
            parent_shas: parse_parent_shas(required[1])?,
            author_name: String::from_utf8_lossy(required[2]).into_owned(),
            author_email: String::from_utf8_lossy(required[3]).into_owned(),
            authored_at: String::from_utf8_lossy(required[4]).into_owned(),
            subject: String::from_utf8_lossy(required[5]).into_owned(),
        },
        committer_name: String::from_utf8_lossy(required[6]).into_owned(),
        committer_email: String::from_utf8_lossy(required[7]).into_owned(),
        committed_at: String::from_utf8_lossy(required[8]).into_owned(),
        body,
        body_truncated,
    })
}

fn parse_parent_shas(bytes: &[u8]) -> Result<Vec<GitSha>, GitError> {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .map(parse_sha)
        .collect()
}

fn parse_sha_from_bytes(bytes: &[u8]) -> Result<GitSha, GitError> {
    parse_sha(&String::from_utf8_lossy(bytes))
}

/// Git quotes porcelain paths using C-style escapes when a path contains
/// whitespace or non-ASCII bytes. Decode that exact transport encoding before
/// exposing the path; this is output parsing only, never command construction.
fn decode_git_quoted_path(value: &str) -> StoredPath {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') {
        return StoredPath::from(value);
    }
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 1;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        if byte != b'\\' {
            output.push(byte);
            index += 1;
            continue;
        }
        index += 1;
        if index + 1 >= bytes.len() {
            output.push(b'\\');
            break;
        }
        let escaped = bytes[index];
        if (b'0'..=b'7').contains(&escaped)
            && index + 2 < bytes.len() - 1
            && bytes[index + 1].is_ascii_digit()
            && bytes[index + 2].is_ascii_digit()
            && bytes[index + 1] <= b'7'
            && bytes[index + 2] <= b'7'
        {
            let octal =
                (escaped - b'0') * 64 + (bytes[index + 1] - b'0') * 8 + (bytes[index + 2] - b'0');
            output.push(octal);
            index += 3;
            continue;
        }
        output.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            other => other,
        });
        index += 1;
    }
    StoredPath::from(String::from_utf8_lossy(&output).into_owned())
}

fn truncate_utf8_lossy(bytes: &[u8], limit: usize) -> (String, bool) {
    if bytes.len() <= limit {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    (String::from_utf8_lossy(&bytes[..limit]).into_owned(), true)
}

fn parse_sha(value: &str) -> Result<GitSha, GitError> {
    GitSha::new(value).map_err(|error| GitError::Parse(error.to_string()))
}

fn validate_repository_relative_path(path: &StoredPath) -> Result<(), GitError> {
    if !is_safe_repository_relative_path(path) {
        Err(GitError::UnsafeRepositoryPath { path: path.clone() })
    } else {
        Ok(())
    }
}

fn fingerprint_snapshot(
    working_tree_patch: &[u8],
    untracked_files: &[UntrackedFile],
) -> ContentFingerprint {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(working_tree_patch.len() as u64).to_be_bytes());
    bytes.extend_from_slice(working_tree_patch);
    let mut files = untracked_files.iter().collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in files {
        bytes.extend_from_slice(file.path.as_str().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(file.fingerprint.as_str().as_bytes());
        bytes.extend_from_slice(&file.byte_len.to_be_bytes());
    }
    ContentFingerprint::from_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use tempfile::TempDir;

    fn git(path: &Path, arguments: &[&str]) {
        let result = Command::new("git")
            .current_dir(path)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git {:?}: {}",
            arguments,
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn revision(path: &Path, reference: &str) -> GitSha {
        let output = Command::new("git")
            .current_dir(path)
            .args(["rev-parse", reference])
            .output()
            .unwrap();
        assert!(output.status.success());
        parse_sha(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    fn initialized_repository() -> TempDir {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        git(root, &["init", "-b", "master"]);
        git(root, &["config", "user.email", "test@example.invalid"]);
        git(root, &["config", "user.name", "Test User"]);
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "-m", "base"]);
        git(root, &["branch", "feature"]);
        git(root, &["switch", "feature"]);
        temporary
    }

    #[test]
    fn resolves_linguist_language_with_git_attribute_semantics() {
        let repository = initialized_repository();
        fs::write(
            repository.path().join(".gitattributes"),
            "*.inc linguist-language=PHP\nplain.inc -linguist-language\n",
        )
        .unwrap();
        let git = GitRepository::open(repository.path());
        assert_eq!(
            git.linguist_language(&StoredPath::from("source.inc"))
                .unwrap()
                .as_deref(),
            Some("PHP")
        );
        assert_eq!(
            git.linguist_language(&StoredPath::from("plain.inc"))
                .unwrap(),
            None
        );
        assert!(matches!(
            git.linguist_language(&StoredPath::from("../outside.inc")),
            Err(GitError::UnsafeRepositoryPath { .. })
        ));
    }

    #[derive(Debug)]
    struct MutatingExecutor {
        root: PathBuf,
        aggregate_reads: AtomicUsize,
        mutate_every_aggregate: bool,
    }

    impl GitExecutor for MutatingExecutor {
        fn execute(&self, command: &GitCommand) -> Result<GitOutput, GitError> {
            let output = ProcessGitExecutor.execute(command)?;
            let aggregate_diff = command
                .arguments
                .first()
                .is_some_and(|argument| argument == "diff")
                && command
                    .arguments
                    .iter()
                    .any(|argument| argument == "--binary")
                && command.arguments.iter().any(|argument| {
                    let value = argument.to_string_lossy();
                    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
            if aggregate_diff {
                let read = self.aggregate_reads.fetch_add(1, Ordering::SeqCst);
                if self.mutate_every_aggregate || read == 0 {
                    let next = if read % 2 == 0 {
                        "second generation\n"
                    } else {
                        "first generation\n"
                    };
                    fs::write(self.root.join("tracked.txt"), next).map_err(|source| {
                        GitError::File {
                            path: self.root.join("tracked.txt"),
                            source,
                        }
                    })?;
                }
            }
            Ok(output)
        }
    }

    #[test]
    fn discovery_ignores_non_repositories_and_does_not_descend_into_a_repo() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::create_dir_all(root.join("c/ordinary")).unwrap();
        git(&root.join("a"), &["init"]);
        git(&root.join("b"), &["init"]);
        fs::create_dir_all(root.join("a/nested/.git")).unwrap();
        let repositories = discover_repositories(root, &DiscoveryConfig::default()).unwrap();
        assert_eq!(repositories.len(), 2);
        assert_eq!(repositories[0].relative_path, PathBuf::from("a"));
        assert_eq!(repositories[1].relative_path, PathBuf::from("b"));
    }

    #[test]
    fn discovery_fixture_handles_one_hundred_repositories_and_non_repo_siblings() {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        for index in 0..100 {
            let repository = root.join(format!("repo-{index:03}"));
            fs::create_dir_all(&repository).unwrap();
            git(&repository, &["init", "--quiet"]);
        }
        for index in 0..25 {
            fs::create_dir_all(root.join(format!("ordinary-{index:03}/nested"))).unwrap();
        }

        let repositories = discover_repositories(
            root,
            &DiscoveryConfig {
                max_depth: 2,
                ..DiscoveryConfig::default()
            },
        )
        .unwrap();
        assert_eq!(repositories.len(), 100);
        assert_eq!(
            repositories.first().unwrap().relative_path,
            PathBuf::from("repo-000")
        );
        assert_eq!(
            repositories.last().unwrap().relative_path,
            PathBuf::from("repo-099")
        );
    }

    #[test]
    fn capture_includes_all_local_change_categories_without_mutation() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(root, &["commit", "-am", "committed change"]);
        fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(root, &["add", "staged.txt"]);
        fs::write(root.join("tracked.txt"), "unstaged\n").unwrap();
        fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("ignored.txt"), "ignored\n").unwrap();
        let index_before = Command::new("git")
            .current_dir(root)
            .args(["write-tree"])
            .output()
            .unwrap()
            .stdout;
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        let index_after = Command::new("git")
            .current_dir(root)
            .args(["write-tree"])
            .output()
            .unwrap()
            .stdout;
        assert_eq!(
            index_before, index_after,
            "capture must not mutate the index"
        );
        assert!(String::from_utf8_lossy(&captured.committed_patch).contains("committed"));
        assert!(String::from_utf8_lossy(&captured.staged_patch).contains("staged.txt"));
        assert!(String::from_utf8_lossy(&captured.unstaged_patch).contains("unstaged"));
        assert_eq!(
            captured.comparison.untracked_files.len(),
            2,
            ".gitignore and untracked file are included"
        );
        assert!(!captured
            .comparison
            .untracked_files
            .iter()
            .any(|file| file.path.as_str() == "ignored.txt"));
        assert_eq!(
            fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "unstaged\n"
        );
    }

    #[test]
    fn one_hundred_changed_file_fixture_is_captured_without_eager_noise() {
        let temporary = initialized_repository();
        let root = temporary.path();
        for index in 0..100 {
            fs::write(
                root.join(format!("changed-{index:03}.txt")),
                format!("review fixture {index}\n"),
            )
            .unwrap();
        }
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        assert_eq!(captured.comparison.untracked_files.len(), 100);
        assert_eq!(captured.captured_untracked_files.len(), 100);
        assert!(captured
            .comparison
            .untracked_files
            .iter()
            .all(|file| file.byte_len < 64));
    }

    #[test]
    fn aggregate_patch_represents_the_final_worktree_not_concatenated_layers() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        git(root, &["commit", "-am", "commit layer"]);
        fs::write(root.join("tracked.txt"), "staged\n").unwrap();
        git(root, &["add", "tracked.txt"]);
        fs::write(root.join("tracked.txt"), "working tree\n").unwrap();

        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        let aggregate = String::from_utf8(captured.working_tree_patch).unwrap();

        assert!(aggregate.contains("+working tree"));
        assert!(!aggregate.contains("+committed\n"));
        assert!(!aggregate.contains("+staged\n"));
        assert!(String::from_utf8_lossy(&captured.committed_patch).contains("+committed"));
        assert!(String::from_utf8_lossy(&captured.staged_patch).contains("+staged"));
        assert!(String::from_utf8_lossy(&captured.unstaged_patch).contains("+working tree"));
    }

    #[test]
    fn untracked_bytes_are_immutable_after_capture() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::write(root.join("draft.txt"), "captured bytes\n").unwrap();
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        fs::write(root.join("draft.txt"), "changed after capture\n").unwrap();

        let untracked = captured
            .captured_untracked_files
            .iter()
            .find(|file| file.metadata.path.as_str() == "draft.txt")
            .unwrap();
        assert_eq!(untracked.content, b"captured bytes\n");
        assert_eq!(
            untracked.metadata.fingerprint,
            ContentFingerprint::from_bytes(b"captured bytes\n")
        );
    }

    #[test]
    fn tracked_target_bytes_are_immutable_after_capture() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::write(root.join("tracked.txt"), "captured target\n").unwrap();
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        fs::write(root.join("tracked.txt"), "changed after capture\n").unwrap();
        let tracked = captured
            .captured_tracked_files
            .iter()
            .find(|file| file.path.as_str() == "tracked.txt")
            .unwrap();
        assert_eq!(tracked.content.as_deref(), Some(&b"captured target\n"[..]));
    }

    #[test]
    fn capture_retries_when_patch_and_retained_bytes_observe_different_generations() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::write(root.join("tracked.txt"), "first generation\n").unwrap();
        let resolver = GitRepository::open(root);
        let resolved = resolver
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let repository = GitRepository::with_executor(
            root,
            MutatingExecutor {
                root: root.to_path_buf(),
                aggregate_reads: AtomicUsize::new(0),
                mutate_every_aggregate: false,
            },
        );

        let captured = repository.capture_local_comparison(resolved).unwrap();
        let patch = String::from_utf8(captured.working_tree_patch).unwrap();
        let tracked = captured
            .captured_tracked_files
            .iter()
            .find(|file| file.path.as_str() == "tracked.txt")
            .unwrap();
        assert!(patch.contains("+second generation"));
        assert_eq!(
            tracked.content.as_deref(),
            Some(&b"second generation\n"[..])
        );
        assert!(repository.executor.aggregate_reads.load(Ordering::SeqCst) >= 4);
    }

    #[test]
    fn capture_fails_cleanly_when_repository_never_stabilizes() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::write(root.join("tracked.txt"), "first generation\n").unwrap();
        let resolver = GitRepository::open(root);
        let resolved = resolver
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let repository = GitRepository::with_executor(
            root,
            MutatingExecutor {
                root: root.to_path_buf(),
                aggregate_reads: AtomicUsize::new(0),
                mutate_every_aggregate: true,
            },
        );

        assert!(matches!(
            repository.capture_local_comparison(resolved),
            Err(GitError::ConcurrentModification {
                attempts: MAX_COHERENT_CAPTURE_ATTEMPTS
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn untracked_symlinks_are_rejected_without_following_their_target() {
        use std::os::unix::fs::symlink;

        let temporary = initialized_repository();
        let root = temporary.path();
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "must not be read").unwrap();
        symlink(&secret, root.join("outside-link.txt")).unwrap();
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        assert!(matches!(
            repository.capture_local_comparison(resolved),
            Err(GitError::UnsafeRepositoryPath { .. })
        ));
    }

    #[test]
    fn porcelain_parser_understands_rename_and_untracked_records() {
        let parsed = parse_porcelain_v1_z(b"R  new.rs\0old.rs\0?? hello world.txt\0").unwrap();
        assert_eq!(parsed[0].kind, WorkingTreeChangeKind::Renamed);
        assert_eq!(parsed[0].original_path.as_ref().unwrap().as_str(), "old.rs");
        assert_eq!(parsed[1].kind, WorkingTreeChangeKind::Untracked);
        assert_eq!(parsed[1].path.as_str(), "hello world.txt");
    }

    #[test]
    fn ahead_behind_reads_direct_base_divergence_without_capture() {
        let temporary = initialized_repository();
        let root = temporary.path();
        git(root, &["checkout", "-b", "review-branch"]);
        fs::write(root.join("tracked.txt"), "review branch change\n").unwrap();
        git(root, &["commit", "-am", "review branch change"]);

        let divergence = GitRepository::open(root)
            .ahead_behind(&BaseReference::new("master").unwrap())
            .unwrap();
        assert_eq!(divergence.ahead, 1);
        assert_eq!(divergence.behind, 0);
    }

    #[test]
    fn raw_manifest_keeps_space_and_unicode_paths_and_no_hunk_statuses() {
        let raw = b":100644 100644 aaaaaaa bbbbbbb R100\0old name \xc3\xbc.txt\0new name \xc3\xbc.txt\0:100644 100755 aaaaaaa aaaaaaa M\0script with space.sh\0:160000 160000 aaaaaaa bbbbbbb M\0vendor/module\0";
        let entries = parse_raw_diff_z(raw).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, CapturedTrackedFileKind::Renamed);
        assert_eq!(entries[0].path.as_str(), "new name ü.txt");
        assert_eq!(
            entries[0].old_path.as_ref().unwrap().as_str(),
            "old name ü.txt"
        );
        assert_eq!(entries[1].kind, CapturedTrackedFileKind::ModeChanged);
        assert_eq!(entries[2].old_mode, 0o160000);
    }

    #[test]
    fn capture_retains_binary_rename_deleted_and_mode_only_records() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::rename(root.join("tracked.txt"), root.join("renamed file ü.txt")).unwrap();
        fs::write(root.join("binary.dat"), [0_u8, 1, 2, 3]).unwrap();
        git(root, &["add", "binary.dat"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = root.join("renamed file ü.txt");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(root, &["add", "-A"]);
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        assert!(captured
            .captured_tracked_files
            .iter()
            .any(|file| file.path.as_str() == "binary.dat" && file.binary));
        assert!(captured.captured_tracked_files.iter().any(|file| {
            file.path.as_str() == "renamed file ü.txt"
                && (file.kind == CapturedTrackedFileKind::Renamed
                    || file.kind == CapturedTrackedFileKind::ModeChanged)
        }));
    }

    #[test]
    fn capture_represents_deleted_tracked_file_without_reading_worktree() {
        let temporary = initialized_repository();
        let root = temporary.path();
        fs::remove_file(root.join("tracked.txt")).unwrap();
        let repository = GitRepository::open(root);
        let resolved = repository
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("master").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        let captured = repository.capture_local_comparison(resolved).unwrap();
        let deleted = captured
            .captured_tracked_files
            .iter()
            .find(|file| file.path.as_str() == "tracked.txt")
            .unwrap();
        assert_eq!(deleted.kind, CapturedTrackedFileKind::Deleted);
        assert_eq!(deleted.content, None);
    }

    #[test]
    fn blame_is_pinned_to_a_sha_and_handles_unusual_repository_paths() {
        let temporary = initialized_repository();
        let root = temporary.path();
        let path = "odd name ü.txt";
        fs::write(root.join(path), "first\nsecond\n").unwrap();
        git(root, &["add", path]);
        git(root, &["commit", "-m", "add unusual path"]);
        let revision = revision(root, "HEAD");
        let repository = GitRepository::open(root);
        let result = repository
            .blame_at(GitBlameRequest {
                revision: revision.clone(),
                path: StoredPath::from(path),
                start_line: 1,
                end_line: 2,
            })
            .unwrap();
        assert_eq!(result.lines.len(), 2);
        assert!(result
            .lines
            .iter()
            .all(|line| line.revision == revision && line.source_path.as_str() == path));
        assert_eq!(result.lines[1].source, "second");
        assert!(matches!(
            repository.blame_at(GitBlameRequest {
                revision,
                path: StoredPath::from("../outside"),
                start_line: 1,
                end_line: 1,
            }),
            Err(GitError::UnsafeRepositoryPath { .. })
        ));
    }

    #[test]
    fn commit_context_filters_metadata_without_changing_the_range_and_checks_selection() {
        let temporary = initialized_repository();
        let root = temporary.path();
        let base = revision(root, "master");
        fs::write(root.join("tracked.txt"), "first change\n").unwrap();
        git(root, &["commit", "-am", "first review change"]);
        let first = revision(root, "HEAD");
        fs::write(root.join("tracked.txt"), "second change\n").unwrap();
        git(root, &["commit", "-am", "second review change"]);
        let head = revision(root, "HEAD");
        let range = GitCommitRange {
            merge_base: base.clone(),
            head: head.clone(),
        };
        let repository = GitRepository::open(root);
        let context = repository
            .commit_context(
                range.clone(),
                GitCommitContextRequest {
                    max_entries: 1,
                    subject_contains: Some("review".into()),
                    selected_commit: Some(head.clone()),
                    ..GitCommitContextRequest::default()
                },
            )
            .unwrap();
        assert_eq!(context.range, range);
        assert!(context.truncated);
        assert_eq!(context.commits.len(), 1);
        assert_eq!(context.selected_commit.unwrap().summary.sha, head);
        assert!(matches!(
            repository.commit_context(
                GitCommitRange {
                    merge_base: base.clone(),
                    head: first,
                },
                GitCommitContextRequest {
                    selected_commit: Some(base),
                    ..GitCommitContextRequest::default()
                }
            ),
            Err(GitError::CommitOutsideComparisonRange { .. })
        ));
    }

    #[test]
    fn classification_marks_generated_vendor_lock_binary_lfs_and_submodule_without_path_loss() {
        let generated = classify_review_file(
            &StoredPath::from("vendor/generated/odd name ü.generated.rs"),
            Some(b"// @generated\nfn generated() {}\n"),
            false,
        )
        .unwrap();
        assert!(generated.generated);
        assert!(generated.vendored);

        let binary = classify_review_file(
            &StoredPath::from("assets/blob.bin"),
            Some(&[0, 1, 2]),
            false,
        )
        .unwrap();
        assert!(binary.binary);
        let lfs = classify_review_file(
            &StoredPath::from("large.dat"),
            Some(b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\n"),
            false,
        )
        .unwrap();
        assert!(lfs.lfs_pointer);
        let submodule =
            classify_review_file(&StoredPath::from("third_party/module"), None, true).unwrap();
        assert!(submodule.submodule && submodule.vendored);
        assert!(classify_review_file(&StoredPath::from("../escape"), None, false).is_err());
        assert!(
            classify_review_file(&StoredPath::from("Cargo.lock"), None, false)
                .unwrap()
                .lockfile
        );
    }

    #[test]
    fn remote_urls_have_one_durable_identity() {
        assert_eq!(
            normalize_remote_url("git@GitHub.com:owner/repo.git"),
            "github.com/owner/repo"
        );
        assert_eq!(
            normalize_remote_url("https://token@github.com/owner/repo/"),
            "github.com/owner/repo"
        );
    }
}
