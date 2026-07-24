import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReviewData, ReviewSettings, ViewportRequest, Workspace } from './lib/types';

const harness = vi.hoisted(() => {
  const state = {
    implementation: {} as Record<PropertyKey, (...args: any[]) => any>,
    native: false,
    listeners: new Map<string, Set<(event: { payload: any }) => void>>()
  };
  const api = new Proxy({}, {
    get: (_target, property) => (...args: any[]) => {
      const operation = state.implementation[property];
      if (!operation) throw new Error(`Unexpected review API call: ${String(property)}`);
      return operation(...args);
    }
  });
  return { api, state };
});

vi.mock('./lib/api', () => ({
  createReviewApi: () => harness.api,
  copyText: async () => {}
}));

vi.mock('@tauri-apps/api/core', () => ({
  isTauri: () => harness.state.native
}));

vi.mock('@tauri-apps/api/event', () => ({
  listen: async (name: string, callback: (event: { payload: any }) => void) => {
    const callbacks = harness.state.listeners.get(name) ?? new Set();
    callbacks.add(callback);
    harness.state.listeners.set(name, callbacks);
    return () => callbacks.delete(callback);
  }
}));

import App from './App.svelte';

const targets: HTMLElement[] = [];
const components: ReturnType<typeof mount>[] = [];

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

const settings: ReviewSettings = {
  fontScale: 1, leftWidth: 244, rightWidth: 332, leftCollapsed: false, rightCollapsed: false,
  fetchOnReview: false, theme: 'dark', codeFont: 'SF Mono', externalEditor: 'system', tabWidth: 2,
  showWhitespace: false, wrapLines: false, vimNavigation: false,
  promptPathStyle: 'absolute', promptIncludeDiffHunks: false, promptIncludeGitState: false,
  shortcuts: {}
};

function workspace(id: string, name: string, reviewReady = true): Workspace {
  return {
    id, name, reviewReady, source: ['local'], location: `/${id}`, detail: '1 repository',
    defaultBase: reviewReady ? undefined : 'origin/master',
    progress: { viewed: 0, total: reviewReady ? 1 : 0 }, draftCount: 0, connection: 'connected'
  };
}

function review(value: Workspace): ReviewData {
  const suffix = value.id.replace('workspace-', '');
  return {
    workspace: value,
    repositories: [{ id: `repo-${suffix}`, name: `repo-${suffix}`, path: '.', branch: 'feature', base: 'origin/main', mergeBase: 'abc123', head: 'def456' }],
    files: value.reviewReady === false || value.id.includes('empty') ? [] : [{
      id: `file-${suffix}`, repositoryId: `repo-${suffix}`, path: `${suffix}.ts`, status: 'modified',
      additions: 1, deletions: 0, hunkCount: 0, language: 'TypeScript', viewed: false, annotationCount: 0
    }],
    annotations: [],
    history: []
  };
}

function installApi(workspaces: Workspace[], initialSettings: ReviewSettings = settings) {
  const reviews = new Map(workspaces.map((value) => [value.id, review(value)]));
  const archivedWorkspaceIds = new Set(workspaces.filter((value) => value.archived).map((value) => value.id));
  const deletedWorkspaceIds = new Set<string>();
  let savedSettings = structuredClone(initialSettings);
  const durableUiStates = new Map<string, Record<string, unknown>>();
  const defaultUiState = { mode: 'unified', fullFileSide: 'both', scrollTop: 0, splitRatio: .5, rightTab: 'files' };
  const calls = {
    drafts: [] as Array<Record<string, unknown>>,
    uiStates: [] as Array<{ workspaceId: string; state: Record<string, unknown> }>,
    sessionReads: [] as string[],
    presentationRequests: [] as ViewportRequest[],
    locationRequests: [] as Array<{ fileId: string; mode: string; side: 'old' | 'new'; line: number }>,
    setupReads: [] as string[],
    inclusionWrites: [] as Array<{ workspaceId: string; repositoryIds: string[]; enabled: boolean }>,
    baseApplications: [] as Array<{ workspaceId: string; repositoryIds: string[]; base: string }>,
    baselineWrites: [] as Array<{ workspaceId: string; defaultBase?: string; repositoryBases?: unknown[] }>,
    startRequests: [] as Array<{ workspaceId: string; request?: Record<string, unknown> }>,
    refreshRequests: [] as Array<{ workspaceId: string; request?: Record<string, unknown> }>,
    settingsWrites: [] as Array<Partial<ReviewSettings>>,
    viewedWrites: [] as Array<{ workspaceId: string; fileId: string; viewed: boolean }>,
    workspaceArchives: [] as string[],
    workspaceDeletes: [] as string[],
    operationOrder: [] as string[]
  };
  harness.state.implementation = {
    listWorkspaces: async () => structuredClone(workspaces.filter((workspace) => !deletedWorkspaceIds.has(workspace.id) && !archivedWorkspaceIds.has(workspace.id))),
    listArchivedWorkspaces: async () => structuredClone(workspaces
      .filter((workspace) => !deletedWorkspaceIds.has(workspace.id) && archivedWorkspaceIds.has(workspace.id))
      .map((workspace) => ({ ...workspace, archived: true }))),
    reopenArchivedWorkspace: async (workspaceId: string) => {
      archivedWorkspaceIds.delete(workspaceId);
      const value = reviews.get(workspaceId)!;
      value.workspace = { ...value.workspace, archived: false };
      return structuredClone(value.workspace);
    },
    archiveWorkspace: async (workspaceId: string) => {
      calls.operationOrder.push(`archive:${workspaceId}`);
      calls.workspaceArchives.push(workspaceId);
      archivedWorkspaceIds.add(workspaceId);
      const value = reviews.get(workspaceId)!;
      value.workspace = { ...value.workspace, archived: true };
    },
    deleteWorkspace: async (workspaceId: string) => {
      calls.operationOrder.push(`delete:${workspaceId}`);
      calls.workspaceDeletes.push(workspaceId);
      archivedWorkspaceIds.delete(workspaceId);
      deletedWorkspaceIds.add(workspaceId);
      reviews.delete(workspaceId);
    },
    getReviewHistory: async (workspaceId: string) => structuredClone(reviews.get(workspaceId)?.history ?? []),
    getSettings: async () => structuredClone(savedSettings),
    saveSettings: async (partial: Partial<ReviewSettings>) => {
      calls.settingsWrites.push(structuredClone(partial));
      savedSettings = { ...savedSettings, ...partial };
      return structuredClone(savedSettings);
    },
    loadReview: async (workspaceId: string) => structuredClone(reviews.get(workspaceId)),
    getReviewFileClassifications: async (workspaceId: string) => {
      calls.sessionReads.push(`classifications:${workspaceId}`);
      return [];
    },
    getWorkspaceUiState: async (workspaceId: string) => {
      calls.sessionReads.push(`ui:${workspaceId}`);
      return structuredClone({ ...defaultUiState, ...(durableUiStates.get(workspaceId) ?? {}) });
    },
    getAnnotationDraft: async (workspaceId: string) => {
      calls.sessionReads.push(`draft:${workspaceId}`);
      return undefined;
    },
    getPresentationWindow: async (request: ViewportRequest) => {
      calls.sessionReads.push(`presentation:${request.fileId}`);
      calls.presentationRequests.push(structuredClone(request));
      return {
        fileId: request.fileId, mode: request.mode, generation: request.generation,
        startRow: 0, endRow: 1, totalRows: 1, rows: [{ id: `${request.fileId}:1`, kind: 'context', oldLine: 1, newLine: 1, oldText: 'old', newText: 'new' }],
        hunks: [], oldTokens: [], newTokens: []
      };
    },
    resolvePresentationLocation: async (fileId: string, mode: string, side: 'old' | 'new', line: number) => {
      calls.locationRequests.push({ fileId, mode, side, line });
      return { rowIndex: Math.max(0, line - 1), side, line };
    },
    getOutline: async (fileId: string) => {
      calls.sessionReads.push(`outline:${fileId}`);
      return [];
    },
    getRepositorySetup: async (workspaceId: string) => {
      calls.setupReads.push(workspaceId);
      const pending = reviews.get(workspaceId)?.workspace.reviewReady === false;
      return [{
        id: `repo-${workspaceId.replace('workspace-', '')}`, path: '.', enabled: true, branch: 'feature',
        statusSummary: 'Clean', effectiveBase: pending ? 'origin/master' : 'origin/main', baseSource: 'workspace',
        suggestedBase: pending ? 'origin/main' : undefined,
        comparisonError: pending ? 'origin/master does not resolve to a commit' : undefined
      }];
    },
    setRepositoryInclusion: async (workspaceId: string, repositoryIds: string[], enabled: boolean) => {
      calls.inclusionWrites.push({ workspaceId, repositoryIds: [...repositoryIds], enabled });
      return harness.state.implementation.getRepositorySetup(workspaceId);
    },
    applyRepositoryBase: async (workspaceId: string, repositoryIds: string[], base: string) => {
      calls.baseApplications.push({ workspaceId, repositoryIds: [...repositoryIds], base });
      return harness.state.implementation.getRepositorySetup(workspaceId);
    },
    configureBaselines: async (workspaceId: string, defaultBase?: string, repositoryBases?: unknown[]) => {
      calls.baselineWrites.push({ workspaceId, defaultBase, repositoryBases });
      return structuredClone(reviews.get(workspaceId));
    },
    startNewReview: async (workspaceId: string, request?: Record<string, unknown>) => {
      calls.startRequests.push({ workspaceId, request });
      const current = reviews.get(workspaceId)!;
      const captured = review({ ...current.workspace, reviewReady: true, progress: { viewed: 0, total: 1 } });
      reviews.set(workspaceId, captured);
      return structuredClone(captured);
    },
    refreshReview: async (workspaceId: string, request?: Record<string, unknown>) => {
      calls.refreshRequests.push({ workspaceId, request: structuredClone(request) });
      return structuredClone(reviews.get(workspaceId));
    },
    saveWorkspaceUiState: async (workspaceId: string, state: Record<string, unknown>) => {
      calls.operationOrder.push(`ui:${workspaceId}`);
      calls.uiStates.push({ workspaceId, state: structuredClone(state) });
      const saved = { ...defaultUiState, ...(durableUiStates.get(workspaceId) ?? {}), ...state };
      durableUiStates.set(workspaceId, saved);
      return structuredClone(saved);
    },
    saveAnnotationDraft: async (draft: Record<string, unknown>) => {
      calls.operationOrder.push(`draft:${draft.workspaceId}`);
      calls.drafts.push(structuredClone(draft));
    },
    setViewed: async (workspaceId: string, fileId: string, viewed: boolean) => {
      calls.viewedWrites.push({ workspaceId, fileId, viewed });
      const current = reviews.get(workspaceId);
      if (!current) return;
      current.files = current.files.map((file) => file.id === fileId ? { ...file, viewed } : file);
      current.workspace.progress = {
        viewed: current.files.filter((file) => file.viewed).length,
        total: current.files.length
      };
    }
  };
  return calls;
}

function target() {
  const element = document.createElement('div');
  document.body.append(element);
  targets.push(element);
  return element;
}

async function settle(rounds = 3) {
  for (let index = 0; index < rounds; index += 1) {
    await Promise.resolve();
    await tick();
    flushSync();
  }
}

async function waitForUi(predicate: () => boolean, message: string, rounds = 40) {
  for (let index = 0; index < rounds; index += 1) {
    await settle(1);
    if (predicate()) return;
  }
  throw new Error(`Timed out waiting for ${message}; status was: ${document.querySelector('.statusbar')?.textContent?.trim() ?? '<missing>'}`);
}

