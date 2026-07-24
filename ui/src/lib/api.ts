import { invoke as tauriInvoke, isTauri } from '@tauri-apps/api/core';
import type {
  Annotation,
  AnnotationDraft,
  AnnotationState,
  CapturedBlameResult,
  CapturedCommitContext,
  ChangedSincePreviousReview,
  CommitContextRequest,
  CopyRequest,
  DiffPresentationWindow,
  OpenWorkspaceRequest,
  RepositoryBaseOverride,
  ReviewCaptureRequest,
  DiffMode,
  DiffRow,
  DiffSide,
  FinishReviewRequest,
  FinishReviewPreview,
  FinishReviewResult,
  GitHubPullRequestContext,
  FullFileSide,
  ImportedGitHubConversationComment,
  ImportedGitHubReviewThread,
  PromptPreview,
  PromptRequest,
  Repository,
  RepositoryFilesResult,
  RepositorySetup,
  ReviewApi,
  ReviewData,
  ReviewFile,
  ReviewFileClassificationRecord,
  ReviewHistoryItem,
  ReviewSettings,
  SymbolNavigationOpenRequest,
  SymbolNavigationQuery,
  SymbolNavigationResult,
  SymbolSourceRequest,
  SymbolSourceView,
  ViewportRequest,
  WorkspaceUiState,
  OutlineSymbol,
  DifftasticPresentation,
  Workspace
} from './types';

type TauriInvoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/**
 * Copying feedback is intentional user-visible work.  Never treat a missing
 * browser clipboard implementation as a successful export: Tauri receives a
 * native clipboard command, while browser development gets the standards API
 * and a legacy selection fallback for older/insecure webviews.
 */
export async function copyText(text: string, invoke?: TauriInvoke): Promise<void> {
  if (!text) throw new Error('There is no prompt content to copy.');
  const nativeInvoke = invoke ?? (isTauri() ? tauriInvoke as TauriInvoke : undefined);
  if (nativeInvoke) {
    await nativeInvoke<void>('copy_to_clipboard', { text });
    return;
  }
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.setAttribute('readonly', '');
  textarea.style.cssText = 'position:fixed;left:-9999px;top:0;opacity:0';
  document.body.append(textarea);
  textarea.select();
  const copied = document.execCommand?.('copy') ?? false;
  textarea.remove();
  if (!copied) throw new Error('Clipboard access is unavailable. Select and copy the prompt manually.');
}

const key = 'localreview.mock.v1';
const settingsKey = 'localreview.settings.v1';
const uiStateKey = 'localreview.ui-state.v1';
const now = () => new Date().toISOString();
const uid = () => crypto.randomUUID?.() ?? `id-${Date.now()}-${Math.random()}`;
const setupEnabledByRepository = new Map<string, boolean>();

const demoWorkspaces: Workspace[] = [
  {
    id: 'workspace-localreview', name: 'LocalReview', source: ['local'], location: '~/Projects/localreview', detail: '3 repositories',
    progress: { viewed: 4, total: 8 }, draftCount: 3, pinned: true, refreshAvailable: true, connection: 'connected'
  },
  {
    id: 'workspace-api', name: 'acme/api', source: ['github'], location: 'github.com/acme/api #482', detail: 'PR review',
    progress: { viewed: 18, total: 23 }, draftCount: 6, pinned: true, connection: 'connected'
  },
  {
    id: 'workspace-remote', name: 'payments-prod', source: ['ssh'], location: 'build@staging:/srv/payments', detail: 'Remote workspace',
    progress: { viewed: 11, total: 17 }, draftCount: 1, connection: 'offline'
  }
];

const repositories: Repository[] = [
  { id: 'repo-desktop', name: 'desktop', path: '.', branch: 'feat/review-shell', base: 'origin/master', mergeBase: '9a2d8f3', head: 'e4f1a92' },
  { id: 'repo-core', name: 'crates/core', path: 'crates/core', branch: 'feat/review-shell', base: 'origin/master', mergeBase: '9a2d8f3', head: 'e4f1a92' },
  { id: 'repo-protocol', name: 'crates/protocol', path: 'crates/protocol', branch: 'feat/review-shell', base: 'origin/HOTFIX-1', mergeBase: 'cc9a420', head: 'e4f1a92', isOverride: true }
];

const files: ReviewFile[] = [
  { id: 'file-app', repositoryId: 'repo-desktop', path: 'ui/src/App.svelte', status: 'modified', additions: 76, deletions: 12, hunkCount: 2, language: 'Svelte', viewed: true, annotationCount: 2, classification: emptyClassification() },
  { id: 'file-api', repositoryId: 'repo-desktop', path: 'ui/src/lib/api.ts', status: 'added', additions: 214, deletions: 0, hunkCount: 1, language: 'TypeScript', viewed: true, annotationCount: 1, classification: emptyClassification() },
  { id: 'file-types', repositoryId: 'repo-desktop', path: 'ui/src/lib/types.ts', status: 'modified', additions: 18, deletions: 5, hunkCount: 1, language: 'TypeScript', viewed: false, annotationCount: 0, classification: emptyClassification() },
  { id: 'file-core', repositoryId: 'repo-core', path: 'src/review/session.rs', status: 'modified', additions: 41, deletions: 9, hunkCount: 1, language: 'Rust', viewed: false, annotationCount: 0, classification: emptyClassification() },
  { id: 'file-model', repositoryId: 'repo-core', path: 'src/diff/model.rs', status: 'renamed', previousPath: 'src/diff/rows.rs', additions: 104, deletions: 42, hunkCount: 1, language: 'Rust', viewed: false, annotationCount: 0, classification: emptyClassification() },
  { id: 'file-protocol', repositoryId: 'repo-protocol', path: 'src/messages.rs', status: 'untracked', additions: 58, deletions: 0, hunkCount: 1, language: 'Rust', viewed: false, annotationCount: 0, classification: { ...emptyClassification(), generated: true } },
  { id: 'file-test', repositoryId: 'repo-desktop', path: 'ui/src/lib/virtual.test.ts', status: 'added', additions: 36, deletions: 0, hunkCount: 1, language: 'TypeScript', viewed: true, annotationCount: 0, classification: emptyClassification() },
  { id: 'file-spec', repositoryId: 'repo-desktop', path: 'PRODUCT_SPEC.md', status: 'modified', additions: 9, deletions: 2, hunkCount: 1, language: 'Markdown', viewed: false, annotationCount: 0, classification: { ...emptyClassification(), lockfile: true } }
];

function emptyClassification() {
  return { generated: false, vendored: false, lockfile: false, binary: false, lfsPointer: false, submodule: false };
}

const annotations: Annotation[] = [
  {
    id: 'annotation-1', fileId: 'file-app', repositoryId: 'repo-desktop', kind: 'comment', state: 'open', side: 'new', startLine: 76, endLine: 76,
    body: 'Could this state live in the review store? Keeping the shell slim will make the Tauri boundary easier to test.',
    selectedSource: 'let activeFileId = $state(files[0].id);', labels: ['important'], localOnly: false, createdAt: '2026-07-21T18:42:00.000Z'
  },
  {
    id: 'annotation-2', fileId: 'file-app', repositoryId: 'repo-desktop', kind: 'question', state: 'open', side: 'new', startLine: 128, endLine: 131,
    body: 'What should happen if a refresh re-anchors only part of this selection?',
    selectedSource: 'await api.archiveAnnotations(workspace.id);', labels: ['question'], localOnly: true, createdAt: '2026-07-21T19:12:00.000Z'
  },
  {
    id: 'annotation-3', fileId: 'file-api', repositoryId: 'repo-desktop', kind: 'suggestion', state: 'open', side: 'new', startLine: 32, endLine: 35,
    body: 'Please keep the Tauri invocation behind this adapter so browser development remains representative.',
    selectedSource: 'const nativeInvoke = window.__TAURI__?.core?.invoke;', labels: ['architecture'], localOnly: false, createdAt: '2026-07-21T20:04:00.000Z'
  }
];

const row = (oldLine: number | undefined, newLine: number | undefined, kind: DiffRow['kind'], oldText = '', newText = ''): DiffRow => ({
  id: `${oldLine ?? '-'}:${newLine ?? '-'}:${kind}`, oldLine, newLine, kind, oldText, newText
});

const appRows: DiffRow[] = [
  { id: 'hunk-1', kind: 'header', hunk: '@@ -62,12 +62,36 @@' },
  row(62, 62, 'context', '  let panels = $state(savedPanels);', '  let panels = $state(savedPanels);'),
  row(63, 63, 'context', '  let selected = $state<string | null>(null);', '  let selected = $state<string | null>(null);'),
  row(64, undefined, 'deletion', '  let scale = $state(1);'),
  row(undefined, 64, 'addition', '', '  let fontScale = $state(settings.fontScale);'),
  row(undefined, 65, 'addition', '', '  let showZoomToast = $state(false);'),
  row(undefined, 66, 'addition', '', ''),
  row(65, 67, 'context', '  const zoom = (delta: number) => {', '  const zoom = (delta: number) => {'),
  row(66, undefined, 'deletion', '    scale = Math.min(2, Math.max(.75, scale + delta));'),
  row(undefined, 68, 'addition', '', '    fontScale = Math.min(2, Math.max(.75, fontScale + delta));'),
  row(undefined, 69, 'addition', '', '    api.saveSettings({ fontScale });'),
  row(undefined, 70, 'addition', '', '    showZoomToast = true;'),
  row(67, 71, 'context', '  };', '  };'),
  row(undefined, 72, 'addition', '', ''),
  row(undefined, 73, 'addition', '', '  $effect(() => {'),
  row(undefined, 74, 'addition', '', '    const onKey = (event: KeyboardEvent) => {'),
  row(undefined, 75, 'addition', '', "      if (!event.metaKey && !event.ctrlKey) return;"),
  row(undefined, 76, 'addition', '', "      if (event.key === '+') zoom(.1);"),
  row(undefined, 77, 'addition', '', "      if (event.key === '-') zoom(-.1);"),
  row(undefined, 78, 'addition', '', "      if (event.key === '0') fontScale = 1;"),
  row(undefined, 79, 'addition', '', '    };'),
  row(undefined, 80, 'addition', '', "    window.addEventListener('keydown', onKey);"),
  row(undefined, 81, 'addition', '', "    return () => window.removeEventListener('keydown', onKey);"),
  row(undefined, 82, 'addition', '', '  });'),
  { id: 'hunk-2', kind: 'header', hunk: '@@ -130,7 +154,17 @@' },
  row(130, 154, 'context', '  async function clearReview() {', '  async function clearReview() {'),
  row(131, undefined, 'deletion', '    annotations = [];'),
  row(undefined, 155, 'addition', '', '    const checkpoint = await api.archiveAnnotations(workspace.id);'),
  row(undefined, 156, 'addition', '', '    history = [checkpoint, ...history];'),
  row(undefined, 157, 'addition', '', '    annotations = [];'),
  row(132, 158, 'context', '  }', '  }')
];

