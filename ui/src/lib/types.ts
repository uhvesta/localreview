export type WorkspaceSource = 'local' | 'github' | 'ssh';
export type DiffMode = 'unified' | 'split' | 'full' | 'difftastic';
export type DiffKind = 'context' | 'addition' | 'deletion' | 'modification' | 'header';
export type DiffSide = 'old' | 'new';
export type FullFileSide = DiffSide;
export type SyntaxClass =
  | 'attribute' | 'boolean' | 'comment' | 'constant' | 'constructor' | 'embedded'
  | 'escape' | 'function' | 'keyword' | 'markup' | 'module' | 'number'
  | 'operator' | 'property' | 'punctuation' | 'string' | 'tag' | 'type' | 'variable';
export type ThemePreference = 'dark' | 'light' | 'system';
export type FileGrouping = 'repository' | 'folder' | 'flat';
export type FileSort = 'path' | 'repository' | 'change_size' | 'annotations' | 'review_order';
export type ViewedFilter = 'all' | 'viewed' | 'unviewed';
/** Capture-time, immutable review-file facts. More than one can apply. */
export type FileClassificationFilter = 'all' | 'text' | 'generated' | 'vendored' | 'lockfile' | 'binary' | 'lfs_pointer' | 'submodule';
export type AnnotationKind = 'comment' | 'question' | 'suggestion' | 'file_note' | 'review_note';
export type AnnotationState = 'open' | 'resolved' | 'outdated';
export type ReviewConclusion = 'comment' | 'approve' | 'request_changes';

export interface Workspace {
  id: string;
  name: string;
  source: WorkspaceSource[];
  location: string;
  detail: string;
  /** Durable default used for inherited repository baselines. */
  defaultBase?: string;
  progress: { viewed: number; total: number };
  draftCount: number;
  pinned?: boolean;
  refreshAvailable?: boolean;
  connection?: 'connected' | 'connecting' | 'offline' | 'error';
  /** Native sends false when discovery is durable but baseline setup has not
   * produced an initial review generation yet. */
  reviewReady?: boolean;
  /** Present only while browsing recoverable workspace history. */
  archived?: boolean;
}

export interface Repository {
  id: string;
  name: string;
  path: string;
  branch: string;
  base: string;
  mergeBase: string;
  head: string;
  isOverride?: boolean;
  /** Options used by this immutable current comparison, if captured natively. */
  comparisonOptions?: ComparisonOptions;
}

/** Fresh setup metadata. Unlike `Repository`, this is not part of a pinned
 * review generation and may be explicitly re-read without refreshing it. */
export interface RepositorySetup {
  id: string;
  path: string;
  enabled: boolean;
  branch: string;
  clean?: boolean;
  changedFileCount?: number;
  statusSummary: string;
  effectiveBase: string;
  baseSource: 'temporary' | 'override' | 'inherited' | 'application default' | string;
  baseOverride?: string;
  resolvedBaseSha?: string;
  mergeBaseSha?: string;
  headSha?: string;
  ahead?: number;
  behind?: number;
  lastFetchAt?: string;
  lastFetchError?: string;
  discoveryError?: string;
  comparisonError?: string;
  statusCheckedAt?: string;
}

export interface ReviewFile {
  id: string;
  /** Stable file IDs may be reused by later review sessions. */
  comparisonId?: string;
  repositoryId: string;
  path: string;
  previousPath?: string;
  status: 'modified' | 'added' | 'deleted' | 'renamed' | 'untracked';
  additions: number;
  deletions: number;
  language: string;
  viewed: boolean;
  annotationCount: number;
  /** Never inferred from the mutable checkout by the browser. */
  classification?: ReviewFileClassification;
}

export interface ReviewFileClassification {
  generated: boolean;
  vendored: boolean;
  lockfile: boolean;
  binary: boolean;
  lfsPointer: boolean;
  submodule: boolean;
}

export interface ReviewFileClassificationRecord {
  comparisonId: string;
  fileId: string;
  path: string;
  classification: ReviewFileClassification;
}

export interface DiffRow {
  id: string;
  kind: DiffKind;
  /** Stable hunk identity; never a virtual-list index. */
  hunkId?: string;
  oldLine?: number;
  newLine?: number;
  oldText?: string;
  newText?: string;
  text?: string;
  hunk?: string;
  hasAnnotation?: boolean;
  /** UTF-8 source byte offsets for safe complete-document Tree-sitter spans. */
  oldSourceStartByte?: number;
  newSourceStartByte?: number;
}

