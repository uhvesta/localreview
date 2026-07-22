<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { safeSyntaxSegments } from './syntax';
  import { getVirtualRange } from './virtual';
  import type { DiffMode, DiffRow, DiffSelection, DiffSide, DifftasticPresentation, FullFileSide, HunkLocation, SyntaxTokenSpan, ViewportRequest } from './types';

  /** `rows` is only the currently cached native window, never necessarily a whole file. */
  export let rows: DiffRow[] = [];
  export let windowStart = 0;
  export let totalRows = 0;
  export let hunks: HunkLocation[] = [];
  export let oldTokens: SyntaxTokenSpan[] = [];
  export let newTokens: SyntaxTokenSpan[] = [];
  export let difftastic: DifftasticPresentation | undefined = undefined;
  export let mode: DiffMode = 'unified';
  export let fontScale = 1;
  export let activeLine: number | undefined = undefined;
  /** The active composer remains tied to immutable source coordinates even
   * while this component swaps bounded virtual windows. */
  export let composerSelection: DiffSelection | undefined = undefined;
  export let composerKind = 'comment';
  export let splitRatio = .5;
  export let fullFileSide: FullFileSide = 'new';
  /** Row index requested by next/previous hunk or restored UI state. */
  export let jumpToRow: number | undefined = undefined;
  /** Persisted pixel position for this workspace/file/mode. Applied once per
   * restoration key after the viewport exists. */
  export let initialScrollTop = 0;
  export let restorationKey = '';
  /** Context is passed explicitly rather than inferred from DOM text so each
   * virtual code row remains intelligible to assistive technology. */
  export let repositoryName = 'repository';
  export let filePath = 'file';
  export let annotationCountAt: (row: DiffRow, side: DiffSide) => number = () => 0;
  export let onAnnotate: (row: DiffRow, selection: DiffSelection) => void = () => {};
  export let onViewportRequest: (request: Pick<ViewportRequest, 'startRow' | 'endRow'>) => void = () => {};
  export let onExpandHunk: (hunk: HunkLocation) => void = () => {};
  export let onSplitRatio: (ratio: number) => void = () => {};
  export let onCanonicalMode: (mode: Exclude<DiffMode, 'difftastic'>, location?: { side: DiffSide; line: number }) => void = () => {};
  export let onLocationChange: (location: { line?: number; side?: DiffSide; scrollTop: number }) => void = () => {};

  let viewport: HTMLDivElement;
  let scrollTop = 0;
  let height = 600;
  let resizeObserver: ResizeObserver | undefined;
  let rangeAnchor: { side: DiffSide; line: number } | undefined;
  let selectionMode = mode;
  let draggingSplit = false;
  let lastRequested = '';
  let handledJump: number | undefined;
  let handledRestorationKey: string | undefined;
  let previousRowHeight = Math.round(24 * fontScale);
  let rangeDrag: { side: DiffSide; anchor: number; current: number; row: DiffRow } | undefined;
  let suppressSyntheticClick = false;
  let focusedLocation: { side: DiffSide; line: number } | undefined;

  $: displayedSelection = rangeDrag
    ? {
        side: rangeDrag.side,
        startLine: Math.min(rangeDrag.anchor, rangeDrag.current),
        endLine: Math.max(rangeDrag.anchor, rangeDrag.current)
      } satisfies DiffSelection
    : composerSelection;

  $: structuralRows = (difftastic?.chunks ?? []).flatMap((chunk, chunkIndex) => chunk.rows.map((entry, rowIndex) => ({
    id: `difftastic:${chunkIndex}:${rowIndex}`,
    kind: entry.old && !entry.new ? 'deletion' as const : entry.new && !entry.old ? 'addition' as const : 'context' as const,
    oldLine: entry.old?.lineNumber, newLine: entry.new?.lineNumber, oldText: entry.old?.text, newText: entry.new?.text
  })));
  $: displayRows = (mode === 'difftastic' && structuralRows.length ? structuralRows : rows) as DiffRow[];
  $: effectiveWindowStart = mode === 'difftastic' && structuralRows.length ? (difftastic?.startRow ?? windowStart) : windowStart;
  $: effectiveTotal = mode === 'difftastic' && structuralRows.length ? (difftastic?.totalRows ?? structuralRows.length) : totalRows || rows.length;
  $: rowHeight = Math.round(24 * fontScale);
  $: globalRange = getVirtualRange(effectiveTotal, scrollTop, height, rowHeight, 16);
  $: localStart = Math.max(0, globalRange.start - effectiveWindowStart);
  $: localEnd = Math.max(localStart, Math.min(displayRows.length, globalRange.end - effectiveWindowStart));
  $: visibleRows = displayRows.slice(localStart, localEnd);
  $: if (viewport && focusedLocation && document.activeElement === viewport) {
    const target = visibleRows.find((entry) => (focusedLocation!.side === 'old' ? entry.oldLine : entry.newLine) === focusedLocation!.line);
    if (target) void tick().then(() => viewport.querySelector<HTMLButtonElement>(`[data-side="${focusedLocation!.side}"][data-line="${focusedLocation!.line}"]`)?.focus({ preventScroll: true }));
  }
  $: windowCoversVisible = globalRange.start >= effectiveWindowStart && globalRange.end <= effectiveWindowStart + displayRows.length;
  $: if (viewport && !windowCoversVisible) requestWindow(globalRange.start, globalRange.end);
  $: if (viewport && jumpToRow !== undefined && jumpToRow !== handledJump) {
    handledJump = jumpToRow;
    viewport.scrollTop = Math.max(0, jumpToRow * rowHeight - Math.floor(height / 3));
    scrollTop = viewport.scrollTop;
    requestWindow(Math.max(0, jumpToRow - 20), jumpToRow + 20);
  }
  $: if (viewport && restorationKey && restorationKey !== handledRestorationKey) {
    handledRestorationKey = restorationKey;
    viewport.scrollTop = Math.max(0, initialScrollTop);
    scrollTop = viewport.scrollTop;
    const first = Math.floor(scrollTop / Math.max(1, rowHeight));
    requestWindow(first, first + Math.ceil(height / Math.max(1, rowHeight)));
  }
  // Font zoom changes row and gutter measurements. Preserve the source row at
  // one-third viewport height rather than snapping back to an arbitrary pixel.
  $: if (viewport && rowHeight !== previousRowHeight) {
    const anchorRow = (scrollTop + height / 3) / Math.max(1, previousRowHeight);
    previousRowHeight = rowHeight;
    viewport.scrollTop = Math.max(0, anchorRow * rowHeight - height / 3);
    scrollTop = viewport.scrollTop;
    const first = Math.floor(scrollTop / Math.max(1, rowHeight));
    requestWindow(first, first + Math.ceil(height / Math.max(1, rowHeight)));
  }
  // A range is meaningful only inside one presentation. Changing modes must
  // never carry a stale virtual-row selection into Full File or Split.
  $: if (selectionMode !== mode) { selectionMode = mode; rangeAnchor = undefined; }

  function observe(node: HTMLDivElement) {
    viewport = node;
    if (typeof ResizeObserver === 'undefined') return;
    resizeObserver = new ResizeObserver((entries) => {
      height = entries[0]?.contentRect.height ?? 600;
      requestWindow(globalRange.start, globalRange.end);
    });
    resizeObserver.observe(node);
    return { destroy: () => resizeObserver?.disconnect() };
  }

  function requestWindow(startRow: number, endRow: number) {
    const paddedStart = Math.max(0, startRow - 80);
    const paddedEnd = Math.min(effectiveTotal, endRow + 80);
    const key = `${paddedStart}:${paddedEnd}`;
    if (key === lastRequested) return;
    lastRequested = key;
    onViewportRequest({ startRow: paddedStart, endRow: paddedEnd });
  }

  function onScroll() {
    const focused = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
    const focusedSide = focused?.dataset.side as DiffSide | undefined;
    const focusedLine = Number(focused?.dataset.line);
    if ((focusedSide === 'old' || focusedSide === 'new') && Number.isFinite(focusedLine)) focusedLocation = { side: focusedSide, line: focusedLine };
    scrollTop = viewport.scrollTop;
    requestWindow(globalRange.start, globalRange.end);
    const representative = visibleRows.find((row) => row.newLine || row.oldLine);
    onLocationChange({ line: representative?.newLine ?? representative?.oldLine, side: representative?.newLine ? 'new' : representative?.oldLine ? 'old' : undefined, scrollTop });
    void tick().then(() => {
      if (focusedLocation && !viewport.querySelector(`[data-side="${focusedLocation.side}"][data-line="${focusedLocation.line}"]`)) viewport.focus({ preventScroll: true });
    });
  }

  function lineFor(row: DiffRow) { return row.newLine ?? row.oldLine; }
  function isComposerRange(row: DiffRow, side: DiffSide, selection: DiffSelection | undefined) {
    if (!selection || selection.side !== side) return false;
    const line = side === 'old' ? row.oldLine : row.newLine;
    return line !== undefined && line >= selection.startLine && line <= selection.endLine;
  }
  function changeLabel(row: DiffRow) {
    if (row.kind === 'addition') return 'added change';
    if (row.kind === 'deletion') return 'removed change';
    return 'unchanged context';
  }
  function sideLabel(row: DiffRow, side: DiffSide) {
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!line) return `${side} side has no source line`;
    const annotations = annotationCountAt(row, side);
    return `${side} line ${line}, ${changeLabel(row)}, ${annotations} ${annotations === 1 ? 'annotation' : 'annotations'}`;
  }
  function rowLabel(row: DiffRow, sides: DiffSide[]) {
    return `Repository ${repositoryName}, file ${filePath}, ${mode} diff. ${sides.map((side) => sideLabel(row, side)).join('; ')}.`;
  }
  function code(row: DiffRow, side: DiffSide) { return side === 'old' ? (row.oldText ?? row.text ?? '') : (row.newText ?? row.text ?? ''); }
  function sourceStart(row: DiffRow, side: DiffSide) { return side === 'old' ? row.oldSourceStartByte : row.newSourceStartByte; }
  function tokensFor(side: DiffSide) { return side === 'old' ? oldTokens : newTokens; }
  function structuralSegments(text: string, spans: Array<{ start: number; end: number; highlight: string }> | undefined) {
    const output: Array<{ text: string; class?: string }> = [];
    let cursor = 0;
    for (const span of [...(spans ?? [])].sort((left, right) => left.start - right.start)) {
      if (span.start < cursor || span.end > text.length || span.end <= span.start) continue;
      if (span.start > cursor) output.push({ text: text.slice(cursor, span.start) });
      output.push({ text: text.slice(span.start, span.end), class: `difftastic-${span.highlight}` });
      cursor = span.end;
    }
    if (cursor < text.length) output.push({ text: text.slice(cursor) });
    return output.length ? output : [{ text }];
  }
  function structuralCell(row: DiffRow, side: DiffSide) {
    const index = structuralRows.findIndex((entry) => entry.id === row.id);
    const original = (difftastic?.chunks ?? []).flatMap((chunk) => chunk.rows)[index];
    return side === 'old' ? original?.old : original?.new;
  }
  function anchorLineFor(side: DiffSide) {
    if (rangeAnchor?.side === side) return rangeAnchor.line;
    // A restored draft has durable source coordinates but no transient
    // pointer anchor. Treat its first line as the original anchor so the
    // first Shift-click after reopening extends instead of replacing it.
    if (composerSelection?.side === side) return composerSelection.startLine;
    return undefined;
  }
  function selectionAt(row: DiffRow, side: DiffSide, extend: boolean) {
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!line || mode === 'difftastic') return undefined;
    const anchorLine = anchorLineFor(side);
    const canExtend = extend && anchorLine !== undefined;
    const selection: DiffSelection = canExtend
      ? { side, startLine: Math.min(anchorLine, line), endLine: Math.max(anchorLine, line) }
      : { side, startLine: line, endLine: line };
    return selection;
  }
  function beginRange(row: DiffRow, side: DiffSide, event: PointerEvent) {
    if (event.button !== 0) return;
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!line || mode === 'difftastic') return;
    event.preventDefault();
    const anchor = event.shiftKey ? (anchorLineFor(side) ?? line) : line;
    rangeDrag = { side, anchor, current: line, row };
    if (!event.shiftKey) rangeAnchor = { side, line };
  }
  function extendRange(row: DiffRow, side: DiffSide) {
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!rangeDrag || rangeDrag.side !== side || !line) return;
    rangeDrag = { ...rangeDrag, current: line, row };
  }
  function finishRange() {
    if (!rangeDrag) return;
    const selection = { side: rangeDrag.side, startLine: Math.min(rangeDrag.anchor, rangeDrag.current), endLine: Math.max(rangeDrag.anchor, rangeDrag.current) };
    onAnnotate(rangeDrag.row, selection);
    rangeDrag = undefined;
    suppressSyntheticClick = true;
    // Browsers dispatch the synthesized click in a later event after
    // pointerup. A microtask clears too early and collapses a shifted range
    // back to the clicked line. Keep suppression through that click task.
    window.setTimeout(() => suppressSyntheticClick = false, 0);
  }
  function clickRange(row: DiffRow, side: DiffSide, event: MouseEvent) {
    if (suppressSyntheticClick) return;
    const selection = selectionAt(row, side, event.shiftKey);
    if (!selection) return;
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (line && !event.shiftKey) rangeAnchor = { side, line };
    onAnnotate(row, selection);
  }
  function annotationKey(row: DiffRow, side: DiffSide, event: KeyboardEvent) {
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!line) return;
    focusedLocation = { side, line };
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      const selection = selectionAt(row, side, event.shiftKey);
      if (!selection) return;
      if (!event.shiftKey) rangeAnchor = { side, line };
      onAnnotate(row, selection);
      return;
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      void focusAdjacent(side, line, event.key === 'ArrowDown' ? 1 : -1);
    }
  }
  async function focusAdjacent(side: DiffSide, line: number, direction: number) {
    const candidates = displayRows.filter((entry) => (side === 'old' ? entry.oldLine : entry.newLine) !== undefined);
    const current = candidates.findIndex((entry) => (side === 'old' ? entry.oldLine : entry.newLine) === line);
    const target = candidates[current + direction];
    const targetLine = target && (side === 'old' ? target.oldLine : target.newLine);
    if (!targetLine) {
      requestWindow(direction > 0 ? globalRange.end : Math.max(0, globalRange.start - 40), direction > 0 ? globalRange.end + 40 : globalRange.start);
      viewport.focus({ preventScroll: true });
      return;
    }
    focusedLocation = { side, line: targetLine };
    const localIndex = displayRows.indexOf(target);
    const globalIndex = effectiveWindowStart + localIndex;
    if (globalIndex < globalRange.start || globalIndex >= globalRange.end) viewport.scrollTop = Math.max(0, globalIndex * rowHeight - height / 3);
    await tick();
    viewport.querySelector<HTMLButtonElement>(`[data-side="${side}"][data-line="${targetLine}"]`)?.focus({ preventScroll: true });
  }
  function returnCanonical(next: Exclude<DiffMode, 'difftastic'>) {
    const representative = visibleRows.find((row) => row.newLine || row.oldLine) ?? structuralRows[0];
    const structuralIndex = representative ? structuralRows.findIndex((row) => row.id === representative.id) : -1;
    const aligned = structuralIndex >= 0 ? difftastic?.alignment[structuralIndex] : undefined;
    const side: DiffSide = aligned?.newLine ? 'new' : 'old';
    const line = aligned?.newLine ?? aligned?.oldLine;
    onCanonicalMode(next, line ? { side, line } : undefined);
  }
  function startSplit(event: PointerEvent) {
    if (mode !== 'split') return;
    draggingSplit = true;
    (event.currentTarget as HTMLElement).setPointerCapture?.(event.pointerId);
  }
  function updateSplit(event: PointerEvent) {
    if (!draggingSplit || !viewport) return;
    const rect = viewport.getBoundingClientRect();
    const ratio = Math.max(.25, Math.min(.75, (event.clientX - rect.left) / rect.width));
    onSplitRatio(ratio);
  }
  function stopSplit() { draggingSplit = false; }
  function resizeSplitKey(event: KeyboardEvent) {
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight' && event.key !== 'Home') return;
    event.preventDefault();
    onSplitRatio(event.key === 'Home' ? .5 : Math.max(.25, Math.min(.75, splitRatio + (event.key === 'ArrowRight' ? .05 : -.05))));
  }
  onDestroy(() => resizeObserver?.disconnect());