function fileRows(fileId: string): DiffRow[] {
  if (fileId === 'file-app') return appRows;
  const file = files.find((item) => item.id === fileId);
  const lines = Array.from({ length: fileId === 'file-core' ? 600 : 115 }, (_, index) => index + 1);
  return [
    { id: `${fileId}-hunk`, kind: 'header', hunk: '@@ -1,8 +1,${lines.length} @@' },
    ...lines.map((lineNumber) => {
      const changed = lineNumber % 9 === 0 || lineNumber % 31 === 0;
      return row(
        changed && lineNumber % 31 === 0 ? undefined : lineNumber,
        changed && lineNumber % 9 === 0 ? undefined : lineNumber,
        changed ? (lineNumber % 31 === 0 ? 'addition' : 'deletion') : 'context',
        changed && lineNumber % 31 === 0 ? '' : `  // ${file?.language ?? 'Source'} line ${lineNumber}`,
        changed && lineNumber % 9 === 0 ? `  export const reviewLine${lineNumber} = true;` : `  // ${file?.language ?? 'Source'} line ${lineNumber}`
      );
    })
  ];
}

const utf8Length = (value: string) => new TextEncoder().encode(value).length;

function tokenSpans(text: string, startByte: number): NonNullable<DiffPresentationWindow['newTokens']> {
  const spans: NonNullable<DiffPresentationWindow['newTokens']> = [];
  const matcher = /\b(?:async|await|const|let|function|export|return|struct|pub|fn|impl|true|false)\b|(?:"[^"\n]*"|'[^'\n]*')|\/\/[^\n]*/g;
  for (const match of text.matchAll(matcher)) {
    const before = text.slice(0, match.index ?? 0);
    const value = match[0];
    const className = value.startsWith('//') ? 'comment' : value.startsWith('"') || value.startsWith("'") ? 'string' : value === 'true' || value === 'false' ? 'boolean' : 'keyword';
    spans.push({ startByte: startByte + utf8Length(before), endByte: startByte + utf8Length(before + value), class: className });
  }
  return spans;
}

function positionedRows(fileId: string) {
  let oldOffset = 0;
  let newOffset = 0;
  let hunkId = `${fileId}:root`;
  const oldTokens: NonNullable<DiffPresentationWindow['oldTokens']> = [];
  const newTokens: NonNullable<DiffPresentationWindow['newTokens']> = [];
  const rows = fileRows(fileId).map((source, index) => {
    if (source.kind === 'header') hunkId = `${fileId}:hunk:${index}`;
    const oldText = source.oldText ?? source.text ?? '';
    const newText = source.newText ?? source.text ?? '';
    const row: DiffRow = { ...source, hunkId, oldSourceStartByte: oldOffset, newSourceStartByte: newOffset };
    if (source.oldLine) oldTokens.push(...tokenSpans(oldText, oldOffset));
    if (source.newLine) newTokens.push(...tokenSpans(newText, newOffset));
    if (source.oldLine) oldOffset += utf8Length(oldText) + 1;
    if (source.newLine) newOffset += utf8Length(newText) + 1;
    return row;
  });
  return { rows, oldTokens, newTokens };
}

function mockFullFileProjection(fileId: string, side: FullFileSide, expandedBlocks: Set<string>, collapsedAdditionBlocks: Set<string>) {
  const source = positionedRows(fileId);
  const rows: DiffRow[] = [];
  const omittedBlocks: NonNullable<DiffPresentationWindow['omittedBlocks']> = [];
  for (let index = 0; index < source.rows.length;) {
    const current = source.rows[index]!;
    if (current.kind === 'header') {
      index += 1;
      continue;
    }
    const isOmitted = side === 'both'
      ? current.kind === 'addition' || current.kind === 'deletion'
      : current.kind === (side === 'old' ? 'addition' : 'deletion');
    if (!isOmitted) {
      if (side === 'old' ? current.oldLine !== undefined : current.newLine !== undefined) rows.push(current);
      index += 1;
      continue;
    }
    const omittedKind = current.kind;
    const omittedSide: DiffSide = omittedKind === 'addition' ? 'new' : 'old';
    const omitted: DiffRow[] = [];
    while (source.rows[index]?.kind === omittedKind) {
      omitted.push(source.rows[index]!);
      index += 1;
    }
    const sourceLine = (row: DiffRow) => omittedSide === 'old' ? row.oldLine : row.newLine;
    const startLine = sourceLine(omitted[0]!) ?? 0;
    const endLine = sourceLine(omitted.at(-1)!) ?? startLine;
    const id = `${fileId}:${omittedSide}:${startLine}-${endLine}`;
    const expanded = side === 'both' && omittedSide === 'new'
      ? !collapsedAdditionBlocks.has(id)
      : expandedBlocks.has(id);
    omittedBlocks.push({ id, side: omittedSide, startLine, endLine, count: omitted.length, expanded, rowIndex: rows.length });
    const action = omittedSide === 'old' ? 'deleted' : 'added';
    rows.push({
      id: `${id}:gate`,
      kind: omittedSide === 'old' ? 'deletion_gate' : 'addition_gate',
      oldLine: omittedSide === 'old' ? startLine : undefined,
      newLine: omittedSide === 'new' ? startLine : undefined,
      omittedEndLine: endLine,
      omittedBlockId: id,
      omittedCount: omitted.length,
      omittedSide,
      omittedExpanded: expanded,
      text: `${omitted.length} ${action} ${omitted.length === 1 ? 'line' : 'lines'}`
    });
    if (expanded) rows.push(...omitted.map((entry) => ({ ...entry, omittedExpanded: true })));
  }
  return { ...source, rows, omittedBlocks };
}

function mockHunks(sourceRows: DiffRow[], presentedRows: DiffRow[]) {
  return sourceRows.flatMap((entry, index) => {
    if (entry.kind !== 'header') return [];
    const firstSource = sourceRows.slice(index + 1).find((candidate) => candidate.kind !== 'header' && (candidate.oldLine || candidate.newLine));
    if (!firstSource) return [];
    const rowIndex = Math.max(0, presentedRows.findIndex((candidate) =>
      candidate.id === firstSource.id
      || ((candidate.kind === 'deletion_gate' || candidate.kind === 'addition_gate')
        && candidate.omittedSide !== undefined
        && candidate.omittedEndLine !== undefined
        && (candidate.omittedSide === 'old' ? firstSource.oldLine : firstSource.newLine) !== undefined
        && (candidate.omittedSide === 'old' ? firstSource.oldLine! : firstSource.newLine!) >= (candidate.omittedSide === 'old' ? candidate.oldLine! : candidate.newLine!)
        && (candidate.omittedSide === 'old' ? firstSource.oldLine! : firstSource.newLine!) <= candidate.omittedEndLine)
    ));
    return [{
      id: entry.hunkId ?? entry.id,
      rowIndex,
      oldLine: firstSource.oldLine,
      newLine: firstSource.newLine,
      header: entry.hunk ?? 'Changed lines',
      collapsedContextLines: 12
    }];
  });
}

function demoDifftastic(rows: DiffRow[]): DifftasticPresentation {
  const cells = rows.filter((row) => row.kind !== 'header').slice(0, 80).map((row) => ({
    old: row.oldLine ? { lineNumber: row.oldLine, text: row.oldText ?? '', changedSpans: row.kind === 'deletion' ? [{ start: 0, end: Math.min(18, (row.oldText ?? '').length), highlight: 'keyword' as const }] : [] } : undefined,
    new: row.newLine ? { lineNumber: row.newLine, text: row.newText ?? '', changedSpans: row.kind === 'addition' ? [{ start: 0, end: Math.min(18, (row.newText ?? '').length), highlight: 'keyword' as const }] : [] } : undefined
  }));
  return {
    status: 'changed', display: 'side_by_side', startRow: 0, totalRows: cells.length, chunks: [{ rows: cells }],
    alignment: cells.map((cell) => ({ oldLine: cell.old?.lineNumber, newLine: cell.new?.lineNumber }))
  };
}

function demoOutline(fileId: string): OutlineSymbol[] {
  const base = fileId === 'file-app' ? [
    ['initialize', 'function', 61, 79], ['selectWorkspace', 'function', 81, 100], ['saveComposer', 'function', 108, 140], ['ReviewSurface', 'class', 148, 260]
  ] : [['Captured review', 'module', 1, 12], ['buildPresentation', 'function', 18, 48]];
  return base.map(([name, kind, startLine, endLine], index) => ({ id: `${fileId}:outline:${index}`, name: String(name), kind: kind as OutlineSymbol['kind'], startLine: Number(startLine), endLine: Number(endLine), depth: 0, side: 'new' }));
}

const defaultSettings: ReviewSettings = {
  fontScale: 1,
  leftWidth: 244,
  rightWidth: 332,
  leftCollapsed: false,
  rightCollapsed: false,
  fetchOnReview: false,
  theme: 'dark',
  codeFont: 'SF Mono',
  externalEditor: 'system',
  tabWidth: 2,
  showWhitespace: false,
  wrapLines: false,
  vimNavigation: false,
  promptPathStyle: 'absolute',
  promptIncludeDiffHunks: false,
  promptIncludeGitState: false,
  shortcuts: {
    nextHunk: 'Alt+ArrowDown', previousHunk: 'Alt+ArrowUp', filePicker: 'Meta+P',
    commandPalette: 'Meta+Shift+P', saveAnnotation: 'Meta+Enter', focusQuestion: 'Meta+Shift+Q'
  }
};

