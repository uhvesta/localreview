use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::Utc;
use localreview_diff::{
    document_from_sources, full_file_base_rows, full_file_current_rows, DiffLineKind, FullFileRow,
    ReviewDiffDocument, ReviewFile, ReviewFileStatus,
};
use localreview_difftastic::{
    DifftasticAdapter, DifftasticBackground, DifftasticCancellation, DifftasticDisplay,
    DifftasticInput, DifftasticOutcome, DifftasticPolicy,
    DifftasticPresentation as NativeDifftasticPresentation, DifftasticRequest, SidecarLocation,
};
use localreview_domain::{
    Annotation, AnnotationAnchor, AnnotationId, AnnotationKind, AnnotationSet, AnnotationSetId,
    AnnotationState, BaseReference, BaselineRequest, BaselineSource, ComparisonId,
    ComparisonOptions, ContentFingerprint, DiffSide, GitSha, HeadState, PromptExportId,
    PromptExportRecord, PromptScope, PublicationState, Repository, RepositoryComparison,
    RepositoryId, ReviewFileId, ReviewSession, ReviewSessionId, ReviewSessionStatus, StoredPath,
    UntrackedFile, Workspace, WorkspaceId, WorkspaceSource,
};
use localreview_git::DiscoveryConfig;
use localreview_highlight::{
    resolve_language, HighlightCacheConfig, HighlightCancellation, HighlightPolicy,
    HighlightRequest, HighlightService, HighlightStatus, HighlightTheme,
};
use localreview_persistence::{
    BackupPolicy, PersistenceDiagnostics, RemoteReviewReplacementPromotion, StateStore,
};
use localreview_protocol::{
    validate_ssh_target, AgentErrorCode, AgentNotification, AgentOperation, AgentProgressPhase,
    AgentResult, DoctorReport, LocalCommand, LocalResponse, RemoteCapturedFile,
    RemoteComparisonCapture, RemoteComparisonOptions, RemoteFileStatus, RemoteHead,
    RemoteRepository, RemoteSourceRevision, RemoteSourceWindow, WorkspaceSourceTag,
    WorkspaceSummary, MAX_REMOTE_SOURCE_WINDOW_LINES, PROTOCOL_VERSION,
};
use localreview_service::{
    prompt_title_for_scope, CapturedBlameRequest, CapturedCommitContextRequest,
    ChangedSincePreviousReviewRequest, FinishGitHubReviewRequest, FormattedPrompt,
    OpenGitHubPullRequestRequest, OpenLocalWorkspaceRequest, PersistedReviewDocument, PromptEntry,
    PromptFormattingOptions, PromptPathStyle, PromptRequest, ReviewService, StartReviewRequest,
    MAX_CHANGED_SINCE_PREVIOUS_REVIEW_FILES,
};
use localreview_ssh::{
    CompanionBootstrapper, CompanionProbe, ForwardedRemoteOpen, ManagedForwardEnvironment,
    RemoteAgentProgram, ReverseForwardError, ReverseForwardListener, ReverseTunnel,
    SshCancellation, SshConnectionConfig, SshConnectionState, SshDestination, SshSession,
};
use localreview_tools::git_executable;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};
use uuid::Uuid;

const SETTINGS_KEY: &str = "ui.settings.v1";
const APPLICATION_BASE_KEY: &str = "review.application_base.v1";
/// Drafts deliberately live in the generic application-setting table rather
/// than the review tables. They are UI recovery data, and tying one to an
/// annotation set would either make autosave mutate review history or lose a
/// draft when a composer is reopened.
const ANNOTATION_DRAFT_KEY_PREFIX: &str = "ui.annotation_draft.v1.";
/// Remote capture manifests are intentionally stored separately from the
/// renderer payload.  They contain no source bytes: a selected file is read
/// later through capture-addressed, bounded source-window requests.
const REMOTE_WORKSPACE_KEY_PREFIX: &str = "remote.workspace.v4.";
const LEGACY_REMOTE_WORKSPACE_KEY_PREFIX: &str = "remote.workspace.v3.";
const REFRESH_AVAILABLE_EVENT: &str = "localreview://refresh-available";
const MANAGED_REVERSE_FORWARD_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MANAGED_REVERSE_FORWARD_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshAvailableEvent {
    workspace_id: String,
    refresh_available: bool,
    revision: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RefreshAvailability {
    available: bool,
    revision: u64,
    watcher_epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RefreshCaptureBoundary {
    revision: u64,
    watcher_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteTarget {
    host: String,
    root: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteFileBinding {
    file_id: String,
    file: RemoteCapturedFile,
    #[serde(default)]
    materialized: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteCaptureBinding {
    repository_id: String,
    comparison_id: String,
    capture: RemoteComparisonCapture,
    files: Vec<RemoteFileBinding>,
}

/// Durable metadata necessary to resume an SSH review after a desktop restart.
/// The live SSH transport deliberately is not serialized: reconnecting always
/// reuses the user's OpenSSH configuration and performs a fresh handshake.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteWorkspaceMetadata {
    schema_version: u16,
    target: RemoteTarget,
    review_session_id: String,
    generation: u64,
    agent_version: Option<String>,
    latency_millis: Option<u128>,
    captures: Vec<RemoteCaptureBinding>,
    #[serde(default)]
    stale: bool,
    #[serde(default)]
    last_error: Option<String>,
    /// The last explicit bootstrap probe is durable so an offline review can
    /// still explain whether its companion was compatible, incompatible, or
    /// absent and which remote platform was observed.
    #[serde(default)]
    companion: Option<RemoteCompanionStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RemoteCompanionAvailability {
    Compatible,
    Incompatible,
    MissingOrUnreachable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteCompanionPlatform {
    operating_system: String,
    architecture: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteCompanionStatus {
    availability: RemoteCompanionAvailability,
    platform: Option<RemoteCompanionPlatform>,
    detail: String,
}

struct RemoteSessionConnect {
    session: SshSession,
    companion: RemoteCompanionStatus,
    reverse_forward: Option<ManagedReverseForwardRuntime>,
}

struct RemoteConnectionFailure {
    companion: RemoteCompanionStatus,
    error: Box<DispatchError>,
}

struct PendingManagedReverseForward {
    listener: ReverseForwardListener,
    handler: ManagedForwardHandler,
    environment: ManagedForwardEnvironment,
}

#[derive(Debug)]
struct RemoteWorkspaceRuntime {
    session: SshSession,
    /// Kept beside the primary SSH child so its loopback listener, token, and
    /// remote companion relay have exactly the same lifetime as this review
    /// session. Watcher transports intentionally never receive one.
    reverse_forward: Option<ManagedReverseForwardRuntime>,
}

type ManagedForwardHandler =
    Arc<dyn Fn(ForwardedRemoteOpen) -> Result<(), String> + Send + Sync + 'static>;

/// Desktop-owned half of one managed reverse forward. The listener and token
/// live only in its worker thread; the worker exits when the paired SSH child
/// disconnects, the review is deleted, or the bounded TTL passes.
struct ManagedReverseForwardRuntime {
    cancelled: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for ManagedReverseForwardRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedReverseForwardRuntime")
            .finish_non_exhaustive()
    }
}

impl ManagedReverseForwardRuntime {
    fn start(
        listener: ReverseForwardListener,
        disconnected: Arc<AtomicBool>,
        handler: ManagedForwardHandler,
    ) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = std::thread::Builder::new()
            .name("localreview-managed-ssh-forward".into())
            .spawn(move || {
                while !worker_cancelled.load(Ordering::Acquire)
                    && !disconnected.load(Ordering::Acquire)
                {
                    match listener.accept_open_with(MANAGED_REVERSE_FORWARD_POLL, |open| {
                        handler(open.clone())
                    }) {
                        Ok(_) | Err(ReverseForwardError::TimedOut) => {}
                        // Invalid frames/tokens are rejected on their one
                        // connection but do not tear down a valid desktop
                        // review session. Expiry/disconnect does.
                        Err(ReverseForwardError::TokenRejected)
                        | Err(ReverseForwardError::Protocol(_))
                        | Err(ReverseForwardError::NonLoopbackPeer)
                        | Err(ReverseForwardError::Rejected(_)) => {}
                        Err(ReverseForwardError::Expired) => break,
                        Err(ReverseForwardError::InvalidSession)
                        | Err(ReverseForwardError::InvalidEndpoint)
                        | Err(ReverseForwardError::EntropyUnavailable(_))
                        | Err(ReverseForwardError::Io(_)) => break,
                    }
                }
            })
            .expect("could not start managed SSH reverse-forward worker");
        Self {
            cancelled,
            worker: Some(worker),
        }
    }
}

impl Drop for ManagedReverseForwardRuntime {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Debug)]
struct RemoteWatcher {
    cancelled: Arc<AtomicBool>,
    cancellation: Arc<Mutex<Option<SshCancellation>>>,
    request_id: String,
    generation: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopOperation {
    Workspace { workspace_id: String },
}

#[derive(Debug)]
pub struct DesktopController {
    service: ReviewService,
    replay_guard: Mutex<ReplayGuard>,
    presentation_cache: Mutex<PresentationCache>,
    highlight: HighlightService,
    /// Git attribute lookup is stable for an immutable comparison. Caching
    /// both hits and misses prevents virtual scrolling from spawning a
    /// `git check-attr` process for every viewport window.
    language_attribute_cache: Mutex<HashMap<(String, String), Option<String>>>,
    /// Presentation jobs share a small worker budget, but are not globally
    /// serialized.  A one-file scroll can therefore never make another file
    /// wait behind a single held mutex, while CPU/process pressure remains
    /// bounded on large workspaces.
    presentation_work: PresentationWorkPool,
    /// The newest viewport generation per file owns the expensive work. A
    /// newer viewport cooperatively cancels old Tree-sitter/Difftastic jobs;
    /// their result is never returned or inserted into a presentation cache.
    presentation_jobs: Mutex<PresentationJobRegistry>,
    refresh_available: Arc<Mutex<BTreeMap<WorkspaceId, RefreshAvailability>>>,
    local_watchers: Mutex<BTreeMap<WorkspaceId, RecommendedWatcher>>,
    /// Exactly one serialized companion transport per remote workspace. A
    /// transport is process-local; durable manifest metadata is restored on
    /// demand after restart and never causes a background full sync.
    remote_sessions: Mutex<BTreeMap<WorkspaceId, Arc<Mutex<RemoteWorkspaceRuntime>>>>,
    remote_watchers: Mutex<BTreeMap<WorkspaceId, Vec<RemoteWatcher>>>,
    app_handle: Arc<Mutex<Option<tauri::AppHandle>>>,
}

#[derive(Default, Debug)]
struct ReplayGuard {
    seen: BTreeSet<String>,
    ordered: VecDeque<(u64, String)>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewUiState {
    #[serde(default)]
    viewed_file_ids: BTreeSet<String>,
    #[serde(default)]
    active_file_id: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    full_file_side: Option<String>,
    #[serde(default)]
    nearest_source_line: Option<u32>,
    #[serde(default)]
    nearest_source_side: Option<String>,
    #[serde(default)]
    scroll_top: Option<f64>,
    #[serde(default)]
    split_ratio: Option<f64>,
    #[serde(default)]
    right_tab: Option<String>,
    /// `None` preserves the legacy behavior of selecting every open draft on
    /// first load. `Some(empty)` is an explicit user choice and must survive
    /// restart instead of being mistaken for missing state.
    #[serde(default)]
    selected_annotation_ids: Option<BTreeSet<String>>,
    /// Per-hunk requested context is persisted with the review session, so a
    /// reload never silently contracts a reviewer-expanded immutable hunk.
    #[serde(default)]
    hunk_context_lines: BTreeMap<String, u32>,
    /// Stable omitted-block ids expanded in Full File mode: deletions while
    /// viewing Current and additions while viewing Base. The persisted field
    /// keeps its original name for compatibility with existing review state.
    #[serde(default)]
    expanded_full_file_deletion_blocks: BTreeSet<String>,
    /// Current-side change blocks are open by default in Full File's `Both`
    /// projection. This inverse set records the additions a reviewer chose to
    /// collapse so that choice survives restart.
    #[serde(default)]
    collapsed_full_file_addition_blocks: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceUiStateView {
    pub active_file_id: Option<String>,
    pub mode: String,
    pub full_file_side: String,
    pub nearest_source_line: Option<u32>,
    pub nearest_source_side: Option<String>,
    pub scroll_top: f64,
    pub split_ratio: f64,
    pub right_tab: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_annotation_ids: Option<Vec<String>>,
    pub expanded_full_file_deletion_blocks: Vec<String>,
    pub collapsed_full_file_addition_blocks: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceUiStatePatch {
    pub active_file_id: Option<String>,
    pub mode: Option<String>,
    pub full_file_side: Option<String>,
    pub nearest_source_line: Option<u32>,
    pub nearest_source_side: Option<String>,
    pub scroll_top: Option<f64>,
    pub split_ratio: Option<f64>,
    pub right_tab: Option<String>,
    pub selected_annotation_ids: Option<Vec<String>>,
    pub expanded_full_file_deletion_blocks: Option<Vec<String>>,
    pub collapsed_full_file_addition_blocks: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PresentationRequest {
    pub file_id: String,
    /// Stable file IDs may occur in several immutable review generations.
    /// Native callers include the comparison so archived presentation never
    /// falls through to the newest row with the same logical file identity.
    pub comparison_id: Option<String>,
    pub mode: String,
    pub start_row: u32,
    pub end_row: u32,
    pub generation: u64,
    pub full_file_side: Option<String>,
    pub split_ratio: Option<f64>,
    /// Optional presentation-only state used while browsing a frozen review.
    /// It is validated against the owning immutable session and is never
    /// written to either the archived or active session UI-state record.
    pub ephemeral_expanded_full_file_deletion_blocks: Option<Vec<String>>,
    pub ephemeral_collapsed_full_file_addition_blocks: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentationWindow {
    pub generation: u64,
    pub mode: String,
    pub file_id: String,
    pub start_row: u32,
    pub total_rows: u32,
    pub rows: Vec<DiffRowView>,
    pub hunks: Vec<HunkLocationView>,
    pub omitted_blocks: Vec<FullFileOmittedBlockView>,
    pub old_tokens: Vec<SyntaxTokenView>,
    pub new_tokens: Vec<SyntaxTokenView>,
    pub highlight_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difftastic: Option<DifftasticPresentationView>,
}

/// A native, canonical source location. `row_index` is an index into the
/// complete presentation, not the currently loaded virtual window.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentationLocation {
    pub row_index: u32,
    pub side: String,
    pub line: u32,
}

/// A complete immutable source selection. The UI must never assemble this
/// from bounded diff rows, because a virtual window can omit context.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedSourceRange {
    pub text: String,
    pub complete: bool,
}

/// Recoverable, workspace-scoped in-progress composer data. It intentionally
/// has no annotation-set foreign key: an unsaved composer is not review
/// history and must not acquire a durable annotation ID.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnnotationDraft {
    pub id: String,
    pub workspace_id: String,
    pub file_id: String,
    pub repository_id: String,
    pub kind: String,
    pub side: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkLocationView {
    pub id: String,
    pub row_index: u32,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub header: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collapsed_context_lines: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntaxTokenView {
    pub start_byte: u32,
    pub end_byte: u32,
    pub class: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticPresentationView {
    pub status: String,
    pub display: String,
    pub start_row: u32,
    pub total_rows: u32,
    pub chunks: Vec<DifftasticChunkView>,
    pub alignment: Vec<DifftasticAlignmentView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<DifftasticFallbackView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticChunkView {
    pub rows: Vec<DifftasticRowView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticRowView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<DifftasticCellView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<DifftasticCellView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticCellView {
    pub line_number: u32,
    pub text: String,
    pub changed_spans: Vec<DifftasticSpanView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticSpanView {
    pub start: u32,
    pub end: u32,
    pub highlight: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticAlignmentView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_line: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DifftasticFallbackView {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutlineSymbolView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
    pub depth: u16,
    pub side: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CopyReviewItemRequest {
    pub kind: String,
    pub file_id: String,
    pub side: Option<String>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
}

#[derive(Clone, Debug, Default)]
struct PresentationCache {
    documents: BTreeMap<String, Arc<PersistedReviewDocument>>,
    document_order: VecDeque<String>,
    document_bytes: usize,
    canonical: BTreeMap<String, Arc<CachedCanonicalPresentation>>,
    canonical_order: VecDeque<String>,
    canonical_rows: usize,
    structural: BTreeMap<String, NativeDifftasticPresentation>,
    structural_order: VecDeque<String>,
    verified_sidecars: BTreeSet<PathBuf>,
}

const MAX_PRESENTATION_JOBS: usize = 128;
const MAX_PRESENTATION_WORKERS: usize = 4;

#[derive(Clone, Debug)]
struct PresentationJob {
    generation: u64,
    highlight: HighlightCancellation,
    difftastic: DifftasticCancellation,
}

#[derive(Clone, Debug)]
struct PresentationJobLease {
    file_id: String,
    generation: u64,
    tracked: bool,
    highlight: HighlightCancellation,
    difftastic: DifftasticCancellation,
}

impl PresentationJobLease {
    fn ephemeral(file_id: String, generation: u64) -> Self {
        Self {
            file_id,
            generation,
            tracked: false,
            highlight: HighlightCancellation::new(),
            difftastic: DifftasticCancellation::new(),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.highlight.is_cancelled() || self.difftastic.is_cancelled()
    }
}

#[derive(Default, Debug)]
struct PresentationJobRegistry {
    jobs: BTreeMap<String, PresentationJob>,
    order: VecDeque<String>,
}

impl PresentationJobRegistry {
    fn acquire(
        &mut self,
        file_id: ReviewFileId,
        generation: u64,
    ) -> Result<PresentationJobLease, DispatchError> {
        let file_id = file_id.to_string();
        // Internal source-location lookups use generation zero. They must not
        // invalidate a visible viewport job for the same immutable file.
        if generation == 0 {
            return Ok(PresentationJobLease::ephemeral(file_id, generation));
        }

        if let Some(existing) = self.jobs.get(&file_id).cloned() {
            if generation < existing.generation {
                return Err(DispatchError::Cancelled);
            }
            if generation == existing.generation {
                self.touch(&file_id);
                return Ok(PresentationJobLease {
                    file_id,
                    generation,
                    tracked: true,
                    highlight: existing.highlight,
                    difftastic: existing.difftastic,
                });
            }
            existing.highlight.cancel();
            existing.difftastic.cancel();
        }

        let job = PresentationJob {
            generation,
            highlight: HighlightCancellation::new(),
            difftastic: DifftasticCancellation::new(),
        };
        self.jobs.insert(file_id.clone(), job.clone());
        self.touch(&file_id);
        while self.order.len() > MAX_PRESENTATION_JOBS {
            if let Some(expired) = self.order.pop_front() {
                if let Some(expired_job) = self.jobs.remove(&expired) {
                    expired_job.highlight.cancel();
                    expired_job.difftastic.cancel();
                }
            }
        }
        Ok(PresentationJobLease {
            file_id,
            generation,
            tracked: true,
            highlight: job.highlight,
            difftastic: job.difftastic,
        })
    }

    fn is_current(&self, lease: &PresentationJobLease) -> bool {
        !lease.tracked
            || (!lease.is_cancelled()
                && self.jobs.get(&lease.file_id).is_some_and(|job| {
                    job.generation == lease.generation
                        && !job.highlight.is_cancelled()
                        && !job.difftastic.is_cancelled()
                }))
    }

    fn cancel_file(&mut self, file_id: ReviewFileId) {
        let file_id = file_id.to_string();
        if let Some(job) = self.jobs.remove(&file_id) {
            job.highlight.cancel();
            job.difftastic.cancel();
        }
        self.order.retain(|candidate| candidate != &file_id);
    }

    fn touch(&mut self, file_id: &str) {
        self.order.retain(|candidate| candidate != file_id);
        self.order.push_back(file_id.to_owned());
    }
}

/// A cancellation-aware semaphore for expensive native presentation work.
/// `Mutex<()>` had accidentally turned every file's syntax and structural
/// render into one global queue. This retains an explicit maximum without
/// sacrificing independent-file concurrency.
#[derive(Debug)]
struct PresentationWorkPool {
    available: Mutex<usize>,
    wake: Condvar,
    maximum: usize,
}

impl PresentationWorkPool {
    fn new(maximum: usize) -> Self {
        let maximum = maximum.max(1);
        Self {
            available: Mutex::new(maximum),
            wake: Condvar::new(),
            maximum,
        }
    }

    fn acquire<'a>(
        &'a self,
        cancellation: Option<&PresentationJobLease>,
    ) -> Result<PresentationWorkPermit<'a>, DispatchError> {
        let mut available = self.available.lock().map_err(|_| DispatchError::Internal)?;
        loop {
            if cancellation.is_some_and(PresentationJobLease::is_cancelled) {
                return Err(DispatchError::Cancelled);
            }
            if *available > 0 {
                *available -= 1;
                return Ok(PresentationWorkPermit { pool: self });
            }
            let (next, _) = self
                .wake
                .wait_timeout(available, Duration::from_millis(8))
                .map_err(|_| DispatchError::Internal)?;
            available = next;
        }
    }

    #[cfg(test)]
    fn available(&self) -> usize {
        self.available.lock().map_or(0, |available| *available)
    }
}

struct PresentationWorkPermit<'a> {
    pool: &'a PresentationWorkPool,
}

impl Drop for PresentationWorkPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut available) = self.pool.available.lock() {
            *available = available.saturating_add(1).min(self.pool.maximum);
            self.pool.wake.notify_one();
        }
    }
}

#[derive(Clone, Debug)]
struct CachedCanonicalPresentation {
    rows: Vec<DiffRowView>,
    hunks: Vec<HunkLocationView>,
    omitted_blocks: Vec<FullFileOmittedBlockView>,
}

type HighlightWindow = (
    Vec<SyntaxTokenView>,
    Vec<SyntaxTokenView>,
    String,
    Option<String>,
);

struct DifftasticWindowExecution<'a> {
    resource_dir: &'a Path,
    max_window_rows: usize,
    job: Option<&'a PresentationJobLease>,
}

struct FullFileWindowExecution<'a> {
    settings: &'a ReviewSettings,
    max_window_rows: usize,
    job: Option<&'a PresentationJobLease>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub source: Vec<String>,
    pub location: String,
    pub detail: String,
    pub default_base: String,
    pub progress: ReviewProgress,
    pub draft_count: usize,
    pub pinned: bool,
    pub refresh_available: bool,
    /// Monotonic process-local revision for refresh-availability state. The
    /// WebView uses it to reject a queued pre-capture event that arrives after
    /// the authoritative successful-refresh response.
    pub refresh_available_revision: u64,
    pub connection: String,
    /// False when repository discovery is durable but no review generation
    /// has captured successfully yet. The UI uses this explicit state to open
    /// baseline setup instead of presenting an inert, empty review.
    pub review_ready: bool,
    /// Archived workspaces are deliberately excluded from the normal rail,
    /// but their captured review/session/document rows remain browseable from
    /// durable history.  This flag makes that distinction explicit at the
    /// native boundary rather than relying on a UI-only convention.
    pub archived: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewProgress {
    pub viewed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryView {
    pub id: String,
    pub name: String,
    pub path: String,
    pub branch: String,
    pub base: String,
    pub merge_base: String,
    pub head: String,
    pub is_override: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison_options: Option<ComparisonOptions>,
}

/// Fresh, non-capturing repository information used by the Review setup
/// table.  These values are intentionally separate from the pinned
/// `RepositoryView` comparison fields: opening this table can inspect Git
/// status and refs, but it never changes the active review generation.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositorySetupView {
    pub id: String,
    pub path: String,
    pub enabled: bool,
    pub branch: String,
    pub clean: Option<bool>,
    pub changed_file_count: Option<usize>,
    pub status_summary: String,
    pub effective_base: String,
    pub suggested_base: Option<String>,
    pub base_source: String,
    pub base_override: Option<String>,
    pub resolved_base_sha: Option<String>,
    pub merge_base_sha: Option<String>,
    pub head_sha: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub last_fetch_at: Option<String>,
    pub last_fetch_error: Option<String>,
    pub discovery_error: Option<String>,
    pub comparison_error: Option<String>,
    pub status_checked_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFileView {
    pub id: String,
    pub comparison_id: String,
    pub repository_id: String,
    pub path: String,
    pub previous_path: Option<String>,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub hunk_count: usize,
    pub language: String,
    pub viewed: bool,
    pub annotation_count: usize,
}

/// Capture-time facts are deliberately exposed as a separate read-only
/// payload. This keeps the base review model compact while preventing the UI
/// from guessing classifications from a mutable checkout path.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFileClassificationView {
    pub comparison_id: String,
    pub file_id: String,
    pub path: String,
    pub classification: ReviewFileClassificationViewFacts,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewFileClassificationViewFacts {
    pub generated: bool,
    pub vendored: bool,
    pub lockfile: bool,
    pub binary: bool,
    pub lfs_pointer: bool,
    pub submodule: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapturedBlameInput {
    pub file_id: String,
    pub side: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedBlameView {
    pub comparison_id: String,
    pub side: String,
    pub lines: Vec<BlameLineView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlameLineView {
    pub revision: String,
    pub original_line: u32,
    pub final_line: u32,
    pub source_path: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: String,
    pub summary: String,
    pub source: String,
    pub source_truncated: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitContextInput {
    pub repository_id: String,
    pub max_entries: Option<usize>,
    pub include_merge_commits: Option<bool>,
    pub author_contains: Option<String>,
    pub subject_contains: Option<String>,
    pub selected_commit: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedCommitContextView {
    pub comparison_id: String,
    pub range: CommitRangeView,
    pub commits: Vec<CommitSummaryView>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_commit: Option<CommitDetailsView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitRangeView {
    pub merge_base: String,
    pub head: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitSummaryView {
    pub sha: String,
    pub parent_shas: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: String,
    pub subject: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitDetailsView {
    pub summary: CommitSummaryView,
    pub committer_name: String,
    pub committer_email: String,
    pub committed_at: String,
    pub body: String,
    pub body_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangedSincePreviousReviewView {
    pub current_comparison_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_comparison_id: Option<String>,
    pub files: Vec<PreviousReviewFileView>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviousReviewFileView {
    pub kind: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_document_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_document_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestUpdateStatusView {
    pub workspace_id: String,
    pub canonical_url: String,
    pub pinned_base_sha: String,
    pub pinned_head_sha: String,
    pub current_base_sha: String,
    pub current_head_sha: String,
    pub base_changed: bool,
    pub head_changed: bool,
    pub metadata_fetched_at: String,
}

/// Compact, explicitly camel-cased GitHub metadata for the WebView. Provider
/// records intentionally retain their storage-oriented serde names, so they
/// must not cross the Tauri boundary directly.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestContextView {
    pub canonical_url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
    pub pinned_base_sha: String,
    pub pinned_head_sha: String,
    pub draft: bool,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    pub commits: Vec<GitHubCommitContextView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubCommitContextView {
    pub sha: String,
    pub message_headline: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedGitHubReviewThreadView {
    pub id: String,
    pub resolved: bool,
    pub outdated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_line: Option<u32>,
    pub comments: Vec<ImportedGitHubReviewCommentView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedGitHubReviewCommentView {
    pub id: String,
    pub body_markdown: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedGitHubConversationCommentView {
    pub id: u64,
    pub body_markdown: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationView {
    pub id: String,
    pub file_id: String,
    pub repository_id: String,
    pub kind: String,
    pub state: String,
    pub side: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    pub selected_source: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub local_only: bool,
    pub created_at: String,
    pub published_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewHistoryItem {
    pub id: String,
    pub label: String,
    pub created_at: String,
    pub annotation_count: usize,
    #[serde(rename = "type")]
    pub item_type: String,
    pub annotations: Option<Vec<AnnotationView>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewData {
    pub workspace: WorkspaceView,
    pub repositories: Vec<RepositoryView>,
    pub files: Vec<ReviewFileView>,
    pub annotations: Vec<AnnotationView>,
    pub history: Vec<ReviewHistoryItem>,
    /// Present only on the response to an explicit local refresh. Ordinary
    /// review loads have no operation outcome to report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_outcome: Option<LocalRefreshOutcomeView>,
    /// A prior review session is a frozen browsing surface.  The frontend
    /// must not accidentally send edits into the currently active session.
    pub historical: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub historical_session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRefreshOutcomeView {
    /// `success`, `partial`, or `failed`.
    pub status: String,
    pub captured_repository_count: usize,
    pub failed_repository_count: usize,
    pub failures: Vec<LocalRefreshFailureView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRefreshFailureView {
    pub repository_id: String,
    pub repository_path: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffRowView {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunk_id: Option<String>,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub text: Option<String>,
    pub hunk: Option<String>,
    pub has_annotation: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_source_start_byte: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_source_start_byte: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_block_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_side: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_expanded: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FullFileOmittedBlockView {
    pub id: String,
    pub side: String,
    pub start_line: u32,
    pub end_line: u32,
    pub count: u32,
    pub expanded: bool,
    pub row_index: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct ReviewSettings {
    pub last_workspace_id: Option<String>,
    pub font_scale: f64,
    pub left_width: u32,
    pub right_width: u32,
    pub left_collapsed: bool,
    pub right_collapsed: bool,
    pub fetch_on_review: bool,
    pub theme: String,
    pub code_font: String,
    pub external_editor: String,
    pub tab_width: u8,
    pub show_whitespace: bool,
    pub wrap_lines: bool,
    pub vim_navigation: bool,
    pub prompt_path_style: String,
    pub prompt_include_diff_hunks: bool,
    pub prompt_include_git_state: bool,
    pub shortcuts: BTreeMap<String, String>,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            last_workspace_id: None,
            font_scale: 1.0,
            left_width: 244,
            right_width: 332,
            left_collapsed: false,
            right_collapsed: false,
            fetch_on_review: false,
            theme: "dark".into(),
            code_font: "SF Mono".into(),
            external_editor: "system".into(),
            tab_width: 2,
            show_whitespace: false,
            wrap_lines: false,
            vim_navigation: false,
            prompt_path_style: "absolute".into(),
            prompt_include_diff_hunks: false,
            prompt_include_git_state: false,
            shortcuts: BTreeMap::from([
                ("saveAnnotation".into(), "Meta+Enter".into()),
                ("nextHunk".into(), "Alt+ArrowDown".into()),
                ("previousHunk".into(), "Alt+ArrowUp".into()),
                ("commandPalette".into(), "Meta+Shift+P".into()),
                ("filePicker".into(), "Meta+P".into()),
                ("focusQuestion".into(), "Meta+Shift+Q".into()),
            ]),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenWorkspaceInput {
    pub path: String,
    pub base: Option<String>,
    #[serde(default)]
    pub repository_bases: Vec<RepositoryBaseInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenGitHubPullRequestInput {
    pub url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenSshWorkspaceInput {
    pub target: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryBaseInput {
    pub repository_id: Option<String>,
    pub repository_path: Option<String>,
    pub relative_path: Option<String>,
    pub base: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartOrRefreshInput {
    pub base: Option<String>,
    #[serde(default)]
    pub repository_bases: Vec<RepositoryBaseInput>,
    #[serde(default)]
    pub fetch_before_capture: bool,
    /// Real Git comparison switches captured with the next local generation.
    /// They are distinct from the presentation-only show-whitespace setting.
    pub comparison_options: Option<ComparisonOptions>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigureBaselinesInput {
    pub default_base: Option<String>,
    #[serde(default)]
    pub repository_bases: Vec<RepositoryBaseInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositorySelectionInput {
    pub repository_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetRepositoryInclusionInput {
    pub repository_ids: Vec<String>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplyRepositoryBaseInput {
    pub repository_ids: Vec<String>,
    pub base: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptInput {
    pub scope: String,
    #[serde(default)]
    pub annotation_ids: Vec<String>,
    /// Backward-compatible two-state path selector used by older clients.
    pub portable: Option<bool>,
    pub path_style: Option<String>,
    pub include_diff_hunks: Option<bool>,
    pub include_git_state: Option<bool>,
    pub history_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptPreview {
    pub export_id: String,
    pub title: String,
    pub content: String,
    pub annotation_count: usize,
    pub estimated_tokens: usize,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptExportSaveFormat {
    Markdown,
    Json,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPromptExport {
    pub saved: bool,
    pub format: String,
}

/// The frontend can only name a durable review-history object, never a
/// fallback selector.  Keeping this typed until the workspace ownership check
/// prevents a malformed `historyId` from quietly exporting today's active
/// annotation set.
#[derive(Clone, Copy, Debug)]
enum PromptHistoryReference {
    Set(AnnotationSetId),
    Review(ReviewSessionId),
    Export(PromptExportId),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewConclusion {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FinishReviewInput {
    #[serde(default)]
    pub annotation_ids: Vec<String>,
    pub summary: String,
    pub conclusion: ReviewConclusion,
}

/// The submit boundary accepts only the opaque token issued by the durable
/// preview. It intentionally cannot receive a replacement comment list,
/// conclusion, summary, SHA, or JSON body.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FinishReviewSubmissionInput {
    pub preview_token: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishReviewResult {
    pub review_id: String,
    pub annotation_count: usize,
    pub annotation_ids: Vec<String>,
    pub payload_json: String,
    pub preview_token: String,
    pub publication_status: String,
    pub submitted: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishReviewPreview {
    pub annotation_count: usize,
    pub annotation_ids: Vec<String>,
    pub payload_json: String,
    pub pinned_head_sha: String,
    pub preview_token: String,
    pub request_fingerprint: String,
    pub preview_request_fingerprint: String,
    pub annotation_snapshot_fingerprint: String,
    pub requires_reconciliation: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("presentation request was superseded by a newer viewport")]
    Cancelled,
    #[error("workspace not found: {0}")]
    NotFound(String),
    #[error("workspace name is ambiguous: {0}")]
    Ambiguous(String),
    #[error("durable review operation failed: {0}")]
    Service(#[from] localreview_service::ServiceError),
    #[error("durable state operation failed: {0}")]
    Persistence(#[from] localreview_persistence::PersistenceError),
    #[error("remote review operation failed: {0}")]
    Remote(String),
    #[error("internal state error")]
    Internal,
}

impl DesktopController {
    #[cfg(test)]
    pub fn new(store: StateStore) -> Self {
        Self::from_service(ReviewService::new(store))
    }

    pub fn with_global_config_path(store: StateStore, global_config_path: PathBuf) -> Self {
        Self::from_service(ReviewService::with_global_config_path(
            store,
            global_config_path,
        ))
    }

    fn from_service(service: ReviewService) -> Self {
        Self {
            service,
            replay_guard: Mutex::new(ReplayGuard::default()),
            presentation_cache: Mutex::new(PresentationCache::default()),
            highlight: HighlightService::new(
                HighlightPolicy::default(),
                HighlightCacheConfig::default(),
            ),
            language_attribute_cache: Mutex::new(HashMap::new()),
            presentation_work: PresentationWorkPool::new(MAX_PRESENTATION_WORKERS),
            presentation_jobs: Mutex::new(PresentationJobRegistry::default()),
            refresh_available: Arc::new(Mutex::new(BTreeMap::new())),
            local_watchers: Mutex::new(BTreeMap::new()),
            remote_sessions: Mutex::new(BTreeMap::new()),
            remote_watchers: Mutex::new(BTreeMap::new()),
            app_handle: Arc::new(Mutex::new(None)),
        }
    }

    pub fn attach_app_handle(&self, app_handle: tauri::AppHandle) -> Result<(), DispatchError> {
        *self
            .app_handle
            .lock()
            .map_err(|_| DispatchError::Internal)? = Some(app_handle);
        Ok(())
    }

    fn managed_forward_handler(&self, host: &str) -> Option<ManagedForwardHandler> {
        let app_handle = self.app_handle.lock().ok()?.clone()?;
        let host = host.to_owned();
        Some(Arc::new(move |open| {
            let target = format!("{}:{}", host, open.path);
            let controller = app_handle.state::<crate::AppState>().controller.clone();
            let (workspace, _) = controller
                .open_ssh_workspace(OpenSshWorkspaceInput {
                    target: target.clone(),
                })
                .map_err(|error| error.to_string())?;
            if open.base.is_some() || !open.repository_bases.is_empty() {
                let workspace_id = parse_workspace_id(&workspace.id)
                    .ok_or_else(|| "forwarded workspace id was invalid".to_owned())?;
                controller
                    .refresh_review(
                        workspace_id,
                        StartOrRefreshInput {
                            base: open.base,
                            repository_bases: open
                                .repository_bases
                                .into_iter()
                                .map(|override_| RepositoryBaseInput {
                                    repository_id: None,
                                    repository_path: None,
                                    relative_path: Some(override_.relative_path),
                                    base: Some(override_.base),
                                })
                                .collect(),
                            fetch_before_capture: false,
                            comparison_options: None,
                        },
                    )
                    .map_err(|error| error.to_string())?;
            }
            let _ = app_handle.emit(
                crate::DESKTOP_OPERATION_EVENT,
                DesktopOperation::Workspace {
                    workspace_id: workspace.id,
                },
            );
            Ok(())
        }))
    }

    #[must_use]
    pub fn state(&self) -> &StateStore {
        self.service.state()
    }

    fn acquire_presentation_job(
        &self,
        file_id: ReviewFileId,
        generation: u64,
    ) -> Result<PresentationJobLease, DispatchError> {
        self.presentation_jobs
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .acquire(file_id, generation)
    }

    fn ensure_presentation_job_current(
        &self,
        job: &PresentationJobLease,
    ) -> Result<(), DispatchError> {
        if job.is_cancelled()
            || !self
                .presentation_jobs
                .lock()
                .map_err(|_| DispatchError::Internal)?
                .is_current(job)
        {
            return Err(DispatchError::Cancelled);
        }
        Ok(())
    }

    /// Safe startup repair only prunes registrations for absent worktrees and
    /// removes clean orphaned app paths.  Dirty paths remain reported by the
    /// service/diagnostics rather than being deleted on launch.
    pub fn repair_managed_worktree_orphans(&self) -> Result<(), DispatchError> {
        let _ = self.service.repair_managed_worktree_orphans()?;
        Ok(())
    }

    pub fn dispatch(
        &self,
        command: LocalCommand,
        request_id: String,
    ) -> Result<LocalResponse, DispatchError> {
        match command {
            LocalCommand::OpenWorkspace {
                path,
                base,
                repository_bases,
            } => {
                let bases = repository_bases
                    .into_iter()
                    .map(|entry| RepositoryBaseInput {
                        repository_id: None,
                        repository_path: None,
                        relative_path: Some(entry.relative_path),
                        base: Some(entry.base),
                    })
                    .collect();
                let (workspace, created) = self.open_local_workspace(OpenWorkspaceInput {
                    path,
                    base,
                    repository_bases: bases,
                })?;
                Ok(LocalResponse::Opened {
                    request_id,
                    workspace: protocol_summary(&workspace),
                    created,
                })
            }
            LocalCommand::FocusWorkspace { selector } => {
                let workspace = self.focus_workspace(&selector)?;
                Ok(LocalResponse::Focused {
                    request_id,
                    workspace: protocol_summary(&workspace),
                })
            }
            LocalCommand::OpenPullRequest { url } => {
                let (workspace, created) =
                    self.open_github_pull_request(OpenGitHubPullRequestInput { url })?;
                Ok(LocalResponse::Opened {
                    request_id,
                    workspace: protocol_summary(&workspace),
                    created,
                })
            }
            LocalCommand::OpenSshWorkspace { target } => {
                let (workspace, created) =
                    self.open_ssh_workspace(OpenSshWorkspaceInput { target })?;
                Ok(LocalResponse::Opened {
                    request_id,
                    workspace: protocol_summary(&workspace),
                    created,
                })
            }
            LocalCommand::ListWorkspaces => Ok(LocalResponse::Workspaces {
                request_id,
                workspaces: self
                    .list_workspaces()?
                    .iter()
                    .map(protocol_summary)
                    .collect(),
            }),
            LocalCommand::Doctor => Ok(LocalResponse::Doctor {
                request_id,
                report: DoctorReport {
                    desktop_reachable: true,
                    protocol_version: PROTOCOL_VERSION,
                    message: "authenticated LocalReview desktop endpoint is ready".into(),
                },
            }),
        }
    }

    pub fn open_local_workspace(
        &self,
        input: OpenWorkspaceInput,
    ) -> Result<(WorkspaceView, bool), DispatchError> {
        let root = PathBuf::from(&input.path);
        if !root.is_absolute() {
            return Err(DispatchError::Invalid(
                "workspace path must be absolute after CLI resolution".into(),
            ));
        }
        let base = parse_base(input.base.as_deref())?;
        let opened = self
            .service
            .open_local_workspace(OpenLocalWorkspaceRequest {
                root,
                display_name: None,
                workspace_default_base: base,
                discovery: DiscoveryConfig::default(),
            })?;
        self.apply_baselines(opened.workspace.id, None, &input.repository_bases)?;
        if self
            .service
            .active_review_session(opened.workspace.id)?
            .is_none()
        {
            match self.start_new_review(opened.workspace.id, StartOrRefreshInput::default()) {
                Ok(_) => {}
                // Repository discovery is useful durable state even when the
                // proposed base is absent from every local repository. Keep
                // the workspace open so the user can correct baselines; no
                // fetch is attempted here or by the setup screen.
                Err(DispatchError::Service(
                    localreview_service::ServiceError::NoRepositoryCaptureSucceeded {
                        workspace_id,
                    },
                )) if workspace_id == opened.workspace.id => {}
                Err(error) => return Err(error),
            }
        }
        let workspace = self.workspace_view(&opened.workspace)?;
        Ok((workspace, !opened.reused_existing_workspace))
    }

    /// Materializes an isolated, pinned GitHub PR workspace.  The service
    /// owns `gh` authentication, known-clone selection, mirror fallback, and
    /// durable worktree registration; the controller only converts it into a
    /// frontend workspace view.
    pub fn open_github_pull_request(
        &self,
        input: OpenGitHubPullRequestInput,
    ) -> Result<(WorkspaceView, bool), DispatchError> {
        let opened = self
            .service
            .open_github_pull_request(OpenGitHubPullRequestRequest {
                url: input.url,
                application_default_base: self.application_base()?,
            })?;
        Ok((
            self.workspace_view(&opened.workspace)?,
            !opened.reused_existing_workspace,
        ))
    }

    /// Opens a manifest-first remote review.  The initial connection only
    /// discovers repositories and captures Git metadata.  It never copies a
    /// workspace or a patch body; source bytes remain behind the companion
    /// until a selected file needs presentation.
    pub fn open_ssh_workspace(
        &self,
        input: OpenSshWorkspaceInput,
    ) -> Result<(WorkspaceView, bool), DispatchError> {
        let target = parse_remote_target(&input.target)?;
        if let Some(existing) = self.find_remote_workspace(&target)? {
            // A durable remote workspace is usable offline from its retained
            // documents. Opening it explicitly still attempts a normal SSH
            // handshake so a missing/incompatible companion is explained at
            // the boundary instead of becoming a silent empty review.
            let mut metadata = self.remote_metadata(existing.id)?;
            self.ensure_remote_session(existing.id, &mut metadata)?;
            return Ok((self.workspace_view(&existing)?, false));
        }

        let RemoteSessionConnect {
            mut session,
            companion,
            reverse_forward,
        } = connect_remote_session(&target, self.managed_forward_handler(&target.host))
            .map_err(|failure| *failure.error)?;
        let generation = 1;
        let repositories = match session.request(
            AgentOperation::DiscoverRepositories {
                root: target.root.clone(),
                max_depth: localreview_protocol::MAX_REMOTE_DISCOVERY_DEPTH,
            },
            generation,
            |_| {},
        ) {
            Ok(AgentResult::Repositories { repositories }) => repositories,
            Ok(result) => return Err(remote_result_error("repository discovery", result)),
            Err(error) => return Err(remote_transport_error(error)),
        };

        let now = Utc::now();
        let workspace = Workspace {
            id: WorkspaceId::new(),
            display_name: remote_workspace_name(&target),
            source: WorkspaceSource::RemoteDirectory {
                host: target.host.clone(),
                root: StoredPath::from(target.root.clone()),
            },
            default_base: BaseReference::default(),
            pinned: false,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        self.state().upsert_workspace(&workspace)?;

        let mut persisted_repositories = Vec::with_capacity(repositories.len());
        for remote in &repositories {
            let repository = remote_repository_record(workspace.id, remote)?;
            self.state().upsert_repository(&repository)?;
            persisted_repositories.push((remote.clone(), repository));
        }
        let review_session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: workspace.id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        let annotation_set = AnnotationSet {
            id: AnnotationSetId::new(),
            review_session_id: review_session.id,
            sequence: 1,
            active: true,
            archived_at: None,
            created_at: now,
        };
        let mut captures = Vec::new();
        let mut generations = Vec::new();
        for (remote, mut repository) in persisted_repositories {
            let base = repository
                .base_override
                .as_ref()
                .unwrap_or(&workspace.default_base)
                .as_str()
                .to_owned();
            match session.request(
                AgentOperation::CaptureComparison {
                    repository: remote.reference.clone(),
                    base,
                    options: Default::default(),
                },
                generation,
                |_| {},
            ) {
                Ok(AgentResult::ComparisonCapture { capture }) => {
                    let comparison =
                        remote_comparison(&repository, &capture, &ComparisonOptions::default())?;
                    let (documents, bindings) = remote_placeholder_documents(&comparison, &capture);
                    let rows = review_generation_rows(documents);
                    generations.push(self.state().prepare_review_generation(&comparison, &rows)?);
                    repository.last_resolved_base_sha = Some(comparison.merge_base_sha.clone());
                    repository.discovery_error = None;
                    self.state().upsert_repository(&repository)?;
                    captures.push(RemoteCaptureBinding {
                        repository_id: repository.id.to_string(),
                        comparison_id: comparison.id.to_string(),
                        capture,
                        files: bindings,
                    });
                }
                Ok(result) => {
                    repository.discovery_error =
                        Some(remote_result_error("comparison capture", result).to_string());
                    self.state().upsert_repository(&repository)?;
                }
                Err(error) => {
                    repository.discovery_error = Some(remote_transport_error(error).to_string());
                    self.state().upsert_repository(&repository)?;
                }
            }
        }

        let metadata = RemoteWorkspaceMetadata {
            schema_version: 4,
            target: target.clone(),
            review_session_id: review_session.id.to_string(),
            generation,
            agent_version: Some(session.connection.hello.agent_version.clone()),
            latency_millis: Some(session.connection.latency.as_millis()),
            captures,
            stale: false,
            last_error: None,
            companion: Some(companion),
        };
        let metadata_json = serde_json::to_string(&metadata)
            .map_err(|error| DispatchError::Invalid(error.to_string()))?;
        self.state().replace_active_review_with_setting(
            workspace.id,
            &review_session,
            &annotation_set,
            &generations,
            now,
            Some((&remote_workspace_key(workspace.id), &metadata_json)),
        )?;
        self.remote_sessions
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .insert(
                workspace.id,
                Arc::new(Mutex::new(RemoteWorkspaceRuntime {
                    session,
                    reverse_forward,
                })),
            );
        // Best-effort notification-only watchers use their own managed
        // transports. A failed watcher never rolls back a valid review.
        let _ = self.start_remote_watchers(workspace.id, &metadata, false);
        Ok((self.workspace_view(&workspace)?, true))
    }

    pub fn focus_workspace(&self, selector: &str) -> Result<WorkspaceView, DispatchError> {
        let workspaces = self.state().workspaces()?;
        if let Some(id) = parse_workspace_id(selector) {
            if let Some(workspace) = self.state().workspace(id)? {
                if workspace.archived_at.is_some() {
                    return Err(DispatchError::Invalid(
                        "workspace is archived; reopen it from Review history or open its local path again"
                            .into(),
                    ));
                }
                return self.workspace_view(&workspace);
            }
        }
        let matches = workspaces
            .into_iter()
            .filter(|workspace| {
                workspace.archived_at.is_none() && workspace.display_name == selector
            })
            .collect::<Vec<_>>();
        match matches.len() {
            0 => Err(DispatchError::NotFound(selector.into())),
            1 => self.workspace_view(&matches[0]),
            _ => Err(DispatchError::Ambiguous(selector.into())),
        }
    }

    /// Explicit reconnect/status probe for a durable SSH workspace. It does
    /// not capture, refresh, or alter its immutable review generation.
    pub fn reconnect_ssh_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceView, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if !matches!(workspace.source, WorkspaceSource::RemoteDirectory { .. }) {
            return Err(DispatchError::Invalid(
                "workspace is not an SSH review".into(),
            ));
        }
        // Reconnect is intentionally a fresh transport, not merely a Ping on
        // a potentially half-open child.  Source/capture readers retain any
        // in-flight Arc until they finish, while the next operation receives
        // a new managed transport.  Stop the notification child first so the
        // replacement below is exactly one watcher for this workspace.
        self.stop_remote_watchers(workspace_id);
        if let Ok(mut sessions) = self.remote_sessions.lock() {
            sessions.remove(&workspace_id);
        }
        let mut metadata = self.remote_metadata(workspace_id)?;
        let runtime = self.ensure_remote_session(workspace_id, &mut metadata)?;
        let mut session = runtime.lock().map_err(|_| DispatchError::Internal)?;
        let result = session
            .session
            .request(AgentOperation::Ping, 0, |_| {})
            .map_err(remote_transport_error)?;
        if !matches!(result, AgentResult::Pong) {
            return Err(remote_result_error("SSH connection probe", result));
        }
        metadata.agent_version = Some(session.session.connection.hello.agent_version.clone());
        metadata.latency_millis = Some(session.session.connection.latency.as_millis());
        metadata.last_error = None;
        self.save_remote_metadata(workspace_id, &metadata)?;
        drop(session);
        // Reconnect is the deliberate recovery boundary for the notification
        // transport too.  Always replace the old watcher (which can be tied
        // to a disconnected transport) with the single bounded workspace
        // watcher; this never captures or refreshes the immutable review.
        self.start_remote_watchers(workspace_id, &metadata, false)?;
        self.workspace_view(&workspace)
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceView>, DispatchError> {
        self.state()
            .workspaces()?
            .iter()
            .filter(|workspace| workspace.archived_at.is_none())
            .map(|workspace| self.workspace_view(workspace))
            .collect()
    }

    /// Returns only source-free persistence health and aggregate backup
    /// storage facts. It is safe to show or copy from the diagnostics UI.
    pub fn persistence_diagnostics(&self) -> Result<PersistenceDiagnostics, DispatchError> {
        self.state()
            .diagnostics(BackupPolicy::default())
            .map_err(DispatchError::from)
    }

    /// Updates rail-only workspace metadata without recapturing Git state or
    /// changing the active review.  Keeping this operation on the native
    /// boundary makes pin/rename durable across CLI- and GUI-opened sessions.
    pub fn update_workspace_metadata(
        &self,
        workspace_id: WorkspaceId,
        display_name: Option<String>,
        pinned: Option<bool>,
    ) -> Result<WorkspaceView, DispatchError> {
        let mut workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if workspace.archived_at.is_some() {
            return Err(DispatchError::Invalid(
                "reopen an archived workspace before changing its rail metadata".into(),
            ));
        }
        if let Some(display_name) = display_name {
            let display_name = display_name.trim();
            if display_name.is_empty() || display_name.chars().count() > 120 {
                return Err(DispatchError::Invalid(
                    "workspace name must contain between 1 and 120 characters".into(),
                ));
            }
            if display_name.chars().any(char::is_control) {
                return Err(DispatchError::Invalid(
                    "workspace name cannot contain control characters".into(),
                ));
            }
            workspace.display_name = display_name.to_owned();
        }
        if let Some(pinned) = pinned {
            workspace.pinned = pinned;
        }
        workspace.updated_at = Utc::now();
        self.state().upsert_workspace(&workspace)?;
        self.workspace_view(&workspace)
    }

    /// Archived workspaces are recoverable review snapshots, not deleted
    /// records.  Keep this list separate from the live workspace rail so a
    /// user can intentionally reopen one even when there is no active review.
    pub fn list_archived_workspaces(&self) -> Result<Vec<WorkspaceView>, DispatchError> {
        self.state()
            .workspaces()?
            .iter()
            .filter(|workspace| workspace.archived_at.is_some())
            .map(|workspace| self.workspace_view(workspace))
            .collect()
    }

    /// Reopen an archived workspace as its durable captured snapshot.  A
    /// deleted GitHub PR worktree is intentionally *not* recreated here:
    /// prior diff documents and annotations are already immutable state, and
    /// reopening must not silently fetch or change the historical review.
    pub fn reopen_archived_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceView, DispatchError> {
        let mut workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if workspace.archived_at.is_none() {
            return Err(DispatchError::Invalid(
                "workspace is already open in the workspace rail".into(),
            ));
        }
        workspace.archived_at = None;
        workspace.updated_at = Utc::now();
        self.state().upsert_workspace(&workspace)?;
        self.clear_refresh_available(workspace_id);
        self.workspace_view(&workspace)
    }

    pub fn load_review(&self, workspace_id: WorkspaceId) -> Result<ReviewData, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let repositories = self.state().repositories(workspace.id)?;
        let session = self.service.active_review_session(workspace.id)?;
        let Some(session) = session else {
            return Ok(ReviewData {
                workspace: self.workspace_view(&workspace)?,
                repositories: repositories
                    .iter()
                    .map(|repository| repository_view(repository, None, &workspace.default_base))
                    .collect(),
                files: Vec::new(),
                annotations: Vec::new(),
                history: self.history(workspace.id)?,
                refresh_outcome: None,
                historical: false,
                historical_session_id: None,
            });
        };
        // Deleting a GitHub workspace removes its app-owned checkout but keeps
        // this immutable capture. If the user reopens that archived snapshot,
        // expose it as read-only instead of presenting refresh/edit actions
        // which require a worktree that intentionally no longer exists.
        let detached_github_snapshot =
            matches!(workspace.source, WorkspaceSource::PullRequest { .. })
                && self
                    .service
                    .github_pull_request(workspace.id)
                    .map_or(true, |review| {
                        !Path::new(review.managed_worktree.worktree_path.as_str()).is_dir()
                    });
        self.review_data_for_session(
            &workspace,
            &repositories,
            &session,
            detached_github_snapshot,
        )
    }

    /// Opens an archived review generation as a strictly read-only browsing
    /// surface.  It uses only durable comparison/doc/annotation rows and does
    /// not change the active review, refs, worktree, or annotation sets.
    pub fn load_archived_review(
        &self,
        workspace_id: WorkspaceId,
        history_id: &str,
    ) -> Result<ReviewData, DispatchError> {
        let session_id = parse_history_review_id(history_id).ok_or_else(|| {
            DispatchError::Invalid("history item is not an archived review".into())
        })?;
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let session = self
            .state()
            .review_sessions_for_id(session_id)?
            .ok_or_else(|| DispatchError::NotFound(history_id.into()))?;
        if session.workspace_id != workspace_id {
            return Err(DispatchError::NotFound(history_id.into()));
        }
        if session.status == ReviewSessionStatus::Active {
            return Err(DispatchError::Invalid(
                "the active review is already available from the workspace rail".into(),
            ));
        }
        let repositories = self.state().repositories(workspace.id)?;
        self.review_data_for_session(&workspace, &repositories, &session, true)
    }

    fn review_data_for_session(
        &self,
        workspace: &Workspace,
        repositories: &[Repository],
        session: &ReviewSession,
        historical: bool,
    ) -> Result<ReviewData, DispatchError> {
        let documents = self.current_documents(session.id)?;
        let annotations = self.annotations_for_session(session)?;
        let review_state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();
        let comparison_by_repo = self.current_comparisons(session.id)?;
        let files = documents
            .iter()
            .map(|document| -> Result<ReviewFileView, DispatchError> {
                let annotation_count = annotations
                    .iter()
                    .filter(|annotation| {
                        !is_soft_deleted(annotation)
                            && annotation_matches_file(annotation, &document.document)
                    })
                    .count();
                Ok(file_view(
                    &document.document,
                    document_repository_id(&document.document, &comparison_by_repo)?.to_string(),
                    review_state
                        .viewed_file_ids
                        .contains(&document.document.file.id.to_string()),
                    annotation_count,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let repository_views = repositories
            .iter()
            .map(|repository| {
                repository_view(
                    repository,
                    comparison_by_repo.get(&repository.id),
                    &workspace.default_base,
                )
            })
            .collect();
        let annotation_views = annotations
            .iter()
            .map(|annotation| annotation_view(annotation, &documents))
            .collect();
        Ok(ReviewData {
            workspace: self.workspace_view(workspace)?,
            repositories: repository_views,
            files,
            annotations: annotation_views,
            history: self.history(workspace.id)?,
            refresh_outcome: None,
            historical,
            historical_session_id: historical.then(|| session.id.to_string()),
        })
    }

    /// Lists classifications derived from the immutable active-review
    /// documents. No worktree is traversed and no review generation changes.
    pub fn review_file_classifications(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ReviewFileClassificationView>, DispatchError> {
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        self.service
            .review_file_classifications(session.id)?
            .into_iter()
            .map(|record| {
                Ok(ReviewFileClassificationView {
                    comparison_id: record.comparison_id.to_string(),
                    file_id: record.file_id.to_string(),
                    path: record.path.to_string(),
                    classification: ReviewFileClassificationViewFacts {
                        generated: record.classification.generated,
                        vendored: record.classification.vendored,
                        lockfile: record.classification.lockfile,
                        binary: record.classification.binary,
                        lfs_pointer: record.classification.lfs_pointer,
                        submodule: record.classification.submodule,
                    },
                })
            })
            .collect()
    }

    /// Blame is intentionally available only for a local captured
    /// comparison. A remote companion cannot safely be treated as a local Git
    /// checkout, and provider worktrees remain pinned and read-only.
    pub fn captured_blame(
        &self,
        workspace_id: WorkspaceId,
        input: CapturedBlameInput,
    ) -> Result<CapturedBlameView, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if !matches!(workspace.source, WorkspaceSource::LocalDirectory { .. }) {
            return Err(DispatchError::Invalid(
                "captured blame is currently available for local workspace reviews only".into(),
            ));
        }
        let file_id = parse_review_file_id(&input.file_id)
            .ok_or_else(|| DispatchError::Invalid("blame fileId is invalid".into()))?;
        let side = parse_side(&input.side)?;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let document = self
            .state()
            .review_file_payload::<PersistedReviewDocument>(file_id)?
            .ok_or_else(|| DispatchError::NotFound(input.file_id.clone()))?
            .document;
        if !self
            .current_comparisons(session.id)?
            .values()
            .any(|comparison| comparison.id == document.comparison_id)
        {
            return Err(DispatchError::Invalid(
                "blame file is not part of the active immutable review".into(),
            ));
        }
        let captured = self.service.captured_blame(CapturedBlameRequest {
            review_session_id: session.id,
            comparison_id: document.comparison_id,
            side,
            file_path: document.file.path,
            start_line: input.start_line,
            end_line: input.end_line,
        })?;
        Ok(CapturedBlameView {
            comparison_id: captured.comparison_id.to_string(),
            side: side_name(captured.side).into(),
            lines: captured
                .blame
                .lines
                .into_iter()
                .map(|line| BlameLineView {
                    revision: line.revision.to_string(),
                    original_line: line.original_line,
                    final_line: line.final_line,
                    source_path: line.source_path.to_string(),
                    author_name: line.author_name,
                    author_email: line.author_email,
                    author_time: line.author_time,
                    summary: line.summary,
                    source: line.source,
                    source_truncated: line.source_truncated,
                })
                .collect(),
        })
    }

    /// Loads bounded, read-only commit context for an already captured local
    /// repository comparison. Filters and a selected SHA never alter the
    /// aggregate review diff or its anchors.
    pub fn captured_commit_context(
        &self,
        workspace_id: WorkspaceId,
        input: CommitContextInput,
    ) -> Result<CapturedCommitContextView, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if !matches!(workspace.source, WorkspaceSource::LocalDirectory { .. }) {
            return Err(DispatchError::Invalid(
                "captured commit context is currently available for local workspace reviews only"
                    .into(),
            ));
        }
        let repository_id = RepositoryId(Uuid::parse_str(&input.repository_id).map_err(|_| {
            DispatchError::Invalid("commit-context repositoryId is invalid".into())
        })?);
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let comparison = self
            .current_comparisons(session.id)?
            .get(&repository_id)
            .cloned()
            .ok_or_else(|| {
                DispatchError::Invalid("repository is not in the active review".into())
            })?;
        let selected_commit = input
            .selected_commit
            .map(GitSha::new)
            .transpose()
            .map_err(|error| DispatchError::Invalid(error.to_string()))?;
        let context = self
            .service
            .captured_commit_context(CapturedCommitContextRequest {
                review_session_id: session.id,
                comparison_id: comparison.id,
                plan: localreview_git::GitCommitContextRequest {
                    max_entries: input.max_entries.unwrap_or(100),
                    include_merge_commits: input.include_merge_commits.unwrap_or(true),
                    author_contains: input.author_contains,
                    subject_contains: input.subject_contains,
                    selected_commit,
                },
            })?;
        Ok(commit_context_view(context.comparison_id, context.context))
    }

    /// Compares two persisted generations for one repository. It is a
    /// metadata operation over fingerprints, not a new mutable-worktree diff.
    pub fn changed_since_previous_review(
        &self,
        workspace_id: WorkspaceId,
        repository_id: String,
    ) -> Result<ChangedSincePreviousReviewView, DispatchError> {
        let repository_id = RepositoryId(
            Uuid::parse_str(&repository_id)
                .map_err(|_| DispatchError::Invalid("history repositoryId is invalid".into()))?,
        );
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let changed =
            self.service
                .changed_since_previous_review(ChangedSincePreviousReviewRequest {
                    review_session_id: session.id,
                    repository_id,
                    max_files: MAX_CHANGED_SINCE_PREVIOUS_REVIEW_FILES,
                })?;
        Ok(ChangedSincePreviousReviewView {
            current_comparison_id: changed.current_comparison_id.to_string(),
            previous_comparison_id: changed.previous_comparison_id.map(|id| id.to_string()),
            files: changed
                .files
                .into_iter()
                .map(|file| PreviousReviewFileView {
                    kind: previous_review_change_name(file.kind).into(),
                    path: file.path.to_string(),
                    previous_path: file.previous_path.map(|path| path.to_string()),
                    current_file_id: file.current_file_id.map(|id| id.to_string()),
                    previous_file_id: file.previous_file_id.map(|id| id.to_string()),
                    current_document_fingerprint: file
                        .current_document_fingerprint
                        .map(|fingerprint| fingerprint.as_str().into()),
                    previous_document_fingerprint: file
                        .previous_document_fingerprint
                        .map(|fingerprint| fingerprint.as_str().into()),
                })
                .collect(),
            truncated: changed.truncated,
        })
    }

    /// One explicit provider read for the current GitHub PR. It never edits
    /// pins, imports, worktrees, annotations, or the active comparison. A
    /// positive result only raises the existing refresh-available indicator.
    pub fn github_pull_request_update_status(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GitHubPullRequestUpdateStatusView, DispatchError> {
        let status = self
            .service
            .github_pull_request_update_status(workspace_id)?;
        if status.base_changed || status.head_changed {
            self.mark_refresh_available(workspace_id)?;
        }
        Ok(GitHubPullRequestUpdateStatusView {
            workspace_id: status.workspace_id.to_string(),
            canonical_url: status.canonical_url,
            pinned_base_sha: status.pinned_base_sha.to_string(),
            pinned_head_sha: status.pinned_head_sha.to_string(),
            current_base_sha: status.current_base_sha.to_string(),
            current_head_sha: status.current_head_sha.to_string(),
            base_changed: status.base_changed,
            head_changed: status.head_changed,
            metadata_fetched_at: status.metadata_fetched_at.to_rfc3339(),
        })
    }

    pub fn rows(
        &self,
        file_id: ReviewFileId,
        mode: &str,
    ) -> Result<Vec<DiffRowView>, DispatchError> {
        self.ensure_remote_file_materialized(file_id)?;
        let document = self
            .state()
            .review_file_payload::<PersistedReviewDocument>(file_id)?
            .ok_or_else(|| DispatchError::NotFound(file_id.to_string()))?
            .document;
        let annotations = self.annotations_for_document(&document)?;
        let has_annotation = |side: DiffSide, line: u32| {
            annotations.iter().any(|annotation| {
                annotation.anchor.as_ref().is_some_and(|anchor| {
                    anchor.comparison_id == document.comparison_id
                        && anchor.file_path == document.file.path
                        && anchor.side == Some(side)
                        && anchor.start_line.unwrap_or_default() <= line
                        && anchor.end_line.unwrap_or_default() >= line
                })
            })
        };
        match mode {
            "unified" | "difftastic" => {
                Ok(document
                    .hunks
                    .iter()
                    .flat_map(|hunk| {
                        let mut rows = vec![header_row(hunk)];
                        rows.extend(hunk.unified_rows.iter().map(|row| DiffRowView {
                            id: row.id.clone(),
                            kind: row_kind(row.kind).into(),
                            hunk_id: Some(hunk.id.0.clone()),
                            old_line: row.old.as_ref().map(|cell| cell.line_number),
                            new_line: row.new.as_ref().map(|cell| cell.line_number),
                            old_text: row.old.as_ref().map(|cell| cell.text.clone()),
                            new_text: row.new.as_ref().map(|cell| cell.text.clone()),
                            text: None,
                            hunk: None,
                            has_annotation: row.old.as_ref().is_some_and(|cell| {
                                has_annotation(DiffSide::Old, cell.line_number)
                            }) || row.new.as_ref().is_some_and(|cell| {
                                has_annotation(DiffSide::New, cell.line_number)
                            }),
                            old_source_start_byte: None,
                            new_source_start_byte: None,
                            omitted_block_id: None,
                            omitted_count: None,
                            omitted_end_line: None,
                            omitted_side: None,
                            omitted_expanded: None,
                        }));
                        rows
                    })
                    .collect())
            }
            "split" => Ok(document
                .hunks
                .iter()
                .flat_map(|hunk| {
                    let mut rows = vec![header_row(hunk)];
                    rows.extend(hunk.split_rows.iter().map(|row| {
                        DiffRowView {
                            id: row.id.clone(),
                            kind: match (&row.old, &row.new) {
                                (None, Some(_)) => "addition",
                                (Some(_), None) => "deletion",
                                (Some(old), Some(new))
                                    if old.kind == DiffLineKind::Context
                                        && new.kind == DiffLineKind::Context =>
                                {
                                    "context"
                                }
                                _ => "context",
                            }
                            .into(),
                            hunk_id: Some(hunk.id.0.clone()),
                            old_line: row.old.as_ref().map(|cell| cell.line_number),
                            new_line: row.new.as_ref().map(|cell| cell.line_number),
                            old_text: row.old.as_ref().map(|cell| cell.text.clone()),
                            new_text: row.new.as_ref().map(|cell| cell.text.clone()),
                            text: None,
                            hunk: None,
                            has_annotation: row.old.as_ref().is_some_and(|cell| {
                                has_annotation(DiffSide::Old, cell.line_number)
                            }) || row.new.as_ref().is_some_and(|cell| {
                                has_annotation(DiffSide::New, cell.line_number)
                            }),
                            old_source_start_byte: None,
                            new_source_start_byte: None,
                            omitted_block_id: None,
                            omitted_count: None,
                            omitted_end_line: None,
                            omitted_side: None,
                            omitted_expanded: None,
                        }
                    }));
                    rows
                })
                .collect()),
            "full" => {
                let view = if document.file.status == localreview_diff::ReviewFileStatus::Deleted {
                    FullFileView::Old
                } else {
                    FullFileView::Both
                };
                let (projection, _) =
                    full_file_projection(&document, view, &BTreeSet::new(), &BTreeSet::new());
                let old_offsets = source_line_offsets(&document.old.content);
                let new_offsets = source_line_offsets(&document.new.content);
                let mut rows = projection
                    .iter()
                    .map(|row| full_file_row_view(row, &old_offsets, &new_offsets))
                    .collect::<Vec<_>>();
                self.mark_annotation_rows(&document, &mut rows)?;
                Ok(rows)
            }
            _ => Err(DispatchError::Invalid("unsupported diff mode".into())),
        }
    }

    /// Returns a bounded window over a canonical immutable presentation. The
    /// client may request a new viewport frequently, but it never receives a
    /// complete unchanged file merely because a virtual list scrolled.
    pub fn presentation_window(
        &self,
        request: PresentationRequest,
        resource_dir: &Path,
    ) -> Result<PresentationWindow, DispatchError> {
        const MAX_WINDOW_ROWS: usize = 2_048;
        let file_id = parse_review_file_id(&request.file_id)
            .ok_or_else(|| DispatchError::Invalid("presentation fileId is invalid".into()))?;
        let comparison_id = request
            .comparison_id
            .as_deref()
            .map(|value| {
                parse_comparison_id(value).ok_or_else(|| {
                    DispatchError::Invalid("presentation comparisonId is invalid".into())
                })
            })
            .transpose()?;
        let mode = validate_diff_mode(&request.mode)?;
        let full_file_side = request
            .full_file_side
            .as_deref()
            .map(parse_full_file_view)
            .transpose()?
            .unwrap_or(FullFileView::Both);
        if let Some(ratio) = request.split_ratio {
            if !ratio.is_finite() || !(0.25..=0.75).contains(&ratio) {
                return Err(DispatchError::Invalid(
                    "split ratio must be between 0.25 and 0.75".into(),
                ));
            }
        }
        self.ensure_remote_file_materialized_for_comparison(file_id, comparison_id)?;
        let job = self.acquire_presentation_job(file_id, request.generation)?;
        self.ensure_presentation_job_current(&job)?;
        let persisted = self.cached_persisted_review_document(file_id, comparison_id)?;
        let document = &persisted.document;
        let session = self
            .session_for_comparison(document.comparison_id)?
            .ok_or_else(|| {
                DispatchError::Invalid("file is not part of a retained immutable review".into())
            })?;
        let mut ui_state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();
        if let Some(values) = request
            .ephemeral_expanded_full_file_deletion_blocks
            .as_ref()
        {
            const MAX_EXPANDED_DELETION_BLOCKS: usize = 10_000;
            if values.len() > MAX_EXPANDED_DELETION_BLOCKS {
                return Err(DispatchError::Invalid(format!(
                    "ephemeralExpandedFullFileDeletionBlocks may contain at most {MAX_EXPANDED_DELETION_BLOCKS} values"
                )));
            }
            let expanded = values.iter().cloned().collect::<BTreeSet<_>>();
            let valid = valid_full_file_deletion_block_ids(&self.current_documents(session.id)?);
            if !expanded.is_subset(&valid) {
                return Err(DispatchError::Invalid(
                    "ephemeralExpandedFullFileDeletionBlocks contains a stale or foreign block"
                        .into(),
                ));
            }
            ui_state.expanded_full_file_deletion_blocks = expanded;
        }
        if let Some(values) = request
            .ephemeral_collapsed_full_file_addition_blocks
            .as_ref()
        {
            const MAX_COLLAPSED_ADDITION_BLOCKS: usize = 10_000;
            if values.len() > MAX_COLLAPSED_ADDITION_BLOCKS {
                return Err(DispatchError::Invalid(format!(
                    "ephemeralCollapsedFullFileAdditionBlocks may contain at most {MAX_COLLAPSED_ADDITION_BLOCKS} values"
                )));
            }
            let collapsed = values.iter().cloned().collect::<BTreeSet<_>>();
            let valid = valid_full_file_deletion_block_ids(&self.current_documents(session.id)?);
            if !collapsed.is_subset(&valid) {
                return Err(DispatchError::Invalid(
                    "ephemeralCollapsedFullFileAdditionBlocks contains a stale or foreign block"
                        .into(),
                ));
            }
            ui_state.collapsed_full_file_addition_blocks = collapsed;
        }
        let settings = self.get_settings()?;

        if mode == "difftastic" {
            return self.difftastic_window(
                request,
                document,
                &ui_state,
                &settings,
                DifftasticWindowExecution {
                    resource_dir,
                    max_window_rows: MAX_WINDOW_ROWS,
                    job: Some(&job),
                },
            );
        }
        if mode == "full" {
            return self.full_file_window(
                request,
                document,
                full_file_side,
                &ui_state,
                FullFileWindowExecution {
                    settings: &settings,
                    max_window_rows: MAX_WINDOW_ROWS,
                    job: Some(&job),
                },
            );
        }
        let language_attribute = self.highlight_language_attribute(document, session.id);

        let canonical = self.cached_canonical_rows(document, mode, full_file_side, &ui_state)?;
        let all_rows = &canonical.rows;
        let (start, end) = bounded_window(
            request.start_row,
            request.end_row,
            all_rows.len(),
            MAX_WINDOW_ROWS,
        );
        let mut rows = all_rows[start..end].to_vec();
        self.mark_annotation_rows(document, &mut rows)?;
        let (old_tokens, new_tokens, highlight_status, highlight_reason) = self.highlight_window(
            document,
            &rows,
            &settings,
            language_attribute.as_deref(),
            Some(&job),
        )?;
        self.ensure_presentation_job_current(&job)?;
        Ok(PresentationWindow {
            generation: request.generation,
            mode: mode.into(),
            file_id: request.file_id,
            start_row: u32::try_from(start).unwrap_or(u32::MAX),
            total_rows: u32::try_from(all_rows.len()).unwrap_or(u32::MAX),
            rows,
            hunks: canonical.hunks.clone(),
            omitted_blocks: Vec::new(),
            old_tokens,
            new_tokens,
            highlight_status,
            highlight_reason,
            difftastic: None,
        })
    }

    /// Resolves a source line against the complete immutable presentation.
    /// This is deliberately separate from `presentation_window`: asking the
    /// frontend to find a row in a 2k-row viewport causes distant jumps to
    /// restart at row zero and makes Difftastic's structural alignment lossy.
    pub fn resolve_presentation_location(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
        mode: &str,
        side: DiffSide,
        line: u32,
        resource_dir: &Path,
    ) -> Result<PresentationLocation, DispatchError> {
        if line == 0 {
            return Err(DispatchError::Invalid(
                "source line must be positive".into(),
            ));
        }
        let mode = validate_diff_mode(mode)?;
        self.ensure_remote_file_materialized_for_comparison(file_id, comparison_id)?;
        let document = self
            .persisted_review_document(file_id, comparison_id)?
            .document;
        let session = self
            .session_for_comparison(document.comparison_id)?
            .ok_or_else(|| {
                DispatchError::Invalid("file is not part of a retained immutable review".into())
            })?;
        let source = match side {
            DiffSide::Old => &document.old,
            DiffSide::New => &document.new,
        };
        if line > source.line_count {
            return Err(DispatchError::Invalid(
                "source line is outside the captured snapshot".into(),
            ));
        }
        let ui_state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();

        let row_index = match mode {
            "full" => {
                let displayed_view =
                    if document.file.status == localreview_diff::ReviewFileStatus::Deleted {
                        FullFileView::Old
                    } else {
                        ui_state
                            .full_file_side
                            .as_deref()
                            .map(parse_full_file_view)
                            .transpose()?
                            .unwrap_or(FullFileView::Both)
                    };
                let (projection, _) = full_file_projection(
                    &document,
                    displayed_view,
                    &ui_state.expanded_full_file_deletion_blocks,
                    &ui_state.collapsed_full_file_addition_blocks,
                );
                exact_full_file_row(&projection, side, line).unwrap_or_else(|| {
                    let target_side = displayed_view.primary_side();
                    let aligned = aligned_source_line(&document, side, line, target_side);
                    nearest_full_file_row(&projection, target_side, aligned)
                })
            }
            "unified" | "split" => {
                let canonical = self.cached_canonical_rows(
                    &document,
                    mode,
                    if side == DiffSide::Old {
                        FullFileView::Old
                    } else {
                        FullFileView::New
                    },
                    &ui_state,
                )?;
                nearest_canonical_row(&canonical.rows, side, line)
            }
            "difftastic" => {
                // Materialise/cache the *complete* normalized structural
                // representation, then resolve against its full alignment.
                // `difftastic_window` itself returns only a bounded slice, so
                // its response is intentionally not used for this lookup.
                let settings = self.get_settings()?;
                let _ = self.difftastic_window(
                    PresentationRequest {
                        file_id: file_id.to_string(),
                        comparison_id: Some(document.comparison_id.to_string()),
                        mode: "difftastic".into(),
                        start_row: 0,
                        end_row: 1,
                        generation: 0,
                        full_file_side: Some(side_name(side).into()),
                        split_ratio: None,
                        ephemeral_expanded_full_file_deletion_blocks: None,
                        ephemeral_collapsed_full_file_addition_blocks: None,
                    },
                    &document,
                    &ui_state,
                    &settings,
                    DifftasticWindowExecution {
                        resource_dir,
                        max_window_rows: 1,
                        job: None,
                    },
                )?;
                let cache_key = structural_cache_key(&document, &settings);
                let structural = self
                    .presentation_cache
                    .lock()
                    .map_err(|_| DispatchError::Internal)?
                    .structural
                    .get(&cache_key)
                    .cloned();
                if let Some(structural) = structural {
                    nearest_difftastic_row(&structural, side, line)
                } else {
                    // A failed or unavailable sidecar deliberately remains a
                    // canonical fallback. Its locations must still be
                    // authoritative and must not depend on the one-row
                    // virtual response above.
                    let canonical = self.cached_canonical_rows(
                        &document,
                        "unified",
                        if side == DiffSide::Old {
                            FullFileView::Old
                        } else {
                            FullFileView::New
                        },
                        &ui_state,
                    )?;
                    nearest_canonical_row(&canonical.rows, side, line)
                }
            }
            _ => unreachable!("validate_diff_mode admits only the matched modes"),
        };
        Ok(PresentationLocation {
            row_index: u32::try_from(row_index).unwrap_or(u32::MAX),
            side: side_name(side).into(),
            line,
        })
    }

    /// Returns an exact range from the retained immutable source document,
    /// rather than from presentation rows. A false `complete` result is
    /// explicit so callers can refuse to save an annotation safely.
    pub fn captured_source_range(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
        side: DiffSide,
        start_line: u32,
        end_line: u32,
    ) -> Result<CapturedSourceRange, DispatchError> {
        if start_line == 0 || end_line < start_line {
            return Err(DispatchError::Invalid(
                "source line range is invalid".into(),
            ));
        }
        self.ensure_remote_file_materialized_for_comparison(file_id, comparison_id)?;
        let document = self
            .persisted_review_document(file_id, comparison_id)?
            .document;
        self.session_for_comparison(document.comparison_id)?
            .ok_or_else(|| {
                DispatchError::Invalid("file is not part of a retained immutable review".into())
            })?;
        let source = match side {
            DiffSide::Old => &document.old.content,
            DiffSide::New => &document.new.content,
        };
        let line_count = u32::try_from(source.lines().count()).unwrap_or(u32::MAX);
        if end_line > line_count {
            return Ok(CapturedSourceRange {
                text: String::new(),
                complete: false,
            });
        }
        // The range helper indexes the retained UTF-8 source directly. It
        // preserves a final newline, unlike joining virtualized line text.
        let range = source_line_range(source, Some(start_line), Some(end_line))?;
        Ok(CapturedSourceRange {
            text: source[range].to_owned(),
            complete: true,
        })
    }

    pub fn annotation_draft(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<AnnotationDraft>, DispatchError> {
        self.state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let Some(session) = self.service.active_review_session(workspace_id)? else {
            return Ok(None);
        };
        let session_key = annotation_draft_key(session.id);
        // Read the old workspace-scoped key once for installations upgraded
        // from v1. Validation below prevents a legacy draft from crossing a
        // review boundary, and the next save migrates it to the session key.
        let raw = self.state().setting(&session_key)?.or(self
            .state()
            .setting(&legacy_annotation_draft_key(workspace_id))?);
        let Some(raw) = raw else {
            return Ok(None);
        };
        let draft = serde_json::from_str::<Option<AnnotationDraft>>(&raw).map_err(|error| {
            DispatchError::Invalid(format!("saved annotation draft is invalid: {error}"))
        })?;
        let Some(draft) = draft else {
            return Ok(None);
        };
        // A saved draft is useful only while its captured file is still part
        // of this workspace's active immutable review. Do not resurrect it
        // into a replacement review merely because the workspace UUID is
        // unchanged.
        match self.validate_annotation_draft(&draft, workspace_id) {
            Ok(()) => Ok(Some(draft)),
            Err(DispatchError::NotFound(_)) | Err(DispatchError::Invalid(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn save_annotation_draft(&self, draft: AnnotationDraft) -> Result<(), DispatchError> {
        let workspace_id = parse_workspace_id(&draft.workspace_id)
            .ok_or_else(|| DispatchError::Invalid("draft workspaceId is invalid".into()))?;
        self.validate_annotation_draft(&draft, workspace_id)?;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        self.state().set_setting(
            &annotation_draft_key(session.id),
            &serde_json::to_string(&draft)
                .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        )?;
        Ok(())
    }

    pub fn clear_annotation_draft(&self, workspace_id: WorkspaceId) -> Result<(), DispatchError> {
        self.state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let session = self.service.active_review_session(workspace_id)?;
        // StateStore intentionally exposes an upsert-only generic setting
        // API. JSON null is an explicit tombstone and keeps no draft content
        // recoverable through a future schema decoder.
        if let Some(session) = session {
            self.state()
                .set_setting(&annotation_draft_key(session.id), "null")?;
        }
        self.state()
            .set_setting(&legacy_annotation_draft_key(workspace_id), "null")?;
        Ok(())
    }

    pub fn expand_hunk_context(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
        hunk_id: &str,
        context_lines: u32,
    ) -> Result<(), DispatchError> {
        if hunk_id.is_empty() || hunk_id.len() > 1_024 {
            return Err(DispatchError::Invalid("hunk id is invalid".into()));
        }
        self.ensure_remote_file_materialized_for_comparison(file_id, comparison_id)?;
        let document = self
            .persisted_review_document(file_id, comparison_id)?
            .document;
        if !document.hunks.iter().any(|hunk| hunk.id.0 == hunk_id) {
            return Err(DispatchError::NotFound(hunk_id.into()));
        }
        let session = self
            .session_for_comparison(document.comparison_id)?
            .ok_or_else(|| DispatchError::Invalid("file is not part of an active review".into()))?;
        if session.status != ReviewSessionStatus::Active {
            return Err(DispatchError::Invalid(
                "hunk expansion is unavailable while browsing an archived review".into(),
            ));
        }
        let mut state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();
        state
            .hunk_context_lines
            .insert(hunk_id.into(), context_lines.clamp(3, 1_200));
        self.state()
            .save_review_session_ui_state(session.id, &state)?;
        self.presentation_jobs
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .cancel_file(file_id);
        let mut cache = self
            .presentation_cache
            .lock()
            .map_err(|_| DispatchError::Internal)?;
        let prefix = format!("{}:", file_id);
        cache.canonical.retain(|key, _| !key.starts_with(&prefix));
        cache.canonical_rows = cache.canonical.values().map(|entry| entry.rows.len()).sum();
        cache
            .canonical_order
            .retain(|key| !key.starts_with(&prefix));
        Ok(())
    }

    pub fn outline(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
        requested_side: DiffSide,
    ) -> Result<Vec<OutlineSymbolView>, DispatchError> {
        self.ensure_remote_file_materialized_for_comparison(file_id, comparison_id)?;
        let document = self
            .persisted_review_document(file_id, comparison_id)?
            .document;
        let side = if document.file.status == localreview_diff::ReviewFileStatus::Deleted {
            DiffSide::Old
        } else {
            requested_side
        };
        let source = match side {
            DiffSide::Old => &document.old.content,
            DiffSide::New => &document.new.content,
        };
        let session = self
            .session_for_comparison(document.comparison_id)?
            .ok_or_else(|| {
                DispatchError::Invalid("file is not part of a retained review".into())
            })?;
        let language_attribute = self.highlight_language_attribute(&document, session.id);
        let side_name = side_name(side).to_owned();
        Ok(localreview_highlight::outline(
            Path::new(document.file.path.as_str()),
            source,
            language_attribute.as_deref(),
        )
        .into_iter()
        .enumerate()
        .map(|(index, symbol)| OutlineSymbolView {
            id: format!("{}:{}:{}", side_name, symbol.start_line, index),
            name: symbol.name,
            kind: outline_kind_name(symbol.kind).into(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            depth: symbol.depth,
            side: side_name.clone(),
        })
        .collect())
    }

    pub fn workspace_ui_state(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<WorkspaceUiStateView, DispatchError> {
        self.state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let Some(session) = self.service.active_review_session(workspace_id)? else {
            // A newly discovered workspace can intentionally have no captured
            // review while the user corrects its local base. Its shell still
            // needs deterministic chrome state so selecting it does not turn
            // the setup flow into an apparent load failure.
            return Ok(workspace_ui_state_view(&ReviewUiState::default()));
        };
        let mut state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();
        let valid_blocks = valid_full_file_deletion_block_ids(&self.current_documents(session.id)?);
        let before = state.expanded_full_file_deletion_blocks.len();
        state
            .expanded_full_file_deletion_blocks
            .retain(|id| valid_blocks.contains(id));
        let collapsed_before = state.collapsed_full_file_addition_blocks.len();
        state
            .collapsed_full_file_addition_blocks
            .retain(|id| valid_blocks.contains(id));
        if state.expanded_full_file_deletion_blocks.len() != before
            || state.collapsed_full_file_addition_blocks.len() != collapsed_before
        {
            self.state()
                .save_review_session_ui_state(session.id, &state)?;
        }
        Ok(workspace_ui_state_view(&state))
    }

    pub fn save_workspace_ui_state(
        &self,
        workspace_id: WorkspaceId,
        patch: WorkspaceUiStatePatch,
    ) -> Result<WorkspaceUiStateView, DispatchError> {
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let mut state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();
        if let Some(value) = patch.active_file_id {
            let file = parse_review_file_id(&value)
                .ok_or_else(|| DispatchError::Invalid("activeFileId is invalid".into()))?;
            let valid = self
                .current_documents(session.id)?
                .iter()
                .any(|document| document.document.file.id == file);
            if !valid {
                return Err(DispatchError::Invalid(
                    "activeFileId is not in this review".into(),
                ));
            }
            state.active_file_id = Some(value);
        }
        if let Some(value) = patch.mode {
            validate_diff_mode(&value)?;
            state.mode = Some(value);
        }
        if let Some(value) = patch.full_file_side {
            parse_full_file_view(&value)?;
            state.full_file_side = Some(value);
        }
        if let Some(value) = patch.nearest_source_line {
            if value == 0 {
                return Err(DispatchError::Invalid(
                    "nearestSourceLine must be positive".into(),
                ));
            }
            state.nearest_source_line = Some(value);
        }
        if let Some(value) = patch.nearest_source_side {
            parse_side(&value)?;
            state.nearest_source_side = Some(value);
        }
        if let Some(value) = patch.scroll_top {
            if !value.is_finite() || !(0.0..=100_000_000.0).contains(&value) {
                return Err(DispatchError::Invalid("scrollTop is invalid".into()));
            }
            state.scroll_top = Some(value);
        }
        if let Some(value) = patch.split_ratio {
            if !value.is_finite() || !(0.25..=0.75).contains(&value) {
                return Err(DispatchError::Invalid(
                    "splitRatio must be between 0.25 and 0.75".into(),
                ));
            }
            state.split_ratio = Some(value);
        }
        if let Some(value) = patch.right_tab {
            if !matches!(value.as_str(), "files" | "comments" | "outline") {
                return Err(DispatchError::Invalid("rightTab is invalid".into()));
            }
            state.right_tab = Some(value);
        }
        if let Some(values) = patch.selected_annotation_ids {
            const MAX_SELECTED_ANNOTATIONS: usize = 10_000;
            if values.len() > MAX_SELECTED_ANNOTATIONS {
                return Err(DispatchError::Invalid(format!(
                    "selectedAnnotationIds may contain at most {MAX_SELECTED_ANNOTATIONS} values"
                )));
            }
            let selected = values.into_iter().collect::<BTreeSet<_>>();
            let active = self
                .state()
                .active_annotation_set(session.id)?
                .ok_or_else(|| {
                    DispatchError::Invalid("active review has no annotation set".into())
                })?;
            let owned = self
                .state()
                .annotations(active.id)?
                .into_iter()
                .map(|annotation| annotation.id.to_string())
                .collect::<BTreeSet<_>>();
            if !selected.is_subset(&owned)
                || selected.iter().any(|id| parse_annotation_id(id).is_none())
            {
                return Err(DispatchError::Invalid(
                    "selectedAnnotationIds contains an annotation outside the active review".into(),
                ));
            }
            state.selected_annotation_ids = Some(selected);
        }
        if let Some(values) = patch.expanded_full_file_deletion_blocks {
            const MAX_EXPANDED_DELETION_BLOCKS: usize = 10_000;
            if values.len() > MAX_EXPANDED_DELETION_BLOCKS {
                return Err(DispatchError::Invalid(format!(
                    "expandedFullFileDeletionBlocks may contain at most {MAX_EXPANDED_DELETION_BLOCKS} values"
                )));
            }
            let expanded = values.into_iter().collect::<BTreeSet<_>>();
            let valid = valid_full_file_deletion_block_ids(&self.current_documents(session.id)?);
            if !expanded.is_subset(&valid) {
                return Err(DispatchError::Invalid(
                    "expandedFullFileDeletionBlocks contains a stale or foreign block".into(),
                ));
            }
            state.expanded_full_file_deletion_blocks = expanded;
        }
        if let Some(values) = patch.collapsed_full_file_addition_blocks {
            const MAX_COLLAPSED_ADDITION_BLOCKS: usize = 10_000;
            if values.len() > MAX_COLLAPSED_ADDITION_BLOCKS {
                return Err(DispatchError::Invalid(format!(
                    "collapsedFullFileAdditionBlocks may contain at most {MAX_COLLAPSED_ADDITION_BLOCKS} values"
                )));
            }
            let collapsed = values.into_iter().collect::<BTreeSet<_>>();
            let valid = valid_full_file_deletion_block_ids(&self.current_documents(session.id)?);
            if !collapsed.is_subset(&valid) {
                return Err(DispatchError::Invalid(
                    "collapsedFullFileAdditionBlocks contains a stale or foreign block".into(),
                ));
            }
            state.collapsed_full_file_addition_blocks = collapsed;
        }
        self.state()
            .save_review_session_ui_state(session.id, &state)?;
        Ok(workspace_ui_state_view(&state))
    }

    pub fn delete_annotation(
        &self,
        workspace_id: WorkspaceId,
        annotation_id: AnnotationId,
    ) -> Result<(), DispatchError> {
        let mut annotation = self.active_annotation(workspace_id, annotation_id)?;
        annotation.state = AnnotationState::Deleted;
        if !annotation
            .labels
            .iter()
            .any(|label| label == "localreview:deleted")
        {
            annotation.labels.push("localreview:deleted".into());
        }
        annotation.updated_at = Utc::now();
        self.state().save_annotation(&annotation)?;
        Ok(())
    }

    pub fn set_annotation_state(
        &self,
        workspace_id: WorkspaceId,
        annotation_id: AnnotationId,
        state: &str,
    ) -> Result<AnnotationView, DispatchError> {
        let mut annotation = self.active_annotation(workspace_id, annotation_id)?;
        match state {
            "open" => {
                annotation.state = AnnotationState::Open;
                if let Some(anchor) = annotation.anchor.as_mut() {
                    anchor.outdated = false;
                }
            }
            "resolved" => annotation.state = AnnotationState::Resolved,
            "outdated" => {
                annotation.state = AnnotationState::Deleted;
                if let Some(anchor) = annotation.anchor.as_mut() {
                    anchor.outdated = true;
                }
            }
            _ => {
                return Err(DispatchError::Invalid(
                    "unsupported annotation state".into(),
                ))
            }
        }
        annotation
            .labels
            .retain(|label| label != "localreview:deleted");
        annotation.updated_at = Utc::now();
        self.state().save_annotation(&annotation)?;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        Ok(annotation_view(
            &annotation,
            &self.current_documents(session.id)?,
        ))
    }

    pub fn copy_review_item(
        &self,
        workspace_id: WorkspaceId,
        request: CopyReviewItemRequest,
    ) -> Result<String, DispatchError> {
        let file_id = parse_review_file_id(&request.file_id)
            .ok_or_else(|| DispatchError::Invalid("copy request fileId is invalid".into()))?;
        self.ensure_remote_file_materialized(file_id)?;
        let document = self
            .state()
            .review_file_payload::<PersistedReviewDocument>(file_id)?
            .ok_or_else(|| DispatchError::NotFound(request.file_id.clone()))?
            .document;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if !self
            .current_comparisons(session.id)?
            .values()
            .any(|comparison| comparison.id == document.comparison_id)
        {
            return Err(DispatchError::Invalid(
                "file is not in the active review".into(),
            ));
        }
        match request.kind.as_str() {
            "path" => return Ok(document.file.path.to_string()),
            "patch" => return Ok(canonical_patch_text(&document)),
            "source" | "source_with_line_numbers" | "hunk" | "provider_permalink" => {}
            _ => return Err(DispatchError::Invalid("unsupported copy kind".into())),
        }
        let side = request
            .side
            .as_deref()
            .map(parse_side)
            .transpose()?
            .unwrap_or_else(|| {
                if document.file.status == localreview_diff::ReviewFileStatus::Deleted {
                    DiffSide::Old
                } else {
                    DiffSide::New
                }
            });
        if request.kind == "provider_permalink" {
            return self.provider_permalink(workspace_id, &document, side, request.start_line);
        }
        if request.kind == "hunk" {
            return canonical_hunk_text(&document, side, request.start_line);
        }
        let source = match side {
            DiffSide::Old => &document.old.content,
            DiffSide::New => &document.new.content,
        };
        let range = source_line_range(source, request.start_line, request.end_line)?;
        match request.kind.as_str() {
            "source" => Ok(source[range.clone()].to_owned()),
            "source_with_line_numbers" => Ok(numbered_source(
                source,
                range,
                request.start_line.unwrap_or(1),
            )),
            _ => unreachable!("copy kind was validated before source extraction"),
        }
    }

    pub fn open_in_external_editor(
        &self,
        workspace_id: WorkspaceId,
        file_id: ReviewFileId,
        line: Option<u32>,
    ) -> Result<(), DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let WorkspaceSource::LocalDirectory {
            root: workspace_root,
        } = workspace.source
        else {
            return Err(DispatchError::Invalid(
                "external editing is unavailable for app-managed or remote review worktrees".into(),
            ));
        };
        if line == Some(0) {
            return Err(DispatchError::Invalid(
                "editor line must be positive".into(),
            ));
        }
        let document = self
            .state()
            .review_file_payload::<PersistedReviewDocument>(file_id)?
            .ok_or_else(|| DispatchError::NotFound(file_id.to_string()))?
            .document;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let comparisons = self.current_comparisons(session.id)?;
        let repository_id = document_repository_id(&document, &comparisons)?;
        let repository = self
            .state()
            .repositories_for_id(repository_id)?
            .ok_or_else(|| DispatchError::Invalid("repository was not found".into()))?;
        let root = std::fs::canonicalize(repository.worktree_path.as_str()).map_err(|error| {
            DispatchError::Invalid(format!("local repository path is unavailable: {error}"))
        })?;
        let workspace_root = std::fs::canonicalize(workspace_root.as_str()).map_err(|error| {
            DispatchError::Invalid(format!("local workspace path is unavailable: {error}"))
        })?;
        if !root.starts_with(&workspace_root) {
            return Err(DispatchError::Invalid(
                "repository path is outside the local workspace".into(),
            ));
        }
        let candidate = root.join(document.file.path.as_str());
        let target = std::fs::canonicalize(&candidate).map_err(|error| {
            DispatchError::Invalid(format!(
                "reviewed file is unavailable in the local checkout: {error}"
            ))
        })?;
        if !target.starts_with(&root) {
            return Err(DispatchError::Invalid(
                "reviewed path escapes the local repository".into(),
            ));
        }
        if !target.is_file() {
            return Err(DispatchError::Invalid(
                "reviewed path is not a regular file".into(),
            ));
        }
        let settings = self.get_settings()?;
        spawn_external_editor(&target, line.unwrap_or(1), &settings.external_editor)
    }

    pub fn save_annotation(&self, value: AnnotationView) -> Result<AnnotationView, DispatchError> {
        const MAX_ANNOTATION_BYTES: usize = 1024 * 1024;
        if value.body.trim().is_empty()
            || value.body.len() > MAX_ANNOTATION_BYTES
            || value.body.contains('\0')
        {
            return Err(DispatchError::Invalid(
                "annotation body is empty or exceeds the supported size".into(),
            ));
        }
        let kind = parse_annotation_kind(&value.kind)?;
        let file_id = parse_review_file_id(&value.file_id)
            .ok_or_else(|| DispatchError::Invalid("annotation fileId is invalid".into()))?;
        self.ensure_remote_file_materialized(file_id)?;
        let document = self
            .state()
            .review_file_payload::<PersistedReviewDocument>(file_id)?
            .ok_or_else(|| DispatchError::NotFound(value.file_id.clone()))?
            .document;
        let session = self
            .active_session_for_comparison(document.comparison_id)?
            .ok_or_else(|| {
                DispatchError::Invalid("the file does not belong to an active review".into())
            })?;
        let active = self
            .state()
            .active_annotation_set(session.id)?
            .ok_or_else(|| DispatchError::Invalid("active review has no annotation set".into()))?;
        let now = Utc::now();
        let existing = parse_annotation_id(&value.id).and_then(|id| {
            self.state()
                .annotations(active.id)
                .ok()
                .into_iter()
                .flatten()
                .find(|annotation| annotation.id == id)
        });
        // A client-provided UUID is only an optimistic draft key. Reusing an
        // archived id would mutate immutable history through SQLite's global
        // annotation id uniqueness, so only ids already in this active set
        // may be updated; every other save receives a fresh durable id.
        let id = existing
            .as_ref()
            .map_or_else(AnnotationId::new, |annotation| annotation.id);
        let state = parse_annotation_state(&value.state)?;
        let repository_id =
            document_repository_id(&document, &self.current_comparisons(session.id)?)?;
        // The domain deliberately distinguishes a line anchor, a file-level
        // anchor, and no anchor.  File/review notes must not be coerced into
        // a fake line anchor: GitHub's inline-review API would otherwise be
        // offered a location it cannot faithfully represent.
        let anchor = match kind {
            AnnotationKind::ReviewNote => {
                if value.start_line != 0 || value.end_line != 0 {
                    return Err(DispatchError::Invalid(
                        "review notes cannot carry a file or line anchor".into(),
                    ));
                }
                None
            }
            AnnotationKind::FileNote => {
                if value.start_line != 0 || value.end_line != 0 {
                    return Err(DispatchError::Invalid(
                        "file notes cannot carry a line range".into(),
                    ));
                }
                Some(AnnotationAnchor::from_file(
                    document.comparison_id,
                    repository_id,
                    document.file.path.clone(),
                ))
            }
            AnnotationKind::Comment | AnnotationKind::Question | AnnotationKind::Suggestion => {
                let side = parse_side(&value.side)?;
                if value.start_line == 0 || value.end_line < value.start_line {
                    return Err(DispatchError::Invalid("annotation range is invalid".into()));
                }
                let source = match side {
                    DiffSide::Old => &document.old.content,
                    DiffSide::New => &document.new.content,
                };
                let source_lines = source.lines().collect::<Vec<_>>();
                let start = usize::try_from(value.start_line.saturating_sub(1))
                    .map_err(|_| DispatchError::Invalid("annotation range is invalid".into()))?;
                let end = usize::try_from(value.end_line)
                    .map_err(|_| DispatchError::Invalid("annotation range is invalid".into()))?;
                if start >= source_lines.len() || end > source_lines.len() {
                    return Err(DispatchError::Invalid(
                        "annotation range is outside the captured source".into(),
                    ));
                }
                // Anchor context is always derived from the immutable reviewed
                // snapshot. Client supplied display text is never trusted as
                // the durable source identity.
                let selected_source = source_lines[start..end].join("\n");
                let context_start = start.saturating_sub(3);
                let context_end = usize::min(end.saturating_add(3), source_lines.len());
                let surrounding_context = source_lines[context_start..context_end].join("\n");
                Some(
                    AnnotationAnchor::from_line(localreview_domain::LineAnchorInput {
                        comparison_id: document.comparison_id,
                        repository_id,
                        file_path: document.file.path.clone(),
                        side,
                        start_line: value.start_line,
                        end_line: value.end_line,
                        selected_source,
                        surrounding_context,
                    })
                    .map_err(|error| DispatchError::Invalid(error.to_string()))?,
                )
            }
        };
        let annotation = Annotation {
            id,
            annotation_set_id: active.id,
            kind,
            state,
            publication_state: if value.local_only {
                PublicationState::LocalOnly
            } else {
                PublicationState::IncludedInNextReview
            },
            labels: value
                .labels
                .into_iter()
                .filter(|label| label != "localreview:deleted")
                .take(64)
                .collect(),
            body_markdown: value.body,
            anchor,
            created_at: existing.as_ref().map_or_else(
                || parse_time(&value.created_at).unwrap_or(now),
                |annotation| annotation.created_at,
            ),
            updated_at: now,
        };
        self.state().save_annotation(&annotation)?;
        Ok(annotation_view(
            &annotation,
            &[PersistedReviewDocument { document }],
        ))
    }

    pub fn archive_annotations(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<ReviewHistoryItem, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if matches!(workspace.source, WorkspaceSource::PullRequest { .. }) {
            self.service
                .ensure_no_unresolved_github_publication(workspace_id)?;
        }
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let cleared = self.service.clear_annotations(session.id, Utc::now())?;
        let annotations = self.state().annotations(cleared.archived.id)?;
        let documents = self.current_documents(session.id)?;
        Ok(ReviewHistoryItem {
            id: format!("set:{}", cleared.archived.id),
            label: "Cleared annotations".into(),
            created_at: cleared
                .archived
                .archived_at
                .unwrap_or_else(Utc::now)
                .to_rfc3339(),
            annotation_count: annotations.len(),
            item_type: "clear".into(),
            annotations: Some(
                annotations
                    .iter()
                    .map(|annotation| annotation_view(annotation, &documents))
                    .collect(),
            ),
        })
    }

    /// Restores a client-held checkpoint into the active review without ever
    /// resurrecting its globally unique annotation IDs. If the current set is
    /// non-empty it is archived first, so replacing a checkpoint is itself
    /// recoverable from review history.
    pub fn restore_annotations(
        &self,
        workspace_id: WorkspaceId,
        annotations: Vec<AnnotationView>,
    ) -> Result<ReviewData, DispatchError> {
        const MAX_RESTORE_ANNOTATIONS: usize = 10_000;
        if annotations.len() > MAX_RESTORE_ANNOTATIONS {
            return Err(DispatchError::Invalid(format!(
                "a restore may contain at most {MAX_RESTORE_ANNOTATIONS} annotations"
            )));
        }
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let session_documents = self.current_documents(session.id)?;

        // Validate the complete checkpoint before archiving anything. This
        // keeps malformed or cross-workspace client data from changing the
        // active set at all.
        for value in &annotations {
            let kind = parse_annotation_kind(&value.kind)?;
            if kind == AnnotationKind::ReviewNote {
                if value.start_line != 0 || value.end_line != 0 {
                    return Err(DispatchError::Invalid(
                        "review notes cannot carry a line range".into(),
                    ));
                }
                continue;
            }
            let file_id = parse_review_file_id(&value.file_id)
                .ok_or_else(|| DispatchError::Invalid("annotation fileId is invalid".into()))?;
            let document = self
                .state()
                .review_file_payload::<PersistedReviewDocument>(file_id)?
                .ok_or_else(|| DispatchError::NotFound(value.file_id.clone()))?
                .document;
            let owner = self
                .active_session_for_comparison(document.comparison_id)?
                .ok_or_else(|| DispatchError::Invalid("annotation file is not active".into()))?;
            if owner.id != session.id {
                return Err(DispatchError::Invalid(
                    "annotation checkpoint contains a file from another workspace".into(),
                ));
            }
            parse_annotation_state(&value.state)?;
            if kind == AnnotationKind::FileNote {
                if value.start_line != 0 || value.end_line != 0 {
                    return Err(DispatchError::Invalid(
                        "file notes cannot carry a line range".into(),
                    ));
                }
                continue;
            }
            let side = parse_side(&value.side)?;
            if value.start_line == 0 || value.end_line < value.start_line {
                return Err(DispatchError::Invalid("annotation range is invalid".into()));
            }
            let source = match side {
                DiffSide::Old => &document.old.content,
                DiffSide::New => &document.new.content,
            };
            let line_count = source.lines().count();
            let end = usize::try_from(value.end_line).unwrap_or(usize::MAX);
            if end > line_count {
                return Err(DispatchError::Invalid(
                    "annotation range is outside the captured source".into(),
                ));
            }
        }

        let active = self
            .state()
            .active_annotation_set(session.id)?
            .ok_or_else(|| DispatchError::Invalid("active review has no annotation set".into()))?;
        if !self.state().annotations(active.id)?.is_empty() {
            let _ = self.service.clear_annotations(session.id, Utc::now())?;
        }
        for mut annotation in annotations {
            // Client IDs may point at immutable archived rows. An empty
            // optimistic key guarantees save_annotation allocates a new ID.
            annotation.id.clear();
            if annotation.kind == "review_note" {
                // A review note has no durable file anchor. The save boundary
                // still needs an active captured document to establish the
                // target session, so use one only as non-persisted routing
                // context; `save_annotation` intentionally stores `None`.
                let document = session_documents.first().ok_or_else(|| {
                    DispatchError::Invalid("review has no captured files for note restore".into())
                })?;
                annotation.file_id = document.document.file.id.to_string();
                annotation.repository_id = document_repository_id(
                    &document.document,
                    &self.current_comparisons(session.id)?,
                )?
                .to_string();
                annotation.side = "new".into();
                annotation.start_line = 0;
                annotation.end_line = 0;
            }
            self.save_annotation(annotation)?;
        }
        self.load_review(workspace_id)
    }

    pub fn restore_history_item(
        &self,
        workspace_id: WorkspaceId,
        history_id: &str,
    ) -> Result<ReviewData, DispatchError> {
        let annotation_set = parse_history_set_id(history_id)
            .ok_or_else(|| DispatchError::Invalid("history item cannot be restored".into()))?;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let source = self
            .state()
            .annotation_set(annotation_set)?
            .ok_or_else(|| DispatchError::NotFound(history_id.into()))?;
        let target = self
            .state()
            .active_annotation_set(session.id)?
            .ok_or_else(|| DispatchError::Invalid("active review has no annotation set".into()))?;
        for mut annotation in self.state().annotations(source.id)? {
            annotation.id = AnnotationId::new();
            annotation.annotation_set_id = target.id;
            annotation.created_at = Utc::now();
            annotation.updated_at = annotation.created_at;
            annotation.publication_state = PublicationState::LocalOnly;
            if let Some(anchor) = annotation.anchor.as_mut() {
                // An archived review has a pinned older comparison. Keep it
                // recoverable but make the required reattachment explicit.
                anchor.outdated = true;
            }
            self.state().save_annotation(&annotation)?;
        }
        self.load_review(workspace_id)
    }

    pub fn generate_prompt(
        &self,
        workspace_id: WorkspaceId,
        input: PromptInput,
    ) -> Result<PromptPreview, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let reference = parse_prompt_history_reference(input.history_id.as_deref())?;
        let (source_session, source_sets, stored_scope) = match reference {
            None => {
                let active_session = self
                    .service
                    .active_review_session(workspace.id)?
                    .ok_or_else(|| {
                        DispatchError::Invalid("workspace has no active review".into())
                    })?;
                let active_set = self
                    .state()
                    .active_annotation_set(active_session.id)?
                    .ok_or_else(|| {
                        DispatchError::Invalid("active review has no annotation set".into())
                    })?;
                (active_session, vec![active_set], None)
            }
            Some(PromptHistoryReference::Set(set_id)) => {
                let set = self
                    .state()
                    .annotation_set(set_id)?
                    .ok_or_else(|| DispatchError::NotFound(set_id.to_string()))?;
                let session = self
                    .state()
                    .review_sessions_for_id(set.review_session_id)?
                    .ok_or_else(|| DispatchError::NotFound(set.review_session_id.to_string()))?;
                ensure_prompt_session_workspace(&session, workspace.id)?;
                (session, vec![set], None)
            }
            Some(PromptHistoryReference::Review(session_id)) => {
                let session = self
                    .state()
                    .review_sessions_for_id(session_id)?
                    .ok_or_else(|| DispatchError::NotFound(session_id.to_string()))?;
                ensure_prompt_session_workspace(&session, workspace.id)?;
                let sets = self.state().annotation_sets(session.id)?;
                if sets.is_empty() {
                    return Err(DispatchError::Invalid(
                        "review has no recoverable annotation sets".into(),
                    ));
                }
                (session, sets, None)
            }
            Some(PromptHistoryReference::Export(export_id)) => {
                let record = self
                    .state()
                    .prompt_export(export_id)?
                    .ok_or_else(|| DispatchError::NotFound(export_id.to_string()))?;
                let session = self
                    .state()
                    .review_sessions_for_id(record.review_session_id)?
                    .ok_or_else(|| DispatchError::NotFound(record.review_session_id.to_string()))?;
                ensure_prompt_session_workspace(&session, workspace.id)?;
                let set_ids = export_annotation_set_ids(&record);
                let sets = self.prompt_sets_for_session(&session, &set_ids)?;
                // A v2 record is an immutable handoff.  Never rerender it
                // against active data merely because History was reopened.
                if record.rendered_markdown.is_some() {
                    return Ok(prompt_preview_from_record(&record));
                }
                // Legacy exports had only a recipe. Regenerate it from the
                // saved source set/scope, then write a new byte-exact record.
                (session, sets, Some(record.scope))
            }
        };
        let source_set_ids = source_sets.iter().map(|set| set.id).collect::<Vec<_>>();
        let primary_set = *source_set_ids
            .first()
            .ok_or_else(|| DispatchError::Invalid("prompt source has no annotation set".into()))?;
        let annotations = source_sets
            .iter()
            .map(|set| self.state().annotations(set.id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let repositories = self.state().repositories(workspace.id)?;
        // A recoverable Clear/New Review checkpoint owns its original pinned
        // comparisons. Prompt regeneration must never accidentally splice it
        // into today's active snapshot.
        let comparisons = self.current_comparisons(source_session.id)?;
        let documents = self.current_documents(source_session.id)?;
        let selected_ids = input
            .annotation_ids
            .iter()
            .map(|id| {
                parse_annotation_id(id).ok_or_else(|| {
                    DispatchError::Invalid(format!("annotation id is invalid: {id}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let scope = stored_scope.unwrap_or(prompt_scope(&input.scope, selected_ids)?);
        if matches!(
            scope,
            PromptScope::Selected(_) | PromptScope::FocusedQuestion(_)
        ) {
            let valid = annotations
                .iter()
                .map(|annotation| annotation.id)
                .collect::<BTreeSet<_>>();
            let requested = match &scope {
                PromptScope::Selected(ids) => ids,
                PromptScope::FocusedQuestion(id) => std::slice::from_ref(id),
                _ => unreachable!("match guard above only permits selected scopes"),
            };
            if requested.iter().any(|id| !valid.contains(id)) {
                return Err(DispatchError::Invalid(
                    "selected annotation does not belong to this prompt source".into(),
                ));
            }
        }
        let entries = annotations
            .into_iter()
            .map(|annotation| {
                let context = annotation.anchor.as_ref().and_then(|anchor| {
                    let repository = repositories
                        .iter()
                        .find(|repository| repository.id == anchor.repository_id)?
                        .clone();
                    let comparison = comparisons.get(&repository.id)?.clone();
                    let relevant_hunk = documents
                        .iter()
                        .find(|document| annotation_matches_file(&annotation, &document.document))
                        .and_then(|document| hunk_for_annotation(&annotation, &document.document));
                    Some((repository, comparison, relevant_hunk))
                });
                PromptEntry {
                    annotation,
                    repository: context
                        .as_ref()
                        .map(|(repository, _, _)| repository.clone()),
                    comparison: context
                        .as_ref()
                        .map(|(_, comparison, _)| comparison.clone()),
                    relevant_hunk: context.and_then(|(_, _, hunk)| hunk),
                }
            })
            .collect();
        let exported = self.service.export_prompt(
            PromptRequest {
                workspace,
                review_session_id: source_session.id,
                annotation_set_id: primary_set,
                annotation_set_ids: source_set_ids,
                scope,
                options: PromptFormattingOptions {
                    path_style: prompt_path_style(input.path_style.as_deref(), input.portable)?,
                    include_diff_hunks: input.include_diff_hunks.unwrap_or(false),
                    include_git_state: input.include_git_state.unwrap_or(false),
                },
                entries,
            },
            Utc::now(),
        )?;
        Ok(prompt_preview(&exported.record, exported.formatted))
    }

    fn prompt_sets_for_session(
        &self,
        session: &ReviewSession,
        set_ids: &[AnnotationSetId],
    ) -> Result<Vec<AnnotationSet>, DispatchError> {
        if set_ids.is_empty() {
            return Err(DispatchError::Invalid(
                "prompt export has no source annotation set".into(),
            ));
        }
        let mut unique = BTreeSet::new();
        let mut sets = Vec::with_capacity(set_ids.len());
        for set_id in set_ids {
            if !unique.insert(*set_id) {
                continue;
            }
            let set = self
                .state()
                .annotation_set(*set_id)?
                .ok_or_else(|| DispatchError::NotFound(set_id.to_string()))?;
            if set.review_session_id != session.id {
                return Err(DispatchError::Invalid(
                    "prompt annotation set does not belong to its review".into(),
                ));
            }
            sets.push(set);
        }
        Ok(sets)
    }

    /// Saves one previously rendered export through a native, user-selected
    /// file dialog.  The caller never supplies a path or contents, so this
    /// command cannot be repurposed as arbitrary filesystem write access.
    pub fn save_prompt_export(
        &self,
        workspace_id: WorkspaceId,
        export_id: &str,
        format: PromptExportSaveFormat,
    ) -> Result<SavedPromptExport, DispatchError> {
        let export_id = Uuid::parse_str(export_id)
            .map(PromptExportId)
            .map_err(|_| DispatchError::Invalid("prompt export id is invalid".into()))?;
        let mut record = self
            .state()
            .prompt_export(export_id)?
            .ok_or_else(|| DispatchError::NotFound(export_id.to_string()))?;
        let session = self
            .state()
            .review_sessions_for_id(record.review_session_id)?
            .ok_or_else(|| DispatchError::NotFound(record.review_session_id.to_string()))?;
        ensure_prompt_session_workspace(&session, workspace_id)?;
        self.prompt_sets_for_session(&session, &export_annotation_set_ids(&record))?;

        // A legacy recipe can still be exported safely: first materialize a
        // new v2 export using its saved source/scope, then save those exact
        // newly durable bytes. No active annotation set is consulted.
        if record.rendered_markdown.is_none() {
            let preview = self.generate_prompt(
                workspace_id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: Some(format!("export:{}", record.id)),
                },
            )?;
            record = self
                .state()
                .prompt_export(
                    Uuid::parse_str(&preview.export_id)
                        .map(PromptExportId)
                        .map_err(|_| DispatchError::Internal)?,
                )?
                .ok_or_else(|| DispatchError::NotFound(preview.export_id))?;
        }
        let markdown = record
            .rendered_markdown
            .clone()
            .ok_or(DispatchError::Internal)?;
        let (extension, content, label) = match format {
            PromptExportSaveFormat::Markdown => ("md", markdown, "markdown"),
            PromptExportSaveFormat::Json => {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct PromptExportFile<'a> {
                    schema_version: u8,
                    export_id: String,
                    title: String,
                    annotation_count: usize,
                    estimated_tokens: usize,
                    template_version: u32,
                    scope: &'a PromptScope,
                    annotation_ids: Vec<String>,
                    annotation_set_ids: Vec<String>,
                    rendered_markdown: &'a str,
                    created_at: String,
                }
                let file = PromptExportFile {
                    schema_version: 1,
                    export_id: record.id.to_string(),
                    title: record
                        .title
                        .clone()
                        .unwrap_or_else(|| prompt_title_for_scope(&record.scope).to_owned()),
                    annotation_count: record
                        .annotation_count
                        .unwrap_or(record.annotation_ids.len()),
                    estimated_tokens: record
                        .estimated_tokens
                        .unwrap_or_else(|| markdown.len().div_ceil(4)),
                    template_version: record.template_version,
                    scope: &record.scope,
                    annotation_ids: record
                        .annotation_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    annotation_set_ids: export_annotation_set_ids(&record)
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    rendered_markdown: &markdown,
                    created_at: record.created_at.to_rfc3339(),
                };
                (
                    "json",
                    serde_json::to_string_pretty(&file)
                        .map_err(|error| DispatchError::Invalid(error.to_string()))?,
                    "structured JSON",
                )
            }
        };
        let filename = format!("localreview-prompt-{}.{}", record.id, extension);
        let Some(path) = rfd::FileDialog::new()
            .set_file_name(&filename)
            .add_filter("LocalReview prompt", &[extension])
            .save_file()
        else {
            return Ok(SavedPromptExport {
                saved: false,
                format: label.into(),
            });
        };
        std::fs::write(path, content).map_err(|error| {
            DispatchError::Invalid(format!("could not save prompt export: {error}"))
        })?;
        Ok(SavedPromptExport {
            saved: true,
            format: label.into(),
        })
    }

    pub fn get_settings(&self) -> Result<ReviewSettings, DispatchError> {
        let settings = self
            .state()
            .setting(SETTINGS_KEY)?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|error| {
                DispatchError::Invalid(format!("saved settings are invalid: {error}"))
            })?
            .unwrap_or_default();
        Ok(settings)
    }

    pub fn save_settings(
        &self,
        mut settings: ReviewSettings,
    ) -> Result<ReviewSettings, DispatchError> {
        settings.last_workspace_id = settings
            .last_workspace_id
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if settings
            .last_workspace_id
            .as_ref()
            .is_some_and(|value| value.len() > 128 || value.contains(['\0', '\n', '\r']))
        {
            return Err(DispatchError::Invalid(
                "last workspace id is invalid".into(),
            ));
        }
        settings.font_scale = (settings.font_scale * 10.0).round() / 10.0;
        settings.font_scale = settings.font_scale.clamp(0.75, 2.0);
        settings.left_width = settings.left_width.clamp(180, 420);
        settings.right_width = settings.right_width.clamp(240, 520);
        if !matches!(settings.theme.as_str(), "dark" | "light" | "system") {
            return Err(DispatchError::Invalid(
                "theme must be dark, light, or system".into(),
            ));
        }
        settings.code_font = settings.code_font.trim().to_owned();
        if settings.code_font.is_empty()
            || settings.code_font.len() > 128
            || settings.code_font.contains(['\0', '\n', '\r'])
        {
            return Err(DispatchError::Invalid("code font is invalid".into()));
        }
        if !matches!(
            settings.external_editor.as_str(),
            "system" | "vscode" | "cursor" | "zed" | "sublime" | "idea"
        ) {
            return Err(DispatchError::Invalid(
                "external editor must be system, vscode, cursor, zed, sublime, or idea".into(),
            ));
        }
        if !matches!(settings.tab_width, 2 | 4 | 8) {
            return Err(DispatchError::Invalid(
                "tab width must be 2, 4, or 8".into(),
            ));
        }
        if !matches!(
            settings.prompt_path_style.as_str(),
            "portable" | "qualified" | "absolute"
        ) {
            return Err(DispatchError::Invalid(
                "prompt path style must be portable, qualified, or absolute".into(),
            ));
        }
        if settings.shortcuts.len() > 64
            || settings.shortcuts.iter().any(|(action, shortcut)| {
                action.is_empty()
                    || action.len() > 64
                    || shortcut.is_empty()
                    || shortcut.len() > 64
                    || action.contains(['\0', '\n', '\r'])
                    || shortcut.contains(['\0', '\n', '\r'])
            })
        {
            return Err(DispatchError::Invalid(
                "keyboard shortcut settings are invalid".into(),
            ));
        }
        self.state().set_setting(
            SETTINGS_KEY,
            &serde_json::to_string(&settings)
                .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        )?;
        Ok(settings)
    }

    pub fn set_viewed(
        &self,
        workspace_id: WorkspaceId,
        file_id: ReviewFileId,
        viewed: bool,
    ) -> Result<(), DispatchError> {
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let mut state = self
            .state()
            .review_session_ui_state::<ReviewUiState>(session.id)?
            .unwrap_or_default();
        if viewed {
            state.viewed_file_ids.insert(file_id.to_string());
        } else {
            state.viewed_file_ids.remove(&file_id.to_string());
        }
        self.state()
            .save_review_session_ui_state(session.id, &state)?;
        Ok(())
    }

    pub fn configure_baselines(
        &self,
        workspace_id: WorkspaceId,
        input: ConfigureBaselinesInput,
    ) -> Result<ReviewData, DispatchError> {
        self.apply_baselines(
            workspace_id,
            input.default_base.as_deref(),
            &input.repository_bases,
        )?;
        self.load_review(workspace_id)
    }

    /// Reads setup metadata for every repository without capturing a review,
    /// fetching, changing refs, or touching the worktree/index.  Per-repo
    /// Git failures become table rows so a healthy sibling remains operable.
    pub fn repository_setup(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<RepositorySetupView>, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let comparison_by_repository = self
            .service
            .active_review_session(workspace_id)?
            .map(|session| self.current_comparisons(session.id))
            .transpose()?
            .unwrap_or_default();
        let application_default = self.application_base()?;
        let is_local = matches!(workspace.source, WorkspaceSource::LocalDirectory { .. });

        let mut views = Vec::new();
        for mut repository in self.state().repositories(workspace_id)? {
            let baseline = BaselineRequest {
                application_default: application_default.clone(),
                workspace_default: Some(workspace.default_base.clone()),
                repository_override: repository.base_override.clone(),
                temporary_override: None,
            }
            .effective();
            let pinned = comparison_by_repository.get(&repository.id);
            let mut live = RepositorySetupLive::default();

            if is_local {
                let git = localreview_git::GitRepository::open(repository.worktree_path.as_str());
                match git.inspect() {
                    Ok(identity) => {
                        repository.current_branch = identity.head;
                    }
                    Err(error) => live.comparison_error = Some(error.to_string()),
                }
                match git.status() {
                    Ok(status) => {
                        live.clean = Some(status.is_empty());
                        live.changed_file_count = Some(status.len());
                        live.status_summary = status_summary(&status);
                        live.status_checked_at = Some(Utc::now());
                    }
                    Err(error) => {
                        live.status_summary = "Status unavailable".into();
                        live.comparison_error
                            .get_or_insert_with(|| error.to_string());
                    }
                }
                live.suggested_base = git.primary_remote_head().ok().flatten();
                match git.resolve_comparison(
                    repository.id,
                    localreview_domain::ComparisonId::new(),
                    baseline.reference.clone(),
                    ComparisonOptions::default(),
                ) {
                    Ok(resolved) => {
                        repository.last_resolved_base_sha = Some(resolved.base_tip_sha.clone());
                        live.resolved_base_sha = Some(resolved.base_tip_sha.to_string());
                        live.merge_base_sha = Some(resolved.merge_base_sha.to_string());
                        live.head_sha = Some(resolved.head_sha.to_string());
                        repository.comparison_error = None;
                        match git.ahead_behind(&baseline.reference) {
                            Ok(divergence) => {
                                live.ahead = Some(divergence.ahead);
                                live.behind = Some(divergence.behind);
                            }
                            Err(error) => live.comparison_error = Some(error.to_string()),
                        }
                    }
                    Err(error) => {
                        let error = error.to_string();
                        repository.comparison_error = Some(error.clone());
                        live.comparison_error = Some(error);
                    }
                }
                self.state().upsert_repository(&repository)?;
            }

            views.push(repository_setup_view(&repository, &baseline, pinned, live));
        }
        Ok(views)
    }

    /// Persists a setup-table inclusion choice.  Disabling a repository never
    /// deletes its existing comparisons, annotations, or history; it simply
    /// omits it from the next explicit local capture.
    pub fn set_repository_inclusion(
        &self,
        workspace_id: WorkspaceId,
        input: SetRepositoryInclusionInput,
    ) -> Result<Vec<RepositorySetupView>, DispatchError> {
        self.require_local_setup_workspace(workspace_id)?;
        let selected = self.selected_repositories(workspace_id, &input.repository_ids)?;
        for mut repository in selected {
            repository.enabled = input.enabled;
            self.state().upsert_repository(&repository)?;
        }
        self.repository_setup(workspace_id)
    }

    /// Applies one already-validated reference as an explicit override to the
    /// selected repositories.  The workspace default is left untouched.
    pub fn apply_repository_base(
        &self,
        workspace_id: WorkspaceId,
        input: ApplyRepositoryBaseInput,
    ) -> Result<Vec<RepositorySetupView>, DispatchError> {
        self.require_local_setup_workspace(workspace_id)?;
        let base = BaseReference::new(input.base)
            .map_err(|error| DispatchError::Invalid(error.to_string()))?;
        let selected = self.selected_repositories(workspace_id, &input.repository_ids)?;
        for mut repository in selected {
            repository.base_override = Some(base.clone());
            repository.comparison_error = None;
            self.state().upsert_repository(&repository)?;
        }
        self.repository_setup(workspace_id)
    }

    /// Removes an explicit per-repository override so baseline precedence
    /// falls back to the workspace (then application) default again.
    pub fn reset_repository_base_overrides(
        &self,
        workspace_id: WorkspaceId,
        input: RepositorySelectionInput,
    ) -> Result<Vec<RepositorySetupView>, DispatchError> {
        self.require_local_setup_workspace(workspace_id)?;
        let selected = self.selected_repositories(workspace_id, &input.repository_ids)?;
        for mut repository in selected {
            repository.base_override = None;
            repository.comparison_error = None;
            self.state().upsert_repository(&repository)?;
        }
        self.repository_setup(workspace_id)
    }

    /// Fetches selected repositories, or all when no selection is supplied.
    /// Every target is attempted independently and its own result is durable;
    /// one unavailable remote never rolls back another repository's fetch.
    pub fn fetch_repositories(
        &self,
        workspace_id: WorkspaceId,
        repository_ids: Option<Vec<String>>,
    ) -> Result<Vec<RepositorySetupView>, DispatchError> {
        self.require_local_setup_workspace(workspace_id)?;
        let repositories = match repository_ids {
            Some(ids) => self.selected_repositories(workspace_id, &ids)?,
            None => self.state().repositories(workspace_id)?,
        };
        for mut repository in repositories {
            let now = Utc::now();
            match localreview_git::GitRepository::open(repository.worktree_path.as_str())
                .fetch_remote("origin")
            {
                Ok(()) => {
                    repository.last_fetch_at = Some(now);
                    repository.last_fetch_error = None;
                }
                Err(error) => {
                    repository.last_fetch_at = Some(now);
                    repository.last_fetch_error = Some(error.to_string());
                }
            }
            self.state().upsert_repository(&repository)?;
        }
        self.repository_setup(workspace_id)
    }

    pub fn start_new_review(
        &self,
        workspace_id: WorkspaceId,
        input: StartOrRefreshInput,
    ) -> Result<ReviewData, DispatchError> {
        if self.is_remote_workspace(workspace_id)? {
            // A remote companion capture is immutable and manifest-first;
            // "New review" is therefore the same explicit operation as
            // Refresh, never an implicit local Git attempt against a remote
            // path stored only as metadata.
            return self.refresh_remote_review_mode(workspace_id, input, true);
        }
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if matches!(workspace.source, WorkspaceSource::PullRequest { .. }) {
            self.service
                .ensure_no_unresolved_github_publication(workspace_id)?;
        }
        let options = input.comparison_options.clone().unwrap_or_default();
        self.apply_baselines(workspace_id, input.base.as_deref(), &input.repository_bases)?;
        let refresh_revision = matches!(workspace.source, WorkspaceSource::LocalDirectory { .. })
            .then(|| self.restart_local_watcher_for_capture(&workspace))
            .transpose()?;
        let settings = self.get_settings()?;
        if input.fetch_before_capture || settings.fetch_on_review {
            self.fetch_workspace_repositories(workspace_id)?;
        }
        let started = self.service.start_local_review(StartReviewRequest {
            workspace_id,
            application_default_base: self.application_base()?,
            temporary_base_overrides: BTreeMap::new(),
            options,
        })?;
        self.persist_local_capture_outcomes(workspace_id, &started)?;
        let refresh_outcome = local_refresh_outcome(&started);
        if started.failures.is_empty() {
            if let Some(refresh_revision) = refresh_revision {
                self.clear_refresh_available_at_boundary(workspace_id, refresh_revision);
            } else if matches!(workspace.source, WorkspaceSource::LocalDirectory { .. }) {
                // A GitHub start_new_review only archives the prior round and
                // captures its existing immutable pin. Provider freshness is
                // acknowledged exclusively by refresh_review below.
                self.clear_refresh_available(workspace_id);
            }
        }
        let mut review = self.load_review(workspace_id)?;
        review.refresh_outcome = Some(refresh_outcome);
        Ok(review)
    }

    pub fn refresh_review(
        &self,
        workspace_id: WorkspaceId,
        input: StartOrRefreshInput,
    ) -> Result<ReviewData, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if matches!(workspace.source, WorkspaceSource::PullRequest { .. }) {
            if input.base.is_some()
                || !input.repository_bases.is_empty()
                || input.comparison_options.is_some()
            {
                return Err(DispatchError::Invalid(
                    "GitHub PR bases and comparison rules are pinned by GitHub metadata; refresh the PR instead of overriding them".into(),
                ));
            }
            if let Err(error) = self
                .service
                .refresh_github_pull_request(workspace_id, self.application_base()?)
            {
                // The review-round boundary may already have succeeded. Keep
                // the provider retry visible and preserve the provider error.
                self.mark_refresh_available(workspace_id)?;
                return Err(error.into());
            }
            self.clear_refresh_available(workspace_id);
            return self.load_review(workspace_id);
        }
        if matches!(workspace.source, WorkspaceSource::RemoteDirectory { .. }) {
            return self.refresh_remote_review_mode(workspace_id, input, false);
        }
        let options = input.comparison_options.clone().unwrap_or_default();
        self.apply_baselines(workspace_id, input.base.as_deref(), &input.repository_bases)?;
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let settings = self.get_settings()?;
        if input.fetch_before_capture || settings.fetch_on_review {
            self.fetch_workspace_repositories(workspace_id)?;
        }
        let refresh_revision = self.restart_local_watcher_for_capture(&workspace)?;
        let refreshed = self.service.refresh_local_review(
            session.id,
            self.application_base()?,
            BTreeMap::new(),
            options,
        )?;
        self.persist_local_capture_outcomes(workspace_id, &refreshed)?;
        if refreshed.failures.is_empty() {
            self.clear_refresh_available_at_boundary(workspace_id, refresh_revision);
        } else {
            // A partial or failed capture leaves at least one repository on
            // its previous generation. Keep the retry affordance visible even
            // when the refresh was invoked manually before a watcher signal.
            self.mark_refresh_available(workspace_id)?;
        }
        let refresh_outcome = local_refresh_outcome(&refreshed);
        let mut review = self.load_review(workspace_id)?;
        review.refresh_outcome = Some(refresh_outcome);
        Ok(review)
    }

    /// Best-effort, per-repository fetch used only by an explicit refresh. A
    /// failed remote is persisted on that repository and never prevents a
    /// sibling repository from being captured against its last resolved base.
    fn fetch_workspace_repositories(&self, workspace_id: WorkspaceId) -> Result<(), DispatchError> {
        for mut repository in self.state().repositories(workspace_id)? {
            if !repository.enabled {
                continue;
            }
            let now = Utc::now();
            match localreview_git::GitRepository::open(repository.worktree_path.as_str())
                .fetch_remote("origin")
            {
                Ok(()) => {
                    repository.last_fetch_at = Some(now);
                    repository.last_fetch_error = None;
                }
                Err(error) => {
                    repository.last_fetch_at = Some(now);
                    repository.last_fetch_error = Some(error.to_string());
                }
            }
            self.state().upsert_repository(&repository)?;
        }
        Ok(())
    }

    fn persist_local_capture_outcomes(
        &self,
        workspace_id: WorkspaceId,
        result: &localreview_service::StartReviewResult,
    ) -> Result<(), DispatchError> {
        let successful = result
            .captures
            .iter()
            .map(|capture| {
                (
                    capture.comparison.repository_id,
                    capture.comparison.base_tip_sha.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let failures = result
            .failures
            .iter()
            .map(|failure| (failure.repository_id, failure.error.clone()))
            .collect::<BTreeMap<_, _>>();
        for mut repository in self.state().repositories(workspace_id)? {
            if let Some(base_sha) = successful.get(&repository.id) {
                repository.last_resolved_base_sha = Some(base_sha.clone());
                repository.comparison_error = None;
                self.state().upsert_repository(&repository)?;
            } else if let Some(error) = failures.get(&repository.id) {
                repository.comparison_error = Some(error.clone());
                self.state().upsert_repository(&repository)?;
            }
        }
        Ok(())
    }

    pub fn finish_review(
        &self,
        workspace_id: WorkspaceId,
        submission: FinishReviewSubmissionInput,
    ) -> Result<FinishReviewResult, DispatchError> {
        if submission.preview_token.is_empty()
            || submission.preview_token.len() > 256
            || submission.preview_token.contains(['\0', '\n', '\r'])
        {
            return Err(DispatchError::Invalid("preview token is invalid".into()));
        }
        let finished = self.service.finish_github_review_preview(
            localreview_service::FinishGitHubReviewSubmission {
                workspace_id,
                preview_token: submission.preview_token,
            },
        )?;
        Ok(FinishReviewResult {
            review_id: finished
                .publication
                .remote_review_id
                .map_or_else(|| finished.publication.id.clone(), |id| id.to_string()),
            annotation_count: finished.annotation_count,
            annotation_ids: finished
                .publication
                .annotation_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
            payload_json: finished.publication.payload_json,
            preview_token: finished.publication.preview_token,
            publication_status: match finished.publication.status {
                localreview_service::GitHubPublicationStatus::Submitted => "submitted",
                localreview_service::GitHubPublicationStatus::Reconciled => "reconciled",
                _ => "unexpected",
            }
            .to_owned(),
            submitted: true,
        })
    }

    /// Releases an unused durable preview, or a Prepared attempt only after a
    /// separate explicit user confirmation. Ordinary dialog cleanup always
    /// passes `false`, so an ambiguous POST is never silently discarded.
    pub fn abandon_finish_review(
        &self,
        workspace_id: WorkspaceId,
        submission: FinishReviewSubmissionInput,
        confirm_prepared: bool,
    ) -> Result<(), DispatchError> {
        if submission.preview_token.is_empty()
            || submission.preview_token.len() > 256
            || submission.preview_token.contains(['\0', '\n', '\r'])
        {
            return Err(DispatchError::Invalid("preview token is invalid".into()));
        }
        let publication = self
            .state()
            .github_publication_by_attempt::<localreview_service::GitHubPublicationRecord>(
                &submission.preview_token,
            )?
            .filter(|record| record.preview_token == submission.preview_token)
            .ok_or_else(|| DispatchError::NotFound(submission.preview_token.clone()))?;
        let may_abandon = publication.status
            == localreview_service::GitHubPublicationStatus::Previewed
            || (confirm_prepared
                && publication.status == localreview_service::GitHubPublicationStatus::Prepared);
        if !may_abandon {
            return Err(DispatchError::Invalid(
                "only an unused Previewed token, or an explicitly confirmed unresolved Prepared token, can be abandoned".into(),
            ));
        }
        self.service.abandon_github_review_publication(
            localreview_service::FinishGitHubReviewSubmission {
                workspace_id,
                preview_token: submission.preview_token,
            },
        )?;
        Ok(())
    }

    pub fn preview_finish_review(
        &self,
        workspace_id: WorkspaceId,
        request: FinishReviewInput,
    ) -> Result<FinishReviewPreview, DispatchError> {
        if request.annotation_ids.len() > 10_000 || request.summary.len() > 100_000 {
            return Err(DispatchError::Invalid("review payload is too large".into()));
        }
        let annotation_ids = request
            .annotation_ids
            .iter()
            .map(|id| {
                parse_annotation_id(id).ok_or_else(|| {
                    DispatchError::Invalid(format!("annotation id is invalid: {id}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let conclusion = github_review_conclusion(request.conclusion);
        let preview = self
            .service
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids,
                summary_markdown: request.summary,
                conclusion,
            })?;
        Ok(FinishReviewPreview {
            annotation_count: preview.annotation_ids.len(),
            annotation_ids: preview
                .annotation_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
            payload_json: preview.prepared.payload_json,
            pinned_head_sha: preview.prepared.pinned_head_sha.to_string(),
            preview_token: preview.preview_token,
            request_fingerprint: preview.prepared.request_fingerprint,
            preview_request_fingerprint: preview.preview_request_fingerprint,
            annotation_snapshot_fingerprint: preview.annotation_snapshot_fingerprint,
            requires_reconciliation: preview.requires_reconciliation,
        })
    }

    pub fn archive_workspace(&self, workspace_id: WorkspaceId) -> Result<(), DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if matches!(workspace.source, WorkspaceSource::PullRequest { .. }) {
            let managed_worktree_exists =
                self.service
                    .github_pull_request(workspace_id)
                    .is_ok_and(|review| {
                        Path::new(review.managed_worktree.worktree_path.as_str()).is_dir()
                    });
            if managed_worktree_exists {
                self.service
                    .delete_github_pull_request_workspace(workspace_id)?;
            } else {
                let mut archived = workspace;
                archived.archived_at = Some(Utc::now());
                archived.updated_at = Utc::now();
                self.state().upsert_workspace(&archived)?;
            }
            self.clear_refresh_available(workspace_id);
            return Ok(());
        }
        if matches!(workspace.source, WorkspaceSource::RemoteDirectory { .. }) {
            self.stop_remote_watchers(workspace_id);
            if let Ok(mut sessions) = self.remote_sessions.lock() {
                // Dropping the managed session terminates its fixed `ssh ...
                // localreview agent --stdio` child; no remote workspace files
                // are ever deleted by a local review deletion.
                sessions.remove(&workspace_id);
            }
        }
        let mut archived = workspace;
        archived.archived_at = Some(Utc::now());
        archived.updated_at = Utc::now();
        self.state().upsert_workspace(&archived)?;
        if let Ok(mut watchers) = self.local_watchers.lock() {
            watchers.remove(&workspace_id);
        }
        self.clear_refresh_available(workspace_id);
        Ok(())
    }

    pub fn delete_workspace(&self, workspace_id: WorkspaceId) -> Result<(), DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;

        // A GitHub workspace owns exactly one isolated worktree. Reuse the
        // conservative clean-worktree removal boundary before purging its
        // durable review records; the shared mirror remains an app-wide cache.
        if matches!(workspace.source, WorkspaceSource::PullRequest { .. }) {
            let managed_worktree_exists =
                self.service
                    .github_pull_request(workspace_id)
                    .is_ok_and(|review| {
                        Path::new(review.managed_worktree.worktree_path.as_str()).is_dir()
                    });
            if managed_worktree_exists {
                self.service
                    .delete_github_pull_request_workspace(workspace_id)?;
            }
        }
        if matches!(workspace.source, WorkspaceSource::RemoteDirectory { .. }) {
            self.stop_remote_watchers(workspace_id);
            if let Ok(mut sessions) = self.remote_sessions.lock() {
                sessions.remove(&workspace_id);
            }
        }
        if let Ok(mut watchers) = self.local_watchers.lock() {
            watchers.remove(&workspace_id);
        }
        self.clear_refresh_available(workspace_id);

        let mut settings = self.get_settings()?;
        let workspace_id_string = workspace_id.to_string();
        if settings.last_workspace_id.as_deref() == Some(workspace_id_string.as_str()) {
            settings.last_workspace_id = None;
            self.save_settings(settings)?;
        }
        let exact_setting_keys = vec![
            legacy_annotation_draft_key(workspace_id),
            remote_workspace_key(workspace_id),
            legacy_remote_workspace_key(workspace_id),
        ];
        let per_session_setting_prefixes = vec![format!("{ANNOTATION_DRAFT_KEY_PREFIX}session.")];
        if !self.state().purge_workspace(
            workspace_id,
            &exact_setting_keys,
            &per_session_setting_prefixes,
        )? {
            return Err(DispatchError::NotFound(workspace_id.to_string()));
        }

        // Presentation caches contain no durable ownership index. A permanent
        // deletion is rare, so clearing the bounded caches is simpler and
        // safer than retaining source-derived entries from the purged review.
        if let Ok(mut cache) = self.presentation_cache.lock() {
            *cache = PresentationCache::default();
        }
        if let Ok(mut cache) = self.language_attribute_cache.lock() {
            cache.clear();
        }
        if let Ok(mut jobs) = self.presentation_jobs.lock() {
            *jobs = PresentationJobRegistry::default();
        }
        Ok(())
    }

    pub fn github_pull_request(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GitHubPullRequestContextView, DispatchError> {
        Ok(github_pull_request_context_view(
            self.service.github_pull_request(workspace_id)?,
        ))
    }

    pub fn github_threads(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ImportedGitHubReviewThreadView>, DispatchError> {
        Ok(self
            .service
            .github_imported_threads(workspace_id)?
            .into_iter()
            .map(imported_github_thread_view)
            .collect())
    }

    pub fn github_conversation(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ImportedGitHubConversationCommentView>, DispatchError> {
        Ok(self
            .service
            .github_imported_conversation(workspace_id)?
            .into_iter()
            .map(imported_github_conversation_view)
            .collect())
    }

    pub fn history(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ReviewHistoryItem>, DispatchError> {
        let sessions = self.state().review_sessions(workspace_id)?;
        let mut items = Vec::new();
        for session in sessions {
            let documents = self.current_documents(session.id)?;
            let sets = self.state().annotation_sets(session.id)?;
            for set in sets {
                if set.active && session.status == ReviewSessionStatus::Active {
                    continue;
                }
                let annotations = self.state().annotations(set.id)?;
                items.push(ReviewHistoryItem {
                    id: format!("set:{}", set.id),
                    label: "Annotation checkpoint".into(),
                    created_at: set.archived_at.unwrap_or(set.created_at).to_rfc3339(),
                    annotation_count: annotations.len(),
                    item_type: "clear".into(),
                    annotations: Some(
                        annotations
                            .iter()
                            .map(|annotation| annotation_view(annotation, &documents))
                            .collect(),
                    ),
                });
            }
            for export in self.state().prompt_exports(session.id)? {
                items.push(ReviewHistoryItem {
                    id: format!("export:{}", export.id),
                    label: export
                        .title
                        .clone()
                        .unwrap_or_else(|| prompt_title_for_scope(&export.scope).to_owned()),
                    created_at: export.created_at.to_rfc3339(),
                    annotation_count: export.annotation_ids.len(),
                    item_type: "export".into(),
                    annotations: None,
                });
            }
            if session.status != ReviewSessionStatus::Active {
                let count = sets_annotation_count(self.state(), session.id)?;
                items.push(ReviewHistoryItem {
                    id: format!("review:{}", session.id),
                    label: "Archived review".into(),
                    created_at: session
                        .archived_at
                        .unwrap_or(session.started_at)
                        .to_rfc3339(),
                    annotation_count: count,
                    item_type: "review".into(),
                    annotations: None,
                });
            }
        }
        items.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        Ok(items)
    }

    pub fn accept_request_id(
        &self,
        request_id: &str,
        issued_at_unix_secs: u64,
    ) -> Result<(), DispatchError> {
        const RETENTION_SECONDS: u64 = 5 * 60;
        const MAX_IDS: usize = 4_096;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut replay_guard = self
            .replay_guard
            .lock()
            .map_err(|_| DispatchError::Internal)?;
        while let Some((timestamp, _)) = replay_guard.ordered.front() {
            if now.saturating_sub(*timestamp) <= RETENTION_SECONDS
                && replay_guard.ordered.len() <= MAX_IDS
            {
                break;
            }
            if let Some((_, expired)) = replay_guard.ordered.pop_front() {
                replay_guard.seen.remove(&expired);
            }
        }
        if !replay_guard.seen.insert(request_id.into()) {
            return Err(DispatchError::Invalid(
                "request has already been processed".into(),
            ));
        }
        replay_guard
            .ordered
            .push_back((issued_at_unix_secs, request_id.into()));
        Ok(())
    }

    fn workspace_view(&self, workspace: &Workspace) -> Result<WorkspaceView, DispatchError> {
        // History browsing must be passive.  An archived snapshot can be
        // rendered without reviving filesystem watchers for a workspace the
        // user intentionally removed from the active rail.
        if workspace.archived_at.is_none() {
            self.ensure_local_watcher(workspace)?;
        }
        let session = self.service.active_review_session(workspace.id)?;
        let documents = match &session {
            Some(session) => self.current_documents(session.id)?,
            None => Vec::new(),
        };
        let viewed = match &session {
            Some(session) => {
                self.state()
                    .review_session_ui_state::<ReviewUiState>(session.id)?
                    .unwrap_or_default()
                    .viewed_file_ids
            }
            None => BTreeSet::new(),
        };
        let draft_count = match &session {
            Some(session) => self
                .state()
                .active_annotation_set(session.id)?
                .map(|set| self.state().annotations(set.id))
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .filter(|annotation| {
                    annotation.state == AnnotationState::Open
                        && annotation.publication_state != PublicationState::Published
                })
                .count(),
            None => 0,
        };
        let refresh_availability = self.refresh_availability(workspace.id)?;
        let (location, detail, refresh_available, connection) = match &workspace.source {
            WorkspaceSource::PullRequest { url, number, .. } => {
                let detail = self.service.github_pull_request(workspace.id).map_or_else(
                    |_| format!("GitHub pull request #{number}"),
                    |review| {
                        format!(
                            "GitHub PR #{} · pinned {}",
                            review.number,
                            &review.pinned_head_sha.as_str()[..7]
                        )
                    },
                );
                (
                    url.clone(),
                    detail,
                    refresh_availability.available,
                    "connected".into(),
                )
            }
            WorkspaceSource::RemoteDirectory { host, root } => {
                let metadata = self.remote_metadata(workspace.id).ok();
                let (connection, agent_detail) =
                    self.remote_connection_view(workspace.id, metadata.as_ref());
                let stale = metadata.as_ref().is_some_and(|metadata| metadata.stale);
                let detail = metadata.as_ref().map_or_else(
                    || {
                        format!(
                            "{} remote repositories",
                            self.state()
                                .repositories(workspace.id)
                                .map_or(0, |items| items.len())
                        )
                    },
                    |metadata| {
                        let version = metadata.agent_version.as_deref().unwrap_or("unknown agent");
                        let latency = metadata
                            .latency_millis
                            .map(|milliseconds| format!(" · {milliseconds}ms"))
                            .unwrap_or_default();
                        let refresh = if stale { " · Refresh required" } else { "" };
                        let companion = metadata
                            .companion
                            .as_ref()
                            .map(|status| format!(" · {}", remote_companion_status_detail(status)))
                            .unwrap_or_default();
                        format!(
                            "{} · {}{}{}{}",
                            metadata.captures.len(),
                            version,
                            latency,
                            refresh,
                            companion,
                        )
                    },
                );
                (
                    format!("{host}:{}", root.as_str()),
                    format!("{detail} · {agent_detail}"),
                    stale || refresh_availability.available,
                    connection,
                )
            }
            _ => (
                workspace.source.root().as_str().to_owned(),
                format!(
                    "{} repositories",
                    self.state().repositories(workspace.id)?.len()
                ),
                refresh_availability.available,
                "connected".into(),
            ),
        };
        Ok(WorkspaceView {
            id: workspace.id.to_string(),
            name: workspace.display_name.clone(),
            source: workspace
                .source
                .tags()
                .into_iter()
                .map(source_name)
                .map(str::to_owned)
                .collect(),
            location,
            detail,
            default_base: workspace.default_base.as_str().to_owned(),
            progress: ReviewProgress {
                viewed: documents
                    .iter()
                    .filter(|document| viewed.contains(&document.document.file.id.to_string()))
                    .count(),
                total: documents.len(),
            },
            draft_count,
            pinned: workspace.pinned,
            refresh_available,
            refresh_available_revision: refresh_availability.revision,
            connection,
            review_ready: session.is_some(),
            archived: workspace.archived_at.is_some(),
        })
    }

    fn remote_connection_view(
        &self,
        workspace_id: WorkspaceId,
        metadata: Option<&RemoteWorkspaceMetadata>,
    ) -> (String, String) {
        let Some(runtime) = self
            .remote_sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&workspace_id).cloned())
        else {
            return ("offline".into(), "reconnect when ready".into());
        };
        let Ok(runtime) = runtime.lock() else {
            return ("offline".into(), "connection state unavailable".into());
        };
        match runtime.session.state() {
            SshConnectionState::Connected {
                agent_version,
                latency,
            } => (
                "connected".into(),
                format!("{agent_version} · {}ms", latency.as_millis()),
            ),
            SshConnectionState::Connecting => ("connecting".into(), "connecting companion".into()),
            SshConnectionState::Disconnected { detail } => (
                "offline".into(),
                metadata
                    .and_then(|metadata| metadata.last_error.clone())
                    .unwrap_or(detail),
            ),
        }
    }

    /// Starts one recursive OS watcher per local workspace. Notifications only
    /// toggle presentation state; Git capture and the immutable review remain
    /// untouched until `refresh_review` is explicitly invoked.
    fn ensure_local_watcher(&self, workspace: &Workspace) -> Result<(), DispatchError> {
        if !matches!(workspace.source, WorkspaceSource::LocalDirectory { .. }) {
            return Ok(());
        }
        let mut watchers = self
            .local_watchers
            .lock()
            .map_err(|_| DispatchError::Internal)?;
        if watchers.contains_key(&workspace.id) {
            return Ok(());
        }
        let workspace_id = workspace.id;
        let watcher_epoch = self.advance_refresh_watcher_epoch(workspace_id)?;
        let repository_roots = self
            .state()
            .repositories(workspace.id)?
            .into_iter()
            .filter(|repository| repository.enabled)
            .map(|repository| {
                let root = PathBuf::from(repository.worktree_path.as_str());
                // macOS may report /private/var/... for a watcher registered
                // through /var/.... Registering and matching the canonical
                // root prevents such aliases from bypassing ignore checks.
                root.canonicalize().unwrap_or(root)
            })
            .collect::<Vec<_>>();
        let flags = Arc::clone(&self.refresh_available);
        let app_handle = Arc::clone(&self.app_handle);
        // notify callbacks are owned by the platform watcher thread. Git
        // ignore/index checks run on this workspace worker instead, and a
        // drain coalesces event storms into at most one Git process per
        // repository and batch.
        let (event_sender, event_receiver) = std::sync::mpsc::channel::<notify::Event>();
        let worker_roots = repository_roots.clone();
        let worker_flags = Arc::clone(&flags);
        let worker_app_handle = Arc::clone(&app_handle);
        std::thread::Builder::new()
            .name(format!("localreview-watch-{workspace_id}"))
            .spawn(move || {
                while let Ok(first) = event_receiver.recv() {
                    let mut events = vec![first];
                    events.extend(event_receiver.try_iter());
                    let semantic = local_events_can_change_source(&worker_roots, &events);
                    if semantic {
                        publish_refresh_availability(
                            &worker_flags,
                            &worker_app_handle,
                            workspace_id,
                            Some(watcher_epoch),
                            true,
                        );
                    }
                }
            })
            .map_err(|error| {
                DispatchError::Invalid(format!("could not start workspace monitor worker: {error}"))
            })?;
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                let candidate = event.need_rescan()
                    || (matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Modify(_)
                            | EventKind::Remove(_)
                            | EventKind::Any
                    ) && (event.paths.is_empty()
                        || event
                            .paths
                            .iter()
                            .any(|path| local_event_can_change_source(path))));
                if candidate {
                    let _ = event_sender.send(event);
                }
            })
            .map_err(|error| {
                DispatchError::Invalid(format!("could not monitor workspace changes: {error}"))
            })?;
        let mut watched_any = false;
        for repository_root in repository_roots {
            if watcher
                .watch(&repository_root, RecursiveMode::Recursive)
                .is_ok()
            {
                watched_any = true;
            }
        }
        if watched_any {
            watchers.insert(workspace.id, watcher);
        }
        Ok(())
    }

    /// A capture owns a fresh watcher generation from before its first Git
    /// read. Any callback already queued by the previous generation is then
    /// harmless, while a real source event during capture advances the new
    /// revision and prevents the success acknowledgement from clearing it.
    fn restart_local_watcher_for_capture(
        &self,
        workspace: &Workspace,
    ) -> Result<RefreshCaptureBoundary, DispatchError> {
        self.advance_refresh_watcher_epoch(workspace.id)?;
        if let Ok(mut watchers) = self.local_watchers.lock() {
            watchers.remove(&workspace.id);
        }
        self.ensure_local_watcher(workspace)?;
        let state = self.refresh_availability(workspace.id)?;
        Ok(RefreshCaptureBoundary {
            revision: state.revision,
            watcher_epoch: state.watcher_epoch,
        })
    }

    fn refresh_availability(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<RefreshAvailability, DispatchError> {
        Ok(self
            .refresh_available
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .get(&workspace_id)
            .copied()
            .unwrap_or_default())
    }

    fn advance_refresh_watcher_epoch(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<u64, DispatchError> {
        let mut flags = self
            .refresh_available
            .lock()
            .map_err(|_| DispatchError::Internal)?;
        let state = flags.entry(workspace_id).or_default();
        state.watcher_epoch = state.watcher_epoch.saturating_add(1).max(1);
        Ok(state.watcher_epoch)
    }

    fn clear_refresh_available(&self, workspace_id: WorkspaceId) {
        publish_refresh_availability(
            &self.refresh_available,
            &self.app_handle,
            workspace_id,
            None,
            false,
        );
    }

    fn clear_refresh_available_at_boundary(
        &self,
        workspace_id: WorkspaceId,
        boundary: RefreshCaptureBoundary,
    ) {
        publish_refresh_availability_at_boundary(
            &self.refresh_available,
            &self.app_handle,
            workspace_id,
            boundary,
            false,
        );
    }

    /// Runs remote watcher operations on dedicated SSH transports. A normal
    /// review transport is intentionally serialized for capture/source work;
    /// these sessions receive only typed filesystem-change notifications and
    /// never auto-capture or send source bytes to the UI.
    fn start_remote_watchers(
        &self,
        workspace_id: WorkspaceId,
        metadata: &RemoteWorkspaceMetadata,
        wait_until_ready: bool,
    ) -> Result<RefreshCaptureBoundary, DispatchError> {
        self.stop_remote_watchers(workspace_id);
        let watcher_epoch = self.advance_refresh_watcher_epoch(workspace_id)?;
        let boundary = RefreshCaptureBoundary {
            revision: self.refresh_availability(workspace_id)?.revision,
            watcher_epoch,
        };
        let flags = Arc::clone(&self.refresh_available);
        let app_handle = Arc::clone(&self.app_handle);
        // One typed watcher tracks every capture over a single dedicated SSH
        // transport. The protocol bounds this list at 1,024, so opening a
        // 100-repository workspace never creates 100 child processes or waits
        // for 100 handshakes, while every enabled repository still contributes
        // to the notification fingerprint.
        const MAX_REMOTE_WORKSPACE_WATCH_REPOSITORIES: usize = 1_024;
        let mut seen = BTreeSet::new();
        let mut repositories = Vec::new();
        for capture in &metadata.captures {
            if repositories.len() >= MAX_REMOTE_WORKSPACE_WATCH_REPOSITORIES {
                break;
            }
            let reference = capture.capture.repository.clone();
            let reference_key = format!("{}:{}", reference.workspace_root, reference.relative_path);
            if !seen.insert(reference_key) {
                continue;
            }
            repositories.push(reference);
        }
        if repositories.is_empty() {
            if wait_until_ready {
                self.mark_refresh_available(workspace_id)?;
                return Err(DispatchError::Remote(
                    "SSH change watcher has no captured repositories to monitor".into(),
                ));
            }
            return Ok(boundary);
        }
        let request_id = format!("watch-workspace-{workspace_id}");
        let generation = metadata.generation.max(1);
        let target = metadata.target.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::new(Mutex::new(None::<SshCancellation>));
        let callback_cancelled = Arc::clone(&cancelled);
        let notification_cancelled = Arc::clone(&cancelled);
        let callback_cancellation = Arc::clone(&cancellation);
        let callback_request_id = request_id.clone();
        let (readiness_sender, readiness_receiver) =
            std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let progress_readiness_sender = readiness_sender.clone();
        let watcher_thread = std::thread::Builder::new()
            .name("localreview-ssh-workspace-watch".into())
            .spawn(move || {
                // Connection/bootstrap run off the UI path. The live review
                // transport remains independent from this notification-only
                // request, and a stop request is honored even if it arrives
                // before the handshake completes.
                if callback_cancelled.load(Ordering::Acquire) {
                    let _ =
                        readiness_sender.try_send(Err("SSH change watcher was cancelled".into()));
                    return;
                }
                let mut session = match connect_remote_session(&target, None) {
                    Ok(RemoteSessionConnect { session, .. }) => session,
                    Err(error) => {
                        let _ = readiness_sender.try_send(Err(format!(
                            "could not connect SSH change watcher: {}",
                            error.error
                        )));
                        return;
                    }
                };
                let remote_cancellation = session.cancellation();
                {
                    let Ok(mut slot) = callback_cancellation.lock() else {
                        let _ = readiness_sender
                            .try_send(Err("SSH change watcher state is unavailable".into()));
                        return;
                    };
                    if callback_cancelled.load(Ordering::Acquire) {
                        let _ = remote_cancellation.cancel(&callback_request_id, generation);
                        let _ = readiness_sender
                            .try_send(Err("SSH change watcher was cancelled".into()));
                        return;
                    }
                    *slot = Some(remote_cancellation);
                }
                let result = session.request_with_id_and_notifications(
                    callback_request_id.clone(),
                    AgentOperation::WatchWorkspaceChanges {
                        repositories,
                        poll_interval_millis: 1_000,
                    },
                    generation,
                    Duration::from_secs(24 * 60 * 60),
                    move |event| {
                        if event.progress.phase == AgentProgressPhase::Watching {
                            let _ = progress_readiness_sender.try_send(Ok(()));
                        }
                    },
                    move |notification| {
                        if !notification_cancelled.load(Ordering::Acquire)
                            && matches!(
                                notification,
                                AgentNotification::FilesystemChangesAvailable {
                                    generation: notification_generation,
                                    ..
                                } if notification_generation == generation
                            )
                        {
                            publish_refresh_availability(
                                &flags,
                                &app_handle,
                                workspace_id,
                                Some(watcher_epoch),
                                true,
                            );
                        }
                    },
                );
                // A protocol-valid watch stays active until cancellation. If
                // it returns before announcing Watching, fail readiness
                // immediately rather than making refresh sit on the full
                // timeout for an error response or premature cancellation.
                let stopped = match result {
                    Ok(result) => {
                        format!("SSH change watcher stopped before ready with result: {result:?}")
                    }
                    Err(error) => format!("SSH change watcher stopped before ready: {error}"),
                };
                let _ = readiness_sender.try_send(Err(stopped));
            });
        if let Err(error) = watcher_thread {
            if wait_until_ready {
                self.mark_refresh_available(workspace_id)?;
            }
            return Err(DispatchError::Remote(format!(
                "could not start SSH change watcher: {error}"
            )));
        }
        self.remote_watchers
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .insert(
                workspace_id,
                vec![RemoteWatcher {
                    cancelled,
                    cancellation,
                    request_id,
                    generation,
                }],
            );
        if wait_until_ready {
            match readiness_receiver.recv_timeout(Duration::from_secs(15)) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    self.stop_remote_watchers(workspace_id);
                    self.mark_refresh_available(workspace_id)?;
                    return Err(DispatchError::Remote(error));
                }
                Err(error) => {
                    self.stop_remote_watchers(workspace_id);
                    self.mark_refresh_available(workspace_id)?;
                    return Err(DispatchError::Remote(format!(
                        "SSH change watcher did not become ready: {error}"
                    )));
                }
            }
        }
        Ok(boundary)
    }

    fn stop_remote_watchers(&self, workspace_id: WorkspaceId) {
        let watchers = self
            .remote_watchers
            .lock()
            .ok()
            .and_then(|mut watchers| watchers.remove(&workspace_id));
        if let Some(watchers) = watchers {
            for watcher in watchers {
                watcher.cancelled.store(true, Ordering::Release);
                if let Ok(cancellation) = watcher.cancellation.lock() {
                    if let Some(cancellation) = cancellation.as_ref() {
                        let _ = cancellation.cancel(&watcher.request_id, watcher.generation);
                    }
                }
            }
        }
    }

    fn require_local_setup_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<(), DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if !matches!(workspace.source, WorkspaceSource::LocalDirectory { .. }) {
            return Err(DispatchError::Invalid(
                "repository setup controls are currently available for local workspaces only"
                    .into(),
            ));
        }
        Ok(())
    }

    fn selected_repositories(
        &self,
        workspace_id: WorkspaceId,
        ids: &[String],
    ) -> Result<Vec<Repository>, DispatchError> {
        const MAX_SETUP_SELECTION: usize = 1_000;
        if ids.is_empty() || ids.len() > MAX_SETUP_SELECTION {
            return Err(DispatchError::Invalid(format!(
                "select between one and {MAX_SETUP_SELECTION} repositories"
            )));
        }
        let mut selected = BTreeSet::new();
        for value in ids {
            let id = Uuid::parse_str(value)
                .map(RepositoryId)
                .map_err(|_| DispatchError::Invalid("repositoryId is invalid".into()))?;
            if !selected.insert(id) {
                return Err(DispatchError::Invalid(
                    "repository selection contains a duplicate".into(),
                ));
            }
        }
        let repositories = self.state().repositories(workspace_id)?;
        let selected_repositories = repositories
            .into_iter()
            .filter(|repository| selected.contains(&repository.id))
            .collect::<Vec<_>>();
        if selected_repositories.len() != selected.len() {
            return Err(DispatchError::Invalid(
                "repository selection contains an item outside this workspace".into(),
            ));
        }
        Ok(selected_repositories)
    }

    fn apply_baselines(
        &self,
        workspace_id: WorkspaceId,
        default_base: Option<&str>,
        entries: &[RepositoryBaseInput],
    ) -> Result<(), DispatchError> {
        if let Some(default_base) = default_base {
            let mut workspace = self
                .state()
                .workspace(workspace_id)?
                .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
            workspace.default_base = BaseReference::new(default_base)
                .map_err(|error| DispatchError::Invalid(error.to_string()))?;
            workspace.updated_at = Utc::now();
            self.state().upsert_workspace(&workspace)?;
        }
        let repositories = self.state().repositories(workspace_id)?;
        for input in entries {
            let repository = repositories.iter().find(|repository| {
                input
                    .repository_id
                    .as_deref()
                    .is_some_and(|id| id == repository.id.to_string())
                    || input
                        .repository_path
                        .as_deref()
                        .is_some_and(|path| path == repository.relative_path.as_str())
                    || input
                        .relative_path
                        .as_deref()
                        .is_some_and(|path| path == repository.relative_path.as_str())
            });
            let Some(repository) = repository else {
                return Err(DispatchError::Invalid(
                    "repository baseline target was not discovered".into(),
                ));
            };
            let mut updated = repository.clone();
            updated.base_override = input
                .base
                .as_deref()
                .map(BaseReference::new)
                .transpose()
                .map_err(|error| DispatchError::Invalid(error.to_string()))?;
            self.state().upsert_repository(&updated)?;
        }
        Ok(())
    }

    fn application_base(&self) -> Result<BaseReference, DispatchError> {
        let raw = self.state().setting(APPLICATION_BASE_KEY)?;
        match raw {
            Some(raw) => {
                let value: String = serde_json::from_str(&raw).map_err(|error| {
                    DispatchError::Invalid(format!("saved base is invalid: {error}"))
                })?;
                BaseReference::new(value).map_err(|error| DispatchError::Invalid(error.to_string()))
            }
            None => Ok(self
                .service
                .global_file_config()?
                .default_base
                .unwrap_or_default()),
        }
    }

    fn current_comparisons(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<BTreeMap<RepositoryId, localreview_domain::RepositoryComparison>, DispatchError>
    {
        let mut current = BTreeMap::new();
        for comparison in self.state().current_comparisons_for_session(session_id)? {
            current.insert(comparison.repository_id, comparison);
        }
        Ok(current)
    }

    fn is_remote_workspace(&self, workspace_id: WorkspaceId) -> Result<bool, DispatchError> {
        Ok(matches!(
            self.state()
                .workspace(workspace_id)?
                .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?
                .source,
            WorkspaceSource::RemoteDirectory { .. }
        ))
    }

    fn find_remote_workspace(
        &self,
        target: &RemoteTarget,
    ) -> Result<Option<Workspace>, DispatchError> {
        Ok(self.state().workspaces()?.into_iter().find(|workspace| {
            matches!(
                &workspace.source,
                WorkspaceSource::RemoteDirectory { host, root }
                    if host == &target.host && root.as_str() == target.root
            ) && workspace.archived_at.is_none()
        }))
    }

    fn remote_metadata(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<RemoteWorkspaceMetadata, DispatchError> {
        let current_key = remote_workspace_key(workspace_id);
        let current = self.state().setting(&current_key)?;
        let (raw, legacy) = match current {
            Some(raw) => (raw, false),
            None => self
                .state()
                .setting(&legacy_remote_workspace_key(workspace_id))?
                .map(|raw| (raw, true))
                .ok_or_else(|| {
                    DispatchError::NotFound(format!("remote metadata for {workspace_id}"))
                })?,
        };
        let mut metadata: RemoteWorkspaceMetadata =
            serde_json::from_str(&raw).map_err(|error| {
                DispatchError::Invalid(format!("remote workspace metadata is invalid: {error}"))
            })?;
        // V3 manifests are source-free and can be retained, but future source
        // windows must use the v4 byte-exact decoder. Promote their opaque
        // metadata key/schema on first read; the legacy key remains harmless
        // recovery history and no v3 companion is negotiated.
        if legacy || metadata.schema_version < 4 {
            metadata.schema_version = 4;
            self.state().set_setting(
                &current_key,
                &serde_json::to_string(&metadata)
                    .map_err(|error| DispatchError::Invalid(error.to_string()))?,
            )?;
        }
        Ok(metadata)
    }

    fn save_remote_metadata(
        &self,
        workspace_id: WorkspaceId,
        metadata: &RemoteWorkspaceMetadata,
    ) -> Result<(), DispatchError> {
        self.state().set_setting(
            &remote_workspace_key(workspace_id),
            &serde_json::to_string(metadata)
                .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        )?;
        Ok(())
    }

    /// Returns the persistent per-workspace transport, reconnecting lazily
    /// after a desktop restart. The fixed SSH client continues to honour the
    /// user's host config, host-key policy, ProxyJump, and key setup.
    fn ensure_remote_session(
        &self,
        workspace_id: WorkspaceId,
        metadata: &mut RemoteWorkspaceMetadata,
    ) -> Result<Arc<Mutex<RemoteWorkspaceRuntime>>, DispatchError> {
        if let Some(runtime) = self
            .remote_sessions
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .get(&workspace_id)
            .cloned()
        {
            let connected = runtime
                .lock()
                .map_err(|_| DispatchError::Internal)?
                .session
                .state();
            if !matches!(connected, SshConnectionState::Disconnected { .. }) {
                return Ok(runtime);
            }
            let RemoteSessionConnect {
                session,
                companion,
                reverse_forward,
            } = connect_remote_session(
                &metadata.target,
                self.managed_forward_handler(&metadata.target.host),
            )
            .map_err(|failure| {
                metadata.companion = Some(failure.companion);
                metadata.last_error = Some(failure.error.to_string());
                let _ = self.save_remote_metadata(workspace_id, metadata);
                *failure.error
            })?;
            let mut runtime_guard = runtime.lock().map_err(|_| DispatchError::Internal)?;
            runtime_guard.session = session;
            runtime_guard.reverse_forward = reverse_forward;
            metadata.companion = Some(companion);
            metadata.last_error = None;
            self.save_remote_metadata(workspace_id, metadata)?;
            return Ok(Arc::clone(&runtime));
        }
        let RemoteSessionConnect {
            session,
            companion,
            reverse_forward,
        } = connect_remote_session(
            &metadata.target,
            self.managed_forward_handler(&metadata.target.host),
        )
        .map_err(|failure| {
            metadata.companion = Some(failure.companion);
            metadata.last_error = Some(failure.error.to_string());
            let _ = self.save_remote_metadata(workspace_id, metadata);
            *failure.error
        })?;
        metadata.companion = Some(companion);
        metadata.last_error = None;
        self.save_remote_metadata(workspace_id, metadata)?;
        let runtime = Arc::new(Mutex::new(RemoteWorkspaceRuntime {
            session,
            reverse_forward,
        }));
        self.remote_sessions
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .insert(workspace_id, Arc::clone(&runtime));
        Ok(runtime)
    }

    /// Materializes a single selected remote file. The capture manifest is
    /// already durable; every request below uses both its opaque capture ID
    /// and generation and rejects mismatched source hashes before combining
    /// any window. This is the boundary that prevents a stale companion reply
    /// from silently joining a newer review.
    fn ensure_remote_file_materialized(&self, file_id: ReviewFileId) -> Result<(), DispatchError> {
        self.ensure_remote_file_materialized_for_comparison(file_id, None)
    }

    fn ensure_remote_file_materialized_for_comparison(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
    ) -> Result<(), DispatchError> {
        let Some((workspace_id, mut metadata, capture_index, file_index)) =
            self.remote_file_binding(file_id, comparison_id)?
        else {
            return Ok(());
        };
        if metadata.captures[capture_index].files[file_index].materialized {
            return Ok(());
        }
        let binding = metadata.captures[capture_index].clone();
        let file_binding = binding.files[file_index].clone();
        let review_file = remote_review_file(file_id, &file_binding.file)?;
        if remote_file_requires_non_text(&file_binding.file) {
            // A binary/LFS/gitlink entry remains intentionally source-free.
            // Its canonical placeholder is already sufficient for status UI.
            metadata.captures[capture_index].files[file_index].materialized = true;
            self.save_remote_metadata(workspace_id, &metadata)?;
            return Ok(());
        }
        let runtime = match self.ensure_remote_session(workspace_id, &mut metadata) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.mark_remote_stale(workspace_id, &mut metadata, error.to_string())?;
                return Err(error);
            }
        };
        let old_path = file_binding
            .file
            .old_path
            .as_deref()
            .unwrap_or(&file_binding.file.path);
        let old = if remote_file_has_old_source(&file_binding.file) {
            match self.read_remote_source(
                workspace_id,
                &runtime,
                &binding.capture,
                old_path,
                RemoteSourceRevision::MergeBase,
            ) {
                Ok(source) => source,
                Err(error) => {
                    self.mark_remote_stale(workspace_id, &mut metadata, error.to_string())?;
                    return Err(error);
                }
            }
        } else {
            String::new()
        };
        let new = if remote_file_has_new_source(&file_binding.file) {
            match self.read_remote_source(
                workspace_id,
                &runtime,
                &binding.capture,
                &file_binding.file.path,
                RemoteSourceRevision::Worktree,
            ) {
                Ok(source) => source,
                Err(error) => {
                    self.mark_remote_stale(workspace_id, &mut metadata, error.to_string())?;
                    return Err(error);
                }
            }
        } else {
            String::new()
        };
        let document = PersistedReviewDocument {
            document: document_from_sources(
                parse_comparison_id(&binding.comparison_id).ok_or_else(|| {
                    DispatchError::Invalid("remote comparison id is invalid".into())
                })?,
                review_file,
                old,
                new,
            ),
        };
        self.state().save_review_file_payload(
            &binding.comparison_id,
            file_id,
            document.document.file.path.as_str(),
            &document,
        )?;
        metadata.captures[capture_index].files[file_index].materialized = true;
        metadata.stale = false;
        metadata.last_error = None;
        self.save_remote_metadata(workspace_id, &metadata)?;
        self.clear_presentation_cache(file_id)?;
        Ok(())
    }

    fn persisted_review_document(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
    ) -> Result<PersistedReviewDocument, DispatchError> {
        let document = match comparison_id {
            Some(comparison_id) => self
                .state()
                .review_file_payload_for_comparison(comparison_id, file_id)?,
            None => self.state().review_file_payload(file_id)?,
        };
        document.ok_or_else(|| DispatchError::NotFound(file_id.to_string()))
    }

    fn cached_persisted_review_document(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
    ) -> Result<Arc<PersistedReviewDocument>, DispatchError> {
        const MAX_DOCUMENT_ENTRIES: usize = 16;
        const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
        let key = format!(
            "{}:{}",
            comparison_id.map_or_else(|| "latest".into(), |value| value.to_string()),
            file_id
        );
        {
            let mut cache = self
                .presentation_cache
                .lock()
                .map_err(|_| DispatchError::Internal)?;
            if let Some(cached) = cache.documents.get(&key).cloned() {
                cache.document_order.retain(|candidate| candidate != &key);
                cache.document_order.push_back(key);
                return Ok(cached);
            }
        }
        let document = Arc::new(self.persisted_review_document(file_id, comparison_id)?);
        let bytes = document
            .document
            .old
            .content
            .len()
            .saturating_add(document.document.new.content.len());
        if bytes <= MAX_DOCUMENT_BYTES {
            let mut cache = self
                .presentation_cache
                .lock()
                .map_err(|_| DispatchError::Internal)?;
            if let Some(previous) = cache.documents.insert(key.clone(), Arc::clone(&document)) {
                cache.document_bytes = cache.document_bytes.saturating_sub(
                    previous
                        .document
                        .old
                        .content
                        .len()
                        .saturating_add(previous.document.new.content.len()),
                );
            }
            cache.document_bytes = cache.document_bytes.saturating_add(bytes);
            cache.document_order.retain(|candidate| candidate != &key);
            cache.document_order.push_back(key);
            while cache.document_order.len() > MAX_DOCUMENT_ENTRIES
                || cache.document_bytes > MAX_DOCUMENT_BYTES
            {
                if let Some(expired) = cache.document_order.pop_front() {
                    if let Some(removed) = cache.documents.remove(&expired) {
                        cache.document_bytes = cache.document_bytes.saturating_sub(
                            removed
                                .document
                                .old
                                .content
                                .len()
                                .saturating_add(removed.document.new.content.len()),
                        );
                    }
                }
            }
        }
        Ok(document)
    }

    fn remote_file_binding(
        &self,
        file_id: ReviewFileId,
        comparison_id: Option<ComparisonId>,
    ) -> Result<Option<(WorkspaceId, RemoteWorkspaceMetadata, usize, usize)>, DispatchError> {
        let needle = file_id.to_string();
        for workspace in self.state().workspaces()? {
            if !matches!(workspace.source, WorkspaceSource::RemoteDirectory { .. }) {
                continue;
            }
            let Ok(metadata) = self.remote_metadata(workspace.id) else {
                continue;
            };
            for (capture_index, capture) in metadata.captures.iter().enumerate() {
                if comparison_id
                    .is_some_and(|comparison_id| capture.comparison_id != comparison_id.to_string())
                {
                    continue;
                }
                if let Some(file_index) =
                    capture.files.iter().position(|file| file.file_id == needle)
                {
                    return Ok(Some((workspace.id, metadata, capture_index, file_index)));
                }
            }
        }
        Ok(None)
    }

    fn read_remote_source(
        &self,
        workspace_id: WorkspaceId,
        runtime: &Arc<Mutex<RemoteWorkspaceRuntime>>,
        capture: &RemoteComparisonCapture,
        path: &str,
        revision: RemoteSourceRevision,
    ) -> Result<String, DispatchError> {
        let mut session = runtime.lock().map_err(|_| DispatchError::Internal)?;
        let mut start_line = 1_u32;
        let mut expected_hash = None::<String>;
        let mut expected_total = None::<u32>;
        let mut output = Vec::new();
        // A pathological companion must not make an unchecked number of
        // requests. The protocol already caps the manifest at 50k files; this
        // source cap is intentionally independent and fails as stale rather
        // than allocating an unbounded presentation document.
        const MAX_WINDOWS_PER_FILE: usize = 16_384;
        const MAX_MATERIALIZED_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
        for _ in 0..MAX_WINDOWS_PER_FILE {
            let result = session.session.request(
                AgentOperation::ReadSourceWindow {
                    capture_id: capture.capture_id.clone(),
                    capture_generation: capture.generation,
                    repository: capture.repository.clone(),
                    path: path.into(),
                    revision,
                    start_line,
                    line_count: MAX_REMOTE_SOURCE_WINDOW_LINES,
                },
                capture.generation,
                |_| {},
            );
            self.drain_remote_notifications(
                workspace_id,
                &mut session.session,
                capture.generation,
            )?;
            let result = result.map_err(remote_transport_error)?;
            let AgentResult::SourceWindow { window } = result else {
                return Err(remote_result_error("remote source window", result));
            };
            validate_remote_source_window(&window, capture, path, revision, start_line)?;
            if expected_hash
                .as_deref()
                .is_some_and(|expected| expected != window.content_sha256_hex)
                || expected_total.is_some_and(|expected| expected != window.total_lines)
            {
                return Err(DispatchError::Remote(
                    "remote source changed while windows were being read; refresh is required"
                        .into(),
                ));
            }
            expected_hash.get_or_insert_with(|| window.content_sha256_hex.clone());
            expected_total.get_or_insert(window.total_lines);
            if window.byte_len > MAX_MATERIALIZED_SOURCE_BYTES
                || u64::try_from(output.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(u64::try_from(window.bytes.len()).unwrap_or(u64::MAX))
                    > MAX_MATERIALIZED_SOURCE_BYTES
            {
                return Err(DispatchError::Remote(format!(
                    "remote source is larger than the {} MiB materialization limit; it remains metadata-only until a narrower source mode is available",
                    MAX_MATERIALIZED_SOURCE_BYTES / (1024 * 1024)
                )));
            }
            output.extend_from_slice(&window.bytes);
            if window.end_of_file {
                if u64::try_from(output.len()).unwrap_or(u64::MAX) != window.byte_len
                    || hex::encode(Sha256::digest(&output)) != window.content_sha256_hex
                {
                    return Err(DispatchError::Remote(
                        "remote source bytes did not match the captured hash/length; refresh is required"
                            .into(),
                    ));
                }
                return String::from_utf8(output).map_err(|_| {
                    DispatchError::Remote(
                        "remote source was not valid UTF-8 text; it cannot be shown as a code diff"
                            .into(),
                    )
                });
            }
            if window.bytes.is_empty() {
                return Err(DispatchError::Remote(
                    "remote source window made no progress; refresh is required".into(),
                ));
            }
            start_line = start_line.saturating_add(MAX_REMOTE_SOURCE_WINDOW_LINES);
        }
        Err(DispatchError::Remote(
            "remote source exceeded the bounded window budget; narrow the file or refresh".into(),
        ))
    }

    fn drain_remote_notifications(
        &self,
        workspace_id: WorkspaceId,
        session: &mut SshSession,
        generation: u64,
    ) -> Result<(), DispatchError> {
        let notifications = session.take_notifications();
        if notifications.into_iter().any(|notification| {
            matches!(notification,
                AgentNotification::FilesystemChangesAvailable { generation: notification_generation, .. }
                    if notification_generation >= generation
            )
        }) {
            self.mark_refresh_available(workspace_id)?;
        }
        Ok(())
    }

    fn mark_remote_stale(
        &self,
        workspace_id: WorkspaceId,
        metadata: &mut RemoteWorkspaceMetadata,
        error: String,
    ) -> Result<(), DispatchError> {
        metadata.stale = true;
        metadata.last_error = Some(error);
        self.save_remote_metadata(workspace_id, metadata)?;
        self.mark_refresh_available(workspace_id)
    }

    fn mark_refresh_available(&self, workspace_id: WorkspaceId) -> Result<(), DispatchError> {
        // Take the lock once here so callers still receive poisoning as a
        // controller error rather than silently losing the refresh signal.
        drop(
            self.refresh_available
                .lock()
                .map_err(|_| DispatchError::Internal)?,
        );
        publish_refresh_availability(
            &self.refresh_available,
            &self.app_handle,
            workspace_id,
            None,
            true,
        );
        Ok(())
    }

    fn clear_presentation_cache(&self, file_id: ReviewFileId) -> Result<(), DispatchError> {
        self.presentation_jobs
            .lock()
            .map_err(|_| DispatchError::Internal)?
            .cancel_file(file_id);
        let mut cache = self
            .presentation_cache
            .lock()
            .map_err(|_| DispatchError::Internal)?;
        let prefix = format!("{}:", file_id);
        let document_suffix = format!(":{file_id}");
        cache
            .documents
            .retain(|key, _| !key.ends_with(&document_suffix));
        cache
            .document_order
            .retain(|key| !key.ends_with(&document_suffix));
        cache.document_bytes = cache
            .documents
            .values()
            .map(|entry| {
                entry
                    .document
                    .old
                    .content
                    .len()
                    .saturating_add(entry.document.new.content.len())
            })
            .sum();
        cache.canonical.retain(|key, _| !key.starts_with(&prefix));
        cache.canonical_rows = cache.canonical.values().map(|entry| entry.rows.len()).sum();
        cache
            .canonical_order
            .retain(|key| !key.starts_with(&prefix));
        cache
            .structural
            .retain(|key, _| !key.contains(&file_id.to_string()));
        cache
            .structural_order
            .retain(|key| !key.contains(&file_id.to_string()));
        Ok(())
    }

    fn refresh_remote_review_mode(
        &self,
        workspace_id: WorkspaceId,
        input: StartOrRefreshInput,
        replace_active_review: bool,
    ) -> Result<ReviewData, DispatchError> {
        let comparison_options = input.comparison_options.clone().unwrap_or_default();
        let remote_options = remote_comparison_options(&comparison_options);
        self.apply_baselines(workspace_id, input.base.as_deref(), &input.repository_bases)?;
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let mut metadata = self.remote_metadata(workspace_id)?;
        // Establish a fresh notification generation before capture. Queued
        // callbacks from the previous transport are invalid, while a remote
        // source event observed during this capture advances the revision and
        // prevents the result from clearing amber.
        let refresh_boundary = self.start_remote_watchers(workspace_id, &metadata, true)?;
        let review_session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        if replace_active_review {
            // The companion retains only a small, process-local capture cache.
            // Before its binding is replaced, make every text document in the
            // outgoing review locally durable. This includes files the user
            // never opened and is the last point at which exact worktree bytes
            // can still be requested from that immutable capture.
            for document in self.current_documents(review_session.id)? {
                self.ensure_remote_file_materialized_for_comparison(
                    document.document.file.id,
                    Some(document.document.comparison_id),
                )?;
            }
            metadata = self.remote_metadata(workspace_id)?;
        }
        let generation = metadata.generation.saturating_add(1).max(1);
        let runtime = match self.ensure_remote_session(workspace_id, &mut metadata) {
            Ok(runtime) => runtime,
            Err(error) => {
                self.mark_remote_stale(workspace_id, &mut metadata, error.to_string())?;
                return self.load_review(workspace_id);
            }
        };
        let discovered = {
            let mut session = runtime.lock().map_err(|_| DispatchError::Internal)?;
            let result = session.session.request(
                AgentOperation::DiscoverRepositories {
                    root: metadata.target.root.clone(),
                    max_depth: localreview_protocol::MAX_REMOTE_DISCOVERY_DEPTH,
                },
                generation,
                |_| {},
            );
            self.drain_remote_notifications(workspace_id, &mut session.session, generation)?;
            match result {
                Ok(AgentResult::Repositories { repositories }) => repositories,
                Ok(result) => {
                    let error = remote_result_error("repository discovery", result);
                    self.mark_remote_stale(workspace_id, &mut metadata, error.to_string())?;
                    return self.load_review(workspace_id);
                }
                Err(error) => {
                    let error = remote_transport_error(error);
                    self.mark_remote_stale(workspace_id, &mut metadata, error.to_string())?;
                    return self.load_review(workspace_id);
                }
            }
        };
        let existing_documents = self.current_documents(review_session.id)?;
        let previous_ids = remote_previous_file_ids(&existing_documents);
        let repositories = self.state().repositories(workspace_id)?;
        let prior_captures = metadata.captures.clone();
        let mut captures = if replace_active_review {
            Vec::new()
        } else {
            metadata.captures.clone()
        };
        let mut prepared_generations = Vec::new();
        // Repository records are part of the captured generation: never
        // write their resolved-base/error state before the matching manifest
        // and document rows can be committed with it.
        let mut staged_repositories = Vec::new();
        let mut succeeded = false;
        let mut errors = Vec::new();
        for remote in discovered {
            let mut repository = match repositories
                .iter()
                .find(|repository| {
                    repository.relative_path.as_str() == remote.reference.relative_path
                })
                .cloned()
            {
                Some(repository) => repository,
                None => {
                    // Discovery can find a repository that did not exist in
                    // the original snapshot. It joins this review only at
                    // the same promotion boundary as its first capture.
                    remote_repository_record(workspace_id, &remote)?
                }
            };
            let base = repository
                .base_override
                .as_ref()
                .unwrap_or(&workspace.default_base)
                .as_str()
                .to_owned();
            let result = {
                let mut session = runtime.lock().map_err(|_| DispatchError::Internal)?;
                let result = session.session.request(
                    AgentOperation::CaptureComparison {
                        repository: remote.reference.clone(),
                        base,
                        options: remote_options.clone(),
                    },
                    generation,
                    |_| {},
                );
                self.drain_remote_notifications(workspace_id, &mut session.session, generation)?;
                result
            };
            match result {
                Ok(AgentResult::ComparisonCapture { capture }) => {
                    let comparison = remote_comparison(&repository, &capture, &comparison_options)?;
                    let (documents, bindings) =
                        remote_placeholder_documents_with_ids(&comparison, &capture, &previous_ids);
                    prepared_generations.push(self.state().prepare_review_generation(
                        &comparison,
                        &review_generation_rows(documents),
                    )?);
                    repository.last_resolved_base_sha = Some(comparison.merge_base_sha.clone());
                    repository.discovery_error = None;
                    staged_repositories.push(repository.clone());
                    captures.retain(|entry| entry.repository_id != repository.id.to_string());
                    captures.push(RemoteCaptureBinding {
                        repository_id: repository.id.to_string(),
                        comparison_id: comparison.id.to_string(),
                        capture,
                        files: bindings,
                    });
                    succeeded = true;
                }
                Ok(result) => {
                    let error = remote_result_error("comparison capture", result).to_string();
                    repository.discovery_error = Some(error.clone());
                    staged_repositories.push(repository);
                    errors.push(error);
                }
                Err(error) => {
                    let error = remote_transport_error(error).to_string();
                    repository.discovery_error = Some(error.clone());
                    staged_repositories.push(repository);
                    errors.push(error);
                }
            }
        }
        metadata.generation = generation;
        metadata.captures = if replace_active_review && !succeeded {
            prior_captures
        } else {
            captures
        };
        metadata.stale = !errors.is_empty();
        metadata.last_error = errors.into_iter().next();
        if succeeded {
            metadata.agent_version = Some(
                runtime
                    .lock()
                    .map_err(|_| DispatchError::Internal)?
                    .session
                    .connection
                    .hello
                    .agent_version
                    .clone(),
            );
            metadata.latency_millis = Some(
                runtime
                    .lock()
                    .map_err(|_| DispatchError::Internal)?
                    .session
                    .connection
                    .latency
                    .as_millis(),
            );
            if replace_active_review {
                let now = Utc::now();
                let replacement = ReviewSession {
                    id: ReviewSessionId::new(),
                    workspace_id,
                    status: ReviewSessionStatus::Active,
                    started_at: now,
                    refreshed_at: None,
                    archived_at: None,
                    completed_at: None,
                };
                let annotation_set = AnnotationSet {
                    id: AnnotationSetId::new(),
                    review_session_id: replacement.id,
                    sequence: 1,
                    active: true,
                    archived_at: None,
                    created_at: now,
                };
                metadata.review_session_id = replacement.id.to_string();
                let metadata_json = serde_json::to_string(&metadata)
                    .map_err(|error| DispatchError::Invalid(error.to_string()))?;
                self.state().replace_active_review_with_remote_state(
                    RemoteReviewReplacementPromotion {
                        workspace_id,
                        session: &replacement,
                        active_annotation_set: &annotation_set,
                        generations: &prepared_generations,
                        repositories: &staged_repositories,
                        archived_at: now,
                        setting: Some((&remote_workspace_key(workspace_id), &metadata_json)),
                    },
                )?;
            } else {
                let mut refreshed = review_session;
                refreshed.refreshed_at = Some(Utc::now());
                let metadata_json = serde_json::to_string(&metadata)
                    .map_err(|error| DispatchError::Invalid(error.to_string()))?;
                self.state()
                    .save_prepared_remote_refresh_with_setting_and_repositories(
                        &refreshed,
                        &prepared_generations,
                        &staged_repositories,
                        &remote_workspace_key(workspace_id),
                        &metadata_json,
                    )?;
            }
        } else {
            // No remote capture succeeded. Keep the current review/session
            // exactly as it was and only persist the reconnect/error state.
            self.save_remote_metadata(workspace_id, &metadata)?;
        }
        if metadata.stale {
            self.mark_refresh_available(workspace_id)?;
        } else {
            self.clear_refresh_available_at_boundary(workspace_id, refresh_boundary);
        }
        self.load_review(workspace_id)
    }

    fn current_documents(
        &self,
        session_id: ReviewSessionId,
    ) -> Result<Vec<PersistedReviewDocument>, DispatchError> {
        let comparisons = self.current_comparisons(session_id)?;
        let ids = comparisons
            .values()
            .map(|comparison| comparison.id.to_string())
            .collect::<Vec<_>>();
        Ok(self.state().review_file_payloads_for_comparisons(&ids)?)
    }

    fn active_session_for_comparison(
        &self,
        comparison_id: localreview_domain::ComparisonId,
    ) -> Result<Option<ReviewSession>, DispatchError> {
        for workspace in self.state().workspaces()? {
            if let Some(session) = self.service.active_review_session(workspace.id)? {
                if self
                    .current_comparisons(session.id)?
                    .values()
                    .any(|comparison| comparison.id == comparison_id)
                {
                    return Ok(Some(session));
                }
            }
        }
        Ok(None)
    }

    /// Finds the owner of a retained comparison even after its review was
    /// archived.  Read-only presentation/history calls use this; mutation
    /// paths intentionally continue to require `active_session_for_comparison`.
    fn session_for_comparison(
        &self,
        comparison_id: localreview_domain::ComparisonId,
    ) -> Result<Option<ReviewSession>, DispatchError> {
        for workspace in self.state().workspaces()? {
            for session in self.state().review_sessions(workspace.id)? {
                if self
                    .current_comparisons(session.id)?
                    .values()
                    .any(|comparison| comparison.id == comparison_id)
                {
                    return Ok(Some(session));
                }
            }
        }
        Ok(None)
    }

    fn annotations_for_session(
        &self,
        session: &ReviewSession,
    ) -> Result<Vec<Annotation>, DispatchError> {
        if session.status == ReviewSessionStatus::Active {
            let active = self
                .state()
                .active_annotation_set(session.id)?
                .ok_or_else(|| {
                    DispatchError::Invalid("active review has no annotation set".into())
                })?;
            return Ok(self
                .state()
                .annotations(active.id)?
                .into_iter()
                .filter(|annotation| !is_soft_deleted(annotation))
                .collect());
        }
        // A completed review aggregates its archived annotation checkpoints.
        // This is the same durable ownership model used by review-level
        // prompt exports, and never turns old IDs back into mutable rows.
        let mut annotations = Vec::new();
        for set in self.state().annotation_sets(session.id)? {
            annotations.extend(
                self.state()
                    .annotations(set.id)?
                    .into_iter()
                    .filter(|annotation| !is_soft_deleted(annotation)),
            );
        }
        Ok(annotations)
    }

    fn annotations_for_document(
        &self,
        document: &ReviewDiffDocument,
    ) -> Result<Vec<Annotation>, DispatchError> {
        let Some(session) = self.session_for_comparison(document.comparison_id)? else {
            return Ok(Vec::new());
        };
        self.annotations_for_session(&session)
    }

    fn cached_canonical_rows(
        &self,
        document: &ReviewDiffDocument,
        mode: &str,
        full_file_view: FullFileView,
        state: &ReviewUiState,
    ) -> Result<Arc<CachedCanonicalPresentation>, DispatchError> {
        const MAX_CANONICAL_ENTRIES: usize = 16;
        const MAX_CANONICAL_ROWS: usize = 100_000;
        let expansion_key = serde_json::to_string(&(
            &state.hunk_context_lines,
            &state.expanded_full_file_deletion_blocks,
            &state.collapsed_full_file_addition_blocks,
        ))
        .map_err(|_| DispatchError::Internal)?;
        let key = format!(
            "{}:{}:{}:{}:{}",
            document.file.id,
            document.comparison_id,
            mode,
            full_file_view.name(),
            expansion_key
        );
        {
            let mut cache = self
                .presentation_cache
                .lock()
                .map_err(|_| DispatchError::Internal)?;
            if let Some(cached) = cache.canonical.get(&key).cloned() {
                cache.canonical_order.retain(|candidate| candidate != &key);
                cache.canonical_order.push_back(key);
                return Ok(cached);
            }
        }
        let cached = Arc::new(build_canonical_presentation(
            document,
            mode,
            full_file_view,
            state,
        )?);
        // A row-budgeted LRU keeps a handful of large full-file projections
        // fast without allowing many huge files or expansion states to remain
        // resident. Responses share the immutable cache entry and clone only
        // their bounded visible window.
        if cached.rows.len() <= MAX_CANONICAL_ROWS {
            let mut cache = self
                .presentation_cache
                .lock()
                .map_err(|_| DispatchError::Internal)?;
            if let Some(previous) = cache.canonical.insert(key.clone(), Arc::clone(&cached)) {
                cache.canonical_rows = cache.canonical_rows.saturating_sub(previous.rows.len());
            }
            cache.canonical_rows = cache.canonical_rows.saturating_add(cached.rows.len());
            cache.canonical_order.retain(|candidate| candidate != &key);
            cache.canonical_order.push_back(key);
            while cache.canonical_order.len() > MAX_CANONICAL_ENTRIES
                || cache.canonical_rows > MAX_CANONICAL_ROWS
            {
                if let Some(expired) = cache.canonical_order.pop_front() {
                    if let Some(removed) = cache.canonical.remove(&expired) {
                        cache.canonical_rows =
                            cache.canonical_rows.saturating_sub(removed.rows.len());
                    }
                }
            }
        }
        Ok(cached)
    }

    fn mark_annotation_rows(
        &self,
        document: &ReviewDiffDocument,
        rows: &mut [DiffRowView],
    ) -> Result<(), DispatchError> {
        let annotations = self.annotations_for_document(document)?;
        for row in rows {
            row.has_annotation = row.old_line.is_some_and(|line| {
                annotation_overlaps(
                    &annotations,
                    document,
                    DiffSide::Old,
                    line,
                    row.omitted_end_line.unwrap_or(line),
                )
            }) || row.new_line.is_some_and(|line| {
                annotation_overlaps(
                    &annotations,
                    document,
                    DiffSide::New,
                    line,
                    row.omitted_end_line.unwrap_or(line),
                )
            });
        }
        Ok(())
    }

    fn highlight_language_attribute(
        &self,
        document: &ReviewDiffDocument,
        session_id: ReviewSessionId,
    ) -> Option<String> {
        let key = (
            document.comparison_id.to_string(),
            document.file.path.to_string(),
        );
        if let Ok(cache) = self.language_attribute_cache.lock() {
            if let Some(cached) = cache.get(&key) {
                return cached.clone();
            }
        }
        let resolved = (|| {
            let comparisons = self.current_comparisons(session_id).ok()?;
            let repository_id = document_repository_id(document, &comparisons).ok()?;
            let repository = self.state().repositories_for_id(repository_id).ok()??;
            localreview_git::GitRepository::open(repository.worktree_path.as_str())
                .linguist_language(&document.file.path)
                .ok()?
        })();
        if let Ok(mut cache) = self.language_attribute_cache.lock() {
            cache.insert(key, resolved.clone());
            // Comparison IDs make entries immutable, but old review history
            // can be unbounded. A deterministic cap avoids permanent growth.
            if cache.len() > 8_192 {
                cache.clear();
            }
        }
        resolved
    }

    fn highlight_window(
        &self,
        document: &ReviewDiffDocument,
        rows: &[DiffRowView],
        settings: &ReviewSettings,
        language_attribute: Option<&str>,
        job: Option<&PresentationJobLease>,
    ) -> Result<HighlightWindow, DispatchError> {
        let _permit = self.presentation_work.acquire(job)?;
        let theme = if settings.theme == "light" {
            HighlightTheme::Light
        } else {
            HighlightTheme::Dark
        };
        let old = self.highlight.highlight(
            &HighlightRequest {
                path: Path::new(document.file.path.as_str()),
                source: &document.old.content,
                side: DiffSide::Old,
                language_attribute,
                theme: theme.clone(),
                force: false,
            },
            job.map(|job| &job.highlight),
        );
        if job.is_some_and(PresentationJobLease::is_cancelled) {
            return Err(DispatchError::Cancelled);
        }
        let new = self.highlight.highlight(
            &HighlightRequest {
                path: Path::new(document.file.path.as_str()),
                source: &document.new.content,
                side: DiffSide::New,
                language_attribute,
                theme,
                force: false,
            },
            job.map(|job| &job.highlight),
        );
        if job.is_some_and(PresentationJobLease::is_cancelled) {
            return Err(DispatchError::Cancelled);
        }
        let old_range = row_byte_range(rows, DiffSide::Old);
        let new_range = row_byte_range(rows, DiffSide::New);
        let old_tokens = old
            .tokens
            .into_iter()
            .filter(|token| token.side == DiffSide::Old)
            .filter(|token| token_overlaps(token.start_byte, token.end_byte, old_range))
            .map(|token| SyntaxTokenView {
                start_byte: token.start_byte,
                end_byte: token.end_byte,
                class: syntax_class_name(token.class).into(),
            })
            .collect();
        let new_tokens = new
            .tokens
            .into_iter()
            .filter(|token| token.side == DiffSide::New)
            .filter(|token| token_overlaps(token.start_byte, token.end_byte, new_range))
            .map(|token| SyntaxTokenView {
                start_byte: token.start_byte,
                end_byte: token.end_byte,
                class: syntax_class_name(token.class).into(),
            })
            .collect();
        let (status, reason) = match (&old.status, &new.status) {
            (HighlightStatus::Highlighted, HighlightStatus::Highlighted) => {
                ("highlighted".into(), None)
            }
            _ => (
                "plain_text".into(),
                Some(format!("old={:?}; new={:?}", old.status, new.status)),
            ),
        };
        Ok((old_tokens, new_tokens, status, reason))
    }

    fn full_file_window(
        &self,
        request: PresentationRequest,
        document: &ReviewDiffDocument,
        requested_view: FullFileView,
        ui_state: &ReviewUiState,
        execution: FullFileWindowExecution<'_>,
    ) -> Result<PresentationWindow, DispatchError> {
        let FullFileWindowExecution {
            settings,
            max_window_rows,
            job,
        } = execution;
        let view = if document.file.status == localreview_diff::ReviewFileStatus::Deleted {
            FullFileView::Old
        } else {
            requested_view
        };
        let canonical = self.cached_canonical_rows(document, "full", view, ui_state)?;
        let total = canonical.rows.len();
        let (start, end) =
            bounded_window(request.start_row, request.end_row, total, max_window_rows);
        let mut rows = canonical.rows[start..end].to_vec();
        self.mark_annotation_rows(document, &mut rows)?;
        let session = self
            .session_for_comparison(document.comparison_id)?
            .ok_or_else(|| {
                DispatchError::Invalid("file is not part of a retained review".into())
            })?;
        let language_attribute = self.highlight_language_attribute(document, session.id);
        let (old_tokens, new_tokens, highlight_status, highlight_reason) = self.highlight_window(
            document,
            &rows,
            settings,
            language_attribute.as_deref(),
            job,
        )?;
        if let Some(job) = job {
            self.ensure_presentation_job_current(job)?;
        }
        Ok(PresentationWindow {
            generation: request.generation,
            mode: "full".into(),
            file_id: request.file_id,
            start_row: u32::try_from(start).unwrap_or(u32::MAX),
            total_rows: u32::try_from(total).unwrap_or(u32::MAX),
            rows,
            hunks: canonical.hunks.clone(),
            omitted_blocks: canonical.omitted_blocks.clone(),
            old_tokens,
            new_tokens,
            highlight_status,
            highlight_reason,
            difftastic: None,
        })
    }

    fn difftastic_window(
        &self,
        request: PresentationRequest,
        document: &ReviewDiffDocument,
        ui_state: &ReviewUiState,
        settings: &ReviewSettings,
        execution: DifftasticWindowExecution<'_>,
    ) -> Result<PresentationWindow, DispatchError> {
        let DifftasticWindowExecution {
            resource_dir,
            max_window_rows,
            job,
        } = execution;
        if job.is_some_and(PresentationJobLease::is_cancelled) {
            return Err(DispatchError::Cancelled);
        }
        let fallback_side = request
            .full_file_side
            .as_deref()
            .map(parse_full_file_view)
            .transpose()?
            .unwrap_or(FullFileView::Both)
            .primary_side();
        let cache_key = structural_cache_key(document, settings);
        let presentation = {
            let mut cache = self
                .presentation_cache
                .lock()
                .map_err(|_| DispatchError::Internal)?;
            let cached = cache.structural.get(&cache_key).cloned();
            if cached.is_some() {
                cache
                    .structural_order
                    .retain(|candidate| candidate != &cache_key);
                cache.structural_order.push_back(cache_key.clone());
            }
            cached
        };
        let outcome = if let Some(presentation) = presentation {
            DifftasticOutcome::Structural(presentation)
        } else {
            let adapter = match DifftasticAdapter::from_location(
                SidecarLocation::PackagedResource {
                    resource_dir: resource_dir.to_path_buf(),
                },
                DifftasticPolicy::default(),
            ) {
                Ok(adapter) => adapter,
                Err(error) => {
                    return self.difftastic_fallback_window(
                        request,
                        document,
                        ui_state,
                        fallback_side,
                        format!("sidecar_unavailable: {error}"),
                        max_window_rows,
                    )
                }
            };
            // Version probes and structural renders both execute the packaged
            // process. They use a cancellation-aware bounded pool instead of
            // a one-at-a-time global mutex, so independent files can render
            // concurrently and stale queued jobs leave without starting.
            let _permit = self.presentation_work.acquire(job)?;
            let executable = adapter.executable().to_path_buf();
            let already_verified = self
                .presentation_cache
                .lock()
                .map_err(|_| DispatchError::Internal)?
                .verified_sidecars
                .contains(&executable);
            if !already_verified {
                if let Err(error) = adapter.verify_pinned_version() {
                    if job.is_some_and(PresentationJobLease::is_cancelled) {
                        return Err(DispatchError::Cancelled);
                    }
                    return self.difftastic_fallback_window(
                        request,
                        document,
                        ui_state,
                        fallback_side,
                        format!("sidecar_unavailable: {error}"),
                        max_window_rows,
                    );
                }
                self.presentation_cache
                    .lock()
                    .map_err(|_| DispatchError::Internal)?
                    .verified_sidecars
                    .insert(executable);
            }
            adapter.render(
                &DifftasticRequest {
                    old: DifftasticInput {
                        path: PathBuf::from(
                            document
                                .file
                                .old_path
                                .as_ref()
                                .unwrap_or(&document.file.path)
                                .as_str(),
                        ),
                        content: document.old.content.as_bytes().to_vec(),
                    },
                    new: DifftasticInput {
                        path: PathBuf::from(document.file.path.as_str()),
                        content: document.new.content.as_bytes().to_vec(),
                    },
                    display: DifftasticDisplay::SideBySide,
                    background: if settings.theme == "light" {
                        DifftasticBackground::Light
                    } else {
                        DifftasticBackground::Dark
                    },
                    width: 180,
                },
                job.map(|job| &job.difftastic),
            )
        };
        if job.is_some_and(PresentationJobLease::is_cancelled) {
            return Err(DispatchError::Cancelled);
        }
        match outcome {
            DifftasticOutcome::Structural(structural) => {
                if let Some(job) = job {
                    self.ensure_presentation_job_current(job)?;
                }
                let (start, end) = bounded_window(
                    request.start_row,
                    request.end_row,
                    structural_row_count(&structural),
                    max_window_rows,
                );
                let view = difftastic_view(&structural, start, end, None);
                let total_rows = structural_row_count(&structural);
                // Native sidecar output is cached only after it has passed the
                // exact-version verification and private-schema validation.
                let mut cache = self
                    .presentation_cache
                    .lock()
                    .map_err(|_| DispatchError::Internal)?;
                cache.structural.insert(cache_key.clone(), structural);
                cache
                    .structural_order
                    .retain(|candidate| candidate != &cache_key);
                cache.structural_order.push_back(cache_key);
                while cache.structural_order.len() > 6 {
                    if let Some(expired) = cache.structural_order.pop_front() {
                        cache.structural.remove(&expired);
                    }
                }
                Ok(PresentationWindow {
                    generation: request.generation,
                    mode: "difftastic".into(),
                    file_id: request.file_id,
                    start_row: u32::try_from(start).unwrap_or(u32::MAX),
                    total_rows: u32::try_from(total_rows).unwrap_or(u32::MAX),
                    rows: Vec::new(),
                    hunks: Vec::new(),
                    omitted_blocks: Vec::new(),
                    old_tokens: Vec::new(),
                    new_tokens: Vec::new(),
                    highlight_status: "disabled".into(),
                    highlight_reason: Some("Difftastic owns structural token spans".into()),
                    difftastic: Some(view),
                })
            }
            DifftasticOutcome::CanonicalFallback(fallback) => self.difftastic_fallback_window(
                request,
                document,
                ui_state,
                fallback_side,
                format!(
                    "{:?}: {}",
                    fallback.reason,
                    fallback.detail.unwrap_or_default()
                ),
                max_window_rows,
            ),
        }
    }

    fn difftastic_fallback_window(
        &self,
        request: PresentationRequest,
        document: &ReviewDiffDocument,
        ui_state: &ReviewUiState,
        full_file_side: DiffSide,
        detail: String,
        max_window_rows: usize,
    ) -> Result<PresentationWindow, DispatchError> {
        let canonical = self.cached_canonical_rows(
            document,
            "unified",
            if full_file_side == DiffSide::Old {
                FullFileView::Old
            } else {
                FullFileView::New
            },
            ui_state,
        )?;
        let all_rows = &canonical.rows;
        let (start, end) = bounded_window(
            request.start_row,
            request.end_row,
            all_rows.len(),
            max_window_rows,
        );
        let mut rows = all_rows[start..end].to_vec();
        self.mark_annotation_rows(document, &mut rows)?;
        Ok(PresentationWindow {
            generation: request.generation,
            // The frontend currently keys stale-response safety to the
            // requested mode. `difftastic` therefore remains the envelope,
            // while the fallback payload explicitly contains canonical rows.
            mode: "difftastic".into(),
            file_id: request.file_id,
            start_row: u32::try_from(start).unwrap_or(u32::MAX),
            total_rows: u32::try_from(all_rows.len()).unwrap_or(u32::MAX),
            rows,
            hunks: canonical.hunks.clone(),
            omitted_blocks: Vec::new(),
            old_tokens: Vec::new(),
            new_tokens: Vec::new(),
            highlight_status: "disabled".into(),
            highlight_reason: Some("Difftastic unavailable; canonical rows returned".into()),
            difftastic: Some(DifftasticPresentationView {
                status: difftastic_file_status(document).into(),
                display: "side_by_side".into(),
                start_row: u32::try_from(start).unwrap_or(u32::MAX),
                total_rows: u32::try_from(all_rows.len()).unwrap_or(u32::MAX),
                chunks: Vec::new(),
                alignment: Vec::new(),
                fallback: Some(DifftasticFallbackView {
                    reason: "canonical_fallback".into(),
                    detail: Some(detail.chars().take(1_000).collect()),
                }),
            }),
        })
    }

    fn validate_annotation_draft(
        &self,
        draft: &AnnotationDraft,
        workspace_id: WorkspaceId,
    ) -> Result<(), DispatchError> {
        const MAX_DRAFT_ID_BYTES: usize = 160;
        const MAX_DRAFT_BODY_BYTES: usize = 1024 * 1024;
        if draft.workspace_id != workspace_id.to_string()
            || draft.id.trim().is_empty()
            || draft.id.len() > MAX_DRAFT_ID_BYTES
            || draft.id.contains(['\0', '\n', '\r'])
            || draft.body.len() > MAX_DRAFT_BODY_BYTES
            || draft.body.contains('\0')
        {
            return Err(DispatchError::Invalid("annotation draft is invalid".into()));
        }
        // Parse before loading anything so malformed frontend strings never
        // become an accidental cross-workspace lookup.
        let file_id = parse_review_file_id(&draft.file_id)
            .ok_or_else(|| DispatchError::Invalid("draft fileId is invalid".into()))?;
        let repository_id = RepositoryId(
            Uuid::parse_str(&draft.repository_id)
                .map_err(|_| DispatchError::Invalid("draft repositoryId is invalid".into()))?,
        );
        parse_annotation_kind(&draft.kind)?;
        let side = parse_side(&draft.side)?;
        if draft.start_line == 0 || draft.end_line < draft.start_line {
            return Err(DispatchError::Invalid(
                "annotation draft range is invalid".into(),
            ));
        }
        chrono::DateTime::parse_from_rfc3339(&draft.updated_at)
            .map_err(|_| DispatchError::Invalid("draft updatedAt is invalid".into()))?;

        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        self.ensure_remote_file_materialized(file_id)?;
        let document = self
            .state()
            .review_file_payload::<PersistedReviewDocument>(file_id)?
            .ok_or_else(|| DispatchError::NotFound(draft.file_id.clone()))?
            .document;
        let comparisons = self.current_comparisons(session.id)?;
        if document_repository_id(&document, &comparisons)? != repository_id {
            return Err(DispatchError::Invalid(
                "draft repositoryId does not own the captured file".into(),
            ));
        }
        let source = match side {
            DiffSide::Old => &document.old.content,
            DiffSide::New => &document.new.content,
        };
        let line_count = u32::try_from(source.lines().count()).unwrap_or(u32::MAX);
        if draft.end_line > line_count {
            return Err(DispatchError::Invalid(
                "draft range is outside the captured source".into(),
            ));
        }
        Ok(())
    }

    fn active_annotation(
        &self,
        workspace_id: WorkspaceId,
        annotation_id: AnnotationId,
    ) -> Result<Annotation, DispatchError> {
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let set = self
            .state()
            .active_annotation_set(session.id)?
            .ok_or_else(|| DispatchError::Invalid("active review has no annotation set".into()))?;
        self.state()
            .annotations(set.id)?
            .into_iter()
            .find(|annotation| annotation.id == annotation_id)
            .ok_or_else(|| DispatchError::NotFound(annotation_id.to_string()))
    }

    fn provider_permalink(
        &self,
        workspace_id: WorkspaceId,
        document: &ReviewDiffDocument,
        side: DiffSide,
        line: Option<u32>,
    ) -> Result<String, DispatchError> {
        let workspace = self
            .state()
            .workspace(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let WorkspaceSource::PullRequest {
            owner, repository, ..
        } = workspace.source
        else {
            return Err(DispatchError::Invalid(
                "provider permalinks are available only for GitHub PR review workspaces".into(),
            ));
        };
        let session = self
            .service
            .active_review_session(workspace_id)?
            .ok_or_else(|| DispatchError::NotFound(workspace_id.to_string()))?;
        let comparisons = self.current_comparisons(session.id)?;
        let comparison = comparisons
            .values()
            .find(|comparison| comparison.id == document.comparison_id)
            .ok_or_else(|| DispatchError::Invalid("comparison is unavailable".into()))?;
        let revision = match side {
            DiffSide::Old => comparison.merge_base_sha.as_str(),
            DiffSide::New => comparison
                .head_sha
                .as_ref()
                .map_or_else(|| comparison.merge_base_sha.as_str(), |sha| sha.as_str()),
        };
        let path = if side == DiffSide::Old {
            document
                .file
                .old_path
                .as_ref()
                .unwrap_or(&document.file.path)
        } else {
            &document.file.path
        };
        let suffix = line.map_or_else(String::new, |value| format!("#L{value}"));
        Ok(format!(
            "https://github.com/{owner}/{repository}/blob/{revision}/{}{}",
            github_path(path.as_str()),
            suffix
        ))
    }
}

fn annotation_draft_key(session_id: ReviewSessionId) -> String {
    format!("{ANNOTATION_DRAFT_KEY_PREFIX}session.{session_id}")
}

fn legacy_annotation_draft_key(workspace_id: WorkspaceId) -> String {
    format!("{ANNOTATION_DRAFT_KEY_PREFIX}{workspace_id}")
}

fn structural_cache_key(document: &ReviewDiffDocument, settings: &ReviewSettings) -> String {
    format!(
        "{}:{}:{}:{}",
        document.file.id, document.old.fingerprint, document.new.fingerprint, settings.theme
    )
}

fn nearest_canonical_row(rows: &[DiffRowView], side: DiffSide, line: u32) -> usize {
    let candidate_line = |row: &DiffRowView| match side {
        DiffSide::Old => row.old_line,
        DiffSide::New => row.new_line,
    };
    rows.iter()
        .enumerate()
        .find_map(|(index, row)| (candidate_line(row) == Some(line)).then_some(index))
        .or_else(|| {
            rows.iter()
                .enumerate()
                .filter_map(|(index, row)| candidate_line(row).map(|candidate| (index, candidate)))
                // `min_by_key` preserves the earlier row on an equal distance,
                // giving deterministic nearest-line behavior around a gap.
                .min_by_key(|(index, candidate)| (candidate.abs_diff(line), *index))
                .map(|(index, _)| index)
        })
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
struct FullFileOmittedBlock {
    id: String,
    side: DiffSide,
    rows: Vec<FullFileRow>,
}

impl FullFileOmittedBlock {
    fn start_line(&self) -> u32 {
        self.rows.first().map_or(0, |row| row.line_number)
    }

    fn end_line(&self) -> u32 {
        self.rows.last().map_or(0, |row| row.line_number)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FullFileView {
    Old,
    New,
    Both,
}

impl FullFileView {
    fn name(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
            Self::Both => "both",
        }
    }

    fn primary_side(self) -> DiffSide {
        match self {
            Self::Old => DiffSide::Old,
            Self::New | Self::Both => DiffSide::New,
        }
    }
}

#[derive(Clone, Debug)]
enum FullFileProjectedRow {
    Line(FullFileRow),
    OmittedGate(FullFileOmittedBlock, bool),
}

fn full_file_omitted_blocks(
    document: &ReviewDiffDocument,
    view: FullFileView,
) -> Vec<FullFileOmittedBlock> {
    let rows = match view {
        FullFileView::Old => full_file_base_rows(document),
        FullFileView::New | FullFileView::Both => full_file_current_rows(document),
    };
    let is_omitted = |row: &FullFileRow| match view {
        FullFileView::Old => row.side == DiffSide::New,
        FullFileView::New => row.side == DiffSide::Old,
        FullFileView::Both => row.changed,
    };
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        if !is_omitted(&rows[index]) {
            index += 1;
            continue;
        }
        let omitted_side = rows[index].side;
        let start = index;
        while index < rows.len() && is_omitted(&rows[index]) && rows[index].side == omitted_side {
            index += 1;
        }
        let block_rows = rows[start..index].to_vec();
        let start_line = block_rows.first().map_or(0, |row| row.line_number);
        let end_line = block_rows.last().map_or(0, |row| row.line_number);
        let id = if omitted_side == DiffSide::Old {
            // Preserve the pre-symmetric deletion ID so expansion choices
            // survive an application upgrade.
            format!(
                "{}:{}:{start_line}-{end_line}",
                document.comparison_id, document.file.id
            )
        } else {
            format!(
                "{}:{}:new:{start_line}-{end_line}",
                document.comparison_id, document.file.id
            )
        };
        blocks.push(FullFileOmittedBlock {
            id,
            side: omitted_side,
            rows: block_rows,
        });
    }
    blocks
}

fn valid_full_file_deletion_block_ids(documents: &[PersistedReviewDocument]) -> BTreeSet<String> {
    documents
        .iter()
        .flat_map(|document| {
            [FullFileView::Old, FullFileView::New, FullFileView::Both]
                .into_iter()
                .flat_map(|view| full_file_omitted_blocks(&document.document, view))
        })
        .map(|block| block.id)
        .collect()
}

fn full_file_projection(
    document: &ReviewDiffDocument,
    view: FullFileView,
    expanded_blocks: &BTreeSet<String>,
    collapsed_addition_blocks: &BTreeSet<String>,
) -> (Vec<FullFileProjectedRow>, Vec<FullFileOmittedBlock>) {
    let rows = match view {
        FullFileView::Old => full_file_base_rows(document),
        FullFileView::New | FullFileView::Both => full_file_current_rows(document),
    };
    let blocks = full_file_omitted_blocks(document, view);
    let mut blocks_by_start = blocks
        .iter()
        .cloned()
        .map(|block| ((side_name(block.side), block.start_line()), block))
        .collect::<BTreeMap<_, _>>();
    let mut projection = Vec::with_capacity(rows.len());
    let mut index = 0;
    while index < rows.len() {
        let row = &rows[index];
        let belongs_to_complete_source = match view {
            FullFileView::Old => row.side == DiffSide::Old,
            FullFileView::New => row.side == DiffSide::New,
            FullFileView::Both => !row.changed,
        };
        if belongs_to_complete_source {
            projection.push(FullFileProjectedRow::Line(row.clone()));
            index += 1;
            continue;
        }
        let Some(block) = blocks_by_start.remove(&(side_name(row.side), row.line_number)) else {
            projection.push(FullFileProjectedRow::Line(row.clone()));
            index += 1;
            continue;
        };
        index = index.saturating_add(block.rows.len());
        let expanded = if view == FullFileView::Both && block.side == DiffSide::New {
            !collapsed_addition_blocks.contains(&block.id)
        } else {
            expanded_blocks.contains(&block.id)
        };
        projection.push(FullFileProjectedRow::OmittedGate(block.clone(), expanded));
        if expanded {
            projection.extend(block.rows.iter().cloned().map(FullFileProjectedRow::Line));
        }
    }
    (projection, blocks)
}

fn nearest_full_file_row(rows: &[FullFileProjectedRow], side: DiffSide, line: u32) -> usize {
    // When a deletion block is expanded, exact source locations must land on
    // the selectable Base row rather than on the preceding range gate.
    exact_full_file_row(rows, side, line)
        .or_else(|| {
            rows.iter()
                .enumerate()
                .filter_map(|(index, row)| match row {
                    FullFileProjectedRow::Line(row) if row.side == side => {
                        Some((index, row.line_number.abs_diff(line)))
                    }
                    FullFileProjectedRow::OmittedGate(block, _) if side == block.side => Some((
                        index,
                        block
                            .start_line()
                            .abs_diff(line)
                            .min(block.end_line().abs_diff(line)),
                    )),
                    _ => None,
                })
                .min_by_key(|(index, distance)| (*distance, *index))
                .map(|(index, _)| index)
        })
        .unwrap_or(0)
}

fn exact_full_file_row(rows: &[FullFileProjectedRow], side: DiffSide, line: u32) -> Option<usize> {
    rows.iter()
        .enumerate()
        .find_map(|(index, row)| match row {
            FullFileProjectedRow::Line(row) => {
                (row.side == side && row.line_number == line).then_some(index)
            }
            FullFileProjectedRow::OmittedGate(_, _) => None,
        })
        .or_else(|| {
            rows.iter().enumerate().find_map(|(index, row)| match row {
                FullFileProjectedRow::OmittedGate(block, _) => {
                    (side == block.side && line >= block.start_line() && line <= block.end_line())
                        .then_some(index)
                }
                FullFileProjectedRow::Line(_) => None,
            })
        })
}

fn aligned_source_line(
    document: &ReviewDiffDocument,
    source_side: DiffSide,
    source_line: u32,
    target_side: DiffSide,
) -> u32 {
    if source_side == target_side {
        return source_line;
    }
    let source_count = match source_side {
        DiffSide::Old => document.old.line_count,
        DiffSide::New => document.new.line_count,
    };
    let target_count = match target_side {
        DiffSide::Old => document.old.line_count,
        DiffSide::New => document.new.line_count,
    };
    if target_count == 0 {
        return 0;
    }
    let mut pairs = document
        .hunks
        .iter()
        .flat_map(|hunk| hunk.split_rows.iter())
        .filter_map(|row| {
            let (Some(old), Some(new)) = (&row.old, &row.new) else {
                return None;
            };
            Some(match source_side {
                DiffSide::Old => (old.line_number, new.line_number),
                DiffSide::New => (new.line_number, old.line_number),
            })
        })
        .collect::<Vec<_>>();
    pairs.push((0, 0));
    pairs.push((
        source_count.saturating_add(1),
        target_count.saturating_add(1),
    ));
    pairs.sort_unstable();
    pairs.dedup();
    if let Some((_, target)) = pairs.iter().find(|(source, _)| *source == source_line) {
        return (*target).clamp(1, target_count);
    }
    let before = pairs
        .iter()
        .rev()
        .find(|(source, _)| *source < source_line)
        .copied()
        .unwrap_or((0, 0));
    let after = pairs
        .iter()
        .find(|(source, _)| *source > source_line)
        .copied()
        .unwrap_or((
            source_count.saturating_add(1),
            target_count.saturating_add(1),
        ));
    let mapped = before
        .1
        .saturating_add(source_line.saturating_sub(before.0))
        .min(after.1);
    mapped.clamp(1, target_count)
}

fn full_file_row_view(
    row: &FullFileProjectedRow,
    old_offsets: &[u32],
    new_offsets: &[u32],
) -> DiffRowView {
    match row {
        FullFileProjectedRow::Line(row) => DiffRowView {
            id: format!("full:{}:{}", side_name(row.side), row.line_number),
            kind: if row.changed {
                if row.side == DiffSide::Old {
                    "deletion"
                } else {
                    "addition"
                }
            } else {
                "context"
            }
            .into(),
            hunk_id: None,
            old_line: (row.side == DiffSide::Old).then_some(row.line_number),
            new_line: (row.side == DiffSide::New).then_some(row.line_number),
            old_text: (row.side == DiffSide::Old).then_some(row.text.clone()),
            new_text: (row.side == DiffSide::New).then_some(row.text.clone()),
            text: Some(row.text.clone()),
            hunk: None,
            has_annotation: false,
            old_source_start_byte: (row.side == DiffSide::Old)
                .then(|| source_offset(old_offsets, row.line_number)),
            new_source_start_byte: (row.side == DiffSide::New)
                .then(|| source_offset(new_offsets, row.line_number)),
            omitted_block_id: None,
            omitted_count: None,
            omitted_end_line: None,
            omitted_side: None,
            omitted_expanded: None,
        },
        FullFileProjectedRow::OmittedGate(block, expanded) => {
            let count = u32::try_from(block.rows.len()).unwrap_or(u32::MAX);
            let omission = if block.side == DiffSide::Old {
                "deleted"
            } else {
                "added"
            };
            DiffRowView {
                id: format!("full:{omission}-gate:{}", block.id),
                kind: if block.side == DiffSide::Old {
                    "deletion_gate"
                } else {
                    "addition_gate"
                }
                .into(),
                hunk_id: None,
                old_line: (block.side == DiffSide::Old).then(|| block.start_line()),
                new_line: (block.side == DiffSide::New).then(|| block.start_line()),
                old_text: None,
                new_text: None,
                text: Some(format!(
                    "{count} {omission} {}",
                    if count == 1 { "line" } else { "lines" }
                )),
                hunk: None,
                has_annotation: false,
                old_source_start_byte: None,
                new_source_start_byte: None,
                omitted_block_id: Some(block.id.clone()),
                omitted_count: Some(count),
                omitted_end_line: Some(block.end_line()),
                omitted_side: Some(side_name(block.side).into()),
                omitted_expanded: Some(*expanded),
            }
        }
    }
}

fn full_file_omitted_block_views(
    blocks: &[FullFileOmittedBlock],
    projection: &[FullFileProjectedRow],
) -> Vec<FullFileOmittedBlockView> {
    blocks
        .iter()
        .map(|block| FullFileOmittedBlockView {
            id: block.id.clone(),
            side: side_name(block.side).into(),
            start_line: block.start_line(),
            end_line: block.end_line(),
            count: u32::try_from(block.rows.len()).unwrap_or(u32::MAX),
            expanded: projection.iter().any(|row| {
                matches!(
                    row,
                    FullFileProjectedRow::OmittedGate(candidate, true)
                        if candidate.id == block.id
                )
            }),
            row_index: u32::try_from(nearest_full_file_row(
                projection,
                block.side,
                block.start_line(),
            ))
            .unwrap_or(u32::MAX),
        })
        .collect()
}

fn nearest_difftastic_row(
    presentation: &NativeDifftasticPresentation,
    side: DiffSide,
    line: u32,
) -> usize {
    let candidate_line = |alignment: &localreview_difftastic::DifftasticAlignment| match side {
        DiffSide::Old => alignment.old_line_number,
        DiffSide::New => alignment.new_line_number,
    };
    presentation
        .alignment
        .iter()
        .enumerate()
        .find_map(|(index, alignment)| (candidate_line(alignment) == Some(line)).then_some(index))
        .or_else(|| {
            presentation
                .alignment
                .iter()
                .enumerate()
                .filter_map(|(index, alignment)| {
                    candidate_line(alignment).map(|candidate| (index, candidate))
                })
                .min_by_key(|(index, candidate)| (candidate.abs_diff(line), *index))
                .map(|(index, _)| index)
        })
        .unwrap_or(0)
}

fn validate_diff_mode(value: &str) -> Result<&str, DispatchError> {
    match value {
        "unified" | "split" | "full" | "difftastic" => Ok(value),
        _ => Err(DispatchError::Invalid("unsupported diff mode".into())),
    }
}

fn bounded_window(start: u32, end: u32, total: usize, max_rows: usize) -> (usize, usize) {
    let start = usize::try_from(start).unwrap_or(usize::MAX).min(total);
    let requested_end = usize::try_from(end).unwrap_or(usize::MAX);
    let end = requested_end
        .max(start)
        .min(total)
        .min(start.saturating_add(max_rows));
    (start, end)
}

fn workspace_ui_state_view(state: &ReviewUiState) -> WorkspaceUiStateView {
    WorkspaceUiStateView {
        active_file_id: state.active_file_id.clone(),
        mode: state.mode.clone().unwrap_or_else(|| "unified".into()),
        full_file_side: state
            .full_file_side
            .clone()
            .unwrap_or_else(|| "both".into()),
        nearest_source_line: state.nearest_source_line,
        nearest_source_side: state.nearest_source_side.clone(),
        scroll_top: state.scroll_top.unwrap_or(0.0),
        split_ratio: state.split_ratio.unwrap_or(0.5),
        right_tab: state.right_tab.clone().unwrap_or_else(|| "files".into()),
        selected_annotation_ids: state
            .selected_annotation_ids
            .as_ref()
            .map(|values| values.iter().cloned().collect()),
        expanded_full_file_deletion_blocks: state
            .expanded_full_file_deletion_blocks
            .iter()
            .cloned()
            .collect(),
        collapsed_full_file_addition_blocks: state
            .collapsed_full_file_addition_blocks
            .iter()
            .cloned()
            .collect(),
    }
}

fn build_canonical_presentation(
    document: &ReviewDiffDocument,
    mode: &str,
    full_file_view: FullFileView,
    state: &ReviewUiState,
) -> Result<CachedCanonicalPresentation, DispatchError> {
    match mode {
        "unified" | "split" => {
            let old_offsets = source_line_offsets(&document.old.content);
            let new_offsets = source_line_offsets(&document.new.content);
            let old_lines = document.old.content.lines().collect::<Vec<_>>();
            let new_lines = document.new.content.lines().collect::<Vec<_>>();
            let mut rows = Vec::new();
            let mut hunks = Vec::new();
            for hunk in &document.hunks {
                let context = state
                    .hunk_context_lines
                    .get(&hunk.id.0)
                    .copied()
                    .unwrap_or(3)
                    .clamp(3, 1_200);
                let header_index = u32::try_from(rows.len()).unwrap_or(u32::MAX);
                let header = format_hunk_header(hunk);
                let (first_old, last_old, first_new, last_new) = hunk_line_bounds(hunk);
                hunks.push(HunkLocationView {
                    id: hunk.id.0.clone(),
                    row_index: header_index,
                    old_line: first_old
                        .or_else(|| (hunk.header.old_count > 0).then_some(hunk.header.old_start)),
                    new_line: first_new
                        .or_else(|| (hunk.header.new_count > 0).then_some(hunk.header.new_start)),
                    header: header.clone(),
                    collapsed_context_lines: Some(context),
                });
                rows.push(DiffRowView {
                    id: format!("header:{}", hunk.id.0),
                    kind: "header".into(),
                    hunk_id: Some(hunk.id.0.clone()),
                    old_line: None,
                    new_line: None,
                    old_text: None,
                    new_text: None,
                    text: None,
                    hunk: Some(header),
                    has_annotation: false,
                    old_source_start_byte: None,
                    new_source_start_byte: None,
                    omitted_block_id: None,
                    omitted_count: None,
                    omitted_end_line: None,
                    omitted_side: None,
                    omitted_expanded: None,
                });
                let extra = context.saturating_sub(3);
                rows.extend(expanded_context_rows(
                    &hunk.id.0,
                    "before",
                    first_old,
                    first_new,
                    extra,
                    &old_lines,
                    &new_lines,
                    &old_offsets,
                    &new_offsets,
                    true,
                ));
                match mode {
                    "unified" => rows.extend(hunk.unified_rows.iter().map(|row| {
                        diff_row_from_cells(
                            row.id.clone(),
                            row_kind(row.kind),
                            &hunk.id.0,
                            row.old.as_ref(),
                            row.new.as_ref(),
                            &old_offsets,
                            &new_offsets,
                        )
                    })),
                    "split" => rows.extend(hunk.split_rows.iter().map(|row| {
                        let kind = match (&row.old, &row.new) {
                            (None, Some(_)) => "addition",
                            (Some(_), None) => "deletion",
                            _ => "context",
                        };
                        diff_row_from_cells(
                            row.id.clone(),
                            kind,
                            &hunk.id.0,
                            row.old.as_ref(),
                            row.new.as_ref(),
                            &old_offsets,
                            &new_offsets,
                        )
                    })),
                    _ => unreachable!(),
                }
                rows.extend(expanded_context_rows(
                    &hunk.id.0,
                    "after",
                    last_old,
                    last_new,
                    extra,
                    &old_lines,
                    &new_lines,
                    &old_offsets,
                    &new_offsets,
                    false,
                ));
            }
            Ok(CachedCanonicalPresentation {
                rows,
                hunks,
                omitted_blocks: Vec::new(),
            })
        }
        "full" => {
            let view = if document.file.status == localreview_diff::ReviewFileStatus::Deleted {
                FullFileView::Old
            } else {
                full_file_view
            };
            let old_offsets = source_line_offsets(&document.old.content);
            let new_offsets = source_line_offsets(&document.new.content);
            let (projection, blocks) = full_file_projection(
                document,
                view,
                &state.expanded_full_file_deletion_blocks,
                &state.collapsed_full_file_addition_blocks,
            );
            let rows = projection
                .iter()
                .map(|row| full_file_row_view(row, &old_offsets, &new_offsets))
                .collect::<Vec<_>>();
            let hunks = full_file_hunk_locations(document, view.primary_side(), &projection);
            let omitted_blocks = full_file_omitted_block_views(&blocks, &projection);
            Ok(CachedCanonicalPresentation {
                rows,
                hunks,
                omitted_blocks,
            })
        }
        _ => Err(DispatchError::Invalid(
            "unsupported canonical diff mode".into(),
        )),
    }
}

fn hunk_line_bounds(
    hunk: &localreview_diff::ReviewHunk,
) -> (Option<u32>, Option<u32>, Option<u32>, Option<u32>) {
    let mut first_old = None;
    let mut last_old = None;
    let mut first_new = None;
    let mut last_new = None;
    for row in &hunk.unified_rows {
        if let Some(old) = &row.old {
            first_old.get_or_insert(old.line_number);
            last_old = Some(old.line_number);
        }
        if let Some(new) = &row.new {
            first_new.get_or_insert(new.line_number);
            last_new = Some(new.line_number);
        }
    }
    (first_old, last_old, first_new, last_new)
}

#[allow(clippy::too_many_arguments)]
fn expanded_context_rows(
    hunk_id: &str,
    phase: &str,
    old_boundary: Option<u32>,
    new_boundary: Option<u32>,
    extra: u32,
    old_lines: &[&str],
    new_lines: &[&str],
    old_offsets: &[u32],
    new_offsets: &[u32],
    before: bool,
) -> Vec<DiffRowView> {
    if extra == 0 {
        return Vec::new();
    }
    let old_boundary = old_boundary.unwrap_or(1);
    let new_boundary = new_boundary.unwrap_or(1);
    let mut result = Vec::new();
    for offset in 1..=extra {
        let old_line = if before {
            old_boundary.checked_sub(offset)
        } else {
            old_boundary.checked_add(offset)
        };
        let new_line = if before {
            new_boundary.checked_sub(offset)
        } else {
            new_boundary.checked_add(offset)
        };
        let old_text = old_line
            .and_then(|line| old_lines.get(usize::try_from(line.saturating_sub(1)).ok()?))
            .map(ToString::to_string);
        let new_text = new_line
            .and_then(|line| new_lines.get(usize::try_from(line.saturating_sub(1)).ok()?))
            .map(ToString::to_string);
        if old_text.is_none() && new_text.is_none() {
            continue;
        }
        result.push(DiffRowView {
            id: format!(
                "expanded:{hunk_id}:{phase}:{}:{}",
                old_line.unwrap_or_default(),
                new_line.unwrap_or_default()
            ),
            kind: "context".into(),
            hunk_id: Some(hunk_id.into()),
            old_line,
            new_line,
            old_text,
            new_text,
            text: None,
            hunk: None,
            has_annotation: false,
            old_source_start_byte: old_line.map(|line| source_offset(old_offsets, line)),
            new_source_start_byte: new_line.map(|line| source_offset(new_offsets, line)),
            omitted_block_id: None,
            omitted_count: None,
            omitted_end_line: None,
            omitted_side: None,
            omitted_expanded: None,
        });
    }
    if before {
        result.reverse();
    }
    result
}

fn diff_row_from_cells(
    id: String,
    kind: &str,
    hunk_id: &str,
    old: Option<&localreview_diff::DiffCell>,
    new: Option<&localreview_diff::DiffCell>,
    old_offsets: &[u32],
    new_offsets: &[u32],
) -> DiffRowView {
    DiffRowView {
        id,
        kind: kind.into(),
        hunk_id: Some(hunk_id.into()),
        old_line: old.map(|cell| cell.line_number),
        new_line: new.map(|cell| cell.line_number),
        old_text: old.map(|cell| cell.text.clone()),
        new_text: new.map(|cell| cell.text.clone()),
        text: None,
        hunk: None,
        has_annotation: false,
        old_source_start_byte: old.map(|cell| source_offset(old_offsets, cell.line_number)),
        new_source_start_byte: new.map(|cell| source_offset(new_offsets, cell.line_number)),
        omitted_block_id: None,
        omitted_count: None,
        omitted_end_line: None,
        omitted_side: None,
        omitted_expanded: None,
    }
}

fn source_line_offsets(source: &str) -> Vec<u32> {
    let mut offsets = Vec::new();
    offsets.push(0);
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' && index.saturating_add(1) < source.len() {
            offsets.push(u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX));
        }
    }
    offsets
}

fn source_offset(offsets: &[u32], line: u32) -> u32 {
    line.checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| offsets.get(index).copied())
        .unwrap_or_default()
}

fn format_hunk_header(hunk: &localreview_diff::ReviewHunk) -> String {
    format!(
        "@@ -{},{} +{},{} @@{}",
        hunk.header.old_start,
        hunk.header.old_count,
        hunk.header.new_start,
        hunk.header.new_count,
        hunk.header
            .context
            .as_deref()
            .map_or_else(String::new, |context| format!(" {context}"))
    )
}

fn full_file_hunk_locations(
    document: &ReviewDiffDocument,
    side: DiffSide,
    rows: &[FullFileProjectedRow],
) -> Vec<HunkLocationView> {
    document
        .hunks
        .iter()
        .map(|hunk| {
            let first_old_change = hunk
                .unified_rows
                .iter()
                .filter(|row| row.kind != DiffLineKind::Context)
                .find_map(|row| row.old.as_ref().map(|cell| cell.line_number));
            let first_new_change = hunk
                .unified_rows
                .iter()
                .filter(|row| row.kind != DiffLineKind::Context)
                .find_map(|row| row.new.as_ref().map(|cell| cell.line_number));
            let displayed_change = hunk
                .unified_rows
                .iter()
                .filter(|row| row.kind != DiffLineKind::Context)
                .find_map(|row| match side {
                    DiffSide::Old => row
                        .old
                        .as_ref()
                        .map(|cell| (DiffSide::Old, cell.line_number))
                        .or_else(|| {
                            row.new
                                .as_ref()
                                .map(|cell| (DiffSide::New, cell.line_number))
                        }),
                    DiffSide::New => row
                        .old
                        .as_ref()
                        .map(|cell| (DiffSide::Old, cell.line_number))
                        .or_else(|| {
                            row.new
                                .as_ref()
                                .map(|cell| (DiffSide::New, cell.line_number))
                        }),
                });
            let row_index = displayed_change
                .map(|(change_side, line)| nearest_full_file_row(rows, change_side, line))
                .unwrap_or_else(|| {
                    let fallback_line = match side {
                        DiffSide::Old => hunk.header.old_start.max(1),
                        DiffSide::New => hunk.header.new_start.max(1),
                    };
                    nearest_full_file_row(rows, side, fallback_line)
                });
            HunkLocationView {
                id: hunk.id.0.clone(),
                row_index: u32::try_from(row_index).unwrap_or(u32::MAX),
                old_line: first_old_change
                    .or_else(|| (hunk.header.old_count > 0).then_some(hunk.header.old_start)),
                new_line: first_new_change
                    .or_else(|| (hunk.header.new_count > 0).then_some(hunk.header.new_start)),
                header: format_hunk_header(hunk),
                collapsed_context_lines: None,
            }
        })
        .collect()
}

fn annotation_overlaps(
    annotations: &[Annotation],
    document: &ReviewDiffDocument,
    side: DiffSide,
    start_line: u32,
    end_line: u32,
) -> bool {
    annotations.iter().any(|annotation| {
        !is_soft_deleted(annotation)
            && annotation.anchor.as_ref().is_some_and(|anchor| {
                anchor.comparison_id == document.comparison_id
                    && anchor.file_path == document.file.path
                    && anchor.side == Some(side)
                    && anchor.start_line.unwrap_or_default() <= end_line
                    && anchor.end_line.unwrap_or_default() >= start_line
            })
    })
}

fn row_byte_range(rows: &[DiffRowView], side: DiffSide) -> Option<(u32, u32)> {
    let mut start = u32::MAX;
    let mut end = 0_u32;
    for row in rows {
        let (offset, text) = match side {
            DiffSide::Old => (
                row.old_source_start_byte,
                row.old_text.as_deref().or(row.text.as_deref()),
            ),
            DiffSide::New => (
                row.new_source_start_byte,
                row.new_text.as_deref().or(row.text.as_deref()),
            ),
        };
        if let (Some(offset), Some(text)) = (offset, text) {
            start = start.min(offset);
            end = end.max(offset.saturating_add(u32::try_from(text.len()).unwrap_or(u32::MAX)));
        }
    }
    (start != u32::MAX).then_some((start, end))
}

fn token_overlaps(start: u32, end: u32, range: Option<(u32, u32)>) -> bool {
    range.is_some_and(|(range_start, range_end)| end > range_start && start < range_end)
}

fn syntax_class_name(class: localreview_highlight::SyntaxClass) -> &'static str {
    match class {
        localreview_highlight::SyntaxClass::Attribute => "attribute",
        localreview_highlight::SyntaxClass::Boolean => "boolean",
        localreview_highlight::SyntaxClass::Comment => "comment",
        localreview_highlight::SyntaxClass::Constant => "constant",
        localreview_highlight::SyntaxClass::Constructor => "constructor",
        localreview_highlight::SyntaxClass::Embedded => "embedded",
        localreview_highlight::SyntaxClass::Escape => "escape",
        localreview_highlight::SyntaxClass::Function => "function",
        localreview_highlight::SyntaxClass::Keyword => "keyword",
        localreview_highlight::SyntaxClass::Markup => "markup",
        localreview_highlight::SyntaxClass::Module => "module",
        localreview_highlight::SyntaxClass::Number => "number",
        localreview_highlight::SyntaxClass::Operator => "operator",
        localreview_highlight::SyntaxClass::Property => "property",
        localreview_highlight::SyntaxClass::Punctuation => "punctuation",
        localreview_highlight::SyntaxClass::String => "string",
        localreview_highlight::SyntaxClass::Tag => "tag",
        localreview_highlight::SyntaxClass::Type => "type",
        localreview_highlight::SyntaxClass::Variable => "variable",
    }
}

fn outline_kind_name(kind: localreview_highlight::OutlineKind) -> &'static str {
    match kind {
        localreview_highlight::OutlineKind::Function => "function",
        localreview_highlight::OutlineKind::Method => "method",
        localreview_highlight::OutlineKind::Class => "class",
        localreview_highlight::OutlineKind::Struct => "struct",
        localreview_highlight::OutlineKind::Enum => "enum",
        localreview_highlight::OutlineKind::Interface => "interface",
        localreview_highlight::OutlineKind::Module => "module",
        localreview_highlight::OutlineKind::Heading => "heading",
        localreview_highlight::OutlineKind::Property => "property",
        localreview_highlight::OutlineKind::Unknown => "unknown",
    }
}

fn structural_row_count(presentation: &NativeDifftasticPresentation) -> usize {
    presentation
        .chunks
        .iter()
        .map(|chunk| chunk.rows.len())
        .sum()
}

fn difftastic_view(
    presentation: &NativeDifftasticPresentation,
    start: usize,
    end: usize,
    fallback: Option<DifftasticFallbackView>,
) -> DifftasticPresentationView {
    let flattened = presentation
        .chunks
        .iter()
        .flat_map(|chunk| chunk.rows.iter())
        .skip(start)
        .take(end.saturating_sub(start))
        .map(difftastic_row_view)
        .collect::<Vec<_>>();
    DifftasticPresentationView {
        status: match presentation.status {
            localreview_difftastic::DifftasticFileStatus::Unchanged => "unchanged",
            localreview_difftastic::DifftasticFileStatus::Changed => "changed",
            localreview_difftastic::DifftasticFileStatus::Created => "created",
            localreview_difftastic::DifftasticFileStatus::Deleted => "deleted",
        }
        .into(),
        display: match presentation.display {
            DifftasticDisplay::Inline => "inline",
            DifftasticDisplay::SideBySide => "side_by_side",
        }
        .into(),
        start_row: u32::try_from(start).unwrap_or(u32::MAX),
        total_rows: u32::try_from(structural_row_count(presentation)).unwrap_or(u32::MAX),
        chunks: flattened
            .is_empty()
            .then(Vec::new)
            .unwrap_or_else(|| vec![DifftasticChunkView { rows: flattened }]),
        alignment: presentation
            .alignment
            .iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|alignment| DifftasticAlignmentView {
                old_line: alignment.old_line_number,
                new_line: alignment.new_line_number,
            })
            .collect(),
        fallback,
    }
}

fn difftastic_row_view(row: &localreview_difftastic::DifftasticRow) -> DifftasticRowView {
    DifftasticRowView {
        old: row.old.as_ref().map(difftastic_cell_view),
        new: row.new.as_ref().map(difftastic_cell_view),
    }
}

fn difftastic_cell_view(cell: &localreview_difftastic::DifftasticCell) -> DifftasticCellView {
    DifftasticCellView {
        line_number: cell.line_number,
        text: cell.text.clone(),
        changed_spans: cell
            .changed_spans
            .iter()
            .filter_map(|span| {
                let start = byte_to_utf16_index(&cell.text, span.start_byte)?;
                let end = byte_to_utf16_index(&cell.text, span.end_byte)?;
                (end > start).then(|| DifftasticSpanView {
                    start,
                    end,
                    highlight: match span.highlight {
                        localreview_difftastic::DifftasticHighlight::Delimiter => "delimiter",
                        localreview_difftastic::DifftasticHighlight::Normal => "normal",
                        localreview_difftastic::DifftasticHighlight::String => "string",
                        localreview_difftastic::DifftasticHighlight::Type => "type",
                        localreview_difftastic::DifftasticHighlight::Comment => "comment",
                        localreview_difftastic::DifftasticHighlight::Keyword => "keyword",
                        localreview_difftastic::DifftasticHighlight::TreeSitterError => {
                            "tree_sitter_error"
                        }
                    }
                    .into(),
                })
            })
            .collect(),
    }
}

fn byte_to_utf16_index(text: &str, byte: u32) -> Option<u32> {
    let byte = usize::try_from(byte).ok()?;
    if byte > text.len() || !text.is_char_boundary(byte) {
        return None;
    }
    u32::try_from(text[..byte].encode_utf16().count()).ok()
}

fn difftastic_file_status(document: &ReviewDiffDocument) -> &'static str {
    match document.file.status {
        localreview_diff::ReviewFileStatus::Added => "created",
        localreview_diff::ReviewFileStatus::Deleted => "deleted",
        _ => "changed",
    }
}

fn source_line_range(
    source: &str,
    requested_start: Option<u32>,
    requested_end: Option<u32>,
) -> Result<std::ops::Range<usize>, DispatchError> {
    if source.is_empty() {
        return Ok(0..0);
    }
    let offsets = source_line_offsets(source);
    let start_line = requested_start.unwrap_or(1);
    let end_line = requested_end.unwrap_or(start_line);
    if start_line == 0 || end_line < start_line {
        return Err(DispatchError::Invalid(
            "source line range is invalid".into(),
        ));
    }
    let start_index = usize::try_from(start_line.saturating_sub(1)).unwrap_or(usize::MAX);
    let end_index = usize::try_from(end_line).unwrap_or(usize::MAX);
    let start = offsets
        .get(start_index)
        .copied()
        .ok_or_else(|| DispatchError::Invalid("source line is outside captured snapshot".into()))?;
    let end = offsets
        .get(end_index)
        .copied()
        .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
        .unwrap_or(source.len());
    Ok(usize::try_from(start).unwrap_or(usize::MAX)..end)
}

fn is_soft_deleted(annotation: &Annotation) -> bool {
    annotation
        .labels
        .iter()
        .any(|label| label == "localreview:deleted")
}

fn numbered_source(source: &str, range: std::ops::Range<usize>, start_line: u32) -> String {
    source[range]
        .split_inclusive('\n')
        .enumerate()
        .map(|(index, line)| {
            format!(
                "{}\t{}",
                start_line.saturating_add(u32::try_from(index).unwrap_or(u32::MAX)),
                line
            )
        })
        .collect()
}

fn canonical_hunk_text(
    document: &ReviewDiffDocument,
    side: DiffSide,
    line: Option<u32>,
) -> Result<String, DispatchError> {
    let line = line.unwrap_or(1);
    let hunk = document
        .hunks
        .iter()
        .find(|hunk| {
            hunk.unified_rows.iter().any(|row| match side {
                DiffSide::Old => row
                    .old
                    .as_ref()
                    .is_some_and(|cell| cell.line_number == line),
                DiffSide::New => row
                    .new
                    .as_ref()
                    .is_some_and(|cell| cell.line_number == line),
            })
        })
        .ok_or_else(|| DispatchError::NotFound("no immutable hunk at the requested line".into()))?;
    let mut output = format_hunk_header(hunk);
    output.push('\n');
    for row in &hunk.unified_rows {
        match row.kind {
            DiffLineKind::Context => output.push(' '),
            DiffLineKind::Addition => output.push('+'),
            DiffLineKind::Removal => output.push('-'),
        }
        output.push_str(
            row.new
                .as_ref()
                .or(row.old.as_ref())
                .map_or("", |cell| cell.text.as_str()),
        );
        output.push('\n');
    }
    Ok(output)
}

fn canonical_patch_text(document: &ReviewDiffDocument) -> String {
    let old = document
        .file
        .old_path
        .as_ref()
        .unwrap_or(&document.file.path);
    let new = &document.file.path;
    let mut output = format!(
        "diff --git a/{} b/{}\n--- a/{}\n+++ b/{}\n",
        old, new, old, new
    );
    for hunk in &document.hunks {
        output.push_str(&format_hunk_header(hunk));
        output.push('\n');
        for row in &hunk.unified_rows {
            output.push(match row.kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Addition => '+',
                DiffLineKind::Removal => '-',
            });
            output.push_str(
                row.new
                    .as_ref()
                    .or(row.old.as_ref())
                    .map_or("", |cell| cell.text.as_str()),
            );
            output.push('\n');
        }
    }
    output
}

fn github_path(path: &str) -> String {
    let mut output = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(output, "%{byte:02X}");
        }
    }
    output
}

fn spawn_external_editor(path: &Path, line: u32, editor: &str) -> Result<(), DispatchError> {
    // Editors are a closed set of typed adapters. A user-controlled command
    // string is never evaluated by a shell, while editors that support it
    // receive an exact positive captured source line.
    let mut command = match editor {
        "vscode" => {
            let mut value = Command::new("code");
            value
                .arg("--goto")
                .arg(format!("{}:{line}", path.display()));
            value
        }
        "cursor" => {
            let mut value = Command::new("cursor");
            value
                .arg("--goto")
                .arg(format!("{}:{line}", path.display()));
            value
        }
        "zed" => {
            let mut value = Command::new("zed");
            value.arg(format!("{}:{line}", path.display()));
            value
        }
        "sublime" => {
            let mut value = Command::new("subl");
            value.arg(format!("{}:{line}", path.display()));
            value
        }
        "idea" => {
            let mut value = Command::new("idea");
            value.arg("--line").arg(line.to_string()).arg(path);
            value
        }
        _ if cfg!(target_os = "macos") => {
            let mut value = Command::new("open");
            value.arg(path);
            value
        }
        _ => {
            let mut value = Command::new("xdg-open");
            value.arg(path);
            value
        }
    };
    command.spawn().map_err(|error| {
        DispatchError::Invalid(format!(
            "could not open the local file in an external editor: {error}"
        ))
    })?;
    Ok(())
}

fn protocol_summary(workspace: &WorkspaceView) -> WorkspaceSummary {
    WorkspaceSummary {
        id: workspace.id.clone(),
        name: workspace.name.clone(),
        source_tags: workspace
            .source
            .iter()
            .filter_map(|source| match source.as_str() {
                "github" => Some(WorkspaceSourceTag::Github),
                "local" => Some(WorkspaceSourceTag::Local),
                "ssh" => Some(WorkspaceSourceTag::Ssh),
                _ => None,
            })
            .collect(),
        available: workspace.connection == "connected",
        location: Some(workspace.location.clone()),
    }
}

fn local_event_can_change_source(path: &Path) -> bool {
    let components = path.components().collect::<Vec<_>>();
    let Some(git_index) = components
        .iter()
        .position(|component| component.as_os_str() == ".git")
    else {
        return true;
    };
    let git_path = components[(git_index + 1)..]
        .iter()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    match git_path.as_slice() {
        // A linked-worktree `.git` file is stable configuration, not source.
        [] => false,
        [name] if matches!(name.as_ref(), "HEAD" | "index" | "packed-refs") => true,
        [directory, name] if directory == "info" && name == "exclude" => true,
        [directory, ..] if directory == "refs" => {
            !git_path.last().is_some_and(|name| name.ends_with(".lock"))
        }
        // Git's status/diff plumbing can update logs, object caches and lock
        // files without changing the captured source generation.
        _ => false,
    }
}

/// Resolves worktree events through Git's own index and ignore configuration.
/// `check-ignore` deliberately runs without `--no-index`: a tracked file must
/// remain relevant even when a later ignore rule matches it, including after
/// that tracked file has been deleted from the worktree.
fn local_events_can_change_source(repository_roots: &[PathBuf], events: &[notify::Event]) -> bool {
    let mut worktree_paths = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for event in events.iter().filter(|event| {
        matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
        )
    }) {
        if event.need_rescan() || (matches!(event.kind, EventKind::Any) && event.paths.is_empty()) {
            return true;
        }
        for path in &event.paths {
            if !local_event_can_change_source(path) {
                continue;
            }
            if path
                .components()
                .any(|component| component.as_os_str() == ".git")
            {
                return true;
            }
            let Some((root, relative)) = repository_roots
                .iter()
                .filter_map(|root| {
                    path.strip_prefix(root)
                        .ok()
                        .map(|relative| (root, relative))
                })
                // A nested registered repository owns its events rather than
                // inheriting the parent repository's ignore rules.
                .max_by_key(|(root, _)| root.components().count())
            else {
                // Watch transports should return paths below registered roots;
                // an unexpected path is conservatively treated as relevant.
                return true;
            };
            if relative.as_os_str().is_empty() {
                return true;
            }
            let Some(relative) = relative.to_str() else {
                return true;
            };
            worktree_paths
                .entry(root.clone())
                .or_default()
                .insert(relative.replace('\\', "/"));
        }
    }

    worktree_paths.into_iter().any(|(root, paths)| {
        git_reports_all_paths_ignored(&root, &paths).map_or(true, |all_ignored| !all_ignored)
    })
}

/// Uses one NUL-delimited Git query for a coalesced event batch. The output is
/// the subset Git considers ignored; command or decoding failures are handled
/// conservatively by the caller.
fn git_reports_all_paths_ignored(
    repository_root: &Path,
    paths: &BTreeSet<String>,
) -> Result<bool, ()> {
    use std::io::Write as _;
    use std::process::Stdio;

    if paths.is_empty() {
        return Ok(true);
    }
    let mut child = Command::new(git_executable())
        .arg("-C")
        .arg(repository_root)
        .args(["check-ignore", "--stdin", "-z"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let write_result = child.stdin.take().ok_or(()).and_then(|mut stdin| {
        for path in paths {
            stdin.write_all(path.as_bytes()).map_err(|_| ())?;
            stdin.write_all(&[0]).map_err(|_| ())?;
        }
        Ok(())
    });
    if write_result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(());
    }
    let output = child.wait_with_output().map_err(|_| ())?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(());
    }
    let ignored = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .collect::<BTreeSet<_>>();
    Ok(paths.iter().all(|path| ignored.contains(path.as_bytes())))
}

fn publish_refresh_availability(
    flags: &Arc<Mutex<BTreeMap<WorkspaceId, RefreshAvailability>>>,
    app_handle: &Arc<Mutex<Option<tauri::AppHandle>>>,
    workspace_id: WorkspaceId,
    expected_watcher_epoch: Option<u64>,
    available: bool,
) {
    let event = flags.lock().ok().and_then(|mut flags| {
        let state = flags.entry(workspace_id).or_default();
        if expected_watcher_epoch.is_some_and(|epoch| epoch != state.watcher_epoch) {
            return None;
        }
        if available {
            // Every source notification advances the revision, even while the
            // indicator is already amber. A later event racing an in-flight
            // capture must prevent that capture from acknowledging it away.
            state.revision = state.revision.saturating_add(1).max(1);
            state.available = true;
        } else {
            if !state.available {
                return None;
            }
            state.revision = state.revision.saturating_add(1).max(1);
            state.available = false;
        }
        Some(RefreshAvailableEvent {
            workspace_id: workspace_id.to_string(),
            refresh_available: state.available,
            revision: state.revision,
        })
    });
    let Some(event) = event else {
        return;
    };
    let handle = app_handle.lock().ok().and_then(|handle| handle.clone());
    if let Some(handle) = handle {
        let _ = handle.emit(REFRESH_AVAILABLE_EVENT, event);
    }
}

fn publish_refresh_availability_at_boundary(
    flags: &Arc<Mutex<BTreeMap<WorkspaceId, RefreshAvailability>>>,
    app_handle: &Arc<Mutex<Option<tauri::AppHandle>>>,
    workspace_id: WorkspaceId,
    boundary: RefreshCaptureBoundary,
    available: bool,
) {
    let event = flags.lock().ok().and_then(|mut flags| {
        let state = flags.entry(workspace_id).or_default();
        if state.revision != boundary.revision
            || state.watcher_epoch != boundary.watcher_epoch
            || state.available == available
        {
            return None;
        }
        state.revision = state.revision.saturating_add(1).max(1);
        state.available = available;
        Some(RefreshAvailableEvent {
            workspace_id: workspace_id.to_string(),
            refresh_available: state.available,
            revision: state.revision,
        })
    });
    let Some(event) = event else {
        return;
    };
    let handle = app_handle.lock().ok().and_then(|handle| handle.clone());
    if let Some(handle) = handle {
        let _ = handle.emit(REFRESH_AVAILABLE_EVENT, event);
    }
}

fn source_name(source: localreview_domain::WorkspaceSourceTag) -> &'static str {
    match source {
        localreview_domain::WorkspaceSourceTag::GitHub => "github",
        localreview_domain::WorkspaceSourceTag::Local => "local",
        localreview_domain::WorkspaceSourceTag::Ssh => "ssh",
    }
}

fn github_pull_request_context_view(
    review: localreview_service::GitHubPullRequestRecord,
) -> GitHubPullRequestContextView {
    GitHubPullRequestContextView {
        canonical_url: review.canonical_url,
        title: review.title,
        author: review.author,
        base_ref: review.base_ref,
        head_ref: review.head_ref,
        pinned_base_sha: review.pinned_base_sha.to_string(),
        pinned_head_sha: review.pinned_head_sha.to_string(),
        draft: review.draft,
        state: review.state,
        review_decision: review.review_decision,
        commits: review
            .commits
            .into_iter()
            .map(|commit| GitHubCommitContextView {
                sha: commit.sha.to_string(),
                message_headline: commit.message_headline,
                authored_at: commit.authored_at.map(|value| value.to_rfc3339()),
            })
            .collect(),
        import_error: review.import_error,
    }
}

fn imported_github_thread_view(
    thread: localreview_github::ImportedReviewThread,
) -> ImportedGitHubReviewThreadView {
    ImportedGitHubReviewThreadView {
        id: thread.id,
        resolved: thread.resolved,
        outdated: thread.outdated,
        path: thread.path.map(|path| path.as_str().to_owned()),
        line: thread.line,
        original_line: thread.original_line,
        comments: thread
            .comments
            .into_iter()
            .map(|comment| ImportedGitHubReviewCommentView {
                id: comment.id,
                body_markdown: comment.body_markdown,
                author: comment.author,
                url: comment.url,
                created_at: comment.created_at.map(|value| value.to_rfc3339()),
            })
            .collect(),
    }
}

fn imported_github_conversation_view(
    comment: localreview_github::ImportedConversationComment,
) -> ImportedGitHubConversationCommentView {
    ImportedGitHubConversationCommentView {
        id: comment.id,
        body_markdown: comment.body_markdown,
        author: comment.author,
        url: comment.url,
        created_at: comment.created_at.map(|value| value.to_rfc3339()),
    }
}

fn repository_view(
    repository: &Repository,
    comparison: Option<&localreview_domain::RepositoryComparison>,
    workspace_default_base: &BaseReference,
) -> RepositoryView {
    let branch = match &repository.current_branch {
        localreview_domain::HeadState::Branch(branch) => branch.clone(),
        localreview_domain::HeadState::Detached(sha) => format!("detached {}", sha.as_str()),
        localreview_domain::HeadState::Unborn => "unborn".into(),
    };
    RepositoryView {
        id: repository.id.to_string(),
        name: repository
            .relative_path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or(repository.relative_path.as_str())
            .into(),
        path: repository.relative_path.as_str().into(),
        branch,
        base: comparison.map_or_else(
            || {
                repository
                    .base_override
                    .as_ref()
                    .unwrap_or(workspace_default_base)
                    .as_str()
                    .into()
            },
            |comparison| comparison.requested_base.as_str().into(),
        ),
        merge_base: comparison.map_or_else(String::new, |comparison| {
            comparison.merge_base_sha.as_str().into()
        }),
        head: comparison
            .and_then(|comparison| comparison.head_sha.as_ref())
            .map_or_else(String::new, |sha| sha.as_str().into()),
        is_override: repository.base_override.is_some(),
        comparison_options: comparison.map(|comparison| comparison.options.clone()),
    }
}

#[derive(Default)]
struct RepositorySetupLive {
    clean: Option<bool>,
    changed_file_count: Option<usize>,
    status_summary: String,
    suggested_base: Option<String>,
    resolved_base_sha: Option<String>,
    merge_base_sha: Option<String>,
    head_sha: Option<String>,
    ahead: Option<u32>,
    behind: Option<u32>,
    comparison_error: Option<String>,
    status_checked_at: Option<chrono::DateTime<Utc>>,
}

fn repository_setup_view(
    repository: &Repository,
    baseline: &localreview_domain::ResolvedBaseline,
    pinned: Option<&RepositoryComparison>,
    live: RepositorySetupLive,
) -> RepositorySetupView {
    let branch = match &repository.current_branch {
        HeadState::Branch(branch) => branch.clone(),
        HeadState::Detached(sha) => format!("detached {}", sha.as_str()),
        HeadState::Unborn => "unborn".into(),
    };
    let fallback_resolved_base = pinned
        .map(|comparison| comparison.base_tip_sha.to_string())
        .or_else(|| {
            repository
                .last_resolved_base_sha
                .as_ref()
                .map(ToString::to_string)
        });
    let fallback_merge_base = pinned.map(|comparison| comparison.merge_base_sha.to_string());
    let fallback_head =
        pinned.and_then(|comparison| comparison.head_sha.as_ref().map(ToString::to_string));
    let comparison_error = live
        .comparison_error
        .or_else(|| repository.comparison_error.clone());
    RepositorySetupView {
        id: repository.id.to_string(),
        path: repository.relative_path.as_str().into(),
        enabled: repository.enabled,
        branch,
        clean: live.clean,
        changed_file_count: live.changed_file_count,
        status_summary: if live.status_summary.is_empty() {
            "Status not checked for this source".into()
        } else {
            live.status_summary
        },
        effective_base: baseline.reference.as_str().into(),
        suggested_base: live.suggested_base,
        base_source: baseline_source_label(baseline.source).into(),
        base_override: repository
            .base_override
            .as_ref()
            .map(|base| base.as_str().into()),
        resolved_base_sha: live.resolved_base_sha.or(fallback_resolved_base),
        merge_base_sha: live.merge_base_sha.or(fallback_merge_base),
        head_sha: live.head_sha.or(fallback_head),
        ahead: live.ahead,
        behind: live.behind,
        last_fetch_at: repository.last_fetch_at.map(|value| value.to_rfc3339()),
        last_fetch_error: repository.last_fetch_error.clone(),
        discovery_error: repository.discovery_error.clone(),
        comparison_error,
        status_checked_at: live.status_checked_at.map(|value| value.to_rfc3339()),
    }
}

fn baseline_source_label(source: BaselineSource) -> &'static str {
    match source {
        BaselineSource::TemporaryReviewOverride => "temporary",
        BaselineSource::RepositoryOverride => "override",
        BaselineSource::WorkspaceDefault => "inherited",
        BaselineSource::ApplicationDefault => "application default",
    }
}

fn status_summary(changes: &[localreview_git::WorkingTreeChange]) -> String {
    if changes.is_empty() {
        return "Clean".into();
    }
    let staged = changes
        .iter()
        .filter(|change| change.index_status != ' ' && change.index_status != '?')
        .count();
    let unstaged = changes
        .iter()
        .filter(|change| change.worktree_status != ' ' && change.worktree_status != '?')
        .count();
    let untracked = changes
        .iter()
        .filter(|change| change.kind == localreview_git::WorkingTreeChangeKind::Untracked)
        .count();
    let mut segments = Vec::new();
    if staged > 0 {
        segments.push(format!("{staged} staged"));
    }
    if unstaged > 0 {
        segments.push(format!("{unstaged} unstaged"));
    }
    if untracked > 0 {
        segments.push(format!("{untracked} untracked"));
    }
    if segments.is_empty() {
        format!("Dirty: {} changed", changes.len())
    } else {
        format!("Dirty: {}", segments.join(", "))
    }
}

fn local_refresh_outcome(
    result: &localreview_service::StartReviewResult,
) -> LocalRefreshOutcomeView {
    let captured_repository_count = result.captures.len();
    let failed_repository_count = result.failures.len();
    let status = if failed_repository_count == 0 {
        "success"
    } else if captured_repository_count == 0 {
        "failed"
    } else {
        "partial"
    };
    LocalRefreshOutcomeView {
        status: status.into(),
        captured_repository_count,
        failed_repository_count,
        failures: result
            .failures
            .iter()
            .map(|failure| LocalRefreshFailureView {
                repository_id: failure.repository_id.to_string(),
                repository_path: failure.relative_path.to_string(),
                error: failure.error.clone(),
            })
            .collect(),
    }
}

fn commit_summary_view(summary: localreview_git::GitCommitSummary) -> CommitSummaryView {
    CommitSummaryView {
        sha: summary.sha.to_string(),
        parent_shas: summary
            .parent_shas
            .into_iter()
            .map(|sha| sha.to_string())
            .collect(),
        author_name: summary.author_name,
        author_email: summary.author_email,
        authored_at: summary.authored_at,
        subject: summary.subject,
    }
}

fn commit_context_view(
    comparison_id: localreview_domain::ComparisonId,
    context: localreview_git::GitCommitContext,
) -> CapturedCommitContextView {
    CapturedCommitContextView {
        comparison_id: comparison_id.to_string(),
        range: CommitRangeView {
            merge_base: context.range.merge_base.to_string(),
            head: context.range.head.to_string(),
        },
        commits: context
            .commits
            .into_iter()
            .map(commit_summary_view)
            .collect(),
        truncated: context.truncated,
        selected_commit: context.selected_commit.map(|details| CommitDetailsView {
            summary: commit_summary_view(details.summary),
            committer_name: details.committer_name,
            committer_email: details.committer_email,
            committed_at: details.committed_at,
            body: details.body,
            body_truncated: details.body_truncated,
        }),
    }
}

fn previous_review_change_name(
    kind: localreview_service::PreviousReviewFileChangeKind,
) -> &'static str {
    match kind {
        localreview_service::PreviousReviewFileChangeKind::Added => "added",
        localreview_service::PreviousReviewFileChangeKind::Removed => "removed",
        localreview_service::PreviousReviewFileChangeKind::Renamed => "renamed",
        localreview_service::PreviousReviewFileChangeKind::Modified => "modified",
        localreview_service::PreviousReviewFileChangeKind::Unchanged => "unchanged",
    }
}

fn file_view(
    document: &ReviewDiffDocument,
    repository_id: String,
    viewed: bool,
    annotation_count: usize,
) -> ReviewFileView {
    let additions = u32::try_from(document.changed_new_lines.len()).unwrap_or(u32::MAX);
    let deletions = u32::try_from(document.changed_old_lines.len()).unwrap_or(u32::MAX);
    ReviewFileView {
        id: document.file.id.to_string(),
        comparison_id: document.comparison_id.to_string(),
        repository_id,
        path: document.file.path.as_str().into(),
        previous_path: document.file.old_path.as_ref().map(ToString::to_string),
        status: match document.file.status {
            localreview_diff::ReviewFileStatus::Added => "added",
            localreview_diff::ReviewFileStatus::Modified => "modified",
            localreview_diff::ReviewFileStatus::Deleted => "deleted",
            localreview_diff::ReviewFileStatus::Renamed => "renamed",
            localreview_diff::ReviewFileStatus::Copied => "renamed",
            localreview_diff::ReviewFileStatus::Binary
            | localreview_diff::ReviewFileStatus::ModeChanged
            | localreview_diff::ReviewFileStatus::TypeChanged => "modified",
            localreview_diff::ReviewFileStatus::Submodule => "submodule",
            localreview_diff::ReviewFileStatus::LfsPointer => "lfs_pointer",
        }
        .into(),
        additions,
        deletions,
        hunk_count: document.hunks.len(),
        language: language_name(document.file.path.as_str()).into(),
        viewed,
        annotation_count,
    }
}

fn annotation_view(
    annotation: &Annotation,
    documents: &[PersistedReviewDocument],
) -> AnnotationView {
    let anchor = annotation.anchor.as_ref();
    let file_id = if anchor.is_some() {
        documents
            .iter()
            .find(|document| annotation_matches_file(annotation, &document.document))
            .map(|document| document.document.file.id.to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    AnnotationView {
        id: annotation.id.to_string(),
        file_id,
        repository_id: anchor.map_or_else(String::new, |anchor| anchor.repository_id.to_string()),
        kind: annotation_kind_name(annotation.kind).into(),
        state: annotation_state_name(annotation.state, anchor).into(),
        side: anchor
            .and_then(|anchor| anchor.side)
            .map(side_name)
            .unwrap_or("new")
            .into(),
        start_line: anchor
            .and_then(|anchor| anchor.start_line)
            .unwrap_or_default(),
        end_line: anchor
            .and_then(|anchor| anchor.end_line)
            .unwrap_or_default(),
        body: annotation.body_markdown.clone(),
        selected_source: anchor.map_or_else(String::new, |anchor| anchor.selected_source.clone()),
        labels: annotation.labels.clone(),
        local_only: annotation.publication_state == PublicationState::LocalOnly,
        created_at: annotation.created_at.to_rfc3339(),
        published_id: (annotation.publication_state == PublicationState::Published)
            .then(|| "published".into()),
    }
}

fn annotation_matches_file(annotation: &Annotation, document: &ReviewDiffDocument) -> bool {
    annotation.anchor.as_ref().is_some_and(|anchor| {
        anchor.comparison_id == document.comparison_id && anchor.file_path == document.file.path
    })
}

fn header_row(hunk: &localreview_diff::ReviewHunk) -> DiffRowView {
    DiffRowView {
        id: format!("header:{}", hunk.id.0),
        kind: "header".into(),
        hunk_id: Some(hunk.id.0.clone()),
        old_line: None,
        new_line: None,
        old_text: None,
        new_text: None,
        text: None,
        hunk: Some(format!(
            "@@ -{},{} +{},{} @@{}",
            hunk.header.old_start,
            hunk.header.old_count,
            hunk.header.new_start,
            hunk.header.new_count,
            hunk.header
                .context
                .as_deref()
                .map_or_else(String::new, |context| format!(" {context}"))
        )),
        has_annotation: false,
        old_source_start_byte: None,
        new_source_start_byte: None,
        omitted_block_id: None,
        omitted_count: None,
        omitted_end_line: None,
        omitted_side: None,
        omitted_expanded: None,
    }
}

fn row_kind(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Context => "context",
        DiffLineKind::Addition => "addition",
        DiffLineKind::Removal => "deletion",
    }
}

fn language_name(path: &str) -> &'static str {
    resolve_language(Path::new(path), "", None).map_or(
        "Text",
        localreview_highlight::HighlightLanguage::display_name,
    )
}

fn parse_remote_target(value: &str) -> Result<RemoteTarget, DispatchError> {
    validate_ssh_target(value).map_err(|error| DispatchError::Invalid(error.to_string()))?;
    let (host, root) = value
        .split_once(':')
        .ok_or_else(|| DispatchError::Invalid("SSH target must be host:/absolute/path".into()))?;
    SshDestination::new(host).map_err(|_| {
        DispatchError::Invalid("SSH host must be a normal OpenSSH host or user@host alias".into())
    })?;
    if root.split('/').any(|segment| segment == "..") {
        return Err(DispatchError::Invalid(
            "SSH workspace root must not contain parent traversal".into(),
        ));
    }
    Ok(RemoteTarget {
        host: host.into(),
        root: root.into(),
    })
}

fn connect_remote_session(
    target: &RemoteTarget,
    forward_handler: Option<ManagedForwardHandler>,
) -> Result<RemoteSessionConnect, RemoteConnectionFailure> {
    let destination = SshDestination::new(&target.host).map_err(|_| RemoteConnectionFailure {
        companion: RemoteCompanionStatus {
            availability: RemoteCompanionAvailability::MissingOrUnreachable,
            platform: None,
            detail: "SSH host must be a normal OpenSSH host or user@host alias".into(),
        },
        error: Box::new(DispatchError::Invalid(
            "SSH host must be a normal OpenSSH host or user@host alias".into(),
        )),
    })?;
    let bootstrapper = remote_bootstrapper(destination.clone());
    let (mut companion, remote_agent_program) = match bootstrapper.probe() {
        CompanionProbe::Compatible {
            connection: _,
            platform,
        } => (
            RemoteCompanionStatus {
                availability: RemoteCompanionAvailability::Compatible,
                platform: platform.map(remote_companion_platform),
                detail: "compatible companion found through the configured SSH command path".into(),
            },
            RemoteAgentProgram::PathLookup,
        ),
        CompanionProbe::Incompatible { platform, detail } => (
            RemoteCompanionStatus {
                availability: RemoteCompanionAvailability::Incompatible,
                platform: Some(remote_companion_platform(platform)),
                detail,
            },
            RemoteAgentProgram::UserLocal,
        ),
        CompanionProbe::MissingOrUnreachable { platform, detail } => (
            RemoteCompanionStatus {
                availability: RemoteCompanionAvailability::MissingOrUnreachable,
                platform: platform.map(remote_companion_platform),
                detail,
            },
            RemoteAgentProgram::UserLocal,
        ),
    };
    let pending_reverse = forward_handler
        .map(|handler| managed_reverse_forward(&target.host, handler))
        .transpose()?;
    let mut config = remote_connection_config(destination);
    config.remote_agent_program = remote_agent_program;
    if let Some(pending) = &pending_reverse {
        config.reverse_tunnel = Some(ReverseTunnel {
            local_port: pending.listener.local_port(),
            remote_port: pending.listener.remote_port(),
        });
    }
    let mut session = SshSession::connect(config).map_err(|error| {
        if matches!(remote_agent_program, RemoteAgentProgram::UserLocal) {
            companion.detail = format!(
                "{}; ~/.local/bin fallback failed: {error}",
                companion.detail
            );
        }
        RemoteConnectionFailure {
            companion: companion.clone(),
            error: Box::new(remote_transport_error(error)),
        }
    })?;
    if let Some(pending) = &pending_reverse {
        session
            .configure_managed_forward_relay(&pending.environment)
            .map_err(|error| RemoteConnectionFailure {
                companion: companion.clone(),
                error: Box::new(DispatchError::Remote(format!(
                    "could not configure the managed SSH reverse-forward relay: {error}"
                ))),
            })?;
    }
    if matches!(remote_agent_program, RemoteAgentProgram::UserLocal) {
        companion = RemoteCompanionStatus {
            availability: RemoteCompanionAvailability::Compatible,
            platform: companion.platform,
            detail: format!(
                "compatible companion found in ~/.local/bin after path probe: {}",
                companion.detail
            ),
        };
    }
    let reverse_forward = pending_reverse.map(|pending| {
        ManagedReverseForwardRuntime::start(
            pending.listener,
            session.disconnection_signal(),
            pending.handler,
        )
    });
    Ok(RemoteSessionConnect {
        session,
        companion,
        reverse_forward,
    })
}

fn managed_reverse_forward(
    host: &str,
    handler: ManagedForwardHandler,
) -> Result<PendingManagedReverseForward, RemoteConnectionFailure> {
    let listener =
        ReverseForwardListener::bind_managed(MANAGED_REVERSE_FORWARD_TTL).map_err(|error| {
            RemoteConnectionFailure {
                companion: RemoteCompanionStatus {
                    availability: RemoteCompanionAvailability::MissingOrUnreachable,
                    platform: None,
                    detail: "could not prepare a managed SSH reverse-forward session".into(),
                },
                error: Box::new(DispatchError::Remote(format!(
                    "could not create managed reverse forwarding for {host}: {error}"
                ))),
            }
        })?;
    let environment = listener.managed_environment();
    let endpoint = environment
        .iter()
        .find(|(key, _)| key == "LOCALREVIEW_MANAGED_FORWARD_ENDPOINT")
        .map(|(_, value)| value.clone())
        .ok_or_else(|| RemoteConnectionFailure {
            companion: RemoteCompanionStatus {
                availability: RemoteCompanionAvailability::MissingOrUnreachable,
                platform: None,
                detail: "managed reverse-forward endpoint was unavailable".into(),
            },
            error: Box::new(DispatchError::Internal),
        })?;
    let token_hex = environment
        .into_iter()
        .find(|(key, _)| key == "LOCALREVIEW_MANAGED_FORWARD_TOKEN")
        .map(|(_, value)| value)
        .ok_or_else(|| RemoteConnectionFailure {
            companion: RemoteCompanionStatus {
                availability: RemoteCompanionAvailability::MissingOrUnreachable,
                platform: None,
                detail: "managed reverse-forward token was unavailable".into(),
            },
            error: Box::new(DispatchError::Internal),
        })?;
    Ok(PendingManagedReverseForward {
        listener,
        handler,
        environment: ManagedForwardEnvironment {
            endpoint,
            token_hex,
            session_id: Uuid::new_v4().simple().to_string(),
        },
    })
}

fn remote_bootstrapper(destination: SshDestination) -> CompanionBootstrapper {
    #[cfg(test)]
    {
        let mut bootstrapper = CompanionBootstrapper::new(destination);
        if let Some(program) = std::env::var_os("LOCALREVIEW_TEST_SSH_PROGRAM") {
            bootstrapper.ssh_program = PathBuf::from(program);
        }
        bootstrapper
    }
    #[cfg(not(test))]
    {
        CompanionBootstrapper::new(destination)
    }
}

fn remote_connection_config(destination: SshDestination) -> SshConnectionConfig {
    #[cfg(test)]
    {
        let mut config = SshConnectionConfig::new(destination);
        if let Some(program) = std::env::var_os("LOCALREVIEW_TEST_SSH_PROGRAM") {
            config.ssh_program = program;
        }
        config
    }
    #[cfg(not(test))]
    {
        SshConnectionConfig::new(destination)
    }
}

fn remote_companion_platform(platform: localreview_ssh::RemotePlatform) -> RemoteCompanionPlatform {
    RemoteCompanionPlatform {
        operating_system: platform.operating_system,
        architecture: platform.architecture,
    }
}

fn remote_companion_availability_name(availability: &RemoteCompanionAvailability) -> &'static str {
    match availability {
        RemoteCompanionAvailability::Compatible => "compatible",
        RemoteCompanionAvailability::Incompatible => "incompatible",
        RemoteCompanionAvailability::MissingOrUnreachable => "missing or unreachable",
    }
}

fn remote_companion_status_detail(status: &RemoteCompanionStatus) -> String {
    let platform = status.platform.as_ref().map_or_else(
        || "platform unknown".to_owned(),
        |platform| format!("{}/{}", platform.operating_system, platform.architecture),
    );
    format!(
        "{} ({platform})",
        remote_companion_availability_name(&status.availability)
    )
}

fn remote_transport_error(error: localreview_ssh::SshError) -> DispatchError {
    DispatchError::Remote(format!(
        "could not connect to the LocalReview SSH companion: {error}. Ensure `localreview agent --stdio` is installed and protocol-compatible on the remote host (or install it manually), then retry."
    ))
}

fn remote_result_error(context: &str, result: AgentResult) -> DispatchError {
    match result {
        AgentResult::Error { error } => {
            let install = matches!(
                error.code,
                AgentErrorCode::Unavailable | AgentErrorCode::UnsupportedVersion
            )
            .then_some(" Install or update `localreview` on the remote host and retry.")
            .unwrap_or("");
            DispatchError::Remote(format!("{context} failed: {}{}", error.message, install))
        }
        other => DispatchError::Remote(format!(
            "{context} returned an unexpected companion result: {other:?}"
        )),
    }
}

fn remote_workspace_name(target: &RemoteTarget) -> String {
    let leaf = target
        .root
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("workspace");
    format!("{}:{leaf}", target.host)
}

fn remote_repository_record(
    workspace_id: WorkspaceId,
    remote: &RemoteRepository,
) -> Result<Repository, DispatchError> {
    let relative_path = if remote.reference.relative_path == "." {
        StoredPath::from(".")
    } else {
        StoredPath::from(remote.reference.relative_path.as_str())
    };
    Ok(Repository {
        id: RepositoryId::new(),
        workspace_id,
        relative_path,
        // This is a remote identity, never a local filesystem path to open.
        // Controller operations branch on WorkspaceSource before any local Git
        // access, keeping it presentation metadata only.
        worktree_path: StoredPath::from(remote.canonical_worktree.as_str()),
        git_common_dir: remote.git_common_dir.as_deref().map(StoredPath::from),
        normalized_primary_remote: remote.primary_remote.clone(),
        enabled: true,
        base_override: None,
        current_branch: remote_head_state(&remote.head)?,
        last_resolved_base_sha: None,
        last_fetch_at: None,
        last_fetch_error: None,
        discovery_error: None,
        comparison_error: None,
    })
}

fn remote_head_state(value: &RemoteHead) -> Result<HeadState, DispatchError> {
    match value {
        RemoteHead::Branch(branch) => Ok(HeadState::Branch(branch.clone())),
        RemoteHead::Detached(sha) => Ok(HeadState::Detached(
            GitSha::new(sha.clone()).map_err(|error| DispatchError::Invalid(error.to_string()))?,
        )),
        RemoteHead::Unborn => Ok(HeadState::Unborn),
    }
}

fn remote_comparison(
    repository: &Repository,
    capture: &RemoteComparisonCapture,
    options: &ComparisonOptions,
) -> Result<RepositoryComparison, DispatchError> {
    Ok(RepositoryComparison {
        id: localreview_domain::ComparisonId::new(),
        repository_id: repository.id,
        requested_base: BaseReference::new(capture.requested_base.clone())
            .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        base_tip_sha: GitSha::new(capture.base_tip_sha.clone())
            .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        merge_base_sha: GitSha::new(capture.merge_base_sha.clone())
            .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        head_sha: capture
            .head_sha
            .as_ref()
            .map(|sha| GitSha::new(sha.clone()))
            .transpose()
            .map_err(|error| DispatchError::Invalid(error.to_string()))?,
        head: remote_head_state(&capture.head)?,
        index_fingerprint: ContentFingerprint::from_bytes(capture.capture_id.as_bytes()),
        working_tree_fingerprint: ContentFingerprint::from_bytes(
            format!("{}:{}", capture.capture_id, capture.generation).as_bytes(),
        ),
        untracked_files: capture
            .files
            .iter()
            .filter(|file| file.untracked)
            .map(|file| UntrackedFile {
                path: StoredPath::from(file.path.as_str()),
                fingerprint: ContentFingerprint::from_bytes(
                    file.new_object_id
                        .as_deref()
                        .unwrap_or(capture.capture_id.as_str())
                        .as_bytes(),
                ),
                byte_len: file.captured_byte_len.unwrap_or_default(),
                binary: file.binary,
            })
            .collect(),
        options: options.clone(),
        captured_at: Utc::now(),
    })
}

fn remote_comparison_options(options: &ComparisonOptions) -> RemoteComparisonOptions {
    RemoteComparisonOptions {
        ignore_all_whitespace: options.ignore_all_whitespace,
        ignore_space_at_eol: options.ignore_space_at_eol,
        ignore_cr_at_eol: options.ignore_cr_at_eol,
        path_filters: options
            .path_filters
            .iter()
            .map(|path| path.as_str().to_owned())
            .collect(),
    }
}

fn remote_placeholder_documents(
    comparison: &RepositoryComparison,
    capture: &RemoteComparisonCapture,
) -> (Vec<PersistedReviewDocument>, Vec<RemoteFileBinding>) {
    remote_placeholder_documents_with_ids(comparison, capture, &BTreeMap::new())
}

fn remote_placeholder_documents_with_ids(
    comparison: &RepositoryComparison,
    capture: &RemoteComparisonCapture,
    existing_ids: &BTreeMap<String, ReviewFileId>,
) -> (Vec<PersistedReviewDocument>, Vec<RemoteFileBinding>) {
    let mut documents = Vec::with_capacity(capture.files.len());
    let mut bindings = Vec::with_capacity(capture.files.len());
    for remote_file in &capture.files {
        let id = existing_ids
            .get(&remote_file.path)
            .or_else(|| {
                remote_file
                    .old_path
                    .as_ref()
                    .and_then(|path| existing_ids.get(path))
            })
            .copied()
            .unwrap_or_else(ReviewFileId::new);
        // The initial payload is intentionally empty. Its durable file/status
        // record makes the files pane available immediately; selected source
        // is filled only by ensure_remote_file_materialized.
        let file = match remote_review_file(id, remote_file) {
            Ok(file) => file,
            Err(_) => continue,
        };
        documents.push(PersistedReviewDocument {
            document: document_from_sources(comparison.id, file, "", ""),
        });
        bindings.push(RemoteFileBinding {
            file_id: id.to_string(),
            file: remote_file.clone(),
            materialized: remote_file_requires_non_text(remote_file),
        });
    }
    (documents, bindings)
}

fn remote_review_file(
    id: ReviewFileId,
    remote: &RemoteCapturedFile,
) -> Result<ReviewFile, DispatchError> {
    localreview_protocol::validate_relative_path(&remote.path)
        .map_err(|error| DispatchError::Invalid(error.to_string()))?;
    if let Some(old_path) = &remote.old_path {
        localreview_protocol::validate_relative_path(old_path)
            .map_err(|error| DispatchError::Invalid(error.to_string()))?;
    }
    Ok(ReviewFile {
        id,
        path: StoredPath::from(remote.path.as_str()),
        old_path: remote.old_path.as_deref().map(StoredPath::from),
        status: remote_review_file_status(remote),
    })
}

fn remote_review_file_status(remote: &RemoteCapturedFile) -> ReviewFileStatus {
    if remote.binary {
        return ReviewFileStatus::Binary;
    }
    if remote.lfs_pointer {
        return ReviewFileStatus::LfsPointer;
    }
    match remote.status {
        RemoteFileStatus::Added | RemoteFileStatus::Untracked => ReviewFileStatus::Added,
        RemoteFileStatus::Modified => ReviewFileStatus::Modified,
        RemoteFileStatus::Deleted => ReviewFileStatus::Deleted,
        RemoteFileStatus::Renamed => ReviewFileStatus::Renamed,
        RemoteFileStatus::Copied => ReviewFileStatus::Copied,
        RemoteFileStatus::ModeChanged => ReviewFileStatus::ModeChanged,
        RemoteFileStatus::TypeChanged => ReviewFileStatus::TypeChanged,
        RemoteFileStatus::Submodule => ReviewFileStatus::Submodule,
    }
}

fn remote_file_requires_non_text(file: &RemoteCapturedFile) -> bool {
    file.binary || file.lfs_pointer || file.status == RemoteFileStatus::Submodule
}

fn remote_file_has_old_source(file: &RemoteCapturedFile) -> bool {
    !matches!(
        file.status,
        RemoteFileStatus::Added | RemoteFileStatus::Untracked
    ) && !remote_file_requires_non_text(file)
}

fn remote_file_has_new_source(file: &RemoteCapturedFile) -> bool {
    !matches!(file.status, RemoteFileStatus::Deleted) && !remote_file_requires_non_text(file)
}

fn validate_remote_source_window(
    window: &RemoteSourceWindow,
    capture: &RemoteComparisonCapture,
    path: &str,
    revision: RemoteSourceRevision,
    start_line: u32,
) -> Result<(), DispatchError> {
    if window.capture_id != capture.capture_id
        || window.capture_generation != capture.generation
        || window.repository != capture.repository
        || window.path != path
        || window.revision != revision
        || window.start_line != start_line
        || window.content_sha256_hex.len() != 64
        || !window
            .content_sha256_hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DispatchError::Remote(
            "remote source response did not match its capture identity; refresh is required".into(),
        ));
    }
    Ok(())
}

fn remote_previous_file_ids(
    documents: &[PersistedReviewDocument],
) -> BTreeMap<String, ReviewFileId> {
    let mut ids = BTreeMap::new();
    for document in documents {
        ids.insert(
            document.document.file.path.as_str().to_owned(),
            document.document.file.id,
        );
        if let Some(old_path) = &document.document.file.old_path {
            ids.insert(old_path.as_str().to_owned(), document.document.file.id);
        }
    }
    ids
}

fn review_generation_rows(
    documents: Vec<PersistedReviewDocument>,
) -> Vec<(String, String, PersistedReviewDocument)> {
    documents
        .into_iter()
        .map(|document| {
            (
                document.document.file.id.to_string(),
                document.document.file.path.as_str().to_owned(),
                document,
            )
        })
        .collect()
}

fn remote_workspace_key(workspace_id: WorkspaceId) -> String {
    format!("{REMOTE_WORKSPACE_KEY_PREFIX}{workspace_id}")
}

fn legacy_remote_workspace_key(workspace_id: WorkspaceId) -> String {
    format!("{LEGACY_REMOTE_WORKSPACE_KEY_PREFIX}{workspace_id}")
}

fn parse_base(value: Option<&str>) -> Result<Option<BaseReference>, DispatchError> {
    value
        .map(BaseReference::new)
        .transpose()
        .map_err(|error| DispatchError::Invalid(error.to_string()))
}

fn github_review_conclusion(value: ReviewConclusion) -> localreview_github::ReviewConclusion {
    match value {
        ReviewConclusion::Comment => localreview_github::ReviewConclusion::Comment,
        ReviewConclusion::Approve => localreview_github::ReviewConclusion::Approve,
        ReviewConclusion::RequestChanges => localreview_github::ReviewConclusion::RequestChanges,
    }
}

fn parse_workspace_id(value: &str) -> Option<WorkspaceId> {
    Uuid::parse_str(value).ok().map(WorkspaceId)
}

fn parse_review_file_id(value: &str) -> Option<ReviewFileId> {
    Uuid::parse_str(value).ok().map(ReviewFileId)
}

fn parse_comparison_id(value: &str) -> Option<localreview_domain::ComparisonId> {
    Uuid::parse_str(value)
        .ok()
        .map(localreview_domain::ComparisonId)
}

fn parse_annotation_id(value: &str) -> Option<AnnotationId> {
    Uuid::parse_str(value).ok().map(AnnotationId)
}

fn parse_history_set_id(value: &str) -> Option<AnnotationSetId> {
    value
        .strip_prefix("set:")
        .and_then(|id| Uuid::parse_str(id).ok())
        .map(AnnotationSetId)
}

fn parse_history_review_id(value: &str) -> Option<ReviewSessionId> {
    value
        .strip_prefix("review:")
        .and_then(|id| Uuid::parse_str(id).ok())
        .map(ReviewSessionId)
}

fn parse_prompt_history_reference(
    value: Option<&str>,
) -> Result<Option<PromptHistoryReference>, DispatchError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let invalid = || DispatchError::Invalid(format!("unsupported prompt history id: {value}"));
    if let Some(id) = value.strip_prefix("set:") {
        return Uuid::parse_str(id)
            .map(AnnotationSetId)
            .map(PromptHistoryReference::Set)
            .map(Some)
            .map_err(|_| invalid());
    }
    if let Some(id) = value.strip_prefix("review:") {
        return Uuid::parse_str(id)
            .map(ReviewSessionId)
            .map(PromptHistoryReference::Review)
            .map(Some)
            .map_err(|_| invalid());
    }
    if let Some(id) = value.strip_prefix("export:") {
        return Uuid::parse_str(id)
            .map(PromptExportId)
            .map(PromptHistoryReference::Export)
            .map(Some)
            .map_err(|_| invalid());
    }
    Err(invalid())
}

fn parse_annotation_kind(value: &str) -> Result<AnnotationKind, DispatchError> {
    match value {
        "comment" => Ok(AnnotationKind::Comment),
        "question" => Ok(AnnotationKind::Question),
        "suggestion" => Ok(AnnotationKind::Suggestion),
        "file_note" => Ok(AnnotationKind::FileNote),
        "review_note" => Ok(AnnotationKind::ReviewNote),
        _ => Err(DispatchError::Invalid("unsupported annotation kind".into())),
    }
}

fn parse_annotation_state(value: &str) -> Result<AnnotationState, DispatchError> {
    match value {
        "open" => Ok(AnnotationState::Open),
        "resolved" => Ok(AnnotationState::Resolved),
        "outdated" => Ok(AnnotationState::Deleted),
        _ => Err(DispatchError::Invalid(
            "unsupported annotation state".into(),
        )),
    }
}

fn parse_side(value: &str) -> Result<DiffSide, DispatchError> {
    match value {
        "old" => Ok(DiffSide::Old),
        "new" => Ok(DiffSide::New),
        _ => Err(DispatchError::Invalid(
            "annotation side must be old or new".into(),
        )),
    }
}

fn parse_full_file_view(value: &str) -> Result<FullFileView, DispatchError> {
    match value {
        "old" => Ok(FullFileView::Old),
        "new" => Ok(FullFileView::New),
        "both" => Ok(FullFileView::Both),
        _ => Err(DispatchError::Invalid(
            "fullFileSide must be old, new, or both".into(),
        )),
    }
}

fn parse_time(value: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn annotation_kind_name(kind: AnnotationKind) -> &'static str {
    match kind {
        AnnotationKind::Comment => "comment",
        AnnotationKind::Question => "question",
        AnnotationKind::Suggestion => "suggestion",
        AnnotationKind::FileNote => "file_note",
        AnnotationKind::ReviewNote => "review_note",
    }
}

fn annotation_state_name(
    state: AnnotationState,
    anchor: Option<&AnnotationAnchor>,
) -> &'static str {
    if anchor.is_some_and(|anchor| anchor.outdated) || state == AnnotationState::Deleted {
        "outdated"
    } else if state == AnnotationState::Resolved {
        "resolved"
    } else {
        "open"
    }
}

fn side_name(side: DiffSide) -> &'static str {
    match side {
        DiffSide::Old => "old",
        DiffSide::New => "new",
    }
}

fn prompt_scope(
    scope: &str,
    selected: Vec<AnnotationId>,
) -> Result<localreview_domain::PromptScope, DispatchError> {
    match scope {
        "feedback" => Ok(localreview_domain::PromptScope::AllActionable),
        "questions" => Ok(localreview_domain::PromptScope::AllQuestions),
        "all" => Ok(localreview_domain::PromptScope::CommentsAndQuestions),
        "selected" => Ok(localreview_domain::PromptScope::Selected(selected)),
        "focused_question" if selected.len() == 1 => Ok(
            localreview_domain::PromptScope::FocusedQuestion(selected[0]),
        ),
        "focused_question" => Err(DispatchError::Invalid(
            "focused_question requires exactly one annotation id".into(),
        )),
        _ => Err(DispatchError::Invalid("unsupported prompt scope".into())),
    }
}

fn prompt_path_style(
    path_style: Option<&str>,
    legacy_portable: Option<bool>,
) -> Result<PromptPathStyle, DispatchError> {
    match path_style {
        Some("portable") => Ok(PromptPathStyle::Portable),
        Some("qualified") => Ok(PromptPathStyle::Qualified),
        Some("absolute") => Ok(PromptPathStyle::Absolute),
        Some(_) => Err(DispatchError::Invalid(
            "prompt path style must be portable, qualified, or absolute".into(),
        )),
        None => Ok(match legacy_portable {
            Some(true) => PromptPathStyle::Portable,
            Some(false) => PromptPathStyle::Qualified,
            None => PromptPathStyle::Absolute,
        }),
    }
}

fn hunk_for_annotation(annotation: &Annotation, document: &ReviewDiffDocument) -> Option<String> {
    let anchor = annotation.anchor.as_ref()?;
    let side = anchor.side?;
    let start_line = anchor.start_line?;
    let end_line = anchor.end_line?;
    let hunk = document.hunks.iter().find(|hunk| {
        hunk.unified_rows.iter().any(|row| {
            row.old.as_ref().is_some_and(|cell| {
                cell.side == side && cell.line_number >= start_line && cell.line_number <= end_line
            }) || row.new.as_ref().is_some_and(|cell| {
                cell.side == side && cell.line_number >= start_line && cell.line_number <= end_line
            })
        })
    })?;
    let mut output = header_row(hunk).hunk?;
    output.push('\n');
    for row in &hunk.unified_rows {
        let (prefix, text) = match row.kind {
            DiffLineKind::Context => (' ', row.new.as_ref().or(row.old.as_ref())?.text.as_str()),
            DiffLineKind::Addition => ('+', row.new.as_ref()?.text.as_str()),
            DiffLineKind::Removal => ('-', row.old.as_ref()?.text.as_str()),
        };
        output.push(prefix);
        output.push_str(text);
        output.push('\n');
    }
    Some(output)
}

fn prompt_preview(record: &PromptExportRecord, formatted: FormattedPrompt) -> PromptPreview {
    let title = record
        .title
        .clone()
        .unwrap_or_else(|| prompt_title_for_scope(&record.scope).to_owned());
    let annotation_count = record
        .annotation_count
        .unwrap_or(formatted.annotation_ids.len());
    let estimated_tokens = record
        .estimated_tokens
        .unwrap_or_else(|| formatted.markdown.len().div_ceil(4));
    PromptPreview {
        export_id: record.id.to_string(),
        title,
        content: formatted.markdown,
        annotation_count,
        estimated_tokens,
    }
}

fn prompt_preview_from_record(record: &PromptExportRecord) -> PromptPreview {
    let content = record
        .rendered_markdown
        .clone()
        .expect("callers only use an exact prompt record");
    PromptPreview {
        export_id: record.id.to_string(),
        title: record
            .title
            .clone()
            .unwrap_or_else(|| prompt_title_for_scope(&record.scope).to_owned()),
        annotation_count: record
            .annotation_count
            .unwrap_or(record.annotation_ids.len()),
        estimated_tokens: record
            .estimated_tokens
            .unwrap_or_else(|| content.len().div_ceil(4)),
        content,
    }
}

fn ensure_prompt_session_workspace(
    session: &ReviewSession,
    workspace_id: WorkspaceId,
) -> Result<(), DispatchError> {
    if session.workspace_id != workspace_id {
        return Err(DispatchError::NotFound(session.id.to_string()));
    }
    Ok(())
}

fn export_annotation_set_ids(record: &PromptExportRecord) -> Vec<AnnotationSetId> {
    if record.annotation_set_ids.is_empty() {
        vec![record.annotation_set_id]
    } else {
        record.annotation_set_ids.clone()
    }
}

fn document_repository_id(
    document: &ReviewDiffDocument,
    comparisons: &BTreeMap<RepositoryId, localreview_domain::RepositoryComparison>,
) -> Result<RepositoryId, DispatchError> {
    comparisons
        .values()
        .find(|comparison| comparison.id == document.comparison_id)
        .map(|comparison| comparison.repository_id)
        .ok_or_else(|| DispatchError::Invalid("file comparison is no longer active".into()))
}

fn sets_annotation_count(
    state: &StateStore,
    session_id: ReviewSessionId,
) -> Result<usize, DispatchError> {
    state
        .annotation_sets(session_id)?
        .into_iter()
        .map(|set| state.annotations(set.id).map_err(DispatchError::from))
        .collect::<Result<Vec<_>, _>>()
        .map(|sets| sets.into_iter().map(|set| set.len()).sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreview_diff::{document_from_sources, ReviewFile, ReviewFileStatus};
    use localreview_domain::{
        AnnotationSet, ComparisonId, ContentFingerprint, GitSha, HeadState, RepositoryComparison,
        StoredPath,
    };
    use tempfile::TempDir;

    #[cfg(unix)]
    static SSH_PROGRAM_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    struct ReviewFixture {
        _state_directory: TempDir,
        _workspace_directory: TempDir,
        controller: DesktopController,
        workspace_id: WorkspaceId,
        file_id: ReviewFileId,
        repository_id: RepositoryId,
    }

    #[test]
    fn start_review_input_accepts_the_camel_case_desktop_contract() {
        let input: StartOrRefreshInput = serde_json::from_value(serde_json::json!({
            "fetchBeforeCapture": false,
            "comparisonOptions": {
                "ignoreAllWhitespace": true,
                "ignoreSpaceAtEol": false,
                "ignoreCrAtEol": true
            }
        }))
        .unwrap();

        let options = input.comparison_options.unwrap();
        assert!(options.ignore_all_whitespace);
        assert!(!options.ignore_space_at_eol);
        assert!(options.ignore_cr_at_eol);
        assert!(options.path_filters.is_empty());
    }

    #[test]
    fn local_refresh_reports_all_repository_failures_and_keeps_retry_available() {
        let fixture = review_fixture("before\n", "captured\n");

        let refreshed = fixture
            .controller
            .refresh_review(fixture.workspace_id, StartOrRefreshInput::default())
            .unwrap();

        let outcome = refreshed
            .refresh_outcome
            .expect("explicit local refresh outcome");
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.captured_repository_count, 0);
        assert_eq!(outcome.failed_repository_count, 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(
            outcome.failures[0].repository_id,
            fixture.repository_id.to_string()
        );
        assert_eq!(outcome.failures[0].repository_path, ".");
        assert!(!outcome.failures[0].error.is_empty());
        assert!(refreshed.workspace.refresh_available);
        assert!(refreshed.workspace.refresh_available_revision > 0);
    }

    fn review_fixture(old_source: &str, new_source: &str) -> ReviewFixture {
        let state_directory = TempDir::new().unwrap();
        let workspace_directory = TempDir::new().unwrap();
        std::fs::write(workspace_directory.path().join("review.rs"), new_source).unwrap();
        let store = StateStore::open(state_directory.path()).unwrap();
        let now = Utc::now();
        let workspace_id = WorkspaceId::new();
        let repository_id = RepositoryId::new();
        let session_id = ReviewSessionId::new();
        let comparison_id = ComparisonId::new();
        let file_id = ReviewFileId::new();
        let workspace = Workspace {
            id: workspace_id,
            display_name: "fixture".into(),
            source: WorkspaceSource::LocalDirectory {
                root: StoredPath::new(workspace_directory.path()),
            },
            default_base: BaseReference::default(),
            pinned: false,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        let repository = Repository {
            id: repository_id,
            workspace_id,
            relative_path: StoredPath::from("."),
            worktree_path: StoredPath::new(workspace_directory.path()),
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
        let session = ReviewSession {
            id: session_id,
            workspace_id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        let sha = GitSha::new("0123456789abcdef0123456789abcdef01234567").unwrap();
        let comparison = RepositoryComparison {
            id: comparison_id,
            repository_id,
            requested_base: BaseReference::default(),
            base_tip_sha: sha.clone(),
            merge_base_sha: sha.clone(),
            head_sha: Some(sha.clone()),
            head: HeadState::Branch("feature".into()),
            index_fingerprint: ContentFingerprint::from_bytes(b"index"),
            working_tree_fingerprint: ContentFingerprint::from_bytes(b"worktree"),
            untracked_files: Vec::new(),
            options: ComparisonOptions::default(),
            captured_at: now,
        };
        let document = document_from_sources(
            comparison_id,
            ReviewFile {
                id: file_id,
                path: StoredPath::from("review.rs"),
                old_path: None,
                status: ReviewFileStatus::Modified,
            },
            old_source,
            new_source,
        );
        let set = AnnotationSet {
            id: AnnotationSetId::new(),
            review_session_id: session_id,
            sequence: 1,
            active: true,
            archived_at: None,
            created_at: now,
        };
        store.upsert_workspace(&workspace).unwrap();
        store.upsert_repository(&repository).unwrap();
        store.save_review_session(&session).unwrap();
        store
            .save_session_comparison(session_id, &comparison)
            .unwrap();
        store
            .set_current_comparison(session_id, &comparison)
            .unwrap();
        store
            .save_review_file_payload(
                comparison_id,
                file_id,
                "review.rs",
                &PersistedReviewDocument { document },
            )
            .unwrap();
        store.save_annotation_set(&set).unwrap();
        ReviewFixture {
            _state_directory: state_directory,
            _workspace_directory: workspace_directory,
            controller: DesktopController::new(store),
            workspace_id,
            file_id,
            repository_id,
        }
    }

    #[test]
    fn local_open_with_missing_base_enters_durable_setup_then_retries_after_restart() {
        let workspace_directory = TempDir::new().unwrap();
        let repository = workspace_directory.path();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(repository)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "user.name", "Review Test"]);
        std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "base"]);
        git(&["switch", "-c", "feature"]);
        std::fs::write(repository.join("tracked.txt"), "changed\n").unwrap();

        let state_directory = TempDir::new().unwrap();
        let controller = DesktopController::new(StateStore::open(state_directory.path()).unwrap());
        let (uncaptured, created) = controller
            .open_local_workspace(OpenWorkspaceInput {
                path: repository.to_string_lossy().into_owned(),
                base: Some("origin/master".into()),
                repository_bases: Vec::new(),
            })
            .unwrap();
        assert!(created);
        assert!(!uncaptured.review_ready);
        assert_eq!(uncaptured.default_base, "origin/master");
        let workspace_id = parse_workspace_id(&uncaptured.id).unwrap();
        let uncaptured_review = controller.load_review(workspace_id).unwrap();
        assert_eq!(uncaptured_review.repositories[0].base, "origin/master");
        assert_eq!(
            controller.workspace_ui_state(workspace_id).unwrap().mode,
            "unified"
        );
        let setup = controller.repository_setup(workspace_id).unwrap();
        assert_eq!(setup.len(), 1);
        assert!(setup[0].comparison_error.is_some());
        drop(controller);

        let reopened = DesktopController::new(StateStore::open(state_directory.path()).unwrap());
        let (captured, created) = reopened
            .open_local_workspace(OpenWorkspaceInput {
                path: repository.to_string_lossy().into_owned(),
                base: Some("main".into()),
                repository_bases: Vec::new(),
            })
            .unwrap();
        assert!(!created);
        assert!(captured.review_ready);
        assert_eq!(captured.id, uncaptured.id);
        let review = reopened.load_review(workspace_id).unwrap();
        assert_eq!(review.workspace.progress.total, 1);
        assert_eq!(review.repositories[0].base, "main");
    }

    #[test]
    fn controller_uses_global_config_before_the_release_base_default() {
        let state_directory = TempDir::new().unwrap();
        let config_directory = TempDir::new().unwrap();
        let config_path = config_directory.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[workspace]\ndefault_base = \"origin/global\"\n",
        )
        .unwrap();
        let controller = DesktopController::with_global_config_path(
            StateStore::open(state_directory.path()).unwrap(),
            config_path,
        );
        assert_eq!(
            controller.application_base().unwrap().as_str(),
            "origin/global"
        );
    }

    #[test]
    fn presentation_language_honors_linguist_language_attributes() {
        let fixture = review_fixture("fn old() {}\n", "fn new() {}\n");
        let output = std::process::Command::new("git")
            .current_dir(fixture._workspace_directory.path())
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(output.status.success());
        std::fs::write(
            fixture._workspace_directory.path().join(".gitattributes"),
            "*.rs linguist-language=Java\n",
        )
        .unwrap();
        let document = fixture
            .controller
            .persisted_review_document(fixture.file_id, None)
            .unwrap()
            .document;
        let session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture
                .controller
                .highlight_language_attribute(&document, session.id)
                .as_deref(),
            Some("Java")
        );
    }

    #[test]
    fn file_list_language_labels_share_the_highlight_resolver() {
        assert_eq!(language_name("src/Main.java"), "Java");
        assert_eq!(language_name("rules/BUILD.bazel"), "Starlark");
        assert_eq!(language_name("src/worker.kt"), "Kotlin");
        assert_eq!(language_name("queries/review.sql"), "SQL");
        assert_eq!(language_name("flake.nix"), "Nix");
        assert_eq!(language_name("unknown.data"), "Text");
    }

    fn replace_fixture_review(
        fixture: &ReviewFixture,
        old_source: &str,
        new_source: &str,
    ) -> (ReviewSession, ReviewFileId) {
        let now = Utc::now();
        let session = ReviewSession {
            id: ReviewSessionId::new(),
            workspace_id: fixture.workspace_id,
            status: ReviewSessionStatus::Active,
            started_at: now,
            refreshed_at: None,
            archived_at: None,
            completed_at: None,
        };
        let set = AnnotationSet {
            id: AnnotationSetId::new(),
            review_session_id: session.id,
            sequence: 1,
            active: true,
            archived_at: None,
            created_at: now,
        };
        let comparison_id = ComparisonId::new();
        let file_id = ReviewFileId::new();
        let sha = GitSha::new("89abcdef0123456789abcdef0123456789abcdef").unwrap();
        let comparison = RepositoryComparison {
            id: comparison_id,
            repository_id: fixture.repository_id,
            requested_base: BaseReference::default(),
            base_tip_sha: sha.clone(),
            merge_base_sha: sha.clone(),
            head_sha: Some(sha),
            head: HeadState::Branch("feature".into()),
            index_fingerprint: ContentFingerprint::from_bytes(b"replacement-index"),
            working_tree_fingerprint: ContentFingerprint::from_bytes(new_source.as_bytes()),
            untracked_files: Vec::new(),
            options: ComparisonOptions::default(),
            captured_at: now,
        };
        let document = PersistedReviewDocument {
            document: document_from_sources(
                comparison_id,
                ReviewFile {
                    id: file_id,
                    path: StoredPath::from("review.rs"),
                    old_path: None,
                    status: ReviewFileStatus::Modified,
                },
                old_source,
                new_source,
            ),
        };
        let generation = fixture
            .controller
            .state()
            .prepare_review_generation(
                &comparison,
                &[(file_id.to_string(), "review.rs".into(), document)],
            )
            .unwrap();
        fixture
            .controller
            .state()
            .replace_active_review(fixture.workspace_id, &session, &set, &[generation], now)
            .unwrap();
        (session, file_id)
    }

    #[test]
    fn workspace_rail_metadata_is_durable_and_cannot_mutate_archived_records() {
        let fixture = review_fixture("old\n", "new\n");
        let updated = fixture
            .controller
            .update_workspace_metadata(
                fixture.workspace_id,
                Some("  Pinned review  ".into()),
                Some(true),
            )
            .unwrap();
        assert_eq!(updated.name, "Pinned review");
        assert!(updated.pinned);
        let reopened = fixture
            .controller
            .state()
            .workspace(fixture.workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(reopened.display_name, "Pinned review");
        assert!(reopened.pinned);

        fixture
            .controller
            .archive_workspace(fixture.workspace_id)
            .unwrap();
        assert!(matches!(
            fixture.controller.update_workspace_metadata(
                fixture.workspace_id,
                Some("hidden mutation".into()),
                None,
            ),
            Err(DispatchError::Invalid(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn ssh_workspace_is_manifest_first_then_materializes_exact_crlf_source() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = SSH_PROGRAM_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let fixture = TempDir::new().unwrap();
        let transcript = fixture.path().join("agent.frames");
        let old = b"old\r\n".to_vec();
        let new = b"new\r\n".to_vec();
        let reference = localreview_protocol::RemoteRepositoryRef {
            workspace_root: "/srv/review".into(),
            relative_path: ".".into(),
        };
        let remote = localreview_protocol::RemoteRepository {
            reference: reference.clone(),
            canonical_worktree: "/srv/review".into(),
            git_common_dir: None,
            primary_remote: Some("origin".into()),
            head: RemoteHead::Branch("feature".into()),
        };
        let capture = RemoteComparisonCapture {
            capture_id: "capture-remote-1".into(),
            generation: 1,
            repository: reference.clone(),
            requested_base: "origin/master".into(),
            base_tip_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            merge_base_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            head_sha: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
            head: RemoteHead::Branch("feature".into()),
            committed: localreview_protocol::RemoteLayerSummary { changed_files: 1 },
            staged: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
            unstaged: localreview_protocol::RemoteLayerSummary { changed_files: 1 },
            untracked: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
            files: vec![RemoteCapturedFile {
                path: "src/lib.rs".into(),
                old_path: None,
                status: RemoteFileStatus::Modified,
                similarity_percent: None,
                old_mode: 0o100644,
                new_mode: 0o100644,
                old_object_id: Some("0123456789abcdef0123456789abcdef01234567".into()),
                new_object_id: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
                untracked: false,
                binary: false,
                lfs_pointer: false,
                captured_byte_len: Some(u64::try_from(new.len()).unwrap()),
                layers: vec![localreview_protocol::RemoteChangeLayer::Unstaged],
            }],
        };
        let response_id = |sequence| format!("ssh-{}-{sequence}", std::process::id());
        let source_window = |bytes: Vec<u8>, revision| RemoteSourceWindow {
            capture_id: capture.capture_id.clone(),
            capture_generation: capture.generation,
            repository: reference.clone(),
            path: "src/lib.rs".into(),
            revision,
            start_line: 1,
            total_lines: 1,
            byte_len: u64::try_from(bytes.len()).unwrap(),
            content_sha256_hex: hex::encode(Sha256::digest(&bytes)),
            bytes,
            end_of_file: true,
        };
        let mut frames = Vec::new();
        for response in [
            localreview_protocol::AgentResponse {
                id: response_id(1),
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: localreview_protocol::AgentHello::current(
                        "fixture-agent",
                        "linux",
                        "x86_64",
                    ),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(2),
                generation: 1,
                result: AgentResult::Repositories {
                    repositories: vec![remote],
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(3),
                generation: 1,
                result: AgentResult::ComparisonCapture {
                    capture: capture.clone(),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(4),
                generation: 1,
                result: AgentResult::SourceWindow {
                    window: source_window(old.clone(), RemoteSourceRevision::MergeBase),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(5),
                generation: 1,
                result: AgentResult::SourceWindow {
                    window: source_window(new.clone(), RemoteSourceRevision::Worktree),
                },
            },
        ] {
            localreview_protocol::write_frame(
                &mut frames,
                &localreview_protocol::AgentMessage::Response(response),
            )
            .unwrap();
        }
        std::fs::write(&transcript, frames).unwrap();
        let program = fixture.path().join("fake-ssh");
        let escaped_transcript = transcript.to_string_lossy().replace('\'', "'\"'\"'");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"uname -s\"*) printf 'Linux\\n'; exit 0;;\n  *\"uname -m\"*) printf 'x86_64\\n'; exit 0;;\nesac\ncat '{escaped_transcript}'\nsleep 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("LOCALREVIEW_TEST_SSH_PROGRAM", &program);

        let store = StateStore::open(fixture.path().join("state")).unwrap();
        let controller = DesktopController::new(store);
        let (workspace, created) = controller
            .open_ssh_workspace(OpenSshWorkspaceInput {
                target: "fixture-host:/srv/review".into(),
            })
            .unwrap();
        assert!(created);
        assert!(workspace.detail.contains("fixture-agent"));
        let review = controller
            .load_review(parse_workspace_id(&workspace.id).unwrap())
            .unwrap();
        let file_id = parse_review_file_id(&review.files[0].id).unwrap();
        let placeholder: PersistedReviewDocument = controller
            .state()
            .review_file_payload(file_id)
            .unwrap()
            .unwrap();
        assert!(
            placeholder.document.hunks.is_empty(),
            "open must remain manifest-first"
        );
        let rows = controller.rows(file_id, "unified").unwrap();
        assert!(!rows.is_empty());
        let materialized: PersistedReviewDocument = controller
            .state()
            .review_file_payload(file_id)
            .unwrap()
            .unwrap();
        assert_eq!(materialized.document.old.content, "old\r\n");
        assert_eq!(materialized.document.new.content, "new\r\n");
        let workspace_id = parse_workspace_id(&workspace.id).unwrap();
        controller.delete_workspace(workspace_id).unwrap();
        std::env::remove_var("LOCALREVIEW_TEST_SSH_PROGRAM");
    }

    #[cfg(unix)]
    #[test]
    fn archived_ssh_review_keeps_its_unopened_source_when_file_ids_are_reused() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = SSH_PROGRAM_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let fixture = TempDir::new().unwrap();
        let state_directory = fixture.path().join("state");
        let transcript = fixture.path().join("agent.frames");
        let reference = localreview_protocol::RemoteRepositoryRef {
            workspace_root: "/srv/review".into(),
            relative_path: ".".into(),
        };
        let remote = localreview_protocol::RemoteRepository {
            reference: reference.clone(),
            canonical_worktree: "/srv/review".into(),
            git_common_dir: None,
            primary_remote: Some("origin".into()),
            head: RemoteHead::Branch("feature".into()),
        };
        let captured_file = |byte_len| RemoteCapturedFile {
            path: "src/lib.rs".into(),
            old_path: None,
            status: RemoteFileStatus::Modified,
            similarity_percent: None,
            old_mode: 0o100644,
            new_mode: 0o100644,
            old_object_id: Some("0123456789abcdef0123456789abcdef01234567".into()),
            new_object_id: None,
            untracked: false,
            binary: false,
            lfs_pointer: false,
            captured_byte_len: Some(byte_len),
            layers: vec![localreview_protocol::RemoteChangeLayer::Unstaged],
        };
        let capture = |capture_id: &str, generation, byte_len| RemoteComparisonCapture {
            capture_id: capture_id.into(),
            generation,
            repository: reference.clone(),
            requested_base: "origin/master".into(),
            base_tip_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            merge_base_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            head_sha: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
            head: RemoteHead::Branch("feature".into()),
            committed: localreview_protocol::RemoteLayerSummary { changed_files: 1 },
            staged: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
            unstaged: localreview_protocol::RemoteLayerSummary { changed_files: 1 },
            untracked: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
            files: vec![captured_file(byte_len)],
        };
        let base = b"base\n".to_vec();
        let review_one = b"review one\n".to_vec();
        let review_two = b"review two\n".to_vec();
        let first_capture = capture(
            "capture-review-one",
            1,
            u64::try_from(review_one.len()).unwrap(),
        );
        let second_capture = capture(
            "capture-review-two",
            2,
            u64::try_from(review_two.len()).unwrap(),
        );
        let source_window =
            |capture: &RemoteComparisonCapture, bytes: &[u8], revision| RemoteSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: capture.generation,
                repository: reference.clone(),
                path: "src/lib.rs".into(),
                revision,
                start_line: 1,
                total_lines: 1,
                byte_len: u64::try_from(bytes.len()).unwrap(),
                content_sha256_hex: hex::encode(Sha256::digest(bytes)),
                bytes: bytes.to_vec(),
                end_of_file: true,
            };
        let response_id = |sequence| format!("ssh-{}-{sequence}", std::process::id());
        let mut frames = Vec::new();
        for response in [
            localreview_protocol::AgentResponse {
                id: response_id(1),
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: localreview_protocol::AgentHello::current(
                        "fixture-agent",
                        "linux",
                        "x86_64",
                    ),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(2),
                generation: 1,
                result: AgentResult::Repositories {
                    repositories: vec![remote.clone()],
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(3),
                generation: 1,
                result: AgentResult::ComparisonCapture {
                    capture: first_capture.clone(),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(4),
                generation: 1,
                result: AgentResult::SourceWindow {
                    window: source_window(&first_capture, &base, RemoteSourceRevision::MergeBase),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(5),
                generation: 1,
                result: AgentResult::SourceWindow {
                    window: source_window(
                        &first_capture,
                        &review_one,
                        RemoteSourceRevision::Worktree,
                    ),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(6),
                generation: 2,
                result: AgentResult::Repositories {
                    repositories: vec![remote],
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(7),
                generation: 2,
                result: AgentResult::ComparisonCapture {
                    capture: second_capture.clone(),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(8),
                generation: 2,
                result: AgentResult::SourceWindow {
                    window: source_window(&second_capture, &base, RemoteSourceRevision::MergeBase),
                },
            },
            localreview_protocol::AgentResponse {
                id: response_id(9),
                generation: 2,
                result: AgentResult::SourceWindow {
                    window: source_window(
                        &second_capture,
                        &review_two,
                        RemoteSourceRevision::Worktree,
                    ),
                },
            },
        ] {
            localreview_protocol::write_frame(
                &mut frames,
                &localreview_protocol::AgentMessage::Response(response),
            )
            .unwrap();
        }
        std::fs::write(&transcript, frames).unwrap();
        let program = fixture.path().join("fake-ssh");
        let escaped_transcript = transcript.to_string_lossy().replace('\'', "'\"'\"'");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"uname -s\"*) printf 'Linux\\n'; exit 0;;\n  *\"uname -m\"*) printf 'x86_64\\n'; exit 0;;\nesac\ncat '{escaped_transcript}'\nsleep 2\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("LOCALREVIEW_TEST_SSH_PROGRAM", &program);

        let controller = DesktopController::new(StateStore::open(&state_directory).unwrap());
        let (workspace, _) = controller
            .open_ssh_workspace(OpenSshWorkspaceInput {
                target: "fixture-host:/srv/review".into(),
            })
            .unwrap();
        let workspace_id = parse_workspace_id(&workspace.id).unwrap();
        // The initial review transport already owns the capture transcript
        // above. Future SSH invocations are notification-only watcher
        // transports. Model the companion contract instead of replaying the
        // capture responses: handshake, establish the initial fingerprint,
        // announce Watching, then stay alive until the desktop cancels the
        // original watch request.
        let watcher_request_id = format!("watch-workspace-{workspace_id}");
        let watcher_generation = 1;
        let watcher_transcript = fixture.path().join("watcher.frames");
        let mut watcher_frames = Vec::new();
        for message in [
            localreview_protocol::AgentMessage::Response(localreview_protocol::AgentResponse {
                id: response_id(1),
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: localreview_protocol::AgentHello::current(
                        "fixture-agent",
                        "linux",
                        "x86_64",
                    ),
                },
            }),
            localreview_protocol::AgentMessage::Progress(localreview_protocol::AgentProgress {
                id: watcher_request_id.clone(),
                generation: watcher_generation,
                phase: AgentProgressPhase::Watching,
                completed: 0,
                total: None,
            }),
        ] {
            localreview_protocol::write_frame(&mut watcher_frames, &message).unwrap();
        }
        std::fs::write(&watcher_transcript, watcher_frames).unwrap();

        let mut initial_watcher_requests = Vec::new();
        for request in [
            localreview_protocol::AgentRequest {
                id: response_id(1),
                generation: 0,
                operation: AgentOperation::Handshake {
                    desktop_versions: vec![PROTOCOL_VERSION],
                },
            },
            localreview_protocol::AgentRequest {
                id: watcher_request_id.clone(),
                generation: watcher_generation,
                operation: AgentOperation::WatchWorkspaceChanges {
                    repositories: vec![reference.clone()],
                    poll_interval_millis: 1_000,
                },
            },
        ] {
            localreview_protocol::write_frame(
                &mut initial_watcher_requests,
                &localreview_protocol::AgentMessage::Request(request),
            )
            .unwrap();
        }
        let watcher_cancelled = fixture.path().join("watcher-cancelled.frames");
        let mut watcher_cancelled_frames = Vec::new();
        localreview_protocol::write_frame(
            &mut watcher_cancelled_frames,
            &localreview_protocol::AgentMessage::Response(localreview_protocol::AgentResponse {
                id: watcher_request_id,
                generation: watcher_generation,
                result: AgentResult::Cancelled,
            }),
        )
        .unwrap();
        std::fs::write(&watcher_cancelled, watcher_cancelled_frames).unwrap();
        let escaped_watcher_transcript = watcher_transcript
            .to_string_lossy()
            .replace('\'', "'\"'\"'");
        let escaped_watcher_cancelled =
            watcher_cancelled.to_string_lossy().replace('\'', "'\"'\"'");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"uname -s\"*) printf 'Linux\\n'; exit 0;;\n  *\"uname -m\"*) printf 'x86_64\\n'; exit 0;;\n  *\"--stdio\"*)\n    cat '{escaped_watcher_transcript}'\n    dd if=/dev/stdin of=/dev/null bs=1 count={} 2>/dev/null\n    dd if=/dev/stdin of=/dev/null bs=1 count=1 2>/dev/null\n    cat '{escaped_watcher_cancelled}'\n    exit 0;;\n  *) exit 1;;\nesac\n",
                initial_watcher_requests.len()
            ),
        )
        .unwrap();
        let first = controller.load_review(workspace_id).unwrap();
        let reused_file_id = first.files[0].id.clone();
        let first_session_id = controller
            .service
            .active_review_session(workspace_id)
            .unwrap()
            .unwrap()
            .id;
        let first_placeholder: PersistedReviewDocument = controller
            .state()
            .review_file_payload_for_comparison(&first.files[0].comparison_id, &reused_file_id)
            .unwrap()
            .unwrap();
        assert!(
            first_placeholder.document.hunks.is_empty(),
            "review one must still be unopened before New Review freezes it"
        );

        let second = controller
            .start_new_review(workspace_id, StartOrRefreshInput::default())
            .unwrap();
        assert_eq!(second.files[0].id, reused_file_id);
        let resources = TempDir::new().unwrap();
        controller
            .presentation_window(
                PresentationRequest {
                    file_id: reused_file_id.clone(),
                    comparison_id: Some(second.files[0].comparison_id.clone()),
                    mode: "full".into(),
                    start_row: 0,
                    end_row: 20,
                    generation: 1,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        controller.stop_remote_watchers(workspace_id);
        drop(controller);
        std::env::remove_var("LOCALREVIEW_TEST_SSH_PROGRAM");

        let reopened = DesktopController::new(StateStore::open(&state_directory).unwrap());
        let sessions = reopened.state().review_sessions(workspace_id).unwrap();
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.status == ReviewSessionStatus::Active)
                .count(),
            1
        );
        let archived = reopened
            .load_archived_review(workspace_id, &format!("review:{first_session_id}"))
            .unwrap();
        let current = reopened.load_review(workspace_id).unwrap();
        assert_eq!(archived.files[0].id, current.files[0].id);

        let archived_window = reopened
            .presentation_window(
                PresentationRequest {
                    file_id: archived.files[0].id.clone(),
                    comparison_id: Some(archived.files[0].comparison_id.clone()),
                    mode: "full".into(),
                    start_row: 0,
                    end_row: 20,
                    generation: 2,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        let current_window = reopened
            .presentation_window(
                PresentationRequest {
                    file_id: current.files[0].id.clone(),
                    comparison_id: Some(current.files[0].comparison_id.clone()),
                    mode: "full".into(),
                    start_row: 0,
                    end_row: 20,
                    generation: 3,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(
            archived_window
                .rows
                .iter()
                .find_map(|row| row.new_text.as_deref()),
            Some("review one")
        );
        assert_eq!(
            current_window
                .rows
                .iter()
                .find_map(|row| row.new_text.as_deref()),
            Some("review two")
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_reverse_forward_uses_fixed_ssh_argv_then_configures_relay_over_stdio() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = SSH_PROGRAM_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let fixture = TempDir::new().unwrap();
        let transcript = fixture.path().join("companion.frames");
        let invocations = fixture.path().join("ssh-invocations");
        let mut frames = Vec::new();
        for response in [
            localreview_protocol::AgentResponse {
                id: format!("ssh-{}-1", std::process::id()),
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: localreview_protocol::AgentHello::current(
                        "fixture-agent",
                        "linux",
                        "x86_64",
                    ),
                },
            },
            localreview_protocol::AgentResponse {
                id: format!("ssh-{}-2", std::process::id()),
                generation: 0,
                result: AgentResult::ManagedForwardRelayConfigured,
            },
        ] {
            localreview_protocol::write_frame(
                &mut frames,
                &localreview_protocol::AgentMessage::Response(response),
            )
            .unwrap();
        }
        std::fs::write(&transcript, frames).unwrap();
        let program = fixture.path().join("fake-ssh");
        let escaped_transcript = transcript.to_string_lossy().replace('\'', "'\"'\"'");
        let escaped_invocations = invocations.to_string_lossy().replace('\'', "'\"'\"'");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"uname -s\"*) printf 'Linux\\n'; exit 0;;\n  *\"uname -m\"*) printf 'x86_64\\n'; exit 0;;\nesac\nprintf '%s\\n' \"$*\" >> '{escaped_invocations}'\ncat '{escaped_transcript}'\nsleep 1\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("LOCALREVIEW_TEST_SSH_PROGRAM", &program);

        let target = RemoteTarget {
            host: "fixture-host".into(),
            root: "/srv/review".into(),
        };
        let connected = match connect_remote_session(&target, Some(Arc::new(|_open| Ok(())))) {
            Ok(connected) => connected,
            Err(_) => panic!("managed reverse session should connect through the fixture"),
        };
        assert!(connected.reverse_forward.is_some());
        let invocations = std::fs::read_to_string(&invocations).unwrap();
        let managed = invocations
            .lines()
            .find(|line| line.contains("-R"))
            .expect("managed session should configure a loopback reverse tunnel");
        assert!(managed.contains("127.0.0.1:"));
        assert!(!managed.contains("LOCALREVIEW_MANAGED_FORWARD_TOKEN"));
        assert!(!managed.contains("LOCALREVIEW_MANAGED_FORWARD_ENDPOINT"));
        assert!(!managed.contains("localreview env"));
        drop(connected);
        std::env::remove_var("LOCALREVIEW_TEST_SSH_PROGRAM");
    }

    #[cfg(unix)]
    #[test]
    fn remote_workspace_watcher_uses_one_async_transport_for_one_hundred_repositories() {
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };

        let _guard = SSH_PROGRAM_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let fixture = TempDir::new().unwrap();
        let transcript = fixture.path().join("handshake.frames");
        let mut frames = Vec::new();
        localreview_protocol::write_frame(
            &mut frames,
            &localreview_protocol::AgentMessage::Response(localreview_protocol::AgentResponse {
                id: format!("ssh-{}-1", std::process::id()),
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: localreview_protocol::AgentHello::current(
                        "fixture-agent",
                        "linux",
                        "x86_64",
                    ),
                },
            }),
        )
        .unwrap();
        std::fs::write(&transcript, frames).unwrap();
        let invocations = fixture.path().join("agent-invocations");
        let program = fixture.path().join("fake-ssh");
        let escaped_transcript = transcript.to_string_lossy().replace('\'', "'\"'\"'");
        let escaped_invocations = invocations.to_string_lossy().replace('\'', "'\"'\"'");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *\"uname -s\"*) printf 'Linux\\n'; exit 0;;\n  *\"uname -m\"*) printf 'x86_64\\n'; exit 0;;\nesac\nprintf 'agent\\n' >> '{escaped_invocations}'\ncat '{escaped_transcript}'\nsleep 2\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::env::set_var("LOCALREVIEW_TEST_SSH_PROGRAM", &program);

        let controller =
            DesktopController::new(StateStore::open(fixture.path().join("state")).unwrap());
        let workspace_id = WorkspaceId::new();
        let captures = (0..100)
            .map(|index| {
                let reference = localreview_protocol::RemoteRepositoryRef {
                    workspace_root: "/srv/review".into(),
                    relative_path: format!("repo-{index}"),
                };
                RemoteCaptureBinding {
                    repository_id: format!("repository-{index}"),
                    comparison_id: format!("comparison-{index}"),
                    capture: RemoteComparisonCapture {
                        capture_id: format!("capture-{index}"),
                        generation: 1,
                        repository: reference,
                        requested_base: "origin/master".into(),
                        base_tip_sha: "0123456789abcdef0123456789abcdef01234567".into(),
                        merge_base_sha: "0123456789abcdef0123456789abcdef01234567".into(),
                        head_sha: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
                        head: RemoteHead::Branch("feature".into()),
                        committed: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
                        staged: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
                        unstaged: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
                        untracked: localreview_protocol::RemoteLayerSummary { changed_files: 0 },
                        files: Vec::new(),
                    },
                    files: Vec::new(),
                }
            })
            .collect();
        let metadata = RemoteWorkspaceMetadata {
            schema_version: 4,
            target: RemoteTarget {
                host: "fixture-host".into(),
                root: "/srv/review".into(),
            },
            review_session_id: ReviewSessionId::new().to_string(),
            generation: 1,
            agent_version: None,
            latency_millis: None,
            captures,
            stale: false,
            last_error: None,
            companion: None,
        };
        let started = Instant::now();
        controller
            .start_remote_watchers(workspace_id, &metadata, false)
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "workspace open must not wait for watcher SSH handshakes"
        );
        assert_eq!(
            controller
                .remote_watchers
                .lock()
                .unwrap()
                .get(&workspace_id)
                .unwrap()
                .len(),
            1,
            "100 repositories must use exactly one managed watcher transport"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_to_string(&invocations)
            .map(|value| value.lines().count())
            .unwrap_or_default()
            < 2
        {
            assert!(
                Instant::now() < deadline,
                "watcher did not complete its connection"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            std::fs::read_to_string(&invocations)
                .unwrap()
                .lines()
                .count(),
            2,
            "one bootstrap probe plus one live SSH watcher transport, not one child per repository"
        );
        controller.stop_remote_watchers(workspace_id);
        std::env::remove_var("LOCALREVIEW_TEST_SSH_PROGRAM");
    }

    fn save_fixture_annotation(fixture: &ReviewFixture, body: &str) -> AnnotationView {
        fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "comment".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: body.into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap()
    }

    #[test]
    fn file_and_review_notes_preserve_optional_anchor_semantics() {
        let fixture = review_fixture("fn old() {}\n", "fn new() {}\n");
        let file_note = fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "file_note".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 0,
                end_line: 0,
                body: "This whole file needs follow-up.".into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        assert_eq!(file_note.file_id, fixture.file_id.to_string());
        assert_eq!(file_note.start_line, 0);

        let review_note = fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                // This is routing context only; the persisted review note is
                // intentionally anchorless.
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "review_note".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 0,
                end_line: 0,
                body: "Check the overall rollout plan.".into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        assert!(review_note.file_id.is_empty());
        assert_eq!(review_note.start_line, 0);

        let session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        let set = fixture
            .controller
            .state()
            .active_annotation_set(session.id)
            .unwrap()
            .unwrap();
        let persisted = fixture.controller.state().annotations(set.id).unwrap();
        assert!(persisted.iter().any(|annotation| {
            annotation.kind == AnnotationKind::FileNote
                && annotation
                    .anchor
                    .as_ref()
                    .is_some_and(|anchor| anchor.side.is_none())
        }));
        assert!(persisted.iter().any(|annotation| {
            annotation.kind == AnnotationKind::ReviewNote && annotation.anchor.is_none()
        }));

        let checkpoint = fixture
            .controller
            .archive_annotations(fixture.workspace_id)
            .unwrap();
        let restored = fixture
            .controller
            .restore_annotations(
                fixture.workspace_id,
                checkpoint.annotations.expect("checkpoint annotations"),
            )
            .unwrap();
        assert!(restored
            .annotations
            .iter()
            .any(|annotation| annotation.kind == "file_note" && annotation.start_line == 0));
        assert!(restored.annotations.iter().any(|annotation| {
            annotation.kind == "review_note" && annotation.file_id.is_empty()
        }));
    }

    #[test]
    fn settings_are_bounded_without_scaling_the_window() {
        let directory = TempDir::new().unwrap();
        let controller = DesktopController::new(StateStore::open(directory.path()).unwrap());
        let settings = controller
            .save_settings(ReviewSettings {
                font_scale: 99.0,
                left_width: 1,
                right_width: 99_999,
                ..ReviewSettings::default()
            })
            .unwrap();
        assert_eq!(settings.font_scale, 2.0);
        assert_eq!(settings.left_width, 180);
        assert_eq!(settings.right_width, 520);
        assert!(controller
            .save_settings(ReviewSettings {
                theme: "sepia".into(),
                ..ReviewSettings::default()
            })
            .is_err());
        assert!(controller
            .save_settings(ReviewSettings {
                tab_width: 3,
                ..ReviewSettings::default()
            })
            .is_err());
        assert!(controller
            .save_settings(ReviewSettings {
                prompt_path_style: "private-cache".into(),
                ..ReviewSettings::default()
            })
            .is_err());
    }

    #[test]
    fn archived_workspace_can_be_reopened_with_its_captured_review_intact() {
        let fixture = review_fixture("before\n", "after\n");
        let state_root = fixture.controller.state().root().to_path_buf();
        let before = fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap();
        assert_eq!(
            fixture.controller.list_archived_workspaces().unwrap().len(),
            0
        );

        fixture
            .controller
            .archive_workspace(fixture.workspace_id)
            .unwrap();
        assert!(fixture.controller.list_workspaces().unwrap().is_empty());
        let archived = fixture.controller.list_archived_workspaces().unwrap();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].archived);
        assert_eq!(archived[0].progress.total, before.files.len());
        assert!(fixture.controller.local_watchers.lock().unwrap().is_empty());
        assert!(matches!(
            fixture
                .controller
                .focus_workspace(&fixture.workspace_id.to_string()),
            Err(DispatchError::Invalid(message)) if message.contains("archived")
        ));
        assert!(matches!(
            fixture.controller.focus_workspace(&before.workspace.name),
            Err(DispatchError::NotFound(_))
        ));

        // Archival, captured documents, annotations, and the ability to
        // recover them are durable database state rather than process-local
        // rail state.
        let restarted = DesktopController::new(StateStore::open(state_root).unwrap());
        let archived_after_restart = restarted.list_archived_workspaces().unwrap();
        assert_eq!(archived_after_restart.len(), 1);
        assert_eq!(
            archived_after_restart[0].id,
            fixture.workspace_id.to_string()
        );
        let reopened = restarted
            .reopen_archived_workspace(fixture.workspace_id)
            .unwrap();
        assert!(!reopened.archived);
        let after = restarted.load_review(fixture.workspace_id).unwrap();
        assert_eq!(after.files.len(), before.files.len());
        assert_eq!(after.files[0].id, before.files[0].id);
        assert_eq!(after.files[0].path, before.files[0].path);
        assert_eq!(after.annotations.len(), before.annotations.len());
        assert_eq!(restarted.list_workspaces().unwrap().len(), 1);
        assert!(restarted.list_archived_workspaces().unwrap().is_empty());
    }

    #[test]
    fn permanent_workspace_delete_purges_reviews_exports_drafts_backups_and_unshared_blobs() {
        let fixture = review_fixture("before\n", "after\n");
        let state_root = fixture.controller.state().root().to_path_buf();
        let session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        fixture
            .controller
            .save_annotation(AnnotationView {
                id: "optimistic-comment".into(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "comment".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: "remove this durable feedback".into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        fixture
            .controller
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "feedback".into(),
                    annotation_ids: Vec::new(),
                    portable: None,
                    path_style: Some("absolute".into()),
                    include_diff_hunks: Some(false),
                    include_git_state: Some(false),
                    history_id: None,
                },
            )
            .unwrap();
        fixture
            .controller
            .save_annotation_draft(AnnotationDraft {
                id: "draft".into(),
                workspace_id: fixture.workspace_id.to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "question".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: "unfinished question".into(),
                updated_at: Utc::now().to_rfc3339(),
            })
            .unwrap();
        for key in [
            legacy_annotation_draft_key(fixture.workspace_id),
            remote_workspace_key(fixture.workspace_id),
            legacy_remote_workspace_key(fixture.workspace_id),
        ] {
            fixture
                .controller
                .state()
                .set_setting(&key, r#"{"owned":true}"#)
                .unwrap();
        }
        fixture
            .controller
            .save_settings(ReviewSettings {
                last_workspace_id: Some(fixture.workspace_id.to_string()),
                ..ReviewSettings::default()
            })
            .unwrap();

        let database = rusqlite::Connection::open(state_root.join("state.sqlite")).unwrap();
        let blob_hash: String = database
            .query_row(
                "SELECT blob_hash FROM review_file WHERE blob_hash IS NOT NULL LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(database);
        let blob_path = state_root
            .join("blobs")
            .join(&blob_hash[..2])
            .join(&blob_hash[2..]);
        assert!(blob_path.is_file());
        let backup = fixture.controller.state().backup_now().unwrap();
        // Pre-migration recovery snapshots can legitimately predate the
        // retired-worktree table. Permanent deletion must scrub those older
        // snapshots too instead of failing halfway through the operation.
        let old_backup = rusqlite::Connection::open(&backup.path).unwrap();
        old_backup
            .execute_batch(
                "DROP TABLE retired_managed_worktree;
                 PRAGMA user_version = 5;",
            )
            .unwrap();
        drop(old_backup);

        fixture
            .controller
            .delete_workspace(fixture.workspace_id)
            .unwrap();

        assert!(fixture
            .controller
            .state()
            .workspace(fixture.workspace_id)
            .unwrap()
            .is_none());
        assert!(fixture.controller.list_workspaces().unwrap().is_empty());
        assert!(fixture
            .controller
            .list_archived_workspaces()
            .unwrap()
            .is_empty());
        assert!(matches!(
            fixture.controller.load_review(fixture.workspace_id),
            Err(DispatchError::NotFound(_))
        ));
        assert!(!blob_path.exists());
        assert!(fixture
            ._workspace_directory
            .path()
            .join("review.rs")
            .is_file());
        assert_eq!(
            fixture.controller.get_settings().unwrap().last_workspace_id,
            None
        );
        for key in [
            annotation_draft_key(session.id),
            legacy_annotation_draft_key(fixture.workspace_id),
            remote_workspace_key(fixture.workspace_id),
            legacy_remote_workspace_key(fixture.workspace_id),
        ] {
            assert_eq!(fixture.controller.state().setting(&key).unwrap(), None);
        }

        for database_path in [state_root.join("state.sqlite"), backup.path] {
            let database = rusqlite::Connection::open(database_path).unwrap();
            let workspace_count: i64 = database
                .query_row(
                    "SELECT COUNT(*) FROM workspace WHERE id = ?1",
                    [fixture.workspace_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let review_count: i64 = database
                .query_row(
                    "SELECT COUNT(*) FROM review_session WHERE workspace_id = ?1",
                    [fixture.workspace_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let annotation_count: i64 = database
                .query_row("SELECT COUNT(*) FROM annotation", [], |row| row.get(0))
                .unwrap();
            let export_count: i64 = database
                .query_row("SELECT COUNT(*) FROM prompt_export", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                (
                    workspace_count,
                    review_count,
                    annotation_count,
                    export_count
                ),
                (0, 0, 0, 0)
            );
        }
    }

    #[test]
    fn opening_archived_local_path_reactivates_and_focuses_its_captured_review() {
        let fixture = review_fixture("before\n", "after\n");
        let git = std::process::Command::new("git")
            .current_dir(fixture._workspace_directory.path())
            .args(["init", "-b", "main"])
            .output()
            .unwrap();
        assert!(
            git.status.success(),
            "git init: {}",
            String::from_utf8_lossy(&git.stderr)
        );
        // `open_local_workspace` deliberately keys local workspaces by their
        // canonical root. The synthetic review fixture predates discovery, so
        // normalize its temporary macOS `/var` alias the same way a real first
        // open does before exercising repeated-open behavior.
        let mut stored_workspace = fixture
            .controller
            .state()
            .workspace(fixture.workspace_id)
            .unwrap()
            .unwrap();
        stored_workspace.source = WorkspaceSource::LocalDirectory {
            root: StoredPath::new(
                std::fs::canonicalize(fixture._workspace_directory.path()).unwrap(),
            ),
        };
        fixture
            .controller
            .state()
            .upsert_workspace(&stored_workspace)
            .unwrap();
        let before = fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap();
        fixture
            .controller
            .archive_workspace(fixture.workspace_id)
            .unwrap();
        assert!(fixture.controller.list_workspaces().unwrap().is_empty());

        let (reopened, created) = fixture
            .controller
            .open_local_workspace(OpenWorkspaceInput {
                path: fixture
                    ._workspace_directory
                    .path()
                    .to_string_lossy()
                    .into_owned(),
                base: None,
                repository_bases: Vec::new(),
            })
            .unwrap();

        assert!(!created);
        assert_eq!(reopened.id, fixture.workspace_id.to_string());
        assert!(!reopened.archived);
        assert_eq!(fixture.controller.list_workspaces().unwrap().len(), 1);
        assert!(fixture
            .controller
            .list_archived_workspaces()
            .unwrap()
            .is_empty());
        let after = fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap();
        assert_eq!(after.files.len(), before.files.len());
        assert_eq!(after.files[0].id, before.files[0].id);
        assert!(fixture
            .controller
            .local_watchers
            .lock()
            .unwrap()
            .contains_key(&fixture.workspace_id));
    }

    #[test]
    fn older_saved_settings_gain_all_new_presentation_defaults() {
        let directory = TempDir::new().unwrap();
        let controller = DesktopController::new(StateStore::open(directory.path()).unwrap());
        controller
            .state()
            .set_setting(
                SETTINGS_KEY,
                r#"{"fontScale":1.2,"leftWidth":250,"rightWidth":340,"leftCollapsed":false,"rightCollapsed":true,"fetchOnReview":false}"#,
            )
            .unwrap();
        let restored = controller.get_settings().unwrap();
        assert_eq!(restored.last_workspace_id, None);
        assert_eq!(restored.theme, "dark");
        assert_eq!(restored.code_font, "SF Mono");
        assert_eq!(restored.tab_width, 2);
        assert!(!restored.wrap_lines);
        assert!(!restored.shortcuts.is_empty());
    }

    #[test]
    fn presentation_jobs_keep_independent_files_concurrent_and_cancel_stale_generations() {
        let mut jobs = PresentationJobRegistry::default();
        let first_file = ReviewFileId::new();
        let second_file = ReviewFileId::new();

        let first = jobs.acquire(first_file, 4).unwrap();
        let second = jobs.acquire(second_file, 9).unwrap();
        assert!(jobs.is_current(&first));
        assert!(jobs.is_current(&second));
        assert!(
            !first.is_cancelled(),
            "a job for a different file must not be serialized through or cancel this file"
        );

        let replacement = jobs.acquire(first_file, 5).unwrap();
        assert!(first.is_cancelled());
        assert!(!jobs.is_current(&first));
        assert!(jobs.is_current(&replacement));
        assert!(jobs.is_current(&second));
        assert!(matches!(
            jobs.acquire(first_file, 4),
            Err(DispatchError::Cancelled)
        ));
    }

    #[test]
    fn presentation_job_registry_and_worker_pool_are_bounded_and_cancellation_aware() {
        let mut jobs = PresentationJobRegistry::default();
        let first_file = ReviewFileId::new();
        let first = jobs.acquire(first_file, 1).unwrap();
        for _ in 0..MAX_PRESENTATION_JOBS {
            jobs.acquire(ReviewFileId::new(), 1).unwrap();
        }
        assert!(jobs.jobs.len() <= MAX_PRESENTATION_JOBS);
        assert!(jobs.order.len() <= MAX_PRESENTATION_JOBS);
        assert!(first.is_cancelled(), "evicted job work must be stopped");

        let pool = PresentationWorkPool::new(2);
        let left = pool.acquire(None).unwrap();
        let right = pool.acquire(None).unwrap();
        assert_eq!(pool.available(), 0, "two independent workers are admitted");
        let cancelled = PresentationJobLease::ephemeral("fixture".into(), 1);
        cancelled.highlight.cancel();
        assert!(matches!(
            pool.acquire(Some(&cancelled)),
            Err(DispatchError::Cancelled)
        ));
        drop(left);
        drop(right);
        assert_eq!(pool.available(), 2, "permits are always returned");
    }

    #[test]
    fn canonical_presentation_cache_remains_bounded_across_view_states() {
        let fixture = review_fixture("fn before() {}\n", "fn after() {}\n");
        let document = fixture
            .controller
            .state()
            .review_file_payload::<PersistedReviewDocument>(fixture.file_id)
            .unwrap()
            .unwrap()
            .document;
        for index in 0..20 {
            let mut state = ReviewUiState::default();
            // Unknown hunk IDs are harmless but participate in the durable
            // view-state key, exercising the same LRU path as real expansion.
            state.hunk_context_lines.insert(format!("view-{index}"), 3);
            fixture
                .controller
                .cached_canonical_rows(&document, "unified", FullFileView::New, &state)
                .unwrap();
        }
        let cache = fixture.controller.presentation_cache.lock().unwrap();
        assert!(cache.canonical.len() <= 16);
        assert!(cache.canonical_order.len() <= 16);
        assert!(cache.canonical_rows <= 100_000);
    }

    #[test]
    fn large_full_file_presentations_reuse_one_shared_projection() {
        let unchanged = "let cached = true;\n".repeat(12_100);
        let old_source = format!("let version = 1;\n{unchanged}");
        let new_source = format!("let version = 2;\n{unchanged}");
        let fixture = review_fixture(&old_source, &new_source);
        let document = fixture
            .controller
            .state()
            .review_file_payload::<PersistedReviewDocument>(fixture.file_id)
            .unwrap()
            .unwrap()
            .document;
        let state = ReviewUiState::default();
        let first_document = fixture
            .controller
            .cached_persisted_review_document(fixture.file_id, Some(document.comparison_id))
            .unwrap();
        let second_document = fixture
            .controller
            .cached_persisted_review_document(fixture.file_id, Some(document.comparison_id))
            .unwrap();

        let first = fixture
            .controller
            .cached_canonical_rows(&document, "full", FullFileView::New, &state)
            .unwrap();
        let second = fixture
            .controller
            .cached_canonical_rows(&document, "full", FullFileView::New, &state)
            .unwrap();

        assert!(first.rows.len() > 12_000);
        assert!(
            Arc::ptr_eq(&first_document, &second_document),
            "viewport requests must not reread and deserialize the immutable document"
        );
        assert!(
            Arc::ptr_eq(&first, &second),
            "viewport requests must share the cached full-file projection"
        );
    }

    #[test]
    fn advanced_review_metadata_stays_read_only_and_capture_bound() {
        let fixture = review_fixture(
            "fn before() {}\n",
            "// @generated by fixture\nfn after() {}\n",
        );
        let classifications = fixture
            .controller
            .review_file_classifications(fixture.workspace_id)
            .unwrap();
        assert_eq!(classifications.len(), 1);
        assert_eq!(classifications[0].file_id, fixture.file_id.to_string());
        assert!(classifications[0].classification.generated);

        let changed = fixture
            .controller
            .changed_since_previous_review(fixture.workspace_id, fixture.repository_id.to_string())
            .unwrap();
        assert!(changed.previous_comparison_id.is_none());
        assert!(changed.files.is_empty());

        let loaded = fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap();
        assert_eq!(
            loaded.repositories[0]
                .comparison_options
                .as_ref()
                .map(|options| options.ignore_all_whitespace),
            Some(false)
        );
        // The feature rejects a non-local attribution request before it can
        // ever read a remote path; the local fixture itself remains unchanged.
        assert_eq!(loaded.files[0].path, "review.rs");
        assert_eq!(loaded.files[0].hunk_count, 1);
    }

    #[test]
    fn canonical_windows_are_bounded_highlighted_and_expand_real_source_context() {
        let old = (1..=80)
            .map(|line| format!("fn value_{line}() -> usize {{ {line} }}\n"))
            .collect::<String>();
        let new = old.replace(
            "fn value_40() -> usize { 40 }",
            "fn value_40() -> usize { 400 }",
        );
        let fixture = review_fixture(&old, &new);
        let resources = TempDir::new().unwrap();
        let first = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "unified".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 7,
                    full_file_side: Some("new".into()),
                    split_ratio: Some(0.5),
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(first.generation, 7);
        assert!(first.rows.len() <= 2_048);
        assert_eq!(first.highlight_status, "highlighted");
        assert!(!first.new_tokens.is_empty());
        let hunk = first.hunks.first().unwrap();
        let initial_rows = first.total_rows;
        fixture
            .controller
            .expand_hunk_context(fixture.file_id, None, &hunk.id, 30)
            .unwrap();
        let expanded = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "unified".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 8,
                    full_file_side: None,
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert!(expanded.total_rows > initial_rows);
        assert!(expanded
            .rows
            .iter()
            .any(|row| row.id.starts_with("expanded:")));

        let full = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 20,
                    end_row: u32::MAX,
                    generation: 9,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(full.start_row, 20);
        assert_eq!(full.total_rows, 81);
        assert_eq!(full.rows.len(), 61);
        assert!(full
            .rows
            .iter()
            .filter(|row| row.new_line.is_some())
            .all(|row| row.new_source_start_byte.is_some()));
        assert!(full.rows.iter().any(|row| {
            row.kind == "deletion_gate"
                && row.old_line == Some(40)
                && row.omitted_end_line == Some(40)
                && row.old_source_start_byte.is_none()
        }));
        assert!(full.old_tokens.is_empty());
        let outline = fixture
            .controller
            .outline(fixture.file_id, None, DiffSide::New)
            .unwrap();
        assert!(outline.iter().any(|symbol| symbol.name == "value_40"));
    }

    #[test]
    fn full_file_current_windows_interleave_removals_and_resolve_old_side_anchors() {
        let old = (1..=30)
            .map(|line| format!("const VALUE_{line}: usize = {line};\n"))
            .collect::<String>();
        let new = old
            .lines()
            .enumerate()
            .filter(|(index, _)| !matches!(index + 1, 5..=7 | 20..=21))
            .map(|(_, line)| format!("{line}\n"))
            .collect::<String>();
        let fixture = review_fixture(&old, &new);
        fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "comment".into(),
                state: "open".into(),
                side: "old".into(),
                start_line: 6,
                end_line: 6,
                body: "Hidden deletion annotation".into(),
                selected_source: "const VALUE_6: usize = 6;".into(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        let resources = TempDir::new().unwrap();
        let full = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 10,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();

        assert_eq!(full.total_rows, 27);
        assert_eq!(full.hunks.len(), 2);
        let gates = full
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind == "deletion_gate")
            .map(|(index, row)| (index, row.old_line, row.omitted_end_line, row.omitted_count))
            .collect::<Vec<_>>();
        assert_eq!(
            gates,
            vec![
                (4, Some(5), Some(7), Some(3)),
                (17, Some(20), Some(21), Some(2)),
            ]
        );
        assert_eq!(full.omitted_blocks.len(), 2);
        assert!(full.omitted_blocks.iter().all(|block| !block.expanded));
        assert!(full.rows[4].has_annotation);
        assert!(!full.rows[17].has_annotation);
        assert_eq!(
            full.hunks
                .iter()
                .map(|hunk| hunk.row_index)
                .collect::<Vec<_>>(),
            vec![4, 17]
        );
        assert!(full
            .rows
            .iter()
            .filter(|row| row.new_line.is_some())
            .all(|row| {
                row.new_source_start_byte.is_some() && row.old_source_start_byte.is_none()
            }));
        assert!(!full.new_tokens.is_empty());

        let location = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::Old,
                20,
                resources.path(),
            )
            .unwrap();
        assert_eq!(location.row_index, 17);

        let mut expanded_ids = full
            .omitted_blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        expanded_ids.sort();
        let ephemeral = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 11,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: Some(expanded_ids.clone()),
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert!(ephemeral.omitted_blocks.iter().all(|block| block.expanded));
        assert!(
            fixture
                .controller
                .workspace_ui_state(fixture.workspace_id)
                .unwrap()
                .expanded_full_file_deletion_blocks
                .is_empty(),
            "a presentation-only expansion must not mutate durable session UI state"
        );
        assert!(fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({
                    "expandedFullFileDeletionBlocks": ["foreign:block"]
                }))
                .unwrap(),
            )
            .is_err());
        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({
                    "expandedFullFileDeletionBlocks": expanded_ids.clone()
                }))
                .unwrap(),
            )
            .unwrap();
        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        assert_eq!(
            reopened
                .workspace_ui_state(fixture.workspace_id)
                .unwrap()
                .expanded_full_file_deletion_blocks,
            expanded_ids
        );
        let expanded = reopened
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 12,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        // Each expanded block retains its one-row inline collapse gate.
        assert_eq!(expanded.total_rows, 32);
        assert!(expanded.omitted_blocks.iter().all(|block| block.expanded));
        assert_eq!(
            expanded
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.kind == "deletion")
                .map(|(index, row)| (index, row.old_line))
                .collect::<Vec<_>>(),
            vec![
                (5, Some(5)),
                (6, Some(6)),
                (7, Some(7)),
                (21, Some(20)),
                (22, Some(21)),
            ]
        );
        assert_eq!(
            expanded
                .rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.kind == "deletion_gate")
                .map(|(index, row)| (index, row.omitted_expanded))
                .collect::<Vec<_>>(),
            vec![(4, Some(true)), (20, Some(true))]
        );
        assert_eq!(
            expanded
                .hunks
                .iter()
                .map(|hunk| hunk.row_index)
                .collect::<Vec<_>>(),
            vec![5, 21]
        );
        let expanded_location = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::Old,
                20,
                resources.path(),
            )
            .unwrap();
        assert_eq!(expanded_location.row_index, 21);
        assert!(!expanded.old_tokens.is_empty());
    }

    #[test]
    fn full_file_base_collapses_additions_and_aligns_both_sides_across_line_shifts() {
        let old = (1..=30)
            .map(|line| format!("let value_{line} = {line};\n"))
            .collect::<String>();
        let mut new = String::new();
        for line in 1..=30 {
            new.push_str(&format!("let value_{line} = {line};\n"));
            if line == 5 {
                new.push_str(
                    "let inserted_a = true;\nlet inserted_b = true;\nlet inserted_c = true;\n",
                );
            }
            if line == 20 {
                new.push_str("let inserted_d = true;\nlet inserted_e = true;\n");
            }
        }
        let fixture = review_fixture(&old, &new);
        let resources = TempDir::new().unwrap();
        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({ "fullFileSide": "old" })).unwrap(),
            )
            .unwrap();
        let base = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 30,
                    full_file_side: Some("old".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        let gates = base
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind == "addition_gate")
            .map(|(index, row)| (index, row.new_line, row.omitted_end_line, row.omitted_count))
            .collect::<Vec<_>>();
        assert_eq!(
            gates,
            vec![
                (5, Some(6), Some(8), Some(3)),
                (21, Some(24), Some(25), Some(2)),
            ]
        );
        assert!(base
            .omitted_blocks
            .iter()
            .all(|block| block.side == "new" && !block.expanded));

        let collapsed_midpoint = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::New,
                7,
                resources.path(),
            )
            .unwrap();
        assert_eq!(collapsed_midpoint.row_index, 5);

        let aligned_base_context = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::New,
                30,
                resources.path(),
            )
            .unwrap();
        assert_eq!(
            base.rows[usize::try_from(aligned_base_context.row_index).unwrap()].old_line,
            Some(25)
        );

        let expanded_ids = base
            .omitted_blocks
            .iter()
            .map(|block| block.id.clone())
            .collect::<Vec<_>>();
        let expanded = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 31,
                    full_file_side: Some("old".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: Some(expanded_ids),
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(
            expanded
                .rows
                .iter()
                .filter(|row| row.kind == "addition")
                .count(),
            5
        );

        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({ "fullFileSide": "new" })).unwrap(),
            )
            .unwrap();
        let aligned_current_context = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::Old,
                25,
                resources.path(),
            )
            .unwrap();
        let current = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: aligned_current_context.row_index,
                    end_row: aligned_current_context.row_index.saturating_add(1),
                    generation: 32,
                    full_file_side: Some("new".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(current.rows[0].new_line, Some(30));
    }

    #[test]
    fn full_file_both_defaults_additions_open_deletions_closed_and_persists_each_control() {
        let fixture = review_fixture(
            "keep\nold a\nold b\ntail\n",
            "keep\nnew a\nnew b\nnew c\ntail\n",
        );
        let resources = TempDir::new().unwrap();
        let both = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 40,
                    full_file_side: Some("both".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        let addition = both
            .omitted_blocks
            .iter()
            .find(|block| block.side == "new")
            .unwrap();
        let deletion = both
            .omitted_blocks
            .iter()
            .find(|block| block.side == "old")
            .unwrap();
        let total_additions = both
            .omitted_blocks
            .iter()
            .filter(|block| block.side == "new")
            .map(|block| usize::try_from(block.count).unwrap())
            .sum::<usize>();
        assert!(addition.expanded);
        assert!(!deletion.expanded);
        assert_eq!(
            both.rows
                .iter()
                .filter(|row| row.kind == "addition")
                .count(),
            total_additions
        );
        assert_eq!(
            both.rows
                .iter()
                .filter(|row| row.kind == "deletion")
                .count(),
            0
        );

        let addition_id = addition.id.clone();
        let deletion_id = deletion.id.clone();
        let collapsed_in_place = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 41,
                    full_file_side: Some("both".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: Some(vec![]),
                    ephemeral_collapsed_full_file_addition_blocks: Some(vec![addition_id.clone()]),
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(
            collapsed_in_place
                .rows
                .iter()
                .filter(|row| row.kind == "addition")
                .count(),
            total_additions.saturating_sub(usize::try_from(addition.count).unwrap())
        );
        let expanded_in_place = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 42,
                    full_file_side: Some("both".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: Some(vec![]),
                    ephemeral_collapsed_full_file_addition_blocks: Some(vec![]),
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(
            expanded_in_place
                .rows
                .iter()
                .filter(|row| row.kind == "addition")
                .count(),
            total_additions
        );
        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({
                    "fullFileSide": "both",
                    "expandedFullFileDeletionBlocks": [deletion_id],
                    "collapsedFullFileAdditionBlocks": [addition_id]
                }))
                .unwrap(),
            )
            .unwrap();
        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        let state = reopened.workspace_ui_state(fixture.workspace_id).unwrap();
        assert_eq!(state.full_file_side, "both");
        assert_eq!(state.expanded_full_file_deletion_blocks, vec![deletion_id]);
        assert_eq!(state.collapsed_full_file_addition_blocks, vec![addition_id]);
        let toggled = reopened
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "full".into(),
                    start_row: 0,
                    end_row: u32::MAX,
                    generation: 43,
                    full_file_side: Some("both".into()),
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                resources.path(),
            )
            .unwrap();
        assert_eq!(
            toggled
                .rows
                .iter()
                .filter(|row| row.kind == "addition")
                .count(),
            total_additions.saturating_sub(usize::try_from(addition.count).unwrap())
        );
        assert_eq!(
            toggled
                .rows
                .iter()
                .filter(|row| row.kind == "deletion")
                .count(),
            usize::try_from(deletion.count).unwrap()
        );
    }

    #[test]
    fn presentation_locations_are_resolved_against_complete_native_rows() {
        let old = (1..=240)
            .map(|line| format!("fn value_{line}() -> usize {{ {line} }}\n"))
            .collect::<String>();
        let new = old.replace(
            "fn value_180() -> usize { 180 }",
            "fn value_180() -> usize { 1_800 }",
        );
        let fixture = review_fixture(&old, &new);
        let resources = TempDir::new().unwrap();

        let unified = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "unified",
                DiffSide::New,
                180,
                resources.path(),
            )
            .unwrap();
        let full = fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::New,
                240,
                resources.path(),
            )
            .unwrap();

        assert_eq!((unified.side.as_str(), unified.line), ("new", 180));
        assert!(unified.row_index > 0);
        // Both is the Full File default. The replacement contributes one
        // collapsed deletion gate plus one expanded addition gate before the
        // Current source row, while retaining the source coordinate.
        assert_eq!(full.row_index, 241);
        assert!(fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "unified",
                DiffSide::New,
                0,
                resources.path(),
            )
            .is_err());
        assert!(fixture
            .controller
            .resolve_presentation_location(
                fixture.file_id,
                None,
                "full",
                DiffSide::New,
                241,
                resources.path(),
            )
            .is_err());
    }

    #[test]
    fn captured_source_ranges_are_exact_and_never_return_partial_text() {
        let fixture = review_fixture("old\n", "first line\nα second line\nthird line\n");
        let captured = fixture
            .controller
            .captured_source_range(fixture.file_id, None, DiffSide::New, 2, 3)
            .unwrap();
        assert!(captured.complete);
        assert_eq!(captured.text, "α second line\nthird line\n");

        let incomplete = fixture
            .controller
            .captured_source_range(fixture.file_id, None, DiffSide::New, 2, 4)
            .unwrap();
        assert!(!incomplete.complete);
        assert!(incomplete.text.is_empty());
        assert!(fixture
            .controller
            .captured_source_range(fixture.file_id, None, DiffSide::New, 0, 1)
            .is_err());
    }

    #[test]
    fn annotation_drafts_survive_restart_validate_ownership_and_clear() {
        let fixture = review_fixture("fn old() {}\n", "fn new() {}\n");
        let draft = AnnotationDraft {
            id: "draft-1".into(),
            workspace_id: fixture.workspace_id.to_string(),
            file_id: fixture.file_id.to_string(),
            repository_id: fixture.repository_id.to_string(),
            kind: "question".into(),
            side: "new".into(),
            start_line: 1,
            end_line: 1,
            // A line range is meaningful restart state before the reviewer
            // has typed the first character.
            body: String::new(),
            updated_at: Utc::now().to_rfc3339(),
        };
        fixture
            .controller
            .save_annotation_draft(draft.clone())
            .unwrap();

        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        assert_eq!(
            reopened.annotation_draft(fixture.workspace_id).unwrap(),
            Some(draft.clone())
        );

        let mut wrong_repository = draft.clone();
        wrong_repository.repository_id = RepositoryId::new().to_string();
        assert!(reopened.save_annotation_draft(wrong_repository).is_err());
        let mut invalid_range = draft;
        invalid_range.end_line = 2;
        assert!(reopened.save_annotation_draft(invalid_range).is_err());

        reopened
            .clear_annotation_draft(fixture.workspace_id)
            .unwrap();
        assert_eq!(
            reopened.annotation_draft(fixture.workspace_id).unwrap(),
            None
        );
    }

    #[test]
    fn multiple_review_sessions_keep_frozen_data_and_session_state_across_restart() {
        let fixture = review_fixture("before one\n", "after one\n");
        let first_session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        let first_annotation = save_fixture_annotation(&fixture, "feedback from review one");
        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({
                    "activeFileId": fixture.file_id.to_string(),
                    "mode": "split",
                    "scrollTop": 410.0,
                    "selectedAnnotationIds": [first_annotation.id]
                }))
                .unwrap(),
            )
            .unwrap();
        fixture
            .controller
            .save_annotation_draft(AnnotationDraft {
                id: "first-session-draft".into(),
                workspace_id: fixture.workspace_id.to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "comment".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: "unfinished in review one".into(),
                updated_at: Utc::now().to_rfc3339(),
            })
            .unwrap();
        let first_export = fixture
            .controller
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: None,
                },
            )
            .unwrap();

        let (second_session, second_file_id) =
            replace_fixture_review(&fixture, "before two\n", "after two\n");
        assert_eq!(
            fixture
                .controller
                .service
                .active_review_session(fixture.workspace_id)
                .unwrap()
                .unwrap()
                .id,
            second_session.id
        );
        assert!(fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap()
            .annotations
            .is_empty());
        assert_eq!(
            fixture
                .controller
                .workspace_ui_state(fixture.workspace_id)
                .unwrap()
                .mode,
            "unified"
        );
        assert_eq!(
            fixture
                .controller
                .annotation_draft(fixture.workspace_id)
                .unwrap(),
            None,
            "a workspace-level draft must not leak into its replacement session"
        );
        let second_annotation = fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                file_id: second_file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "question".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: "feedback from review two".into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({
                    "activeFileId": second_file_id.to_string(),
                    "mode": "full",
                    "scrollTop": 99.0,
                    "selectedAnnotationIds": [second_annotation.id]
                }))
                .unwrap(),
            )
            .unwrap();
        let second_export = fixture
            .controller
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: None,
                },
            )
            .unwrap();

        let (third_session, third_file_id) =
            replace_fixture_review(&fixture, "before three\n", "after three\n");
        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        let sessions = reopened
            .state()
            .review_sessions(fixture.workspace_id)
            .unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.status == ReviewSessionStatus::Active)
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            vec![third_session.id]
        );
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.status == ReviewSessionStatus::Archived)
                .count(),
            2
        );
        let history = reopened.history(fixture.workspace_id).unwrap();
        let review_ids = history
            .iter()
            .filter(|item| item.item_type == "review")
            .map(|item| item.id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            review_ids,
            BTreeSet::from([
                format!("review:{}", first_session.id),
                format!("review:{}", second_session.id),
            ])
        );

        let frozen_first = reopened
            .load_archived_review(
                fixture.workspace_id,
                &format!("review:{}", first_session.id),
            )
            .unwrap();
        let frozen_second = reopened
            .load_archived_review(
                fixture.workspace_id,
                &format!("review:{}", second_session.id),
            )
            .unwrap();
        assert_eq!(frozen_first.files[0].id, fixture.file_id.to_string());
        assert_eq!(frozen_first.annotations[0].body, "feedback from review one");
        assert_eq!(frozen_second.files[0].id, second_file_id.to_string());
        assert_eq!(
            frozen_second.annotations[0].body,
            "feedback from review two"
        );
        let active = reopened.load_review(fixture.workspace_id).unwrap();
        assert_eq!(active.files[0].id, third_file_id.to_string());
        assert!(active.annotations.is_empty());
        assert_eq!(
            reopened
                .workspace_ui_state(fixture.workspace_id)
                .unwrap()
                .mode,
            "unified"
        );
        assert_eq!(
            reopened
                .state()
                .review_session_ui_state::<ReviewUiState>(first_session.id)
                .unwrap()
                .unwrap()
                .mode
                .as_deref(),
            Some("split")
        );
        assert_eq!(
            reopened
                .state()
                .review_session_ui_state::<ReviewUiState>(second_session.id)
                .unwrap()
                .unwrap()
                .mode
                .as_deref(),
            Some("full")
        );
        for export in [first_export, second_export] {
            let exact = reopened
                .generate_prompt(
                    fixture.workspace_id,
                    PromptInput {
                        scope: "feedback".into(),
                        annotation_ids: Vec::new(),
                        portable: Some(false),
                        path_style: None,
                        include_diff_hunks: None,
                        include_git_state: None,
                        history_id: Some(format!("export:{}", export.export_id)),
                    },
                )
                .unwrap();
            assert_eq!(exact.content, export.content);
        }
    }

    #[test]
    fn structural_presentation_choices_survive_controller_restart() {
        let fixture = review_fixture("fn old() {}\n", "fn new() {}\n");
        fixture
            .controller
            .save_workspace_ui_state(
                fixture.workspace_id,
                serde_json::from_value(serde_json::json!({
                    "activeFileId": fixture.file_id.to_string(),
                    "mode": "difftastic",
                    "fullFileSide": "old",
                    "scrollTop": 321.0,
                    "splitRatio": 0.63,
                    "rightTab": "outline"
                }))
                .unwrap(),
            )
            .unwrap();

        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        let restored = reopened.workspace_ui_state(fixture.workspace_id).unwrap();
        assert_eq!(restored.active_file_id, Some(fixture.file_id.to_string()));
        assert_eq!(restored.mode, "difftastic");
        assert_eq!(restored.full_file_side, "old");
        assert_eq!(restored.scroll_top, 321.0);
        assert_eq!(restored.split_ratio, 0.63);
        assert_eq!(restored.right_tab, "outline");
    }

    #[test]
    fn explicit_annotation_inclusion_survives_restart_including_an_empty_selection() {
        let fixture = review_fixture("fn old() {}\n", "fn new() {}\n");
        let saved = fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "comment".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: "Durable inclusion choice".into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        let selected_patch = serde_json::from_value(serde_json::json!({
            "selectedAnnotationIds": [saved.id]
        }))
        .unwrap();
        fixture
            .controller
            .save_workspace_ui_state(fixture.workspace_id, selected_patch)
            .unwrap();

        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        assert_eq!(
            reopened
                .workspace_ui_state(fixture.workspace_id)
                .unwrap()
                .selected_annotation_ids,
            Some(vec![saved.id.clone()])
        );

        let empty_patch = serde_json::from_value(serde_json::json!({
            "selectedAnnotationIds": []
        }))
        .unwrap();
        reopened
            .save_workspace_ui_state(fixture.workspace_id, empty_patch)
            .unwrap();
        let reopened_empty =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        assert_eq!(
            reopened_empty
                .workspace_ui_state(fixture.workspace_id)
                .unwrap()
                .selected_annotation_ids,
            Some(Vec::new())
        );

        let foreign_patch = serde_json::from_value(serde_json::json!({
            "selectedAnnotationIds": [AnnotationId::new().to_string()]
        }))
        .unwrap();
        assert!(reopened_empty
            .save_workspace_ui_state(fixture.workspace_id, foreign_patch)
            .is_err());
    }

    #[test]
    fn restoring_a_checkpoint_allocates_new_ids_and_keeps_archived_rows_immutable() {
        let fixture = review_fixture("fn old() {}\n", "fn new() {}\n");
        let saved = fixture
            .controller
            .save_annotation(AnnotationView {
                id: Uuid::new_v4().to_string(),
                file_id: fixture.file_id.to_string(),
                repository_id: fixture.repository_id.to_string(),
                kind: "comment".into(),
                state: "open".into(),
                side: "new".into(),
                start_line: 1,
                end_line: 1,
                body: "Keep this immutable".into(),
                selected_source: String::new(),
                labels: Vec::new(),
                local_only: true,
                created_at: Utc::now().to_rfc3339(),
                published_id: None,
            })
            .unwrap();
        let checkpoint = fixture
            .controller
            .archive_annotations(fixture.workspace_id)
            .unwrap();
        let archived_set = parse_history_set_id(&checkpoint.id).unwrap();
        let archived_before = fixture
            .controller
            .state()
            .annotations(archived_set)
            .unwrap();
        let restored = fixture
            .controller
            .restore_annotations(
                fixture.workspace_id,
                checkpoint.annotations.clone().unwrap(),
            )
            .unwrap();
        assert_eq!(restored.annotations.len(), 1);
        assert_ne!(restored.annotations[0].id, saved.id);
        assert_eq!(restored.annotations[0].body, "Keep this immutable");
        assert_eq!(
            fixture
                .controller
                .state()
                .annotations(archived_set)
                .unwrap(),
            archived_before
        );
        fixture
            .controller
            .delete_annotation(
                fixture.workspace_id,
                parse_annotation_id(&restored.annotations[0].id).unwrap(),
            )
            .unwrap();
        assert!(fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap()
            .annotations
            .is_empty());
        assert_eq!(
            fixture
                .controller
                .state()
                .annotations(archived_set)
                .unwrap(),
            archived_before
        );
    }

    #[test]
    fn source_offsets_and_urls_preserve_unicode_boundaries() {
        let old = "α\nlet value = 1;\n";
        let new = "α\nlet value = 2;\n";
        let document = document_from_sources(
            ComparisonId::new(),
            ReviewFile {
                id: ReviewFileId::new(),
                path: StoredPath::from("space and #/💡.rs"),
                old_path: None,
                status: ReviewFileStatus::Modified,
            },
            old,
            new,
        );
        let presentation = build_canonical_presentation(
            &document,
            "unified",
            FullFileView::New,
            &ReviewUiState::default(),
        )
        .unwrap();
        let changed = presentation
            .rows
            .iter()
            .find(|row| row.new_line == Some(2) && row.kind == "addition")
            .unwrap();
        assert_eq!(changed.new_source_start_byte, Some("α\n".len() as u32));
        assert_eq!(byte_to_utf16_index("a💡b", 5), Some(3));
        assert_eq!(
            github_path("space and #/💡.rs"),
            "space%20and%20%23/%F0%9F%92%A1.rs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copies_use_the_immutable_snapshot_and_editor_open_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let fixture = review_fixture("fn old() {}\n", "fn captured() {}\n");
        let checkout_file = fixture._workspace_directory.path().join("review.rs");
        std::fs::write(&checkout_file, "fn mutated_after_capture() {}\n").unwrap();
        let source = fixture
            .controller
            .copy_review_item(
                fixture.workspace_id,
                CopyReviewItemRequest {
                    kind: "source".into(),
                    file_id: fixture.file_id.to_string(),
                    side: Some("new".into()),
                    start_line: Some(1),
                    end_line: Some(1),
                },
            )
            .unwrap();
        assert_eq!(source, "fn captured() {}\n");
        assert_eq!(
            fixture
                .controller
                .copy_review_item(
                    fixture.workspace_id,
                    CopyReviewItemRequest {
                        kind: "path".into(),
                        file_id: fixture.file_id.to_string(),
                        side: None,
                        start_line: Some(u32::MAX),
                        end_line: Some(u32::MAX),
                    },
                )
                .unwrap(),
            "review.rs"
        );
        assert!(fixture
            .controller
            .open_in_external_editor(fixture.workspace_id, fixture.file_id, Some(0))
            .is_err());

        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&outside_file, "fn outside() {}\n").unwrap();
        std::fs::remove_file(&checkout_file).unwrap();
        symlink(&outside_file, &checkout_file).unwrap();
        assert!(fixture
            .controller
            .open_in_external_editor(fixture.workspace_id, fixture.file_id, Some(1))
            .is_err());
    }

    #[test]
    fn focused_question_scope_requires_exactly_one_annotation() {
        let annotation = AnnotationId::new();
        assert_eq!(
            prompt_scope("focused_question", vec![annotation]).unwrap(),
            localreview_domain::PromptScope::FocusedQuestion(annotation)
        );
        assert!(prompt_scope("focused_question", Vec::new()).is_err());
        assert!(prompt_scope(
            "focused_question",
            vec![AnnotationId::new(), AnnotationId::new()]
        )
        .is_err());
    }

    #[test]
    fn prompt_modes_are_strict_and_history_keeps_scope_labels() {
        let fixture = review_fixture("before\n", "after\n");
        let save = |kind: &str, body: &str| {
            fixture
                .controller
                .save_annotation(AnnotationView {
                    id: Uuid::new_v4().to_string(),
                    file_id: fixture.file_id.to_string(),
                    repository_id: fixture.repository_id.to_string(),
                    kind: kind.into(),
                    state: "open".into(),
                    side: "new".into(),
                    start_line: if matches!(kind, "file_note" | "review_note") {
                        0
                    } else {
                        1
                    },
                    end_line: if matches!(kind, "file_note" | "review_note") {
                        0
                    } else {
                        1
                    },
                    body: body.into(),
                    selected_source: String::new(),
                    labels: Vec::new(),
                    local_only: true,
                    created_at: Utc::now().to_rfc3339(),
                    published_id: None,
                })
                .unwrap()
        };
        save("comment", "comment body");
        save("question", "question body");
        save("file_note", "file note body");
        save("suggestion", "suggestion body");
        save("review_note", "review note body");
        let generate = |scope: &str| {
            fixture
                .controller
                .generate_prompt(
                    fixture.workspace_id,
                    PromptInput {
                        scope: scope.into(),
                        annotation_ids: Vec::new(),
                        portable: None,
                        path_style: Some("absolute".into()),
                        include_diff_hunks: Some(false),
                        include_git_state: Some(false),
                        history_id: None,
                    },
                )
                .unwrap()
        };

        let feedback = generate("feedback");
        assert_eq!(feedback.title, "Review feedback");
        assert_eq!(feedback.annotation_count, 2);
        assert!(feedback.content.contains("comment body"));
        assert!(feedback.content.contains("suggestion body"));
        assert!(!feedback.content.contains("question body"));
        assert!(!feedback.content.contains("file note body"));
        assert!(!feedback.content.contains("review note body"));

        let questions = generate("questions");
        assert_eq!(questions.title, "Questions for investigation");
        assert_eq!(questions.annotation_count, 1);
        assert!(questions.content.contains("# Review questions"));
        assert!(questions.content.contains("Question:\n\nquestion body"));
        assert!(!questions.content.contains("comment body"));

        let full = generate("all");
        assert_eq!(full.title, "Full review prompt");
        assert_eq!(full.annotation_count, 5);
        for body in [
            "comment body",
            "question body",
            "file note body",
            "suggestion body",
            "review note body",
        ] {
            assert!(full.content.contains(body));
        }
        assert!(!full.content.contains("Requested base `"));
        assert!(!full.content.contains("merge-base `"));
        assert!(!full.content.contains("HEAD `"));
        assert!(!full.content.contains("snapshot `"));
        assert!(!full.content.contains("Relevant diff hunk:"));
        let repository = fixture
            .controller
            .state()
            .repositories(fixture.workspace_id)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(full.content.contains(repository.worktree_path.as_str()));
        assert_eq!(full.content.matches("Selected source:").count(), 3);
        assert!(full
            .content
            .contains("Selected source:\n```text\nafter\n```"));

        let verbose = fixture
            .controller
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: None,
                    path_style: Some("qualified".into()),
                    include_diff_hunks: Some(true),
                    include_git_state: Some(true),
                    history_id: None,
                },
            )
            .unwrap();
        assert_eq!(verbose.content.matches("Requested base `").count(), 1);
        assert_eq!(verbose.content.matches("Relevant diff hunk:").count(), 1);
        assert!(!full.content.contains("Surrounding context:"));
        assert!(full.content.contains("/review.rs`"));
        assert!(!full.content.contains("Logical path:"));
        assert!(full.content.contains(
            fixture
                ._workspace_directory
                .path()
                .to_string_lossy()
                .as_ref()
        ));

        let history = fixture.controller.history(fixture.workspace_id).unwrap();
        for expected in [
            "Review feedback",
            "Questions for investigation",
            "Full review prompt",
        ] {
            assert!(history
                .iter()
                .any(|item| { item.item_type == "export" && item.label == expected }));
        }
    }

    #[test]
    fn prompt_history_references_are_explicit_and_workspace_bound() {
        let fixture = review_fixture("before\n", "after\n");
        save_fixture_annotation(&fixture, "Only the original set belongs here.");
        let session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        let set = fixture
            .controller
            .state()
            .active_annotation_set(session.id)
            .unwrap()
            .unwrap();
        let source = PromptInput {
            scope: "all".into(),
            annotation_ids: Vec::new(),
            portable: Some(true),
            path_style: None,
            include_diff_hunks: None,
            include_git_state: None,
            history_id: Some(format!("set:{}", set.id)),
        };
        assert!(fixture
            .controller
            .generate_prompt(fixture.workspace_id, source)
            .unwrap()
            .content
            .contains("Only the original set belongs here."));
        for history_id in [
            "no-prefix".to_owned(),
            format!("set:{}", AnnotationSetId::new()),
            format!("review:{}", ReviewSessionId::new()),
            format!("export:{}", PromptExportId::new()),
        ] {
            assert!(fixture
                .controller
                .generate_prompt(
                    fixture.workspace_id,
                    PromptInput {
                        scope: "all".into(),
                        annotation_ids: Vec::new(),
                        portable: Some(true),
                        path_style: None,
                        include_diff_hunks: None,
                        include_git_state: None,
                        history_id: Some(history_id),
                    },
                )
                .is_err());
        }
        let mut other = fixture
            .controller
            .state()
            .workspace(fixture.workspace_id)
            .unwrap()
            .unwrap();
        other.id = WorkspaceId::new();
        other.display_name = "other workspace".into();
        fixture.controller.state().upsert_workspace(&other).unwrap();
        assert!(fixture
            .controller
            .generate_prompt(
                other.id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: Some(format!("set:{}", set.id)),
                },
            )
            .is_err());
    }

    #[test]
    fn archived_review_prompt_aggregates_its_original_sets_and_pinned_snapshot() {
        let fixture = review_fixture("before\n", "after\n");
        save_fixture_annotation(&fixture, "First checkpoint feedback.");
        fixture
            .controller
            .archive_annotations(fixture.workspace_id)
            .unwrap();
        save_fixture_annotation(&fixture, "Second checkpoint feedback.");
        let session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        fixture
            .controller
            .state()
            .archive_review_session(session.clone(), Utc::now())
            .unwrap();
        let preview = fixture
            .controller
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: Some(format!("review:{}", session.id)),
                },
            )
            .unwrap();
        assert!(preview.content.contains("First checkpoint feedback."));
        assert!(preview.content.contains("Second checkpoint feedback."));
        let export = fixture
            .controller
            .state()
            .prompt_export(PromptExportId(Uuid::parse_str(&preview.export_id).unwrap()))
            .unwrap()
            .unwrap();
        assert_eq!(export.review_session_id, session.id);
        assert_eq!(export.annotation_set_ids.len(), 2);
    }

    #[test]
    fn archived_review_session_exposes_frozen_diff_and_annotations_read_only() {
        let fixture = review_fixture("before\n", "after\n");
        save_fixture_annotation(&fixture, "Archived inline feedback.");
        let session = fixture
            .controller
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        fixture
            .controller
            .state()
            .archive_review_session(session.clone(), Utc::now())
            .unwrap();

        let review = fixture
            .controller
            .load_archived_review(fixture.workspace_id, &format!("review:{}", session.id))
            .unwrap();
        assert!(review.historical);
        let expected_session_id = session.id.to_string();
        assert_eq!(
            review.historical_session_id.as_deref(),
            Some(expected_session_id.as_str())
        );
        assert_eq!(review.files.len(), 1);
        assert_eq!(review.annotations.len(), 1);
        let window = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "unified".into(),
                    start_row: 0,
                    end_row: 50,
                    generation: 1,
                    full_file_side: None,
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                Path::new("."),
            )
            .unwrap();
        assert!(window.rows.iter().any(|row| row.has_annotation));
        assert!(matches!(
            fixture
                .controller
                .expand_hunk_context(fixture.file_id, None, &window.hunks[0].id, 24),
            Err(DispatchError::Invalid(message)) if message.contains("archived review")
        ));
    }

    #[test]
    fn prompt_exports_reopen_exactly_after_restart_and_legacy_records_regenerate_safely() {
        let fixture = review_fixture("before\n", "after\n");
        let saved = save_fixture_annotation(&fixture, "Frozen handoff bytes.");
        let first = fixture
            .controller
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "all".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: None,
                },
            )
            .unwrap();
        let mut changed = saved;
        changed.body = "Changed after export.".into();
        let changed_id = changed.id.clone();
        fixture.controller.save_annotation(changed).unwrap();
        let reopened =
            DesktopController::new(StateStore::open(fixture._state_directory.path()).unwrap());
        let exact = reopened
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "feedback".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(false),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: Some(format!("export:{}", first.export_id)),
                },
            )
            .unwrap();
        assert_eq!(exact.content, first.content);
        assert_eq!(exact.title, first.title);
        assert_eq!(exact.annotation_count, first.annotation_count);
        assert!(reopened
            .history(fixture.workspace_id)
            .unwrap()
            .iter()
            .any(|item| {
                item.id == format!("export:{}", first.export_id)
                    && item.label == first.title
                    && item.annotation_count == first.annotation_count
            }));

        let session = reopened
            .service
            .active_review_session(fixture.workspace_id)
            .unwrap()
            .unwrap();
        let set = reopened
            .state()
            .active_annotation_set(session.id)
            .unwrap()
            .unwrap();
        let legacy = PromptExportRecord {
            id: PromptExportId::new(),
            review_session_id: session.id,
            annotation_set_id: set.id,
            annotation_set_ids: Vec::new(),
            scope: PromptScope::AllActionable,
            annotation_ids: vec![AnnotationId(Uuid::parse_str(&changed_id).unwrap())],
            template_version: 1,
            rendered_markdown: None,
            title: None,
            annotation_count: None,
            estimated_tokens: None,
            created_at: Utc::now(),
        };
        reopened.state().save_prompt_export(&legacy).unwrap();
        let regenerated = reopened
            .generate_prompt(
                fixture.workspace_id,
                PromptInput {
                    scope: "questions".into(),
                    annotation_ids: Vec::new(),
                    portable: Some(true),
                    path_style: None,
                    include_diff_hunks: None,
                    include_git_state: None,
                    history_id: Some(format!("export:{}", legacy.id)),
                },
            )
            .unwrap();
        assert!(regenerated.content.contains("Changed after export."));
        let materialized = reopened
            .state()
            .prompt_export(PromptExportId(
                Uuid::parse_str(&regenerated.export_id).unwrap(),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            materialized.rendered_markdown.as_deref(),
            Some(regenerated.content.as_str())
        );
    }

    #[test]
    fn filesystem_notifications_only_mark_refresh_available() {
        use std::time::{Duration, Instant};

        let fixture = review_fixture("fn old() {}\n", "fn captured() {}\n");
        let root = fixture._workspace_directory.path();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "user.name", "Review Test"]);
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        git(&["add", ".gitignore", "review.rs"]);
        git(&["commit", "-m", "base"]);
        let before = fixture
            .controller
            .state()
            .review_file_payload::<PersistedReviewDocument>(fixture.file_id)
            .unwrap()
            .unwrap();
        fixture
            .controller
            .load_review(fixture.workspace_id)
            .unwrap();
        // Ignore setup-time notifications and start this assertion at a
        // stable watcher boundary, as an explicit refresh does in production.
        std::thread::sleep(Duration::from_millis(300));
        fixture
            .controller
            .clear_refresh_available(fixture.workspace_id);
        let ignored_bundle = root.join("target/release/bundle/macos/LocalReview.app");
        std::fs::create_dir_all(&ignored_bundle).unwrap();
        std::fs::write(
            ignored_bundle.join("Contents.cache"),
            "ignored build output\n",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !fixture.controller.list_workspaces().unwrap()[0].refresh_available,
            "ignored build output must not enable explicit Refresh"
        );
        std::fs::write(root.join("review.rs"), "fn changed_after_capture() {}\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let available = loop {
            let available = fixture
                .controller
                .list_workspaces()
                .unwrap()
                .into_iter()
                .find(|workspace| workspace.id == fixture.workspace_id.to_string())
                .unwrap()
                .refresh_available;
            if available || Instant::now() >= deadline {
                break available;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(available, "the OS watcher should enable explicit Refresh");
        let after = fixture
            .controller
            .state()
            .review_file_payload::<PersistedReviewDocument>(fixture.file_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            before.document.new.content, after.document.new.content,
            "a notification must not replace the immutable captured source"
        );
        fixture
            .controller
            .clear_refresh_available(fixture.workspace_id);
        assert!(!fixture.controller.list_workspaces().unwrap()[0].refresh_available);
    }

    #[test]
    fn capture_watcher_epoch_ignores_queued_events_and_preserves_later_events() {
        let fixture = review_fixture("fn old() {}\n", "fn captured() {}\n");
        let workspace = fixture
            .controller
            .state()
            .workspace(fixture.workspace_id)
            .unwrap()
            .unwrap();
        fixture.controller.ensure_local_watcher(&workspace).unwrap();
        fixture
            .controller
            .mark_refresh_available(fixture.workspace_id)
            .unwrap();
        let old_epoch = fixture
            .controller
            .refresh_availability(fixture.workspace_id)
            .unwrap()
            .watcher_epoch;

        let boundary = fixture
            .controller
            .restart_local_watcher_for_capture(&workspace)
            .unwrap();
        publish_refresh_availability(
            &fixture.controller.refresh_available,
            &fixture.controller.app_handle,
            fixture.workspace_id,
            Some(old_epoch),
            true,
        );
        assert_eq!(
            fixture
                .controller
                .refresh_availability(fixture.workspace_id)
                .unwrap()
                .revision,
            boundary.revision,
            "a callback queued by the pre-capture watcher must be ignored"
        );
        fixture
            .controller
            .clear_refresh_available_at_boundary(fixture.workspace_id, boundary);
        assert!(!fixture.controller.list_workspaces().unwrap()[0].refresh_available);

        let later_boundary = fixture
            .controller
            .restart_local_watcher_for_capture(&workspace)
            .unwrap();
        let current_epoch = fixture
            .controller
            .refresh_availability(fixture.workspace_id)
            .unwrap()
            .watcher_epoch;
        publish_refresh_availability(
            &fixture.controller.refresh_available,
            &fixture.controller.app_handle,
            fixture.workspace_id,
            Some(current_epoch),
            true,
        );
        fixture
            .controller
            .clear_refresh_available_at_boundary(fixture.workspace_id, later_boundary);
        let summary = fixture.controller.list_workspaces().unwrap().remove(0);
        assert!(summary.refresh_available);
        assert!(summary.refresh_available_revision > later_boundary.revision);
    }

    #[test]
    fn github_workspace_summary_preserves_revisioned_refresh_availability() {
        let fixture = review_fixture("fn old() {}\n", "fn captured() {}\n");
        let mut workspace = fixture
            .controller
            .state()
            .workspace(fixture.workspace_id)
            .unwrap()
            .unwrap();
        workspace.source = WorkspaceSource::PullRequest {
            url: "https://github.com/acme/project/pull/42".into(),
            owner: "acme".into(),
            repository: "project".into(),
            number: 42,
            worktree: StoredPath::new(fixture._workspace_directory.path()),
        };
        fixture
            .controller
            .state()
            .upsert_workspace(&workspace)
            .unwrap();

        fixture
            .controller
            .mark_refresh_available(fixture.workspace_id)
            .unwrap();
        let summary = fixture
            .controller
            .list_workspaces()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == fixture.workspace_id.to_string())
            .unwrap();

        assert!(summary.refresh_available);
        assert!(summary.refresh_available_revision > 0);
    }

    #[test]
    fn github_round_boundary_and_failed_provider_refresh_keep_retry_available() {
        let workspace_directory = TempDir::new().unwrap();
        let repository = workspace_directory.path();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(repository)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "user.name", "Review Test"]);
        std::fs::write(repository.join("review.rs"), "fn old() {}\n").unwrap();
        git(&["add", "review.rs"]);
        git(&["commit", "-m", "base"]);
        git(&["switch", "-c", "feature"]);
        std::fs::write(repository.join("review.rs"), "fn captured() {}\n").unwrap();

        let state_directory = TempDir::new().unwrap();
        let controller = DesktopController::new(StateStore::open(state_directory.path()).unwrap());
        let (opened, _) = controller
            .open_local_workspace(OpenWorkspaceInput {
                path: repository.to_string_lossy().into_owned(),
                base: Some("main".into()),
                repository_bases: Vec::new(),
            })
            .unwrap();
        let workspace_id = parse_workspace_id(&opened.id).unwrap();
        let mut workspace = controller.state().workspace(workspace_id).unwrap().unwrap();
        workspace.source = WorkspaceSource::PullRequest {
            url: "https://github.com/acme/project/pull/42".into(),
            owner: "acme".into(),
            repository: "project".into(),
            number: 42,
            worktree: StoredPath::new(repository),
        };
        controller.state().upsert_workspace(&workspace).unwrap();

        controller.mark_refresh_available(workspace_id).unwrap();
        let started = controller
            .start_new_review(workspace_id, StartOrRefreshInput::default())
            .unwrap();
        assert!(started.workspace.refresh_available);

        controller.clear_refresh_available(workspace_id);
        let error = controller
            .refresh_review(workspace_id, StartOrRefreshInput::default())
            .unwrap_err();
        assert!(matches!(error, DispatchError::Service(_)));
        assert!(
            controller
                .list_workspaces()
                .unwrap()
                .into_iter()
                .find(|candidate| candidate.id == workspace_id.to_string())
                .unwrap()
                .refresh_available
        );
    }

    #[test]
    fn local_watcher_ignores_git_plumbing_but_keeps_source_generations() {
        assert!(!local_event_can_change_source(Path::new(
            "/repo/.git/index.lock"
        )));
        assert!(!local_event_can_change_source(Path::new(
            "/repo/.git/objects/ab/cdef"
        )));
        assert!(!local_event_can_change_source(Path::new(
            "/repo/.git/logs/HEAD"
        )));
        assert!(local_event_can_change_source(Path::new("/repo/.git/index")));
        assert!(local_event_can_change_source(Path::new(
            "/repo/.git/refs/heads/main"
        )));
        assert!(local_event_can_change_source(Path::new(
            "/repo/.git/info/exclude"
        )));
        assert!(local_event_can_change_source(Path::new(
            "/repo/src/main.rs"
        )));
    }

    #[test]
    fn local_watcher_uses_git_ignore_and_index_semantics_across_event_shapes() {
        use notify::event::{AccessKind, MetadataKind, ModifyKind, RenameMode};

        let repository = TempDir::new().unwrap();
        let root = repository.path();
        let git = |root: &Path, arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.email", "review@example.invalid"]);
        git(root, &["config", "user.name", "Review Test"]);
        std::fs::write(root.join(".gitignore"), "target/\n*.cache\n*.log\n").unwrap();
        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        std::fs::write(root.join("tracked.log"), "tracked despite ignore\n").unwrap();
        git(root, &["add", ".gitignore", "tracked.txt"]);
        git(root, &["add", "-f", "tracked.log"]);
        git(root, &["commit", "-m", "base"]);

        let metadata_event = |path: PathBuf| {
            notify::Event::new(EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::Extended,
            )))
            .add_path(path)
        };
        let roots = vec![root.to_path_buf()];
        let ignored_bundle = root.join("target/release/bundle/macos/LocalReview.app");
        std::fs::create_dir_all(&ignored_bundle).unwrap();
        assert!(
            !local_events_can_change_source(&roots, &[metadata_event(ignored_bundle)]),
            "an ignored build product must not relight Refresh"
        );

        let untracked = root.join("src/new.rs");
        std::fs::create_dir_all(untracked.parent().unwrap()).unwrap();
        std::fs::write(&untracked, "fn new() {}\n").unwrap();
        assert!(local_events_can_change_source(
            &roots,
            &[metadata_event(untracked)]
        ));

        std::fs::write(root.join("tracked.log"), "modified\n").unwrap();
        assert!(
            local_events_can_change_source(&roots, &[metadata_event(root.join("tracked.log"))]),
            "tracked files stay relevant even when an ignore rule matches"
        );
        std::fs::remove_file(root.join("tracked.txt")).unwrap();
        assert!(
            local_events_can_change_source(
                &roots,
                &[
                    notify::Event::new(EventKind::Remove(notify::event::RemoveKind::File))
                        .add_path(root.join("tracked.txt"))
                ]
            ),
            "tracked deletions must relight Refresh"
        );

        let ignored_rename =
            notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(root.join("before.cache"))
                .add_path(root.join("after.cache"));
        assert!(!local_events_can_change_source(&roots, &[ignored_rename]));
        let relevant_rename =
            notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                .add_path(root.join("before.cache"))
                .add_path(root.join("after.rs"));
        assert!(local_events_can_change_source(&roots, &[relevant_rename]));
        assert!(!local_events_can_change_source(
            &roots,
            &[notify::Event::new(EventKind::Access(AccessKind::Any))
                .add_path(root.join("tracked.log"))]
        ));

        let nested = TempDir::new_in(root).unwrap();
        let nested_root = nested.path();
        git(nested_root, &["init", "-b", "main"]);
        std::fs::write(nested_root.join(".gitignore"), "generated/\n").unwrap();
        let nested_generated = nested_root.join("generated/output.rs");
        std::fs::create_dir_all(nested_generated.parent().unwrap()).unwrap();
        assert!(
            !local_events_can_change_source(
                &[root.to_path_buf(), nested_root.to_path_buf()],
                &[metadata_event(nested_generated)]
            ),
            "the deepest registered repository must own ignore evaluation"
        );
    }

    #[test]
    fn read_only_local_capture_emits_no_semantic_filesystem_events() {
        use std::time::{Duration, Instant};

        let repository = TempDir::new().unwrap();
        let root = repository.path();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(arguments)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "user.name", "Review Test"]);
        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-m", "base"]);
        git(&["switch", "-c", "feature"]);
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        watcher.watch(root, RecursiveMode::Recursive).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        while receiver.try_recv().is_ok() {}

        let git = localreview_git::GitRepository::open(root);
        let resolved = git
            .resolve_comparison(
                RepositoryId::new(),
                ComparisonId::new(),
                BaseReference::new("main").unwrap(),
                ComparisonOptions::default(),
            )
            .unwrap();
        git.capture_local_comparison(resolved).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = Vec::new();
        while Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => observed.push(event),
                Ok(Err(error)) => panic!("filesystem watcher failed: {error}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let semantic = observed
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    EventKind::Create(_)
                        | EventKind::Modify(_)
                        | EventKind::Remove(_)
                        | EventKind::Any
                ) && event
                    .paths
                    .iter()
                    .any(|path| local_event_can_change_source(path))
            })
            .collect::<Vec<_>>();
        assert!(
            semantic.is_empty(),
            "read-only capture emitted semantic filesystem events: {semantic:#?}; all events: {observed:#?}"
        );
    }

    #[test]
    fn github_imported_context_uses_the_webview_camel_case_contract() {
        let context = GitHubPullRequestContextView {
            canonical_url: "https://github.com/acme/repo/pull/42".into(),
            title: "Review this".into(),
            author: Some("octocat".into()),
            base_ref: "main".into(),
            head_ref: "feature".into(),
            pinned_base_sha: "a".repeat(40),
            pinned_head_sha: "b".repeat(40),
            draft: false,
            state: "OPEN".into(),
            review_decision: Some("CHANGES_REQUESTED".into()),
            commits: vec![GitHubCommitContextView {
                sha: "c".repeat(40),
                message_headline: "Use stable capture".into(),
                authored_at: Some("2026-07-22T00:00:00+00:00".into()),
            }],
            import_error: None,
        };
        let thread = ImportedGitHubReviewThreadView {
            id: "thread-1".into(),
            resolved: true,
            outdated: false,
            path: Some("src/main.rs".into()),
            line: Some(42),
            original_line: Some(40),
            comments: vec![ImportedGitHubReviewCommentView {
                id: "comment-1".into(),
                body_markdown: "Please simplify this.".into(),
                author: Some("octocat".into()),
                url: None,
                created_at: None,
            }],
        };
        let context = serde_json::to_value(context).unwrap();
        let thread = serde_json::to_value(thread).unwrap();
        assert_eq!(
            context["canonicalUrl"],
            "https://github.com/acme/repo/pull/42"
        );
        assert_eq!(context["pinnedHeadSha"], "b".repeat(40));
        assert!(context.get("canonical_url").is_none());
        assert_eq!(thread["originalLine"], 40);
        assert_eq!(
            thread["comments"][0]["bodyMarkdown"],
            "Please simplify this."
        );
        assert!(thread["comments"][0].get("body_markdown").is_none());
    }

    #[test]
    fn review_notes_keep_their_distinct_webview_kind() {
        assert_eq!(
            parse_annotation_kind("review_note").unwrap(),
            AnnotationKind::ReviewNote
        );
        assert_eq!(
            annotation_kind_name(AnnotationKind::ReviewNote),
            "review_note"
        );
        assert_ne!(
            annotation_kind_name(AnnotationKind::ReviewNote),
            annotation_kind_name(AnnotationKind::FileNote)
        );
    }

    #[test]
    fn repository_setup_controls_are_scoped_strict_and_keep_failures_on_their_own_row() {
        let fixture = review_fixture("fn before() {}\n", "fn after() {}\n");
        let initial = fixture
            .controller
            .repository_setup(fixture.workspace_id)
            .unwrap();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].path, ".");
        assert_eq!(initial[0].effective_base, "origin/master");
        assert_eq!(initial[0].base_source, "inherited");
        assert!(
            initial[0].merge_base_sha.is_some(),
            "pinned fallback remains visible"
        );
        assert!(
            initial[0].comparison_error.is_some(),
            "a bad sibling is a row error, not a table failure"
        );

        let disabled = fixture
            .controller
            .set_repository_inclusion(
                fixture.workspace_id,
                SetRepositoryInclusionInput {
                    repository_ids: vec![fixture.repository_id.to_string()],
                    enabled: false,
                },
            )
            .unwrap();
        assert!(!disabled[0].enabled);

        let overridden = fixture
            .controller
            .apply_repository_base(
                fixture.workspace_id,
                ApplyRepositoryBaseInput {
                    repository_ids: vec![fixture.repository_id.to_string()],
                    base: "origin/release-1".into(),
                },
            )
            .unwrap();
        assert_eq!(overridden[0].effective_base, "origin/release-1");
        assert_eq!(overridden[0].base_source, "override");

        let inherited = fixture
            .controller
            .reset_repository_base_overrides(
                fixture.workspace_id,
                RepositorySelectionInput {
                    repository_ids: vec![fixture.repository_id.to_string()],
                },
            )
            .unwrap();
        assert_eq!(inherited[0].effective_base, "origin/master");
        assert_eq!(inherited[0].base_source, "inherited");

        let invalid = fixture.controller.apply_repository_base(
            fixture.workspace_id,
            ApplyRepositoryBaseInput {
                repository_ids: vec![fixture.repository_id.to_string()],
                base: "origin/main^".into(),
            },
        );
        assert!(matches!(invalid, Err(DispatchError::Invalid(_))));

        let fetched = fixture
            .controller
            .fetch_repositories(
                fixture.workspace_id,
                Some(vec![fixture.repository_id.to_string()]),
            )
            .unwrap();
        assert!(fetched[0].last_fetch_at.is_some());
        assert!(fetched[0].last_fetch_error.is_some());
        assert!(matches!(
            fixture.controller.set_repository_inclusion(
                fixture.workspace_id,
                SetRepositoryInclusionInput {
                    repository_ids: vec![
                        fixture.repository_id.to_string(),
                        fixture.repository_id.to_string()
                    ],
                    enabled: true,
                },
            ),
            Err(DispatchError::Invalid(_))
        ));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn packaged_pinned_difftastic_flows_through_the_native_window_contract() {
        let resource_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
        let sidecar = resource_dir.join("localreview-sidecars/difft");
        if !sidecar.is_file() {
            // The generated 237 MB artifact is intentionally absent from a
            // source checkout. Packaging CI provisions it before this smoke.
            return;
        }
        let fixture = review_fixture("fn value() -> usize { 1 }\n", "fn value() -> usize { 2 }\n");
        let response = fixture
            .controller
            .presentation_window(
                PresentationRequest {
                    file_id: fixture.file_id.to_string(),
                    comparison_id: None,
                    mode: "difftastic".into(),
                    start_row: 0,
                    end_row: 100,
                    generation: 11,
                    full_file_side: None,
                    split_ratio: Some(0.5),
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                &resource_dir,
            )
            .unwrap();
        let structural = response.difftastic.expect("structural payload");
        assert_eq!(response.mode, "difftastic");
        assert!(
            structural.fallback.is_none(),
            "packaged sidecar must not fall back"
        );
        assert!(!structural.chunks.is_empty());
        assert!(structural.total_rows > 0);
    }
}
