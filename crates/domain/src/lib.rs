//! Durable, presentation-independent types shared by the desktop, CLI, and
//! remote companion. This crate deliberately has no Tauri, Git, or database
//! dependency so serialized review records remain portable between clients.

use std::{fmt, path::Path};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Clone,
            Copy,
            Debug,
            Default,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(RepositoryId);
id_type!(ReviewSessionId);
id_type!(ComparisonId);
id_type!(ReviewFileId);
id_type!(AnnotationSetId);
id_type!(AnnotationId);
id_type!(PromptExportId);
id_type!(PublicationId);

/// A path persisted in the application database. Paths are normalized by the
/// service at the boundary and never used as shell fragments.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StoredPath(String);

impl StoredPath {
    #[must_use]
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        Self(path.to_string_lossy().into_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_absolute(&self) -> bool {
        Path::new(&self.0).is_absolute()
    }
}

impl fmt::Display for StoredPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<String> for StoredPath {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for StoredPath {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceSource {
    LocalDirectory {
        root: StoredPath,
    },
    PullRequest {
        url: String,
        owner: String,
        repository: String,
        number: u64,
        worktree: StoredPath,
    },
    RemoteDirectory {
        host: String,
        root: StoredPath,
    },
    RemotePullRequest {
        host: String,
        url: String,
        owner: String,
        repository: String,
        number: u64,
        root: StoredPath,
    },
}

impl WorkspaceSource {
    #[must_use]
    pub fn tags(&self) -> Vec<WorkspaceSourceTag> {
        match self {
            Self::LocalDirectory { .. } => vec![WorkspaceSourceTag::Local],
            Self::PullRequest { .. } => vec![WorkspaceSourceTag::GitHub],
            Self::RemoteDirectory { .. } => vec![WorkspaceSourceTag::Ssh],
            Self::RemotePullRequest { .. } => {
                vec![WorkspaceSourceTag::GitHub, WorkspaceSourceTag::Ssh]
            }
        }
    }