interface StoredState {
  reviews: Record<string, ReviewData>;
  /** One mutable/current session per workspace; replacements get a new id. */
  activeReviewIds?: Record<string, string>;
  /** Full frozen review surfaces, keyed by workspace + review history id. */
  archivedReviews?: Record<string, ReviewData>;
  annotationDrafts?: Record<string, AnnotationDraft>;
  /** Browser-fixture equivalent of the native immutable prompt-export rows. */
  promptExports?: Record<string, PromptPreview>;
}

function validateMockBase(base: string) {
  if (!base.trim() || base.startsWith('-') || /[\s~^:@{\\?*\[]/.test(base) || base.includes('..')) {
    throw new Error('The base reference must be a safe branch, tag, remote ref, or object ID.');
  }
}

/**
 * Browser development cannot resolve Git refs. Ref names containing an
 * explicit missing marker provide a deterministic fixture for the native
 * "discovered, but no repository captured" recovery path.
 */
function mockBaseResolves(base: string) {
  return !/(?:^|[\/_-])(?:missing|does-not-exist|unknown)(?:$|[\/_-])/i.test(base);
}

function workspaceReviewTemplates(workspaceId: string, defaultBase: string): Pick<ReviewData, 'repositories' | 'files'> {
  const repositoryIds = new Map(repositories.map((repository) => [repository.id, `${workspaceId}:${repository.id}`]));
  return {
    repositories: repositories.map((repository) => ({
      ...repository,
      id: repositoryIds.get(repository.id)!,
      base: defaultBase,
      isOverride: false
    })),
    files: files.map((file) => ({
      ...file,
      id: `${workspaceId}:${file.id}`,
      repositoryId: repositoryIds.get(file.repositoryId)!,
      viewed: false,
      annotationCount: 0
    }))
  };
}

function defaultState(): StoredState {
  return {
    activeReviewIds: Object.fromEntries(demoWorkspaces.map((workspace) => [workspace.id, `active-${workspace.id}`])),
    archivedReviews: {},
    annotationDrafts: {},
    promptExports: {},
    reviews: Object.fromEntries(demoWorkspaces.map((workspace) => [workspace.id, {
      workspace,
      repositories,
      files: files.map((file) => ({ ...file })),
      annotations: annotations.map((annotation) => ({ ...annotation })),
      history: []
    }]))
  };
}

function loadState(): StoredState {
  if (typeof localStorage === 'undefined') return defaultState();
  try {
    const stored = localStorage.getItem(key);
    return stored ? JSON.parse(stored) as StoredState : defaultState();
  } catch {
    return defaultState();
  }
}

function saveState(state: StoredState) {
  if (typeof localStorage !== 'undefined') localStorage.setItem(key, JSON.stringify(state));
}

export function makeMockApi(): ReviewApi {
  let state = loadState();
  state.activeReviewIds ??= {};
  state.archivedReviews ??= {};
  state.annotationDrafts ??= {};
  let stateMigrated = false;
  for (const [workspaceId, data] of Object.entries(state.reviews)) {
    if (data.workspace.reviewReady === false) {
      if (state.activeReviewIds[workspaceId]) {
        delete state.activeReviewIds[workspaceId];
        stateMigrated = true;
      }
      continue;
    }
    if (!state.activeReviewIds[workspaceId]) {
      state.activeReviewIds[workspaceId] = `legacy-${workspaceId}`;
      stateMigrated = true;
    }
    if (data.workspace.reviewReady !== true) {
      data.workspace.reviewReady = true;
      stateMigrated = true;
    }
    for (const file of data.files) {
      if (Number.isSafeInteger(file.hunkCount) && file.hunkCount >= 0) continue;
      file.hunkCount = positionedRows(file.id).rows.filter((entry) => entry.kind === 'header').length;
      stateMigrated = true;
    }
  }
  const finishPreviews = new Map<string, { workspaceId: string; annotationIds: string[]; payloadJson: string; pinnedHeadSha: string }>();
  let settings = (() => {
    try { return { ...defaultSettings, ...JSON.parse(localStorage.getItem(settingsKey) ?? '{}') }; }
    catch { return { ...defaultSettings }; }
  })();
  let workspaceUiStates: Record<string, WorkspaceUiState> = (() => {
    try { return JSON.parse(localStorage.getItem(uiStateKey) ?? '{}') as Record<string, WorkspaceUiState>; }
    catch { return {}; }
  })();
  const saveWorkspaceUiStates = () => {
    if (typeof localStorage !== 'undefined') localStorage.setItem(uiStateKey, JSON.stringify(workspaceUiStates));
  };
  const defaultWorkspaceUiState = (): WorkspaceUiState => ({ mode: 'unified', fullFileSide: 'both', scrollTop: 0, splitRatio: .5, rightTab: 'files' });
  const activeReviewKey = (workspaceId: string) => {
    const reviewId = state.activeReviewIds?.[workspaceId];
    return reviewId ? `${workspaceId}:${reviewId}` : undefined;
  };
  const migrateLegacySessionState = (workspaceId: string) => {
    const sessionKey = activeReviewKey(workspaceId);
    if (!sessionKey) return;
    if (Object.prototype.hasOwnProperty.call(state.annotationDrafts, workspaceId)) {
      state.annotationDrafts![sessionKey] ??= state.annotationDrafts![workspaceId];
      delete state.annotationDrafts![workspaceId];
      stateMigrated = true;
    }
    if (Object.prototype.hasOwnProperty.call(workspaceUiStates, workspaceId)) {
      workspaceUiStates[sessionKey] ??= workspaceUiStates[workspaceId];
      delete workspaceUiStates[workspaceId];
      saveWorkspaceUiStates();
    }
  };
  for (const workspaceId of Object.keys(state.reviews)) migrateLegacySessionState(workspaceId);
  if (stateMigrated) saveState(state);
  const review = (workspaceId: string) => {
    const value = state.reviews[workspaceId];
    if (!value) throw new Error(`Workspace ${workspaceId} does not exist`);
    return value;
  };
  const captureCanSucceed = (data: ReviewData) => {
    const included = data.repositories.filter((repository) => setupEnabledByRepository.get(repository.id) ?? true);
    return included.length === 0 || included.some((repository) => mockBaseResolves(repository.base));
  };
  const captureInitialReview = (data: ReviewData) => {
    if (!captureCanSucceed(data)) {
      data.workspace.reviewReady = false;
      data.workspace.progress = { viewed: 0, total: 0 };
      throw new Error('No repository capture succeeded. Correct the missing baseline and try again.');
    }
    const templates = workspaceReviewTemplates(data.workspace.id, data.workspace.defaultBase ?? 'origin/master');
    const baseByRepositoryPath = new Map(data.repositories.map((repository) => [repository.path, repository]));
    data.files = templates.files;
    // Preserve discovery-time ids and configured effective bases while the
    // browser fixture materializes its first immutable review surface.
    data.repositories = templates.repositories.map((repository) => {
      const configured = baseByRepositoryPath.get(repository.path);
      return configured ? { ...repository, id: configured.id, base: configured.base, isOverride: configured.isOverride } : repository;
    });
    data.files = data.files.map((file) => {
      const repositoryPath = templates.repositories.find((repository) => repository.id === file.repositoryId)?.path;
      const repositoryId = data.repositories.find((repository) => repository.path === repositoryPath)?.id;
      return repositoryId ? { ...file, repositoryId } : file;
    });
    state.activeReviewIds![data.workspace.id] = uid();
    data.workspace.reviewReady = true;
    data.workspace.progress = { viewed: 0, total: data.files.length };
    migrateLegacySessionState(data.workspace.id);
  };
  return {
    async pickLocalFolder() { return {}; },
    async openWorkspace(request: OpenWorkspaceRequest) {
      if (request.base) validateMockBase(request.base);
      for (const repositoryBase of request.repositoryBases ?? []) validateMockBase(repositoryBase.base);
      const existing = Object.values(state.reviews).find(({ workspace }) => workspace.location === request.path);
      if (existing) {
        // Match native canonical-root reuse: explicitly opening an archived
        // path is also an explicit request to return that workspace to the
        // live rail. Its durable review/session identity remains unchanged.
        existing.workspace.archived = false;
        if (request.base) {
          existing.workspace.defaultBase = request.base;
          existing.repositories = existing.repositories.map((repository) => repository.isOverride ? repository : { ...repository, base: request.base! });
        }
        for (const override of request.repositoryBases ?? []) {
          existing.repositories = existing.repositories.map((repository) => repository.path === override.repositoryPath
            ? { ...repository, base: override.base, isOverride: true }
            : repository);
        }
        if (!state.activeReviewIds![existing.workspace.id]) {
          try { captureInitialReview(existing); }
          catch { /* Missing refs leave discovery durable for baseline recovery. */ }
        }
        saveState(state);
        return structuredClone(existing.workspace);
      }
      const id = `workspace-${uid().slice(0, 8)}`;
      const defaultBase = request.base ?? 'origin/master';
      const templates = workspaceReviewTemplates(id, defaultBase);
      const workspace: Workspace = {
        id,
        name: request.path.split('/').filter(Boolean).at(-1) || 'Local workspace',
        source: ['local'],
        location: request.path,
        detail: 'Discovering repositories',
        defaultBase,
        progress: { viewed: 0, total: 0 },
        draftCount: 0,
        connection: 'connected',
        reviewReady: false
      };
      state.reviews[id] = {
        workspace,
        repositories: templates.repositories.map((repository) => {
          const override = request.repositoryBases?.find((item) => item.repositoryPath === repository.path);
          return override ? { ...repository, base: override.base, isOverride: true } : repository;
        }),
        files: [],
        annotations: [],
        history: []
      };
      try { captureInitialReview(state.reviews[id]); }
      catch { /* Match native open: keep the discovered shell for correction. */ }
      saveState(state);
      return structuredClone(workspace);
    },
    async openGitHubPr(url) {
      if (!/^https:\/\/github\.com\/[^/]+\/[^/]+\/pull\/\d+\/?$/i.test(url)) throw new Error('Enter a GitHub.com pull-request URL.');
      const workspace = await this.openWorkspace({ path: url });
      const data = review(workspace.id);
      const match = url.match(/^https:\/\/github\.com\/([^/]+\/[^/]+)\/pull\/(\d+)/i);
      data.workspace = { ...data.workspace, name: `${match?.[1] ?? 'GitHub PR'} #${match?.[2] ?? ''}`, source: ['github'], detail: 'PR review', location: url };
      saveState(state);
      return structuredClone(data.workspace);
    },
    async openSshWorkspace(target) {
      if (!/^[^:\s]+:\/.+/.test(target)) throw new Error('Enter SSH target as host:/absolute/path.');
      const workspace = await this.openWorkspace({ path: target });
      const data = review(workspace.id);
      data.workspace = { ...data.workspace, source: ['ssh'], detail: 'Remote workspace', location: target };
      saveState(state);
      return structuredClone(data.workspace);
    },
    async reconnectSshWorkspace(workspaceId) {
      const data = review(workspaceId);
      if (!data.workspace.source.includes('ssh')) throw new Error('This workspace is not an SSH review.');
      data.workspace = { ...data.workspace, connection: 'connected', detail: `${data.workspace.detail.replace(/ · reconnecting$/i, '')} · reconnected` };
      saveState(state);
      return structuredClone(data.workspace);
    },
    async listWorkspaces() {
      return Object.values(state.reviews)
        .map(({ workspace }) => workspace)
        .filter((workspace) => !workspace.archived)
        .sort((left, right) => Number(Boolean(right.pinned)) - Number(Boolean(left.pinned)))
        .map((workspace) => structuredClone(workspace));
    },
    async listArchivedWorkspaces() {
      return Object.values(state.reviews)
        .map(({ workspace }) => workspace)
        .filter((workspace) => Boolean(workspace.archived))
        .map((workspace) => structuredClone(workspace));
    },
    async reopenArchivedWorkspace(workspaceId) {
      const data = review(workspaceId);
      if (!data.workspace.archived) throw new Error('Workspace is already open in the workspace rail.');
      data.workspace = { ...data.workspace, archived: false };
      saveState(state);
      return structuredClone(data.workspace);
    },
    async updateWorkspaceMetadata(workspaceId, metadata) {
      const data = review(workspaceId);
      if (data.workspace.archived) throw new Error('Reopen this workspace before changing its rail metadata.');
      const name = metadata.name?.trim();
      if (metadata.name !== undefined && (!name || [...name].length > 120)) throw new Error('Workspace name must contain between 1 and 120 characters.');
      data.workspace = { ...data.workspace, ...(name ? { name } : {}), ...(metadata.pinned !== undefined ? { pinned: metadata.pinned } : {}) };
      saveState(state);
      return structuredClone(data.workspace);
    },
    async getPersistenceDiagnostics() {
      return {
        databaseHealthy: true,
        integrityDiagnostic: 'ok',
        recoverableBackupCount: 3,
        backupStorage: {
          retainedCount: 3,
          retainedBytes: 2_457_600,
          newestBackupAt: now(),
          oldestBackupAt: '2026-07-19T00:00:00.000Z',
          exceedsSizePreference: false,
          policy: { maxBackups: 7, maxTotalBytes: 536_870_912 }
        }
      };
    },
    async archiveWorkspace(workspaceId) {
      const data = review(workspaceId);
      data.workspace = { ...data.workspace, archived: true };
      saveState(state);
    },
    async deleteWorkspace(workspaceId) {
      review(workspaceId);
      delete state.reviews[workspaceId];
      if (state.archivedReviews) {
        for (const key of Object.keys(state.archivedReviews)) {
          if (key.startsWith(`${workspaceId}:`)) delete state.archivedReviews[key];
        }
      }
      saveState(state);
    },
    async loadReview(workspaceId) { return structuredClone(review(workspaceId)); },
    async loadArchivedReview(workspaceId, historyId) {
      const data = review(workspaceId);
      const item = data.history.find((entry) => entry.id === historyId && entry.type === 'review');
      if (!item) throw new Error('This history entry is not an archived review.');
      const archived = state.archivedReviews?.[`${workspaceId}:${historyId}`];
      if (archived) return structuredClone(archived);
      // Compatibility for old browser fixtures that retained only annotation
      // checkpoints before full immutable review snapshots were introduced.
      return structuredClone({
        ...data,
        annotations: item.annotations ? structuredClone(item.annotations) : [],
        historical: true,
        historicalSessionId: historyId
      });
    },
    async getReviewFileClassifications(workspaceId) {
      return review(workspaceId).files.map((file) => ({
        comparisonId: `comparison-${file.repositoryId}`,
        fileId: file.id,
        path: file.path,
        classification: structuredClone(file.classification ?? emptyClassification())
      }));
    },
    async getCapturedBlame(workspaceId, fileId, side, startLine, endLine): Promise<CapturedBlameResult> {
      const data = review(workspaceId);
      const file = data.files.find((candidate) => candidate.id === fileId);
      if (!file || startLine < 1 || endLine < startLine || endLine - startLine >= 500) throw new Error('Select between 1 and 500 captured source lines for blame.');
      const source = await this.getCapturedSourceRange(fileId, side, startLine, endLine);
      if (!source.complete) throw new Error('The selected captured source range is unavailable.');
      return {
        comparisonId: `comparison-${file.repositoryId}`,
        side,
        lines: source.text.split('\n').filter((_, index, all) => index < all.length - 1 || all[index] !== '').map((text, index) => ({
          revision: data.repositories.find((repository) => repository.id === file.repositoryId)?.head ?? 'e4f1a92',
          originalLine: startLine + index,
          finalLine: startLine + index,
          sourcePath: file.path,
          authorName: 'Captured reviewer',
          authorEmail: 'reviewer@example.invalid',
          authorTime: '2026-07-22T00:00:00Z',
          summary: 'Captured comparison fixture',
          source: text,
          sourceTruncated: false
        }))
      };
    },
    async getCommitContext(workspaceId, request: CommitContextRequest): Promise<CapturedCommitContext> {
      const data = review(workspaceId);
      const repository = data.repositories.find((candidate) => candidate.id === request.repositoryId);
      if (!repository) throw new Error('Repository is no longer part of this review.');
      const commits = [
        { sha: repository.mergeBase, parentShas: [], authorName: 'Base author', authorEmail: 'base@example.invalid', authoredAt: '2026-07-20T12:00:00Z', subject: 'Baseline for captured review' },
        { sha: repository.head, parentShas: [repository.mergeBase], authorName: 'Review author', authorEmail: 'author@example.invalid', authoredAt: '2026-07-22T00:00:00Z', subject: 'Captured review change' }
      ].filter((commit) => (request.includeMergeCommits ?? true) || commit.parentShas.length < 2)
        .filter((commit) => !request.authorContains || `${commit.authorName} ${commit.authorEmail}`.toLowerCase().includes(request.authorContains.toLowerCase()))
        .filter((commit) => !request.subjectContains || commit.subject.toLowerCase().includes(request.subjectContains.toLowerCase()))
        .slice(0, request.maxEntries ?? 100);
      const selected = request.selectedCommit ? commits.find((commit) => commit.sha === request.selectedCommit) : undefined;
      return {
        comparisonId: `comparison-${repository.id}`,
        range: { mergeBase: repository.mergeBase, head: repository.head }, commits, truncated: false,
        selectedCommit: selected ? { summary: selected, committerName: selected.authorName, committerEmail: selected.authorEmail, committedAt: selected.authoredAt, body: 'Fixture commit details are read-only and tied to the captured comparison.', bodyTruncated: false } : undefined
      };
    },
    async getChangedSincePreviousReview(workspaceId, repositoryId): Promise<ChangedSincePreviousReview> {
      const data = review(workspaceId);
      const repository = data.repositories.find((candidate) => candidate.id === repositoryId);
      if (!repository) throw new Error('Repository is no longer part of this review.');
      const matching = data.files.filter((file) => file.repositoryId === repositoryId);
      return {
        currentComparisonId: `comparison-${repositoryId}`,
        previousComparisonId: `previous-comparison-${repositoryId}`,
        files: matching.map((file, index) => ({
          kind: index % 3 === 0 ? 'modified' : file.status === 'renamed' ? 'renamed' : 'unchanged',
          path: file.path, previousPath: file.previousPath, currentFileId: file.id,
          currentDocumentFingerprint: `current-${file.id}`, previousDocumentFingerprint: `previous-${file.id}`
        })),
        truncated: false
      };
    },
    async getGitHubUpdateStatus(workspaceId) {
      const data = review(workspaceId);
      if (!data.workspace.source.includes('github')) throw new Error('This workspace is not a GitHub pull-request review.');
      const repository = data.repositories[0];
      return {
        workspaceId,
        canonicalUrl: data.workspace.location,
        pinnedBaseSha: repository?.mergeBase ?? 'base',
        pinnedHeadSha: repository?.head ?? 'head',
        currentBaseSha: repository?.mergeBase ?? 'base',
        currentHeadSha: repository?.head ?? 'head',
        baseChanged: false,
        headChanged: false,
        metadataFetchedAt: now()
      };
    },
    async getGitHubPullRequest(workspaceId): Promise<GitHubPullRequestContext> {
      const data = review(workspaceId);
      if (!data.workspace.source.includes('github')) throw new Error('This workspace is not a GitHub pull-request review.');
      const repository = data.repositories[0];
      return { canonical_url: data.workspace.location, title: `${data.workspace.name} review`, author: 'octocat', base_ref: repository?.base ?? 'main', head_ref: repository?.branch ?? 'review', pinned_base_sha: repository?.mergeBase ?? 'base', pinned_head_sha: repository?.head ?? 'head', draft: false, state: 'OPEN', review_decision: 'REVIEW_REQUIRED', commits: [{ sha: repository?.head ?? 'head', message_headline: 'Captured pull request revision', authored_at: '2026-07-22T00:00:00Z' }] };
    },
    async getGitHubThreads(workspaceId): Promise<ImportedGitHubReviewThread[]> {
      const data = review(workspaceId);
      if (!data.workspace.source.includes('github')) throw new Error('This workspace is not a GitHub pull-request review.');
      return [{ id: 'imported-thread-1', resolved: false, outdated: false, path: data.files[0]?.path, line: 64, comments: [{ id: 'imported-comment-1', body_markdown: 'Imported GitHub thread context remains separate from local annotations.', author: 'octocat', created_at: '2026-07-21T00:00:00Z', review: { state: 'COMMENTED', author: 'octocat' } }] }];
    },
    async getGitHubConversation(workspaceId): Promise<ImportedGitHubConversationComment[]> {
      const data = review(workspaceId);
      if (!data.workspace.source.includes('github')) throw new Error('This workspace is not a GitHub pull-request review.');
      return [{ id: 1, body_markdown: 'Imported general PR conversation.', author: 'octocat', created_at: '2026-07-21T00:00:00Z' }];
    },
    async getRows(fileId, _mode: DiffMode) { return positionedRows(fileId).rows; },
    async getPresentationWindow(request: ViewportRequest) {
      const source = positionedRows(request.fileId);
      const expandedBlocks = new Set(request.ephemeralExpandedFullFileDeletionBlocks
        ?? Object.values(workspaceUiStates)
          .flatMap((workspaceState) => workspaceState.expandedFullFileDeletionBlocks ?? []));
      const collapsedAdditionBlocks = new Set(request.ephemeralCollapsedFullFileAdditionBlocks
        ?? Object.values(workspaceUiStates)
          .flatMap((workspaceState) => workspaceState.collapsedFullFileAdditionBlocks ?? []));
      const all = request.mode === 'full'
        ? mockFullFileProjection(request.fileId, request.fullFileSide ?? 'both', expandedBlocks, collapsedAdditionBlocks)
        : { ...source, omittedBlocks: [] as NonNullable<DiffPresentationWindow['omittedBlocks']> };
      const startRow = Math.max(0, Math.min(all.rows.length, request.startRow));
      const endRow = Math.max(startRow, Math.min(all.rows.length, request.endRow));
      return {
        generation: request.generation,
        mode: request.mode,
        fileId: request.fileId,
        startRow,
        totalRows: all.rows.length,
        rows: all.rows.slice(startRow, endRow),
        hunks: mockHunks(source.rows, all.rows),
        omittedBlocks: all.omittedBlocks,
        oldTokens: all.oldTokens,
        newTokens: all.newTokens,
        highlightStatus: 'highlighted',
        difftastic: request.mode === 'difftastic' ? demoDifftastic(all.rows) : undefined
      };
    },
    async resolvePresentationLocation(fileId, mode, side, line) {
      const source = positionedRows(fileId);
      const expandedBlocks = new Set(Object.values(workspaceUiStates)
        .flatMap((workspaceState) => workspaceState.expandedFullFileDeletionBlocks ?? []));
      const collapsedAdditionBlocks = new Set(Object.values(workspaceUiStates)
        .flatMap((workspaceState) => workspaceState.collapsedFullFileAdditionBlocks ?? []));
      const fullFileSide = Object.values(workspaceUiStates)
        .find((workspaceState) => workspaceState.activeFileId === fileId && workspaceState.mode === 'full')
        ?.fullFileSide ?? 'both';
      const all = mode === 'full'
        ? mockFullFileProjection(fileId, fullFileSide, expandedBlocks, collapsedAdditionBlocks).rows
        : source.rows;
      let rowIndex = all.findIndex((entry) => (side === 'old' ? entry.oldLine : entry.newLine) === line);
      if (rowIndex < 0 && mode === 'full') {
        rowIndex = all.findIndex((entry) =>
          (entry.kind === 'deletion_gate' || entry.kind === 'addition_gate')
          && entry.omittedSide === side
          && entry.omittedEndLine !== undefined
          && (side === 'old' ? entry.oldLine : entry.newLine) !== undefined
          && line >= (side === 'old' ? entry.oldLine! : entry.newLine!)
          && line <= entry.omittedEndLine
        );
      }
      if (rowIndex < 0 && mode === 'full') {
        const aligned = source.rows.find((entry) => (side === 'old' ? entry.oldLine : entry.newLine) === line);
        const targetSide = side === 'old' ? 'new' : 'old';
        const targetLine = targetSide === 'old' ? aligned?.oldLine : aligned?.newLine;
        if (targetLine !== undefined) {
          rowIndex = all.findIndex((entry) => (targetSide === 'old' ? entry.oldLine : entry.newLine) === targetLine);
        }
      }
      if (mode === 'difftastic') {
        const structural = demoDifftastic(source.rows);
        const exact = structural.alignment.findIndex((entry) => (side === 'old' ? entry.oldLine : entry.newLine) === line);
        if (exact >= 0) rowIndex = exact;
      }
      if (rowIndex < 0) {
        const candidates = all.map((entry, index) => ({ index, line: side === 'old' ? entry.oldLine : entry.newLine })).filter((entry): entry is { index: number; line: number } => entry.line !== undefined);
        rowIndex = candidates.sort((left, right) => Math.abs(left.line - line) - Math.abs(right.line - line))[0]?.index ?? 0;
      }
      return { rowIndex, side, line };
    },
    async getCapturedSourceRange(fileId, side, startLine, endLine) {
      const selected = positionedRows(fileId).rows
        .filter((entry) => entry.kind !== 'header')
        .map((entry) => ({ line: side === 'old' ? entry.oldLine : entry.newLine, text: side === 'old' ? (entry.oldText ?? entry.text ?? '') : (entry.newText ?? entry.text ?? '') }))
        .filter((entry): entry is { line: number; text: string } => entry.line !== undefined && entry.line >= startLine && entry.line <= endLine)
        .sort((left, right) => left.line - right.line);
      return { text: selected.map((entry) => entry.text).join('\n'), complete: selected.length === endLine - startLine + 1 };
    },
    async expandHunk(_fileId, _hunkId, _contextLines) { /* browser fixture already includes context */ },
    async getOutline(fileId, _side) { return demoOutline(fileId); },
    async openSymbolNavigation(input) {
      const data = review(input.workspaceId);
      const file = data.files.find((candidate) =>
        candidate.id === input.fileId
        && candidate.repositoryId === input.repositoryId
        && (!input.comparisonId || candidate.comparisonId === input.comparisonId)
      );
      if (!file) throw new Error('The symbol target does not belong to this captured workspace.');
      return { windowLabel: `symbol-navigation-${uid().slice(0, 12)}` };
    },
    async querySymbolNavigation(input): Promise<SymbolNavigationResult> {
      const data = review(input.workspaceId);
      const repositoryFiles = data.files.filter((file) => file.repositoryId === input.repositoryId);
      const escaped = input.symbol.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
      const matcher = new RegExp(`(^|[^\\p{L}\\p{N}_$])(${escaped})(?=$|[^\\p{L}\\p{N}_$])`, 'u');
      const locations = repositoryFiles.flatMap((file) =>
        positionedRows(file.id).rows.flatMap((row) => {
          if (row.kind === 'header' || !row.newLine) return [];
          const source = row.newText ?? row.text ?? '';
          const match = source.match(matcher);
          if (!match || match.index === undefined) return [];
          const column = match.index + match[1]!.length + 1;
          const definition = new RegExp(`\\b(?:class|const|def|enum|fn|function|interface|let|module|struct|type)\\s+${escaped}\\b`).test(source);
          return [{
            repositoryId: input.repositoryId,
            path: file.path,
            line: row.newLine,
            column,
            endLine: row.newLine,
            endColumn: column + input.symbol.length,
            preview: source,
            kind: definition ? 'declaration' : 'usage',
            role: definition ? 'definition' as const : 'reference' as const,
            sourceFingerprint: `mock:${file.id}:${file.comparisonId ?? 'current'}`,
            fileId: file.id,
            comparisonId: file.comparisonId,
            side: 'new' as const
          }];
        })
      );
      const limit = Math.max(1, Math.min(500, input.limit ?? 200));
      const definitions = input.kind === 'references'
        ? []
        : locations.filter((location) => location.role === 'definition').slice(0, limit);
      const references = input.kind === 'definitions'
        ? []
        : locations.filter((location) => location.role === 'reference').slice(0, limit);
      return {
        symbol: input.symbol,
        definitions,
        references,
        truncated: definitions.length + references.length < locations.length,
        diagnostics: []
      };
    },
    async getSymbolSource(input): Promise<SymbolSourceView> {
      const data = review(input.workspaceId);
      const file = data.files.find((candidate) =>
        candidate.repositoryId === input.repositoryId && candidate.path === input.path
      );
      if (!file) throw new Error('The symbol source path is outside this repository.');
      const fingerprint = `mock:${file.id}:${file.comparisonId ?? 'current'}`;
      if (fingerprint !== input.expectedFingerprint) throw new Error('The symbol source changed; run the search again.');
      const source = positionedRows(file.id).rows
        .filter((row) => row.kind !== 'header' && row.newLine)
        .map((row) => ({ line: row.newLine!, text: row.newText ?? row.text ?? '' }))
        .sort((left, right) => left.line - right.line);
      const startLine = Math.max(1, input.startLine);
      const endLine = startLine + Math.max(1, input.lineCount);
      return {
        repositoryId: input.repositoryId,
        path: input.path,
        sourceFingerprint: fingerprint,
        startLine,
        totalLines: source.at(-1)?.line ?? 0,
        lines: source.filter((row) => row.line >= startLine && row.line < endLine).map((row) => row.text),
        lineStartBytes: source
          .filter((row) => row.line >= startLine && row.line < endLine)
          .map((row) => source.slice(0, Math.max(0, row.line - 1)).reduce((bytes, item) => bytes + new TextEncoder().encode(`${item.text}\n`).length, 0)),
        tokens: [],
        highlightStatus: 'plain_text',
        highlightReason: 'Browser fixture source'
      };
    },
    async getRepositoryFiles(input): Promise<RepositoryFilesResult> {
      const needle = input.query?.trim().toLowerCase() ?? '';
      const files = review(input.workspaceId).files
        .filter((file) => file.repositoryId === input.repositoryId && (!needle || file.path.toLowerCase().includes(needle)))
        .slice(0, input.limit ?? 2_000)
        .map((file) => ({
          path: file.path,
          fileId: file.id,
          comparisonId: file.comparisonId,
          side: file.status === 'deleted' ? 'old' as const : 'new' as const
        }));
      return { files, truncated: false, diagnostics: [] };
    },
    async openRepositorySource(input): Promise<SymbolSourceView> {
      const data = review(input.workspaceId);
      const file = data.files.find((candidate) =>
        candidate.repositoryId === input.repositoryId && candidate.path === input.path
      );
      if (!file) throw new Error('The repository source path is unavailable.');
      const source = positionedRows(file.id).rows
        .filter((row) => row.kind !== 'header' && row.newLine)
        .map((row) => ({ line: row.newLine!, text: row.newText ?? row.text ?? '' }))
        .sort((left, right) => left.line - right.line);
      const startLine = Math.max(1, input.startLine);
      const endLine = startLine + Math.max(1, input.lineCount);
      const selected = source.filter((row) => row.line >= startLine && row.line < endLine);
      return {
        repositoryId: input.repositoryId,
        path: input.path,
        sourceFingerprint: `mock:${file.id}:${file.comparisonId ?? 'current'}`,
        startLine,
        totalLines: source.at(-1)?.line ?? 0,
        lines: selected.map((row) => row.text),
        lineStartBytes: selected.map((row) => source.slice(0, Math.max(0, row.line - 1)).reduce((bytes, item) => bytes + new TextEncoder().encode(`${item.text}\n`).length, 0)),
        tokens: [],
        highlightStatus: 'plain_text',
        highlightReason: 'Browser fixture source'
      };
    },
    async saveAnnotation(workspaceId, annotation) {
      const data = review(workspaceId);
      const file = data.files.find((candidate) => candidate.id === annotation.fileId);
      if (!file) {
        throw new Error('The annotation file does not belong to this workspace\'s active review.');
      }
      annotation = { ...annotation, repositoryId: file.repositoryId };
      const existing = data.annotations.findIndex((item) => item.id === annotation.id);
      if (existing >= 0) data.annotations[existing] = annotation;
      else data.annotations.unshift(annotation);
      if (existing < 0) file.annotationCount += 1;
      data.workspace.draftCount = data.annotations.filter((item) => !item.publishedId && item.state === 'open').length;
      saveState(state);
      return structuredClone(annotation);
    },
    async getAnnotationDraft(workspaceId) {
      const reviewKey = activeReviewKey(workspaceId);
      return reviewKey ? structuredClone(state.annotationDrafts?.[reviewKey]) : undefined;
    },
    async saveAnnotationDraft(draft) {
      const reviewKey = activeReviewKey(draft.workspaceId);
      if (!reviewKey) throw new Error('Workspace has no active review.');
      state.annotationDrafts ??= {};
      state.annotationDrafts[reviewKey] = structuredClone(draft);
      saveState(state);
    },
    async clearAnnotationDraft(workspaceId) {
      const reviewKey = activeReviewKey(workspaceId);
      if (state.annotationDrafts && reviewKey) delete state.annotationDrafts[reviewKey];
      if (state.annotationDrafts) delete state.annotationDrafts[workspaceId];
      saveState(state);
    },
    async deleteAnnotation(workspaceId, annotationId) {
      const data = review(workspaceId);
      const deleted = data.annotations.find((annotation) => annotation.id === annotationId);
      data.annotations = data.annotations.filter((annotation) => annotation.id !== annotationId);
      if (deleted) data.files = data.files.map((file) => file.id === deleted.fileId ? { ...file, annotationCount: Math.max(0, file.annotationCount - 1) } : file);
      data.workspace.draftCount = data.annotations.filter((item) => !item.publishedId && item.state === 'open').length;
      saveState(state);
    },
    async setAnnotationState(workspaceId, annotationId, annotationState: AnnotationState) {
      const data = review(workspaceId);
      const current = data.annotations.find((annotation) => annotation.id === annotationId);
      if (!current) throw new Error('Annotation no longer exists.');
      const updated = { ...current, state: annotationState };
      data.annotations = data.annotations.map((annotation) => annotation.id === annotationId ? updated : annotation);
      saveState(state);
      return structuredClone(updated);
    },
    async archiveAnnotations(workspaceId) {
      const data = review(workspaceId);
      const item: ReviewHistoryItem = { id: uid(), type: 'clear', label: 'Cleared annotations', annotationCount: data.annotations.length, createdAt: now(), annotations: structuredClone(data.annotations) };
      data.history.unshift(item);
      data.annotations = [];
      data.files = data.files.map((file) => ({ ...file, annotationCount: 0 }));
      data.workspace.draftCount = 0;
      saveState(state);
      return item;
    },
    async restoreAnnotations(workspaceId, annotationsToRestore) {
      const data = review(workspaceId);
      // The native controller recreates each annotation through normal
      // validation.  Match that immutable-history contract in the browser
      // fixture: old checkpoint IDs are never resurrected into the active set.
      data.annotations = annotationsToRestore.map((annotation) => ({ ...structuredClone(annotation), id: uid() }));
      data.files = data.files.map((file) => ({
        ...file,
        annotationCount: data.annotations.filter((annotation) => annotation.fileId === file.id).length
      }));
      data.workspace.draftCount = data.annotations.filter((item) => !item.publishedId && item.state === 'open').length;
      saveState(state);
      return structuredClone(data);
    },
    async generatePrompt(workspaceId, request) {
      const data = review(workspaceId);
      if (request.historyId?.startsWith('export:')) {
        const exportId = request.historyId.slice('export:'.length);
        const exact = state.promptExports?.[`${workspaceId}:${exportId}`];
        if (!exact) throw new Error('The requested prompt export does not belong to this workspace.');
        return structuredClone(exact);
      }
      const archivedReview = request.historyId?.startsWith('review:')
        ? state.archivedReviews?.[`${workspaceId}:${request.historyId}`]
        : undefined;
      const source = request.historyId
        ? (() => {
            const entry = data.history.find((candidate) => candidate.id === request.historyId);
            if (!entry) throw new Error('The requested review-history item does not belong to this workspace.');
            return entry.annotations ?? [];
          })()
        : data.annotations;
      let selected = source.filter((annotation) => request.annotationIds?.includes(annotation.id) ?? true);
      if (request.scope === 'feedback') selected = selected.filter((annotation) => annotation.state === 'open' && (annotation.kind === 'comment' || annotation.kind === 'suggestion'));
      if (request.scope === 'questions') selected = selected.filter((annotation) => annotation.state === 'open' && annotation.kind === 'question');
      if (request.scope === 'all') selected = selected.filter((annotation) => annotation.state === 'open');
      if (request.scope === 'focused_question') selected = selected.filter((annotation) => annotation.kind === 'question').slice(0, 1);
      const portable = request.pathStyle
        ? request.pathStyle === 'portable'
        : request.portable ?? true;
      const content = formatPrompt(archivedReview ?? data, selected, portable, request.scope);
      const exportId = uid();
      const item: ReviewHistoryItem = { id: `export:${exportId}`, type: 'export', label: promptTitle(request.scope), annotationCount: selected.length, createdAt: now() };
      data.history.unshift(item);
      const preview = { exportId, title: promptTitle(request.scope), content, annotationCount: selected.length, estimatedTokens: Math.ceil(content.length / 4) };
      state.promptExports ??= {};
      state.promptExports[`${workspaceId}:${exportId}`] = preview;
      saveState(state);
      return preview;
    },
    async savePromptExport(workspaceId, exportId, format) {
      if (!state.promptExports?.[`${workspaceId}:${exportId}`]) {
        throw new Error('The requested prompt export does not belong to this workspace.');
      }
      // The browser fixture deliberately has no native file picker. Its
      // explicit result lets UI tests exercise non-destructive cancellation.
      return { saved: false, format };
    },
    async getReviewHistory(workspaceId) { return structuredClone(review(workspaceId).history); },
    async restoreHistoryItem(workspaceId, historyId) {
      const data = review(workspaceId);
      const item = data.history.find((entry) => entry.id === historyId);
      if (!item?.annotations) throw new Error('This history entry does not contain an annotation checkpoint.');
      data.annotations = structuredClone(item.annotations);
      data.files = data.files.map((file) => ({ ...file, annotationCount: data.annotations.filter((annotation) => annotation.fileId === file.id).length }));
      data.workspace.draftCount = data.annotations.filter((annotation) => annotation.state === 'open' && !annotation.publishedId).length;
      saveState(state);
      return structuredClone(data);
    },
    async setViewed(workspaceId, fileId, viewed) {
      const data = review(workspaceId);
      data.files = data.files.map((file) => file.id === fileId ? { ...file, viewed } : file);
      data.workspace.progress = { viewed: data.files.filter((file) => file.viewed).length, total: data.files.length };
      saveState(state);
    },
    async getRepositorySetup(workspaceId): Promise<RepositorySetup[]> {
      return review(workspaceId).repositories.map((repository, index) => {
        const baseResolves = mockBaseResolves(repository.base);
        return {
          id: repository.id,
          path: repository.path,
          enabled: setupEnabledByRepository.get(repository.id) ?? true,
          branch: repository.branch,
          clean: index !== 1,
          changedFileCount: index === 1 ? 3 : 0,
          statusSummary: index === 1 ? 'Dirty: 1 staged, 1 unstaged, 1 untracked' : 'Clean',
          effectiveBase: repository.base,
          baseSource: repository.isOverride ? 'override' : 'inherited',
          baseOverride: repository.isOverride ? repository.base : undefined,
          resolvedBaseSha: baseResolves ? repository.mergeBase : undefined,
          mergeBaseSha: baseResolves ? repository.mergeBase : undefined,
          headSha: repository.head,
          ahead: index === 1 ? 2 : 0,
          behind: index === 1 ? 1 : 0,
          lastFetchAt: '2026-07-22T00:00:00.000Z',
          statusCheckedAt: now(),
          comparisonError: baseResolves ? undefined : `Base ${repository.base} does not resolve in this repository.`,
          issues: baseResolves ? [] : [{
            id: `repository:${repository.id}:missing_base:${repository.base}`,
            kind: 'missing_base_reference',
            severity: 'error',
            title: 'Base reference was not found',
            message: `Git cannot resolve \`${repository.base}\` in this repository. Choose a valid base.`,
            dismissible: true,
            actions: [{ kind: 'open_review_setup', label: 'Open Review setup' }]
          }]
        };
      });
    },
    async setRepositoryInclusion(workspaceId, repositoryIds, enabled): Promise<RepositorySetup[]> {
      for (const repositoryId of repositoryIds) setupEnabledByRepository.set(repositoryId, enabled);
      saveState(state);
      return this.getRepositorySetup(workspaceId);
    },
    async applyRepositoryBase(workspaceId, repositoryIds, base): Promise<RepositorySetup[]> {
      validateMockBase(base);
      const data = review(workspaceId);
      data.repositories = data.repositories.map((repository) => repositoryIds.includes(repository.id)
        ? { ...repository, base, isOverride: true }
        : repository);
      saveState(state);
      return this.getRepositorySetup(workspaceId);
    },
    async resetRepositoryBaseOverrides(workspaceId, repositoryIds): Promise<RepositorySetup[]> {
      const data = review(workspaceId);
      const inherited = data.workspace.defaultBase ?? 'origin/master';
      data.repositories = data.repositories.map((repository) => repositoryIds.includes(repository.id)
        ? { ...repository, base: inherited, isOverride: false }
        : repository);
      saveState(state);
      return this.getRepositorySetup(workspaceId);
    },
    async fetchRepositories(workspaceId, _repositoryIds): Promise<RepositorySetup[]> {
      return this.getRepositorySetup(workspaceId);
    },
    async configureBaselines(workspaceId, defaultBase, repositoryBases: RepositoryBaseOverride[] = []) {
      const data = review(workspaceId);
      if (defaultBase) validateMockBase(defaultBase);
      for (const override of repositoryBases) {
        if (override.base) validateMockBase(override.base);
      }
      if (defaultBase) data.workspace.defaultBase = defaultBase;
      const inherited = data.workspace.defaultBase ?? 'origin/master';
      data.repositories = data.repositories.map((repository) => {
        const override = repositoryBases.find((item) => item.repositoryId === repository.id || item.repositoryPath === repository.path);
        return { ...repository, base: override ? (override.base ?? inherited) : (defaultBase ?? repository.base), isOverride: Boolean(override?.base) };
      });
      saveState(state);
      return structuredClone(data);
    },
    async startNewReview(workspaceId, request: ReviewCaptureRequest = {}) {
      const data = review(workspaceId);
      if (request.base || request.repositoryBases?.length) await this.configureBaselines(workspaceId, request.base, request.repositoryBases);
      if (!state.activeReviewIds![workspaceId]) {
        captureInitialReview(data);
        saveState(state);
        return structuredClone(data);
      }
      if (!captureCanSucceed(data)) throw new Error('No repository capture succeeded. Correct the missing baseline and try again.');
      const archived: ReviewHistoryItem = { id: `review:${state.activeReviewIds![workspaceId]}`, type: 'review', label: 'Archived review', annotationCount: data.annotations.length, createdAt: now(), annotations: structuredClone(data.annotations) };
      const frozen = structuredClone(data);
      frozen.historical = true;
      frozen.historicalSessionId = state.activeReviewIds![workspaceId];
      state.archivedReviews![`${workspaceId}:${archived.id}`] = frozen;
      data.history.unshift(archived);
      state.activeReviewIds![workspaceId] = uid();
      data.annotations = [];
      data.files = data.files.map((file) => ({ ...file, annotationCount: 0, viewed: false }));
      data.workspace.progress = { viewed: 0, total: data.files.length };
      data.workspace.draftCount = 0;
      data.workspace.reviewReady = true;
      saveState(state);
      return structuredClone(data);
    },
    async refreshReview(workspaceId, request: ReviewCaptureRequest = {}) {
      const data = review(workspaceId);
      if (request.base || request.repositoryBases?.length) await this.configureBaselines(workspaceId, request.base, request.repositoryBases);
      if (!state.activeReviewIds![workspaceId]) throw new Error('Workspace has no active review to refresh.');
      if (!captureCanSucceed(data)) throw new Error('No repository capture succeeded. Correct the missing baseline and try again.');
      data.workspace.refreshAvailable = false;
      saveState(state);
      return structuredClone(data);
    },
    async previewFinishReview(workspaceId, request: FinishReviewRequest): Promise<FinishReviewPreview> {
      const data = review(workspaceId);
      const selected = data.annotations.filter((annotation) => request.annotationIds.includes(annotation.id));
      const previewToken = `preview-${uid()}`;
      const payloadJson = JSON.stringify({ body: request.summary, event: request.conclusion, comments: selected.map((annotation) => ({ path: data.files.find((file) => file.id === annotation.fileId)?.path, side: annotation.side.toUpperCase(), line: annotation.endLine, body: annotation.body })) }, null, 2);
      const pinnedHeadSha = data.repositories[0]?.head ?? 'HEAD';
      finishPreviews.set(previewToken, { workspaceId, annotationIds: selected.map((annotation) => annotation.id), payloadJson, pinnedHeadSha });
      return {
        annotationCount: selected.length,
        annotationIds: selected.map((annotation) => annotation.id),
        pinnedHeadSha,
        payloadJson,
        previewToken,
        requestFingerprint: `request-${previewToken}`,
        previewRequestFingerprint: `intent-${previewToken}`,
        annotationSnapshotFingerprint: `annotations-${previewToken}`,
        requiresReconciliation: false
      };
    },
    async finishReview(workspaceId, submission): Promise<FinishReviewResult> {
      const data = review(workspaceId);
      const preview = finishPreviews.get(submission.previewToken);
      if (!preview || preview.workspaceId !== workspaceId) throw new Error('The exact preview is no longer available. Reopen Finish review.');
      const id = `github-review-${uid().slice(0, 8)}`;
      for (const annotation of data.annotations) {
        if (preview.annotationIds.includes(annotation.id)) annotation.publishedId = id;
      }
      data.workspace.draftCount = data.annotations.filter((item) => !item.publishedId && item.state === 'open').length;
      saveState(state);
      finishPreviews.delete(submission.previewToken);
      return {
        reviewId: id,
        annotationCount: preview.annotationIds.length,
        annotationIds: [...preview.annotationIds],
        payloadJson: preview.payloadJson,
        previewToken: submission.previewToken,
        publicationStatus: 'submitted',
        submitted: true
      };
    },
    async abandonFinishReview(workspaceId, submission, _confirmPrepared = false) {
      const preview = finishPreviews.get(submission.previewToken);
      if (!preview || preview.workspaceId !== workspaceId) throw new Error('The Previewed review token is no longer available.');
      finishPreviews.delete(submission.previewToken);
    },
    async getSettings() { return { ...settings }; },
    async saveSettings(partial) {
      settings = { ...settings, ...partial };
      if (typeof localStorage !== 'undefined') localStorage.setItem(settingsKey, JSON.stringify(settings));
      return { ...settings };
    },
    async getWorkspaceUiState(workspaceId) {
      const reviewKey = activeReviewKey(workspaceId);
      return { ...defaultWorkspaceUiState(), ...(reviewKey ? workspaceUiStates[reviewKey] ?? {} : {}) };
    },
    async saveWorkspaceUiState(workspaceId, partial) {
      const reviewKey = activeReviewKey(workspaceId);
      if (!reviewKey) throw new Error('Workspace has no active review.');
      workspaceUiStates[reviewKey] = { ...defaultWorkspaceUiState(), ...(workspaceUiStates[reviewKey] ?? {}), ...partial };
      saveWorkspaceUiStates();
      return structuredClone(workspaceUiStates[reviewKey]);
    },
    async copyReviewItem(workspaceId, request: CopyRequest) {
      const data = review(workspaceId);
      const file = data.files.find((value) => value.id === request.fileId);
      if (!file) throw new Error('Review file no longer exists.');
      const source = positionedRows(file.id).rows.filter((row) => row.kind !== 'header').filter((row) => {
        const line = request.side === 'old' ? row.oldLine : row.newLine;
        return !request.startLine || (line && line >= request.startLine && line <= (request.endLine ?? request.startLine));
      });
      if (request.kind === 'path') return file.path;
      if (request.kind === 'provider_permalink') {
        if (!data.workspace.source.includes('github')) throw new Error('Provider permalinks are available only for GitHub PR reviews.');
        return `${data.workspace.location}/files#${file.path}`;
      }
      if (request.kind === 'patch' || request.kind === 'hunk') return source.map((row) => `${row.kind === 'addition' ? '+' : row.kind === 'deletion' ? '-' : ' '}${request.side === 'old' ? row.oldText ?? '' : row.newText ?? ''}`).join('\n');
      return source.map((row) => {
        const text = request.side === 'old' ? row.oldText ?? '' : row.newText ?? '';
        const line = request.side === 'old' ? row.oldLine : row.newLine;
        return request.kind === 'source_with_line_numbers' ? `${String(line ?? '').padStart(5)}  ${text}` : text;
      }).join('\n');
    },
    async openInExternalEditor(workspaceId, _fileId, _line) {
      const data = review(workspaceId);
      if (data.workspace.source.includes('github')) throw new Error('App-managed PR worktrees are review-only and cannot be opened for editing.');
    }
  };
}

function promptTitle(scope: PromptRequest['scope']) {
  return scope === 'focused_question' ? 'Focused code question' : scope === 'questions' ? 'Questions for investigation' : scope === 'feedback' ? 'Review feedback' : scope === 'selected' ? 'Selected review annotations' : 'Full review prompt';
}

function promptRelativePath(value: string | undefined, fallback: string) {
  if (!value) return fallback;
  const windowsAbsolute = /^[a-z]:[\\/]/i.test(value) || value.startsWith('\\');
  const unsafeSegment = value.split(/[\\/]/).some((segment) => segment === '..');
  return value.startsWith('/') || value.startsWith('~') || windowsAbsolute || value.includes('://') || unsafeSegment || /[\0\r\n`]/.test(value) ? fallback : value;
}

function promptWorkspaceName(value: string) {
  const trimmed = value.trim();
  const candidate = trimmed.includes('/') || trimmed.includes('\\')
    ? trimmed.split(/[\\/]/).filter(Boolean).at(-1) ?? 'workspace'
    : trimmed;
  return !candidate || /[\0\r\n`]/.test(candidate) ? 'workspace' : candidate;
}

function promptLogicalPath(repositoryPath: string, filePath: string) {
  return repositoryPath === '.' ? filePath : `${repositoryPath}/${filePath}`;
}

function promptBodyLabel(kind: Annotation['kind']) {
  return kind === 'question' ? 'Question' : kind === 'suggestion' ? 'Suggestion' : kind === 'file_note' ? 'File note' : kind === 'review_note' ? 'Review note' : 'Feedback';
}

function promptHeading(scope: PromptRequest['scope']) {
  return scope === 'focused_question' ? '# Read-only review question' : scope === 'questions' ? '# Review questions' : scope === 'all' ? '# Full LocalReview prompt' : scope === 'selected' ? '# Selected review annotations' : '# LocalReview feedback';
}

function promptInstruction(scope: PromptRequest['scope']) {
  return scope === 'focused_question'
    ? 'Answer the question using the supplied review context. Do not modify source files.'
    : scope === 'questions'
      ? 'Answer every included question using the supplied review context. Do not modify source files.'
      : scope === 'all'
        ? 'Address the included feedback, answer the included questions, and handle the included file and review notes. Preserve unrelated behavior and report how each item was handled.'
        : scope === 'selected'
          ? "Handle only the selected annotations below according to each annotation's stated kind and intent. Preserve unrelated behavior and report how each selected item was handled."
          : 'Address every actionable item below, preserve unrelated behavior, and report how each item was handled.';
}

export function formatPrompt(data: ReviewData, selected: Annotation[], portable: boolean, scope: PromptRequest['scope'] = 'all'): string {
  const repositoryById = new Map(data.repositories.map((repository) => [repository.id, repository]));
  const fileById = new Map(data.files.map((file) => [file.id, file]));
  const ordered = [...selected].sort((a, b) => `${repositoryById.get(a.repositoryId)?.path}/${fileById.get(a.fileId)?.path}`.localeCompare(`${repositoryById.get(b.repositoryId)?.path}/${fileById.get(b.fileId)?.path}`));
  const lines = [
    promptHeading(scope),
    '',
    promptInstruction(scope),
    '',
    `Workspace: ${promptWorkspaceName(data.workspace.name)}`,
    `Review source: ${data.workspace.source.join(' + ')}`,
    ''
  ];
  for (const item of ordered) {
    const repository = repositoryById.get(item.repositoryId);
    const file = fileById.get(item.fileId);
    if (item.kind === 'review_note' || !repository || !file) {
      lines.push('## Overall review');
      lines.push('Anchor: overall review');
      lines.push(`Kind: ${item.kind}`);
      lines.push('', `${promptBodyLabel(item.kind)}:`, '', item.body, '');
      continue;
    }
    const repositoryPath = promptRelativePath(repository.path, `repository:${repository.id}`);
    const filePath = promptRelativePath(file.path, `captured-file:${file.id}`);
    lines.push(`## Repository \`${repositoryPath}\` — \`${filePath}\``);
    lines.push(`Comparison: ${repository.base} (${repository.mergeBase}) → ${repository.head}`);
    if (!portable) lines.push(`Logical path: \`${promptLogicalPath(repositoryPath, filePath)}\``);
    lines.push(item.kind === 'file_note' || item.startLine <= 0
      ? 'Anchor: whole file'
      : `Anchor: ${item.side} lines ${item.startLine}${item.startLine === item.endLine ? '' : `-${item.endLine}`}`);
    lines.push(`Kind: ${item.kind}`);
    lines.push('', `${promptBodyLabel(item.kind)}:`, '', item.body, '', 'Selected source:', '```');
    lines.push(item.selectedSource, '```', '');
  }
  return lines.join('\n');
}

/** Exposed for adapter-contract tests; production selects it only in Tauri. */
export function createNativeReviewApi(invoke: TauriInvoke): ReviewApi {
  return {
    pickLocalFolder: () => invoke('pick_local_folder'),
    openWorkspace: (request) => invoke('open_workspace', { request }),
    openGitHubPr: (url) => invoke('open_github_pr', { url }),
    openSshWorkspace: (target) => invoke('open_ssh_workspace', { target }),
    reconnectSshWorkspace: (workspaceId) => invoke('reconnect_ssh_workspace', { workspaceId }),
    listWorkspaces: () => invoke('list_workspaces'),
    listArchivedWorkspaces: () => invoke('list_archived_workspaces'),
    reopenArchivedWorkspace: (workspaceId) => invoke('reopen_archived_workspace', { workspaceId }),
    archiveWorkspace: (workspaceId) => invoke('archive_workspace', { workspaceId }),
    updateWorkspaceMetadata: (workspaceId, metadata) => invoke('update_workspace_metadata', { workspaceId, ...metadata }),
    getPersistenceDiagnostics: () => invoke('get_persistence_diagnostics'),
    deleteWorkspace: (workspaceId) => invoke('delete_workspace', { workspaceId }),
    loadReview: (workspaceId) => invoke('load_review', { workspaceId }),
    loadArchivedReview: (workspaceId, historyId) => invoke('load_archived_review', { workspaceId, historyId }),
    getReviewFileClassifications: (workspaceId) => invoke('get_review_file_classifications', { workspaceId }),
    getCapturedBlame: (workspaceId, fileId, side, startLine, endLine) => invoke('get_captured_blame', { workspaceId, fileId, side, startLine, endLine }),
    getCommitContext: (workspaceId, request) => invoke('get_commit_context', { workspaceId, request }),
    getChangedSincePreviousReview: (workspaceId, repositoryId) => invoke('get_changed_since_previous_review', { workspaceId, repositoryId }),
    getGitHubUpdateStatus: (workspaceId) => invoke('get_github_update_status', { workspaceId }),
    getGitHubPullRequest: (workspaceId) => invoke('get_github_pull_request', { workspaceId }),
    getGitHubThreads: (workspaceId) => invoke('get_github_threads', { workspaceId }),
    getGitHubConversation: (workspaceId) => invoke('get_github_conversation', { workspaceId }),
    // No browser/mock fallback here. A packaged desktop must obtain bounded,
    // canonical presentation data from Rust rather than silently inventing a
    // review from fixture rows.
    getPresentationWindow: (request) => invoke('get_presentation_window', { request }),
    resolvePresentationLocation: (fileId, mode, side, line, comparisonId) => invoke('resolve_presentation_location', { fileId, comparisonId, mode, side, line }),
    getCapturedSourceRange: (fileId, side, startLine, endLine, comparisonId) => invoke('get_captured_source_range', { fileId, comparisonId, side, startLine, endLine }),
    getRows: (fileId, mode) => invoke('get_presentation_rows', { fileId, mode }),
    expandHunk: (fileId, hunkId, contextLines, comparisonId) => invoke('expand_hunk_context', { fileId, comparisonId, hunkId, contextLines }),
    getOutline: (fileId, side, comparisonId) => invoke('get_outline', { fileId, comparisonId, side }),
    openSymbolNavigation: (input) => invoke('open_symbol_navigation', { input }),
    querySymbolNavigation: (input) => invoke('query_symbol_navigation', { input }),
    getSymbolSource: (input) => invoke('get_symbol_source', { input }),
    getRepositoryFiles: (input) => invoke('get_repository_files', { input }),
    openRepositorySource: (input) => invoke('open_repository_source', { input }),
    saveAnnotation: (_workspaceId, annotation) => invoke('save_annotation', { annotation }),
    getAnnotationDraft: (workspaceId) => invoke('get_annotation_draft', { workspaceId }),
    saveAnnotationDraft: (draft) => invoke('save_annotation_draft', { draft }),
    clearAnnotationDraft: (workspaceId) => invoke('clear_annotation_draft', { workspaceId }),
    deleteAnnotation: (workspaceId, annotationId) => invoke('delete_annotation', { workspaceId, annotationId }),
    setAnnotationState: (workspaceId, annotationId, state) => invoke('set_annotation_state', { workspaceId, annotationId, state }),
    archiveAnnotations: (workspaceId) => invoke('archive_annotations', { workspaceId }),
    restoreAnnotations: (workspaceId, annotations) => invoke('restore_annotations', { workspaceId, annotations }),
    generatePrompt: (workspaceId, request) => invoke('generate_prompt', { workspaceId, request }),
    savePromptExport: (workspaceId, exportId, format) => invoke('save_prompt_export', { workspaceId, exportId, format }),
    getReviewHistory: (workspaceId) => invoke('get_review_history', { workspaceId }),
    restoreHistoryItem: (workspaceId, historyId) => invoke('restore_history_item', { workspaceId, historyId }),
    setViewed: (workspaceId, fileId, viewed) => invoke('set_viewed', { workspaceId, fileId, viewed }),
    getRepositorySetup: (workspaceId) => invoke('get_repository_setup', { workspaceId }),
    setRepositoryInclusion: (workspaceId, repositoryIds, enabled) => invoke('set_repository_inclusion', { workspaceId, input: { repositoryIds, enabled } }),
    applyRepositoryBase: (workspaceId, repositoryIds, base) => invoke('apply_repository_base', { workspaceId, input: { repositoryIds, base } }),
    resetRepositoryBaseOverrides: (workspaceId, repositoryIds) => invoke('reset_repository_base_overrides', { workspaceId, input: { repositoryIds } }),
    fetchRepositories: (workspaceId, repositoryIds) => invoke('fetch_repositories', { workspaceId, repositoryIds }),
    configureBaselines: (workspaceId, defaultBase, repositoryBases) => invoke('configure_baselines', { workspaceId, defaultBase, repositoryBases }),
    startNewReview: (workspaceId, request) => invoke('start_new_review', { workspaceId, request }),
    refreshReview: (workspaceId, request) => invoke('refresh_review', { workspaceId, request }),
    previewFinishReview: (workspaceId, request) => invoke('preview_finish_review', { workspaceId, request }),
    finishReview: (workspaceId, submission) => invoke('finish_review', { workspaceId, submission }),
    abandonFinishReview: (workspaceId, submission, confirmPrepared = false) => invoke('abandon_finish_review', { workspaceId, submission, confirmPrepared }),
    getSettings: () => invoke('get_ui_settings'),
    saveSettings: (settings) => invoke('save_ui_settings', { settings }),
    getWorkspaceUiState: (workspaceId) => invoke('get_workspace_ui_state', { workspaceId }),
    saveWorkspaceUiState: (workspaceId, state) => invoke('save_workspace_ui_state', { workspaceId, state }),
    copyReviewItem: (workspaceId, request) => invoke('copy_review_item', { workspaceId, request }),
    openInExternalEditor: (workspaceId, fileId, line) => invoke('open_in_external_editor', { workspaceId, fileId, line })
  };
}

/** Uses typed Tauri commands in the desktop app and a persistent mock in a browser. */
export function createReviewApi(): ReviewApi {
  return isTauri() ? createNativeReviewApi(tauriInvoke as TauriInvoke) : makeMockApi();
}
