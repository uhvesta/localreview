import { flushSync, mount, tick, unmount } from 'svelte';
import { readFileSync } from 'node:fs';
import { afterEach, describe, expect, it } from 'vitest';
import App from '../App.svelte';
import VirtualDiff from './VirtualDiff.svelte';
import VirtualFileList from './VirtualFileList.svelte';
import WorkspaceRail from './WorkspaceRail.svelte';
import type { Annotation, DiffRow, Workspace } from './types';

const targets: HTMLElement[] = [];

function target() {
  const element = document.createElement('div');
  document.body.append(element);
  targets.push(element);
  return element;
}

async function settle() {
  await Promise.resolve();
  await tick();
  flushSync();
}

afterEach(() => {
  for (const element of targets.splice(0)) element.remove();
  localStorage.clear();
});

describe('review components', () => {
  it('mounts the app, clamps zoom, changes diff mode, and collapses the review panel', async () => {
    const component = mount(App, { target: target() });
    await settle();
    expect(document.body.textContent).toContain('LOCALREVIEW');

    const increase = document.querySelector<HTMLButtonElement>('[aria-label="Increase font size"]');
    for (let index = 0; index < 12; index += 1) {
      increase?.click();
      await settle();
    }
    expect(document.body.textContent).toContain('200%');
    expect(document.querySelector('.app-shell')?.classList.contains('large-text')).toBe(true);
    expect(document.querySelector('.workspace-rail')).not.toBeNull();
    expect(document.querySelector('.review-panel')).not.toBeNull();
    expect(document.querySelector('.theme-root')?.classList.contains('large-text-root')).toBe(true);
    expect(document.querySelector('.actions-menu summary')?.textContent).toBe('Actions');
    expect([...document.querySelectorAll<HTMLElement>('.actions-menu [role="menuitem"]')].map((button) => button.textContent)).toEqual(['Copy review prompt', 'Review setup', 'Start new review', 'History', 'Blame selected lines', 'Commit context', 'Changed since previous review', 'Settings']);
    document.querySelector<HTMLButtonElement>('.actions-menu [role="menuitem"]')?.click();
    await settle();
    expect(document.querySelector('[aria-labelledby="prompt-title"]')).not.toBeNull();
    document.querySelector<HTMLButtonElement>('[aria-label="Close prompt preview"]')?.click();
    await settle();
    const decrease = document.querySelector<HTMLButtonElement>('[aria-label="Decrease font size"]');
    for (let index = 0; index < 20; index += 1) {
      decrease?.click();
      await settle();
    }
    expect(document.body.textContent).toContain('75%');

    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((button) => button.textContent === 'Difftastic')?.click();
    await settle();
    expect(document.body.textContent).toContain('Backend Difftastic adapter');
    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')].find((button) => button.textContent === 'Unified')?.click();
    await settle();
    expect(document.body.textContent).not.toContain('Backend Difftastic adapter');

    document.querySelector<HTMLButtonElement>('[aria-label="Close review panel"]')?.click();
    await settle();
    expect(document.querySelector('[aria-label="Open files and review panel"]')).not.toBeNull();
    unmount(component);
  });

  it('selects the most-recent visible workspace when a source filter hides the active one', async () => {
    const workspaces: Workspace[] = [
      { id: 'local', name: 'Local', source: ['local'], location: '/local', detail: '', progress: { viewed: 0, total: 1 }, draftCount: 0 },
      { id: 'github', name: 'GitHub PR', source: ['github'], location: 'github.com/acme/repo#1', detail: '', progress: { viewed: 0, total: 1 }, draftCount: 0 },
      { id: 'ssh', name: 'SSH', source: ['ssh'], location: 'host:/repo', detail: '', progress: { viewed: 0, total: 1 }, draftCount: 0 }
    ];
    const selections: string[] = [];
    const component = mount(WorkspaceRail, { target: target(), props: { workspaces, selectedId: 'local', onSelect: (id: string) => selections.push(id) } });
    await settle();
    [...document.querySelectorAll('button')].find((button) => button.textContent === 'GitHub')?.click();
    await settle();
    expect(selections.at(-1)).toBe('github');
    unmount(component);
  });

  it('reveals All workspaces when a newly opened PR crosses the current source filter', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    const sourceFilter = document.querySelector<HTMLElement>('[aria-label="Filter workspaces"]')!;
    [...sourceFilter.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Local')?.click();
    await settle();
    expect(sourceFilter.querySelector<HTMLButtonElement>('[aria-pressed="true"]')?.textContent).toBe('Local');

    document.querySelector<HTMLButtonElement>('.open-workspace')?.click();
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('.open-options button')]
      .find((button) => button.textContent?.includes('Paste GitHub PR URL'))?.click();
    await settle();
    const input = document.querySelector<HTMLInputElement>('[aria-label="GitHub pull request URL"]')!;
    input.value = 'https://github.com/spinyfin/mono/pull/2138';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    [...document.querySelectorAll<HTMLButtonElement>('.open-modal footer button')]
      .find((button) => button.textContent === 'Open PR review')?.click();
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();

    expect(sourceFilter.querySelector<HTMLButtonElement>('[aria-pressed="true"]')?.textContent).toBe('All');
    expect([...document.querySelectorAll('.workspace-tab')].some((tab) => tab.textContent?.includes('LocalReview'))).toBe(true);
    expect([...document.querySelectorAll('.workspace-tab')].some((tab) => tab.textContent?.includes('spinyfin/mono #2138'))).toBe(true);
    expect([...document.querySelectorAll<HTMLElement>('.workspace-tab')]
      .find((tab) => tab.textContent?.includes('spinyfin/mono #2138'))?.getAttribute('aria-selected')).toBe('true');
    unmount(component);
  });

  it('exposes explicit SSH reconnect, archive, and permanent-delete actions in the workspace rail', async () => {
    const ssh: Workspace = { id: 'ssh', name: 'Remote', source: ['ssh'], location: 'host:/repo', detail: '', progress: { viewed: 0, total: 1 }, draftCount: 0, connection: 'offline' };
    const archived: string[] = [];
    const removed: string[] = [];
    const reconnected: string[] = [];
    const component = mount(WorkspaceRail, {
      target: target(),
      props: {
        workspaces: [ssh], selectedId: ssh.id,
        onArchive: (workspace: Workspace) => archived.push(workspace.id),
        onDelete: (workspace: Workspace) => removed.push(workspace.id),
        onReconnect: (workspace: Workspace) => reconnected.push(workspace.id)
      }
    });
    await settle();
    document.querySelector<HTMLButtonElement>('[aria-label="Reconnect Remote"]')?.click();
    const archiveButton = document.querySelector<HTMLButtonElement>('[aria-label="Archive workspace Remote"]');
    const deleteButton = document.querySelector<HTMLButtonElement>('[aria-label="Delete workspace Remote"]');
    expect(archiveButton?.textContent).toBe('Archive');
    expect(deleteButton?.textContent).toBe('Delete…');
    archiveButton?.click();
    deleteButton?.click();
    expect(reconnected).toEqual(['ssh']);
    expect(archived).toEqual(['ssh']);
    expect(removed).toEqual(['ssh']);
    unmount(component);
  });

  it('retains rapid optimistic zoom changes through the 200% bound', async () => {
    const component = mount(App, { target: target() });
    await settle();
    const increase = document.querySelector<HTMLButtonElement>('[aria-label="Increase font size"]');
    for (let index = 0; index < 12; index += 1) increase?.click();
    await settle();
    await Promise.resolve();
    await settle();
    expect(document.body.textContent).toContain('200%');
    unmount(component);
  });

  it('keeps local workspace open errors visible inside the modal', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    document.querySelector<HTMLButtonElement>('.open-workspace')?.click();
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('.open-options button')]
      .find((button) => button.textContent?.includes('Open local folder'))?.click();
    await settle();
    const baseInput = document.querySelector<HTMLInputElement>('[aria-label="Default base reference"]');
    expect(baseInput?.value).toBe('');
    expect(baseInput?.placeholder).toBe('Use workspace/global default');
    [...document.querySelectorAll<HTMLButtonElement>('.open-modal footer button')]
      .find((button) => button.textContent === 'Open local folder')?.click();
    await settle();
    expect(document.querySelector('.open-modal [role="alert"]')?.textContent)
      .toBe('Enter a folder path before opening a workspace.');
    expect(document.querySelector('.open-modal')).not.toBeNull();
    unmount(component);
  });

  it('creates editable file and review notes without inventing an inline range', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.click();
    await settle();

    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent === 'File note')?.click();
    await settle();
    expect(document.body.textContent).toContain('File note · ui/src/App.svelte');
    const fileTextarea = document.querySelector<HTMLTextAreaElement>('[aria-label="Annotation text"]')!;
    fileTextarea.value = 'Capture the whole file concern.';
    fileTextarea.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.startsWith('Save annotation'))?.click();
    await settle();
    expect(document.body.textContent).toContain('File-level note');
    expect(document.querySelector('.workspace-card.selected .draft-count')?.textContent).toBe('4');

    const reviewNote = document.querySelector<HTMLButtonElement>('.comment-actions button:nth-of-type(6)');
    expect(reviewNote?.textContent).toBe('Review note');
    expect(reviewNote?.disabled).toBe(false);
    reviewNote?.click();
    await settle();
    expect(document.body.textContent).toContain('Review note · whole review');
    const reviewTextarea = document.querySelector<HTMLTextAreaElement>('[aria-label="Annotation text"]')!;
    reviewTextarea.value = 'Capture a cross-file rollout concern.';
    reviewTextarea.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.startsWith('Save annotation'))?.click();
    await settle();
    expect(document.body.textContent).toContain('Anchorless review note');
    expect(document.querySelector('.workspace-card.selected .draft-count')?.textContent).toBe('5');
    unmount(component);
  });

  it('shows a newly saved multiline annotation in the diff immediately', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();

    document.querySelector<HTMLButtonElement>('[aria-label="Add annotation at new line 68"]')?.click();
    await settle();
    document.querySelector<HTMLButtonElement>('[aria-label="Add annotation at new line 70"]')
      ?.dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    await settle();
    const textarea = document.querySelector<HTMLTextAreaElement>('[aria-label="Annotation text"]')!;
    textarea.value = 'Keep the zoom update atomic across these lines.';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.startsWith('Save annotation'))?.click();
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    await settle();

    const startGutter = document.querySelector<HTMLButtonElement>('[data-side="new"][data-line="68"]')!;
    const startRow = startGutter.closest<HTMLElement>('.diff-row')!;
    expect(startRow.classList.contains('annotation-range')).toBe(true);
    expect(startRow.getAttribute('aria-label')).toContain('1 annotation');
    expect(document.querySelectorAll('.diff-row.annotation-range')).toHaveLength(4);

    const toggle = document.querySelector<HTMLButtonElement>('[aria-label*="Show 1 annotation"][data-line="70"]')!;
    expect(toggle).not.toBeNull();
    toggle.click();
    await settle();
    expect(document.querySelector('.inline-thread-popover')?.textContent)
      .toContain('Keep the zoom update atomic across these lines.');
    unmount(component);
  });

  it('lists every selected local item and its publication state before a GitHub submit', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('button')]
      .find((button) => button.textContent?.includes('acme/api'))?.click();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    document.querySelector<HTMLButtonElement>('.finish-button')?.click();
    await new Promise((resolve) => window.setTimeout(resolve, 300));
    await settle();
    expect(document.querySelector('[aria-label="Selected review items"]')?.textContent).toContain('Selected local items');
    expect(document.querySelector('[aria-label="Selected review items"]')?.textContent).toContain('Local only · excluded from GitHub');
    expect(document.body.textContent).toContain('LocalReview verified every path and line anchor');
    expect(document.querySelector('[aria-label="Exact GitHub review payload"]')).toBeNull();
    const finishContent = document.querySelector<HTMLElement>('.finish-content')!;
    const selectedItem = document.querySelector<HTMLElement>('.finish-items article')!;
    const selectedPath = selectedItem.querySelector<HTMLElement>('code')!;
    const selectedBody = selectedItem.querySelector<HTMLElement>('small')!;
    expect(finishContent).not.toBeNull();
    expect(selectedPath).not.toBeNull();
    expect(selectedBody).not.toBeNull();
    const appCss = readFileSync('src/app.css', 'utf8');
    expect(appCss).toMatch(/\.finish-content\s*\{[^}]*overflow-x:\s*hidden/);
    expect(appCss).toMatch(/\.finish-items article > div\s*\{[^}]*min-width:\s*0/);
    expect(appCss).toMatch(/\.finish-items code\s*\{[^}]*min-width:\s*0[^}]*text-overflow:\s*ellipsis/);
    expect(appCss).toMatch(/\.finish-items small\s*\{[^}]*white-space:\s*pre-wrap[^}]*overflow-wrap:\s*anywhere/);
    unmount(component);
  });

  it('keeps Difftastic read-only and sends same-side shifted ranges to the annotation composer', async () => {
    const rows: DiffRow[] = [
      { id: 'old-4', kind: 'deletion', oldLine: 4, oldText: 'before four' },
      { id: 'old-7', kind: 'deletion', oldLine: 7, oldText: 'before seven' },
      { id: 'old-9', kind: 'deletion', oldLine: 9, oldText: 'before nine' },
      { id: 'new-8', kind: 'addition', newLine: 8, newText: 'after eight' }
    ];
    const selections: Array<{ side: string; startLine: number; endLine: number }> = [];
    let component = mount(VirtualDiff, { target: target(), props: { rows, mode: 'difftastic', activeLine: undefined, onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => { selections.push(selection); } } });
    await settle();
    expect([...document.querySelectorAll<HTMLButtonElement>('.annotation-gutter')].every((button) => button.disabled)).toBe(true);
    unmount(component);

    component = mount(VirtualDiff, { target: target(), props: { rows, mode: 'split', activeLine: undefined, onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => { selections.push(selection); } } });
    await settle();
    const oldButtons = [...document.querySelectorAll<HTMLButtonElement>('[aria-label^="Add annotation at old line"]')];
    oldButtons[0].dispatchEvent(new MouseEvent('click', { bubbles: true }));
    oldButtons[1].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    await settle();
    expect(selections.at(-1)).toEqual({ side: 'old', startLine: 4, endLine: 7 });
    oldButtons[2].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    await settle();
    expect(selections.at(-1)).toEqual({ side: 'old', startLine: 4, endLine: 9 });
    unmount(component);

    const fullTarget = target();
    component = mount(VirtualDiff, { target: fullTarget, props: { rows: [rows[0]], mode: 'full', activeLine: undefined, onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => { selections.push(selection); } } });
    await settle();
    fullTarget.querySelector<HTMLButtonElement>('[aria-label="Add annotation at old line 4"]')?.click();
    await settle();
    expect(selections.at(-1)).toEqual({ side: 'old', startLine: 4, endLine: 4 });
    unmount(component);
  });

  it('renders Full File Current removal tombstones as selectable highlighted Base rows', async () => {
    const rows: DiffRow[] = [
      { id: 'new-3', kind: 'context', newLine: 3, newText: 'before();', newSourceStartByte: 20 },
      { id: 'old-4', kind: 'deletion', oldLine: 4, oldText: 'const removed = 1;', oldSourceStartByte: 30 },
      { id: 'old-5', kind: 'deletion', oldLine: 5, oldText: 'const alsoRemoved = 2;', oldSourceStartByte: 49 },
      { id: 'new-4', kind: 'context', newLine: 4, newText: 'after();', newSourceStartByte: 30 }
    ];
    const selections: Array<{ side: string; startLine: number; endLine: number }> = [];
    const host = target();
    const component = mount(VirtualDiff, {
      target: host,
      props: {
        rows,
        totalRows: rows.length,
        mode: 'full',
        fullFileSide: 'new',
        oldTokens: [{ startByte: 30, endByte: 35, class: 'keyword' }],
        onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => selections.push(selection)
      }
    });
    await settle();

    const tombstones = [...host.querySelectorAll<HTMLElement>('.diff-row.removed')];
    expect(tombstones).toHaveLength(2);
    expect(tombstones[0].getAttribute('aria-label')).toContain('old line 4, removed change');
    expect(tombstones[0].getAttribute('aria-label')).toContain('Removed Base line shown inline at its Current-file deletion anchor');
    expect(tombstones[0].querySelector('.line-number.old')?.textContent).toBe('4');
    expect(tombstones[0].querySelector('.line-number.new')?.textContent).toBe('');
    expect(tombstones[0].querySelector('.syntax-keyword')?.textContent).toBe('const');

    const gutters = [...host.querySelectorAll<HTMLButtonElement>('[aria-label^="Add annotation at old line"]')];
    gutters[0].dispatchEvent(new MouseEvent('click', { bubbles: true }));
    gutters[1].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    await settle();
    expect(selections.at(-1)).toEqual({ side: 'old', startLine: 4, endLine: 5 });
    unmount(component);
  });

  it('renders wrapped Full File deletion gates as compact full-width controls and expands on demand', async () => {
    const toggles: string[] = [];
    const host = target();
    const component = mount(VirtualDiff, {
      target: host,
      props: {
        rows: [{
          id: 'gate-4-8', kind: 'deletion_gate', oldLine: 4, oldEndLine: 8,
          deletionBlockId: 'block-4-8', deletionCount: 5, text: '5 deleted lines', hasAnnotation: true
        }],
        totalRows: 1,
        mode: 'full',
        fullFileSide: 'new',
        wrapLines: true,
        onToggleDeletionBlock: (id: string) => toggles.push(id)
      }
    });
    await settle();

    const gate = host.querySelector<HTMLElement>('.deletion-gate-row')!;
    expect(gate.getAttribute('aria-label')).toContain('5 deleted Base lines, lines 4–8, collapsed');
    expect(gate.getAttribute('aria-label')).toContain('Contains annotations');
    const toggle = gate.querySelector<HTMLButtonElement>('.deletion-gate-toggle')!;
    expect(toggle.textContent).toContain('› 5 deleted lines · Base 4–8 · annotations hidden');
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    const appCss = readFileSync('src/app.css', 'utf8');
    const gateColumns = appCss.match(/^\.diff-row\.deletion-gate-row\s*\{[^}]+\}/gm)?.at(-1) ?? '';
    expect(gateColumns).toContain('minmax(0, 1fr)');
    const wrappedGate = appCss.match(/\.diff-viewport\.wrap-lines \.diff-row\.deletion-gate-row\s*\{[^}]+\}/g)?.at(-1) ?? '';
    expect(wrappedGate).toContain('white-space: nowrap');
    const wrappedToggle = appCss.match(/\.diff-viewport\.wrap-lines \.deletion-gate-toggle\s*\{[^}]+\}/g)?.at(-1) ?? '';
    expect(wrappedToggle).toContain('min-width: 0');
    expect(wrappedToggle).toContain('overflow: hidden');
    expect(wrappedToggle).toContain('text-overflow: ellipsis');
    toggle.click();
    expect(toggles).toEqual(['block-4-8']);
    unmount(component);
  });

  it('retains an expanded Full File deletion gate as a per-block collapse control', async () => {
    const toggles: string[] = [];
    const host = target();
    const component = mount(VirtualDiff, {
      target: host,
      props: {
        rows: [{
          id: 'gate-4-8', kind: 'deletion_gate', oldLine: 4, oldEndLine: 8,
          deletionBlockId: 'block-4-8', deletionCount: 5, deletionExpanded: true,
          text: '5 deleted lines'
        }],
        totalRows: 1,
        mode: 'full',
        fullFileSide: 'new',
        onToggleDeletionBlock: (id: string) => toggles.push(id)
      }
    });
    await settle();

    const gate = host.querySelector<HTMLElement>('.deletion-gate-row')!;
    expect(gate.getAttribute('aria-label')).toContain('5 deleted Base lines, lines 4–8, expanded');
    const toggle = gate.querySelector<HTMLButtonElement>('.deletion-gate-toggle')!;
    expect(toggle.textContent).toContain('⌄ 5 deleted lines · Base 4–8');
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(toggle.getAttribute('aria-label')).toContain('Hide 5 deleted lines');
    toggle.click();
    expect(toggles).toEqual(['block-4-8']);
    unmount(component);
  });

  it('keeps an active inline draft visibly attached across the virtual diff surface', async () => {
    const host = target();
    const component = mount(VirtualDiff, {
      target: host,
      props: {
        rows: [
          { id: 'new-40', kind: 'context', newLine: 40, newText: 'const first = true;' },
          { id: 'new-41', kind: 'context', newLine: 41, newText: 'const second = true;' },
          { id: 'new-42', kind: 'context', newLine: 42, newText: 'const third = true;' }
        ],
        mode: 'unified',
        composerSelection: { side: 'new', startLine: 40, endLine: 42 },
        composerKind: 'comment'
      }
    });
    await settle();
    expect(host.textContent).toContain('Draft attached to new lines 40–42 · comment');
    expect(host.querySelectorAll('.diff-row.composer-range')).toHaveLength(3);
    unmount(component);
  });

  it('shows stacked inline comment and question indicators without opening the editor', async () => {
    const rows: DiffRow[] = [10, 11, 12].map((line) => ({
      id: `new-${line}`, kind: 'context', newLine: line, newText: `const value${line} = true;`
    }));
    const annotations: Annotation[] = [
      { id: 'comment-range', fileId: 'file', repositoryId: 'repo', kind: 'comment', state: 'open', side: 'new', startLine: 10, endLine: 12, body: 'Keep this range cohesive.', selectedSource: 'range', labels: ['important'], localOnly: false, createdAt: '2026-01-01T00:00:00Z' },
      { id: 'question-range', fileId: 'file', repositoryId: 'repo', kind: 'question', state: 'open', side: 'new', startLine: 11, endLine: 12, body: 'Should this be awaited?', selectedSource: 'range', labels: [], localOnly: true, createdAt: '2026-01-01T00:00:01Z' },
      { id: 'comment-line', fileId: 'file', repositoryId: 'repo', kind: 'comment', state: 'resolved', side: 'new', startLine: 12, endLine: 12, body: 'Resolved context still belongs here.', selectedSource: 'line', labels: [], localOnly: false, createdAt: '2026-01-01T00:00:02Z' }
    ];
    const edits: string[] = [];
    const additions: Array<{ side: string; startLine: number; endLine: number }> = [];
    const host = target();
    const component = mount(VirtualDiff, {
      target: host,
      props: {
        rows, totalRows: rows.length, mode: 'unified',
        annotationsForRow: (row: DiffRow, side: string) => annotations.filter((annotation) => {
          const line = side === 'old' ? row.oldLine : row.newLine;
          return annotation.side === side && line !== undefined && annotation.startLine <= line && annotation.endLine >= line;
        }),
        annotationCountAt: (row: DiffRow, side: string) => annotations.filter((annotation) => {
          const line = side === 'old' ? row.oldLine : row.newLine;
          return annotation.side === side && line !== undefined && annotation.startLine <= line && annotation.endLine >= line;
        }).length,
        onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => additions.push(selection),
        onEditAnnotation: (annotation: Annotation) => edits.push(annotation.id)
      }
    });
    await settle();

    expect(host.querySelectorAll('.diff-row.annotation-range')).toHaveLength(3);
    expect(host.querySelectorAll('.annotation-thread-toggle')).toHaveLength(1);
    const toggle = host.querySelector<HTMLButtonElement>('.annotation-thread-toggle')!;
    expect(toggle.getAttribute('aria-label')).toContain('3 annotations (2 comments, 1 question)');
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(toggle.classList.contains('question-thread')).toBe(true);
    expect(host.querySelector('.inline-thread-popover')).toBeNull();
    expect(additions).toHaveLength(0);
    expect(edits).toHaveLength(0);

    toggle.focus();
    expect(document.activeElement).toBe(toggle);
    toggle.click();
    await settle();
    const panel = host.querySelector<HTMLElement>('.inline-thread-popover')!;
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(toggle.getAttribute('aria-controls')).toBe(panel.id);
    expect(panel.querySelectorAll('.inline-thread-item')).toHaveLength(3);
    expect(panel.querySelector('.inline-thread-kind.comment')?.textContent).toBe('Comment');
    expect(panel.querySelector('.inline-thread-kind.question')?.textContent).toBe('Question');
    expect(panel.textContent).toContain('new lines 10–12');
    expect(panel.textContent).toContain('Should this be awaited?');
    expect(host.querySelector<HTMLElement>('.virtual-spacer')?.style.height).toBe('60px');
    expect(additions).toHaveLength(0);
    expect(edits).toHaveLength(0);

    panel.querySelector<HTMLButtonElement>('.inline-thread-item button')?.click();
    expect(edits).toEqual(['comment-range']);
    expect(additions).toHaveLength(0);
    await settle();
    toggle.click();
    await settle();
    host.querySelector<HTMLButtonElement>('.inline-thread-actions button')?.click();
    expect(additions.at(-1)).toEqual({ side: 'new', startLine: 12, endLine: 12 });
    unmount(component);
  });

  it('keeps split and full-file annotation threads side-aware and read-only when requested', async () => {
    const rows: DiffRow[] = [1, 2].map((line) => ({ id: `paired-${line}`, kind: 'context', oldLine: line, newLine: line, oldText: `old ${line}`, newText: `new ${line}` }));
    const annotations: Annotation[] = [
      { id: 'old-comment', fileId: 'file', repositoryId: 'repo', kind: 'comment', state: 'open', side: 'old', startLine: 1, endLine: 2, body: 'Old-side feedback.', selectedSource: 'old', labels: [], localOnly: false, createdAt: '2026-01-01T00:00:00Z' },
      { id: 'new-question', fileId: 'file', repositoryId: 'repo', kind: 'question', state: 'open', side: 'new', startLine: 1, endLine: 2, body: 'New-side question?', selectedSource: 'new', labels: [], localOnly: false, createdAt: '2026-01-01T00:00:01Z' }
    ];
    const annotationsForRow = (row: DiffRow, side: string) => annotations.filter((annotation) => {
      const line = side === 'old' ? row.oldLine : row.newLine;
      return annotation.side === side && line !== undefined && annotation.startLine <= line && annotation.endLine >= line;
    });
    const host = target();
    let component = mount(VirtualDiff, { target: host, props: { rows, totalRows: rows.length, mode: 'split', annotationsForRow } });
    await settle();
    expect(host.querySelectorAll('.annotation-thread-toggle')).toHaveLength(2);
    expect(host.querySelectorAll('.annotation-range-cell')).toHaveLength(16);
    const oldToggle = host.querySelector<HTMLButtonElement>('[aria-label*="at old line 2"]')!;
    const newToggle = host.querySelector<HTMLButtonElement>('[aria-label*="at new line 2"]')!;
    oldToggle.click();
    await settle();
    expect(host.querySelector('.inline-thread-popover.side-old')?.textContent).toContain('Old-side feedback.');
    newToggle.click();
    await settle();
    expect(host.querySelector('.inline-thread-popover.side-old')).toBeNull();
    expect(host.querySelector('.inline-thread-popover.side-new')?.textContent).toContain('New-side question?');
    unmount(component);

    component = mount(VirtualDiff, { target: host, props: { rows, totalRows: rows.length, mode: 'full', fullFileSide: 'new', annotationsForRow, annotationsEditable: false } });
    await settle();
    host.querySelector<HTMLButtonElement>('.annotation-thread-toggle')?.click();
    await settle();
    expect(host.querySelector('.inline-thread-popover.side-new')?.textContent).toContain('New-side question?');
    expect(host.querySelector<HTMLButtonElement>('.inline-thread-item button')?.disabled).toBe(true);
    expect(host.querySelector<HTMLButtonElement>('.inline-thread-actions button')?.disabled).toBe(true);
    unmount(component);
  });

  it('keeps real pointer shift ranges expanded, highlights drag previews, and colors only the selected split side', async () => {
    const selections: Array<{ side: string; startLine: number; endLine: number }> = [];
    const rows: DiffRow[] = [1, 2, 3].map((line) => ({
      id: `line-${line}`, kind: 'context', oldLine: line, newLine: line,
      oldText: `const old${line} = true;`, newText: `const next${line} = true;`
    }));
    const host = target();
    let component = mount(VirtualDiff, {
      target: host,
      props: { rows, mode: 'unified', onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => selections.push(selection) }
    });
    await settle();
    const gutters = [...host.querySelectorAll<HTMLButtonElement>('[aria-label^="Add annotation at new line"]')];
    gutters[0].dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, button: 0 }));
    window.dispatchEvent(new MouseEvent('pointerup', { bubbles: true, button: 0 }));
    gutters[0].dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await new Promise((resolve) => window.setTimeout(resolve, 0));

    gutters[2].dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, button: 0, shiftKey: true }));
    await settle();
    expect(host.textContent).toContain('Selecting new lines 1–3');
    expect(host.querySelectorAll('.diff-row.composer-range')).toHaveLength(3);
    window.dispatchEvent(new MouseEvent('pointerup', { bubbles: true, button: 0, shiftKey: true }));
    gutters[2].dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    expect(selections.at(-1)).toEqual({ side: 'new', startLine: 1, endLine: 3 });
    unmount(component);

    component = mount(VirtualDiff, {
      target: host,
      props: { rows: [rows[0]], mode: 'split', composerSelection: { side: 'old', startLine: 1, endLine: 1 } }
    });
    await settle();
    expect(host.querySelectorAll('.composer-range-cell-old')).toHaveLength(4);
    expect(host.querySelectorAll('.composer-range-cell-new')).toHaveLength(0);
    expect(host.querySelector('[aria-label="Add annotation at old line 1"]')?.getAttribute('aria-pressed')).toBe('true');
    expect(host.querySelector('[aria-label="Add annotation at new line 1"]')?.getAttribute('aria-pressed')).toBe('false');
    unmount(component);
  });

  it('preserves typed draft text while extending a range and restores that range after remount', async () => {
    let component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 30));
    await settle();
    const start = document.querySelector<HTMLButtonElement>('[aria-label="Add annotation at new line 62"]')!;
    const end = document.querySelector<HTMLButtonElement>('[aria-label="Add annotation at new line 65"]')!;
    start.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await settle();
    const textarea = document.querySelector<HTMLTextAreaElement>('.composer textarea')!;
    textarea.value = 'Keep this feedback while I extend the selection.';
    textarea.dispatchEvent(new Event('input', { bubbles: true }));
    end.dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    await settle();
    expect(document.querySelector<HTMLTextAreaElement>('.composer textarea')?.value).toBe('Keep this feedback while I extend the selection.');
    expect(document.body.textContent).toContain('Draft attached to new lines 62–65');
    expect(document.querySelectorAll('.annotation-gutter[aria-pressed="true"]')).toHaveLength(4);
    await new Promise((resolve) => window.setTimeout(resolve, 10));
    unmount(component);

    component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 40));
    await settle();
    expect(document.querySelector<HTMLTextAreaElement>('.composer textarea')?.value).toBe('Keep this feedback while I extend the selection.');
    expect(document.body.textContent).toContain('Draft attached to new lines 62–65');
    const restoredEnd = document.querySelector<HTMLButtonElement>('[aria-label="Add annotation at new line 68"]')!;
    restoredEnd.dispatchEvent(new MouseEvent('click', { bubbles: true, shiftKey: true }));
    await settle();
    expect(document.querySelector<HTMLTextAreaElement>('.composer textarea')?.value).toBe('Keep this feedback while I extend the selection.');
    expect(document.body.textContent).toContain('Draft attached to new lines 62–68');
    expect(document.querySelectorAll('.annotation-gutter[aria-pressed="true"]')).toHaveLength(7);
    unmount(component);
  });

  it('exposes complete row context to assistive technology without rendering every changed file', async () => {
    const rows: DiffRow[] = [{ id: 'new-8', kind: 'addition', newLine: 8, newText: 'after eight' }];
    let component = mount(VirtualDiff, {
      target: target(),
      props: { rows, mode: 'unified', repositoryName: 'api', filePath: 'src/routes.ts', annotationCountAt: () => 2 }
    });
    await settle();
    expect(document.querySelector('.diff-row')?.getAttribute('aria-label')).toContain('Repository api, file src/routes.ts');
    expect(document.querySelector('.diff-row')?.getAttribute('aria-label')).toContain('new line 8, added change, 2 annotations');
    unmount(component);

    const files = Array.from({ length: 1_000 }, (_, index) => ({
      id: `file-${index}`, repositoryId: 'repo', path: `src/${index}.ts`, status: 'modified' as const,
      additions: index, deletions: 0, hunkCount: 0, language: 'TypeScript', viewed: false, annotationCount: 0
    }));
    const fileTarget = target();
    component = mount(VirtualFileList, {
      target: fileTarget,
      props: {
        files,
        repositories: [{ id: 'repo', name: 'API', path: '/tmp/api', branch: 'feature', base: 'origin/master', mergeBase: 'a', head: 'b' }],
        grouping: 'flat', activeFileId: 'file-0'
      }
    });
    await settle();
    expect(fileTarget.querySelector('.virtual-file-list')?.getAttribute('aria-label')).toBe('1000 changed files');
    expect(fileTarget.querySelectorAll('.file-row').length).toBeLessThan(40);
    expect(fileTarget.querySelectorAll('.file-row[role=treeitem]').length).toBeGreaterThan(0);
    expect(fileTarget.querySelector('.file-row .file-select')?.getAttribute('aria-label')).toContain('src/0.ts, API, modified');
    unmount(component);
  });

  it('moves focus into a dialog and restores the launcher after Escape', async () => {
    const component = mount(App, { target: target() });
    await settle();
    const launcher = document.querySelector<HTMLButtonElement>('.file-picker')!;
    launcher.focus();
    launcher.click();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    expect(document.activeElement?.getAttribute('aria-label')).toBe('Find changed file');
    document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    await settle();
    expect(document.querySelector('dialog')).toBeNull();
    expect(document.activeElement).toBe(launcher);
    unmount(component);
  });

  it('requests bounded viewport windows and keeps a source location while switching modes', async () => {
    const requests: Array<{ startRow: number; endRow: number }> = [];
    const rows: DiffRow[] = Array.from({ length: 120 }, (_, index) => ({
      id: `row-${index}`, kind: 'context', newLine: index + 1, newText: `const line${index} = true;`
    }));
    const component = mount(VirtualDiff, {
      target: target(),
      props: {
        rows: rows.slice(0, 80), windowStart: 0, totalRows: 50_000, mode: 'unified',
        onViewportRequest: (request: { startRow: number; endRow: number }) => requests.push(request)
      }
    });
    await settle();
    const viewport = document.querySelector<HTMLElement>('.diff-viewport')!;
    Object.defineProperty(viewport, 'scrollTop', { configurable: true, value: 12_000, writable: true });
    viewport.dispatchEvent(new Event('scroll'));
    await settle();
    expect(requests.some((request) => request.startRow > 0 && request.endRow - request.startRow < 900)).toBe(true);
    expect(document.querySelectorAll('.diff-row').length).toBeLessThan(100);
    unmount(component);
  });

  it('uses a collapsible repository/folder tree with scaled variable rows', async () => {
    const files = Array.from({ length: 100 }, (_, index) => ({
      id: `tree-${index}`, repositoryId: 'repo', path: `src/features/area-${index % 5}/a-very-long-review-file-name-${index}.ts`, status: 'modified' as const,
      additions: 1, deletions: 1, hunkCount: 0, language: 'TypeScript', viewed: false, annotationCount: 0
    }));
    const host = target();
    const component = mount(VirtualFileList, { target: host, props: { files, repositories: [{ id: 'repo', name: 'API', path: '/tmp/api', branch: 'feature', base: 'origin/main', mergeBase: 'a', head: 'b' }], grouping: 'repository', fontScale: 1.8 } });
    await settle();
    const repository = host.querySelector<HTMLButtonElement>('.file-group-label')!;
    expect(repository.getAttribute('aria-expanded')).toBe('true');
    expect(host.querySelectorAll('.file-row').length).toBeGreaterThan(0);
    repository.click();
    await settle();
    expect(repository.getAttribute('aria-expanded')).toBe('false');
    expect(host.querySelectorAll('.file-row')).toHaveLength(0);
    unmount(component);
  });

  it('shows immutable capture classifications as file badges', async () => {
    const host = target();
    const component = mount(VirtualFileList, { target: host, props: {
      files: [{ id: 'generated-lock', repositoryId: 'repo', path: 'generated/Cargo.lock', status: 'modified', additions: 1, deletions: 1, hunkCount: 0, language: 'TOML', viewed: false, annotationCount: 0, classification: { generated: true, vendored: false, lockfile: true, binary: false, lfsPointer: false, submodule: false } }],
      repositories: [{ id: 'repo', name: 'API', path: '/tmp/api', branch: 'feature', base: 'origin/main', mergeBase: 'a', head: 'b' }], grouping: 'flat'
    } });
    await settle();
    expect(host.textContent).toContain('Generated');
    expect(host.textContent).toContain('Lockfile');
    expect(host.querySelector('.classification-badges')?.getAttribute('aria-label')).toBe('Capture-time file classifications');
    unmount(component);
  });

  it('fails soft when stale persisted data contains duplicate immutable file ids', async () => {
    const host = target();
    const duplicate = { id: 'file-model', repositoryId: 'repo', path: 'src/model.rs', status: 'modified' as const, additions: 1, deletions: 1, hunkCount: 0, language: 'Rust', viewed: false, annotationCount: 0 };
    const component = mount(VirtualFileList, { target: host, props: {
      files: [duplicate, { ...duplicate, path: 'src/model-stale.rs' }],
      repositories: [{ id: 'repo', name: 'API', path: '/tmp/api', branch: 'feature', base: 'origin/main', mergeBase: 'a', head: 'b' }], grouping: 'flat'
    } });
    await settle();
    expect(host.querySelector('.virtual-file-list')?.getAttribute('aria-label')).toBe('1 changed files');
    expect(host.querySelectorAll('.file-row')).toHaveLength(1);
    unmount(component);
  });

  it('renders normalized Difftastic inline and returns using the nearest alignment', async () => {
    const returns: Array<{ side: string; line: number } | undefined> = [];
    const host = target();
    const component = mount(VirtualDiff, { target: host, props: {
      mode: 'difftastic', totalRows: 1,
      difftastic: { status: 'changed', display: 'inline', chunks: [{ rows: [{ old: { lineNumber: 9, text: 'let old = 1;', changedSpans: [] }, new: { lineNumber: 10, text: 'let next = 2;', changedSpans: [] } }] }], alignment: [{ oldLine: 9, newLine: 10 }] },
      onCanonicalMode: (_mode: string, location?: { side: string; line: number }) => returns.push(location)
    } });
    await settle();
    const presentation = host.querySelector('.diff-presentation');
    expect(presentation?.querySelector(':scope > .structural-notice')).not.toBeNull();
    expect(presentation?.querySelector(':scope > .diff-viewport')).not.toBeNull();
    expect(host.querySelector('[data-structural-display="inline"]')?.textContent).toContain('let old = 1;');
    [...host.querySelectorAll<HTMLButtonElement>('.structural-actions button')].find((button) => button.textContent === 'Show Unified')?.click();
    expect(returns.at(-1)).toEqual({ side: 'new', line: 10 });
    unmount(component);
  });

  it('renders paired structural rows as modifications with all Difftastic change classes', async () => {
    const host = target();
    const component = mount(VirtualDiff, { target: host, props: {
      mode: 'difftastic', totalRows: 1,
      difftastic: {
        status: 'changed', display: 'side_by_side',
        chunks: [{ rows: [{
          old: { lineNumber: 4, text: 'call(old)', changedSpans: [{ start: 5, end: 8, highlight: 'normal' }] },
          new: { lineNumber: 4, text: 'call(new)', changedSpans: [{ start: 4, end: 5, highlight: 'delimiter' }, { start: 5, end: 8, highlight: 'normal' }] }
        }] }],
        alignment: [{ oldLine: 4, newLine: 4 }]
      }
    } });
    await settle();
    const row = host.querySelector('.structural-row');
    expect(row?.classList.contains('modified')).toBe(true);
    expect(row?.getAttribute('aria-label')).toContain('modified change');
    expect(row?.querySelector('.difftastic-normal')?.textContent).toBe('old');
    expect(row?.querySelector('.difftastic-delimiter')?.textContent).toBe('(');
    unmount(component);
  });

  it('explains an empty unchanged structural presentation such as a pure rename', async () => {
    const host = target();
    const component = mount(VirtualDiff, { target: host, props: {
      mode: 'difftastic', totalRows: 0,
      difftastic: { status: 'unchanged', display: 'side_by_side', chunks: [], alignment: [] }
    } });
    await settle();
    expect(host.querySelector('.structural-empty')?.textContent).toBe('No structural changes detected by Difftastic.');
    expect(host.querySelector('.structural-notice')?.textContent).toContain('unchanged');
    unmount(component);
  });

  it('keeps a 2MB captured presentation virtual and supports keyboard same-side ranges', async () => {
    const selections: Array<{ side: string; startLine: number; endLine: number }> = [];
    const payload = 'x'.repeat(2048);
    const rows: DiffRow[] = Array.from({ length: 1024 }, (_, index) => ({ id: `large-${index}`, kind: 'context', newLine: index + 1, newText: payload }));
    const host = target();
    const component = mount(VirtualDiff, { target: host, props: { rows, totalRows: rows.length, mode: 'unified', onAnnotate: (_row: DiffRow, selection: { side: string; startLine: number; endLine: number }) => selections.push(selection) } });
    await settle();
    expect(host.querySelectorAll('.diff-row').length).toBeLessThan(100);
    const gutters = host.querySelectorAll<HTMLButtonElement>('.annotation-gutter:not(:disabled)');
    gutters[0].dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    gutters[2].dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }));
    expect(selections.at(-1)).toEqual({ side: 'new', startLine: 1, endLine: 3 });
    gutters[0].focus();
    const viewport = host.querySelector<HTMLElement>('.diff-viewport')!;
    viewport.scrollTop = 12_000;
    viewport.dispatchEvent(new Event('scroll'));
    await settle();
    expect(document.activeElement).toBe(viewport);
    viewport.scrollTop = 0;
    viewport.dispatchEvent(new Event('scroll'));
    await settle();
    expect((document.activeElement as HTMLElement).dataset.line).toBe('1');
    unmount(component);
  });

  it('wraps long diff rows without fixed-height clipping and keeps nowrap as the default', async () => {
    const rows: DiffRow[] = [{ id: 'long-added', kind: 'addition', newLine: 1, newText: `const message = '${'long '.repeat(120)}';` }];
    const nowrapHost = target();
    const nowrap = mount(VirtualDiff, { target: nowrapHost, props: { rows, totalRows: 1, mode: 'unified' } });
    await settle();
    expect(nowrapHost.querySelector('.diff-viewport')?.classList.contains('wrap-lines')).toBe(false);
    expect(nowrapHost.querySelector<HTMLElement>('.diff-row')?.style.height).toBe('20px');
    expect(nowrapHost.querySelector('.diff-row')?.classList.contains('added')).toBe(true);
    unmount(nowrap);

    const wrapHost = target();
    const wrapped = mount(VirtualDiff, { target: wrapHost, props: { rows, totalRows: 1, mode: 'unified', wrapLines: true } });
    await settle();
    expect(wrapHost.querySelector('.diff-viewport')?.classList.contains('wrap-lines')).toBe(true);
    expect(wrapHost.querySelector<HTMLElement>('.diff-row')?.style.height).toBe('');
    expect(wrapHost.querySelector<HTMLElement>('.diff-row')?.style.minHeight).toBe('20px');
    expect(wrapHost.querySelector('.diff-row')?.getAttribute('data-virtual-row')).toBe('0');
    unmount(wrapped);
  });

  it('sizes split columns from the center diff viewport instead of the OS window', async () => {
    const appCss = readFileSync('src/app.css', 'utf8');
    const splitRule = appCss.match(/\.diff-row\.split-row\s*\{[^}]+\}/g)?.at(-1) ?? '';
    expect(splitRule).toContain('var(--split-old-width');
    expect(splitRule).toContain('var(--split-new-width');
    expect(splitRule).not.toContain('100vw');
  });

  it('uses prompt export as the primary local Finish action', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 20));
    await settle();
    const finish = document.querySelector<HTMLButtonElement>('.finish-button')!;
    expect(finish.textContent).toContain('Copy review prompt');
    finish.click();
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    await settle();
    expect(document.querySelector('.prompt-modal')).not.toBeNull();
    expect(document.querySelector('.finish-modal')).toBeNull();
    unmount(component);
  });

  it('renders imported GitHub context separately from local annotations', async () => {
    const component = mount(App, { target: target() });
    await settle();
    await new Promise((resolve) => window.setTimeout(resolve, 30));
    [...document.querySelectorAll<HTMLButtonElement>('[aria-label="Filter workspaces"] button')]
      .find((button) => button.textContent === 'GitHub')?.click();
    await new Promise((resolve) => window.setTimeout(resolve, 50));
    await settle();
    [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')]
      .find((button) => button.textContent?.startsWith('Comments'))?.click();
    await settle();

    const imported = document.querySelector<HTMLElement>('[aria-label="Imported GitHub pull request context"]');
    expect(imported?.textContent).toContain('GITHUB · IMPORTED CONTEXT');
    expect(imported?.textContent).toContain('Imported review threads');
    expect(imported?.textContent).toContain('Imported general conversation');
    expect(document.querySelector('.comment-list')?.textContent).toContain('Could this state live in the review store?');
    unmount(component);
  });
});