/** A Rust Tree-sitter span. It is rendered as text nodes, never injected HTML. */
export interface SyntaxTokenSpan {
  startByte: number;
  endByte: number;
  class: SyntaxClass;
}

export interface HunkLocation {
  id: string;
  rowIndex: number;
  oldLine?: number;
  newLine?: number;
  header: string;
  collapsedContextLines?: number;
}

export interface DiffPresentationWindow {
  /** Monotonic native job generation; stale responses are ignored by the UI. */
  generation: number;
  mode: DiffMode;
  fileId: string;
  /** Zero-based row index represented by rows[0]. */
  startRow: number;
  totalRows: number;
  rows: DiffRow[];
  hunks: HunkLocation[];
  oldTokens?: SyntaxTokenSpan[];
  newTokens?: SyntaxTokenSpan[];
  highlightStatus?: 'highlighted' | 'plain_text' | 'disabled';
  highlightReason?: string;
  /** Only populated for read-only Difftastic responses. */
  difftastic?: DifftasticPresentation;
}

export interface ViewportRequest {
  fileId: string;
  /** Immutable generation owning this stable logical file ID. */
  comparisonId?: string;
  mode: DiffMode;
  startRow: number;
  endRow: number;
  generation: number;
  fullFileSide?: FullFileSide;
  splitRatio?: number;
}

export interface DifftasticSpan {
  start: number;
  end: number;
  highlight: 'delimiter' | 'normal' | 'string' | 'type' | 'comment' | 'keyword' | 'tree_sitter_error';
}

export interface DifftasticCell {
  lineNumber?: number;
  text: string;
  changedSpans: DifftasticSpan[];
}

export interface DifftasticRow {
  old?: DifftasticCell;
  new?: DifftasticCell;
}

export interface DifftasticChunk { rows: DifftasticRow[]; }

export interface DifftasticPresentation {
  status: 'unchanged' | 'changed' | 'created' | 'deleted';
  display: 'inline' | 'side_by_side';
  /** Bounded structural rows represented by this response. */
  startRow?: number;
  totalRows?: number;
  chunks: DifftasticChunk[];
  alignment: Array<{ oldLine?: number; newLine?: number }>;
  fallback?: { reason: string; detail?: string };
}

export interface OutlineSymbol {
  id: string;
  name: string;
  kind: 'function' | 'method' | 'class' | 'struct' | 'enum' | 'interface' | 'module' | 'heading' | 'property' | 'unknown';
  startLine: number;
  endLine: number;
  depth: number;
  side: DiffSide;
}

export interface DiffSelection {
  side: DiffSide;
  startLine: number;
  endLine: number;
}

/** Native, presentation-authoritative mapping used for distant annotation,
 * outline, history and Difftastic-to-canonical jumps. */
export interface PresentationLocation {
  rowIndex: number;
  side: DiffSide;
  line: number;
}

/** The complete captured source for an annotation range. `complete` is false
 * when the immutable source cannot be read; the UI must not save a truncated
 * range assembled from virtual windows. */
export interface CapturedSourceRange {
  text: string;
  complete: boolean;
}

export interface Annotation {
  id: string;
  fileId: string;
  repositoryId: string;
  kind: AnnotationKind;
  state: AnnotationState;
  side: DiffSide;
  startLine: number;
  endLine: number;
  body: string;
  selectedSource: string;
  labels: string[];
  localOnly: boolean;
  createdAt: string;
  publishedId?: string;
}

export interface AnnotationDraft {
  id: string;
  workspaceId: string;
  fileId: string;
  repositoryId: string;
  kind: AnnotationKind;
  side: DiffSide;
  startLine: number;
  endLine: number;
  body: string;
  updatedAt: string;
}

export interface ReviewHistoryItem {
  id: string;
  label: string;
  createdAt: string;
  annotationCount: number;
  type: 'clear' | 'review' | 'export';
  /** Present for browser fixtures and native history entries that can be restored. */
  annotations?: Annotation[];
}

export interface ReviewData {
  workspace: Workspace;
  repositories: Repository[];
  files: ReviewFile[];
  annotations: Annotation[];
  history: ReviewHistoryItem[];
  /** A frozen prior review session; its annotation/diff records are browsed read-only. */
  historical?: boolean;
  historicalSessionId?: string;
}

export interface ReviewSettings {
  fontScale: number;
  leftWidth: number;
  rightWidth: number;
  leftCollapsed: boolean;
  rightCollapsed: boolean;
  fetchOnReview: boolean;
  theme: ThemePreference;
  codeFont: string;
  externalEditor: 'system' | 'vscode' | 'cursor' | 'zed' | 'sublime' | 'idea';
  tabWidth: number;
  showWhitespace: boolean;
  vimNavigation: boolean;
  shortcuts: Record<string, string>;
}