</script>

<svelte:window on:pointerup={finishRange} on:pointercancel={() => rangeDrag = undefined} />

{#if mode === 'difftastic'}
  <div class="structural-notice" role="status">
    <span class="spark">✦</span>
    <span><strong>Structural diff</strong> · Backend Difftastic adapter · {difftastic?.display === 'inline' ? 'inline' : 'side-by-side'} · Read-only</span>
    {#if difftastic?.fallback}
      <span class="difftastic-fallback">Fallback: {difftastic.fallback.reason}</span>
    {:else}
      <span class="muted">Pinned normalized output. Canonical anchors stay authoritative.</span>
    {/if}
    <span class="structural-actions"><button on:click={() => returnCanonical('unified')}>Show Unified</button><button on:click={() => returnCanonical('split')}>Split</button><button on:click={() => returnCanonical('full')}>Full file</button></span>
  </div>
{/if}

<!-- svelte-ignore a11y_no_noninteractive_tabindex -- the scroll region is intentionally keyboard-focusable. -->
<div
  bind:this={viewport}
  class:structural={mode === 'difftastic'}
  class="diff-viewport"
  use:observe
  on:scroll={onScroll}
  on:pointermove={updateSplit}
  on:pointerup={stopSplit}
  on:pointercancel={stopSplit}
  aria-label={mode === 'difftastic' ? 'Read-only structural code diff' : 'Code diff'}
  role="region"
  tabindex="0"
>
  {#if displayedSelection && mode !== 'difftastic'}
    <div class="inline-composer-anchor" role="status" aria-live="polite">
      <span>{rangeDrag ? 'Selecting' : 'Draft attached to'} {displayedSelection.side} lines {displayedSelection.startLine}{displayedSelection.endLine === displayedSelection.startLine ? '' : `–${displayedSelection.endLine}`} · {composerKind}</span>
    </div>
  {/if}
  <div class="virtual-spacer" style:height={`${effectiveTotal * rowHeight}px`}>
    <div class="virtual-window" style:transform={`translateY(${globalRange.offset}px)`}>
      {#if !windowCoversVisible && effectiveTotal > 0}
        <div class="diff-loading" style:height={`${rowHeight}px`}>Loading captured rows…</div>
      {/if}
      {#each visibleRows as row (row.id)}
        {#if row.kind === 'header'}
          {@const hunk = hunks.find((entry) => entry.id === row.hunkId || entry.id === row.id)}
          <div class="hunk-row" role="group" aria-label={`Repository ${repositoryName}, file ${filePath}, collapsed hunk ${row.hunk ?? ''}`} style:height={`${rowHeight}px`}><span>{row.hunk}</span><button aria-label={`Expand context for ${row.hunk ?? 'hunk'}`} on:click={() => hunk && onExpandHunk(hunk)}>⋯ <span class="visually-hidden">Expand context</span></button></div>
        {:else if mode === 'split'}
          <div class:active={lineFor(row) === activeLine} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class="diff-row split-row" role="group" aria-label={rowLabel(row, ['old', 'new'])} style:height={`${rowHeight}px;--split-ratio:${splitRatio}`}>
            <button class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class="annotation-gutter" data-side="old" data-line={row.oldLine} aria-label={`Add annotation at old line ${row.oldLine ?? ''}`} aria-pressed={isComposerRange(row, 'old', displayedSelection)} disabled={!row.oldLine} on:focus={() => row.oldLine && (focusedLocation = { side: 'old', line: row.oldLine })} on:pointerdown={(event) => beginRange(row, 'old', event)} on:pointerenter={() => extendRange(row, 'old')} on:click={(event) => clickRange(row, 'old', event)} on:keydown={(event) => annotationKey(row, 'old', event)}>+</button>
            <span class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class="line-number">{row.oldLine ?? ''}</span><span class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class="marker">{row.kind === 'deletion' ? '−' : ''}</span>
            <code class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)}>{#each safeSyntaxSegments(code(row, 'old'), sourceStart(row, 'old'), tokensFor('old')) as segment}<span class:syntax-token={segment.class} class={`syntax-${segment.class ?? 'plain'}`}>{segment.text}</span>{/each}</code>
            <!-- svelte-ignore a11y_no_interactive_element_to_noninteractive_role -- separator is deliberately keyboard-operable -->
            <button class="split-divider" role="separator" aria-orientation="vertical" aria-label="Resize split diff" aria-valuemin="25" aria-valuemax="75" aria-valuenow={Math.round(splitRatio * 100)} on:pointerdown={startSplit} on:keydown={resizeSplitKey}></button>
            <button class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class="annotation-gutter" data-side="new" data-line={row.newLine} aria-label={`Add annotation at new line ${row.newLine ?? ''}`} aria-pressed={isComposerRange(row, 'new', displayedSelection)} disabled={!row.newLine} on:focus={() => row.newLine && (focusedLocation = { side: 'new', line: row.newLine })} on:pointerdown={(event) => beginRange(row, 'new', event)} on:pointerenter={() => extendRange(row, 'new')} on:click={(event) => clickRange(row, 'new', event)} on:keydown={(event) => annotationKey(row, 'new', event)}>+</button>
            <span class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class="line-number">{row.newLine ?? ''}</span><span class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class="marker">{row.kind === 'addition' ? '+' : ''}</span>
            <code class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)}>{#each safeSyntaxSegments(code(row, 'new'), sourceStart(row, 'new'), tokensFor('new')) as segment}<span class:syntax-token={segment.class} class={`syntax-${segment.class ?? 'plain'}`}>{segment.text}</span>{/each}</code>
          </div>
        {:else if mode === 'difftastic' && difftastic?.display === 'inline'}
          {@const oldCell = structuralCell(row, 'old')}
          {@const newCell = structuralCell(row, 'new')}
          <div class:active={lineFor(row) === activeLine} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class="diff-row structural-inline-row" data-structural-display="inline" role="group" aria-label={`${rowLabel(row, [row.newLine ? 'new' : 'old'])} Read-only inline structural presentation.`} style:height={`${rowHeight}px`}>
            <span class="annotation-gutter structural-gutter" aria-hidden="true">•</span><span class="line-number old">{row.oldLine ?? ''}</span><span class="line-number new">{row.newLine ?? ''}</span><span class="marker">{row.oldLine && row.newLine ? '↦' : row.newLine ? '+' : '−'}</span><code>{#if oldCell && newCell && oldCell.text !== newCell.text}<span class="structural-before">{#each structuralSegments(oldCell.text, oldCell.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</span><span class="structural-arrow"> → </span>{/if}{#each structuralSegments((newCell ?? oldCell)?.text ?? '', (newCell ?? oldCell)?.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</code>
          </div>
        {:else if mode === 'difftastic'}
          {@const oldCell = structuralCell(row, 'old')}
          {@const newCell = structuralCell(row, 'new')}
          <div class:active={lineFor(row) === activeLine} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class="diff-row split-row structural-row" role="group" aria-label={`${rowLabel(row, ['old', 'new'])} Read-only structural presentation.`} style:height={`${rowHeight}px;--split-ratio:${splitRatio}`}>
            <button class="annotation-gutter" aria-label={`Annotation disabled for structural old line ${row.oldLine ?? ''}`} disabled>•</button><span class="line-number">{row.oldLine ?? ''}</span><span class="marker">{row.kind === 'deletion' ? '−' : ''}</span><code>{#each structuralSegments(code(row, 'old'), oldCell?.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</code><span class="split-divider"></span><button class="annotation-gutter" aria-label={`Annotation disabled for structural new line ${row.newLine ?? ''}`} disabled>•</button><span class="line-number">{row.newLine ?? ''}</span><span class="marker">{row.kind === 'addition' ? '+' : ''}</span><code>{#each structuralSegments(code(row, 'new'), newCell?.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</code>
          </div>
        {:else}
          {@const displaySide = mode === 'full' ? (fullFileSide === 'new' && !row.newLine ? 'old' : fullFileSide === 'old' && !row.oldLine ? 'new' : fullFileSide) : (row.kind === 'deletion' ? 'old' : 'new')}
          <div class:active={lineFor(row) === activeLine} class:composer-range={isComposerRange(row, displaySide, displayedSelection)} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class="diff-row" role="group" aria-label={rowLabel(row, [displaySide])} style:height={`${rowHeight}px`}>
            <button class="annotation-gutter" data-side={displaySide} data-line={displaySide === 'old' ? row.oldLine : row.newLine} aria-label={`Add annotation at ${displaySide} line ${displaySide === 'old' ? row.oldLine ?? '' : row.newLine ?? ''}`} aria-pressed={isComposerRange(row, displaySide, displayedSelection)} disabled={displaySide === 'old' ? !row.oldLine : !row.newLine} on:focus={() => { const line = displaySide === 'old' ? row.oldLine : row.newLine; if (line) focusedLocation = { side: displaySide, line }; }} on:pointerdown={(event) => beginRange(row, displaySide, event)} on:pointerenter={() => extendRange(row, displaySide)} on:click={(event) => clickRange(row, displaySide, event)} on:keydown={(event) => annotationKey(row, displaySide, event)}>+</button>
            <span class="line-number old">{mode === 'full' && displaySide === 'old' ? row.oldLine ?? '' : (mode === 'full' ? '' : row.oldLine ?? '')}</span>
            <span class="line-number new">{mode === 'full' && displaySide === 'old' ? '' : row.newLine ?? ''}</span>
            <span class="marker">{row.kind === 'addition' ? '+' : row.kind === 'deletion' ? '−' : ' '}</span>
            <code>{#each safeSyntaxSegments(code(row, displaySide), sourceStart(row, displaySide), tokensFor(displaySide)) as segment}<span class:syntax-token={segment.class} class={`syntax-${segment.class ?? 'plain'}`}>{segment.text}</span>{/each}</code>
          </div>
        {/if}
      {/each}
    </div>
  </div>
</div>
