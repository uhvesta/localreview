import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import SymbolNavigationWindow from '../SymbolNavigationWindow.svelte';
import VirtualDiff from './VirtualDiff.svelte';
import type {
  ReviewApi,
  SymbolNavigationLocation,
  SymbolNavigationOpenRequest
} from './types';

const mounted: Array<{ component: Record<string, unknown>; host: HTMLElement }> = [];

function target() {
  const host = document.createElement('div');
  document.body.append(host);
  return host;
}

async function settle(turns = 2) {
  for (let turn = 0; turn < turns; turn += 1) {
    await Promise.resolve();
    await tick();
    flushSync();
  }
}

afterEach(() => {
  for (const { component, host } of mounted.splice(0)) {
    unmount(component);
    host.remove();
  }
});

describe('symbol navigation UI', () => {
  it('opens definitions on Cmd-click or keyboard activation and exposes references from the context menu', async () => {
    const requests: SymbolNavigationOpenRequest[] = [];
    const host = target();
    const component = mount(VirtualDiff, {
      target: host,
      props: {
        rows: [{
          id: 'row-1',
          kind: 'addition',
          newLine: 42,
          newText: 'const launch = render();',
          newSourceStartByte: 0
        }],
        totalRows: 1,
        newTokens: [
          { startByte: 0, endByte: 5, class: 'keyword' },
          { startByte: 6, endByte: 12, class: 'function' },
          { startByte: 15, endByte: 21, class: 'function' }
        ],
        symbolContext: {
          workspaceId: 'workspace-1',
          repositoryId: 'repo-1',
          fileId: 'file-1',
          comparisonId: 'comparison-1',
          filePath: 'src/main.ts'
        },
        onNavigateSymbol: (request: SymbolNavigationOpenRequest) => requests.push(request)
      }
    });
    mounted.push({ component, host });
    await settle();

    const launch = host.querySelector<HTMLElement>('[data-symbol="launch"]')!;
    expect(launch.tagName).toBe('SPAN');
    expect(launch.getAttribute('tabindex')).toBeNull();
    expect(host.querySelectorAll('button[data-symbol]')).toHaveLength(0);
    launch.dispatchEvent(new MouseEvent('click', { bubbles: true, metaKey: true, detail: 1 }));
    expect(requests.at(-1)).toEqual({
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      fileId: 'file-1',
      comparisonId: 'comparison-1',
      side: 'new',
      line: 42,
      column: 7,
      symbol: 'launch',
      initialQuery: 'definitions'
    });

    launch.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: 20, clientY: 30 }));
    await settle();
    expect(document.querySelector('[role="menu"]')?.textContent).toContain('Find references');
    [...document.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')]
      .find((button) => button.textContent?.includes('Find references'))?.click();
    expect(requests.at(-1)?.initialQuery).toBe('references');

    const code = launch.closest<HTMLElement>('code')!;
    code.focus();
    await settle();
    code.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'ArrowRight' }));
    code.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'Enter' }));
    expect(requests.at(-1)).toMatchObject({
      symbol: 'render',
      column: 16,
      initialQuery: 'definitions'
    });

    code.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'F10', shiftKey: true }));
    await settle();
    expect(document.querySelector('[role="menu"]')?.textContent).toContain('Find references');
  });

  it('queries each tab lazily and loads bounded verified source only for a selected result', async () => {
    const definition: SymbolNavigationLocation = {
      repositoryId: 'repo-1',
      path: 'src/definition.rs',
      line: 42,
      column: 8,
      endLine: 42,
      endColumn: 14,
      preview: 'pub fn launch() {}',
      kind: 'function',
      role: 'definition',
      sourceFingerprint: 'fingerprint-definition'
    };
    const reference: SymbolNavigationLocation = {
      ...definition,
      path: 'src/caller.ts',
      line: 12,
      column: 3,
      endLine: 12,
      endColumn: 9,
      preview: '  launch();',
      kind: 'call',
      role: 'reference',
      sourceFingerprint: 'fingerprint-reference'
    };
    const querySymbolNavigation = vi.fn(async (input: { kind: string }) => ({
      symbol: 'launch',
      definitions: input.kind === 'definitions' ? [definition] : [],
      references: input.kind === 'references' ? [reference] : [],
      truncated: false,
      diagnostics: []
    }));
    const getSymbolSource = vi.fn(async (input: { path: string; expectedFingerprint: string; startLine: number }) => ({
      repositoryId: 'repo-1',
      path: input.path,
      sourceFingerprint: input.expectedFingerprint,
      startLine: input.startLine,
      totalLines: 80,
      lines: Array.from({ length: 80 }, (_, index) => index === 41 ? 'pub fn launch() {}' : ''),
      lineStartBytes: Array.from({ length: 80 }, (_, index) => index),
      tokens: [],
      highlightStatus: 'plain_text' as const
    }));
    const api = {
      querySymbolNavigation,
      getSymbolSource,
      getRepositoryFiles: vi.fn(async () => ({ files: [], truncated: false, diagnostics: [] })),
      openRepositorySource: vi.fn(),
      getPresentationWindow: vi.fn()
    } as unknown as Pick<
      ReviewApi,
      'querySymbolNavigation' | 'getSymbolSource' | 'getRepositoryFiles' |
      'openRepositorySource' | 'getPresentationWindow'
    >;
    const request: SymbolNavigationOpenRequest = {
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      fileId: 'file-1',
      comparisonId: 'comparison-1',
      side: 'new',
      line: 42,
      column: 8,
      symbol: 'launch',
      initialQuery: 'definitions'
    };
    const host = target();
    const component = mount(SymbolNavigationWindow, { target: host, props: { request, api } });
    mounted.push({ component, host });
    await settle(4);

    expect(querySymbolNavigation).toHaveBeenCalledTimes(1);
    expect(querySymbolNavigation).toHaveBeenLastCalledWith({
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      comparisonId: 'comparison-1',
      symbol: 'launch',
      kind: 'definitions',
      limit: 200
    });
    expect(getSymbolSource).toHaveBeenCalledWith({
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      path: 'src/definition.rs',
      expectedFingerprint: 'fingerprint-definition',
      startLine: 1,
      lineCount: 1000
    });
    expect(host.textContent).toContain('src/definition.rs');

    [...host.querySelectorAll<HTMLButtonElement>('.navigation-tabs button')]
      .find((button) => button.textContent?.includes('References'))?.click();
    await settle(4);
    expect(querySymbolNavigation).toHaveBeenCalledTimes(2);
    expect(querySymbolNavigation.mock.calls[1]?.[0]).toMatchObject({ kind: 'references', limit: 200 });
    expect(host.textContent).toContain('src/caller.ts');
  });

  it('chains editor navigation, keeps history, and toggles the reviewed Both view instantly', async () => {
    const location = (symbol: string, path: string, line = 1): SymbolNavigationLocation => ({
      repositoryId: 'repo-1',
      path,
      line,
      column: 4,
      endLine: line,
      endColumn: 4 + symbol.length,
      preview: `fn ${symbol}() { helper(); }`,
      kind: 'function',
      role: 'definition',
      sourceFingerprint: `fingerprint-${symbol}`,
      fileId: 'file-1',
      comparisonId: 'comparison-1',
      side: 'new'
    });
    const querySymbolNavigation = vi.fn(async (input: { symbol: string }) => ({
      symbol: input.symbol,
      definitions: [location(input.symbol, input.symbol === 'launch' ? 'src/launch.rs' : 'src/helper.rs')],
      references: [],
      truncated: false,
      diagnostics: []
    }));
    const getSymbolSource = vi.fn(async (input: { path: string; expectedFingerprint: string }) => ({
      repositoryId: 'repo-1',
      path: input.path,
      sourceFingerprint: input.expectedFingerprint,
      startLine: 1,
      totalLines: 1,
      lines: ['fn launch() { helper(); }'],
      lineStartBytes: [0],
      tokens: [],
      highlightStatus: 'highlighted' as const,
      language: 'Rust'
    }));
    const getPresentationWindow = vi.fn(async (input: { generation: number }) => ({
      generation: input.generation,
      mode: 'full' as const,
      fileId: 'file-1',
      startRow: 0,
      totalRows: 2,
      rows: [
        { id: 'addition-gate', kind: 'addition_gate' as const, newLine: 2, omittedBlockId: 'add-1', omittedCount: 2, omittedEndLine: 3, omittedSide: 'new' as const, omittedExpanded: true },
        { id: 'deletion-gate', kind: 'deletion_gate' as const, oldLine: 4, omittedBlockId: 'del-1', omittedCount: 2, omittedEndLine: 5, omittedSide: 'old' as const, omittedExpanded: false }
      ],
      hunks: [],
      omittedBlocks: [
        { id: 'add-1', side: 'new' as const, startLine: 2, endLine: 3, count: 2, expanded: true, rowIndex: 0 },
        { id: 'del-1', side: 'old' as const, startLine: 4, endLine: 5, count: 2, expanded: false, rowIndex: 1 }
      ],
      oldTokens: [],
      newTokens: [],
      highlightStatus: 'highlighted' as const
    }));
    const api = {
      querySymbolNavigation,
      getSymbolSource,
      getPresentationWindow,
      getRepositoryFiles: vi.fn(async () => ({ files: [], truncated: false, diagnostics: [] })),
      openRepositorySource: vi.fn()
    } as unknown as Pick<
      ReviewApi,
      'querySymbolNavigation' | 'getSymbolSource' | 'getRepositoryFiles' |
      'openRepositorySource' | 'getPresentationWindow'
    >;
    const request: SymbolNavigationOpenRequest = {
      workspaceId: 'workspace-1',
      repositoryId: 'repo-1',
      fileId: 'file-1',
      comparisonId: 'comparison-1',
      side: 'new',
      line: 1,
      column: 4,
      symbol: 'launch',
      initialQuery: 'definitions'
    };
    const host = target();
    const component = mount(SymbolNavigationWindow, { target: host, props: { request, api } });
    mounted.push({ component, host });
    await settle(5);

    const helper = [...host.querySelectorAll<HTMLButtonElement>('.source-token')]
      .find((button) => button.textContent === 'helper')!;
    helper.dispatchEvent(new MouseEvent('click', { bubbles: true, metaKey: true }));
    await settle(5);
    expect(querySymbolNavigation).toHaveBeenLastCalledWith(expect.objectContaining({ symbol: 'helper' }));
    expect(host.textContent).toContain('src/helper.rs');

    host.querySelector<HTMLButtonElement>('button[aria-label="Back"]')?.click();
    await settle(4);
    expect(host.textContent).toContain('src/launch.rs');

    [...host.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('Show review diff'))?.click();
    await settle(4);
    expect(host.textContent).toContain('Full File · Both');
    expect(host.textContent).toContain('Show 2 deleted lines');

    const beforeBulk = getPresentationWindow.mock.calls.length;
    [...host.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('Show all deletions'))?.click();
    await settle(4);
    expect(getPresentationWindow.mock.calls.length).toBeGreaterThan(beforeBulk);

    [...host.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('Hide diff decorations'))?.click();
    await settle();
    expect(host.textContent).not.toContain('Full File · Both');
    expect(host.querySelector('.source-scroll pre')).not.toBeNull();
  });
});