export interface WorkspaceUiState {
  activeFileId?: string;
  mode: DiffMode;
  fullFileSide: FullFileSide;
  nearestSourceLine?: number;
  nearestSourceSide?: DiffSide;
  scrollTop: number;
  splitRatio: number;
  rightTab: 'files' | 'comments' | 'outline';
  /** Undefined means a legacy/default selection; an empty array is an explicit durable choice. */
  selectedAnnotationIds?: string[];
}

export interface PromptRequest {
  scope: 'feedback' | 'questions' | 'all' | 'selected' | 'focused_question';
  annotationIds?: string[];
  portable?: boolean;
  /** Export an archived annotation checkpoint without modifying the active set. */
  historyId?: string;
}

export interface PromptPreview {
  /** Durable native record; save actions never re-submit webview Markdown. */
  exportId: string;
  title: string;
  content: string;
  annotationCount: number;
  estimatedTokens: number;
}

export type PromptExportSaveFormat = 'markdown' | 'json';

export interface SavedPromptExport {
  saved: boolean;
  format: string;
}

export interface FinishReviewRequest {
  annotationIds: string[];
  summary: string;
  conclusion: ReviewConclusion;
}

/** A server-authoritative dry run of the single native GitHub review payload. */
export interface FinishReviewPreview {
  annotationCount: number;
  /** Exact durable annotation batch represented by this preview, including recovery after restart. */
  annotationIds: string[];
  payloadJson: string;
  pinnedHeadSha: string;
  /** Opaque durable capability; this is the only value accepted on submit. */
  previewToken: string;
  requestFingerprint: string;
  previewRequestFingerprint: string;
  annotationSnapshotFingerprint: string;
  requiresReconciliation: boolean;
}

export interface FinishReviewSubmission {
  previewToken: string;
}

export interface FinishReviewResult {
  reviewId: string;
  annotationCount: number;
  annotationIds: string[];
  payloadJson: string;
  /** The durable attempt actually completed; it may predate the current dialog after restart reconciliation. */
  previewToken: string;
  publicationStatus: 'submitted' | 'reconciled';
  submitted: boolean;
}

export interface RepositoryBaseOverride {
  repositoryId?: string;
  repositoryPath?: string;
  /** `null` explicitly returns the repository to the inherited workspace base. */
  base?: string | null;
}

export interface ReviewCaptureRequest {
  base?: string;
  repositoryBases?: RepositoryBaseOverride[];
  fetchBeforeCapture?: boolean;
  /** These options change the captured Git comparison, never just rendering. */
  comparisonOptions?: ComparisonOptions;
}

export interface ComparisonOptions {
  ignoreAllWhitespace: boolean;
  ignoreSpaceAtEol: boolean;
  ignoreCrAtEol: boolean;
}

export interface BlameLine {
  revision: string;
  originalLine: number;
  finalLine: number;
  sourcePath: string;
  authorName: string;
  authorEmail: string;
  authorTime: string;
  summary: string;
  source: string;
  sourceTruncated: boolean;
}

export interface CapturedBlameResult {
  comparisonId: string;
  side: DiffSide;
  lines: BlameLine[];
}

export interface CommitSummary {
  sha: string;
  parentShas: string[];
  authorName: string;
  authorEmail: string;
  authoredAt: string;
  subject: string;
}

export interface CommitDetails {
  summary: CommitSummary;
  committerName: string;
  committerEmail: string;
  committedAt: string;
  body: string;
  bodyTruncated: boolean;
}

export interface CommitContextRequest {
  repositoryId: string;
  maxEntries?: number;
  includeMergeCommits?: boolean;
  authorContains?: string;
  subjectContains?: string;
  selectedCommit?: string;
}

export interface CapturedCommitContext {
  comparisonId: string;
  range: { mergeBase: string; head: string };
  commits: CommitSummary[];
  truncated: boolean;
  selectedCommit?: CommitDetails;
}

export type PreviousReviewFileChangeKind = 'added' | 'removed' | 'renamed' | 'modified' | 'unchanged';

export interface PreviousReviewFileComparison {
  kind: PreviousReviewFileChangeKind;
  path: string;
  previousPath?: string;
  currentFileId?: string;
  previousFileId?: string;
  currentDocumentFingerprint?: string;
  previousDocumentFingerprint?: string;
}

export interface ChangedSincePreviousReview {
  currentComparisonId: string;
  previousComparisonId?: string;
  files: PreviousReviewFileComparison[];
  truncated: boolean;
}