    #[must_use]
    pub fn root(&self) -> &StoredPath {
        match self {
            Self::LocalDirectory { root }
            | Self::RemoteDirectory { root, .. }
            | Self::RemotePullRequest { root, .. } => root,
            Self::PullRequest { worktree, .. } => worktree,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSourceTag {
    GitHub,
    Local,
    Ssh,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub display_name: String,
    pub source: WorkspaceSource,
    pub default_base: BaseReference,
    pub pinned: bool,
    pub archived_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub id: RepositoryId,
    pub workspace_id: WorkspaceId,
    /// Always relative to the workspace root; `.` is valid for a root repository.
    pub relative_path: StoredPath,
    pub worktree_path: StoredPath,
    pub git_common_dir: Option<StoredPath>,
    pub normalized_primary_remote: Option<String>,
    pub enabled: bool,
    pub base_override: Option<BaseReference>,
    pub current_branch: HeadState,
    pub last_resolved_base_sha: Option<GitSha>,
    pub last_fetch_at: Option<DateTime<Utc>>,
    pub last_fetch_error: Option<String>,
    pub discovery_error: Option<String>,
    /// The last explicit review/setup comparison failure for this repository.
    /// It is independent of a pinned successful generation so sibling
    /// repositories can continue to be reviewed safely.
    #[serde(default)]
    pub comparison_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseReference(String);

impl BaseReference {
    pub const DEFAULT: &'static str = "origin/master";

    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        // A baseline is passed to Git as a revision argument.  It is never a
        // shell string, but Git revision expressions (for example `main^` or
        // `main:path`) still have surprising semantics and option-looking
        // values can alter command parsing.  LocalReview accepts ordinary
        // branch/remote/tag names and immutable object IDs only.  This is the
        // same intentionally conservative grammar used by the local/remote
        // forwarding protocol, kept here so GUI, config, CLI and SSH callers
        // share one boundary.
        if value.trim().is_empty()
            || value.len() > 512
            || value.starts_with('-')
            || value.contains('\0')
            || value.contains("..")
            || value.ends_with('.')
            || value.contains("@{")
            || value.chars().any(|character| {
                character.is_whitespace()
                    || matches!(character, '~' | '^' | ':' | '\\' | '?' | '*' | '[')
            })
        {
            return Err(DomainError::InvalidBaseReference);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for BaseReference {
    fn default() -> Self {
        // This literal is a validated product default.
        Self(Self::DEFAULT.to_owned())
    }
}

impl fmt::Display for BaseReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "value")]
pub enum HeadState {
    Branch(String),
    Detached(GitSha),
    Unborn,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitSha(String);

impl GitSha {
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let valid =
            (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit());
        if valid {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(DomainError::InvalidGitSha)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GitSha {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonOptions {
    pub ignore_all_whitespace: bool,
    pub ignore_space_at_eol: bool,
    pub ignore_cr_at_eol: bool,
    pub path_filters: Vec<StoredPath>,
}

/// The effective whitespace rule for a comparison. Git accepts several
/// whitespace switches at once, but `--ignore-all-space` subsumes the more
/// narrow end-of-line-space rule. Exposing the resolved rule keeps settings
/// summaries and exported review metadata honest without changing the caller's
/// serialized intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhitespaceComparisonMode {
    Exact,
    IgnoreAll,
    IgnoreSpaceAtEol,
}

/// Carriage-return handling is deliberately independent from whitespace
/// handling. `IgnoreCarriageReturnAtEol` maps to Git's
/// `--ignore-cr-at-eol`; it does not normalize arbitrary line endings or file
/// bytes outside line ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEndingComparisonMode {
    Respect,
    IgnoreCarriageReturnAtEol,
}

/// Classification facts captured for a review file. The booleans are not
/// mutually exclusive: for example, a generated lock file can be both
/// `generated` and `lockfile`. Consumers can choose their own filtering and
/// badging policy without re-running fragile path heuristics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFileClassification {
    pub generated: bool,
    pub vendored: bool,
    pub lockfile: bool,
    pub binary: bool,
    pub lfs_pointer: bool,
    pub submodule: bool,
}

impl ReviewFileClassification {
    #[must_use]
    pub fn requires_non_text_presentation(&self) -> bool {
        self.binary || self.lfs_pointer || self.submodule
    }

    #[must_use]
    pub fn has_review_attention_hint(&self) -> bool {
        self.generated || self.vendored || self.lockfile || self.requires_non_text_presentation()
    }
}

impl ComparisonOptions {
    /// Bounds keep an accidentally broad CLI/API request from creating an
    /// unbounded argument vector, while path validation keeps options safe to
    /// pass after Git's `--` pathspec separator.
    pub const MAX_PATH_FILTERS: usize = 2_048;

    pub fn validate(&self) -> Result<(), DomainError> {
        if self.path_filters.len() > Self::MAX_PATH_FILTERS {
            return Err(DomainError::TooManyComparisonPathFilters {
                limit: Self::MAX_PATH_FILTERS,
            });
        }
        for path in &self.path_filters {
            if !is_safe_repository_relative_path(path) {
                return Err(DomainError::InvalidComparisonPathFilter { path: path.clone() });
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn whitespace_mode(&self) -> WhitespaceComparisonMode {
        if self.ignore_all_whitespace {
            WhitespaceComparisonMode::IgnoreAll
        } else if self.ignore_space_at_eol {
            WhitespaceComparisonMode::IgnoreSpaceAtEol
        } else {
            WhitespaceComparisonMode::Exact
        }
    }

    #[must_use]
    pub fn line_ending_mode(&self) -> LineEndingComparisonMode {
        if self.ignore_cr_at_eol {
            LineEndingComparisonMode::IgnoreCarriageReturnAtEol
        } else {
            LineEndingComparisonMode::Respect
        }
    }
}

/// Shared validation for all repository-relative paths that can become Git
/// pathspec arguments. A `StoredPath` intentionally supports absolute paths
/// for workspace records, so callers must use this explicit helper at the
/// repository boundary.
#[must_use]
pub fn is_safe_repository_relative_path(path: &StoredPath) -> bool {
    let value = Path::new(path.as_str());
    !path.as_str().is_empty()
        && !path.as_str().contains('\0')
        // A leading colon activates Git pathspec magic even after `--`; this
        // app's stored paths are literal repository paths, never pathspec
        // expressions.
        && !path.as_str().starts_with(':')
        && !value.is_absolute()
        && !value.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

/// The inputs used to resolve a repository's requested base. Keeping these
/// fields separate makes inherited-vs-overridden state explainable in the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineRequest {
    pub application_default: BaseReference,
    pub workspace_default: Option<BaseReference>,
    pub repository_override: Option<BaseReference>,
    pub temporary_override: Option<BaseReference>,
}

impl BaselineRequest {
    #[must_use]
    pub fn effective(&self) -> ResolvedBaseline {
        if let Some(reference) = &self.temporary_override {
            return ResolvedBaseline {
                reference: reference.clone(),
                source: BaselineSource::TemporaryReviewOverride,
            };
        }
        if let Some(reference) = &self.repository_override {
            return ResolvedBaseline {
                reference: reference.clone(),
                source: BaselineSource::RepositoryOverride,
            };
        }
        if let Some(reference) = &self.workspace_default {
            return ResolvedBaseline {
                reference: reference.clone(),
                source: BaselineSource::WorkspaceDefault,
            };
        }
        ResolvedBaseline {
            reference: self.application_default.clone(),
            source: BaselineSource::ApplicationDefault,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedBaseline {
    pub reference: BaseReference,
    pub source: BaselineSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineSource {
    TemporaryReviewOverride,
    RepositoryOverride,
    WorkspaceDefault,
    ApplicationDefault,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComparisonRequest {
    pub id: ComparisonId,
    pub repository_id: RepositoryId,
    pub requested_base: ResolvedBaseline,
    pub options: ComparisonOptions,
    pub captured_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryComparison {
    pub id: ComparisonId,
    pub repository_id: RepositoryId,
    pub requested_base: BaseReference,
    pub base_tip_sha: GitSha,
    pub merge_base_sha: GitSha,
    pub head_sha: Option<GitSha>,
    pub head: HeadState,
    pub index_fingerprint: ContentFingerprint,
    pub working_tree_fingerprint: ContentFingerprint,
    pub untracked_files: Vec<UntrackedFile>,
    pub options: ComparisonOptions,
    pub captured_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrackedFile {
    pub path: StoredPath,
    pub fingerprint: ContentFingerprint,
    pub byte_len: u64,
    pub binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentFingerprint(String);

impl ContentFingerprint {
    #[must_use]
    pub fn from_bytes(value: &[u8]) -> Self {
        Self(blake3::hash(value).to_hex().to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSession {
    pub id: ReviewSessionId,
    pub workspace_id: WorkspaceId,
    pub status: ReviewSessionStatus,
    pub started_at: DateTime<Utc>,
    pub refreshed_at: Option<DateTime<Utc>>,
    pub archived_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSessionStatus {
    Active,
    Archived,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    Old,
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    Comment,
    Question,
    Suggestion,
    FileNote,
    ReviewNote,
}

impl AnnotationKind {
    #[must_use]
    pub fn actionable(self) -> bool {
        matches!(self, Self::Comment | Self::Suggestion)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationState {
    Open,
    Resolved,
    Deleted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    LocalOnly,
    IncludedInNextReview,
    Published,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotationAnchor {
    pub comparison_id: ComparisonId,
    pub repository_id: RepositoryId,
    pub file_path: StoredPath,
    pub old_path: Option<StoredPath>,
    pub side: Option<DiffSide>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub selected_source: String,
    pub surrounding_context: String,
    pub anchor_fingerprint: ContentFingerprint,
    pub outdated: bool,
}

/// Complete, side-aware input for creating a durable line anchor. A dedicated
/// value avoids error-prone positional arguments at Tauri/CLI boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineAnchorInput {
    pub comparison_id: ComparisonId,
    pub repository_id: RepositoryId,
    pub file_path: StoredPath,
    pub side: DiffSide,
    pub start_line: u32,
    pub end_line: u32,
    pub selected_source: String,
    pub surrounding_context: String,
}

impl AnnotationAnchor {
    /// Builds a durable file-level anchor. File notes deliberately retain the
    /// immutable comparison and path, but have no source side or line range;
    /// they therefore cannot be mistaken for an inline provider comment.
    #[must_use]
    pub fn from_file(
        comparison_id: ComparisonId,
        repository_id: RepositoryId,
        file_path: StoredPath,
    ) -> Self {
        let seed = format!("{}\0file", file_path);
        Self {
            comparison_id,
            repository_id,
            file_path,
            old_path: None,
            side: None,
            start_line: None,
            end_line: None,
            selected_source: String::new(),
            surrounding_context: String::new(),
            anchor_fingerprint: ContentFingerprint::from_bytes(seed.as_bytes()),
            outdated: false,
        }
    }

    pub fn from_line(input: LineAnchorInput) -> Result<Self, DomainError> {
        if input.start_line == 0 || input.end_line < input.start_line {
            return Err(DomainError::InvalidLineRange);
        }
        let seed = format!(
            "{}\0{:?}\0{}\0{}\0{}\0{}",
            input.file_path,
            input.side,
            input.start_line,
            input.end_line,
            input.selected_source,
            input.surrounding_context
        );
        Ok(Self {
            comparison_id: input.comparison_id,
            repository_id: input.repository_id,
            file_path: input.file_path,
            old_path: None,
            side: Some(input.side),
            start_line: Some(input.start_line),
            end_line: Some(input.end_line),
            selected_source: input.selected_source,
            surrounding_context: input.surrounding_context,
            anchor_fingerprint: ContentFingerprint::from_bytes(seed.as_bytes()),
            outdated: false,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub id: AnnotationId,
    pub annotation_set_id: AnnotationSetId,
    pub kind: AnnotationKind,
    pub state: AnnotationState,
    pub publication_state: PublicationState,
    pub labels: Vec<String>,
    pub body_markdown: String,
    pub anchor: Option<AnnotationAnchor>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotationSet {
    pub id: AnnotationSetId,
    pub review_session_id: ReviewSessionId,
    pub sequence: u32,
    pub active: bool,
    pub archived_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptScope {
    AllActionable,
    AllQuestions,
    CommentsAndQuestions,
    Selected(Vec<AnnotationId>),
    FocusedQuestion(AnnotationId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptExportRecord {
    pub id: PromptExportId,
    pub review_session_id: ReviewSessionId,
    pub annotation_set_id: AnnotationSetId,
    /// All immutable sets used by this export.  The singular field above is
    /// retained as the primary/legacy source so databases written by older
    /// clients remain readable; new review-level exports populate this list.
    #[serde(default)]
    pub annotation_set_ids: Vec<AnnotationSetId>,
    pub scope: PromptScope,
    pub annotation_ids: Vec<AnnotationId>,
    pub template_version: u32,
    /// The exact Markdown handed to the user.  Exports are an audit/history
    /// artifact, not merely a recipe for rendering whatever happens to be in
    /// the active review later.  `None` is reserved for records written by
    /// pre-v2 clients, which can be regenerated from their saved scope/set.
    #[serde(default)]
    pub rendered_markdown: Option<String>,
    /// Presentation metadata is persisted with the bytes so History can
    /// reopen an export without depending on a later title/token heuristic.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub annotation_count: Option<usize>,
    #[serde(default)]
    pub estimated_tokens: Option<usize>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("a base reference must be a safe branch, tag, remote ref, or object ID")]
    InvalidBaseReference,
    #[error("a git sha must contain 7 to 64 hexadecimal characters")]
    InvalidGitSha,
    #[error("line annotations require a one-based, non-empty range")]
    InvalidLineRange,
    #[error("comparison path filter is not a safe repository-relative path: {path}")]
    InvalidComparisonPathFilter { path: StoredPath },
    #[error("comparison has more path filters than the supported limit ({limit})")]
    TooManyComparisonPathFilters { limit: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_precedence_is_explicit() {
        let base = BaseReference::default();
        let request = BaselineRequest {
            application_default: base,
            workspace_default: Some(BaseReference::new("origin/main").unwrap()),
            repository_override: Some(BaseReference::new("origin/release").unwrap()),
            temporary_override: Some(BaseReference::new("abc1234").unwrap()),
        };
        let resolved = request.effective();
        assert_eq!(resolved.reference.as_str(), "abc1234");
        assert_eq!(resolved.source, BaselineSource::TemporaryReviewOverride);
    }

    #[test]
    fn base_references_allow_named_or_immutable_revisions_but_not_expressions() {
        for value in ["origin/main", "refs/heads/release-1", "v2.4.0", "a1b2c3d"] {
            assert!(BaseReference::new(value).is_ok(), "{value}");
        }
        for value in [
            "--upload-pack=x",
            "main^",
            "main~1",
            "main:path",
            "main..other",
            "main@{1}",
        ] {
            assert!(BaseReference::new(value).is_err(), "{value}");
        }
    }

    #[test]
    fn line_anchor_is_side_aware_and_stable() {
        let comparison = ComparisonId::new();
        let repository = RepositoryId::new();
        let first = AnnotationAnchor::from_line(LineAnchorInput {
            comparison_id: comparison,
            repository_id: repository,
            file_path: StoredPath::from("src/lib.rs"),
            side: DiffSide::New,
            start_line: 12,
            end_line: 14,
            selected_source: "selected".into(),
            surrounding_context: "context".into(),
        })
        .unwrap();
        let second = AnnotationAnchor::from_line(LineAnchorInput {
            comparison_id: comparison,
            repository_id: repository,
            file_path: StoredPath::from("src/lib.rs"),
            side: DiffSide::New,
            start_line: 12,
            end_line: 14,
            selected_source: "selected".into(),
            surrounding_context: "context".into(),
        })
        .unwrap();
        assert_eq!(first.anchor_fingerprint, second.anchor_fingerprint);
        assert_eq!(first.side, Some(DiffSide::New));
    }

    #[test]
    fn file_anchor_is_explicitly_not_an_inline_line_anchor() {
        let anchor = AnnotationAnchor::from_file(
            ComparisonId::new(),
            RepositoryId::new(),
            StoredPath::from("src/lib.rs"),
        );
        assert_eq!(anchor.side, None);
        assert_eq!(anchor.start_line, None);
        assert_eq!(anchor.end_line, None);
        assert!(anchor.selected_source.is_empty());
    }

    #[test]
    fn comparison_options_explain_whitespace_and_reject_pathspec_magic() {
        let exact = ComparisonOptions::default();
        assert_eq!(exact.whitespace_mode(), WhitespaceComparisonMode::Exact);
        assert_eq!(exact.line_ending_mode(), LineEndingComparisonMode::Respect);

        let options = ComparisonOptions {
            ignore_all_whitespace: true,
            ignore_space_at_eol: true,
            ignore_cr_at_eol: true,
            path_filters: vec![StoredPath::from("src/odd name ü.rs")],
        };
        assert_eq!(
            options.whitespace_mode(),
            WhitespaceComparisonMode::IgnoreAll
        );
        assert_eq!(
            options.line_ending_mode(),
            LineEndingComparisonMode::IgnoreCarriageReturnAtEol
        );
        options.validate().unwrap();

        for unsafe_path in ["../outside", "/absolute", ":(glob)**/*.rs", ""] {
            let invalid = ComparisonOptions {
                path_filters: vec![StoredPath::from(unsafe_path)],
                ..ComparisonOptions::default()
            };
            assert!(matches!(
                invalid.validate(),
                Err(DomainError::InvalidComparisonPathFilter { .. })
            ));
        }
    }

    #[test]
    fn classification_retains_overlapping_facts() {
        let classification = ReviewFileClassification {
            generated: true,
            lockfile: true,
            binary: true,
            ..ReviewFileClassification::default()
        };
        assert!(classification.has_review_attention_hint());
        assert!(classification.requires_non_text_presentation());
    }
}
