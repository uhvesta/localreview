<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import type { FileGrouping, Repository, ReviewFile } from './types';

  export let files: ReviewFile[] = [];
  export let repositories: Repository[] = [];
  export let grouping: FileGrouping = 'repository';
  export let activeFileId = '';
  export let fontScale = 1;
  /** Monotonic parent actions keep bulk tree controls keyboard-accessible. */
  export let collapseAllToken = 0;
  export let expandAllToken = 0;
  export let onSelect: (fileId: string) => void = () => {};
  export let onToggleViewed: (fileId: string, viewed: boolean) => void = () => {};

  type Entry =
    | { kind: 'group'; id: string; label: string; depth: number; height: number; expanded: boolean }
    | { kind: 'file'; id: string; file: ReviewFile; depth: number; height: number };
  type TreeGroup = { id: string; label: string; groups: Map<string, TreeGroup>; files: ReviewFile[] };

  let viewport: HTMLDivElement;
  let scrollTop = 0;
  let height = 480;
  let observer: ResizeObserver | undefined;
  let collapsedGroups = new Set<string>();
  let focusedFileId: string | undefined;
  let appliedCollapseAllToken = 0;
  let appliedExpandAllToken = 0;

  $: repositoryNames = new Map(repositories.map((repository) => [repository.id, repository.name]));
  $: normalizedFiles = dedupeFileIds(files);
  $: entries = buildEntries(normalizedFiles, grouping, repositoryNames, collapsedGroups, fontScale);
  $: offsets = buildOffsets(entries);
  $: totalHeight = offsets.at(-1) ?? 0;
  $: range = visibleRange(offsets, scrollTop, height, 6);
  $: visible = entries.slice(range.start, range.end);
  $: translateY = offsets[range.start] ?? 0;
  $: if (collapseAllToken !== appliedCollapseAllToken) {
    collapsedGroups = new Set(groupIds(files, grouping));
    appliedCollapseAllToken = collapseAllToken;
  }
  $: if (expandAllToken !== appliedExpandAllToken) {
    collapsedGroups = new Set();
    appliedExpandAllToken = expandAllToken;
  }
  $: if (viewport && focusedFileId && document.activeElement === viewport && visible.some((entry) => entry.kind === 'file' && entry.id === focusedFileId)) {
    void tick().then(() => viewport.querySelector<HTMLButtonElement>(`[data-file-id="${CSS.escape(focusedFileId!)}"]`)?.focus({ preventScroll: true }));
  }

  function group(root: TreeGroup, id: string, label: string) {
    let value = root.groups.get(id);
    if (!value) {
      value = { id, label, groups: new Map(), files: [] };
      root.groups.set(id, value);
    }
    return value;
  }

  function dedupeFileIds(source: ReviewFile[]) {
    const ids = new Set<string>();
    return source.filter((file) => {
      if (ids.has(file.id)) return false;
      ids.add(file.id);
      return true;
    });
  }

  function groupIds(source: ReviewFile[], mode: FileGrouping) {
    if (mode === 'flat') return [];
    const ids = new Set<string>();
    for (const file of source) {
      let prefix = mode === 'repository' ? `repo:${file.repositoryId}` : `folder-repo:${file.repositoryId}`;
      if (mode === 'repository') ids.add(prefix);
      for (const part of file.path.split('/').slice(0, -1)) {
        prefix = `${prefix}/${part}`;
        ids.add(prefix);
      }
    }
    return ids;
  }

  function buildEntries(source: ReviewFile[], mode: FileGrouping, names: Map<string, string>, collapsed: Set<string>, scale: number): Entry[] {
    const result: Entry[] = [];
    const fileHeight = (file: ReviewFile) => {
      // Long paths and large text need a second visual line. Keeping this
      // estimate in the offset table prevents the virtual list from drifting.
      const pathLines = Math.max(1, Math.ceil(file.path.length / Math.max(24, Math.floor(42 / scale))));
      return Math.ceil((58 + Math.min(2, pathLines - 1) * 16) * scale);
    };
    if (mode === 'flat') return source.map((file) => ({ kind: 'file', id: file.id, file, depth: 0, height: fileHeight(file) }));

    const root: TreeGroup = { id: 'root', label: '', groups: new Map(), files: [] };
    const ordered = [...source].sort((left, right) => `${left.repositoryId}/${left.path}`.localeCompare(`${right.repositoryId}/${right.path}`));
    for (const file of ordered) {
      let cursor = root;
      let prefix = mode === 'repository' ? `repo:${file.repositoryId}` : `folder-repo:${file.repositoryId}`;
      if (mode === 'repository') cursor = group(cursor, prefix, names.get(file.repositoryId) ?? file.repositoryId);
      const folders = file.path.split('/').slice(0, -1);
      for (const part of folders) {
        prefix = `${prefix}/${part}`;
        cursor = group(cursor, prefix, part);
      }
      cursor.files.push(file);
    }

    const flatten = (node: TreeGroup, depth: number) => {
      for (const child of node.groups.values()) {
        const expanded = !collapsed.has(child.id);
        result.push({ kind: 'group', id: child.id, label: child.label, depth, height: Math.ceil(30 * scale), expanded });
        if (!expanded) continue;
        flatten(child, depth + 1);
      }
      for (const file of node.files) result.push({ kind: 'file', id: file.id, file, depth, height: fileHeight(file) });
    };
    flatten(root, 0);
    return result;
  }

  function buildOffsets(source: Entry[]) {
    const result = [0];
    for (const entry of source) result.push(result.at(-1)! + entry.height);
    return result;
  }

  function visibleRange(source: number[], top: number, viewportHeight: number, overscan: number) {
    const entryCount = Math.max(0, source.length - 1);
    let start = 0;
    while (start < entryCount && source[start + 1] <= top) start += 1;
    let end = start;
    const bottom = top + viewportHeight;
    while (end < entryCount && source[end] < bottom) end += 1;
    return { start: Math.max(0, start - overscan), end: Math.min(entryCount, end + overscan) };
  }

  function observe(node: HTMLDivElement) {
    viewport = node;
    if (typeof ResizeObserver === 'undefined') return;
    observer = new ResizeObserver((values) => height = values[0]?.contentRect.height ?? 480);
    observer.observe(node);
    return { destroy: () => observer?.disconnect() };
  }

  function onScroll() {
    const focused = document.activeElement instanceof HTMLElement ? document.activeElement.dataset.fileId : undefined;
    if (focused) focusedFileId = focused;
    scrollTop = viewport.scrollTop;
    void tick().then(() => {
      if (focusedFileId && !viewport.querySelector(`[data-file-id="${CSS.escape(focusedFileId)}"]`)) viewport.focus({ preventScroll: true });
    });
  }
  function activate(fileId: string) { onSelect(fileId); }
  function toggleGroup(id: string) {
    const next = new Set(collapsedGroups);
    next.has(id) ? next.delete(id) : next.add(id);
    collapsedGroups = next;
  }
  async function moveFocus(fileId: string, delta: number) {
    const visibleFiles = entries.filter((entry): entry is Extract<Entry, { kind: 'file' }> => entry.kind === 'file');
    const index = visibleFiles.findIndex((entry) => entry.id === fileId);
    const target = visibleFiles[Math.max(0, Math.min(visibleFiles.length - 1, index + delta))];
    if (!target) return;
    activate(target.id);
    focusedFileId = target.id;
    const entryIndex = entries.findIndex((entry) => entry.id === target.id);
    viewport.scrollTop = Math.max(0, (offsets[entryIndex] ?? 0) - height / 3);
    scrollTop = viewport.scrollTop;
    await tick();
    viewport.querySelector<HTMLButtonElement>(`[data-file-id="${CSS.escape(target.id)}"]`)?.focus({ preventScroll: true });
  }
  function onKeydown(event: KeyboardEvent, fileId: string) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      void moveFocus(fileId, event.key === 'ArrowDown' ? 1 : -1);
      return;
    }
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    activate(fileId);
  }

  function classificationBadges(file: ReviewFile) {
    const value = file.classification;
    if (!value) return [];
    return [
      value.generated && ['generated', 'Generated'],
      value.vendored && ['vendored', 'Vendored'],
      value.lockfile && ['lockfile', 'Lockfile'],
      value.binary && ['binary', 'Binary'],
      value.lfsPointer && ['lfs', 'LFS'],
      value.submodule && ['submodule', 'Submodule']
    ].filter((badge): badge is [string, string] => Boolean(badge));
  }

  onDestroy(() => observer?.disconnect());
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex -- scroll container is keyboard reachable. -->
<div bind:this={viewport} class="file-list virtual-file-list" role="tree" aria-label={`${normalizedFiles.length} changed files`} tabindex="0" use:observe on:scroll={onScroll}>
  <div class="virtual-file-spacer" style:height={`${totalHeight}px`}>
    <div class="virtual-file-window" style:transform={`translateY(${translateY}px)`}>
      {#each visible as entry (entry.id)}
        {#if entry.kind === 'group'}
          <button class="file-group-label virtual-group" role="treeitem" aria-selected="false" aria-expanded={entry.expanded} style:height={`${entry.height}px`} style:padding-left={`${8 + entry.depth * 14}px`} on:click={() => toggleGroup(entry.id)}>
            <span class="tree-chevron" aria-hidden="true">{entry.expanded ? '⌄' : '›'}</span><span>{entry.label}</span>
          </button>
        {:else}
          {@const file = entry.file}
          {@const repositoryName = repositoryNames.get(file.repositoryId) ?? file.repositoryId}
          {@const badges = classificationBadges(file)}
          <div class:active={file.id === activeFileId} class="file-row" role="treeitem" aria-selected={file.id === activeFileId} style:height={`${entry.height}px`} style:padding-left={`${entry.depth * 14}px`}>
            <button class="file-select" data-file-id={file.id} aria-label={`${file.path}, ${repositoryName}, ${file.status}, ${file.viewed ? 'viewed' : 'unviewed'}, ${file.annotationCount} annotations${badges.length ? `, ${badges.map((badge) => badge[1]).join(', ')}` : ''}`} on:focus={() => focusedFileId = file.id} on:click={() => activate(file.id)} on:keydown={(event) => onKeydown(event, file.id)}>
              <span class:viewed={file.viewed} class="view-marker" aria-hidden="true"></span>
              <span class="file-info"><span class="file-path">{file.path}</span>{#if file.previousPath}<span class="old-path">{file.previousPath}</span>{/if}<span class="file-repo">{repositoryName}</span>{#if badges.length}<span class="classification-badges" aria-label="Capture-time file classifications">{#each badges as badge (badge[0])}<span class={`classification-badge ${badge[0]}`}>{badge[1]}</span>{/each}</span>{/if}</span>
            </button>
            <div class="file-row-stats"><button class="view-toggle" aria-label={`Mark ${file.path} ${file.viewed ? 'unviewed' : 'viewed'}`} aria-pressed={file.viewed} on:click={() => onToggleViewed(file.id, !file.viewed)}>{file.viewed ? 'Viewed' : 'Mark viewed'}</button><span class="status-chip {file.status}" aria-label={file.status}>{file.status[0].toUpperCase()}</span><span class="additions">+{file.additions}</span>{#if file.deletions}<span class="deletions">−{file.deletions}</span>{/if}{#if file.annotationCount}<span class="annotation-count">● {file.annotationCount}</span>{/if}</div>
          </div>
        {/if}
      {/each}
    </div>
  </div>
</div>