/** Volatile provider status only. It never promotes new GitHub revisions. */
export interface GitHubPullRequestUpdateStatus {
  workspaceId: string;
  canonicalUrl: string;
  pinnedBaseSha: string;
  pinnedHeadSha: string;
  currentBaseSha: string;
  currentHeadSha: string;
  baseChanged: boolean;
  headChanged: boolean;
  metadataFetchedAt: string;
}

/** Imported GitHub state stays separate from unpublished local annotations. */
export interface GitHubPullRequestContext {
  canonical_url: string;
  title: string;
  author?: string;
  base_ref: string;
  head_ref: string;
  pinned_base_sha: string;
  pinned_head_sha: string;
  draft: boolean;
  state: string;
  review_decision?: string;
  commits: Array<{ sha: string; message_headline: string; authored_at?: string }>;
  import_error?: string;
}

export interface ImportedGitHubReviewThread {
  id: string;
  resolved: boolean;
  outdated: boolean;
  path?: string;
  line?: number;
  original_line?: number;
  comments: Array<{ id: string; body_markdown: string; author?: string; url?: string; created_at?: string; review?: { state?: string; author?: string } }>;
}

export interface ImportedGitHubConversationComment {
  id: number;
  body_markdown: string;
  author?: string;
  url?: string;
  created_at?: string;
}

export interface OpenWorkspaceRequest {
  path: string;
  base?: string;
  repositoryBases?: Array<{ repositoryPath: string; base: string }>;
}

export interface PersistenceDiagnostics {
  databaseHealthy: boolean;
  integrityDiagnostic: string;
  recoverableBackupCount: number;
  backupStorage: {
    retainedCount: number;
    retainedBytes: number;
    newestBackupAt?: string;
    oldestBackupAt?: string;
    exceedsSizePreference: boolean;
    policy: { maxBackups: number; maxTotalBytes?: number };
  };
}

