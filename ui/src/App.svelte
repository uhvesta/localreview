<script lang="ts">
  import { onMount } from 'svelte';
  import { isTauri } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import WorkspaceRail from './lib/WorkspaceRail.svelte';
  import VirtualDiff from './lib/VirtualDiff.svelte';
  import VirtualFileList from './lib/VirtualFileList.svelte';
  import { focusTrap } from './lib/focusTrap';
  import { copyText, createReviewApi } from './lib/api';
  import type { Annotation, AnnotationDraft, AnnotationKind, AnnotationState, CapturedBlameResult, CapturedCommitContext, ChangedSincePreviousReview, ComparisonOptions, CopyRequest, DiffMode, DiffPresentationWindow, DiffRow, DiffSelection, DiffSide, FileClassificationFilter, FileGrouping, FileSort, FinishReviewPreview, FullFileSide, GitHubPullRequestContext, HunkLocation, ImportedGitHubConversationComment, ImportedGitHubReviewThread, OutlineSymbol, PersistenceDiagnostics, PromptPreview, PromptRequest, RepositoryBaseOverride, RepositorySetup, ReviewConclusion, ReviewData, ReviewSettings, ViewedFilter, Workspace, WorkspaceUiState } from './lib/types';

  const api = createReviewApi();
  type ComposerScope = 'inline' | 'file' | 'review';
  type AnnotationComposer = {
    row?: DiffRow;
    kind: AnnotationKind;
    body: string;
    /** Undefined is intentional for file/review notes: they use the domain's
     * file-level or anchorless representation rather than a fake line. */
    selection?: DiffSelection;
    scope: ComposerScope;
    labels?: string[];
  };
  /** Browser data exists only to exercise the UI; native builds never select it. */
  const browserFixtureMode = !isTauri();
  const modes: { id: DiffMode; label: string; shortcut?: string }[] = [
    { id: 'unified', label: 'Unified' }, { id: 'split', label: 'Split' }, { id: 'full', label: 'Full File' }, { id: 'difftastic', label: 'Difftastic' }
  ];
  function apiFailureMessage(error: unknown, fallback: string) {
    if (error instanceof Error && error.message) return error.message;
    if (typeof error === 'string' && error) return error;
    if (typeof error === 'object' && error !== null && 'message' in error && typeof error.message === 'string') return error.message;
    return fallback;
  }
  function apiFailureCode(error: unknown) {
    return typeof error === 'object' && error !== null && 'code' in error && typeof error.code === 'string' ? error.code : undefined;
  }
  function apiFailureRecoveryPreviewToken(error: unknown) {
    return typeof error === 'object' && error !== null && 'recoveryPreviewToken' in error && typeof error.recoveryPreviewToken === 'string'
      ? error.recoveryPreviewToken
      : undefined;
  }

  let workspaces: Workspace[] = [];
  let review: ReviewData | undefined;
  let activeWorkspaceId = 'workspace-localreview';
  let activeFileId = '';
  let mode: DiffMode = 'unified';
  let rows: DiffRow[] = [];
  let presentation: DiffPresentationWindow | undefined;
  let viewportGeneration = 0;
  let jumpToRow: number | undefined;
  let fullFileSide: FullFileSide = 'new';
  let splitRatio = .5;
  let nearestSourceLine: number | undefined;
  let nearestSourceSide: DiffSide | undefined;
  let restoredScrollTop = 0;
  let outline: OutlineSymbol[] = [];
  let rightTab: 'files' | 'comments' | 'outline' = 'files';
  let fileSearch = '';
  let repositoryFilter = 'all';
  let fileGrouping: FileGrouping = 'repository';
  let fileSort: FileSort = 'review_order';
  let viewedFilter: ViewedFilter = 'all';
  let classificationFilter: FileClassificationFilter = 'all';
  let fileStatusFilter: 'all' | ReviewData['files'][number]['status'] = 'all';
  let fileLanguageFilter = 'all';
  let collapseAllToken = 0;
  let expandAllToken = 0;
  let annotationKindFilter: 'all' | AnnotationKind = 'all';
  let annotationStateFilter: 'all' | AnnotationState = 'all';
  let annotationPublicationFilter: 'all' | 'published' | 'unpublished' | 'local_only' = 'all';
  let annotationLabelFilter = 'all';
  let changedSincePreviousOnly = false;
  let changedSincePrevious: ChangedSincePreviousReview | undefined;
  let changedFileIds = new Set<string>();
  let activeLine: number | undefined;
  let activeSelection: DiffSelection | undefined;
  let settings: ReviewSettings = { fontScale: 1, leftWidth: 244, rightWidth: 332, leftCollapsed: false, rightCollapsed: false, fetchOnReview: false, theme: 'dark', codeFont: 'SF Mono', externalEditor: 'system', tabWidth: 2, showWhitespace: false, vimNavigation: false, shortcuts: {} };
  let zoomToast = '';
  let busy = false;
  let statusMessage = 'Snapshot captured · local refs only';
  let composer: AnnotationComposer | undefined;
  let prompt: PromptPreview | undefined;
  let promptScope: PromptRequest['scope'] = 'feedback';
  let promptHistoryId: string | undefined;
  let promptPortable = true;
  let largePromptCopyWarning = false;
  let showHistory = false;
  let archivedWorkspaces: Workspace[] = [];
  let showDeleteWorkspace = false;
  let workspacePendingDeletion: Workspace | undefined;
  let deleteWorkspaceError = '';
  let workspacePendingRename: Workspace | undefined;
  let workspaceRenameValue = '';
  let showFinish = false;
  let finishConclusion: ReviewConclusion = 'comment';
  let finishSummary = '';
  let finishPreview: FinishReviewPreview | undefined;
  let finishPreviewAnnotationIds: string[] = [];
  let finishPreviewLoading = false;
  let finishPreviewError: { message: string; annotationId?: string } | undefined;
  let finishSubmitting = false;
  let finishSubmissionError = '';
  let finishSubmissionAmbiguous = false;
  let finishRecoveryPreviewToken: string | undefined;
  let showClear = false;
  let showOpen = false;
  let openLocalForm = false;
  let openGitHubForm = false;
  let openSshForm = false;
  let localPath = '';
  let localBase = 'origin/master';
  let localOpenError = '';
  let githubPrUrl = '';
  let sshTarget = '';
  let showBaselines = false;
  let workspaceBase = 'origin/master';
  let repositoryBases: Record<string, string> = {};
  let repositorySetup: RepositorySetup[] = [];
  let selectedSetupRepositoryIds = new Set<string>();
  let setupOverrideBase = '';
  let setupLoading = false;
  let setupMutating = false;
  let setupError = '';
  let comparisonOptions: ComparisonOptions = { ignoreAllWhitespace: false, ignoreSpaceAtEol: false, ignoreCrAtEol: false };
  let showBlame = false;
  let blameResult: CapturedBlameResult | undefined;
  let blameLoading = false;
  let showCommitContext = false;
  let commitContext: CapturedCommitContext | undefined;
  let commitContextLoading = false;
  let commitAuthorFilter = '';
  let commitSubjectFilter = '';
  let includeMergeCommits = true;
  let githubContext: GitHubPullRequestContext | undefined;
  let githubThreads: ImportedGitHubReviewThread[] = [];
  let githubConversation: ImportedGitHubConversationComment[] = [];
  let githubContextLoading = false;
  let showNewReview = false;
  let historyEntries: ReviewData['history'] = [];
  let resizeSide: 'left' | 'right' | undefined;
  let undoCheckpoint: { annotations: Annotation[]; files: ReviewData['files'] } | undefined;
  let selectedAnnotationIds = new Set<string>();
  let activeAnnotationId: string | undefined;
  let editingAnnotationId: string | undefined;
  let showCommandPalette = false;
  let commandQuery = '';
  let showFilePicker = false;
  let filePickerQuery = '';
  let showSettings = false;
  let persistenceDiagnostics: PersistenceDiagnostics | undefined;
  let showCopyMenu = false;
  let actionsOpen = false;
  let copiedMessage = '';
  let uiStateSaveTimer: number | undefined;
  let composerDraftTimer: number | undefined;
  let pendingUiStateSave: { workspaceId: string; state: Partial<WorkspaceUiState> } | undefined;
  let pendingComposerDraft: AnnotationDraft | undefined;
  let finishPreviewTimer: number | undefined;
  // Native setting saves return the entire record.  Serialize requests and
  // ignore older completions so rapid zoom clicks never roll optimistic state
  // back to a stale full-settings response.
  let settingsSaveChain: Promise<void> = Promise.resolve();
  let uiStateSaveChain: Promise<void> = Promise.resolve();
  let composerDraftSaveChain: Promise<void> = Promise.resolve();
  let settingsRevision = 0;
  let finishPreviewGeneration = 0;
  const commandItems: Array<{ label: string; shortcut: string; run: () => void }> = [
    { label: 'Open file picker', shortcut: '⌘P', run: () => showFilePicker = true },
    { label: 'Refresh review', shortcut: '', run: () => void refresh() },
    { label: 'New review', shortcut: '', run: () => { if (canMutateReview) showNewReview = true; } },
    { label: 'Copy feedback prompt', shortcut: '', run: () => void previewPrompt('feedback') },
    { label: 'Ask focused question', shortcut: '⌘⇧Q', run: () => void previewFocusedQuestion() },
    { label: 'Previous annotation', shortcut: '⌥⇞', run: () => void navigateAnnotation(-1) },
    { label: 'Next annotation', shortcut: '⌥⇟', run: () => void navigateAnnotation(1) },
    { label: 'Toggle workspace rail', shortcut: '⌘⇧W', run: () => togglePanel('left') },
    { label: 'Toggle files panel', shortcut: '⌘⇧F', run: () => togglePanel('right') },
    { label: 'Open settings', shortcut: '', run: () => showSettings = true }
  ];

  $: activeFile = review?.files.find((file) => file.id === activeFileId);
  $: activeRepo = review?.repositories.find((repository) => repository.id === activeFile?.repositoryId);
  $: shownFiles = sortFiles((review?.files ?? []).filter((file) =>
    (repositoryFilter === 'all' || file.repositoryId === repositoryFilter) &&
    (viewedFilter === 'all' || (viewedFilter === 'viewed' ? file.viewed : !file.viewed)) &&
    (classificationFilter === 'all' || classificationMatches(file, classificationFilter)) &&
    (fileStatusFilter === 'all' || file.status === fileStatusFilter) &&
    (fileLanguageFilter === 'all' || file.language === fileLanguageFilter) &&
    (!changedSincePreviousOnly || changedFileIds.has(file.id)) &&
    fuzzyMatch(`${file.path} ${file.previousPath ?? ''}`, fileSearch)
  ));
  $: fileLanguages = [...new Set((review?.files ?? []).map((file) => file.language).filter(Boolean))].sort((left, right) => left.localeCompare(right));
  $: changedFileIds = new Set((changedSincePrevious?.files ?? []).filter((file) => file.kind !== 'unchanged').flatMap((file) => file.currentFileId ? [file.currentFileId] : []));
  $: shownAnnotations = (review?.annotations ?? []).filter((annotation) =>
    (annotationKindFilter === 'all' || annotation.kind === annotationKindFilter) &&
    (annotationStateFilter === 'all' || annotation.state === annotationStateFilter) &&
    (annotationPublicationFilter === 'all' ||
      (annotationPublicationFilter === 'published' && Boolean(annotation.publishedId)) ||
      (annotationPublicationFilter === 'unpublished' && !annotation.publishedId) ||
      (annotationPublicationFilter === 'local_only' && annotation.localOnly)) &&
    (annotationLabelFilter === 'all' || annotation.labels.includes(annotationLabelFilter))
  );
  $: finishAnnotations = (review?.annotations ?? []).filter((annotation) => annotation.state === 'open' && !annotation.publishedId && !annotation.localOnly && selectedAnnotationIds.has(annotation.id));
  $: githubReview = Boolean(review?.workspace.source.includes('github'));
  $: historicalReview = Boolean(review?.historical);
  $: reviewCaptureReady = Boolean(review && review.workspace.reviewReady !== false);
  $: canMutateReview = Boolean(reviewCaptureReady && !historicalReview);
  $: canExportReview = Boolean(reviewCaptureReady);
  $: selectableFinishAnnotations = (review?.annotations ?? []).filter((annotation) => annotation.state === 'open' && !annotation.publishedId && selectedAnnotationIds.has(annotation.id));
  $: comparisonOptionsSupported = Boolean(review && !review.workspace.source.includes('github'));
  // Initial setup is the workspace's only valid surface until a capture has
  // created the first review session. Escape/close attempts therefore return
  // to setup instead of exposing an inert empty diff.
  $: if (review?.workspace.reviewReady === false && !showBaselines) showBaselines = true;
  $: layoutStyle = `grid-template-columns:${settings.leftCollapsed ? 0 : settings.leftWidth}px minmax(360px,1fr) ${settings.rightCollapsed ? 0 : settings.rightWidth}px;--font-scale:${settings.fontScale};--left-width:${settings.leftCollapsed ? 0 : settings.leftWidth}px;--right-width:${settings.rightCollapsed ? 0 : settings.rightWidth}px`;
  $: codeFontPercent = Math.round(settings.fontScale * 100);
  $: appTheme = settings.theme;
  $: codeStyle = `font-family:${JSON.stringify(settings.codeFont)}, ui-monospace, SFMono-Regular, Menlo, monospace;tab-size:${settings.tabWidth};--split-ratio:${splitRatio}`;

  function annotationsAt(row: DiffRow, side: DiffSide) {
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!review || !activeFile || !line) return 0;
    return review.annotations.filter((annotation) => annotation.fileId === activeFile.id && annotation.side === side && annotation.startLine <= line && annotation.endLine >= line).length;
  }

  function hasLineAnchor(annotation: Annotation) {
    return Boolean(annotation.fileId && annotation.startLine > 0 && annotation.endLine >= annotation.startLine);
  }

  function fuzzyMatch(value: string, query: string) {
    const needle = query.trim().toLowerCase();
    if (!needle) return true;
    const haystack = value.toLowerCase();
    let cursor = 0;
    for (const character of needle) {
      cursor = haystack.indexOf(character, cursor);
      if (cursor < 0) return false;
      cursor += 1;
    }
    return true;
  }

  function classificationMatches(file: ReviewData['files'][number], filter: Exclude<FileClassificationFilter, 'all'>) {
    const classification = file.classification;
    if (!classification) return false;
    if (filter === 'text') return !classification.binary;
    return filter === 'lfs_pointer' ? classification.lfsPointer : classification[filter];
  }

  function sortFiles(input: ReviewData['files']) {
    return [...dedupeFiles(input)].sort((left, right) => {
      if (fileSort === 'path') return left.path.localeCompare(right.path);
      if (fileSort === 'repository') return `${left.repositoryId}/${left.path}`.localeCompare(`${right.repositoryId}/${right.path}`);
      if (fileSort === 'change_size') return (right.additions + right.deletions) - (left.additions + left.deletions);
      if (fileSort === 'annotations') return right.annotationCount - left.annotationCount || left.path.localeCompare(right.path);
      return 0;
    });
  }

  /** A stale browser fixture or interrupted persistence write must never make
   * a keyed virtual list crash. Native immutable file ids are unique; keep
   * the first occurrence if untrusted fixture state violates that invariant. */
  function dedupeFiles(input: ReviewData['files']) {
    const seen = new Set<string>();
    return input.filter((file) => {
      if (seen.has(file.id)) return false;
      seen.add(file.id);
      return true;
    });
  }

  function normalizeReview(data: ReviewData): ReviewData {
    return { ...data, files: dedupeFiles(data.files) };
  }

  function matchesShortcut(event: KeyboardEvent, configured: string | undefined) {
    if (!configured) return false;
    const parts = configured.toLowerCase().split('+').filter(Boolean);
    const key = parts.at(-1);
    if (!key) return false;
    const needsMeta = parts.includes('meta') || parts.includes('cmd') || parts.includes('ctrl') || parts.includes('control');
    const needsAlt = parts.includes('alt') || parts.includes('option');
    const needsShift = parts.includes('shift');
    if (needsMeta !== Boolean(event.metaKey || event.ctrlKey) || needsAlt !== event.altKey || needsShift !== event.shiftKey) return false;
    return event.key.toLowerCase() === key || (key === 'plus' && (event.key === '+' || event.key === '='));
  }

  onMount(() => {
    let disposed = false;
    let unlistenDesktopOperation: (() => void) | undefined;
    let unlistenRefreshAvailable: (() => void) | undefined;
    void initialize();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') { closeTopOverlay(); return; }
      if (composer && matchesShortcut(event, settings.shortcuts.saveAnnotation ?? 'Meta+Enter')) { event.preventDefault(); void saveComposer(); return; }
      if (matchesShortcut(event, settings.shortcuts.nextHunk ?? 'Alt+ArrowDown')) { event.preventDefault(); nextHunk(); return; }
      if (matchesShortcut(event, settings.shortcuts.previousHunk ?? 'Alt+ArrowUp')) { event.preventDefault(); previousHunk(); return; }
      if (event.altKey && event.key === 'PageDown') { event.preventDefault(); void navigateAnnotation(1); return; }
      if (event.altKey && event.key === 'PageUp') { event.preventDefault(); void navigateAnnotation(-1); return; }
      if (matchesShortcut(event, settings.shortcuts.commandPalette ?? 'Meta+Shift+P')) { event.preventDefault(); showCommandPalette = true; return; }
      if (matchesShortcut(event, settings.shortcuts.filePicker ?? 'Meta+P')) { event.preventDefault(); showFilePicker = true; return; }
      if (matchesShortcut(event, settings.shortcuts.focusQuestion ?? 'Meta+Shift+Q')) { event.preventDefault(); void previewFocusedQuestion(); return; }
      if (!(event.metaKey || event.ctrlKey)) {
        if (settings.vimNavigation && !event.altKey && !event.shiftKey && !event.target?.toString().includes('HTMLInput')) {
          if (event.key === 'j') nextFile();
          if (event.key === 'k') previousFile();
        }
        return;
      }
      if (event.key === '+' || event.key === '=') { event.preventDefault(); changeZoom(.1); }
      if (event.key === '-') { event.preventDefault(); changeZoom(-.1); }
      if (event.key === '0') { event.preventDefault(); setZoom(1); }
      if (event.shiftKey && event.key.toLowerCase() === 'd') { event.preventDefault(); focusDiff(); }
      if (event.shiftKey && event.key.toLowerCase() === 'f') { event.preventDefault(); togglePanel('right'); }
      if (event.shiftKey && event.key.toLowerCase() === 'w') { event.preventDefault(); togglePanel('left'); }
      if (event.key === ']') { event.preventDefault(); nextFile(); }
      if (event.key === '[') { event.preventDefault(); previousFile(); }
    };
    const onMove = (event: PointerEvent) => {
      if (!resizeSide) return;
      if (resizeSide === 'left') setSettings({ leftWidth: Math.min(420, Math.max(180, event.clientX)) });
      else setSettings({ rightWidth: Math.min(520, Math.max(240, window.innerWidth - event.clientX)) });
    };
    const onUp = () => resizeSide = undefined;
    const onPageHide = () => { void flushReviewPersistence(); };
    const onVisibilityChange = () => {
      if (document.visibilityState === 'hidden') void flushReviewPersistence();
    };
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
    window.addEventListener('pagehide', onPageHide);
    document.addEventListener('visibilitychange', onVisibilityChange);
    if (isTauri()) {
      // Forwarded CLI requests are applied by the desktop backend. The event
      // carries only the already-validated workspace id and never paths or a
      // generic command; this keeps focus activation as narrow as Tauri IPC.
      void listen<{ kind: 'workspace'; workspaceId: string }>('localreview://desktop-operation', (event) => {
        if (event.payload.kind !== 'workspace' || !event.payload.workspaceId) return;
        void (async () => {
          const loaded = await api.listWorkspaces();
          if (disposed || !loaded.some((workspace) => workspace.id === event.payload.workspaceId)) return;
          workspaces = loaded;
          await selectWorkspace(event.payload.workspaceId!);
          statusMessage = 'Focused workspace opened by LocalReview CLI.';
        })();
      }).then((unlisten) => {
        if (disposed) unlisten();
        else unlistenDesktopOperation = unlisten;
      }).catch((error) => { statusMessage = `Desktop event listener unavailable: ${error instanceof Error ? error.message : 'unknown error'}`; });
      void listen<{ workspaceId: string }>('localreview://refresh-available', (event) => {
        const workspaceId = event.payload.workspaceId;
        workspaces = workspaces.map((workspace) => workspace.id === workspaceId ? { ...workspace, refreshAvailable: true } : workspace);
        if (review?.workspace.id === workspaceId) review = { ...review, workspace: { ...review.workspace, refreshAvailable: true } };
      }).then((unlisten) => {
        if (disposed) unlisten();
        else unlistenRefreshAvailable = unlisten;
      }).catch((error) => { statusMessage = `Change watcher unavailable: ${error instanceof Error ? error.message : 'unknown error'}`; });
    }
    return () => {
      disposed = true;
      unlistenDesktopOperation?.();
      unlistenRefreshAvailable?.();
      // Capture and dispatch the last stable snapshots while the webview still
      // permits lifecycle work. Native persistence may complete after
      // unmount; the queued payloads no longer read component globals.
      void flushReviewPersistence();
      if (finishPreviewTimer) window.clearTimeout(finishPreviewTimer);
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      window.removeEventListener('pagehide', onPageHide);
      document.removeEventListener('visibilitychange', onVisibilityChange);
    };
  });

  async function initialize() {
    const [loadedWorkspaces, loadedSettings] = await Promise.all([api.listWorkspaces(), api.getSettings()]);
    workspaces = loadedWorkspaces;
    settings = loadedSettings;
    const initial = loadedWorkspaces.find((workspace) => workspace.id === activeWorkspaceId) ?? loadedWorkspaces[0];
    if (initial) await selectWorkspace(initial.id);
    else statusMessage = 'Open a local folder to start a review.';
  }

  async function selectWorkspace(id: string) {
    busy = true;
    try {
      if (review && review.workspace.id !== id) await flushReviewPersistence();
      composer = undefined;
      editingAnnotationId = undefined;
      activeSelection = undefined;
      selectedAnnotationIds = new Set();
      prompt = undefined;
      promptHistoryId = undefined;
      showCopyMenu = false;
      showBaselines = false;
      activeWorkspaceId = id;
      review = normalizeReview(await api.loadReview(id));
      comparisonOptions = review?.repositories[0]?.comparisonOptions ?? { ignoreAllWhitespace: false, ignoreSpaceAtEol: false, ignoreCrAtEol: false };
      if (review.workspace.reviewReady === false) {
        // An uncaptured workspace has no active review session yet. Do not
        // issue session-scoped UI-state, draft, presentation, or outline reads;
        // resume the durable repository setup workflow immediately instead.
        activeFileId = '';
        activeLine = undefined;
        nearestSourceLine = undefined;
        nearestSourceSide = undefined;
        presentation = undefined;
        rows = [];
        outline = [];
        repositoryFilter = 'all';
        fileSearch = '';
        githubContext = undefined;
        githubThreads = [];
        githubConversation = [];
        await openBaselineSetup();
        statusMessage = `Resume setup for ${review.workspace.name}. Choose a local base to capture the initial review; fetching remains explicit.`;
        return;
      }
      await hydrateClassifications(id);
      // A one-shot, read-only provider check on selection. This does not poll,
      // mutate pins, fetch a worktree, or refresh the review generation.
      if (review?.workspace.source.includes('github')) void checkGitHubFreshness(id);
      if (review?.workspace.source.includes('github')) void loadGitHubContext(id);
      else { githubContext = undefined; githubThreads = []; githubConversation = []; }
      const state = await api.getWorkspaceUiState(id);
      mode = state.mode ?? 'unified';
      fullFileSide = state.fullFileSide ?? 'new';
      splitRatio = state.splitRatio ?? .5;
      rightTab = state.rightTab ?? 'files';
      nearestSourceLine = state.nearestSourceLine;
      nearestSourceSide = state.nearestSourceSide;
      restoredScrollTop = state.scrollTop ?? 0;
      activeFileId = review.files.some((file) => file.id === state.activeFileId) ? state.activeFileId! : review.files[0]?.id ?? '';
      const selectableAnnotationIds = new Set(review.annotations.filter((annotation) => annotation.state === 'open' && !annotation.publishedId).map((annotation) => annotation.id));
      selectedAnnotationIds = state.selectedAnnotationIds === undefined
        ? selectableAnnotationIds
        : new Set(state.selectedAnnotationIds.filter((id) => selectableAnnotationIds.has(id)));
      repositoryFilter = 'all';
      fileSearch = '';
      try {
        const draft = await api.getAnnotationDraft(id);
        const draftFile = draft && review.files.find((file) => file.id === draft.fileId);
        if (draft && draftFile) {
          activeFileId = draft.fileId;
          composer = {
            row: { id: draft.id, kind: 'context', [draft.side === 'old' ? 'oldLine' : 'newLine']: draft.startLine, [draft.side === 'old' ? 'oldText' : 'newText']: '' },
            selection: draft.kind === 'file_note' || draft.kind === 'review_note' ? undefined : { side: draft.side, startLine: draft.startLine, endLine: draft.endLine },
            scope: draft.kind === 'review_note' ? 'review' : draft.kind === 'file_note' ? 'file' : 'inline',
            kind: draft.kind,
            body: draft.body
          };
          statusMessage = 'Recovered an unfinished annotation draft.';
        }
      } catch (error) {
        statusMessage = `Draft recovery is unavailable: ${error instanceof Error ? error.message : 'native draft command failed'}`;
      }
      await loadPresentation(0, 220);
      await loadOutline();
    } finally { busy = false; }
  }

  /** The classification payload is capture-derived metadata. Join it by the
   * immutable review file id so filtering never walks the current checkout. */
  async function hydrateClassifications(workspaceId: string) {
    try {
      const records = await api.getReviewFileClassifications(workspaceId);
      if (!review || review.workspace.id !== workspaceId) return;
      const byFileId = new Map(records.map((record) => [record.fileId, record.classification]));
      review = { ...review, files: review.files.map((file) => ({ ...file, classification: byFileId.get(file.id) ?? file.classification })) };
    } catch (error) {
      // Read-only metadata must never block opening a captured review. The UI
      // deliberately leaves classification filters empty rather than guessing.
      statusMessage = `File classifications are unavailable: ${error instanceof Error ? error.message : 'metadata command failed'}`;
    }
  }

  function syncActiveWorkspaceSummary(recalculateDrafts = false) {
    if (!review || review.historical) return;
    if (recalculateDrafts) {
      review = {
        ...review,
        workspace: {
          ...review.workspace,
          draftCount: review.annotations.filter((annotation) => annotation.state === 'open' && !annotation.publishedId).length
        }
      };
    }
    const summary = review.workspace;
    workspaces = workspaces.map((workspace) => workspace.id === summary.id ? { ...workspace, ...summary } : workspace);
  }

  async function checkGitHubFreshness(workspaceId: string) {
    try {
      const status = await api.getGitHubUpdateStatus(workspaceId);
      if (!review || review.workspace.id !== workspaceId) return;
      const refreshAvailable = status.baseChanged || status.headChanged;
      review = { ...review, workspace: { ...review.workspace, refreshAvailable } };
      workspaces = workspaces.map((workspace) => workspace.id === workspaceId ? { ...workspace, refreshAvailable } : workspace);
      if (refreshAvailable) statusMessage = 'GitHub has newer base or head revisions. Refresh explicitly to capture them.';
    } catch {
      // Provider status is optional read-only context. A network/auth issue
      // must not make the already-pinned local PR review unusable.
    }
  }

  async function loadGitHubContext(workspaceId: string) {
    githubContextLoading = true;
    try {
      const [context, threads, conversation] = await Promise.all([
        api.getGitHubPullRequest(workspaceId),
        api.getGitHubThreads(workspaceId),
        api.getGitHubConversation(workspaceId)
      ]);
      if (!review || review.workspace.id !== workspaceId) return;
      githubContext = context;
      githubThreads = threads;
      githubConversation = conversation;
    } catch {
      // Imported provider state is useful context, but failures must never
      // obscure the locally pinned review or its local annotations.
      if (review?.workspace.id === workspaceId) {
        githubContext = undefined;
        githubThreads = [];
        githubConversation = [];
      }
    } finally {
      if (review?.workspace.id === workspaceId) githubContextLoading = false;
    }
  }

  async function loadPresentation(startRow = 0, endRow = 220) {
    if (!activeFileId) return;
    busy = true;
    const generation = ++viewportGeneration;
    try {
      const next = await api.getPresentationWindow({ fileId: activeFileId, comparisonId: activeFile?.comparisonId, mode, startRow, endRow, generation, fullFileSide, splitRatio });
      if (next.generation !== viewportGeneration || next.fileId !== activeFileId || next.mode !== mode) return;
      presentation = next;
      rows = next.rows;
    }
    finally { busy = false; }
  }

  async function selectFile(fileId: string) {
    await persistComposerDraftNow();
    activeFileId = fileId;
    activeLine = undefined;
    activeSelection = undefined;
    composer = undefined;
    restoredScrollTop = 0;
    // The file choice is durable presentation state. Save it before any
    // potentially slow native presentation/highlighting work so closing the
    // window cannot restore the previously selected file.
    await persistWorkspaceUiStateNow({ activeFileId: fileId, scrollTop: 0 });
    await loadPresentation(0, 220);
    await loadOutline();
    if (review && !review.files.find((file) => file.id === fileId)?.viewed) {
      await api.setViewed(review.workspace.id, fileId, true);
      review = {
        ...review,
        workspace: { ...review.workspace, progress: { viewed: review.files.filter((file) => file.id === fileId || file.viewed).length, total: review.files.length } },
        files: review.files.map((file) => file.id === fileId ? { ...file, viewed: true } : file)
      };
      syncActiveWorkspaceSummary();
    }
  }

  async function setMode(next: DiffMode, location?: { side: DiffSide; line: number }) {
    const source = location ?? (nearestSourceLine ? { side: nearestSourceSide ?? 'new' as DiffSide, line: nearestSourceLine } : undefined);
    mode = next;
    // Difftastic can take materially longer than canonical presentation. The
    // selection must reach native storage before that work begins; otherwise
    // a quit or presentation failure leaves the visibly selected tab durable
    // only in the soon-to-be-destroyed webview.
    await persistWorkspaceUiStateNow({ mode: next });
    if (source && activeFileId) await jumpToSource(activeFileId, source.side, source.line, next);
    else await loadPresentation(presentation?.startRow ?? 0, (presentation?.startRow ?? 0) + 220);
  }

  async function setSettings(partial: Partial<ReviewSettings>) {
    settings = { ...settings, ...partial };
    const revision = ++settingsRevision;
    const save = settingsSaveChain
      .catch(() => {
        // A prior persistence failure must not permanently poison a later
        // explicit setting change.
      })
      .then(async () => {
        const saved = await api.saveSettings(partial);
        // Native bounds still apply, but only the last requested full record
        // may replace optimistic UI state.  This prevents out-of-order
        // responses from resetting rapid A+ clicks (for example to 130%).
        if (revision === settingsRevision) settings = saved;
      });
    settingsSaveChain = save;
    try {
      await save;
    } catch (error) {
      statusMessage = `Could not save review settings: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  async function loadPersistenceDiagnostics() {
    try {
      persistenceDiagnostics = await api.getPersistenceDiagnostics();
    } catch (error) {
      statusMessage = `Could not load diagnostics: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  async function copyPersistenceDiagnostics() {
    if (!persistenceDiagnostics) return;
    try {
      await copyText(JSON.stringify(persistenceDiagnostics, null, 2));
      statusMessage = 'Copied source-free LocalReview diagnostics.';
    } catch (error) {
      statusMessage = `Could not copy diagnostics: ${error instanceof Error ? error.message : 'clipboard error'}`;
    }
  }

  function persistWorkspaceUiState(partial: Partial<WorkspaceUiState> = {}) {
    const snapshot = captureWorkspaceUiState(partial);
    if (!snapshot) return;
    if (uiStateSaveTimer) window.clearTimeout(uiStateSaveTimer);
    pendingUiStateSave = snapshot;
    uiStateSaveTimer = window.setTimeout(() => void flushPendingWorkspaceUiState(), 120);
  }

  function captureWorkspaceUiState(partial: Partial<WorkspaceUiState> = {}) {
    if (!review || review.historical || review.workspace.reviewReady === false) return undefined;
    return {
      workspaceId: review.workspace.id,
      state: {
        activeFileId, mode, fullFileSide, nearestSourceLine, nearestSourceSide,
        scrollTop: restoredScrollTop, splitRatio, rightTab,
        selectedAnnotationIds: [...selectedAnnotationIds], ...partial
      }
    };
  }

  function enqueueWorkspaceUiStateSave(snapshot: NonNullable<typeof pendingUiStateSave>) {
    const save = uiStateSaveChain
      .catch(() => { /* a failed save must not prevent the next snapshot */ })
      .then(async () => {
        try { await api.saveWorkspaceUiState(snapshot.workspaceId, snapshot.state); }
        catch (error) { statusMessage = `Could not save workspace layout: ${apiFailureMessage(error, 'native UI-state command failed')}`; }
      });
    uiStateSaveChain = save;
    return save;
  }

  async function flushPendingWorkspaceUiState() {
    if (uiStateSaveTimer) window.clearTimeout(uiStateSaveTimer);
    uiStateSaveTimer = undefined;
    const snapshot = pendingUiStateSave;
    pendingUiStateSave = undefined;
    if (snapshot) await enqueueWorkspaceUiStateSave(snapshot);
    else await uiStateSaveChain.catch(() => {});
  }

  async function persistWorkspaceUiStateNow(partial: Partial<WorkspaceUiState> = {}) {
    const snapshot = captureWorkspaceUiState(partial);
    if (snapshot) pendingUiStateSave = snapshot;
    await flushPendingWorkspaceUiState();
  }

  function changeZoom(delta: number) { setZoom(settings.fontScale + delta); }
  function setZoom(value: number) {
    const fontScale = Math.max(.75, Math.min(2, Math.round(value * 10) / 10));
    setSettings({ fontScale });
    zoomToast = `${Math.round(fontScale * 100)}% font size`;
    window.setTimeout(() => zoomToast = '', 1200);
  }
  function togglePanel(side: 'left' | 'right') {
    setSettings(side === 'left' ? { leftCollapsed: !settings.leftCollapsed } : { rightCollapsed: !settings.rightCollapsed });
  }
  function focusDiff() { setSettings({ leftCollapsed: !settings.leftCollapsed || !settings.rightCollapsed, rightCollapsed: !settings.leftCollapsed || !settings.rightCollapsed }); }
  function restorePanel(side: 'left' | 'right') { setSettings(side === 'left' ? { leftCollapsed: false } : { rightCollapsed: false }); }
  function resetDivider(side: 'left' | 'right') { setSettings(side === 'left' ? { leftWidth: 244, leftCollapsed: false } : { rightWidth: 332, rightCollapsed: false }); }
  function resizePanelKey(side: 'left' | 'right', event: KeyboardEvent) {
    if (!['ArrowLeft', 'ArrowRight', 'Home'].includes(event.key)) return;
    event.preventDefault();
    if (event.key === 'Home') { resetDivider(side); return; }
    const delta = event.key === 'ArrowRight' ? 16 : -16;
    if (side === 'left') void setSettings({ leftWidth: Math.min(420, Math.max(180, settings.leftWidth + delta)) });
    else void setSettings({ rightWidth: Math.min(520, Math.max(240, settings.rightWidth - delta)) });
  }
  function closeTopOverlay() {
    if (composer) { void closeComposer(); return; }
    if (showCommandPalette) { showCommandPalette = false; return; }
    if (showFilePicker) { showFilePicker = false; return; }
    if (showCopyMenu) { showCopyMenu = false; return; }
    if (prompt) { prompt = undefined; return; }
    if (showSettings) { showSettings = false; return; }
    if (showBlame) { showBlame = false; return; }
    if (showCommitContext) { showCommitContext = false; return; }
    if (showOpen) { showOpen = false; openLocalForm = false; openGitHubForm = false; openSshForm = false; return; }
    if (showBaselines) { closeBaselineSetup(); return; }
    if (showFinish) { closeFinishReview(); return; }
    if (showClear) { showClear = false; return; }
    if (showNewReview) { showNewReview = false; return; }
    if (showDeleteWorkspace) { showDeleteWorkspace = false; workspacePendingDeletion = undefined; return; }
    if (workspacePendingRename) { workspacePendingRename = undefined; return; }
    if (showHistory) showHistory = false;
  }

  function annotate(row: DiffRow, selection: DiffSelection) {
    if (!canMutateReview || mode === 'difftastic' || !activeFile || !activeRepo) return;
    activeLine = selection.startLine;
    activeSelection = selection;
    const existing = composer?.scope === 'inline' ? composer : undefined;
    composer = {
      row,
      selection,
      scope: 'inline',
      kind: existing?.kind ?? 'comment',
      body: existing?.body ?? '',
      labels: existing?.labels
    };
    // Persist the range itself immediately. A selected-but-empty draft is
    // still meaningful state and should reopen on the same captured lines.
    void persistComposerDraftNow();
  }

  function scheduleComposerDraft() {
    const draft = captureComposerDraft();
    if (!draft) return;
    if (composerDraftTimer) window.clearTimeout(composerDraftTimer);
    pendingComposerDraft = draft;
    composerDraftTimer = window.setTimeout(() => void flushPendingComposerDraft(), 350);
  }

  function captureComposerDraft(): AnnotationDraft | undefined {
    if (!composer || !review || review.historical || review.workspace.reviewReady === false || !activeFile) return undefined;
    const repository = review.repositories.find((item) => item.id === activeFile.repositoryId);
    if (!repository) return undefined;
    const fallback = selectedCapturedRange() ?? {
      side: activeFile.status === 'deleted' ? 'old' as DiffSide : 'new' as DiffSide,
      startLine: 1,
      endLine: 1
    };
    const draftSelection = composer.selection ?? fallback;
    return {
      id: editingAnnotationId ?? `draft-${review.workspace.id}`,
      workspaceId: review.workspace.id,
      fileId: activeFile.id,
      repositoryId: repository.id,
      kind: composer.kind,
      side: draftSelection.side,
      startLine: draftSelection.startLine,
      endLine: draftSelection.endLine,
      body: composer.body,
      updatedAt: new Date().toISOString()
    };
  }

  function enqueueComposerDraftSave(draft: AnnotationDraft) {
    const save = composerDraftSaveChain
      .catch(() => { /* a failed save must not prevent the next draft */ })
      .then(async () => {
        try { await api.saveAnnotationDraft(draft); }
        catch (error) { statusMessage = `Draft autosave failed: ${apiFailureMessage(error, 'native draft command failed')}`; }
      });
    composerDraftSaveChain = save;
    return save;
  }

  async function flushPendingComposerDraft() {
    if (composerDraftTimer) window.clearTimeout(composerDraftTimer);
    composerDraftTimer = undefined;
    const draft = pendingComposerDraft;
    pendingComposerDraft = undefined;
    if (draft) await enqueueComposerDraftSave(draft);
    else await composerDraftSaveChain.catch(() => {});
  }

  async function persistComposerDraftNow() {
    const draft = captureComposerDraft();
    if (draft) pendingComposerDraft = draft;
    await flushPendingComposerDraft();
  }

  async function flushReviewPersistence() {
    await Promise.all([persistWorkspaceUiStateNow(), persistComposerDraftNow()]);
  }

  async function flushPendingReviewPersistence() {
    await Promise.all([flushPendingWorkspaceUiState(), flushPendingComposerDraft()]);
  }

  async function discardComposerDraft(workspaceId: string) {
    if (pendingComposerDraft?.workspaceId === workspaceId) {
      if (composerDraftTimer) window.clearTimeout(composerDraftTimer);
      composerDraftTimer = undefined;
      pendingComposerDraft = undefined;
    }
    await composerDraftSaveChain.catch(() => {});
    await api.clearAnnotationDraft(workspaceId);
  }

  async function closeComposer() {
    const workspaceId = review?.workspace.id;
    if (workspaceId) {
      try { await discardComposerDraft(workspaceId); }
      catch (error) { statusMessage = `Could not discard saved draft: ${error instanceof Error ? error.message : 'native draft command failed'}`; }
    }
    composer = undefined;
    editingAnnotationId = undefined;
  }

  function startQuestion() {
    if (!canMutateReview || !activeFile || !activeRepo || mode === 'difftastic') return;
    const line = nearestSourceLine ?? activeLine ?? 1;
    composer = {
      row: rows.find((row) => row.newLine === line || row.oldLine === line) ?? { id: 'focused-question', kind: 'context', newLine: line, newText: '' },
      selection: { side: nearestSourceSide ?? (activeFile.status === 'deleted' ? 'old' : 'new'), startLine: line, endLine: line },
      scope: 'inline',
      kind: 'question', body: ''
    };
    void persistComposerDraftNow();
  }

  function startFileNote() {
    if (!canMutateReview || !activeFile || !activeRepo) return;
    composer = { kind: 'file_note', scope: 'file', body: '' };
    void persistComposerDraftNow();
  }

  function startReviewNote() {
    if (!canMutateReview || !review) return;
    composer = { kind: 'review_note', scope: 'review', body: '' };
    void persistComposerDraftNow();
  }

  function chooseComposerKind(kind: AnnotationKind) {
    if (!composer || !activeFile) return;
    if (kind === 'file_note') composer = { ...composer, kind, scope: 'file', selection: undefined };
    else if (kind === 'review_note') composer = { ...composer, kind, scope: 'review', selection: undefined };
    else {
      const selection = composer.selection ?? selectedCapturedRange();
      if (!selection || mode === 'difftastic') {
        statusMessage = 'Choose a canonical diff line before creating an inline annotation.';
        return;
      }
      composer = { ...composer, kind, scope: 'inline', selection };
    }
    void persistComposerDraftNow();
    scheduleComposerDraft();
  }

  function composerLocationLabel(value: AnnotationComposer) {
    if (value.scope === 'review') return 'Review note · whole review';
    if (value.scope === 'file') return `File note · ${activeFile?.path ?? 'captured file'}`;
    const selection = value.selection;
    return `Inline ${value.kind} · ${selection?.side ?? 'new'} lines ${selection?.startLine ?? ''}${selection?.endLine && selection.endLine !== selection.startLine ? `–${selection.endLine}` : ''}`;
  }

  function selectedCapturedRange(): DiffSelection | undefined {
    if (!activeFile) return undefined;
    if (activeSelection) return activeSelection;
    const line = activeLine ?? nearestSourceLine;
    if (!line) return undefined;
    return { side: nearestSourceSide ?? (activeFile.status === 'deleted' ? 'old' : 'new'), startLine: line, endLine: line };
  }

  async function openBlame() {
    if (!canExportReview || !review || !activeFile || mode === 'difftastic') {
      statusMessage = mode === 'difftastic' ? 'Return to a canonical diff before requesting captured blame.' : 'Select a captured source line first.';
      return;
    }
    const selection = selectedCapturedRange();
    if (!selection) { statusMessage = 'Select a captured source line first.'; return; }
    blameLoading = true;
    showBlame = true;
    blameResult = undefined;
    try {
      blameResult = await api.getCapturedBlame(review.workspace.id, activeFile.id, selection.side, selection.startLine, selection.endLine);
    } catch (error) {
      statusMessage = `Could not load captured blame: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { blameLoading = false; }
  }

  async function loadCommitContext(selectedCommit?: string) {
    if (!canExportReview || !review || !activeRepo) return;
    commitContextLoading = true;
    try {
      commitContext = await api.getCommitContext(review.workspace.id, {
        repositoryId: activeRepo.id,
        maxEntries: 100,
        includeMergeCommits,
        authorContains: commitAuthorFilter.trim() || undefined,
        subjectContains: commitSubjectFilter.trim() || undefined,
        selectedCommit
      });
    } catch (error) {
      statusMessage = `Could not load captured commit context: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { commitContextLoading = false; }
  }

  async function openCommitContext() {
    if (!canExportReview || !activeRepo) return;
    showCommitContext = true;
    commitContext = undefined;
    await loadCommitContext();
  }

  async function showChangedSincePreviousReview() {
    if (!canExportReview || !review || !activeRepo) return;
    try {
      const result = await api.getChangedSincePreviousReview(review.workspace.id, activeRepo.id);
      changedSincePrevious = result;
      if (!result.previousComparisonId) {
        changedSincePreviousOnly = false;
        statusMessage = 'There is no earlier immutable review generation for this repository.';
        return;
      }
      changedSincePreviousOnly = true;
      rightTab = 'files';
      const changedCount = result.files.filter((file) => file.kind !== 'unchanged' && file.currentFileId).length;
      statusMessage = `Showing ${changedCount} files changed since the previous captured review${result.truncated ? ' (result truncated)' : ''}.`;
    } catch (error) {
      statusMessage = `Could not compare immutable review history: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  async function saveComposer() {
    if (!composer?.body.trim() || !activeFile || !activeRepo || !review || !canMutateReview) return;
    const savedComposer = { ...composer, labels: composer.labels ? [...composer.labels] : undefined };
    const savedFile = activeFile;
    const savedRepository = activeRepo;
    const workspaceId = review.workspace.id;
    const savedEditingAnnotationId = editingAnnotationId;
    const inline = savedComposer.scope === 'inline';
    if (inline && !savedComposer.selection) return;
    let selectedSource = '';
    if (inline && savedComposer.selection) {
      let sourceRange;
      try {
        sourceRange = await api.getCapturedSourceRange(savedFile.id, savedComposer.selection.side, savedComposer.selection.startLine, savedComposer.selection.endLine, savedFile.comparisonId);
      } catch (error) {
        statusMessage = `Could not read the complete captured selection: ${error instanceof Error ? error.message : 'native source-range command failed'}`;
        return;
      }
      if (!sourceRange.complete) {
        statusMessage = 'Annotation was not saved because the immutable captured source range was incomplete.';
        return;
      }
      selectedSource = sourceRange.text;
    }
    const selection = savedComposer.selection ?? { side: savedFile.status === 'deleted' ? 'old' as DiffSide : 'new' as DiffSide, startLine: 0, endLine: 0 };
    const annotation: Annotation = {
      id: savedEditingAnnotationId ?? crypto.randomUUID?.() ?? `annotation-${Date.now()}`,
      fileId: savedFile.id, repositoryId: savedRepository.id, kind: savedComposer.kind, state: 'open', side: selection.side,
      startLine: selection.startLine, endLine: selection.endLine, body: savedComposer.body.trim(), selectedSource,
      labels: [...new Set([...(savedComposer.labels ?? []), ...(savedComposer.kind === 'question' ? ['question'] : savedComposer.kind === 'file_note' ? ['file-note'] : savedComposer.kind === 'review_note' ? ['review-note'] : [])])],
      // File/review notes have no GitHub inline representation. Keep them
      // safely local by default; imported legacy rows still surface clearly
      // in Finish Review when they are selected.
      localOnly: savedComposer.kind === 'question' || savedComposer.kind === 'file_note' || savedComposer.kind === 'review_note',
      createdAt: new Date().toISOString()
    };
    const saved = await api.saveAnnotation(workspaceId, annotation);
    if (review?.workspace.id !== workspaceId) {
      await discardComposerDraft(workspaceId);
      return;
    }
    review.annotations = [saved, ...review.annotations.filter((annotation) => annotation.id !== saved.id)];
    const wasEditing = Boolean(savedEditingAnnotationId);
    if (savedComposer.kind !== 'review_note') {
      review.files = review.files.map((file) => file.id === savedFile.id ? { ...file, annotationCount: wasEditing ? file.annotationCount : file.annotationCount + 1 } : file);
    }
    syncActiveWorkspaceSummary(true);
    composer = undefined;
    editingAnnotationId = undefined;
    selectedAnnotationIds = new Set([...selectedAnnotationIds, saved.id]);
    rightTab = 'comments';
    persistWorkspaceUiState();
    await discardComposerDraft(workspaceId);
  }

  async function previewPrompt(scope = promptScope, historyId = promptHistoryId, focusedAnnotationId?: string) {
    if (!review || (!canExportReview && !historyId)) return;
    promptScope = scope;
    promptHistoryId = historyId;
    largePromptCopyWarning = false;
    prompt = await api.generatePrompt(review.workspace.id, { scope, portable: promptPortable, historyId, annotationIds: scope === 'focused_question' ? (focusedAnnotationId ? [focusedAnnotationId] : []) : scope === 'selected' ? [...selectedAnnotationIds] : undefined });
  }

  async function previewFocusedQuestion() {
    if (!review) return;
    const focused = review.annotations.find((annotation) => annotation.kind === 'question' && annotation.fileId === activeFileId && annotation.startLine === (activeLine ?? nearestSourceLine));
    if (focused) await previewPrompt('focused_question', undefined, focused.id);
    else startQuestion();
  }

  function promptNeedsLargeCopyWarning() {
    return Boolean(prompt && (prompt.estimatedTokens > 100_000 || new TextEncoder().encode(prompt.content).byteLength > 4_000_000));
  }

  async function copyPrompt(confirmLarge = false) {
    if (!prompt) return;
    if (promptNeedsLargeCopyWarning() && !confirmLarge) {
      largePromptCopyWarning = true;
      return;
    }
    try {
      await copyText(prompt.content);
      statusMessage = `Copied ${prompt.annotationCount} annotations — annotations are unchanged`;
    } catch (error) {
      statusMessage = `Could not copy prompt: ${error instanceof Error ? error.message : 'clipboard error'}`;
    }
  }

  async function savePrompt(format: 'markdown' | 'json') {
    if (!prompt || !review) return;
    try {
      const result = await api.savePromptExport(review.workspace.id, prompt.exportId, format);
      statusMessage = result.saved
        ? `Saved ${result.format}; annotations are unchanged.`
        : `Save ${result.format} cancelled; annotations are unchanged.`;
    } catch (error) {
      statusMessage = `Could not save prompt: ${error instanceof Error ? error.message : 'native save failed'}`;
    }
  }

  async function confirmClear() {
    if (!review || !canMutateReview) return;
    undoCheckpoint = { annotations: review.annotations, files: review.files };
    const checkpoint = await api.archiveAnnotations(review.workspace.id);
    review.annotations = [];
    review.files = review.files.map((file) => ({ ...file, annotationCount: 0 }));
    review.history = [checkpoint, ...review.history];
    syncActiveWorkspaceSummary(true);
    selectedAnnotationIds = new Set();
    showClear = false;
    statusMessage = `Archived ${checkpoint.annotationCount} annotations. Undo is available for this session.`;
  }

  async function undoClear() {
    if (!review || review.historical || !undoCheckpoint) return;
    review = normalizeReview(await api.restoreAnnotations(review.workspace.id, undoCheckpoint.annotations));
    syncActiveWorkspaceSummary(true);
    undoCheckpoint = undefined;
    statusMessage = 'Restored as fresh active annotations. The archived checkpoint remains immutable and recoverable.';
  }

  function finishRequest() {
    return { annotationIds: finishAnnotations.map((annotation) => annotation.id), summary: finishSummary, conclusion: finishConclusion };
  }
  function finishItemStatus(annotation: Annotation) {
    if (annotation.publishedId) return 'Already published';
    if (annotation.localOnly) return 'Local only · excluded from GitHub';
    if (!hasLineAnchor(annotation)) return 'No line anchor · GitHub cannot publish inline';
    if (annotation.state !== 'open') return `${annotation.state} · excluded from GitHub`;
    return `Inline ${annotation.side} ${annotation.startLine}${annotation.endLine === annotation.startLine ? '' : `–${annotation.endLine}`} · ready`;
  }
  function describeFinishPreviewFailure(error: unknown) {
    const message = apiFailureMessage(error, 'unknown error');
    const id = /annotation ([0-9a-f-]{8,})/i.exec(message)?.[1];
    const annotation = id ? review?.annotations.find((item) => item.id === id) : undefined;
    const context = annotation
      ? `${annotation.kind.replace('_', ' ')}${hasLineAnchor(annotation) ? ` at ${review?.files.find((file) => file.id === annotation.fileId)?.path ?? 'captured file'}:${annotation.startLine}` : ' without a GitHub line anchor'}`
      : undefined;
    return { message: context ? `${message} (${context}). Change its local-only/anchor state, then prepare again.` : message, annotationId: annotation?.id };
  }
  function scheduleFinishPreview() {
    if (finishSubmissionAmbiguous) {
      finishSubmissionError = 'Reconcile or explicitly abandon the unresolved GitHub attempt before changing this review.';
      return;
    }
    finishSubmissionError = '';
    const previous = finishPreview;
    finishPreview = undefined;
    finishPreviewAnnotationIds = [];
    finishPreviewError = undefined;
    if (previous && review) void discardFinishPreview(review.workspace.id, previous.previewToken);
    finishPreviewGeneration += 1;
    if (finishPreviewTimer) window.clearTimeout(finishPreviewTimer);
    finishPreviewTimer = window.setTimeout(() => void previewFinishReview(), 250);
  }
  async function previewFinishReview() {
    if (!review || review.historical) { finishPreview = undefined; return; }
    const generation = ++finishPreviewGeneration;
    const workspaceId = review.workspace.id;
    const request = finishRequest();
    const fingerprint = JSON.stringify(request);
    finishPreviewLoading = true;
    try {
      const next = await api.previewFinishReview(workspaceId, request);
      if (generation !== finishPreviewGeneration || review?.workspace.id !== workspaceId || JSON.stringify(finishRequest()) !== fingerprint) {
        void discardFinishPreview(workspaceId, next.previewToken);
        return;
      }
      finishPreview = next;
      finishPreviewAnnotationIds = [...next.annotationIds];
      if (next.requiresReconciliation) {
        finishSubmissionAmbiguous = true;
        finishRecoveryPreviewToken = next.previewToken;
        finishSubmissionError = 'Recovered an unresolved GitHub submission from durable history. Check GitHub again or explicitly abandon it.';
      }
    }
    catch (error) {
      finishPreview = undefined;
      finishPreviewError = describeFinishPreviewFailure(error);
      statusMessage = `Could not prepare GitHub review payload: ${finishPreviewError.message}`;
    } finally { if (generation === finishPreviewGeneration) finishPreviewLoading = false; }
  }
  function openFinishReview() {
    if (!review || !canMutateReview) return;
    showFinish = true;
    finishPreview = undefined;
    finishPreviewAnnotationIds = [];
    finishSubmitting = false;
    finishSubmissionError = '';
    finishSubmissionAmbiguous = false;
    finishRecoveryPreviewToken = undefined;
    scheduleFinishPreview();
  }
  function closeFinishReview(abandonPreview = true) {
    if (finishSubmissionAmbiguous) {
      finishSubmissionError = 'This submission may already exist on GitHub. Check again or explicitly abandon the unresolved attempt before closing.';
      return;
    }
    const preview = finishPreview;
    const workspaceId = review?.workspace.id;
    showFinish = false;
    finishPreview = undefined;
    finishPreviewAnnotationIds = [];
    finishPreviewLoading = false;
    finishPreviewError = undefined;
    finishSubmitting = false;
    finishSubmissionError = '';
    finishRecoveryPreviewToken = undefined;
    finishPreviewGeneration += 1;
    if (finishPreviewTimer) window.clearTimeout(finishPreviewTimer);
    finishPreviewTimer = undefined;
    if (abandonPreview && preview && workspaceId) void discardFinishPreview(workspaceId, preview.previewToken);
  }
  async function discardFinishPreview(workspaceId: string, previewToken: string) {
    // Cleanup is intentionally best effort. The native side accepts only
    // Previewed records here and never abandons an ambiguous Prepared POST.
    try { await api.abandonFinishReview(workspaceId, { previewToken }); }
    catch { /* a submitted/prepared/stale token is deliberately retained */ }
  }
  function primaryFinishAction() {
    if (!canExportReview) return;
    if (review?.historical) { void previewPrompt('all'); return; }
    if (githubReview) openFinishReview();
    else void previewPrompt('all');
  }
  async function submitReview() {
    if (!review || review.historical || !finishPreview || finishSubmitting) return;
    const preview = finishPreview;
    finishSubmitting = true;
    finishSubmissionError = '';
    try {
      const result = await api.finishReview(review.workspace.id, { previewToken: preview.previewToken });
      if (result.previewToken === preview.previewToken && result.payloadJson !== preview.payloadJson) {
        finishSubmissionError = 'The server returned a payload different from the reviewed preview. No local annotations were marked published.';
        statusMessage = finishSubmissionError;
        return;
      }
      if (result.previewToken !== preview.previewToken && result.publicationStatus !== 'reconciled') {
        finishSubmissionError = 'The server completed a different, non-reconciled publication. No local annotations were marked published.';
        statusMessage = finishSubmissionError;
        return;
      }
      const completedIds = new Set(result.annotationIds);
      review.annotations = review.annotations.map((annotation) => completedIds.has(annotation.id) ? { ...annotation, publishedId: result.reviewId } : annotation);
      syncActiveWorkspaceSummary(true);
      if (result.previewToken !== preview.previewToken) await discardFinishPreview(review.workspace.id, preview.previewToken);
      finishSubmissionAmbiguous = false;
      finishRecoveryPreviewToken = undefined;
      closeFinishReview(false);
      finishSummary = '';
      statusMessage = `${result.publicationStatus === 'reconciled' ? 'Reconciled' : 'Submitted'} one native GitHub review (${result.annotationCount} comments).`;
    } catch (error) {
      const message = apiFailureMessage(error, 'GitHub review submission failed.');
      const code = apiFailureCode(error);
      const recoveryPreviewToken = apiFailureRecoveryPreviewToken(error);
      if (recoveryPreviewToken) finishRecoveryPreviewToken = recoveryPreviewToken;
      finishSubmissionError = message;
      finishSubmissionAmbiguous = finishSubmissionAmbiguous
        || code === 'github_publication_ambiguous'
        || code === 'github_publication_reconciliation_pending'
        || message.includes('outcome is ambiguous')
        || message.includes('prepared attempt');
      statusMessage = finishSubmissionAmbiguous
        ? 'GitHub submission outcome is unresolved. Check GitHub again or explicitly abandon the attempt.'
        : `GitHub rejected the review: ${message}`;
      if (!finishSubmissionAmbiguous) {
        finishPreview = undefined;
        finishPreviewAnnotationIds = [];
        await previewFinishReview();
      }
    } finally {
      finishSubmitting = false;
    }
  }

  async function abandonUnresolvedFinishReview() {
    if (!review || !finishSubmissionAmbiguous || finishSubmitting) return;
    const recoveryPreviewToken = finishRecoveryPreviewToken ?? finishPreview?.previewToken;
    if (!recoveryPreviewToken) return;
    finishSubmitting = true;
    try {
      await api.abandonFinishReview(review.workspace.id, { previewToken: recoveryPreviewToken }, true);
      if (finishPreview && finishPreview.previewToken !== recoveryPreviewToken) {
        await discardFinishPreview(review.workspace.id, finishPreview.previewToken);
      }
      finishSubmissionAmbiguous = false;
      finishRecoveryPreviewToken = undefined;
      finishSubmissionError = '';
      closeFinishReview(false);
      statusMessage = 'Abandoned the unresolved GitHub attempt by explicit request. You can prepare a new review.';
    } catch (error) {
      finishSubmissionError = `Could not abandon the unresolved attempt: ${apiFailureMessage(error, 'unknown error')}`;
    } finally {
      finishSubmitting = false;
    }
  }

  function nextFile() {
    const list = shownFiles;
    const index = list.findIndex((file) => file.id === activeFileId);
    if (list.length) selectFile(list[(index + 1 + list.length) % list.length].id);
  }
  function previousFile() {
    const list = shownFiles;
    const index = list.findIndex((file) => file.id === activeFileId);
    if (list.length) selectFile(list[(index - 1 + list.length) % list.length].id);
  }
  function nextHunk() {
    const locations = presentation?.hunks ?? [];
    const target = locations.find((hunk) => (hunk.newLine ?? hunk.oldLine ?? 0) > (activeLine ?? nearestSourceLine ?? 0)) ?? locations[0];
    if (!target) return;
    activeLine = target.newLine ?? target.oldLine ?? activeLine;
    jumpToRow = target.rowIndex;
    persistWorkspaceUiState({ nearestSourceLine: activeLine });
  }
  function previousHunk() {
    const locations = presentation?.hunks ?? [];
    const current = activeLine ?? nearestSourceLine ?? Number.MAX_SAFE_INTEGER;
    const candidates = locations.filter((hunk) => (hunk.newLine ?? hunk.oldLine ?? 0) < current);
    const target = candidates.at(-1) ?? locations.at(-1);
    if (!target) return;
    activeLine = target.newLine ?? target.oldLine;
    jumpToRow = target.rowIndex;
    persistWorkspaceUiState({ nearestSourceLine: activeLine });
  }
  async function navigateAnnotation(direction: 1 | -1) {
    const annotations = shownAnnotations.filter(hasLineAnchor);
    if (!annotations.length) return;
    const current = activeAnnotationId
      ? annotations.findIndex((annotation) => annotation.id === activeAnnotationId)
      : -1;
    const index = (current + direction + annotations.length) % annotations.length;
    const annotation = annotations[index];
    if (!annotation) return;
    activeAnnotationId = annotation.id;
    await jumpToAnnotation(annotation);
  }
  async function requestViewport(request: Pick<import('./lib/types').ViewportRequest, 'startRow' | 'endRow'>) {
    await loadPresentation(request.startRow, request.endRow);
  }
  async function expandHunk(hunk: HunkLocation) {
    if (!activeFile || review?.historical) return;
    await api.expandHunk(activeFile.id, hunk.id, (hunk.collapsedContextLines ?? 12) + 12, activeFile.comparisonId);
    await loadPresentation(Math.max(0, hunk.rowIndex - 80), hunk.rowIndex + 160);
  }
  function saveLocation(location: { line?: number; side?: DiffSide; scrollTop: number }) {
    nearestSourceLine = location.line ?? nearestSourceLine;
    nearestSourceSide = location.side ?? nearestSourceSide;
    restoredScrollTop = location.scrollTop;
    persistWorkspaceUiState({ nearestSourceLine, nearestSourceSide, scrollTop: location.scrollTop });
  }
  function updateSplitRatio(value: number) {
    splitRatio = value;
    persistWorkspaceUiState({ splitRatio });
  }
  async function loadOutline() {
    if (!activeFileId) { outline = []; return; }
    try { outline = await api.getOutline(activeFileId, fullFileSide, activeFile?.comparisonId); }
    catch { outline = []; }
  }
  async function setFullFileSide(side: FullFileSide) {
    fullFileSide = side;
    // Persist this discrete presentation choice before loading the selected
    // side and its outline for the same shutdown-safety guarantee as mode.
    await persistWorkspaceUiStateNow({ fullFileSide: side });
    await loadPresentation(presentation?.startRow ?? 0, (presentation?.startRow ?? 0) + 220);
    await loadOutline();
  }
  async function jumpToSource(fileId: string, side: DiffSide, line: number, targetMode: DiffMode = mode) {
    activeFileId = fileId;
    activeLine = line;
    nearestSourceLine = line;
    nearestSourceSide = side;
    try {
      const location = await api.resolvePresentationLocation(fileId, targetMode, side, line, review?.files.find((file) => file.id === fileId)?.comparisonId);
      await loadPresentation(Math.max(0, location.rowIndex - 100), location.rowIndex + 120);
      jumpToRow = location.rowIndex;
      restoredScrollTop = Math.max(0, location.rowIndex * Math.round(24 * settings.fontScale));
      persistWorkspaceUiState({ activeFileId: fileId, nearestSourceLine: line, nearestSourceSide: side, scrollTop: restoredScrollTop });
    } catch (error) {
      statusMessage = `Could not locate the captured source line: ${error instanceof Error ? error.message : 'native location command failed'}`;
    }
  }

  async function jumpToAnnotation(annotation: Annotation) {
    rightTab = 'comments';
    if (!hasLineAnchor(annotation)) {
      activeAnnotationId = annotation.id;
      statusMessage = annotation.kind === 'review_note'
        ? 'This review note is intentionally not attached to a file or line.'
        : 'This file note is attached to the captured file, not a line range.';
      return;
    }
    activeAnnotationId = annotation.id;
    await jumpToSource(annotation.fileId, annotation.side, annotation.startLine);
  }

  async function editAnnotation(annotation: Annotation) {
    if (!canMutateReview) return;
    editingAnnotationId = annotation.id;
    if (hasLineAnchor(annotation)) await jumpToSource(annotation.fileId, annotation.side, annotation.startLine);
    composer = {
      row: hasLineAnchor(annotation) ? rows.find((row) => row.newLine === annotation.startLine || row.oldLine === annotation.startLine) ?? { id: annotation.id, kind: 'context', newLine: annotation.startLine, newText: annotation.selectedSource } : undefined,
      selection: hasLineAnchor(annotation) ? { side: annotation.side, startLine: annotation.startLine, endLine: annotation.endLine } : undefined,
      scope: annotation.kind === 'review_note' ? 'review' : annotation.kind === 'file_note' ? 'file' : 'inline',
      kind: annotation.kind,
      body: annotation.body,
      labels: [...annotation.labels]
    };
  }
  function toggleComposerLabel(label: string) {
    if (!composer) return;
    const labels = new Set(composer.labels ?? []);
    labels.has(label) ? labels.delete(label) : labels.add(label);
    composer = { ...composer, labels: [...labels] };
    scheduleComposerDraft();
  }
  async function removeAnnotation(annotation: Annotation) {
    if (!review || !canMutateReview) return;
    await api.deleteAnnotation(review.workspace.id, annotation.id);
    review.annotations = review.annotations.filter((value) => value.id !== annotation.id);
    if (annotation.kind !== 'review_note') review.files = review.files.map((file) => file.id === annotation.fileId ? { ...file, annotationCount: Math.max(0, file.annotationCount - 1) } : file);
    selectedAnnotationIds = new Set([...selectedAnnotationIds].filter((id) => id !== annotation.id));
    syncActiveWorkspaceSummary(true);
    persistWorkspaceUiState();
  }
  async function changeAnnotationState(annotation: Annotation, state: AnnotationState) {
    if (!review || !canMutateReview) return;
    const updated = await api.setAnnotationState(review.workspace.id, annotation.id, state);
    review.annotations = review.annotations.map((value) => value.id === updated.id ? updated : value);
    syncActiveWorkspaceSummary(true);
  }
  function toggleSelectedAnnotation(id: string) {
    if (!canExportReview) return;
    const next = new Set(selectedAnnotationIds);
    next.has(id) ? next.delete(id) : next.add(id);
    selectedAnnotationIds = next;
    persistWorkspaceUiState();
    if (showFinish) scheduleFinishPreview();
  }
  async function toggleAnnotationPublication(annotation: Annotation) {
    if (!review || !canMutateReview || annotation.publishedId) return;
    if (!hasLineAnchor(annotation)) {
      statusMessage = `${annotation.kind === 'review_note' ? 'Review' : 'File'} notes are local-only because GitHub accepts only line-anchored inline comments.`;
      return;
    }
    const updated = await api.saveAnnotation(review.workspace.id, { ...annotation, localOnly: !annotation.localOnly });
    review.annotations = review.annotations.map((value) => value.id === updated.id ? updated : value);
    if (!updated.localOnly) selectedAnnotationIds = new Set([...selectedAnnotationIds, updated.id]);
    persistWorkspaceUiState();
    if (showFinish) scheduleFinishPreview();
  }
  async function copyReviewItem(kind: CopyRequest['kind']) {
    if (!canExportReview || !review || !activeFile) return;
    try {
      const text = await api.copyReviewItem(review.workspace.id, { kind, fileId: activeFile.id, side: nearestSourceSide ?? 'new', startLine: nearestSourceLine, endLine: nearestSourceLine });
      await copyText(text);
      copiedMessage = `Copied ${kind.replace(/_/g, ' ')}.`;
      window.setTimeout(() => copiedMessage = '', 1600);
    } catch (error) { statusMessage = `Could not copy: ${error instanceof Error ? error.message : 'unknown error'}`; }
    showCopyMenu = false;
  }
  async function openExternalEditor() {
    if (!review || !activeFile) return;
    try { await api.openInExternalEditor(review.workspace.id, activeFile.id, nearestSourceLine); }
    catch (error) { statusMessage = error instanceof Error ? error.message : 'Could not open an external editor.'; }
  }
  async function refresh() {
    if (!review || !canMutateReview) return;
    busy = true;
    try {
      review = normalizeReview(await api.refreshReview(review.workspace.id, { fetchBeforeCapture: settings.fetchOnReview, comparisonOptions: comparisonOptionsSupported ? comparisonOptions : undefined }));
      syncActiveWorkspaceSummary();
      await hydrateClassifications(review.workspace.id);
      changedSincePrevious = undefined;
      changedSincePreviousOnly = false;
      activeFileId = review.files.some((file) => file.id === activeFileId) ? activeFileId : review.files[0]?.id ?? '';
      await loadPresentation(0, 220);
      statusMessage = `${settings.fetchOnReview ? 'Refreshed after fetching configured remotes.' : 'Refreshed from local refs; no automatic fetch.'} Comparison options were captured with this generation.`;
    } catch (error) {
      statusMessage = `Refresh failed: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { busy = false; }
  }

  async function beginNewReview() {
    if (!review || !canMutateReview) return;
    const workspaceId = review.workspace.id;
    busy = true;
    try {
      // Flush the old session's chrome before its atomic archive. Any later
      // save would otherwise target the replacement active session.
      await flushReviewPersistence();
      review = normalizeReview(await api.startNewReview(workspaceId, { fetchBeforeCapture: settings.fetchOnReview, comparisonOptions: comparisonOptionsSupported ? comparisonOptions : undefined }));
      await hydrateClassifications(review.workspace.id);
      const state = await api.getWorkspaceUiState(review.workspace.id);
      changedSincePrevious = undefined;
      changedSincePreviousOnly = false;
      composer = undefined;
      editingAnnotationId = undefined;
      activeSelection = undefined;
      prompt = undefined;
      promptHistoryId = undefined;
      mode = state.mode ?? 'unified';
      fullFileSide = state.fullFileSide ?? 'new';
      splitRatio = state.splitRatio ?? .5;
      rightTab = state.rightTab ?? 'files';
      nearestSourceLine = state.nearestSourceLine;
      nearestSourceSide = state.nearestSourceSide;
      restoredScrollTop = state.scrollTop ?? 0;
      activeFileId = review.files.some((file) => file.id === state.activeFileId) ? state.activeFileId! : review.files[0]?.id ?? '';
      const selectableAnnotationIds = new Set(review.annotations.filter((annotation) => annotation.state === 'open' && !annotation.publishedId).map((annotation) => annotation.id));
      selectedAnnotationIds = state.selectedAnnotationIds === undefined
        ? selectableAnnotationIds
        : new Set(state.selectedAnnotationIds.filter((id) => selectableAnnotationIds.has(id)));
      await loadPresentation(0, 220);
      await loadOutline();
      showNewReview = false;
      statusMessage = 'Archived the prior review and captured a new empty review.';
    } catch (error) {
      statusMessage = `Could not start a new review: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { busy = false; }
  }

  const setupOperationsSupported = () => Boolean(review?.workspace.source.includes('local'));
  const selectedSetupIds = () => [...selectedSetupRepositoryIds];

  function syncRepositoryBaseInputs(rows: RepositorySetup[]) {
    repositoryBases = Object.fromEntries(rows
      .filter((repository) => repository.baseOverride)
      .map((repository) => [repository.id, repository.baseOverride ?? '']));
  }

  async function loadRepositorySetup() {
    if (!review) return;
    setupLoading = true;
    try {
      const rows = await api.getRepositorySetup(review.workspace.id);
      repositorySetup = rows;
      selectedSetupRepositoryIds = new Set([...selectedSetupRepositoryIds]
        .filter((repositoryId) => rows.some((repository) => repository.id === repositoryId)));
      syncRepositoryBaseInputs(rows);
    } catch (error) {
      repositorySetup = [];
      statusMessage = `Could not read repository setup: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally {
      setupLoading = false;
    }
  }

  function toggleSetupSelection(repositoryId: string, selected: boolean) {
    const next = new Set(selectedSetupRepositoryIds);
    selected ? next.add(repositoryId) : next.delete(repositoryId);
    selectedSetupRepositoryIds = next;
  }

  async function toggleRepositoryInclusion(repositoryId: string, enabled: boolean) {
    if (!review || !setupOperationsSupported()) return;
    setupMutating = true;
    try {
      repositorySetup = await api.setRepositoryInclusion(review.workspace.id, [repositoryId], enabled);
      syncRepositoryBaseInputs(repositorySetup);
      statusMessage = `${enabled ? 'Included' : 'Excluded'} repository for the next explicit review capture.`;
    } catch (error) {
      statusMessage = `Could not change repository inclusion: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { setupMutating = false; }
  }

  async function applySetupBase() {
    if (!review || !setupOperationsSupported()) return;
    const repositoryIds = selectedSetupIds();
    if (!repositoryIds.length) { statusMessage = 'Select one or more repositories before applying an override.'; return; }
    if (!setupOverrideBase.trim()) { statusMessage = 'Enter a branch, remote branch, tag, or commit ID.'; return; }
    setupMutating = true;
    try {
      repositorySetup = await api.applyRepositoryBase(review.workspace.id, repositoryIds, setupOverrideBase.trim());
      syncRepositoryBaseInputs(repositorySetup);
      setupOverrideBase = '';
      statusMessage = `Applied the base override to ${repositoryIds.length} selected ${repositoryIds.length === 1 ? 'repository' : 'repositories'}. Refresh captures the new bases.`;
    } catch (error) {
      statusMessage = `Could not apply base override: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { setupMutating = false; }
  }

  async function resetSetupBases() {
    if (!review || !setupOperationsSupported()) return;
    const repositoryIds = selectedSetupIds();
    if (!repositoryIds.length) { statusMessage = 'Select one or more repositories before resetting overrides.'; return; }
    setupMutating = true;
    try {
      repositorySetup = await api.resetRepositoryBaseOverrides(review.workspace.id, repositoryIds);
      syncRepositoryBaseInputs(repositorySetup);
      statusMessage = `Reset ${repositoryIds.length} ${repositoryIds.length === 1 ? 'override' : 'overrides'} to the workspace default.`;
    } catch (error) {
      statusMessage = `Could not reset base overrides: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { setupMutating = false; }
  }

  async function fetchSetupRepositories(all: boolean) {
    if (!review || !setupOperationsSupported()) return;
    const selectedIds = selectedSetupIds();
    if (!all && !selectedIds.length) { statusMessage = 'Select one or more repositories before fetching.'; return; }
    setupMutating = true;
    try {
      repositorySetup = await api.fetchRepositories(review.workspace.id, all ? undefined : selectedIds);
      syncRepositoryBaseInputs(repositorySetup);
      const failures = repositorySetup.filter((repository) => repository.lastFetchError).length;
      statusMessage = failures
        ? `Fetch completed with ${failures} repository ${failures === 1 ? 'error' : 'errors'}; successful siblings are preserved.`
        : `Fetched ${all ? 'all repositories' : `${selectedIds.length} selected ${selectedIds.length === 1 ? 'repository' : 'repositories'}`}.`;
    } catch (error) {
      statusMessage = `Could not fetch repositories: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { setupMutating = false; }
  }

  async function openBaselineSetup() {
    if (!review || review.historical) return;
    setupError = '';
    workspaceBase = review.workspace.defaultBase ?? review.repositories[0]?.base ?? 'origin/master';
    repositoryBases = Object.fromEntries(review.repositories.filter((repository) => repository.isOverride).map((repository) => [repository.id, repository.base]));
    selectedSetupRepositoryIds = new Set();
    setupOverrideBase = '';
    showBaselines = true;
    await loadRepositorySetup();
  }

  function closeBaselineSetup() {
    if (review?.workspace.reviewReady === false) {
      statusMessage = 'Finish setup and capture the initial review before leaving this screen.';
      return;
    }
    showBaselines = false;
  }

  async function applyBaselines() {
    if (!review) return;
    setupError = '';
    const needsInitialReview = review.workspace.reviewReady === false;
    const workspaceId = review.workspace.id;
    const overrides: RepositoryBaseOverride[] = review.repositories.map((repository) => ({
      repositoryId: repository.id,
      base: repositoryBases[repository.id]?.trim() || null
    }));
    setupMutating = true;
    try {
      const configured = normalizeReview(await api.configureBaselines(workspaceId, workspaceBase.trim() || undefined, overrides));
      if (needsInitialReview) {
        // Keep the visible reviewReady=false guard in place until capture has
        // actually succeeded; configuring a base alone is not a review.
        review = normalizeReview(await api.startNewReview(workspaceId, { comparisonOptions }));
        // Re-enter through the normal workspace restoration path so the first
        // captured file, presentation, outline, per-session UI state, and
        // empty draft/export selection all become active immediately.
        await selectWorkspace(review.workspace.id);
        workspaces = await api.listWorkspaces();
        showBaselines = false;
        statusMessage = 'Baseline settings saved and the initial review was captured from local refs.';
        return;
      }
      review = configured;
      await loadRepositorySetup();
      statusMessage = 'Baseline settings saved. The active snapshot remains pinned until Refresh.';
    } catch (error) {
      setupError = `Could not ${needsInitialReview ? 'capture the initial review' : 'update baselines'}: ${apiFailureMessage(error, 'unknown error')}`;
      statusMessage = setupError;
      if (needsInitialReview) await loadRepositorySetup();
    } finally { setupMutating = false; }
  }

  async function openLocalWorkspace() {
    const path = localPath.trim();
    localOpenError = '';
    if (!path) {
      localOpenError = 'Enter a folder path before opening a workspace.';
      statusMessage = localOpenError;
      return;
    }
    busy = true;
    try {
      const workspace = await api.openWorkspace({ path, base: localBase.trim() || undefined });
      workspaces = await api.listWorkspaces();
      await selectWorkspace(workspace.id);
      showOpen = false;
      openLocalForm = false;
      if (workspace.reviewReady === false) {
        await openBaselineSetup();
        statusMessage = `Opened ${workspace.name}. Choose a local base below to capture the initial review; fetching remains explicit.`;
      } else {
        statusMessage = `Opened ${workspace.name}; repository discovery is available in the review setup.`;
      }
    } catch (error) {
      localOpenError = `Could not open local folder: ${apiFailureMessage(error, 'unknown error')}`;
      statusMessage = localOpenError;
    } finally { busy = false; }
  }

  async function chooseLocalFolder() {
    try {
      const result = await api.pickLocalFolder();
      if (result.path) { localPath = result.path; await openLocalWorkspace(); }
    } catch (error) {
      localOpenError = `Folder picker failed: ${apiFailureMessage(error, 'unknown error')}`;
      statusMessage = localOpenError;
    }
  }

  async function openForwardedWorkspace(kind: 'github' | 'ssh') {
    const value = (kind === 'github' ? githubPrUrl : sshTarget).trim();
    if (!value) { statusMessage = `Enter a ${kind === 'github' ? 'GitHub PR URL' : 'host:/absolute/path'} first.`; return; }
    busy = true;
    try {
      const workspace = kind === 'github' ? await api.openGitHubPr(value) : await api.openSshWorkspace(value);
      workspaces = await api.listWorkspaces();
      showOpen = false;
      openGitHubForm = false;
      openSshForm = false;
      await selectWorkspace(workspace.id);
      statusMessage = kind === 'github' ? 'Opened an isolated, read-only GitHub PR review.' : 'Opened the SSH workspace through the LocalReview companion.';
    } catch (error) {
      statusMessage = `Could not open ${kind === 'github' ? 'GitHub PR' : 'SSH workspace'}: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { busy = false; }
  }

  function handleWorkspaceDrop(event: DragEvent) {
    event.preventDefault();
    const raw = event.dataTransfer?.getData('text/uri-list') || event.dataTransfer?.getData('text/plain') || '';
    const candidate = raw.split(/\r?\n/).map((value) => value.trim()).find((value) => /^https:\/\/github\.com\/[^/\s]+\/[^/\s]+\/pull\/\d+\/?$/i.test(value));
    if (!candidate) {
      statusMessage = 'Drop a canonical GitHub.com pull-request URL to open a review.';
      return;
    }
    githubPrUrl = candidate;
    void openForwardedWorkspace('github');
  }

  async function openHistory() {
    try {
      const [archived, entries] = await Promise.all([
        api.listArchivedWorkspaces(),
        review ? api.getReviewHistory(review.workspace.id) : Promise.resolve([])
      ]);
      archivedWorkspaces = archived;
      historyEntries = entries;
      showHistory = true;
    } catch (error) {
      statusMessage = `Could not load durable review history: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  async function reconnectSshWorkspace(workspace: Workspace) {
    busy = true;
    try {
      const reconnected = await api.reconnectSshWorkspace(workspace.id);
      workspaces = workspaces.map((candidate) => candidate.id === reconnected.id ? reconnected : candidate);
      if (review?.workspace.id === reconnected.id) review = { ...review, workspace: reconnected };
      statusMessage = `Reconnected ${reconnected.name}; its change watcher was restarted.`;
    } catch (error) {
      statusMessage = `Could not reconnect ${workspace.name}: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { busy = false; }
  }

  async function toggleWorkspacePin(workspace: Workspace) {
    try {
      const updated = await api.updateWorkspaceMetadata(workspace.id, { pinned: !workspace.pinned });
      workspaces = workspaces
        .map((item) => item.id === updated.id ? updated : item)
        .sort((left, right) => Number(Boolean(right.pinned)) - Number(Boolean(left.pinned)));
      if (review?.workspace.id === updated.id) review = { ...review, workspace: updated };
      statusMessage = `${updated.name} ${updated.pinned ? 'pinned' : 'unpinned'}.`;
    } catch (error) {
      statusMessage = `Could not update workspace pin: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  function requestWorkspaceRename(workspace: Workspace) {
    workspacePendingRename = workspace;
    workspaceRenameValue = workspace.name;
  }

  async function confirmWorkspaceRename() {
    if (!workspacePendingRename || !workspaceRenameValue.trim()) return;
    try {
      const updated = await api.updateWorkspaceMetadata(workspacePendingRename.id, { name: workspaceRenameValue });
      workspaces = workspaces.map((item) => item.id === updated.id ? updated : item);
      if (review?.workspace.id === updated.id) review = { ...review, workspace: updated };
      workspacePendingRename = undefined;
      statusMessage = `Workspace renamed to ${updated.name}.`;
    } catch (error) {
      statusMessage = `Could not rename workspace: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  function requestWorkspaceDeletion(workspace: Workspace) {
    workspacePendingDeletion = workspace;
    deleteWorkspaceError = '';
    showDeleteWorkspace = true;
  }

  async function confirmWorkspaceDeletion() {
    const workspace = workspacePendingDeletion;
    if (!workspace) return;
    busy = true;
    deleteWorkspaceError = '';
    try {
      await api.deleteWorkspace(workspace.id);
      workspaces = await api.listWorkspaces();
      if (review?.workspace.id === workspace.id) {
        review = undefined;
        activeFileId = '';
        const next = workspaces[0];
        if (next) await selectWorkspace(next.id);
        else statusMessage = 'Workspace archived. Its captured review remains recoverable in Review history.';
      } else {
        statusMessage = `${workspace.name} was archived. Its captured review remains recoverable in Review history.`;
      }
      workspacePendingDeletion = undefined;
      showDeleteWorkspace = false;
    } catch (error) {
      const message = error instanceof Error ? error.message : 'unknown error';
      deleteWorkspaceError = message.includes('managed PR worktree must be clean')
        ? `The isolated PR worktree is dirty, so it was kept intact. Commit, stash, or discard those changes in that worktree, then try again.`
        : message;
    } finally { busy = false; }
  }

  async function reopenArchivedWorkspace(workspace: Workspace) {
    busy = true;
    try {
      const reopened = await api.reopenArchivedWorkspace(workspace.id);
      workspaces = await api.listWorkspaces();
      await selectWorkspace(reopened.id);
      archivedWorkspaces = await api.listArchivedWorkspaces();
      showHistory = false;
      statusMessage = `Reopened ${reopened.name} as its pinned captured review.`;
    } catch (error) {
      statusMessage = `Could not reopen archived review: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { busy = false; }
  }

  async function restoreHistory(entryId: string) {
    if (!review) return;
    if (!canMutateReview) {
      statusMessage = review.historical ? 'Return to the active review before restoring a checkpoint.' : 'Capture the initial review before restoring annotations.';
      return;
    }
    try {
      review = await api.restoreHistoryItem(review.workspace.id, entryId);
      syncActiveWorkspaceSummary(true);
      historyEntries = await api.getReviewHistory(review.workspace.id);
      statusMessage = 'Restored the archived annotation checkpoint into the active review.';
    } catch (error) {
      statusMessage = `Could not restore history: ${error instanceof Error ? error.message : 'unknown error'}`;
    }
  }

  async function browseArchivedReview(entryId: string) {
    if (!review) return;
    busy = true;
    try {
      const snapshot = normalizeReview(await api.loadArchivedReview(review.workspace.id, entryId));
      review = snapshot;
      activeFileId = snapshot.files[0]?.id ?? '';
      selectedAnnotationIds = new Set();
      mode = 'unified';
      fullFileSide = 'new';
      await loadPresentation(0, 220);
      await loadOutline();
      showHistory = false;
      statusMessage = 'Browsing the frozen archived review. Its diff and annotations are read-only.';
    } catch (error) {
      statusMessage = `Could not open archived review: ${error instanceof Error ? error.message : 'unknown error'}`;
    } finally { busy = false; }
  }

  async function toggleViewed(fileId: string, viewed: boolean) {
    if (!review || !canMutateReview) {
      if (review?.historical) statusMessage = 'Viewed state is not changed while browsing an archived review.';
      return;
    }
    await api.setViewed(review.workspace.id, fileId, viewed);
    const files = review.files.map((file) => file.id === fileId ? { ...file, viewed } : file);
    review = { ...review, files, workspace: { ...review.workspace, progress: { viewed: files.filter((file) => file.viewed).length, total: files.length } } };
    syncActiveWorkspaceSummary();
  }

  function selectFileFromKeyboard(event: KeyboardEvent, fileId: string) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      void selectFile(fileId);
    }
  }
</script>

<svelte:head><meta name="theme-color" content="#11141a" /></svelte:head>

<div class="theme-root" class:large-text-root={settings.fontScale > 1.25} data-theme={appTheme} style={`${codeStyle};--font-scale:${settings.fontScale}`}>
<main class="app-shell" style={`${layoutStyle};${codeStyle}`} data-theme={appTheme} class:show-whitespace={settings.showWhitespace} class:large-text={settings.fontScale > 1.25} aria-busy={busy} on:dragover={(event) => { if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy'; event.preventDefault(); }} on:drop={handleWorkspaceDrop}>
  <WorkspaceRail workspaces={workspaces} selectedId={activeWorkspaceId} collapsed={settings.leftCollapsed} onSelect={selectWorkspace} onOpen={() => { localOpenError = ''; showOpen = true; }} onExpand={() => restorePanel('left')} onCollapse={() => togglePanel('left')} onSettings={() => showSettings = true} onDelete={requestWorkspaceDeletion} onReconnect={reconnectSshWorkspace} onPin={toggleWorkspacePin} onRename={requestWorkspaceRename} />
  <!-- svelte-ignore a11y_no_noninteractive_tabindex a11y_no_noninteractive_element_interactions -- ARIA separator follows the splitter pattern and handles pointer plus arrow keys -->
  <div class="resize-handle left-handle" class:collapsed={settings.leftCollapsed} role="separator" tabindex="0" aria-orientation="vertical" aria-label="Resize workspace rail" aria-valuemin="180" aria-valuemax="420" aria-valuenow={settings.leftWidth} on:pointerdown={() => resizeSide = 'left'} on:keydown={(event) => resizePanelKey('left', event)} on:dblclick={() => resetDivider('left')}></div>

  <section class="review-surface" aria-label="Review surface">
    <header class="topbar">
      <div class="topbar-leading">
        {#if settings.leftCollapsed}<button class="compact-panel-button" on:click={() => restorePanel('left')} aria-label="Open workspace rail"><span class="ui-icon" aria-hidden="true">☰</span><span>Workspace</span></button>{/if}
        <button class="repo-picker" title="Choose repository" on:click={() => showFilePicker = true}><span class="repo-dot"></span>{activeRepo?.name ?? (review?.workspace.reviewReady === false ? 'Setup' : 'Loading')} <span class="chevron">⌄</span></button>
        <span class="path-divider">/</span>
        <button class="file-picker" title="Find changed file (⌘P)" on:click={() => showFilePicker = true}>{activeFile?.path ?? (review?.workspace.reviewReady === false ? 'Initial review setup' : 'Loading diff')} <span class="chevron">⌄</span></button>
        <span class:added={activeFile?.status === 'added'} class:modified={activeFile?.status === 'modified'} class="file-status">{activeFile?.status ?? ''}</span>
      </div>
      <div class="topbar-actions">
        <button class="icon-button" title="Previous file (⌘[)" on:click={previousFile}>‹</button><button class="icon-button" title="Next file (⌘])" on:click={nextFile}>›</button>
        <span class="toolbar-rule"></span>
        <button class="status-button" disabled={!canMutateReview} on:click={refresh} title={review?.historical ? 'Archived review snapshots are read-only' : !reviewCaptureReady ? 'Finish initial setup before refreshing' : 'Capture a new review snapshot'}><span class:available={review?.workspace.refreshAvailable} class="status-light"></span>{review?.historical ? 'Archived snapshot' : review?.workspace.refreshAvailable ? 'Changes available · Refresh' : 'Refresh'}</button>
        <details class="actions-menu" bind:open={actionsOpen}>
          <summary aria-label="More review actions">Actions</summary>
          <div role="menu" aria-label="Review actions"><button role="menuitem" disabled={!canExportReview || review?.historical} on:click={() => { actionsOpen = false; primaryFinishAction(); }}>{githubReview ? 'Finish review' : 'Copy review prompt'}</button><button role="menuitem" disabled={review?.historical} on:click={() => { actionsOpen = false; void openBaselineSetup(); }}>Baselines</button><button role="menuitem" disabled={!canMutateReview} on:click={() => { actionsOpen = false; showNewReview = true; }}>New review</button><button role="menuitem" on:click={() => { actionsOpen = false; void openHistory(); }}>History</button><button role="menuitem" disabled={!canExportReview} on:click={() => { actionsOpen = false; void openBlame(); }}>Blame selected lines</button><button role="menuitem" disabled={!canExportReview} on:click={() => { actionsOpen = false; void openCommitContext(); }}>Commit context</button><button role="menuitem" disabled={!canMutateReview} on:click={() => { actionsOpen = false; void showChangedSincePreviousReview(); }}>Changed since previous review</button><button role="menuitem" on:click={() => { actionsOpen = false; showSettings = true; }}>Settings</button></div>
        </details>
        <button class="finish-button" disabled={!canExportReview || review?.historical} on:click={primaryFinishAction}>{review?.historical ? 'Archived review' : githubReview ? 'Finish review' : 'Copy review prompt'} <span>⌘↵</span></button>
      </div>
    </header>

    <div class="diff-toolbar">
      <div class="base-summary"><span class="branch-icon">⑂</span><span>{activeRepo?.base ?? 'origin/master'}</span><span class="arrow">→</span><span>{activeRepo?.branch ?? 'HEAD'}</span><span class="sha">{activeRepo?.mergeBase ?? ''}</span></div>
      <div class="mode-picker" role="tablist" aria-label="Diff view">
        {#each modes as item}
          <button role="tab" aria-selected={mode === item.id} class:active={mode === item.id} on:click={() => setMode(item.id)}>{item.label}</button>
        {/each}
      </div>
      <div class="diff-stats"><span class="additions">+{activeFile?.additions ?? 0}</span><span class="deletions">−{activeFile?.deletions ?? 0}</span>{#if mode === 'full' && activeFile?.status !== 'added'}<span class="full-side-toggle" role="group" aria-label="Full-file source side"><button class:active={fullFileSide === 'new'} aria-pressed={fullFileSide === 'new'} on:click={() => setFullFileSide('new')}>Current</button><button class:active={fullFileSide === 'old'} aria-pressed={fullFileSide === 'old'} on:click={() => setFullFileSide('old')}>Base</button></span>{/if}<button class="icon-button small" title="Copy review content" aria-label="Copy review content" disabled={!canExportReview} aria-expanded={showCopyMenu} on:click={() => showCopyMenu = !showCopyMenu}>⧉</button><button class="icon-button small" title="Focus diff" aria-label="Focus diff" on:click={focusDiff}>⛶</button></div>
    </div>

    {#if review?.historical}<div class="historical-banner" role="status">Browsing archived review {review.historicalSessionId?.slice(0, 8)} · frozen diff and annotations · read-only</div>{/if}
    {#if mode === 'full' && activeFile?.status === 'deleted'}<div class="deleted-banner">This file was deleted. Showing the baseline version.</div>{/if}
    {#if showCopyMenu}
      <div class="copy-menu" role="menu" aria-label="Copy review content">
        {#each [['source', 'Copy source'], ['source_with_line_numbers', 'Copy with line numbers'], ['path', 'Copy path'], ['hunk', 'Copy hunk'], ['patch', 'Copy patch'], ['provider_permalink', 'Copy GitHub permalink']] as item}
          <button role="menuitem" disabled={item[0] === 'provider_permalink' && !review?.workspace.source.includes('github')} on:click={() => copyReviewItem(item[0] as CopyRequest['kind'])}>{item[1]}</button>
        {/each}
        {#if !review?.workspace.source.includes('github')}<button role="menuitem" on:click={openExternalEditor}>Open in external editor</button>{/if}
      </div>
    {/if}
    <VirtualDiff {rows} windowStart={presentation?.startRow ?? 0} totalRows={presentation?.totalRows ?? rows.length} hunks={presentation?.hunks ?? []} oldTokens={presentation?.oldTokens ?? []} newTokens={presentation?.newTokens ?? []} difftastic={presentation?.difftastic} {mode} fontScale={settings.fontScale} {activeLine} composerSelection={composer?.selection} composerKind={composer?.kind ?? 'comment'} {splitRatio} {fullFileSide} {jumpToRow} initialScrollTop={restoredScrollTop} restorationKey={`${activeWorkspaceId}:${activeFileId}:${mode}`} repositoryName={activeRepo?.name ?? 'repository'} filePath={activeFile?.path ?? 'file'} annotationCountAt={annotationsAt} onAnnotate={annotate} onViewportRequest={requestViewport} onExpandHunk={expandHunk} onSplitRatio={updateSplitRatio} onCanonicalMode={setMode} onLocationChange={saveLocation} />
    {#if mode === 'full'}
      <nav class="full-minimap" aria-label="Changed-line and annotation minimap">{#each presentation?.hunks ?? [] as hunk (hunk.id)}<button title={`Jump to ${hunk.header}`} aria-label={`Jump to ${hunk.header}`} style:top={`${Math.min(94, Math.max(2, ((hunk.rowIndex / Math.max(1, presentation?.totalRows ?? 1)) * 92) + 2))}%`} on:click={() => { jumpToRow = hunk.rowIndex; activeLine = hunk.newLine ?? hunk.oldLine; }}></button>{/each}{#each (review?.annotations ?? []).filter((annotation) => annotation.fileId === activeFileId && annotation.startLine > 0) as annotation (annotation.id)}<button class="annotation-marker" title={`${annotation.kind} at ${annotation.side} line ${annotation.startLine}`} aria-label={`Jump to ${annotation.kind} at ${annotation.side} line ${annotation.startLine}`} style:top={`${Math.min(96, Math.max(1, (annotation.startLine / Math.max(1, presentation?.totalRows ?? 1)) * 96))}%`} on:click={() => void jumpToAnnotation(annotation)}></button>{/each}</nav>
    {/if}

    {#if composer}
      <section class="composer" aria-label="New annotation">
        <div class="composer-header"><span>{composerLocationLabel(composer)}</span><button class="icon-button" aria-label="Close composer" on:click={closeComposer}>×</button></div>
        <div class="composer-types" role="radiogroup" aria-label="Annotation type">
          {#each [['comment', 'Comment'], ['question', 'Question'], ['suggestion', 'Suggestion'], ['file_note', 'File note'], ['review_note', 'Review note']] as item}
            <button role="radio" aria-checked={composer.kind === item[0]} class:active={composer.kind === item[0]} on:click={() => chooseComposerKind(item[0] as AnnotationKind)}>{item[1]}</button>
          {/each}
        </div>
        <div class="composer-labels" aria-label="Annotation labels">{#each ['blocking', 'important', 'nit', 'security', 'performance'] as label}<button type="button" class:active={composer.labels?.includes(label)} aria-pressed={composer.labels?.includes(label) ?? false} on:click={() => toggleComposerLabel(label)}>{label}</button>{/each}</div>
        <textarea value={composer.body} on:input={(event) => { if (composer) { composer = { ...composer, body: event.currentTarget.value }; scheduleComposerDraft(); } }} placeholder={composer.scope === 'review' ? 'Capture an overall review observation…' : composer.scope === 'file' ? 'Capture a note about this whole file…' : 'Leave clear, actionable feedback…'} aria-label="Annotation text"></textarea>
        <div class="composer-footer"><span>{composer.kind === 'question' ? 'Question prompts are local-only by default.' : composer.scope === 'review' ? 'Review notes are anchorless and stay local-only.' : composer.scope === 'file' ? 'File notes are file-level and stay local-only.' : 'Autosaved locally until you choose to publish.'}</span><div><button class="secondary-button" on:click={closeComposer}>Cancel</button><button class="primary-button" on:click={saveComposer} disabled={!composer.body.trim()}>Save annotation <kbd>⌘↵</kbd></button></div></div>
      </section>
    {/if}

    <footer class="statusbar"><span>{statusMessage}</span>{#if browserFixtureMode}<span class="dev-fixture-badge" title="Browser-only fixture; packaged Tauri uses native review data">DEV FIXTURE</span>{/if}{#if undoCheckpoint}<button on:click={undoClear}>Undo clear</button>{/if}<span class="statusbar-right"><button on:click={() => changeZoom(-.1)} aria-label="Decrease font size">A−</button><span>{codeFontPercent}%</span><button on:click={() => changeZoom(.1)} aria-label="Increase font size">A+</button></span></footer>
  </section>

  <!-- svelte-ignore a11y_no_noninteractive_tabindex a11y_no_noninteractive_element_interactions -- ARIA separator follows the splitter pattern and handles pointer plus arrow keys -->
  <div class="resize-handle right-handle" class:collapsed={settings.rightCollapsed} role="separator" tabindex="0" aria-orientation="vertical" aria-label="Resize files and review panel" aria-valuemin="240" aria-valuemax="520" aria-valuenow={settings.rightWidth} on:pointerdown={() => resizeSide = 'right'} on:keydown={(event) => resizePanelKey('right', event)} on:dblclick={() => resetDivider('right')}></div>
  {#if settings.rightCollapsed}
    <button class="right-restore" on:click={() => restorePanel('right')} aria-label="Open files and review panel">☷</button>
  {:else}
    <aside class="review-panel" aria-label="Files and review">
      <div class="panel-tabs" role="tablist">
        {#each [['files', 'Files'], ['comments', `Comments${review?.annotations.length ? ` (${review.annotations.length})` : ''}`], ['outline', 'Outline']] as tab}
          <button role="tab" aria-selected={rightTab === tab[0]} class:active={rightTab === tab[0]} on:click={() => { rightTab = tab[0] as typeof rightTab; persistWorkspaceUiState({ rightTab }); }}>{tab[1]}</button>
        {/each}
        <button class="icon-button panel-close" aria-label="Close review panel" on:click={() => togglePanel('right')}>×</button>
      </div>
      {#if rightTab === 'files'}
        <div class="panel-filter"><label class="search-field"><span>⌕</span><input bind:value={fileSearch} placeholder="Fuzzy filter files" aria-label="Filter files" /></label><div class="file-filter-grid"><select bind:value={repositoryFilter} aria-label="Filter by repository"><option value="all">All repositories</option>{#each review?.repositories ?? [] as repository}<option value={repository.id}>{repository.name}</option>{/each}</select><select bind:value={viewedFilter} aria-label="Filter by viewed state"><option value="all">All viewed states</option><option value="unviewed">Unviewed</option><option value="viewed">Viewed</option></select><select bind:value={classificationFilter} aria-label="Filter by immutable file classification"><option value="all">All file classifications</option><option value="text">Text</option><option value="binary">Binary</option><option value="generated">Generated</option><option value="vendored">Vendored</option><option value="lockfile">Lockfiles</option><option value="lfs_pointer">Git LFS pointers</option><option value="submodule">Submodules</option></select><select bind:value={fileStatusFilter} aria-label="Filter by file status"><option value="all">All statuses</option><option value="modified">Modified</option><option value="added">Added</option><option value="deleted">Deleted</option><option value="renamed">Renamed</option><option value="untracked">Untracked</option></select><select bind:value={fileLanguageFilter} aria-label="Filter by file language"><option value="all">All languages</option>{#each fileLanguages as language}<option value={language}>{language}</option>{/each}</select><select bind:value={fileGrouping} aria-label="Group files"><option value="repository">Group: repository</option><option value="folder">Group: folder</option><option value="flat">Flat list</option></select><select bind:value={fileSort} aria-label="Sort files"><option value="review_order">Sort: review order</option><option value="path">Sort: path</option><option value="repository">Sort: repository</option><option value="change_size">Sort: change size</option><option value="annotations">Sort: annotations</option></select></div>{#if changedSincePreviousOnly}<div class="history-filter-notice"><span>Changed since prior review</span><button on:click={() => { changedSincePreviousOnly = false; changedSincePrevious = undefined; }}>Show all files</button></div>{/if}<div class="file-tree-actions"><button class="secondary-button" disabled={fileGrouping === 'flat'} on:click={() => collapseAllToken += 1}>Collapse tree</button><button class="secondary-button" disabled={fileGrouping === 'flat'} on:click={() => expandAllToken += 1}>Expand tree</button></div><button class="bulk-view-button" disabled={!canMutateReview} on:click={() => Promise.all(shownFiles.filter((file) => !file.viewed).map((file) => toggleViewed(file.id, true)))}>Mark filtered viewed</button></div>
        <div class="review-progress"><div><strong>{review?.workspace.progress.viewed ?? 0}/{review?.workspace.progress.total ?? 0}</strong><span> files viewed</span></div><div class="progress-track"><span style:width={`${((review?.workspace.progress.viewed ?? 0) / (review?.workspace.progress.total ?? 1)) * 100}%`}></span></div><div class="repository-progress" aria-label="Review progress by repository">{#each review?.repositories ?? [] as repository}{@const repositoryFiles = (review?.files ?? []).filter((file) => file.repositoryId === repository.id)}<span title={`${repository.name}: ${repositoryFiles.filter((file) => file.viewed).length} of ${repositoryFiles.length} viewed`}>{repository.name} {repositoryFiles.filter((file) => file.viewed).length}/{repositoryFiles.length}</span>{/each}</div></div>
        <VirtualFileList files={shownFiles} repositories={review?.repositories ?? []} grouping={fileGrouping} {activeFileId} fontScale={settings.fontScale} {collapseAllToken} {expandAllToken} onSelect={selectFile} onToggleViewed={toggleViewed} />
      {:else if rightTab === 'comments'}
        {#if githubReview}
          <section class="github-context" aria-label="Imported GitHub pull request context">
            <div class="github-context-header"><span>GITHUB · IMPORTED CONTEXT</span>{#if githubContextLoading}<small>Loading…</small>{:else}<small>Read-only</small>{/if}</div>
            {#if githubContext}
              <h3>{githubContext.title}</h3><p>{githubContext.author ? `@${githubContext.author}` : 'Unknown author'} · {githubContext.state}{githubContext.review_decision ? ` · ${githubContext.review_decision}` : ''}{githubContext.draft ? ' · Draft' : ''}</p>
              <div class="github-ref-summary"><code>{githubContext.base_ref}@{githubContext.pinned_base_sha.slice(0, 8)}</code><span>→</span><code>{githubContext.head_ref}@{githubContext.pinned_head_sha.slice(0, 8)}</code><span>{githubContext.commits.length} commits</span></div>
              {#if githubContext.import_error}<p class="github-import-error">Imported context is incomplete: {githubContext.import_error}</p>{/if}
            {:else if !githubContextLoading}<p class="github-context-unavailable">GitHub context could not be loaded. Your pinned local review and annotations remain available.</p>{/if}
            {#if githubThreads.length}<div class="github-import-group"><strong>Imported review threads ({githubThreads.length})</strong>{#each githubThreads as thread (thread.id)}<article class:resolved={thread.resolved} class:outdated={thread.outdated} class="github-thread"><div><span class="github-thread-state">{thread.resolved ? 'Resolved' : thread.outdated ? 'Outdated' : 'Open'}</span><span>{thread.path ?? 'General thread'}{thread.line ? `:${thread.line}` : ''}</span></div>{#each thread.comments as comment (comment.id)}<p><strong>{comment.author ? `@${comment.author}` : 'GitHub user'}</strong> {comment.body_markdown}</p>{/each}</article>{/each}</div>{/if}
            {#if githubConversation.length}<div class="github-import-group github-conversation"><strong>Imported general conversation ({githubConversation.length})</strong>{#each githubConversation as comment (comment.id)}<article><p><strong>{comment.author ? `@${comment.author}` : 'GitHub user'}</strong> {comment.body_markdown}</p></article>{/each}</div>{/if}
          </section>
        {/if}
        <div class="comment-actions"><button class="primary-button" disabled={!canExportReview} on:click={() => previewPrompt('feedback', undefined)}>Copy feedback prompt</button><button class="secondary-button" disabled={!canExportReview} on:click={() => previewPrompt('questions', undefined)}>Questions</button><button class="secondary-button" disabled={!canExportReview || !selectedAnnotationIds.size} on:click={() => previewPrompt('selected', undefined)}>Selected ({selectedAnnotationIds.size})</button><button class="secondary-button" disabled={!canMutateReview} on:click={startQuestion}>Ask question</button><button class="secondary-button" disabled={!canMutateReview || !activeFile} on:click={startFileNote}>File note</button><button class="secondary-button" disabled={!canMutateReview} on:click={startReviewNote}>Review note</button><button class="secondary-button" disabled={!shownAnnotations.some(hasLineAnchor)} on:click={() => void navigateAnnotation(-1)} aria-label="Previous annotation">‹ Annotation</button><button class="secondary-button" disabled={!shownAnnotations.some(hasLineAnchor)} on:click={() => void navigateAnnotation(1)} aria-label="Next annotation">Annotation ›</button><button class="secondary-button destructive" disabled={!canMutateReview} on:click={() => showClear = true}>Clear</button></div>
        <div class="comment-filters" aria-label="Filter local annotations"><select bind:value={annotationKindFilter} aria-label="Filter comments by kind"><option value="all">All kinds</option><option value="comment">Comments</option><option value="question">Questions</option><option value="suggestion">Suggestions</option><option value="file_note">File notes</option><option value="review_note">Review notes</option></select><select bind:value={annotationStateFilter} aria-label="Filter comments by state"><option value="all">All states</option><option value="open">Open</option><option value="resolved">Resolved</option><option value="outdated">Outdated</option></select><select bind:value={annotationPublicationFilter} aria-label="Filter comments by publication"><option value="all">All publication states</option><option value="published">Published</option><option value="unpublished">Unpublished</option><option value="local_only">Local only</option></select><select bind:value={annotationLabelFilter} aria-label="Filter comments by label"><option value="all">All labels</option><option value="blocking">Blocking</option><option value="important">Important</option><option value="nit">Nit</option><option value="security">Security</option><option value="performance">Performance</option><option value="question">Question</option></select></div>
        <div class="comment-list">
          {#each shownAnnotations as annotation (annotation.id)}
            <article class:active={activeAnnotationId === annotation.id} class:outdated={annotation.state === 'outdated'} class="comment-card">
              <div class="comment-card-head"><span class="annotation-kind {annotation.kind}">{annotation.kind.replace('_', ' ')}</span><span>{annotation.kind === 'review_note' ? 'Whole review' : `${review?.files.find((file) => file.id === annotation.fileId)?.path ?? 'Captured file'}${hasLineAnchor(annotation) ? `:${annotation.startLine}` : ''}`}</span>{#if annotation.publishedId}<span class="published">Published</span>{:else if annotation.localOnly}<span class="local-only">Local only</span>{/if}</div>
              {#if hasLineAnchor(annotation)}<button class="comment-jump" on:click={() => jumpToAnnotation(annotation)}>Jump to code</button>{:else}<span class="annotation-anchorless">{annotation.kind === 'review_note' ? 'Anchorless review note' : 'File-level note'}</span>{/if}<p>{annotation.body}</p>{#if annotation.labels.length}<div class="annotation-labels">{#each annotation.labels as label}<span>{label}</span>{/each}</div>{/if}{#if annotation.selectedSource}<code>{annotation.selectedSource}</code>{/if}<div class="annotation-controls"><label><input type="checkbox" checked={selectedAnnotationIds.has(annotation.id)} disabled={!canExportReview} aria-label={`Include ${annotation.kind.replace('_', ' ')} in exports and Finish review`} on:change={() => toggleSelectedAnnotation(annotation.id)} /> Include</label><label><input type="checkbox" checked={!annotation.localOnly} disabled={Boolean(annotation.publishedId) || !hasLineAnchor(annotation) || !canMutateReview} aria-label={`Publish ${annotation.kind.replace('_', ' ')} to GitHub`} on:change={() => toggleAnnotationPublication(annotation)} /> Publish to GitHub</label><button disabled={!canMutateReview} on:click={() => editAnnotation(annotation)}>Edit</button>{#if annotation.state === 'open'}<button disabled={!canMutateReview} aria-pressed="false" on:click={() => changeAnnotationState(annotation, 'resolved')}>Resolve</button>{:else}<button disabled={!canMutateReview} aria-pressed="true" on:click={() => changeAnnotationState(annotation, 'open')}>Reopen</button>{/if}<button class="destructive" disabled={!canMutateReview} on:click={() => removeAnnotation(annotation)}>Delete</button>{#if annotation.kind === 'question'}<button disabled={!canExportReview} on:click={() => previewPrompt('focused_question', undefined, annotation.id)}>Prompt</button>{/if}</div>
            </article>
          {:else}<div class="empty-state large"><span>◌</span><strong>{(review?.annotations.length ?? 0) ? 'No annotations match these filters' : 'No active annotations'}</strong><p>{(review?.annotations.length ?? 0) ? 'Change or clear a filter to see another local annotation.' : 'Add a comment from a code gutter, or browse archived sets in History.'}</p></div>{/each}
        </div>
      {:else}
        <div class="outline-header"><strong>Outline</strong><span>from Tree-sitter</span></div>
        <div class="outline-list">{#each outline as symbol (symbol.id)}<button style:padding-left={`${12 + symbol.depth * 14}px`} on:click={() => jumpToSource(activeFileId, symbol.side, symbol.startLine)}><span>{symbol.kind === 'function' || symbol.kind === 'method' ? 'ƒ' : '▣'}</span><code>{symbol.name}</code><small>{symbol.startLine}–{symbol.endLine}</small></button>{:else}<div class="empty-state">No outline is available for this captured file.</div>{/each}</div>
        <div class="outline-footer">The outline is derived from the immutable captured snapshot.</div>
      {/if}
    </aside>
  {/if}
</main>

{#if zoomToast}<div class="zoom-toast" role="status">{zoomToast}</div>{/if}

{#if prompt}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal prompt-modal" aria-modal="true" aria-labelledby="prompt-title" use:focusTrap={{ onClose: () => prompt = undefined }}><header><div><span class="eyebrow">STRUCTURED EXPORT</span><h2 id="prompt-title">{prompt.title}</h2><p>{prompt.annotationCount} annotations · about {prompt.estimatedTokens.toLocaleString()} tokens</p></div><button class="icon-button" on:click={() => prompt = undefined} aria-label="Close prompt preview">×</button></header>{#if promptHistoryId?.startsWith('export:')}<div class="prompt-exact-note" role="status">Exact durable export · original Markdown and path mode are read-only.</div>{:else}<div class="prompt-scopes" role="group" aria-label="Prompt scope"><button aria-pressed={promptScope === 'feedback'} class:active={promptScope === 'feedback'} on:click={() => previewPrompt('feedback')}>Feedback</button><button aria-pressed={promptScope === 'questions'} class:active={promptScope === 'questions'} on:click={() => previewPrompt('questions')}>Questions</button><button aria-pressed={promptScope === 'all'} class:active={promptScope === 'all'} on:click={() => previewPrompt('all')}>Full</button><span class="prompt-path-mode" aria-label="Prompt path mode"><button aria-pressed={promptPortable} class:active={promptPortable} on:click={() => { promptPortable = true; void previewPrompt(promptScope); }}>Portable paths</button><button aria-pressed={!promptPortable} class:active={!promptPortable} on:click={() => { promptPortable = false; void previewPrompt(promptScope); }}>Qualified paths</button></span></div>{/if}{#if promptNeedsLargeCopyWarning()}<div class="prompt-size-warning" role="alert">This prompt is unusually large. Copying can exceed clipboard or model limits; it remains unchanged unless you choose to copy it.</div>{/if}<pre>{prompt.content}</pre><footer><span>{promptHistoryId?.startsWith('export:') ? 'Exact durable export; copy and saves never alter annotations.' : promptPortable ? 'Portable prompt: no local filesystem paths.' : 'Qualified prompt: repository-qualified logical paths; no local filesystem roots.'}</span><div><button class="secondary-button" on:click={() => savePrompt('markdown')}>Save Markdown…</button><button class="secondary-button" on:click={() => savePrompt('json')}>Save JSON…</button><button class="secondary-button" on:click={() => prompt = undefined}>Close</button><button class="primary-button" on:click={() => copyPrompt(largePromptCopyWarning)}>{largePromptCopyWarning ? 'Copy large prompt anyway' : 'Copy prompt'}</button></div></footer></dialog></div>
{/if}

{#if showHistory}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal history-modal" aria-modal="true" aria-labelledby="history-title" use:focusTrap={{ onClose: () => showHistory = false }}><header><div><span class="eyebrow">DURABLE REVIEW DATA</span><h2 id="history-title">Review history</h2><p>Each workspace has one current review. Prior reviews are frozen here and remain available after restart.</p></div><button class="icon-button" on:click={() => showHistory = false}>×</button></header>{#if archivedWorkspaces.length}<section class="archived-workspaces" aria-label="Archived workspaces"><strong>Archived workspaces</strong>{#each archivedWorkspaces as workspace (workspace.id)}<article><div><strong>{workspace.name}</strong><p>{workspace.source.join(' + ')} · {workspace.location}</p><small>{workspace.detail} · {workspace.progress.total} captured files</small></div><button class="secondary-button" on:click={() => reopenArchivedWorkspace(workspace)}>Reopen snapshot</button></article>{/each}</section>{/if}<div class="history-list">{#each historyEntries as entry}<article><span class="history-type {entry.type}">{entry.type}</span><div><strong>{entry.label}</strong><p>{new Date(entry.createdAt).toLocaleString()} · {entry.annotationCount} annotations</p></div><div class="history-actions">{#if entry.type === 'review'}<button class="secondary-button" on:click={() => browseArchivedReview(entry.id)}>Browse frozen diff</button>{/if}<button class="secondary-button" on:click={() => previewPrompt('all', entry.id)}>{entry.type === 'export' ? 'Open exact export' : 'Export'}</button>{#if entry.annotations?.length}<button class="secondary-button" disabled={review?.historical} on:click={() => restoreHistory(entry.id)}>Restore</button>{/if}</div></article>{:else}{#if !archivedWorkspaces.length}<div class="empty-state">No review history yet.</div>{/if}{/each}</div><footer><button class="primary-button" on:click={() => showHistory = false}>Done</button></footer></dialog></div>
{/if}

{#if showDeleteWorkspace}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal confirm-modal" aria-modal="true" aria-labelledby="delete-workspace-title" use:focusTrap={{ onClose: () => { showDeleteWorkspace = false; workspacePendingDeletion = undefined; } }}><header><span class="warning-icon">!</span><div><h2 id="delete-workspace-title">Remove {workspacePendingDeletion?.name}?</h2>{#if workspacePendingDeletion?.source.includes('github')}<p>The app-owned PR worktree will be deleted only if it is clean. The shared repository cache and all captured review history stay intact.</p>{:else}<p>This removes the workspace from the live rail. Its captured diff, annotations, and exports remain recoverable in Review history.</p>{/if}</div></header>{#if deleteWorkspaceError}<p class="modal-error" role="alert">{deleteWorkspaceError}</p>{/if}<footer><button class="secondary-button" on:click={() => { showDeleteWorkspace = false; workspacePendingDeletion = undefined; }}>Cancel</button><button class="primary-button warning" disabled={busy} on:click={confirmWorkspaceDeletion}>{workspacePendingDeletion?.source.includes('github') ? 'Delete clean worktree and archive' : 'Archive workspace'}</button></footer></dialog></div>
{/if}

{#if workspacePendingRename}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal confirm-modal" aria-modal="true" aria-labelledby="rename-workspace-title" use:focusTrap={{ onClose: () => workspacePendingRename = undefined }}><header><div><span class="eyebrow">WORKSPACE RAIL</span><h2 id="rename-workspace-title">Rename workspace</h2><p>This changes only the durable display name. Paths, captures, and history stay unchanged.</p></div></header><div class="setup-content"><label>Workspace name<input data-dialog-initial-focus bind:value={workspaceRenameValue} maxlength="120" on:keydown={(event) => { if (event.key === 'Enter') { event.preventDefault(); void confirmWorkspaceRename(); } }} /></label></div><footer><button class="secondary-button" on:click={() => workspacePendingRename = undefined}>Cancel</button><button class="primary-button" disabled={!workspaceRenameValue.trim()} on:click={confirmWorkspaceRename}>Rename</button></footer></dialog></div>
{/if}

{#if showClear}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal confirm-modal" aria-modal="true" aria-labelledby="clear-title" use:focusTrap={{ onClose: () => showClear = false }}><header><span class="warning-icon">!</span><div><h2 id="clear-title">Archive active annotations?</h2><p>{review?.annotations.length ?? 0} comments, questions, and suggestions will be moved into a recoverable history checkpoint.</p></div></header><footer><button class="secondary-button" on:click={() => showClear = false}>Cancel</button><button class="primary-button warning" on:click={confirmClear}>Archive and clear</button></footer></dialog></div>
{/if}

{#if showBlame}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal context-modal" aria-modal="true" aria-labelledby="blame-title" use:focusTrap={{ onClose: () => showBlame = false }}><header><div><span class="eyebrow">CAPTURED ATTRIBUTION</span><h2 id="blame-title">Blame selected lines</h2><p>Read-only attribution is pinned to this review’s captured revision, never a moving branch.</p></div><button class="icon-button" aria-label="Close blame" on:click={() => showBlame = false}>×</button></header><div class="context-content" aria-live="polite">{#if blameLoading}<div class="empty-state">Loading captured blame…</div>{:else if blameResult?.lines.length}{#each blameResult.lines as line (`${line.revision}:${line.finalLine}`)}<article class="blame-line"><header><code>{line.revision.slice(0, 12)}</code><strong>{line.authorName}</strong><time>{line.authorTime}</time></header><p>{line.summary} · source line {line.finalLine}</p><code>{line.source}{line.sourceTruncated ? ' …' : ''}</code></article>{/each}{:else}<div class="empty-state">No captured blame is available for this selection.</div>{/if}</div><footer><button class="primary-button" on:click={() => showBlame = false}>Done</button></footer></dialog></div>
{/if}

{#if showCommitContext}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal context-modal" aria-modal="true" aria-labelledby="commit-context-title" use:focusTrap={{ onClose: () => showCommitContext = false }}><header><div><span class="eyebrow">CAPTURED COMMITS</span><h2 id="commit-context-title">Commit context</h2><p>{commitContext ? `${commitContext.range.mergeBase.slice(0, 12)} → ${commitContext.range.head.slice(0, 12)}` : 'Loading captured commit range…'}</p></div><button class="icon-button" aria-label="Close commit context" on:click={() => showCommitContext = false}>×</button></header><div class="context-content"><div class="commit-filter-grid"><label>Author<input bind:value={commitAuthorFilter} on:change={() => void loadCommitContext()} placeholder="Filter author" /></label><label>Subject<input bind:value={commitSubjectFilter} on:change={() => void loadCommitContext()} placeholder="Filter subject" /></label><label class="fetch-setting"><input type="checkbox" bind:checked={includeMergeCommits} on:change={() => void loadCommitContext()} /> Include merge commits</label></div>{#if commitContextLoading}<div class="empty-state">Loading immutable commit metadata…</div>{:else if commitContext?.commits.length}<div class="commit-list">{#each commitContext.commits as commit (commit.sha)}<button class:active={commitContext.selectedCommit?.summary.sha === commit.sha} on:click={() => void loadCommitContext(commit.sha)}><code>{commit.sha.slice(0, 12)}</code><span><strong>{commit.subject}</strong><small>{commit.authorName} · {commit.authoredAt}</small></span></button>{/each}</div>{:else}<div class="empty-state">No commits match this captured range and filter.</div>{/if}{#if commitContext?.truncated}<p class="form-hint">The list is capped at 100 captured commits; narrow the filters for a smaller view.</p>{/if}{#if commitContext?.selectedCommit}<article class="commit-details"><h3>{commitContext.selectedCommit.summary.subject}</h3><p><code>{commitContext.selectedCommit.summary.sha}</code> · committed by {commitContext.selectedCommit.committerName} at {commitContext.selectedCommit.committedAt}</p><pre>{commitContext.selectedCommit.body}</pre></article>{/if}</div><footer><button class="primary-button" on:click={() => showCommitContext = false}>Done</button></footer></dialog></div>
{/if}

{#if showFinish}
  <div class="modal-backdrop" role="presentation">
    <dialog open class="modal finish-modal" aria-modal="true" aria-labelledby="finish-title" use:focusTrap={{ onClose: () => closeFinishReview() }}>
      <header>
        <div><span class="eyebrow">GITHUB REVIEW</span><h2 id="finish-title">Finish review</h2><p>One native GitHub review will include {finishPreview?.annotationCount ?? finishAnnotations.length} publishable inline annotations. Every selected local item is listed below before anything is submitted.</p></div>
        <button class="icon-button" aria-label="Close Finish review" disabled={finishSubmitting || finishSubmissionAmbiguous} on:click={() => closeFinishReview()}>×</button>
      </header>
      <div class="finish-content">
        <label>Overall review summary<textarea bind:value={finishSummary} disabled={finishSubmitting || finishSubmissionAmbiguous} on:input={scheduleFinishPreview} placeholder="Optional summary for this review"></textarea></label>
        <div>
          <span class="label">Conclusion</span>
          <div class="conclusion-options" role="radiogroup" aria-label="Review conclusion">{#each [['comment', 'Comment'], ['approve', 'Approve'], ['request_changes', 'Request changes']] as item}<button role="radio" aria-checked={finishConclusion === item[0]} class:active={finishConclusion === item[0]} disabled={finishSubmitting || finishSubmissionAmbiguous} on:click={() => { finishConclusion = item[0] as ReviewConclusion; scheduleFinishPreview(); }}>{item[1]}</button>{/each}</div>
        </div>
        <section class="finish-items" aria-label="Selected review items">
          <strong>Selected local items ({selectableFinishAnnotations.length})</strong>
          {#each selectableFinishAnnotations as annotation (annotation.id)}<article class:blocked={annotation.localOnly || !hasLineAnchor(annotation) || annotation.state !== 'open'} class:failed={finishPreviewError?.annotationId === annotation.id}><div><span class="annotation-kind {annotation.kind}">{annotation.kind.replace('_', ' ')}</span><code>{annotation.kind === 'review_note' ? 'whole review' : `${review?.files.find((file) => file.id === annotation.fileId)?.path ?? 'captured file'}${hasLineAnchor(annotation) ? `:${annotation.startLine}` : ''}`}</code></div><p>{finishItemStatus(annotation)}</p><small>{annotation.body}</small></article>{:else}<p class="finish-empty">No local annotations are selected. Select an inline annotation in Comments before preparing a GitHub review.</p>{/each}
        </section>
        <div class="publish-preview" aria-live="polite">
          <span>Exact native payload preview</span>
          <strong>{finishPreviewLoading ? 'Preparing server-authoritative payload…' : `${finishPreview?.annotationCount ?? finishAnnotations.length} inline comments · ${finishSummary ? '1 summary' : 'no summary'} · ${finishConclusion.replace('_', ' ')}`}</strong>
          <p>{finishPreview ? `Pinned to ${finishPreview.pinnedHeadSha}. This is the JSON submitted by the native GitHub boundary.` : 'Payload is prepared and anchor-validated before submission.'}</p>
          {#if finishPreviewError}<p class="finish-preview-error" role="alert">{finishPreviewError.message}</p>{/if}
          {#if finishSubmissionError}<p class="finish-preview-error" role="alert">{finishSubmissionError}</p>{/if}
          {#if finishSubmissionAmbiguous}
            <div class="ambiguous-publication" role="alert">
              <strong>GitHub may already have accepted this review.</strong>
              <p>Check GitHub again to reconcile the durable attempt. LocalReview will not issue another POST while its outcome is unresolved. If you confirm it was not created, you can explicitly abandon the attempt and prepare a new review.</p>
              <button class="secondary-button warning" disabled={finishSubmitting} on:click={abandonUnresolvedFinishReview}>Abandon unresolved attempt</button>
            </div>
          {/if}
          {#if finishPreview}<pre aria-label="Exact GitHub review payload">{finishPreview.payloadJson}</pre>{/if}
        </div>
      </div>
      <footer>
        <button class="secondary-button" disabled={finishSubmitting || finishSubmissionAmbiguous} on:click={() => closeFinishReview()}>Cancel</button>
        <button class="primary-button" disabled={(!finishAnnotations.length && !finishPreview?.requiresReconciliation) || finishPreviewLoading || !finishPreview || finishSubmitting} on:click={submitReview}>{finishSubmitting ? 'Checking GitHub…' : finishSubmissionAmbiguous ? 'Check GitHub again' : 'Submit one review'}</button>
      </footer>
    </dialog>
  </div>
{/if}

{#if showBaselines}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal baseline-modal" aria-modal="true" aria-labelledby="baseline-title" use:focusTrap={{ onClose: () => showBaselines = false }}><header><div><span class="eyebrow">REVIEW SETUP</span><h2 id="baseline-title">Baselines and comparison</h2><p>Setup never replaces this pinned review. It reads local Git status only when opened or when you press Refresh setup; fetching is always an explicit action.</p></div><button class="icon-button" aria-label="Close review setup" on:click={() => showBaselines = false}>×</button></header><div class="setup-content"><label>Workspace default base<input bind:value={workspaceBase} aria-label="Workspace default base" placeholder="origin/master" /></label>{#if setupError}<p class="modal-error" role="alert">{setupError}</p>{/if}{#if setupLoading}<div class="empty-state">Reading repository status and local refs…</div>{:else if repositorySetup.length}<section class="repository-setup" aria-labelledby="repository-setup-heading"><div class="setup-table-header"><div><h3 id="repository-setup-heading">Repositories</h3><p>Base source shows whether a repository inherits the workspace default or has its own override.</p></div><button class="secondary-button" disabled={setupMutating} on:click={() => void loadRepositorySetup()}>Refresh setup</button></div>{#if setupOperationsSupported()}<div class="setup-bulk-controls"><label>Base for selected repositories<input bind:value={setupOverrideBase} aria-label="Base override for selected repositories" placeholder="origin/HOTFIX-1, v2.0.0, or commit ID" /></label><div><button class="secondary-button" disabled={setupMutating || !selectedSetupRepositoryIds.size} on:click={() => void applySetupBase()}>Apply override</button><button class="secondary-button" disabled={setupMutating || !selectedSetupRepositoryIds.size} on:click={() => void resetSetupBases()}>Reset to inherited</button><button class="secondary-button" disabled={setupMutating || !selectedSetupRepositoryIds.size} on:click={() => void fetchSetupRepositories(false)}>Fetch selected</button><button class="secondary-button" disabled={setupMutating} on:click={() => void fetchSetupRepositories(true)}>Fetch all</button></div></div>{:else}<p class="form-hint">Repository inclusion, local-ref fetch, and base overrides are available for Local workspaces. This provider-pinned review is read-only here.</p>{/if}<div class="setup-table-wrap"><table class="repository-setup-table"><thead><tr><th scope="col">Use</th><th scope="col">Repository</th><th scope="col">Working tree</th><th scope="col">Effective base</th><th scope="col">Resolved revisions</th><th scope="col">Divergence</th><th scope="col">Fetch / errors</th></tr></thead><tbody>{#each repositorySetup as repository (repository.id)}<tr class:excluded={!repository.enabled}><td>{#if setupOperationsSupported()}<label class="setup-check"><input type="checkbox" checked={selectedSetupRepositoryIds.has(repository.id)} aria-label={`Select ${repository.path}`} on:change={(event) => toggleSetupSelection(repository.id, event.currentTarget.checked)} /><input type="checkbox" checked={repository.enabled} disabled={setupMutating} aria-label={`Include ${repository.path} in the next review`} on:change={(event) => void toggleRepositoryInclusion(repository.id, event.currentTarget.checked)} /><span>{repository.enabled ? 'Included' : 'Excluded'}</span></label>{:else}<span>{repository.enabled ? 'Included' : 'Excluded'}</span>{/if}</td><td><strong>{repository.path}</strong><small>{repository.branch}</small></td><td><span class:dirty={repository.clean === false} class:clean={repository.clean === true}>{repository.statusSummary}</span>{#if repository.changedFileCount !== undefined}<small>{repository.changedFileCount} changed {repository.changedFileCount === 1 ? 'file' : 'files'}{repository.statusCheckedAt ? ` · checked ${new Date(repository.statusCheckedAt).toLocaleTimeString()}` : ''}</small>{/if}</td><td><code>{repository.effectiveBase}</code><small>{repository.baseSource}{repository.baseOverride ? ' · explicit override' : ''}</small></td><td><small>Base <code>{repository.resolvedBaseSha?.slice(0, 12) ?? 'unresolved'}</code></small><small>Merge <code>{repository.mergeBaseSha?.slice(0, 12) ?? 'unresolved'}</code></small><small>HEAD <code>{repository.headSha?.slice(0, 12) ?? 'unresolved'}</code></small></td><td>{#if repository.ahead !== undefined || repository.behind !== undefined}<span>↑ {repository.ahead ?? '—'} · ↓ {repository.behind ?? '—'}</span>{:else}<span>Unavailable</span>{/if}</td><td>{#if repository.lastFetchAt}<small>Last fetch {new Date(repository.lastFetchAt).toLocaleString()}</small>{:else}<small>Not fetched by LocalReview</small>{/if}{#if repository.lastFetchError || repository.discoveryError || repository.comparisonError}<p class="setup-error" role="alert">{repository.lastFetchError ?? repository.discoveryError ?? repository.comparisonError}</p>{/if}</td></tr>{/each}</tbody></table></div></section>{:else}<div class="empty-state">No repositories are configured for this workspace.</div>{/if}<fieldset class="comparison-options"><legend>Capture comparison options</legend><label class="fetch-setting"><input type="checkbox" checked={comparisonOptions.ignoreAllWhitespace} disabled={!comparisonOptionsSupported} on:change={(event) => comparisonOptions = { ...comparisonOptions, ignoreAllWhitespace: event.currentTarget.checked }} /> Ignore all whitespace changes</label><label class="fetch-setting"><input type="checkbox" checked={comparisonOptions.ignoreSpaceAtEol} disabled={!comparisonOptionsSupported || comparisonOptions.ignoreAllWhitespace} on:change={(event) => comparisonOptions = { ...comparisonOptions, ignoreSpaceAtEol: event.currentTarget.checked }} /> Ignore whitespace at end of line</label><label class="fetch-setting"><input type="checkbox" checked={comparisonOptions.ignoreCrAtEol} disabled={!comparisonOptionsSupported} on:change={(event) => comparisonOptions = { ...comparisonOptions, ignoreCrAtEol: event.currentTarget.checked }} /> Ignore carriage return at end of line (CRLF)</label><small>{comparisonOptionsSupported ? 'These are Git comparison inputs, not presentation-only whitespace toggles. They apply on your next explicit Refresh or new review, including through the SSH companion.' : 'This GitHub PR uses provider-pinned base and head revisions, so comparison switches are unavailable.'}</small></fieldset><label class="fetch-setting"><input type="checkbox" checked={settings.fetchOnReview} on:change={(event) => setSettings({ fetchOnReview: event.currentTarget.checked })} /> Fetch remotes whenever I explicitly refresh this review</label></div><footer><button class="secondary-button" on:click={() => showBaselines = false}>Done</button><button class="primary-button" disabled={setupMutating} on:click={applyBaselines}>{review?.workspace.reviewReady === false ? 'Save and start review' : 'Save workspace default'}</button></footer></dialog></div>
{/if}

{#if showNewReview}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal confirm-modal" aria-modal="true" aria-labelledby="new-review-title" use:focusTrap={{ onClose: () => showNewReview = false }}><header><span class="warning-icon">+</span><div><h2 id="new-review-title">Start a new review?</h2><p>The current review and its active annotations will be archived. The new captured review starts with no annotations.</p></div></header><footer><button class="secondary-button" on:click={() => showNewReview = false}>Cancel</button><button class="primary-button" on:click={beginNewReview}>Archive and start review</button></footer></dialog></div>
{/if}

{#if showOpen}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal open-modal" aria-modal="true" aria-labelledby="open-title" use:focusTrap={{ onClose: () => { showOpen = false; openLocalForm = false; openGitHubForm = false; openSshForm = false; localOpenError = ''; } }}><header><div><span class="eyebrow">OPEN OR CONNECT</span><h2 id="open-title">Add a workspace</h2></div><button class="icon-button" on:click={() => { showOpen = false; openLocalForm = false; openGitHubForm = false; openSshForm = false; localOpenError = ''; }}>×</button></header>{#if openLocalForm}<div class="setup-content"><label>Local folder path<input bind:value={localPath} on:input={() => localOpenError = ''} aria-label="Local folder path" placeholder="/Users/me/Projects/workspace" /></label><label>Default base reference<input bind:value={localBase} on:input={() => localOpenError = ''} aria-label="Default base reference" placeholder="origin/master" /></label><p class="form-hint">In the desktop app, use the native folder picker. In browser development, enter a path to exercise the same open contract.</p>{#if localOpenError}<p class="modal-error" role="alert">{localOpenError}</p>{/if}</div><footer><button class="secondary-button" on:click={() => { openLocalForm = false; localOpenError = ''; }}>Back</button><button class="secondary-button" disabled={busy} on:click={chooseLocalFolder}>Choose folder…</button><button class="primary-button" disabled={busy} on:click={openLocalWorkspace}>{busy ? 'Opening…' : 'Open local folder'}</button></footer>{:else if openGitHubForm}<div class="setup-content"><label>GitHub pull request URL<input bind:value={githubPrUrl} aria-label="GitHub pull request URL" placeholder="https://github.com/owner/repository/pull/123" /></label><p class="form-hint">The desktop app resolves the exact GitHub.com head/base and prepares a read-only worktree.</p></div><footer><button class="secondary-button" on:click={() => openGitHubForm = false}>Back</button><button class="primary-button" on:click={() => openForwardedWorkspace('github')}>Open PR review</button></footer>{:else if openSshForm}<div class="setup-content"><label>SSH target<input bind:value={sshTarget} aria-label="SSH target" placeholder="build@host:/absolute/path" /></label><p class="form-hint">The desktop app launches the managed LocalReview companion over your SSH configuration.</p></div><footer><button class="secondary-button" on:click={() => openSshForm = false}>Back</button><button class="primary-button" on:click={() => openForwardedWorkspace('ssh')}>Connect workspace</button></footer>{:else}<div class="open-options"><button on:click={() => { localOpenError = ''; openLocalForm = true; }}><span>⌂</span><strong>Open local folder</strong><small>Discover repositories under a local directory.</small></button><button on:click={() => openGitHubForm = true}><span>⌘</span><strong>Paste GitHub PR URL</strong><small>Create an isolated, read-only PR worktree.</small></button><button on:click={() => openSshForm = true}><span>↗</span><strong>Connect over SSH</strong><small>Review code through a LocalReview companion.</small></button><button on:click={() => { showOpen = false; openHistory(); }}><span>◴</span><strong>Reopen archived review</strong><small>Browse durable review history.</small></button></div>{/if}</dialog></div>
{/if}

{#if showFilePicker}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal picker-modal" aria-modal="true" aria-labelledby="file-picker-title" use:focusTrap={{ onClose: () => showFilePicker = false }}><header><div><span class="eyebrow">CHANGED FILES</span><h2 id="file-picker-title">Find a file</h2><p>Fuzzy path matching across the active review.</p></div><button class="icon-button" aria-label="Close file picker" on:click={() => showFilePicker = false}>×</button></header><div class="picker-content"><input data-dialog-initial-focus bind:value={filePickerQuery} aria-label="Find changed file" placeholder="Type a path or symbol…" />{#each sortFiles((review?.files ?? []).filter((file) => fuzzyMatch(file.path, filePickerQuery))).slice(0, 20) as file}<button on:click={() => { showFilePicker = false; void selectFile(file.id); }}><code>{file.path}</code><span>{file.status} · +{file.additions} −{file.deletions}</span></button>{:else}<div class="empty-state">No changed file matches that query.</div>{/each}</div><footer><span>⌘P opens this picker</span><button class="primary-button" on:click={() => showFilePicker = false}>Done</button></footer></dialog></div>
{/if}

{#if showCommandPalette}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal picker-modal" aria-modal="true" aria-labelledby="command-palette-title" use:focusTrap={{ onClose: () => showCommandPalette = false }}><header><div><span class="eyebrow">COMMAND PALETTE</span><h2 id="command-palette-title">Run a review action</h2></div><button class="icon-button" aria-label="Close command palette" on:click={() => showCommandPalette = false}>×</button></header><div class="picker-content"><input data-dialog-initial-focus bind:value={commandQuery} aria-label="Search commands" placeholder="Search actions…" />{#each commandItems.filter((command) => fuzzyMatch(`${command.label} ${command.shortcut}`, commandQuery)) as command}<button on:click={() => { showCommandPalette = false; command.run(); }}><strong>{command.label}</strong><span>{command.shortcut}</span></button>{/each}</div><footer><span>Shortcuts are configurable in Settings.</span><button class="primary-button" on:click={() => showCommandPalette = false}>Done</button></footer></dialog></div>
{/if}

{#if showSettings}
  <div class="modal-backdrop" role="presentation"><dialog open class="modal settings-modal" aria-modal="true" aria-labelledby="settings-title" use:focusTrap={{ onClose: () => showSettings = false }}>
    <header><div><span class="eyebrow">PRESENTATION &amp; HEALTH</span><h2 id="settings-title">Review settings</h2><p>Presentation settings never change the captured comparison. Diagnostics contain no source or reviewed paths.</p></div><button class="icon-button" aria-label="Close settings" on:click={() => showSettings = false}>×</button></header>
    <div class="setup-content settings-content">
      <label>Theme<select data-dialog-initial-focus value={settings.theme} on:change={(event) => setSettings({ theme: event.currentTarget.value as ReviewSettings['theme'] })}><option value="dark">Dark</option><option value="light">Light</option><option value="system">System</option></select></label>
      <label>Code font<input value={settings.codeFont} on:change={(event) => setSettings({ codeFont: event.currentTarget.value })} aria-label="Code font" /></label>
      <label>External editor<select value={settings.externalEditor} on:change={(event) => setSettings({ externalEditor: event.currentTarget.value as ReviewSettings['externalEditor'] })} aria-label="External editor"><option value="system">System default</option><option value="vscode">Visual Studio Code CLI</option><option value="cursor">Cursor CLI</option><option value="zed">Zed CLI</option><option value="sublime">Sublime Text CLI</option><option value="idea">JetBrains IDE CLI</option></select></label>
      <label>Tab width<select value={String(settings.tabWidth)} on:change={(event) => setSettings({ tabWidth: Number(event.currentTarget.value) })} aria-label="Tab width"><option value="2">2 spaces</option><option value="4">4 spaces</option><option value="8">8 spaces</option></select></label>
      <label class="fetch-setting"><input type="checkbox" checked={settings.showWhitespace} on:change={(event) => setSettings({ showWhitespace: event.currentTarget.checked })} /> Show whitespace</label>
      <label class="fetch-setting"><input type="checkbox" checked={settings.vimNavigation} on:change={(event) => setSettings({ vimNavigation: event.currentTarget.checked })} /> Enable Vim j/k file navigation</label>
      <div class="shortcut-settings"><strong>Shortcut defaults</strong>{#each Object.entries(settings.shortcuts) as [action, shortcut]}<label>{action}<input value={shortcut} on:change={(event) => setSettings({ shortcuts: { ...settings.shortcuts, [action]: event.currentTarget.value } })} /></label>{/each}</div>
      <section class="diagnostics-panel" aria-label="LocalReview diagnostics"><div><strong>Persistence diagnostics</strong><p>Database integrity and aggregate backup storage only.</p></div><button class="secondary-button" on:click={() => void loadPersistenceDiagnostics()}>Check health</button>{#if persistenceDiagnostics}<dl><div><dt>Database</dt><dd>{persistenceDiagnostics.databaseHealthy ? 'Healthy' : persistenceDiagnostics.integrityDiagnostic}</dd></div><div><dt>Backups</dt><dd>{persistenceDiagnostics.backupStorage.retainedCount} · {persistenceDiagnostics.backupStorage.retainedBytes.toLocaleString()} bytes</dd></div><div><dt>Recoverable</dt><dd>{persistenceDiagnostics.recoverableBackupCount}</dd></div></dl><button class="secondary-button" on:click={() => void copyPersistenceDiagnostics()}>Copy source-free JSON</button>{/if}</section>
    </div>
    <footer><span>{codeFontPercent}% font zoom · 75–200%</span><button class="primary-button" on:click={() => showSettings = false}>Done</button></footer>
  </dialog></div>
{/if}

{#if copiedMessage}<div class="zoom-toast" role="status">{copiedMessage}</div>{/if}
</div>
