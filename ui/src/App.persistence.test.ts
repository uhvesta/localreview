import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReviewData, ReviewSettings, Workspace } from './lib/types';

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
  showWhitespace: false, vimNavigation: false, shortcuts: {}
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
  let savedSettings = structuredClone(initialSettings);
  const durableUiStates = new Map<string, Record<string, unknown>>();
  const defaultUiState = { mode: 'unified', fullFileSide: 'new', scrollTop: 0, splitRatio: .5, rightTab: 'files' };
  const calls = {
    drafts: [] as Array<Record<string, unknown>>,
    uiStates: [] as Array<{ workspaceId: string; state: Record<string, unknown> }>,
    sessionReads: [] as string[],
    presentationRequests: [] as Array<{ fileId: string; mode: string; startRow?: number; endRow?: number; fullFileSide?: string }>,
    setupReads: [] as string[],
    inclusionWrites: [] as Array<{ workspaceId: string; repositoryIds: string[]; enabled: boolean }>,
    baselineWrites: [] as Array<{ workspaceId: string; defaultBase?: string; repositoryBases?: unknown[] }>,
    startRequests: [] as Array<{ workspaceId: string; request?: Record<string, unknown> }>,
    refreshRequests: [] as Array<{ workspaceId: string; request?: Record<string, unknown> }>,
    settingsWrites: [] as Array<Partial<ReviewSettings>>
  };
  harness.state.implementation = {
    listWorkspaces: async () => structuredClone(workspaces),
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
    getPresentationWindow: async (request: { fileId: string; mode: string; generation: number }) => {
      calls.sessionReads.push(`presentation:${request.fileId}`);
      calls.presentationRequests.push(structuredClone(request));
      return {
        fileId: request.fileId, mode: request.mode, generation: request.generation,
        startRow: 0, endRow: 1, totalRows: 1, rows: [{ id: `${request.fileId}:1`, kind: 'context', oldLine: 1, newLine: 1, oldText: 'old', newText: 'new' }],
        hunks: [], oldTokens: [], newTokens: []
      };
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
      calls.uiStates.push({ workspaceId, state: structuredClone(state) });
      const saved = { ...defaultUiState, ...(durableUiStates.get(workspaceId) ?? {}), ...state };
      durableUiStates.set(workspaceId, saved);
      return structuredClone(saved);
    },
    saveAnnotationDraft: async (draft: Record<string, unknown>) => {
      calls.drafts.push(structuredClone(draft));
    },
    setViewed: async () => {}
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

  it('resumes initial setup after restart without reading a missing review session', async () => {
    const pending = workspace('workspace-localreview', 'Needs setup', false);
    const calls = installApi([pending]);
    components.push(mount(App, { target: target() }));
    await settle(5);

    expect(document.querySelector('[aria-labelledby="baseline-title"]')).not.toBeNull();
    expect(document.querySelector('#baseline-title')?.textContent).toBe('Start review');
    expect(document.querySelector<HTMLDetailsElement>('.advanced-setup')?.open).toBe(false);
    expect(document.querySelector<HTMLInputElement>('[aria-label="Comparison branch for ."]')?.value).toBe('origin/main');
    expect(document.querySelector('.setup-notice')?.textContent).toContain('selected origin/main for you');
    expect([...document.querySelectorAll<HTMLButtonElement>('button')].some((button) => button.textContent === 'Start review')).toBe(true);
    expect(document.querySelector('.file-picker')?.textContent).toContain('Initial review setup');
    expect(document.body.textContent).not.toContain('Loading diff');
    expect(calls.setupReads).toEqual([pending.id]);
    expect(calls.sessionReads).toEqual([]);
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('.finish-button')?.disabled).toBe(true);
    expect(document.querySelector<HTMLButtonElement>('[aria-label="Copy review content"]')?.disabled).toBe(true);
    expect([...document.querySelectorAll<HTMLButtonElement>('.actions-menu [role="menuitem"]')]
      .find((button) => button.textContent === 'New review')?.disabled).toBe(true);

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
      return loaded;
    };
    const getPresentationWindow = harness.state.implementation.getPresentationWindow;
    harness.state.implementation.getPresentationWindow = async (request: Record<string, unknown>) => {
      const result = await getPresentationWindow(request);
      return request.mode === 'full' && request.fullFileSide !== 'old'
        ? { ...result, totalRows: result.totalRows + 1, deletionBlocks: [{ id: 'removed-1', startLine: 2, endLine: 4, count: 3, expanded: false, rowIndex: 1 }] }
        : result;
    };
    const readUiState = harness.state.implementation.getWorkspaceUiState;
    harness.state.implementation.getWorkspaceUiState = async (workspaceId: string) => ({
      ...(await readUiState(workspaceId)), nearestSourceLine: 80, nearestSourceSide: 'new', scrollTop: 1800
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

    expect(calls.presentationRequests.at(-1)).toMatchObject({ mode: 'full', fullFileSide: 'old' });
    expect(document.querySelector('.full-file-extent')?.textContent).toContain('3 removed lines highlighted');
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
    await settle(5);

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
    expect(viewport.scrollTop).toBe(2_200);
    expect(document.querySelector<HTMLButtonElement>('.mode-picker [role="tab"][aria-selected="true"]')?.textContent).toBe('Full File');

    await clickNavigation('Next');
    expectLastWindowToContain(700);
    expect(viewport.scrollTop).toBe(16_600);

    await clickNavigation('Next');
    expectLastWindowToContain(100);
    expect(viewport.scrollTop).toBe(2_200);

    await clickNavigation('Previous');
    expectLastWindowToContain(700);
    expect(viewport.scrollTop).toBe(16_600);

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', altKey: true, bubbles: true }));
    await settle(5);
    expectLastWindowToContain(100);
    expect(viewport.scrollTop).toBe(2_200);
    expect(document.querySelector<HTMLButtonElement>('.mode-picker [role="tab"][aria-selected="true"]')?.textContent).toBe('Full File');

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', altKey: true, bubbles: true }));
    await settle(5);
    expectLastWindowToContain(700);
    expect(viewport.scrollTop).toBe(16_600);
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.some((call) => call.state.mode === 'full' && call.state.fullFileSide === 'new' && call.state.nearestSourceLine === 651 && call.state.nearestSourceSide === 'old' && call.state.scrollTop === 16_600)).toBe(true);

    document.querySelector<HTMLButtonElement>('[aria-label="Full-file source side"] button:last-child')?.click();
    await settle(5);
    calls.presentationRequests.length = 0;
    await clickNavigation('Next');
    expectLastWindowToContain(100);
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.some((call) => call.state.mode === 'full' && call.state.fullFileSide === 'old' && call.state.nearestSourceLine === 91 && call.state.nearestSourceSide === 'old' && call.state.scrollTop === 2_200)).toBe(true);

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
    harness.state.implementation.getPresentationWindow = async (request: Record<string, unknown>) => ({
      ...(await getPresentationWindow(request)),
      totalRows: 1_000,
      hunks: request.fileId === 'file-zero' ? [] : request.fileId === 'file-other' ? [
        { id: 'other-first', rowIndex: 50, oldLine: 41, newLine: 51, header: '@@ -41 +51 @@' },
        { id: 'other-last', rowIndex: 300, oldLine: 281, newLine: 301, header: '@@ -281 +301 @@' }
      ] : [
        { id: 'local-first', rowIndex: 100, oldLine: 91, newLine: 101, header: '@@ -91 +101 @@' },
        { id: 'local-last', rowIndex: 700, oldLine: 651, newLine: 701, header: '@@ -651 +701 @@' }
      ]
    });
    components.push(mount(App, { target: target() }));
    await settle(5);

    const next = () => document.querySelector<HTMLButtonElement>('[aria-label="Next hunk"]')?.click();
    const previous = () => document.querySelector<HTMLButtonElement>('[aria-label="Previous hunk"]')?.click();
    const activePath = () => document.querySelector('.file-picker')?.textContent ?? '';
    calls.presentationRequests.length = 0;

    next(); await settle(5); // local first
    next(); await settle(5); // local last
    next(); await settle(10); // other first, skipping file-zero
    expect(activePath()).toContain('other.ts');
    expect(calls.presentationRequests.some((request) => request.fileId === 'file-zero')).toBe(false);

    previous(); await settle(10); // local last
    expect(activePath()).toContain('localreview.ts');
    previous(); await settle(5); // local first
    previous(); await settle(10); // wrap to other last
    expect(activePath()).toContain('other.ts');
    await new Promise((resolve) => window.setTimeout(resolve, 160));
    await settle(3);
    expect(calls.uiStates.some((call) => call.state.activeFileId === 'file-other' && call.state.nearestSourceLine === 301)).toBe(true);

    const filter = document.querySelector<HTMLInputElement>('[aria-label="Filter files"]')!;
    filter.value = 'localreview';
    filter.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    next(); await settle(10); // active file is outside filter; enter sole shown file
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
    await settle(5);

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
    harness.state.implementation.refreshReview = () => {
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
    await settle(3);

    expect(refreshCalls).toBe(1);
    expect(refreshButton.disabled).toBe(true);
    expect(refreshButton.textContent).toContain('Capturing…');
    expect(document.querySelector('.refresh-status strong')?.textContent).toContain('Capturing a snapshot');
    expect(document.querySelector('[role="progressbar"]')?.getAttribute('aria-valuetext')).toContain('Capturing a snapshot');
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
    expect(document.querySelector('.statusbar')?.textContent).toContain('complete snapshot is now displayed');
    expect(calls.uiStates.some((call) => call.state.activeFileId === 'next-1')).toBe(true);
  });

  it('keeps the old file list and diff intact when refreshed presentation staging fails', async () => {
    const local = workspace('workspace-localreview', 'LocalReview');
    installApi([local]);
    const initial = review(local);
    harness.state.implementation.loadReview = async () => structuredClone(initial);
    const next = structuredClone(initial);
    next.files[0] = { ...next.files[0], id: 'next-file', path: 'renamed.ts', previousPath: initial.files[0].path, additions: 99, comparisonId: 'next-comparison' };
    harness.state.implementation.refreshReview = async () => structuredClone(next);
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
    expect(document.querySelector('.statusbar')?.textContent).toContain('previous snapshot remains displayed: presentation unavailable');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.disabled).toBe(false);
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.textContent).toContain('Refresh failed');
    expect(document.querySelector<HTMLButtonElement>('.status-button')?.getAttribute('aria-label')).toContain('Retry refresh');
  });

  it('reports an all-repository native capture failure without claiming the review updated', async () => {
    vi.useFakeTimers();
    const local = { ...workspace('workspace-localreview', 'LocalReview'), refreshAvailable: true, refreshAvailableRevision: 2 };
    installApi([local]);
    const failed = review({ ...local, refreshAvailable: true, refreshAvailableRevision: 3 });
    failed.refreshOutcome = {
      status: 'failed',
      capturedRepositoryCount: 0,
      failedRepositoryCount: 1,
      failures: [{ repositoryId: 'repo-localreview', repositoryPath: '.', error: 'main no longer resolves to a commit' }]
    };
    harness.state.implementation.refreshReview = async () => structuredClone(failed);
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
    harness.state.implementation.refreshReview = async () => structuredClone(partial);
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
    harness.state.implementation.refreshReview = () => capture.promise;
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
    harness.state.implementation.refreshReview = async () => structuredClone(next);
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
    harness.state.implementation.refreshReview = () => capture.promise;
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
    harness.state.implementation.refreshReview = () => capture.promise;
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
    harness.state.implementation.refreshReview = async () => { throw new Error('capture failed'); };
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