function emitNative(name: string, payload: unknown) {
  for (const callback of harness.state.listeners.get(name) ?? []) callback({ payload });
}

beforeEach(() => {
  localStorage.clear();
  harness.state.native = false;
  harness.state.listeners.clear();
});

afterEach(async () => {
  vi.useRealTimers();
  for (const component of components.splice(0)) unmount(component);
  await settle();
  for (const element of targets.splice(0)) element.remove();
});

describe('App review-session persistence boundaries', () => {
  it('persists the line-wrap preference across app restarts', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const first = mount(App, { target: target() });
    await settle(6);

    const wrap = document.querySelector<HTMLButtonElement>('.wrap-toggle')!;
    expect(wrap.getAttribute('aria-pressed')).toBe('false');
    wrap.click();
    await settle(5);
    expect(calls.settingsWrites.at(-1)).toEqual({ wrapLines: true });
    expect(document.querySelector('.diff-viewport')?.classList.contains('wrap-lines')).toBe(true);

    unmount(first);
    await settle();
    components.push(mount(App, { target: target() }));
    await settle(6);
    const toggles = [...document.querySelectorAll<HTMLButtonElement>('.wrap-toggle')];
    expect(toggles.at(-1)?.getAttribute('aria-pressed')).toBe('true');
    const viewports = [...document.querySelectorAll('.diff-viewport')];
    expect(viewports.at(-1)?.classList.contains('wrap-lines')).toBe(true);
  });

  it('restores the last selected workspace and persists later rail selections', async () => {
    const first = workspace('workspace-first', 'First');
    const second = workspace('workspace-second', 'Second');
    const calls = installApi([first, second], { ...settings, lastWorkspaceId: second.id });
    components.push(mount(App, { target: target() }));
    await settle(6);

    const tabs = [...document.querySelectorAll<HTMLButtonElement>('.workspace-tab')];
    expect(tabs.find((tab) => tab.textContent?.includes('Second'))?.getAttribute('aria-selected')).toBe('true');
    tabs.find((tab) => tab.textContent?.includes('First'))?.click();
    await settle(6);

    expect(calls.settingsWrites.at(-1)).toEqual({ lastWorkspaceId: first.id });
    expect(tabs.find((tab) => tab.textContent?.includes('First'))?.getAttribute('aria-selected')).toBe('true');
  });

  it('toggles viewed state exactly once when the filename opens an already selected file', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const initial = review(local);
    initial.files[0].viewed = true;
    initial.workspace.progress = { viewed: 1, total: 1 };
    harness.state.implementation.loadReview = async () => structuredClone(initial);
    components.push(mount(App, { target: target() }));
    await waitForUi(
      () => document.querySelector('.statusbar')?.textContent?.includes('Opened LocalReview.') === true,
      'workspace open completion'
    );

    expect(calls.viewedWrites).toEqual([]);
    const filename = document.querySelector<HTMLButtonElement>('.file-select')!;
    expect(filename.getAttribute('aria-pressed')).toBe('true');
    filename.click();
    await waitForUi(() => calls.viewedWrites.length === 1, 'single unview write');
    expect(calls.viewedWrites).toEqual([{
      workspaceId: local.id,
      fileId: 'file-localreview',
      viewed: false
    }]);
    expect(document.querySelector<HTMLButtonElement>('.file-select')?.getAttribute('aria-pressed')).toBe('false');
    expect(document.querySelector('.review-progress strong')?.textContent).toBe('0/1');

    document.querySelector<HTMLButtonElement>('.file-select')?.click();
    await waitForUi(() => calls.viewedWrites.length === 2, 'single re-view write');
    expect(calls.viewedWrites.at(-1)).toEqual({
      workspaceId: local.id,
      fileId: 'file-localreview',
      viewed: true
    });
    expect(document.querySelector<HTMLButtonElement>('.file-select')?.getAttribute('aria-pressed')).toBe('true');
    expect(document.querySelector('.review-progress strong')?.textContent).toBe('1/1');
  });

  it('keeps the selected tab and visible stable panel identical through asynchronous hydration', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const hydratedState = deferred<Record<string, unknown>>();
    harness.state.implementation.getWorkspaceUiState = () => hydratedState.promise;
    components.push(mount(App, { target: target() }));
    await settle(4);

    const selectedTab = () => document.querySelector<HTMLButtonElement>('.panel-tabs [role="tab"][aria-selected="true"]');
    const panelBodies = () => [...document.querySelectorAll<HTMLElement>('.right-panel-body')];
    const panelBody = () => panelBodies().find((panel) => !panel.hidden);
    const expectSelectedPanel = (tab: 'files' | 'comments' | 'outline') => {
      expect(selectedTab()?.id).toBe(`right-panel-tab-${tab}`);
      expect(selectedTab()?.getAttribute('aria-controls')).toBe(`right-panel-${tab}`);
      expect(panelBody()?.id).toBe(`right-panel-${tab}`);
      expect(panelBody()?.dataset.rightPanelBody).toBe(tab);
      expect(panelBody()?.getAttribute('aria-labelledby')).toBe(`right-panel-tab-${tab}`);
      expect(panelBodies().filter((panel) => !panel.hidden)).toHaveLength(1);
      for (const panel of panelBodies()) {
        expect(panel.getAttribute('aria-hidden')).toBe(String(panel.dataset.rightPanelBody !== tab));
      }
    };
    const clickTab = async (label: string) => {
      [...document.querySelectorAll<HTMLButtonElement>('.panel-tabs [role="tab"]')]
        .find((button) => button.textContent?.startsWith(label))?.click();
      await settle();
    };

    // All three panels have stable identities before the native per-session
    // state read resolves. Only Files is initially visible and accessible.
    expect(panelBodies().map((panel) => panel.dataset.rightPanelBody)).toEqual(['files', 'comments', 'outline']);
    expectSelectedPanel('files');
    expect(panelBody()?.querySelector('.panel-filter')).not.toBeNull();

    hydratedState.resolve({
      mode: 'unified',
      fullFileSide: 'new',
      scrollTop: 0,
      splitRatio: .5,
      rightTab: 'comments'
    });
    await settle(6);
    expect(selectedTab()?.textContent).toBe('Comments');
    expectSelectedPanel('comments');
    expect(panelBody()?.querySelector('.comment-actions')).not.toBeNull();
    expect(panelBody()?.querySelector('.panel-filter')).toBeNull();
    expect(panelBody()?.querySelector('.outline-header')).toBeNull();
    expect(document.querySelector('.statusbar')?.textContent).toContain('Opened LocalReview.');
    expect(document.querySelector('.statusbar')?.textContent).not.toMatch(/Opening|Loading/);
    expect(document.querySelector('.busy-indicator')).toBeNull();
    expect(calls.sessionReads).toContain('presentation:file-localreview');

    const hydratedCommentsBody = panelBody();
    await clickTab('Comments');
    expect(selectedTab()?.textContent).toBe('Comments');
    expectSelectedPanel('comments');
    expect(panelBody()).toBe(hydratedCommentsBody);
    expect(panelBody()?.querySelector('.comment-actions')).not.toBeNull();

    await clickTab('Outline');
    expect(selectedTab()?.textContent).toBe('Outline');
    expectSelectedPanel('outline');
    expect(panelBody()?.querySelector('.outline-header')).not.toBeNull();
    expect(panelBody()?.querySelector('.comment-actions')).toBeNull();

    await clickTab('Comments');
    expect(selectedTab()?.textContent).toBe('Comments');
    expectSelectedPanel('comments');
    expect(panelBody()?.querySelector('.comment-actions')).not.toBeNull();
    expect(panelBody()?.querySelector('.outline-header')).toBeNull();

    await clickTab('Files');
    expect(selectedTab()?.textContent).toBe('Files');
    expectSelectedPanel('files');
    expect(panelBody()?.querySelector('.panel-filter')).not.toBeNull();
    expect(panelBody()?.querySelector('.comment-actions')).toBeNull();

    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle();
    expect(calls.uiStates.at(-1)?.state).toMatchObject({ rightTab: 'files' });
  });

  it('repeatedly closes and reopens the right panel in Full File while settings persistence is queued', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local], { ...settings, rightCollapsed: false });
    const firstSettingsWrite = deferred<void>();
    let settingsWriteCount = 0;
    let persistedSettings = { ...settings, rightCollapsed: false };
    harness.state.implementation.saveSettings = async (partial: Partial<ReviewSettings>) => {
      calls.settingsWrites.push(structuredClone(partial));
      settingsWriteCount += 1;
      if (settingsWriteCount === 1) await firstSettingsWrite.promise;
      persistedSettings = { ...persistedSettings, ...partial };
      return structuredClone(persistedSettings);
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    const fullFile = [...document.querySelectorAll<HTMLButtonElement>('.mode-picker button')]
      .find((button) => button.textContent === 'Full File')!;
    fullFile.click();
    await settle(5);
    expect(fullFile.getAttribute('aria-selected')).toBe('true');
    const panel = document.querySelector<HTMLElement>('.review-panel')!;
    const restore = document.querySelector<HTMLButtonElement>('[aria-label="Open files and review panel"]')!;
    expect(panel.hidden).toBe(false);
    expect(restore.hidden).toBe(true);

    document.querySelector<HTMLButtonElement>('[aria-label="Close review panel"]')!.click();
    expect(panel.hidden).toBe(true);
    expect(restore.hidden).toBe(false);

    // Reopen while the close write is still unresolved. The large file-tree
    // panel and its restore target keep stable DOM identities, so WebKit
    // cannot leave a visible button backed by the prior hit-test subtree.
    restore.click();

    expect(panel.hidden).toBe(false);
    expect(restore.hidden).toBe(true);
    expect(document.querySelector<HTMLElement>('.app-shell')?.getAttribute('style')).toContain('minmax(0,1fr)');
    await settle(1);
    expect(document.activeElement?.id).toBe('right-panel-tab-files');

    // A second close also takes effect immediately while both prior native
    // writes are queued.
    document.querySelector<HTMLButtonElement>('[aria-label="Close review panel"]')!.click();
    expect(panel.hidden).toBe(true);
    expect(restore.hidden).toBe(false);

    firstSettingsWrite.resolve();
    await settle(8);
    expect(panel.hidden).toBe(true);
    expect(calls.settingsWrites.slice(-3)).toEqual([
      { rightCollapsed: true },
      { rightCollapsed: false },
      { rightCollapsed: true }
    ]);

    restore.click();
    expect(panel.hidden).toBe(false);
    expect(restore.hidden).toBe(true);
    await settle(6);
    expect(calls.settingsWrites).toContainEqual({ rightCollapsed: false });
  });

  it('surfaces an asynchronous workspace hydration failure instead of remaining stuck opening', async () => {
    const local = workspace('workspace-localreview', 'Broken state');
    installApi([local]);
    harness.state.implementation.getWorkspaceUiState = async () => {
      throw new Error('saved tab state is unavailable');
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    const status = document.querySelector('.statusbar')?.textContent ?? '';
    expect(status).toContain('Could not open Broken state: saved tab state is unavailable');
    expect(status).not.toContain('Opening Broken state');
    expect(document.querySelector('.busy-indicator')).toBeNull();
  });

  it('expands, hides, and shows Full File deletions in history without persisting review state', async () => {
    const local = workspace('workspace-localreview', 'Historical review');
    const calls = installApi([local]);
    const historical = review(local);
    historical.historical = true;
    historical.historicalSessionId = 'historical-session';
    historical.files[0] = {
      ...historical.files[0],
      deletions: 2,
      hunkCount: 1,
      comparisonId: 'comparison-historical'
    };
    harness.state.implementation.loadReview = async () => structuredClone(historical);
    harness.state.implementation.getWorkspaceUiState = async () => ({
      mode: 'full',
      fullFileSide: 'new',
      scrollTop: 0,
      splitRatio: .5,
      rightTab: 'files',
      expandedFullFileDeletionBlocks: []
    });
    const blockId = 'comparison-historical:old:4-5';
    harness.state.implementation.getPresentationWindow = async (request: ViewportRequest) => {
      calls.presentationRequests.push(structuredClone(request));
      const expanded = (request.ephemeralExpandedFullFileDeletionBlocks ?? []).includes(blockId);
      const gate = {
        id: 'historical-gate',
        kind: 'deletion_gate',
        oldLine: 4,
        omittedEndLine: 5,
        omittedBlockId: blockId,
        omittedCount: 2,
        omittedSide: 'old',
        omittedExpanded: expanded,
        hasAnnotation: false
      };
      return {
        fileId: request.fileId,
        mode: request.mode,
        generation: request.generation,
        startRow: 0,
        totalRows: expanded ? 3 : 1,
        rows: expanded
          ? [gate, { id: 'old-4', kind: 'deletion', oldLine: 4, oldText: 'removed four' }, { id: 'old-5', kind: 'deletion', oldLine: 5, oldText: 'removed five' }]
          : [gate],
        hunks: [{ id: 'historical-hunk', rowIndex: 0, oldLine: 4, header: '@@ -4,2 +4,0 @@' }],
        omittedBlocks: [{ id: blockId, side: 'old', startLine: 4, endLine: 5, count: 2, expanded, rowIndex: 0 }],
        oldTokens: [],
        newTokens: [],
        highlightStatus: 'highlighted'
      };
    };
    components.push(mount(App, { target: target() }));
    await settle(10);

    expect(document.querySelector('.historical-banner')?.textContent).toContain('read-only');
    const gate = document.querySelector<HTMLButtonElement>('.deletion-gate-toggle')!;
    const showAll = [...document.querySelectorAll<HTMLButtonElement>('.full-deletion-controls button')]
      .find((button) => button.textContent === 'Show all deletions')!;
    const hideAll = [...document.querySelectorAll<HTMLButtonElement>('.full-deletion-controls button')]
      .find((button) => button.textContent === 'Hide all deletions')!;
    expect(gate.disabled).toBe(false);
    expect(showAll.disabled).toBe(false);
    expect(hideAll.disabled).toBe(true);

    gate.click();
    await settle(6);
    expect(calls.presentationRequests.at(-1)?.ephemeralExpandedFullFileDeletionBlocks).toEqual([blockId]);
    expect(document.querySelectorAll('.diff-row.removed')).toHaveLength(2);
    expect(calls.uiStates).toHaveLength(0);

    const refreshedHideAll = [...document.querySelectorAll<HTMLButtonElement>('.full-deletion-controls button')]
      .find((button) => button.textContent === 'Hide all deletions')!;
    refreshedHideAll.click();
    await settle(6);
    expect(calls.presentationRequests.at(-1)?.ephemeralExpandedFullFileDeletionBlocks).toEqual([]);
    expect(document.querySelectorAll('.diff-row.removed')).toHaveLength(0);

    const refreshedShowAll = [...document.querySelectorAll<HTMLButtonElement>('.full-deletion-controls button')]
      .find((button) => button.textContent === 'Show all deletions')!;
    refreshedShowAll.click();
    await settle(6);
    expect(calls.presentationRequests.at(-1)?.ephemeralExpandedFullFileDeletionBlocks).toEqual([blockId]);
    expect(document.querySelectorAll('.diff-row.removed')).toHaveLength(2);
    expect(calls.uiStates).toHaveLength(0);
  });

  it('flushes the active draft and viewport before archiving, then reopens the durable snapshot', async () => {
    const local = workspace('workspace-localreview', 'Short-lived review');
    const calls = installApi([local]);
    components.push(mount(App, { target: target() }));
    await settle(6);

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.click();
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'File note')?.click();
    await settle();
    const draft = document.querySelector<HTMLTextAreaElement>('[aria-label="Annotation text"]')!;
    draft.value = 'Keep this unsaved thought across archival';
    draft.dispatchEvent(new Event('input', { bubbles: true }));

    document.querySelector<HTMLButtonElement>('[aria-label="Archive workspace Short-lived review"]')?.click();
    await settle();
    expect(document.querySelector('#archive-workspace-title')?.textContent).toContain('Archive Short-lived review?');
    expect(document.querySelector('[aria-labelledby="archive-workspace-title"]')?.textContent).toContain('folder and Git repositories on disk are untouched');
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Archive workspace')?.click();
    await settle(10);

    expect(calls.workspaceArchives).toEqual([local.id]);
    const archiveIndex = calls.operationOrder.lastIndexOf(`archive:${local.id}`);
    expect(calls.operationOrder.lastIndexOf(`draft:${local.id}`)).toBeLessThan(archiveIndex);
    expect(calls.operationOrder.lastIndexOf(`ui:${local.id}`)).toBeLessThan(archiveIndex);
    expect(calls.drafts.at(-1)).toMatchObject({
      workspaceId: local.id,
      body: 'Keep this unsaved thought across archival'
    });
    expect(document.querySelector(`[aria-label="Delete workspace Short-lived review"]`)).toBeNull();

    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'History')?.click();
    await settle(5);
    expect(document.querySelector('.archived-workspaces')?.textContent).toContain('Short-lived review');
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Reopen snapshot')?.click();
    await settle(8);
    expect(document.querySelector('.workspace-card.selected')?.textContent).toContain('Short-lived review');
  });

  it('requires the exact workspace name before permanently deleting all local history', async () => {
    const doomed = workspace('workspace-doomed', 'Disposable review');
    const survivor = workspace('workspace-survivor', 'Keep me');
    const calls = installApi([doomed, survivor], { ...settings, lastWorkspaceId: doomed.id });
    components.push(mount(App, { target: target() }));
    await settle(8);

    document.querySelector<HTMLButtonElement>('[aria-label="Delete workspace Disposable review"]')?.click();
    await settle();
    const dialog = document.querySelector('[aria-labelledby="delete-workspace-title"]');
    expect(dialog?.textContent).toContain('This cannot be undone');
    expect(dialog?.textContent).toContain('erased from LocalReview and its backups');
    const confirmButton = [...dialog!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Delete permanently')!;
    expect(confirmButton.disabled).toBe(true);

    const confirmation = dialog!.querySelector<HTMLInputElement>('[aria-label="Workspace name confirmation"]')!;
    confirmation.value = 'wrong';
    confirmation.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    expect(confirmButton.disabled).toBe(true);
    confirmation.value = doomed.name;
    confirmation.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    expect(confirmButton.disabled).toBe(false);
    confirmButton.click();
    await settle(10);

    expect(calls.workspaceDeletes).toEqual([doomed.id]);
    expect(calls.workspaceArchives).toEqual([]);
    expect(document.querySelector('[aria-label="Delete workspace Disposable review"]')).toBeNull();
    expect(document.querySelector('.workspace-card.selected')?.textContent).toContain('Keep me');

    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'History')?.click();
    await settle(5);
    expect(document.querySelector('.archived-workspaces')?.textContent ?? '').not.toContain('Disposable review');
  });

  it('resumes initial setup after restart without reading a missing review session', async () => {
    const pending = workspace('workspace-localreview', 'Needs setup', false);
    const calls = installApi([pending]);
    components.push(mount(App, { target: target() }));
    await settle(8);

    expect(document.querySelector('[aria-labelledby="baseline-title"]')).not.toBeNull();
    expect(document.querySelector('#baseline-title')?.textContent).toBe('Start review');
    expect(document.querySelector<HTMLDetailsElement>('.advanced-setup')?.open).toBe(false);
    expect(document.querySelector<HTMLInputElement>('[aria-label="Comparison branch for ."]')?.value).toBe('origin/main');
    expect(document.querySelector('.setup-notice')?.textContent).toContain('staged origin/main as the detected repair');
    expect([...document.querySelectorAll<HTMLButtonElement>('button')].some((button) => button.textContent === 'Start review')).toBe(true);
    expect(document.querySelector('.file-picker')?.textContent).toContain('Initial review setup');
    expect(document.body.textContent).not.toContain('Loading diff');
    expect(calls.setupReads).toEqual([pending.id]);
    expect(calls.sessionReads).toEqual([]);
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('.finish-button')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Copy review content"]')?.disabled).toBe(true);
    expect([...document.querySelectorAll<HTMLButtonElement>('.actions-menu [role="menuitem"]')]
      .find((button) => button.textContent === 'Start new review')?.disabled).toBe(true);

    document.querySelector<HTMLButtonElement>('[aria-label="Close review setup"]')?.click();
    await settle();
    expect(document.querySelector('[aria-labelledby="baseline-title"]')).toBeNull();
    expect(document.querySelector('.file-picker')?.textContent).toContain('Initial review setup');
    expect([...document.querySelectorAll<HTMLButtonElement>('button')].some((button) => button.textContent === 'Set up review')).toBe(true);
  });

  it('starts an initial review from the compact setup using the detected repository base', async () => {
    const pending = workspace('workspace-localreview', 'Needs setup', false);
    const calls = installApi([pending]);
    components.push(mount(App, { target: target() }));
    await settle(5);

    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Start review')?.click();
    await settle(8);

    expect(calls.baselineWrites).toEqual([{
      workspaceId: pending.id,
      defaultBase: 'origin/master',
      repositoryBases: [{ repositoryId: 'repo-localreview', base: 'origin/main' }]
    }]);
    expect(calls.startRequests).toEqual([{
      workspaceId: pending.id,
      request: { comparisonOptions: { ignoreAllWhitespace: false, ignoreSpaceAtEol: false, ignoreCrAtEol: false } }
    }]);
    expect(document.querySelector('[aria-labelledby="baseline-title"]')).toBeNull();
    expect(document.querySelector('.file-picker')?.textContent).toContain('localreview.ts');
  });

  it('stages repository inclusion until Save changes and discards it on Close', async () => {
    const active = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([active]);
    components.push(mount(App, { target: target() }));
    await settle(5);

    const openSetup = async () => {
      const actions = document.querySelector<HTMLDetailsElement>('.actions-menu')!;
      actions.open = true;
      [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
        .find((button) => button.textContent === 'Review setup')?.click();
      await settle(5);
    };
    await openSetup();
    const inclusion = document.querySelector<HTMLInputElement>('[aria-label="Review ."]')!;
    expect(inclusion.checked).toBe(true);
    inclusion.click();
    await settle();
    expect(inclusion.checked).toBe(false);
    expect(document.querySelector('.simple-section-heading span')?.textContent).toBe('0 of 1 included');
    expect(calls.inclusionWrites).toEqual([]);

    [...document.querySelectorAll<HTMLButtonElement>('[aria-labelledby="baseline-title"] button')]
      .find((button) => button.textContent === 'Close')?.click();
    await settle();
    expect(document.querySelector('.statusbar')?.textContent).toContain('Discarded unsaved review setup changes.');
    await openSetup();

    expect(document.querySelector<HTMLInputElement>('[aria-label="Review ."]')?.checked).toBe(true);
    expect(calls.inclusionWrites).toEqual([]);
  });

  it('stages a detected default-branch repair when an active review opens setup', async () => {
    const active = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([active]);
    const readSetup = harness.state.implementation.getRepositorySetup;
    harness.state.implementation.getRepositorySetup = async (workspaceId: string) => {
      const rows = await readSetup(workspaceId);
      return rows.map((repository: Record<string, unknown>) => ({
        ...repository,
        effectiveBase: 'origin/main',
        suggestedBase: 'origin/dev',
        resolvedBaseSha: undefined,
        comparisonError: 'origin/main does not resolve to a commit'
      }));
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    const actions = document.querySelector<HTMLDetailsElement>('.actions-menu')!;
    actions.open = true;
    [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
      .find((button) => button.textContent === 'Review setup')?.click();
    await settle(6);

    expect(document.querySelector<HTMLInputElement>('[aria-label="Comparison branch for ."]')?.value).toBe('origin/dev');
    expect(document.querySelector('.setup-notice')?.textContent).toContain('staged origin/dev as the detected repair');
    expect(calls.baselineWrites).toEqual([]);
  });

  it('persists layout choices for a valid zero-file review without an empty active file id', async () => {
    const empty = workspace('workspace-empty', 'Empty review');
    const calls = installApi([empty]);
    components.push(mount(App, { target: target() }));
    await settle(5);

    expect(document.querySelector('#no-changes-heading')?.textContent).toBe('No changes to review');
    expect(document.querySelector('.file-picker')?.textContent).toContain('No changed files');
    expect(document.body.textContent).not.toContain('Loading diff');
    expect(document.querySelector('[aria-label="File navigation"] .navigation-label')?.textContent).toBe('File');
    expect(document.querySelector('[aria-label="Hunk navigation"] .navigation-label')?.textContent).toBe('Hunk');
    expect([...document.querySelectorAll<HTMLButtonElement>('[aria-label="File navigation"] button')].every((button) => button.disabled)).toBe(true);
    expect([...document.querySelectorAll<HTMLButtonElement>('[aria-label="Hunk navigation"] button')].every((button) => button.disabled)).toBe(true);

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Split')?.click();
    await settle(5);

    expect(calls.uiStates.length).toBeGreaterThan(0);
    expect(calls.uiStates.at(-1)?.state).toMatchObject({ mode: 'split' });
    expect(calls.uiStates.at(-1)?.state).not.toHaveProperty('activeFileId');
  });

  it('opens a directly selected Full File presentation at line one and labels its complete extent', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const loadReview = harness.state.implementation.loadReview;
    harness.state.implementation.loadReview = async (workspaceId: string) => {
      const loaded = await loadReview(workspaceId) as ReviewData;
      loaded.files[0].deletions = 3;
      loaded.files[0].additions = 2;
      return loaded;
    };
    const getPresentationWindow = harness.state.implementation.getPresentationWindow;
    harness.state.implementation.getPresentationWindow = async (request: Record<string, unknown>) => {
      const result = await getPresentationWindow(request);
      if (request.mode !== 'full') return result;
      return request.fullFileSide === 'old'
        ? { ...result, totalRows: result.totalRows + 1, rows: [{ id: 'added-gate', kind: 'addition_gate', newLine: 2, omittedEndLine: 3, omittedSide: 'new', omittedBlockId: 'added-1', omittedCount: 2 }], omittedBlocks: [{ id: 'added-1', side: 'new', startLine: 2, endLine: 3, count: 2, expanded: false, rowIndex: 1 }] }
        : { ...result, totalRows: result.totalRows + 1, rows: [{ id: 'removed-gate', kind: 'deletion_gate', oldLine: 2, omittedEndLine: 4, omittedSide: 'old', omittedBlockId: 'removed-1', omittedCount: 3 }], omittedBlocks: [{ id: 'removed-1', side: 'old', startLine: 2, endLine: 4, count: 3, expanded: false, rowIndex: 1 }] };
    };
    const readUiState = harness.state.implementation.getWorkspaceUiState;
    harness.state.implementation.getWorkspaceUiState = async (workspaceId: string) => ({
      ...(await readUiState(workspaceId)), fullFileSide: 'new', nearestSourceLine: 80, nearestSourceSide: 'new', scrollTop: 1800
    });
    components.push(mount(App, { target: target() }));
    await settle(5);

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Full File')?.click();
    await settle(6);

    expect(calls.presentationRequests.at(-1)).toMatchObject({ mode: 'full', startRow: 0 });
    expect(calls.presentationRequests.at(-1)?.endRow).toBeGreaterThan(0);
    expect(document.querySelector('.full-file-extent')?.textContent).toContain('Entire current file · 1 line');
    expect(document.querySelector('.full-file-extent')?.textContent).toContain('3 removed lines in 1 block');
    expect(calls.uiStates.some((call) => call.state.mode === 'full' && call.state.scrollTop === 0)).toBe(true);

    document.querySelector<HTMLButtonElement>('[aria-label="Full-file source side"] button:last-child')?.click();
    await settle(5);

    expect(calls.presentationRequests.some((request) => request.mode === 'full' && request.fullFileSide === 'old')).toBe(true);
    expect(calls.locationRequests.at(-1)).toMatchObject({ mode: 'full', side: 'old', line: 3 });
    expect(document.querySelector('.full-file-extent')?.textContent).toContain('2 added lines in 1 block');
  });

  it('defaults Full File to Both and exposes independent addition and deletion disclosure controls', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const getPresentationWindow = harness.state.implementation.getPresentationWindow;
    let heldDisclosure: ReturnType<typeof deferred<void>> | undefined;
    harness.state.implementation.getPresentationWindow = async (request: ViewportRequest) => {
      if (heldDisclosure && request.ephemeralCollapsedFullFileAdditionBlocks?.includes('added-1')) {
        await heldDisclosure.promise;
      }
      const result = await getPresentationWindow(request);
      if (request.mode !== 'full') return result;
      const deletionExpanded = (request.ephemeralExpandedFullFileDeletionBlocks ?? []).includes('removed-1');
      const additionExpanded = !(request.ephemeralCollapsedFullFileAdditionBlocks ?? []).includes('added-1');
      const rows = [
        { id: 'removed-gate', kind: 'deletion_gate', oldLine: 4, omittedEndLine: 5, omittedSide: 'old', omittedBlockId: 'removed-1', omittedCount: 2, omittedExpanded: deletionExpanded },
        ...(deletionExpanded ? [
          { id: 'removed-4', kind: 'deletion', oldLine: 4, oldText: 'const previous = false;' },
          { id: 'removed-5', kind: 'deletion', oldLine: 5, oldText: 'return previous;' }
        ] : []),
        { id: 'added-gate', kind: 'addition_gate', newLine: 4, omittedEndLine: 6, omittedSide: 'new', omittedBlockId: 'added-1', omittedCount: 3, omittedExpanded: additionExpanded },
        ...(additionExpanded ? [
          { id: 'added-4', kind: 'addition', newLine: 4, newText: 'const replacement = true;' }
        ] : [])
      ];
      return {
        ...result,
        totalRows: rows.length,
        rows,
        omittedBlocks: [
          { id: 'removed-1', side: 'old', startLine: 4, endLine: 5, count: 2, expanded: deletionExpanded, rowIndex: 0 },
          { id: 'added-1', side: 'new', startLine: 4, endLine: 6, count: 3, expanded: additionExpanded, rowIndex: deletionExpanded ? 3 : 1 }
        ]
      };
    };
    components.push(mount(App, { target: target() }));
    await settle(5);

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Full File')?.click();
    await settle(6);

    expect(calls.presentationRequests.at(-1)).toMatchObject({ mode: 'full', fullFileSide: 'both' });
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Full-file source side"] button[aria-pressed="true"]')?.textContent).toBe('Both');
    const additionControls = document.querySelector('[aria-label="Full-file addition blocks"]');
    const deletionControls = document.querySelector('[aria-label="Full-file deletion blocks"]');
    expect(additionControls?.textContent).toContain('Show all additions');
    expect(additionControls?.textContent).toContain('Hide all additions');
    expect(deletionControls?.textContent).toContain('Show all deletions');
    expect(deletionControls?.textContent).toContain('Hide all deletions');

    const savesBeforeDisclosure = calls.uiStates.length;
    const requestsBeforeDisclosure = calls.presentationRequests.length;
    heldDisclosure = deferred<void>();
    [...additionControls!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Hide all additions')?.click();
    await settle(2);
    // The chevron acknowledges the click before native projection work or
    // the debounced durable save completes.
    expect(document.querySelector<HTMLButtonElement>('.addition-gate-toggle')?.ariaExpanded).toBe('false');
    expect(calls.uiStates).toHaveLength(savesBeforeDisclosure);
    // A newer click while the projection request is still in flight remains
    // immediately interactive and becomes the sole trailing native request.
    [...additionControls!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Show all additions')?.click();
    await settle(2);
    expect(document.querySelector<HTMLButtonElement>('.addition-gate-toggle')?.ariaExpanded).toBe('true');
    heldDisclosure.resolve();
    heldDisclosure = undefined;
    await settle(8);
    const coalescedRequests = calls.presentationRequests.slice(requestsBeforeDisclosure);
    expect(coalescedRequests).toHaveLength(2);
    expect(coalescedRequests[0]?.ephemeralCollapsedFullFileAdditionBlocks).toEqual(['added-1']);
    expect(coalescedRequests[1]?.ephemeralCollapsedFullFileAdditionBlocks).toEqual([]);
    expect(document.querySelector<HTMLButtonElement>('.addition-gate-toggle')?.ariaExpanded).toBe('true');
    expect(calls.uiStates).toHaveLength(savesBeforeDisclosure);

    [...additionControls!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Hide all additions')?.click();
    await settle(4);
    expect(calls.presentationRequests.at(-1)?.ephemeralCollapsedFullFileAdditionBlocks).toEqual(['added-1']);

    [...deletionControls!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Show all deletions')?.click();
    await settle(4);
    expect(calls.presentationRequests.at(-1)?.ephemeralExpandedFullFileDeletionBlocks).toEqual(['removed-1']);
    expect(calls.presentationRequests.at(-1)?.ephemeralCollapsedFullFileAdditionBlocks).toEqual(['added-1']);

    // A bulk show followed by an individual collapse must use the current
    // in-memory state, even though the durable write remains debounced.
    [...additionControls!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Show all additions')?.click();
    await settle(4);
    expect(document.querySelector<HTMLButtonElement>('.addition-gate-toggle')?.ariaExpanded).toBe('true');
    document.querySelector<HTMLButtonElement>('.addition-gate-toggle')?.click();
    await settle(4);
    expect(calls.presentationRequests.at(-1)?.ephemeralCollapsedFullFileAdditionBlocks).toEqual(['added-1']);
    expect(document.querySelector<HTMLButtonElement>('.addition-gate-toggle')?.ariaExpanded).toBe('false');

    await new Promise((resolve) => window.setTimeout(resolve, 140));
    await settle();
    expect(calls.uiStates.at(-1)?.state.expandedFullFileDeletionBlocks).toContain('removed-1');
    expect(calls.uiStates.at(-1)?.state.collapsedFullFileAdditionBlocks).toContain('added-1');
  });

  it('shows a deleted file only as a clearly labelled base snapshot', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const loadReview = harness.state.implementation.loadReview;
    harness.state.implementation.loadReview = async (workspaceId: string) => {
      const loaded = await loadReview(workspaceId) as ReviewData;
      loaded.files[0] = { ...loaded.files[0], status: 'deleted', additions: 0, deletions: 1 };
      return loaded;
    };
    components.push(mount(App, { target: target() }));
    await waitForUi(
      () => calls.presentationRequests.some((request) => request.fileId === 'file-localreview'),
      'the initial hunk presentation'
    );

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Full File')?.click();
    await settle(6);

    expect(calls.presentationRequests.at(-1)).toMatchObject({ mode: 'full', fullFileSide: 'old' });
    expect(document.querySelector('.full-file-extent')?.textContent).toContain('Entire base file · 1 line');
    expect(document.querySelector('.full-file-extent')?.textContent).toContain('file deleted; Current has no content');
    expect(document.querySelector('[aria-label="Full-file source side"]')).toBeNull();
    expect(document.querySelector('.deleted-banner')?.textContent).toContain('Showing the complete Base snapshot; Current has no content');
  });

  it('keeps File and Hunk navigation explicit, lands exactly on Full File hunks, and wraps at both boundaries', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local], {
      ...settings,
      // Reserved navigation chords remain authoritative even if stale or
      // imported settings attempt to assign them to hunk navigation.
      shortcuts: { nextHunk: 'Meta+]', previousHunk: 'Meta+[' }
    });
    const loadReview = harness.state.implementation.loadReview;
    harness.state.implementation.loadReview = async (workspaceId: string) => {
      const loaded = await loadReview(workspaceId) as ReviewData;
      loaded.files[0].hunkCount = 2;
      loaded.files.push({ ...loaded.files[0], id: 'file-other', path: 'other.ts', hunkCount: 0 });
      return loaded;
    };
    const getPresentationWindow = harness.state.implementation.getPresentationWindow;
    harness.state.implementation.getPresentationWindow = async (request: Record<string, unknown>) => ({
      ...(await getPresentationWindow(request)),
      startRow: request.startRow ?? 0,
      totalRows: 1_000,
      hunks: [
        { id: 'hunk-first', rowIndex: 100, oldLine: 91, newLine: 101, header: '@@ -91 +101 @@' },
        { id: 'hunk-second', rowIndex: 700, oldLine: 651, newLine: 701, header: '@@ -651 +701 @@' }
      ]
    });
    components.push(mount(App, { target: target() }));
    await settle(5);

    expect(document.querySelector('[aria-label="File navigation"] .navigation-label')?.textContent).toBe('File');
    expect(document.querySelector('[aria-label="Hunk navigation"] .navigation-label')?.textContent).toBe('Hunk');
    expect(document.querySelector<HTMLButtonElement>('.topbar-actions [aria-label="Previous file"]')?.title).toBe('Previous file (⌘[)');
    expect(document.querySelector<HTMLButtonElement>('.topbar-actions [aria-label="Next file"]')?.title).toBe('Next file (⌘])');
    expect(document.querySelector<HTMLButtonElement>('.topbar-actions [aria-label="Previous hunk"]')?.title).toBe('Previous hunk across files (⌥↑)');
    expect(document.querySelector<HTMLButtonElement>('.topbar-actions [aria-label="Next hunk"]')?.title).toBe('Next hunk across files (⌥↓)');
    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Full File')?.click();
    await settle(6);
    expect(document.querySelector('[aria-label="File navigation"]')).not.toBeNull();
    expect(document.querySelector('[aria-label="Hunk navigation"]')).not.toBeNull();
    calls.presentationRequests.length = 0;

    const clickNavigation = async (label: 'Previous' | 'Next') => {
      document.querySelector<HTMLButtonElement>(`.topbar-actions [aria-label="${label} hunk"]`)?.click();
      await settle(5);
    };
    const viewport = document.querySelector<HTMLElement>('.diff-viewport')!;
    const expectLastWindowToContain = (row: number) => {
      const request = calls.presentationRequests.at(-1)!;
      expect(request.mode).toBe('full');
      expect(request.startRow ?? 0).toBeLessThanOrEqual(row);
      expect(request.endRow ?? 0).toBeGreaterThan(row);
    };

    await clickNavigation('Next');
    expectLastWindowToContain(100);
    expect(viewport.scrollTop).toBe(2_000);
    expect(document.querySelector<HTMLButtonElement>('.mode-picker [role="tab"][aria-selected="true"]')?.textContent).toBe('Full File');

    await clickNavigation('Next');
    expectLastWindowToContain(700);
    expect(viewport.scrollTop).toBe(14_000);

    await clickNavigation('Next');
    expectLastWindowToContain(100);
    expect(viewport.scrollTop).toBe(2_000);

    await clickNavigation('Previous');
    expectLastWindowToContain(700);
    expect(viewport.scrollTop).toBe(14_000);

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', altKey: true, bubbles: true }));
    await settle(5);
    expectLastWindowToContain(100);
    expect(viewport.scrollTop).toBe(2_000);
    expect(document.querySelector<HTMLButtonElement>('.mode-picker [role="tab"][aria-selected="true"]')?.textContent).toBe('Full File');

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', altKey: true, bubbles: true }));
    await settle(5);
    expectLastWindowToContain(700);
    expect(viewport.scrollTop).toBe(14_000);
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.some((call) => call.state.mode === 'full' && call.state.fullFileSide === 'both' && call.state.nearestSourceLine === 651 && call.state.nearestSourceSide === 'old' && call.state.scrollTop === 14_000)).toBe(true);

    document.querySelector<HTMLButtonElement>('[aria-label="Full-file source side"] button:last-child')?.click();
    await settle(5);
    calls.presentationRequests.length = 0;
    await clickNavigation('Next');
    expectLastWindowToContain(100);
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.some((call) => call.state.mode === 'full' && call.state.fullFileSide === 'old' && call.state.nearestSourceLine === 91 && call.state.nearestSourceSide === 'old' && call.state.scrollTop === 2_000)).toBe(true);

    document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.click();
    await settle(6);
    expect(document.querySelector('.file-picker')?.textContent).toContain('other.ts');
    expect(document.querySelector<HTMLButtonElement>('.mode-picker [role="tab"][aria-selected="true"]')?.textContent).toBe('Full File');

    document.querySelector<HTMLButtonElement>('[aria-label="Previous file"]')?.click();
    await settle(6);
    expect(document.querySelector('.file-picker')?.textContent).toContain('localreview.ts');

    document.dispatchEvent(new KeyboardEvent('keydown', { key: ']', metaKey: true, bubbles: true }));
    await settle(6);
    expect(document.querySelector('.file-picker')?.textContent).toContain('other.ts');

    document.dispatchEvent(new KeyboardEvent('keydown', { key: '[', metaKey: true, bubbles: true }));
    await settle(6);
    expect(document.querySelector('.file-picker')?.textContent).toContain('localreview.ts');

    const fileFilter = document.querySelector<HTMLInputElement>('[aria-label="Filter files"]')!;
    fileFilter.value = 'other';
    fileFilter.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.disabled).toBe(false);
    document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.click();
    await settle(6);
    expect(document.querySelector('.file-picker')?.textContent).toContain('other.ts');
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Previous file"]')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.disabled).toBe(true);
  });

  it('navigates hunks across shown files, skips no-hunk files, wraps, filters, and serializes rapid presses', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const loadReview = harness.state.implementation.loadReview;
    harness.state.implementation.loadReview = async (workspaceId: string) => {
      const loaded = await loadReview(workspaceId) as ReviewData;
      loaded.files[0].hunkCount = 2;
      loaded.files.push(
        { ...loaded.files[0], id: 'file-zero', path: 'middle-no-hunks.ts', hunkCount: 0 },
        { ...loaded.files[0], id: 'file-other', path: 'other.ts', hunkCount: 2 }
      );
      return loaded;
    };
    const getPresentationWindow = harness.state.implementation.getPresentationWindow;
    harness.state.implementation.getPresentationWindow = async (request: Record<string, any>) => ({
      ...(await getPresentationWindow(request)),
      startRow: request.startRow,
      totalRows: 1_000,
      rows: Array.from(
        { length: Math.max(0, request.endRow - request.startRow) },
        (_, index) => {
          const line = request.startRow + index + 1;
          return { id: `${request.fileId}:${line}`, kind: 'context', oldLine: line, newLine: line, oldText: `line ${line}`, newText: `line ${line}` };
        }
      ),
      hunks: request.fileId === 'file-zero' ? [] : request.fileId === 'file-other' ? [
        { id: 'other-first', rowIndex: 50, oldLine: 41, newLine: 51, header: '@@ -41 +51 @@' },
        { id: 'other-last', rowIndex: 300, oldLine: 281, newLine: 301, header: '@@ -281 +301 @@' }
      ] : [
        { id: 'local-first', rowIndex: 100, oldLine: 91, newLine: 101, header: '@@ -91 +101 @@' },
        { id: 'local-last', rowIndex: 700, oldLine: 651, newLine: 701, header: '@@ -651 +701 @@' }
      ]
    });
    components.push(mount(App, { target: target() }));
    await waitForUi(
      () => document.querySelector('.statusbar')?.textContent?.includes('Opened LocalReview.') === true,
      'cross-file hunk workspace open completion'
    );

    const next = () => document.querySelector<HTMLButtonElement>('[aria-label="Next hunk"]')?.click();
    const previous = () => document.querySelector<HTMLButtonElement>('[aria-label="Previous hunk"]')?.click();
    const activePath = () => document.querySelector('.file-picker')?.textContent ?? '';
    calls.presentationRequests.length = 0;

    // Issue the first traversal as real rapid button presses. The navigation
    // queue must serialize local first → local last → the next file without
    // depending on jsdom's synthetic viewport-location callbacks.
    next();
    next();
    next();
    await waitForUi(() => activePath().includes('other.ts'), 'cross-file forward hunk navigation', 160);
    expect(activePath()).toContain('other.ts');
    expect(calls.presentationRequests.some((request) => request.fileId === 'file-zero')).toBe(false);

    previous();
    await waitForUi(() => activePath().includes('localreview.ts'), 'cross-file reverse hunk navigation', 160);
    expect(activePath()).toContain('localreview.ts');
    previous(); await settle(5); // local first
    previous();
    await waitForUi(() => activePath().includes('other.ts'), 'wrapped reverse hunk navigation', 160);
    expect(activePath()).toContain('other.ts');
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.some((call) => call.state.activeFileId === 'file-other' && call.state.nearestSourceLine === 301)).toBe(true);

    const filter = document.querySelector<HTMLInputElement>('[aria-label="Filter files"]')!;
    filter.value = 'localreview';
    filter.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    next();
    await waitForUi(() => activePath().includes('localreview.ts'), 'filtered cross-file hunk navigation', 160);
    expect(activePath()).toContain('localreview.ts');

    // Two presses issued in one turn advance from local first to local last,
    // then wrap back to local first instead of racing from the same snapshot.
    next();
    next();
    await settle(12);
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.at(-1)?.state).toMatchObject({ activeFileId: 'file-localreview', nearestSourceLine: 101, nearestSourceSide: 'new' });
  });

  it('disables navigation groups when the current filter has no alternate file and the file has no hunks', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    installApi([local]);
    components.push(mount(App, { target: target() }));
    await waitForUi(
      () => document.querySelector('.statusbar')?.textContent?.includes('Opened LocalReview.') === true,
      'workspace open completion'
    );

    expect(document.querySelector<HTMLButtonElement>('[aria-label="Previous file"]')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Previous hunk"]')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Next hunk"]')?.disabled).toBe(true);
  });

  it('keeps the current snapshot and navigation responsive, then promotes a prepared refresh atomically', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const initial = review(local);
    initial.files[0].additions = 1;
    initial.files.push({ ...initial.files[0], id: 'file-other', path: 'other.ts', additions: 2 });
    initial.workspace.progress = { viewed: 0, total: 2 };
    harness.state.implementation.loadReview = async () => structuredClone(initial);
    const capture = deferred<ReviewData>();
    let refreshCalls = 0;
    harness.state.implementation.startNewReview = () => {
      refreshCalls += 1;
      return capture.promise;
    };
    const normalPresentation = harness.state.implementation.getPresentationWindow;
    let preparedRequest: Record<string, any> | undefined;
    const preparedPresentation = deferred<any>();
    harness.state.implementation.getPresentationWindow = async (request: Record<string, any>) => {
      if (String(request.fileId).startsWith('next-')) {
        preparedRequest = request;
        return preparedPresentation.promise;
      }
      const result = await normalPresentation(request);
      return { ...result, rows: [{ ...result.rows[0], oldText: `old:${request.fileId}`, newText: `old:${request.fileId}` }] };
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    const refreshButton = document.querySelector<HTMLButtonElement>('.status-button')!;
    refreshButton.click();
    expect(refreshButton.textContent).toContain('Capturing…');
    expect(refreshButton.getAttribute('aria-busy')).toBe('true');
    expect(refreshButton.disabled).toBe(true);
    expect(document.querySelector('.workspace-card.selected .refresh-dot')?.textContent).toContain('Capturing…');
    refreshButton.click();
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    await settle(3);

    expect(refreshCalls).toBe(1);
    expect(refreshButton.disabled).toBe(true);
    expect(refreshButton.textContent).toContain('Capturing…');
    expect(document.querySelector('.refresh-status strong')?.textContent).toContain('Archiving this round');
    expect(document.querySelector('[role="progressbar"]')?.getAttribute('aria-valuetext')).toContain('Archiving this round');
    expect(document.querySelector('main')?.getAttribute('aria-busy')).not.toBe('true');
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.disabled).toBe(false);
    expect(document.querySelector<HTMLButtonElement>('.bulk-view-button')?.disabled).toBe(true);

    // File and panel navigation are unrelated to capture and remain live.
    document.querySelector<HTMLButtonElement>('[aria-label="Next file"]')?.click();
    await settle(6);
    expect(document.querySelector('.file-picker')?.textContent).toContain('other.ts');
    [...document.querySelectorAll<HTMLButtonElement>('.panel-tabs [role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.click();
    await settle();
    expect(document.querySelector<HTMLButtonElement>('.panel-tabs [aria-selected="true"]')?.textContent).toContain('Comments');
    [...document.querySelectorAll<HTMLButtonElement>('.panel-tabs [role="tab"]')]
      .find((button) => button.textContent === 'Files')?.click();
    await settle();

    const next = structuredClone(initial);
    next.files = next.files.map((file, index) => ({
      ...file,
      id: `next-${index}`,
      additions: index === 0 ? 11 : 22,
      comparisonId: `comparison-${index}`
    }));
    next.workspace.progress = { viewed: 1, total: 2 };
    capture.resolve(next);
    await settle(5);

    expect(preparedRequest?.fileId).toBe('next-1');
    expect(refreshButton.textContent).toContain('Preparing view…');
    expect(document.querySelector('.refresh-status strong')?.textContent).toContain('Preparing the captured diff');
    // The refreshed file list/counter is not exposed ahead of its diff.
    expect(document.querySelector('.file-picker')?.textContent).toContain('other.ts');
    expect(document.querySelector('.diff-stats .additions')?.textContent).toBe('+2');
    expect(document.querySelector('.review-progress strong')?.textContent).toBe('0/2');
    expect(document.querySelector('.diff-viewport')?.textContent).toContain('old:file-other');
    expect(document.querySelector('.diff-viewport')?.textContent).not.toBe('');

    preparedPresentation.resolve({
      fileId: preparedRequest!.fileId,
      mode: preparedRequest!.mode,
      generation: preparedRequest!.generation,
      startRow: 0,
      totalRows: 1,
      rows: [{ id: 'new-row', kind: 'context', oldLine: 1, newLine: 1, oldText: 'new snapshot', newText: 'new snapshot' }],
      hunks: [], oldTokens: [], newTokens: []
    });
    await settle(8);
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);

    expect(document.querySelector('.refresh-status')).toBeNull();
    expect(refreshButton.textContent).toContain('Updated');
    expect(refreshButton.getAttribute('aria-label')).toContain('2 files');
    expect(refreshButton.getAttribute('aria-busy')).toBe('false');
    expect(document.querySelector('.file-picker')?.textContent).toContain('other.ts');
    expect(document.querySelector('.diff-stats .additions')?.textContent).toBe('+22');
    expect(document.querySelector('.review-progress strong')?.textContent).toBe('1/2');
    expect(document.querySelector('.diff-viewport')?.textContent).toContain('new snapshot');
    expect(document.querySelector('.statusbar')?.textContent).toContain('new review round is now displayed');
    expect(calls.uiStates.some((call) => call.state.activeFileId === 'next-1')).toBe(true);
  });

  it('uses Refresh as a durable review-round boundary and exposes the prior feedback in History', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    const calls = installApi([local]);
    const initial = review(local);
    initial.files[0].comparisonId = 'comparison-prior-round';
    initial.annotations = [{
      id: 'feedback-1', fileId: initial.files[0].id, repositoryId: initial.repositories[0].id,
      kind: 'comment', state: 'open', side: 'new', startLine: 1, endLine: 1,
      body: 'Keep this feedback in the prior round.', selectedSource: 'new', labels: [],
      localOnly: false, createdAt: '2026-07-22T00:00:00.000Z'
    }];
    initial.files[0].annotationCount = 1;
    initial.workspace.draftCount = 1;
    harness.state.implementation.loadReview = async () => structuredClone(initial);
    const next = structuredClone(initial);
    next.files[0].comparisonId = 'comparison-next-round';
    next.annotations = [];
    next.files[0].annotationCount = 0;
    next.workspace.draftCount = 0;
    next.history = [{
      id: 'review:prior-round', type: 'review', label: 'Archived review',
      annotationCount: 1, createdAt: '2026-07-22T00:01:00.000Z'
    }];
    harness.state.implementation.startNewReview = async (workspaceId: string, request?: Record<string, unknown>) => {
      calls.startRequests.push({ workspaceId, request: structuredClone(request) });
      return structuredClone(next);
    };
    harness.state.implementation.listArchivedWorkspaces = async () => [];
    harness.state.implementation.getReviewHistory = async () => structuredClone(next.history);
    components.push(mount(App, { target: target() }));
    await waitForUi(
      () => document.querySelector('.annotation-thread-toggle') !== null,
      'the restored inline annotation'
    );

    document.querySelector<HTMLButtonElement>('.annotation-thread-toggle')?.click();
    await settle(3);
    expect(document.querySelector('.inline-thread-popover')?.textContent)
      .toContain('Keep this feedback in the prior round.');

    document.querySelector<HTMLButtonElement>('.status-button')?.click();
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    await settle(8);
    expect(calls.startRequests).toHaveLength(1);
    expect(calls.refreshRequests).toHaveLength(0);
    expect([...document.querySelectorAll<HTMLButtonElement>('.panel-tabs [role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.textContent).toBe('Comments');
    expect(document.querySelector('.inline-thread-popover')).toBeNull();
    expect(document.querySelector('.statusbar')?.textContent).toContain('Archived the prior round');

    const actions = document.querySelector<HTMLDetailsElement>('.actions-menu')!;
    actions.open = true;
    [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
      .find((button) => button.textContent === 'History')?.click();
    await settle(5);
    expect(document.querySelector('.history-list')?.textContent).toContain('Archived review');
    expect(document.querySelector('.history-list')?.textContent).toContain('1 annotations');
  });

  it('hands History off to each exact prompt export without stacking modal backdrops', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    installApi([local]);
    const initial = review(local);
    const exactExports = [
      { id: 'export:feedback', label: 'Review feedback', title: 'Review feedback', content: '# Exact feedback\n\nKeep the feedback exact.' },
      { id: 'export:questions', label: 'Questions for investigation', title: 'Questions for investigation', content: '# Exact questions\n\nWhy does this happen?' },
      { id: 'export:full', label: 'Full review prompt', title: 'Full review prompt', content: '# Exact full review\n\nFeedback and questions.' }
    ];
    initial.history = exactExports.map((entry, index) => ({
      id: entry.id,
      type: 'export' as const,
      label: entry.label,
      annotationCount: index + 1,
      createdAt: `2026-07-22T00:0${index}:00.000Z`
    }));
    harness.state.implementation.loadReview = async () => structuredClone(initial);
    harness.state.implementation.getReviewHistory = async () => structuredClone(initial.history);
    const promptRequests: Array<{ workspaceId: string; historyId?: string }> = [];
    harness.state.implementation.generatePrompt = async (workspaceId: string, request: { historyId?: string }) => {
      promptRequests.push({ workspaceId, historyId: request.historyId });
      const exact = exactExports.find((entry) => entry.id === request.historyId);
      if (!exact) throw new Error('missing exact export');
      return {
        exportId: exact.id.slice('export:'.length),
        title: exact.title,
        content: exact.content,
        annotationCount: 1,
        estimatedTokens: 12
      };
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    for (const exact of exactExports) {
      const actions = document.querySelector<HTMLDetailsElement>('.actions-menu')!;
      actions.open = true;
      [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
        .find((button) => button.textContent === 'History')?.click();
      await settle(5);

      expect(document.querySelector('.history-modal')).not.toBeNull();
      expect(document.querySelectorAll('dialog[open]')).toHaveLength(1);
      const historyEntry = [...document.querySelectorAll<HTMLElement>('.history-list article')]
        .find((article) => article.querySelector('strong')?.textContent === exact.label)!;
      [...historyEntry.querySelectorAll<HTMLButtonElement>('button')]
        .find((button) => button.textContent === 'Open exact export')?.click();
      await settle(5);

      expect(document.querySelector('.history-modal')).toBeNull();
      expect(document.querySelectorAll('dialog[open]')).toHaveLength(1);
      expect(document.querySelector('.prompt-modal #prompt-title')?.textContent).toBe(exact.title);
      expect(document.querySelector('.prompt-modal pre')?.textContent).toBe(exact.content);
      expect(document.querySelector('.prompt-exact-note')?.textContent).toContain('Exact durable export');

      document.querySelector<HTMLButtonElement>('[aria-label="Close prompt preview"]')?.click();
      await settle();
      expect(document.querySelectorAll('dialog[open]')).toHaveLength(0);
    }

    expect(promptRequests).toEqual(exactExports.map((entry) => ({
      workspaceId: local.id,
      historyId: entry.id
    })));
  });

  it('exposes feedback, question, and full prompt exports in the GitHub review flow', async () => {
    const github = {
      ...workspace('workspace-github-prompts', 'GitHub prompt review'),
      source: ['github'] as Workspace['source']
    };
    installApi([github]);
    const promptRequests: Array<{ workspaceId: string; scope: string }> = [];
    harness.state.implementation.generatePrompt = async (
      workspaceId: string,
      request: { scope: string }
    ) => {
      promptRequests.push({ workspaceId, scope: request.scope });
      return {
        exportId: `export-${request.scope}`,
        title: `${request.scope} prompt`,
        content: `# ${request.scope} GitHub PR prompt`,
        annotationCount: 0,
        estimatedTokens: 8
      };
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    const actions = document.querySelector<HTMLDetailsElement>('.actions-menu')!;
    actions.open = true;
    const labels = [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
      .map((button) => button.textContent?.trim());
    expect(labels).toContain('Finish review');
    expect(labels).toContain('Copy feedback prompt');
    expect(labels).toContain('Copy questions prompt');
    expect(labels).toContain('Copy full review prompt');

    for (const [label, scope] of [
      ['Copy feedback prompt', 'feedback'],
      ['Copy questions prompt', 'questions'],
      ['Copy full review prompt', 'all']
    ]) {
      actions.open = true;
      [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
        .find((button) => button.textContent?.trim() === label)?.click();
      await settle(3);

      expect(document.querySelector('.prompt-modal #prompt-title')?.textContent)
        .toBe(`${scope} prompt`);
      expect(document.querySelector('.prompt-modal pre')?.textContent)
        .toBe(`# ${scope} GitHub PR prompt`);
      document.querySelector<HTMLButtonElement>('[aria-label="Close prompt preview"]')?.click();
      await settle();
    }

    expect(promptRequests).toEqual([
      { workspaceId: github.id, scope: 'feedback' },
      { workspaceId: github.id, scope: 'questions' },
      { workspaceId: github.id, scope: 'all' }
    ]);

    [...document.querySelectorAll<HTMLButtonElement>('.panel-tabs [role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.click();
    await settle(2);
    expect([...document.querySelectorAll<HTMLButtonElement>('.comment-actions button')]
      .some((button) => button.textContent === 'Copy questions prompt')).toBe(true);
  });

  it('uses feedback for the primary local prompt and persists concise formatting defaults', async () => {
    const local = workspace('workspace-prompt-defaults', 'Prompt defaults');
    const calls = installApi([local]);
    const promptRequests: Array<{
      scope: string;
      pathStyle?: string;
      includeDiffHunks?: boolean;
      includeGitState?: boolean;
    }> = [];
    harness.state.implementation.generatePrompt = async (
      _workspaceId: string,
      request: {
        scope: string;
        pathStyle?: string;
        includeDiffHunks?: boolean;
        includeGitState?: boolean;
      }
    ) => {
      promptRequests.push(structuredClone(request));
      return {
        exportId: `export-${promptRequests.length}`,
        title: request.scope === 'feedback' ? 'Review feedback' : `${request.scope} prompt`,
        content: '# Prompt preview',
        annotationCount: 1,
        estimatedTokens: 4
      };
    };
    components.push(mount(App, { target: target() }));
    await settle(6);

    document.querySelector<HTMLButtonElement>('.finish-button')?.click();
    await settle(4);
    expect(promptRequests[0]).toMatchObject({
      scope: 'feedback',
      pathStyle: 'absolute',
      includeDiffHunks: false,
      includeGitState: false
    });
    expect(document.querySelector('.prompt-modal #prompt-title')?.textContent).toBe('Review feedback');

    [...document.querySelectorAll<HTMLButtonElement>('[aria-label="Prompt path style"] button')]
      .find((button) => button.textContent === 'Qualified')?.click();
    await settle(5);
    const optionLabels = [...document.querySelectorAll<HTMLLabelElement>('.prompt-formatting label')];
    optionLabels.find((label) => label.textContent?.includes('Diff hunks'))
      ?.querySelector<HTMLInputElement>('input')?.click();
    await settle(5);
    optionLabels.find((label) => label.textContent?.includes('Git state'))
      ?.querySelector<HTMLInputElement>('input')?.click();
    await settle(5);

    expect(promptRequests.at(-1)).toMatchObject({
      scope: 'feedback',
      pathStyle: 'qualified',
      includeDiffHunks: true,
      includeGitState: true
    });
    expect(calls.settingsWrites).toContainEqual({ promptPathStyle: 'qualified' });
    expect(calls.settingsWrites).toContainEqual({ promptIncludeDiffHunks: true });
    expect(calls.settingsWrites).toContainEqual({ promptIncludeGitState: true });
  });

  it('promotes the committed GitHub review round when the provider refresh fails', async () => {
    const github = {
      ...workspace('workspace-github', 'GitHub review'),
      source: ['github'] as Workspace['source'],
      refreshAvailable: true,
      refreshAvailableRevision: 4
    };
    const calls = installApi([github]);
    const initial = review(github);
    initial.files[0] = { ...initial.files[0], comparisonId: 'github-prior', additions: 1 };
    initial.annotations = [{
      id: 'github-feedback', fileId: initial.files[0].id, repositoryId: initial.repositories[0].id,
      kind: 'comment', state: 'open', side: 'new', startLine: 1, endLine: 1,
      body: 'Preserve this in the archived GitHub round.', selectedSource: 'new', labels: [],
      localOnly: false, createdAt: '2026-07-22T00:00:00.000Z'
    }];
    initial.files[0].annotationCount = 1;
    initial.workspace.draftCount = 1;
    harness.state.implementation.loadReview = async () => structuredClone(initial);

    const next = structuredClone(initial);
    next.files[0] = { ...next.files[0], comparisonId: 'github-next', additions: 19, annotationCount: 0 };
    next.annotations = [];
    next.workspace = { ...next.workspace, draftCount: 0 };
    next.history = [{
      id: 'review:github-prior', type: 'review', label: 'Archived GitHub review',
      annotationCount: 1, createdAt: '2026-07-22T00:01:00.000Z'
    }];
    harness.state.implementation.startNewReview = async (workspaceId: string, request?: Record<string, unknown>) => {
      calls.startRequests.push({ workspaceId, request: structuredClone(request) });
      return structuredClone(next);
    };
    harness.state.implementation.refreshReview = async (workspaceId: string, request?: Record<string, unknown>) => {
      calls.refreshRequests.push({ workspaceId, request: structuredClone(request) });
      throw new Error('provider temporarily unavailable');
    };
    harness.state.implementation.getReviewHistory = async () => structuredClone(next.history);
    components.push(mount(App, { target: target() }));
    await settle(6);

    document.querySelector<HTMLButtonElement>('.status-button')?.click();
    await settle(12);

    expect(calls.startRequests).toHaveLength(1);
    expect(calls.refreshRequests).toHaveLength(1);
    expect(document.querySelector('.diff-stats .additions')?.textContent).toBe('+19');
    expect([...document.querySelectorAll<HTMLButtonElement>('.panel-tabs [role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.textContent).toBe('Comments');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent)
      .toContain('Refresh incomplete');
    expect(document.querySelector('.statusbar')?.textContent)
      .toContain('new review round is displayed at the previous pinned GitHub revisions');
    expect(document.querySelector('.statusbar')?.textContent).toContain('provider temporarily unavailable');

    const actions = document.querySelector<HTMLDetailsElement>('.actions-menu')!;
    actions.open = true;
    [...actions.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
      .find((button) => button.textContent === 'History')?.click();
    await settle(5);
    expect(document.querySelector('.history-list')?.textContent).toContain('Archived GitHub review');
    expect(document.querySelector('.history-list')?.textContent).toContain('1 annotations');
  });

  it('keeps the old file list and diff intact when refreshed presentation staging fails', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    installApi([local]);
    const initial = review(local);
    harness.state.implementation.loadReview = async () => structuredClone(initial);
    const next = structuredClone(initial);
    next.files[0] = { ...next.files[0], id: 'next-file', path: 'renamed.ts', previousPath: initial.files[0].path, additions: 99, comparisonId: 'next-comparison' };
    harness.state.implementation.startNewReview = async () => structuredClone(next);
    const normalPresentation = harness.state.implementation.getPresentationWindow;
    harness.state.implementation.getPresentationWindow = (request: Record<string, any>) => request.fileId === 'next-file'
      ? Promise.reject(new Error('presentation unavailable'))
      : normalPresentation(request);
    components.push(mount(App, { target: target() }));
    await settle(6);

    document.querySelector<HTMLButtonElement>('.status-button')?.click();
    await settle(8);

    expect(document.querySelector('.refresh-status')).toBeNull();
    expect(document.querySelector('.file-picker')?.textContent).toContain('localreview.ts');
    expect(document.querySelector('.file-picker')?.textContent).not.toContain('renamed.ts');
    expect(document.querySelector('.diff-stats .additions')?.textContent).toBe('+1');
    expect(document.querySelector('.diff-viewport')?.textContent).not.toBe('');
    expect(document.querySelector('.statusbar')?.textContent).toContain('previous snapshot remains displayed or is available in History: presentation unavailable');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.disabled).toBe(false);
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent).toContain('Refresh failed');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.getAttribute('aria-label')).toContain('Retry refresh');
  });

  it('reports an all-repository native capture failure without claiming the review updated', async () => {
    vi.useFakeTimers();
    const local = { ...workspace('workspace-localreview', 'LocalReview'), refreshAvailable: true, refreshAvailableRevision: 2 };
    const calls = installApi([local]);
    const readSetup = harness.state.implementation.getRepositorySetup;
    harness.state.implementation.getRepositorySetup = async (workspaceId: string) => {
      const rows = await readSetup(workspaceId);
      return rows.map((repository: Record<string, unknown>) => ({
        ...repository,
        effectiveBase: 'origin/main',
        suggestedBase: 'origin/dev',
        comparisonError: 'origin/main does not resolve to a commit',
        issues: [{
          id: 'missing-base:repo-localreview',
          kind: 'missing_base_reference',
          severity: 'error',
          title: 'Comparison branch is unavailable',
          message: 'origin/main does not resolve; origin/dev is the detected remote default.',
          dismissible: true,
          actions: [
            { kind: 'apply_suggested_base', label: 'Use origin/dev', value: 'origin/dev' },
            { kind: 'open_review_setup', label: 'Open Review setup' }
          ]
        }]
      }));
    };
    const failed = review({ ...local, refreshAvailable: true, refreshAvailableRevision: 3 });
    failed.refreshOutcome = {
      status: 'failed',
      capturedRepositoryCount: 0,
      failedRepositoryCount: 1,
      failures: [{ repositoryId: 'repo-localreview', repositoryPath: '.', error: 'main no longer resolves to a commit' }]
    };
    harness.state.implementation.startNewReview = async () => structuredClone(failed);
    components.push(mount(App, { target: target() }));
    await settle(8);

    const refreshButton = document.querySelector<HTMLButtonElement>('.status-button')!;
    refreshButton.click();
    await settle(8);

    expect(refreshButton.textContent).toContain('Refresh failed');
    expect(refreshButton.textContent).not.toContain('Updated');
    expect(refreshButton.getAttribute('aria-label')).toContain('repository error');
    expect(document.querySelector('.statusbar')?.textContent).toContain('no repositories were updated');
    expect(document.querySelector('.statusbar')?.textContent).toContain('main no longer resolves to a commit');
    expect(document.querySelector('.file-picker')?.textContent).toContain('localreview.ts');
    const repairDialog = document.querySelector('[aria-labelledby="configuration-repair-title"]');
    expect(repairDialog?.textContent).toContain('Fix review configuration');
    expect(repairDialog?.textContent).toContain('origin/main');
    expect(repairDialog?.textContent).toContain('origin/dev');

    [...repairDialog!.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Not now')?.click();
    await settle();
    expect(document.querySelector('[aria-labelledby="configuration-repair-title"]')).toBeNull();

    refreshButton.click();
    await settle(8);
    const reopenedRepair = document.querySelector('[aria-labelledby="configuration-repair-title"]')!;
    [...reopenedRepair.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'Apply fix')?.click();
    await settle(6);
    expect(calls.baseApplications).toEqual([{
      workspaceId: local.id,
      repositoryIds: ['repo-localreview'],
      base: 'origin/dev'
    }]);
    expect(document.querySelector('[aria-labelledby="configuration-repair-title"]')).toBeNull();
    expect(document.querySelector('.statusbar')?.textContent).toContain('Press Refresh to capture the corrected comparison');

    vi.advanceTimersByTime(5001);
    await settle(3);
    expect(refreshButton.textContent).toContain('Changes available · Refresh');
  });

  it('promotes successful repositories but reports a partial native refresh as incomplete', async () => {
    vi.useFakeTimers();
    const local = { ...workspace('workspace-localreview', 'LocalReview'), refreshAvailable: true, refreshAvailableRevision: 2 };
    installApi([local]);
    const partial = review({ ...local, refreshAvailable: true, refreshAvailableRevision: 3 });
    partial.files[0] = { ...partial.files[0], id: 'partial-file', additions: 17, comparisonId: 'partial-comparison' };
    partial.refreshOutcome = {
      status: 'partial',
      capturedRepositoryCount: 1,
      failedRepositoryCount: 1,
      failures: [{ repositoryId: 'repo-b', repositoryPath: 'packages/b', error: 'repository changed during capture' }]
    };
    harness.state.implementation.startNewReview = async () => structuredClone(partial);
    components.push(mount(App, { target: target() }));
    await settle(8);

    const refreshButton = document.querySelector<HTMLButtonElement>('.status-button')!;
    refreshButton.click();
    await settle(12);

    expect(refreshButton.textContent).toContain('Refresh incomplete');
    expect(refreshButton.textContent).not.toBe('Updated');
    expect(document.querySelector('.diff-stats .additions')?.textContent).toBe('+17');
    expect(document.querySelector('.statusbar')?.textContent).toContain('1 repository was updated, but 1 failed');
    expect(document.querySelector('.statusbar')?.textContent).toContain('packages/b');

    vi.advanceTimersByTime(5001);
    await settle(3);
    expect(refreshButton.textContent).toContain('Changes available · Refresh');
  });

  it('drops a stale refresh completion after workspace navigation', async () => {
    const first = workspace('workspace-localreview', 'Workspace A');
    const second = workspace('workspace-b', 'Workspace B');
    installApi([first, second]);
    const capture = deferred<ReviewData>();
    harness.state.implementation.startNewReview = () => capture.promise;
    components.push(mount(App, { target: target() }));
    await settle(6);

    document.querySelector<HTMLButtonElement>('.status-button')?.click();
    await settle(2);
    [...document.querySelectorAll<HTMLButtonElement>('.workspace-tab')]
      .find((button) => button.textContent?.includes('Workspace B'))?.click();
    await settle(12);
    expect(document.querySelector('.workspace-card.selected')?.textContent).toContain('Workspace B');
    expect(document.querySelector('.file-picker')?.textContent).toContain('b.ts');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent).toContain('Refresh');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.disabled).toBe(false);
    expect([...document.querySelectorAll('.workspace-card')].find((card) => card.textContent?.includes('Workspace A'))?.querySelector('.refresh-dot')?.textContent).toContain('Capturing…');

    const stale = review(first);
    stale.files[0] = { ...stale.files[0], path: 'stale-a.ts', additions: 77 };
    capture.resolve(stale);
    await settle(8);

    expect(document.querySelector('.workspace-card.selected')?.textContent).toContain('Workspace B');
    expect(document.querySelector('.file-picker')?.textContent).toContain('b.ts');
    expect(document.body.textContent).not.toContain('stale-a.ts');
    expect(document.querySelector('.refresh-status')).toBeNull();
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent).toContain('Refresh');
    expect([...document.querySelectorAll('.workspace-card')].find((card) => card.textContent?.includes('Workspace A'))?.querySelector('.refresh-dot')?.textContent).toContain('Updated');
  });

  it('briefly confirms success before returning to a newer changes-available revision', async () => {
    vi.useFakeTimers();
    const local = workspace('workspace-localreview', 'LocalReview');
    installApi([local]);
    const next = review({ ...local, refreshAvailable: true });
    harness.state.implementation.startNewReview = async () => structuredClone(next);
    components.push(mount(App, { target: target() }));
    await settle(8);

    const refreshButton = document.querySelector<HTMLButtonElement>('.status-button')!;
    refreshButton.click();
    await settle(10);

    expect(refreshButton.textContent).toContain('Updated');
    expect(refreshButton.classList.contains('updated')).toBe(true);
    vi.advanceTimersByTime(1601);
    await settle(3);
    expect(refreshButton.textContent).toContain('Changes available · Refresh');
    expect(refreshButton.classList.contains('updated')).toBe(false);
  });

  it('orders queued refresh events by the native capture revision', async () => {
    vi.useFakeTimers();
    harness.state.native = true;
    const local = { ...workspace('workspace-localreview', 'LocalReview'), refreshAvailable: true, refreshAvailableRevision: 4 };
    installApi([local]);
    const capture = deferred<ReviewData>();
    harness.state.implementation.startNewReview = () => capture.promise;
    components.push(mount(App, { target: target() }));
    await settle(8);

    const refreshButton = document.querySelector<HTMLButtonElement>('.status-button')!;
    expect(refreshButton.textContent).toContain('Changes available · Refresh');
    refreshButton.click();
    await settle(3);

    // This was queued by the watcher generation that existed before capture.
    // The successful response below acknowledges through revision 5, so the
    // older true event cannot resurrect amber when the queues cross.
    emitNative('localreview://refresh-available', {
      workspaceId: local.id,
      refreshAvailable: true,
      revision: 4
    });
    const refreshed = review({ ...local, refreshAvailable: false, refreshAvailableRevision: 5 });
    capture.resolve(refreshed);
    await settle(10);
    vi.advanceTimersByTime(1601);
    await settle(3);
    expect(refreshButton.textContent).toBe('Refresh');

    emitNative('localreview://refresh-available', {
      workspaceId: local.id,
      refreshAvailable: true,
      revision: 6
    });
    await settle(3);
    expect(refreshButton.textContent).toContain('Changes available · Refresh');
  });

  it('never lets a delayed negative GitHub freshness read clear a newer native event', async () => {
    harness.state.native = true;
    const github = {
      ...workspace('workspace-github', 'GitHub review'),
      source: ['github'] as Workspace['source'],
      refreshAvailable: false,
      refreshAvailableRevision: 3
    };
    installApi([github]);
    const providerStatus = deferred<{ baseChanged: boolean; headChanged: boolean }>();
    harness.state.implementation.getGitHubUpdateStatus = () => providerStatus.promise;
    components.push(mount(App, { target: target() }));
    await settle(8);

    emitNative('localreview://refresh-available', {
      workspaceId: github.id,
      refreshAvailable: true,
      revision: 4
    });
    providerStatus.resolve({ baseChanged: false, headChanged: false });
    await settle(5);

    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent)
      .toContain('Changes available · Refresh');
  });

  it('keeps failure amber and applies an authoritative clear to an inactive rail card', async () => {
    vi.useFakeTimers();
    harness.state.native = true;
    const first = { ...workspace('workspace-localreview', 'Workspace A'), refreshAvailable: true, refreshAvailableRevision: 2 };
    const second = workspace('workspace-b', 'Workspace B');
    installApi([first, second]);
    const capture = deferred<ReviewData>();
    harness.state.implementation.startNewReview = () => capture.promise;
    components.push(mount(App, { target: target() }));
    await settle(8);

    document.querySelector<HTMLButtonElement>('.status-button')?.click();
    await settle(3);
    [...document.querySelectorAll<HTMLButtonElement>('.workspace-tab')]
      .find((button) => button.textContent?.includes('Workspace B'))?.click();
    await settle(10);
    emitNative('localreview://refresh-available', {
      workspaceId: first.id,
      refreshAvailable: false,
      revision: 3
    });
    capture.resolve(review({ ...first, refreshAvailable: false, refreshAvailableRevision: 3 }));
    await settle(10);
    vi.advanceTimersByTime(1601);
    await settle(3);
    const firstCard = [...document.querySelectorAll('.workspace-card')]
      .find((card) => card.textContent?.includes('Workspace A'));
    expect(firstCard?.querySelector('.refresh-dot')).toBeNull();

    [...document.querySelectorAll<HTMLButtonElement>('.workspace-tab')]
      .find((button) => button.textContent?.includes('Workspace A'))?.click();
    await settle(10);
    emitNative('localreview://refresh-available', {
      workspaceId: first.id,
      refreshAvailable: true,
      revision: 4
    });
    harness.state.implementation.startNewReview = async () => { throw new Error('capture failed'); };
    document.querySelector<HTMLButtonElement>('.status-button')?.click();
    await settle(8);
    vi.advanceTimersByTime(3201);
    await settle(3);
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent).toContain('Changes available · Refresh');
  });

  it('flushes A snapshots before a rapid workspace switch and never writes them into B', async () => {
    const first = workspace('workspace-localreview', 'Workspace A');
    const second = workspace('workspace-b', 'Workspace B');
    const calls = installApi([first, second]);
    components.push(mount(App, { target: target() }));
    await settle(5);

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Split')?.click();
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.click();
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'File note')?.click();
    await settle();
    const firstDraft = document.querySelector<HTMLTextAreaElement>('[aria-label="Annotation text"]')!;
    firstDraft.value = 'A draft must stay in A';
    firstDraft.dispatchEvent(new Event('input', { bubbles: true }));

    [...document.querySelectorAll<HTMLButtonElement>('.workspace-tab')]
      .find((button) => button.textContent?.includes('Workspace B'))?.click();
    await settle(8);

    expect(document.querySelector('.workspace-card.selected')?.textContent).toContain('Workspace B');
    expect(calls.uiStates.some((call) => call.workspaceId === first.id && call.state.mode === 'split' && call.state.activeFileId === 'file-localreview')).toBe(true);
    expect(calls.drafts.some((draft) => draft.workspaceId === first.id && draft.fileId === 'file-localreview' && draft.body === 'A draft must stay in A')).toBe(true);

    await new Promise((resolve) => window.setTimeout(resolve, 420));
    await settle();
    expect(calls.uiStates.some((call) => call.workspaceId === second.id && call.state.activeFileId === 'file-localreview')).toBe(false);
    expect(calls.drafts.some((draft) => draft.workspaceId === second.id && draft.fileId === 'file-localreview')).toBe(false);
  });

  it('persists Difftastic before its slow native presentation and restores it after teardown', async () => {
    const local = workspace('workspace-localreview', 'Workspace A');
    const calls = installApi([local]);
    const first = mount(App, { target: target() });
    components.push(first);
    await settle(5);

    const normalPresentation = harness.state.implementation.getPresentationWindow;
    let releaseDifftastic: (() => void) | undefined;
    harness.state.implementation.getPresentationWindow = (request: { mode: string }) => {
      if (request.mode !== 'difftastic') return normalPresentation(request);
      return new Promise((resolve) => {
        releaseDifftastic = () => resolve(normalPresentation(request));
      });
    };

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Difftastic')?.click();
    await settle(4);

    expect(calls.uiStates.some((call) => call.workspaceId === local.id && call.state.mode === 'difftastic')).toBe(true);
    expect(releaseDifftastic).toBeTypeOf('function');

    components.pop();
    await unmount(first);
    harness.state.implementation.getPresentationWindow = normalPresentation;
    releaseDifftastic?.();
    await settle(3);

    components.push(mount(App, { target: target() }));
    await settle(6);
    const difftasticTab = [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent === 'Difftastic');
    expect(difftasticTab?.getAttribute('aria-selected')).toBe('true');
  });
});