export interface ReviewApi {
  /** Native only. Browser development intentionally uses the explicit path field. */
  pickLocalFolder(): Promise<{ path?: string }>;
  openWorkspace(request: OpenWorkspaceRequest): Promise<Workspace>;
  openGitHubPr(url: string): Promise<Workspace>;
  openSshWorkspace(target: string): Promise<Workspace>;
  /** Explicit recovery action; reconnect also restarts the bounded remote watcher. */
  reconnectSshWorkspace(workspaceId: string): Promise<Workspace>;
  listWorkspaces(): Promise<Workspace[]>;
  /** Recoverable workspace snapshots intentionally hidden from the live rail. */
  listArchivedWorkspaces(): Promise<Workspace[]>;
  /** Put an archived snapshot back on the live rail without recapturing it. */
  reopenArchivedWorkspace(workspaceId: string): Promise<Workspace>;
  /** Rename and/or pin a live rail item without changing its review capture. */
  updateWorkspaceMetadata(workspaceId: string, metadata: { name?: string; pinned?: boolean }): Promise<Workspace>;
  /** Source-free persistence health and aggregate backup-storage facts. */
  getPersistenceDiagnostics(): Promise<PersistenceDiagnostics>;
  /** Archives a workspace; GitHub review worktrees are removed only when clean. */
  deleteWorkspace(workspaceId: string): Promise<void>;
  loadReview(workspaceId: string): Promise<ReviewData>;
  /** Opens a native `review:<uuid>` history entry without restoring or recapturing it. */
  loadArchivedReview(workspaceId: string, historyId: string): Promise<ReviewData>;
  getReviewFileClassifications(workspaceId: string): Promise<ReviewFileClassificationRecord[]>;
  /** Attribution is pinned to a revision captured by the active comparison. */
  getCapturedBlame(workspaceId: string, fileId: string, side: DiffSide, startLine: number, endLine: number): Promise<CapturedBlameResult>;
  /** Read-only commit metadata for one captured repository comparison. */
  getCommitContext(workspaceId: string, request: CommitContextRequest): Promise<CapturedCommitContext>;
  getChangedSincePreviousReview(workspaceId: string, repositoryId: string): Promise<ChangedSincePreviousReview>;
  getGitHubUpdateStatus(workspaceId: string): Promise<GitHubPullRequestUpdateStatus>;
  getGitHubPullRequest(workspaceId: string): Promise<GitHubPullRequestContext>;
  getGitHubThreads(workspaceId: string): Promise<ImportedGitHubReviewThread[]>;
  getGitHubConversation(workspaceId: string): Promise<ImportedGitHubConversationComment[]>;
  /**
   * The native path is windowed. It must never serialize an entire 50k-line
   * source just because a Svelte viewport scrolled.
   */
  getPresentationWindow(request: ViewportRequest): Promise<DiffPresentationWindow>;
  resolvePresentationLocation(fileId: string, mode: DiffMode, side: DiffSide, line: number, comparisonId?: string): Promise<PresentationLocation>;
  getCapturedSourceRange(fileId: string, side: DiffSide, startLine: number, endLine: number, comparisonId?: string): Promise<CapturedSourceRange>;
  /** Legacy fixture seam kept only for older browser demos; never used in Tauri. */
  getRows?(fileId: string, mode: DiffMode): Promise<DiffRow[]>;
  expandHunk(fileId: string, hunkId: string, contextLines: number, comparisonId?: string): Promise<void>;
  getOutline(fileId: string, side: DiffSide, comparisonId?: string): Promise<OutlineSymbol[]>;
  /** Workspace scope disambiguates browser fixtures; native file IDs remain globally unique. */
  saveAnnotation(workspaceId: string, annotation: Annotation): Promise<Annotation>;
  getAnnotationDraft(workspaceId: string): Promise<AnnotationDraft | undefined>;
  saveAnnotationDraft(draft: AnnotationDraft): Promise<void>;
  clearAnnotationDraft(workspaceId: string): Promise<void>;
  deleteAnnotation(workspaceId: string, annotationId: string): Promise<void>;
  setAnnotationState(workspaceId: string, annotationId: string, state: AnnotationState): Promise<Annotation>;
  archiveAnnotations(workspaceId: string): Promise<ReviewHistoryItem>;
  /** Restored annotations receive fresh IDs; callers must replace all review data. */
  restoreAnnotations(workspaceId: string, annotations: Annotation[]): Promise<ReviewData>;
  generatePrompt(workspaceId: string, request: PromptRequest): Promise<PromptPreview>;
  /** Native-only user-selected save of the durable `PromptPreview.exportId`. */
  savePromptExport(workspaceId: string, exportId: string, format: PromptExportSaveFormat): Promise<SavedPromptExport>;
  getReviewHistory(workspaceId: string): Promise<ReviewHistoryItem[]>;
  restoreHistoryItem(workspaceId: string, historyId: string): Promise<ReviewData>;
  setViewed(workspaceId: string, fileId: string, viewed: boolean): Promise<void>;
  getRepositorySetup(workspaceId: string): Promise<RepositorySetup[]>;
  setRepositoryInclusion(workspaceId: string, repositoryIds: string[], enabled: boolean): Promise<RepositorySetup[]>;
  applyRepositoryBase(workspaceId: string, repositoryIds: string[], base: string): Promise<RepositorySetup[]>;
  resetRepositoryBaseOverrides(workspaceId: string, repositoryIds: string[]): Promise<RepositorySetup[]>;
  /** Empty/omitted selection fetches every repository, and returns each result. */
  fetchRepositories(workspaceId: string, repositoryIds?: string[]): Promise<RepositorySetup[]>;
  configureBaselines(workspaceId: string, defaultBase?: string, repositoryBases?: RepositoryBaseOverride[]): Promise<ReviewData>;
  startNewReview(workspaceId: string, request?: ReviewCaptureRequest): Promise<ReviewData>;
  refreshReview(workspaceId: string, request?: ReviewCaptureRequest): Promise<ReviewData>;
  previewFinishReview(workspaceId: string, request: FinishReviewRequest): Promise<FinishReviewPreview>;
  finishReview(workspaceId: string, submission: FinishReviewSubmission): Promise<FinishReviewResult>;
  /** Prepared attempts require a separate, explicit user confirmation. */
  abandonFinishReview(workspaceId: string, submission: FinishReviewSubmission, confirmPrepared?: boolean): Promise<void>;
  getSettings(): Promise<ReviewSettings>;
  saveSettings(settings: Partial<ReviewSettings>): Promise<ReviewSettings>;
  getWorkspaceUiState(workspaceId: string): Promise<WorkspaceUiState>;
  saveWorkspaceUiState(workspaceId: string, state: Partial<WorkspaceUiState>): Promise<WorkspaceUiState>;
  copyReviewItem(workspaceId: string, request: CopyRequest): Promise<string>;
  openInExternalEditor(workspaceId: string, fileId: string, line?: number): Promise<void>;
}

export interface CopyRequest {
  kind: 'source' | 'source_with_line_numbers' | 'path' | 'hunk' | 'patch' | 'provider_permalink';
  fileId: string;
  side?: DiffSide;
  startLine?: number;
  endLine?: number;
}
