import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { ReviewData, ReviewSettings, Workspace } from './lib/types';

const harness = vi.hoisted(() => {
  const state = { implementation: {} as Record<PropertyKey, (...args: any[]) => any> };
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

import App from './App.svelte';

const targets: HTMLElement[] = [];
const components: ReturnType<typeof mount>[] = [];

const settings: ReviewSettings = {
  fontScale: 1, leftWidth: 244, rightWidth: 332, leftCollapsed: false, rightCollapsed: false,
  fetchOnReview: false, theme: 'dark', codeFont: 'SF Mono', externalEditor: 'system', tabWidth: 2,
  showWhitespace: false, vimNavigation: false, shortcuts: {}
};

function workspace(id: string, name: string, reviewReady = true): Workspace {
  return {
    id, name, reviewReady, source: ['local'], location: `/${id}`, detail: '1 repository',
    progress: { viewed: 0, total: reviewReady ? 1 : 0 }, draftCount: 0, connection: 'connected'
  };
}

function review(value: Workspace): ReviewData {
  const suffix = value.id.replace('workspace-', '');
  return {
    workspace: value,
    repositories: [{ id: `repo-${suffix}`, name: `repo-${suffix}`, path: '.', branch: 'feature', base: 'origin/main', mergeBase: 'abc123', head: 'def456' }],
    files: value.reviewReady === false ? [] : [{
      id: `file-${suffix}`, repositoryId: `repo-${suffix}`, path: `${suffix}.ts`, status: 'modified',
      additions: 1, deletions: 0, language: 'TypeScript', viewed: false, annotationCount: 0
    }],
    annotations: [],
    history: []
  };
}

function installApi(workspaces: Workspace[]) {
  const reviews = new Map(workspaces.map((value) => [value.id, review(value)]));
  const durableUiStates = new Map<string, Record<string, unknown>>();
  const defaultUiState = { mode: 'unified', fullFileSide: 'new', scrollTop: 0, splitRatio: .5, rightTab: 'files' };
  const calls = {
    drafts: [] as Array<Record<string, unknown>>,
    uiStates: [] as Array<{ workspaceId: string; state: Record<string, unknown> }>,
    sessionReads: [] as string[],
    setupReads: [] as string[]
  };
  harness.state.implementation = {
    listWorkspaces: async () => structuredClone(workspaces),
    getSettings: async () => structuredClone(settings),
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
      return [{
        id: `repo-${workspaceId.replace('workspace-', '')}`, path: '.', enabled: true, branch: 'feature',
        statusSummary: 'Clean', effectiveBase: 'origin/main', baseSource: 'workspace'
      }];
    },
    saveWorkspaceUiState: async (workspaceId: string, state: Record<string, unknown>) => {
      calls.uiStates.push({ workspaceId, state: structuredClone(state) });
      const saved = { ...defaultUiState, ...(durableUiStates.get(workspaceId) ?? {}), ...state };
      durableUiStates.set(workspaceId, saved);
      return structuredClone(saved);
    },
    saveAnnotationDraft: async (draft: Record<string, unknown>) => {
      calls.drafts.push(structuredClone(draft));
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

beforeEach(() => {
  localStorage.clear();
});

afterEach(async () => {
  for (const component of components.splice(0)) unmount(component);
  await settle();
  for (const element of targets.splice(0)) element.remove();
});

describe('App review-session persistence boundaries', () => {
  it('resumes initial setup after restart without reading a missing review session', async () => {
    const pending = workspace('workspace-localreview', 'Needs setup', false);
    const calls = installApi([pending]);
    components.push(mount(App, { target: target() }));
    await settle(5);

    expect(document.querySelector('[aria-labelledby="baseline-title"]')).not.toBeNull();
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
    expect(document.querySelector('[aria-labelledby="baseline-title"]')).not.toBeNull();
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
