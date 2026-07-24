<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { safeSyntaxSegments, type SafeSyntaxSegment } from './syntax';
  import {
    renderedSymbolToken,
    selectedSymbolToken,
    symbolNavigationRequest,
    type SymbolInteractionContext
  } from './symbolNavigation';
  import { getVirtualRange } from './virtual';
  import type { Annotation, AnnotationKind, DiffMode, DiffRow, DiffSelection, DiffSide, DifftasticPresentation, FullFileSide, HunkLocation, SymbolNavigationOpenRequest, SyntaxTokenSpan, ViewportRequest } from './types';

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
  export let wrapLines = false;
  export let activeLine: number | undefined = undefined;
  /** Immutable source side associated with `activeLine`. Full Current can
   * deliberately target an Old-side deletion gate. */
  export let activeSide: DiffSide | undefined = undefined;
  /** The active composer remains tied to immutable source coordinates even
   * while this component swaps bounded virtual windows. */
  export let composerSelection: DiffSelection | undefined = undefined;
  export let composerKind = 'comment';
  export let splitRatio = .5;
  export let fullFileSide: FullFileSide = 'new';
  /** Row index requested by next/previous hunk or restored UI state. */
  export let jumpToRow: number | undefined = undefined;
  /** Only explicit hunk navigation owns the review highlight. Manual scrolling
   * is deliberately passive so the hot path never scans or transfers hunks. */
  export let activeHunkId: string | undefined = undefined;
  /** Distinguishes repeated jumps to the same row after wrapping or manual scrolling. */
  export let jumpGeneration = 0;
  /** Optional row-top offset captured before a projection swap. */
  export let jumpViewportOffset: number | undefined = undefined;
  /** Persisted pixel position for this workspace/file/mode. Applied once per
   * restoration key after the viewport exists. */
  export let initialScrollTop = 0;
  export let restorationKey = '';
  /** Context is passed explicitly rather than inferred from DOM text so each
   * virtual code row remains intelligible to assistive technology. */
  export let repositoryName = 'repository';
  export let filePath = 'file';
  export let annotationCountAt: (row: DiffRow, side: DiffSide) => number = () => 0;
  /** Complete local annotations whose durable range covers this source row.
   * Thread controls are rendered only on each range's end line so a multi-line
   * annotation highlights every covered row without duplicating its content. */
  export let annotationsForRow: (row: DiffRow, side: DiffSide) => Annotation[] = () => [];
  /** Explicit invalidation for annotations read through the stable callback
   * above. Svelte cannot otherwise observe the callback's mutable review data. */
  export let annotationRevision = '';
  /** Immutable comparison identity for the displayed review generation.
   * Expanded threads must never survive into a newly captured round whose
   * virtual row IDs happen to be reused. */
  export let annotationContextKey = '';
  export let annotationsEditable = true;
  /** Deletion disclosure changes presentation only and remains available in
   * otherwise read-only archived reviews. */
  export let omittedBlocksExpandable = true;
  export let onAnnotate: (row: DiffRow, selection: DiffSelection) => void = () => {};
  export let onEditAnnotation: (annotation: Annotation) => void = () => {};
  export let onViewportRequest: (request: Pick<ViewportRequest, 'startRow' | 'endRow'>) => void = () => {};
  export let onExpandHunk: (hunk: HunkLocation) => void = () => {};
  export let onToggleOmittedBlock: (blockId: string) => void = () => {};
  export let onSplitRatio: (ratio: number) => void = () => {};
  export let onCanonicalMode: (mode: Exclude<DiffMode, 'difftastic'>, location?: { side: DiffSide; line: number }) => void = () => {};
  export let onLocationChange: (location: { line?: number; side?: DiffSide; scrollTop: number }) => void = () => {};
  /** When present, canonical syntax tokens can launch the native symbol
   * navigator. Difftastic remains presentation-only and never supplies source
   * coordinates for this interaction. */
  export let symbolContext: SymbolInteractionContext | undefined = undefined;
  export let onNavigateSymbol: (request: SymbolNavigationOpenRequest) => void = () => {};

  let viewport: HTMLDivElement;
  let virtualWindow: HTMLDivElement;
  let scrollTop = 0;
  let height = 600;
  let viewportWidth = 0;
  let contentWidth = 0;
  let resizeObserver: ResizeObserver | undefined;
  let wrappedHeights = new Map<number, number>();
  let wrappedOffsets: Array<[row: number, cumulativeExtra: number]> = [];
  let wrapContext = '';
  let wrapMeasurementQueued = false;
  let scrollFocusFrame: number | undefined;
  let scrollLocationTimer: number | undefined;
  let rangeAnchor: { side: DiffSide; line: number } | undefined;
  let selectionMode = mode;
  let draggingSplit = false;
  let lastRequested = '';
  let handledJumpGeneration = -1;
  let handledRestorationKey: string | undefined;
  let manualScrollSinceJump = true;
  let lastProgrammaticScrollTop: number | undefined;
  let previousRowHeight = Math.round(20 * fontScale);
  let rangeDrag: { side: DiffSide; anchor: number; current: number; row: DiffRow } | undefined;
  let suppressSyntheticClick = false;
  let focusedLocation: { side: DiffSide; line: number } | undefined;
  let expandedThreadKey: string | undefined;
  let expansionContext = '';
  let symbolMenu: { x: number; y: number; request: SymbolNavigationOpenRequest } | undefined;
  let keyboardSymbol: { side: DiffSide; line: number; column: number } | undefined;
  /** Highlighted source is immutable for one comparison. Keep its already
   * segmented visible lines across disclosure responses so expanding one gate
   * does not rebuild every syntax span in the viewport. */
  let syntaxSegmentCache = new Map<string, SafeSyntaxSegment[]>();
  let syntaxSegmentCacheContext = '';

  $: displayedSelection = rangeDrag
    ? {
        side: rangeDrag.side,
        startLine: Math.min(rangeDrag.anchor, rangeDrag.current),
        endLine: Math.max(rangeDrag.anchor, rangeDrag.current)
      } satisfies DiffSelection
    : composerSelection;

  $: structuralRows = (difftastic?.chunks ?? []).flatMap((chunk, chunkIndex) => chunk.rows.map((entry, rowIndex) => ({
    id: `difftastic:${chunkIndex}:${rowIndex}`,
    kind: entry.old && !entry.new ? 'deletion' as const : entry.new && !entry.old ? 'addition' as const : 'modification' as const,
    oldLine: entry.old?.lineNumber, newLine: entry.new?.lineNumber, oldText: entry.old?.text, newText: entry.new?.text
  })));
  $: displayRows = (mode === 'difftastic' && structuralRows.length ? structuralRows : rows) as DiffRow[];
  $: effectiveWindowStart = mode === 'difftastic' && structuralRows.length ? (difftastic?.startRow ?? windowStart) : windowStart;
  $: effectiveTotal = mode === 'difftastic' && structuralRows.length ? (difftastic?.totalRows ?? structuralRows.length) : totalRows || rows.length;
  $: rowHeight = Math.round(20 * fontScale);
  $: globalRange = wrapLines ? getWrappedRange(effectiveTotal, scrollTop, height, wrappedOffsets) : getVirtualRange(effectiveTotal, scrollTop, height, rowHeight, 16);
  $: virtualHeight = wrapLines ? offsetForRow(effectiveTotal, wrappedOffsets) : effectiveTotal * rowHeight;
  $: virtualOffset = wrapLines ? offsetForRow(globalRange.start, wrappedOffsets) : globalRange.offset;
  $: localStart = Math.max(0, globalRange.start - effectiveWindowStart);
  $: localEnd = Math.max(localStart, Math.min(displayRows.length, globalRange.end - effectiveWindowStart));
  $: visibleRows = displayRows.slice(localStart, localEnd);
  $: orderedHunks = [...hunks].sort((left, right) => left.rowIndex - right.rowIndex);
  $: activeHunkIndex = activeHunkId
    ? orderedHunks.findIndex((hunk) => hunk.id === activeHunkId)
    : -1;
  $: activeHunk = activeHunkIndex < 0 ? undefined : orderedHunks[activeHunkIndex];
  $: activeHunkEndRow = activeHunkIndex < 0
    ? 0
    : (orderedHunks[activeHunkIndex + 1]?.rowIndex ?? effectiveTotal);
  $: rowMeasurementSignature = `${wrapLines}:${globalRange.start}:${globalRange.end}:${visibleRows.map((row) => row.id).join('|')}:${annotationRevision}:${expandedThreadKey ?? ''}:${Math.round(viewportWidth)}:${rowHeight}`;
  $: if (rowMeasurementSignature && virtualWindow) queueRowMeasurement();
  $: renderedWidth = wrapLines ? Math.max(0, viewportWidth) : Math.max(viewportWidth, contentWidth);
  $: if (viewport && focusedLocation && document.activeElement === viewport) {
    const target = visibleRows.find((entry) => (focusedLocation!.side === 'old' ? entry.oldLine : entry.newLine) === focusedLocation!.line);
    if (target) void tick().then(() => viewport.querySelector<HTMLButtonElement>(`[data-side="${focusedLocation!.side}"][data-line="${focusedLocation!.line}"]`)?.focus({ preventScroll: true }));
  }
  $: windowCoversVisible = globalRange.start >= effectiveWindowStart && globalRange.end <= effectiveWindowStart + displayRows.length;
  $: if (viewport && !windowCoversVisible) requestWindow(globalRange.start, globalRange.end);
  $: if (viewport && jumpToRow !== undefined && jumpGeneration !== handledJumpGeneration) {
    handledJumpGeneration = jumpGeneration;
    const offset = jumpViewportOffset === undefined
      ? 0
      : Math.max(0, Math.min(height - rowHeight, jumpViewportOffset));
    viewport.scrollTop = Math.max(0, (wrapLines ? offsetForRow(jumpToRow) : jumpToRow * rowHeight) - offset);
    scrollTop = viewport.scrollTop;
    lastProgrammaticScrollTop = scrollTop;
    manualScrollSinceJump = false;
    onLocationChange({
      line: activeLine,
      side: mode === 'full' && activeLine
        ? (activeSide ?? (fullFileSide === 'old' ? 'old' : 'new'))
        : undefined,
      scrollTop
    });
    requestWindow(Math.max(0, jumpToRow - 20), jumpToRow + 20);
  }
  $: if (viewport && restorationKey && restorationKey !== handledRestorationKey) {
    handledRestorationKey = restorationKey;
    viewport.scrollTop = Math.max(0, initialScrollTop);
    scrollTop = viewport.scrollTop;
    const first = wrapLines ? rowAtOffset(scrollTop) : Math.floor(scrollTop / Math.max(1, rowHeight));
    requestWindow(first, first + Math.ceil(height / Math.max(1, rowHeight)));
  }
  // Font zoom changes row and gutter measurements. Preserve the source row at
  // one-third viewport height rather than snapping back to an arbitrary pixel.
  $: if (viewport && rowHeight !== previousRowHeight) {
    const anchorRow = wrapLines ? rowAtOffset(scrollTop + height / 3) : (scrollTop + height / 3) / Math.max(1, previousRowHeight);
    previousRowHeight = rowHeight;
    wrappedHeights = new Map();
    wrappedOffsets = [];
    viewport.scrollTop = Math.max(0, anchorRow * rowHeight - height / 3);
    scrollTop = viewport.scrollTop;
    const first = Math.floor(scrollTop / Math.max(1, rowHeight));
    requestWindow(first, first + Math.ceil(height / Math.max(1, rowHeight)));
  }
  // A range is meaningful only inside one presentation. Changing modes must
  // never carry a stale virtual-row selection into Full File or Split.
  $: if (selectionMode !== mode) { selectionMode = mode; rangeAnchor = undefined; }
  $: {
    const nextWrapContext = `${wrapLines ? 'wrap' : 'nowrap'}:${annotationContextKey}:${annotationRevision}:${expandedThreadKey ?? ''}:${restorationKey}:${filePath}:${mode}:${fullFileSide}:${fontScale}:${splitRatio}:${Math.round(viewportWidth)}`;
    if (nextWrapContext !== wrapContext) {
      const anchorRow = wrappedHeights.size ? rowAtOffset(scrollTop + height / 3) : Math.floor((scrollTop + height / 3) / Math.max(1, rowHeight));
      wrapContext = nextWrapContext;
      wrappedHeights = new Map();
      wrappedOffsets = [];
      contentWidth = viewportWidth;
      if (viewport) {
        viewport.scrollTop = Math.max(0, anchorRow * rowHeight - height / 3);
        scrollTop = viewport.scrollTop;
      }
    }
  }
  $: {
    const nextExpansionContext = `${annotationContextKey}:${restorationKey}:${filePath}:${mode}`;
    if (nextExpansionContext !== expansionContext) {
      expansionContext = nextExpansionContext;
      expandedThreadKey = undefined;
    }
  }
  $: {
    const nextSyntaxSegmentCacheContext = `${annotationContextKey}:${restorationKey}:${filePath}`;
    if (nextSyntaxSegmentCacheContext !== syntaxSegmentCacheContext) {
      syntaxSegmentCacheContext = nextSyntaxSegmentCacheContext;
      syntaxSegmentCache = new Map();
    }
  }

  function observe(node: HTMLDivElement) {
    viewport = node;
    height = node.clientHeight || 600;
    viewportWidth = node.clientWidth;
    if (typeof ResizeObserver === 'undefined') return;
    resizeObserver = new ResizeObserver((entries) => {
      height = entries[0]?.contentRect.height ?? 600;
      viewportWidth = entries[0]?.contentRect.width ?? node.clientWidth;
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

  export function viewportAnchor(): { side: DiffSide; line: number; viewportOffset: number } | undefined {
    if (!viewport || !virtualWindow) return undefined;
    const viewportRect = viewport.getBoundingClientRect();
    const focalY = viewportRect.top + viewportRect.height / 2;
    let closest: { element: HTMLElement; distance: number } | undefined;
    for (const element of virtualWindow.querySelectorAll<HTMLElement>('[data-virtual-row]')) {
      const rect = element.getBoundingClientRect();
      const distance = Math.abs((rect.top + rect.bottom) / 2 - focalY);
      if (!closest || distance < closest.distance) closest = { element, distance };
    }
    if (!closest) return undefined;
    const rowIndex = Number(closest.element.dataset.virtualRow);
    if (!Number.isSafeInteger(rowIndex)) return undefined;
    const row = displayRows[rowIndex - effectiveWindowStart];
    if (!row) return undefined;
    const viewportOffset = closest.element.getBoundingClientRect().top - viewportRect.top;
    if ((row.kind === 'deletion_gate' || row.kind === 'addition_gate') && row.omittedSide) {
      const start = row.omittedSide === 'old' ? row.oldLine : row.newLine;
      if (!start) return undefined;
      const end = row.omittedEndLine ?? start;
      return {
        side: row.omittedSide,
        line: start + Math.floor((end - start) / 2),
        viewportOffset
      };
    }
    const preferred: DiffSide = mode === 'full'
      ? (fullFileSide === 'old' ? 'old' : fullFileSide === 'both' && row.oldLine && !row.newLine ? 'old' : 'new')
      : (row.newLine ? 'new' : 'old');
    const preferredLine = preferred === 'old' ? row.oldLine : row.newLine;
    if (preferredLine) return { side: preferred, line: preferredLine, viewportOffset };
    if (row.newLine) return { side: 'new', line: row.newLine, viewportOffset };
    if (row.oldLine) return { side: 'old', line: row.oldLine, viewportOffset };
    return undefined;
  }

  /** Read only on an explicit navigation command. Unlike viewportAnchor this
   * requires no DOM query or layout measurement. */
  export function viewportTopRow(): number | undefined {
    if (!viewport) return undefined;
    return wrapLines
      ? rowAtOffset(viewport.scrollTop)
      : Math.max(0, Math.floor(viewport.scrollTop / Math.max(1, rowHeight)));
  }

  /** The selected hunk remains the navigation cursor until an actual manual
   * scroll moves away from the last programmatic landing. */
  export function hunkNavigationCursorRow(): number | undefined {
    if (!manualScrollSinceJump && activeHunk) return activeHunk.rowIndex;
    return viewportTopRow();
  }

  function offsetForRow(index: number, offsets = wrappedOffsets) {
    const bounded = Math.max(0, Math.min(effectiveTotal, index));
    let low = 0;
    let high = offsets.length;
    while (low < high) {
      const middle = Math.floor((low + high) / 2);
      if (offsets[middle][0] < bounded) low = middle + 1;
      else high = middle;
    }
    const extra = low > 0 ? offsets[low - 1][1] : 0;
    return Math.max(0, bounded * rowHeight + extra);
  }

  function rowAtOffset(offset: number, offsets = wrappedOffsets) {
    let low = 0;
    let high = effectiveTotal;
    while (low < high) {
      const middle = Math.floor((low + high) / 2);
      if (offsetForRow(middle + 1, offsets) <= offset) low = middle + 1;
      else high = middle;
    }
    return Math.min(effectiveTotal, low);
  }

  function getWrappedRange(total: number, top: number, viewportHeight: number, offsets = wrappedOffsets) {
    if (total <= 0) return { start: 0, end: 0, offset: 0 };
    const overscan = rowHeight * 16;
    const start = Math.max(0, rowAtOffset(Math.max(0, top - overscan), offsets));
    const end = Math.min(total, Math.max(start + 1, rowAtOffset(top + viewportHeight + overscan, offsets) + 1));
    return { start, end, offset: offsetForRow(start, offsets) };
  }

  function buildWrappedOffsets(heights: Map<number, number>) {
    let cumulative = 0;
    return [...heights.entries()]
      .sort(([left], [right]) => left - right)
      .map(([row, measured]) => {
        cumulative += measured - rowHeight;
        return [row, cumulative] as [number, number];
      });
  }

  function queueRowMeasurement() {
    if (wrapMeasurementQueued) return;
    wrapMeasurementQueued = true;
    void tick().then(() => {
      wrapMeasurementQueued = false;
      if (wrapLines) measureWrappedRows();
      else measureNowrapWidth();
    });
  }

  function measureNowrapWidth() {
    if (!virtualWindow) return;
    // Reading every row's scrollWidth serializes layout once per row in
    // WebKit. The containing virtual window already exposes the maximum
    // overflow width, so one layout read is both equivalent and substantially
    // cheaper after a disclosure swaps the visible row topology.
    const measured = Math.max(viewportWidth, virtualWindow.scrollWidth);
    if (measured > contentWidth) contentWidth = measured;
  }

  function measureWrappedRows() {
    if (!wrapLines || !virtualWindow || !viewport) return;
    const next = new Map(wrappedHeights);
    const anchor = rowAtOffset(scrollTop);
    let deltaAboveAnchor = 0;
    let changed = false;
    for (const element of virtualWindow.querySelectorAll<HTMLElement>('[data-virtual-row]')) {
      const index = Number(element.dataset.virtualRow);
      if (!Number.isFinite(index)) continue;
      if (next.has(index)) continue;
      const measured = Math.max(rowHeight, Math.ceil(element.getBoundingClientRect().height));
      const previous = next.get(index) ?? rowHeight;
      if (Math.abs(measured - previous) < 1) continue;
      next.set(index, measured);
      if (index < anchor) deltaAboveAnchor += measured - previous;
      changed = true;
    }
    if (!changed) return;
    wrappedHeights = next;
    wrappedOffsets = buildWrappedOffsets(next);
    if (deltaAboveAnchor) {
      viewport.scrollTop = Math.max(0, viewport.scrollTop + deltaAboveAnchor);
      scrollTop = viewport.scrollTop;
    }
  }

  function rowStyle(extra = '') {
    return `${wrapLines ? '' : `height:${rowHeight}px;`}min-height:${rowHeight}px;${extra}`;
  }

  function splitRowStyle() {
    const gutters = 180 * fontScale + 7;
    const available = Math.max(0, viewportWidth - gutters);
    return rowStyle(`--split-ratio:${splitRatio};--split-old-width:${available * splitRatio}px;--split-new-width:${available * (1 - splitRatio)}px`);
  }

  function onScroll() {
    const focused = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
    const focusedSide = focused?.dataset.side as DiffSide | undefined;
    const focusedLine = Number(focused?.dataset.line);
    if ((focusedSide === 'old' || focusedSide === 'new') && Number.isFinite(focusedLine)) focusedLocation = { side: focusedSide, line: focusedLine };
    scrollTop = viewport.scrollTop;
    if (lastProgrammaticScrollTop === undefined || Math.abs(scrollTop - lastProgrammaticScrollTop) > 1) {
      manualScrollSinceJump = true;
    }
    // Preserve restart restoration with one trailing pixel-position save. It
    // deliberately carries no source or hunk lookup and performs no parent
    // work during the event burst.
    if (scrollLocationTimer !== undefined) window.clearTimeout(scrollLocationTimer);
    scrollLocationTimer = window.setTimeout(() => {
      scrollLocationTimer = undefined;
      onLocationChange({ scrollTop });
    }, 80);
    // Focus repair is needed only when a code cell, rather than the viewport,
    // actually owned keyboard focus.
    if (focusedLocation && scrollFocusFrame === undefined) {
      scrollFocusFrame = window.requestAnimationFrame(() => {
        scrollFocusFrame = undefined;
        if (focusedLocation && !viewport.querySelector(`[data-side="${focusedLocation.side}"][data-line="${focusedLocation.line}"]`)) {
          viewport.focus({ preventScroll: true });
        }
      });
    }
  }

  function lineFor(row: DiffRow) { return row.newLine ?? row.oldLine; }
  function isActiveHunkChange(row: DiffRow, rowIndex: number) {
    if (!activeHunk || mode === 'difftastic') return false;
    if (rowIndex < activeHunk.rowIndex || rowIndex >= activeHunkEndRow) return false;
    return row.kind === 'addition'
      || row.kind === 'deletion'
      || row.kind === 'modification'
      || row.kind === 'addition_gate'
      || row.kind === 'deletion_gate';
  }
  function activeHunkSuffix(isCurrent: boolean) {
    return isCurrent && activeHunk
      ? ` Current review hunk ${activeHunkIndex + 1} of ${orderedHunks.length}.`
      : '';
  }
  function isComposerRange(row: DiffRow, side: DiffSide, selection: DiffSelection | undefined) {
    if (!selection || selection.side !== side) return false;
    const line = side === 'old' ? row.oldLine : row.newLine;
    return line !== undefined && line >= selection.startLine && line <= selection.endLine;
  }
  function changeLabel(row: DiffRow) {
    if (row.kind === 'addition') return 'added change';
    if (row.kind === 'deletion') return 'removed change';
    if (row.kind === 'modification') return 'modified change';
    return 'unchanged context';
  }
  function sideLabel(row: DiffRow, side: DiffSide, _revision: string) {
    const line = side === 'old' ? row.oldLine : row.newLine;
    if (!line) return `${side} side has no source line`;
    const annotations = annotationCountAt(row, side);
    return `${side} line ${line}, ${changeLabel(row)}, ${annotations} ${annotations === 1 ? 'annotation' : 'annotations'}`;
  }
  function rowLabel(row: DiffRow, sides: DiffSide[], revision = annotationRevision) {
    const inlineRemoval = mode === 'full' && fullFileSide === 'new' && row.kind === 'deletion' && row.oldLine
      ? ' Removed Base line shown inline at its Current-file deletion anchor.'
      : '';
    return `Repository ${repositoryName}, file ${filePath}, ${mode} diff. ${sides.map((side) => sideLabel(row, side, revision)).join('; ')}.${inlineRemoval}`;
  }
  function code(row: DiffRow, side: DiffSide) { return side === 'old' ? (row.oldText ?? row.text ?? '') : (row.newText ?? row.text ?? ''); }
  function sourceStart(row: DiffRow, side: DiffSide) { return side === 'old' ? row.oldSourceStartByte : row.newSourceStartByte; }
  function tokensFor(side: DiffSide) { return side === 'old' ? oldTokens : newTokens; }
  function syntaxSegments(row: DiffRow, side: DiffSide) {
    const text = code(row, side);
    const start = sourceStart(row, side);
    const key = `${side}:${start ?? 'none'}:${text}`;
    const cached = syntaxSegmentCache.get(key);
    if (cached) return cached;
    const segments = safeSyntaxSegments(text, start, tokensFor(side));
    // A review can traverse very large files. Bound the cache while retaining
    // the recently visited windows that make hunk navigation and disclosure
    // toggles instant.
    if (syntaxSegmentCache.size >= 4096) {
      const oldest = syntaxSegmentCache.keys().next().value;
      if (oldest !== undefined) syntaxSegmentCache.delete(oldest);
    }
    syntaxSegmentCache.set(key, segments);
    return segments;
  }
  function navigableSyntaxToken(text: string, start: number, syntaxClass: SyntaxTokenSpan['class'] | undefined) {
    if (syntaxClass && !['attribute', 'constant', 'constructor', 'function', 'module', 'property', 'tag', 'type', 'variable'].includes(syntaxClass)) {
      return undefined;
    }
    return renderedSymbolToken(text, start);
  }
  function elementSymbolToken(element: Element | null) {
    const target = element?.closest<HTMLElement>('[data-symbol][data-symbol-column]');
    const column = Number(target?.dataset.symbolColumn);
    if (!target?.dataset.symbol || !Number.isSafeInteger(column) || column < 1) return undefined;
    return { symbol: target.dataset.symbol, column };
  }
  function symbolTokenFromEvent(event: MouseEvent, code: HTMLElement, side: DiffSide, line: number, keyboardFallback = false) {
    const selected = selectedSymbolToken(window.getSelection(), code);
    if (selected) return selected;
    const pointed = elementSymbolToken(event.target instanceof Element ? event.target : null);
    if (pointed) return pointed;
    if (!keyboardFallback) return undefined;
    const active = keyboardSymbol?.side === side && keyboardSymbol.line === line
      ? code.querySelector<HTMLElement>(`[data-symbol-column="${keyboardSymbol.column}"]`)
      : undefined;
    return elementSymbolToken(active ?? code.querySelector<HTMLElement>('[data-symbol][data-symbol-column]'));
  }
  function symbolRequest(row: DiffRow, side: DiffSide, token: { symbol: string; column: number } | undefined, initialQuery: 'definitions' | 'references') {
    const line = sourceLine(row, side);
    if (!symbolContext || !line || !token || mode === 'difftastic') return undefined;
    return symbolNavigationRequest(symbolContext, token, side, line, initialQuery);
  }
  function symbolClick(node: HTMLElement, row: DiffRow, side: DiffSide, event: MouseEvent) {
    if (!(event.metaKey || event.ctrlKey)) return;
    const line = sourceLine(row, side);
    if (!line) return;
    const request = symbolRequest(row, side, symbolTokenFromEvent(event, node, side, line), 'definitions');
    if (!request) return;
    event.preventDefault();
    event.stopPropagation();
    symbolMenu = undefined;
    onNavigateSymbol(request);
  }
  function showSymbolMenu(request: SymbolNavigationOpenRequest, x: number, y: number) {
    symbolMenu = {
      x: Math.max(8, Math.min(window.innerWidth - 224, x)),
      y: Math.max(8, Math.min(window.innerHeight - 112, y)),
      request
    };
    void tick().then(() => document.querySelector<HTMLButtonElement>('.symbol-context-menu [role="menuitem"]')?.focus());
  }
  function symbolContextMenu(node: HTMLElement, row: DiffRow, side: DiffSide, event: MouseEvent) {
    const line = sourceLine(row, side);
    if (!line) return;
    const keyboardInvocation = event.clientX === 0 && event.clientY === 0;
    const token = symbolTokenFromEvent(event, node, side, line, keyboardInvocation);
    const request = symbolRequest(row, side, token, 'definitions');
    if (!request) return;
    event.preventDefault();
    event.stopPropagation();
    const tokenElement = token
      ? node.querySelector<HTMLElement>(`[data-symbol-column="${token.column}"]`)
      : undefined;
    const rect = tokenElement?.getBoundingClientRect() ?? node.getBoundingClientRect();
    showSymbolMenu(request, keyboardInvocation ? rect.left : event.clientX, keyboardInvocation ? rect.bottom : event.clientY);
  }
  function setKeyboardSymbol(node: HTMLElement, side: DiffSide, line: number, direction: number) {
    const tokens = [...node.querySelectorAll<HTMLElement>('[data-symbol][data-symbol-column]')];
    if (!tokens.length) return;
    const current = keyboardSymbol?.side === side && keyboardSymbol.line === line
      ? tokens.findIndex((token) => Number(token.dataset.symbolColumn) === keyboardSymbol?.column)
      : -1;
    const index = current < 0
      ? (direction < 0 ? tokens.length - 1 : 0)
      : (current + direction + tokens.length) % tokens.length;
    const column = Number(tokens[index].dataset.symbolColumn);
    keyboardSymbol = { side, line, column };
    tokens[index].scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  }
  function symbolKeydown(node: HTMLElement, row: DiffRow, side: DiffSide, event: KeyboardEvent) {
    const line = sourceLine(row, side);
    if (!symbolContext || !line) return;
    if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
      event.preventDefault();
      setKeyboardSymbol(node, side, line, event.key === 'ArrowRight' ? 1 : -1);
      return;
    }
    if (event.key === 'Enter') {
      const token = keyboardSymbol?.side === side && keyboardSymbol.line === line
        ? elementSymbolToken(node.querySelector(`[data-symbol-column="${keyboardSymbol.column}"]`))
        : elementSymbolToken(node.querySelector('[data-symbol][data-symbol-column]'));
      const request = symbolRequest(row, side, token, 'definitions');
      if (!request) return;
      event.preventDefault();
      onNavigateSymbol(request);
      return;
    }
    if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
      const token = keyboardSymbol?.side === side && keyboardSymbol.line === line
        ? elementSymbolToken(node.querySelector(`[data-symbol-column="${keyboardSymbol.column}"]`))
        : elementSymbolToken(node.querySelector('[data-symbol][data-symbol-column]'));
      const request = symbolRequest(row, side, token, 'definitions');
      if (!request) return;
      event.preventDefault();
      const target = token
        ? node.querySelector<HTMLElement>(`[data-symbol-column="${token.column}"]`)
        : node;
      const rect = target?.getBoundingClientRect() ?? node.getBoundingClientRect();
      showSymbolMenu(request, rect.left, rect.bottom);
    }
  }
  function symbolInteractions(node: HTMLElement, initial: { row: DiffRow; side: DiffSide }) {
    let parameters = initial;
    const click = (event: MouseEvent) => symbolClick(node, parameters.row, parameters.side, event);
    const contextmenu = (event: MouseEvent) => symbolContextMenu(node, parameters.row, parameters.side, event);
    const keydown = (event: KeyboardEvent) => symbolKeydown(node, parameters.row, parameters.side, event);
    const focus = () => {
      const line = sourceLine(parameters.row, parameters.side);
      if (symbolContext && line
        && (keyboardSymbol?.side !== parameters.side || keyboardSymbol.line !== line)) {
        setKeyboardSymbol(node, parameters.side, line, 1);
      }
    };
    node.addEventListener('click', click);
    node.addEventListener('contextmenu', contextmenu);
    node.addEventListener('keydown', keydown);
    node.addEventListener('focus', focus);
    return {
      update(next: { row: DiffRow; side: DiffSide }) { parameters = next; },
      destroy() {
        node.removeEventListener('click', click);
        node.removeEventListener('contextmenu', contextmenu);
        node.removeEventListener('keydown', keydown);
        node.removeEventListener('focus', focus);
      }
    };
  }
  function chooseSymbolQuery(initialQuery: 'definitions' | 'references') {
    if (!symbolMenu) return;
    onNavigateSymbol({ ...symbolMenu.request, initialQuery });
    symbolMenu = undefined;
  }
  function closeSymbolMenuOnKey(event: KeyboardEvent) {
    if (event.key === 'Escape') symbolMenu = undefined;
  }
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
  function sourceLine(row: DiffRow, side: DiffSide) { return side === 'old' ? row.oldLine : row.newLine; }
  function currentAnnotations(row: DiffRow, side: DiffSide, _revision: string) {
    return annotationsForRow(row, side);
  }
  function threadKey(row: DiffRow, side: DiffSide) { return `${row.id}:${side}`; }
  function threadId(row: DiffRow, side: DiffSide) { return `inline-thread-${threadKey(row, side).replace(/[^a-zA-Z0-9_-]/g, '-')}`; }
  function anchoredAnnotations(row: DiffRow, side: DiffSide, covered: Annotation[]) {
    const line = sourceLine(row, side);
    return line ? covered.filter((annotation) => annotation.endLine === line) : [];
  }
  function includesKind(annotations: Annotation[], kind: AnnotationKind) { return annotations.some((annotation) => annotation.kind === kind); }
  function annotationGlyph(annotations: Annotation[]) {
    if (includesKind(annotations, 'question')) return '?';
    if (includesKind(annotations, 'suggestion')) return '↗';
    return '●';
  }
  function kindLabel(kind: AnnotationKind) {
    if (kind === 'file_note') return 'File note';
    if (kind === 'review_note') return 'Review note';
    return `${kind.slice(0, 1).toUpperCase()}${kind.slice(1)}`;
  }
  function kindSummary(annotations: Annotation[]) {
    const counts = new Map<AnnotationKind, number>();
    for (const annotation of annotations) counts.set(annotation.kind, (counts.get(annotation.kind) ?? 0) + 1);
    return [...counts.entries()].map(([kind, count]) => `${count} ${kindLabel(kind).toLowerCase()}${count === 1 ? '' : 's'}`).join(', ');
  }
  function threadToggleLabel(row: DiffRow, side: DiffSide, annotations: Annotation[]) {
    const line = sourceLine(row, side);
    const action = expandedThreadKey === threadKey(row, side) ? 'Hide' : 'Show';
    return `${action} ${annotations.length} ${annotations.length === 1 ? 'annotation' : 'annotations'} (${kindSummary(annotations)}) at ${side} line ${line ?? ''}`;
  }
  function annotationRangeLabel(annotation: Annotation) {
    return `${annotation.side} line${annotation.startLine === annotation.endLine ? '' : 's'} ${annotation.startLine}${annotation.endLine === annotation.startLine ? '' : `–${annotation.endLine}`}`;
  }
  function toggleThread(row: DiffRow, side: DiffSide) {
    const key = threadKey(row, side);
    expandedThreadKey = expandedThreadKey === key ? undefined : key;
  }
  function threadNavigationKey(row: DiffRow, side: DiffSide, event: KeyboardEvent) {
    const line = sourceLine(row, side);
    if (!line || (event.key !== 'ArrowDown' && event.key !== 'ArrowUp')) return;
    event.preventDefault();
    void focusAdjacent(side, line, event.key === 'ArrowDown' ? 1 : -1);
  }
  function addInlineAnnotation(row: DiffRow, side: DiffSide) {
    const selection = selectionAt(row, side, false);
    if (!selection) return;
    rangeAnchor = { side, line: selection.startLine };
    expandedThreadKey = undefined;
    onAnnotate(row, selection);
  }
  function editInlineAnnotation(annotation: Annotation) {
    expandedThreadKey = undefined;
    onEditAnnotation(annotation);
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
    if (globalIndex < globalRange.start || globalIndex >= globalRange.end) viewport.scrollTop = Math.max(0, (wrapLines ? offsetForRow(globalIndex) : globalIndex * rowHeight) - height / 3);
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
  onDestroy(() => {
    resizeObserver?.disconnect();
    if (scrollFocusFrame !== undefined) window.cancelAnimationFrame(scrollFocusFrame);
    if (scrollLocationTimer !== undefined) window.clearTimeout(scrollLocationTimer);
  });
</script>

<svelte:window
  on:pointerup={finishRange}
  on:pointercancel={() => rangeDrag = undefined}
  on:click={() => symbolMenu = undefined}
  on:keydown={closeSymbolMenuOnKey}
/>

{#snippet syntaxRow(row: DiffRow, side: DiffSide)}
  {#each syntaxSegments(row, side) as segment}
    {@const token = navigableSyntaxToken(segment.text, segment.start, segment.class)}
    <span
      class:syntax-token={segment.class}
      class:symbol-token={Boolean(symbolContext && token)}
      class:symbol-token-active={Boolean(token && ((symbolMenu
        && symbolMenu.request.side === side
        && symbolMenu.request.line === sourceLine(row, side)
        && symbolMenu.request.column === token.column)
        || (keyboardSymbol
          && keyboardSymbol.side === side
          && keyboardSymbol.line === sourceLine(row, side)
          && keyboardSymbol.column === token.column)))}
      class={`syntax-${segment.class ?? 'plain'}`}
      data-symbol={symbolContext ? token?.symbol : undefined}
      data-symbol-column={symbolContext ? token?.column : undefined}
    >{segment.text}</span>
  {/each}
{/snippet}

{#snippet threadToggle(row: DiffRow, side: DiffSide, annotations: Annotation[], covered: Annotation[])}
  <button
    class="annotation-gutter annotation-thread-toggle"
    class:annotation-range-cell={covered.length > 0}
    class:question-annotation-range-cell={includesKind(covered, 'question')}
    class:question-thread={includesKind(annotations, 'question')}
    class:suggestion-thread={includesKind(annotations, 'suggestion') && !includesKind(annotations, 'question')}
    data-side={side}
    data-line={sourceLine(row, side)}
    aria-label={threadToggleLabel(row, side, annotations)}
    aria-expanded={expandedThreadKey === threadKey(row, side)}
    aria-controls={threadId(row, side)}
    on:focus={() => { const line = sourceLine(row, side); if (line) focusedLocation = { side, line }; }}
    on:click|stopPropagation={() => toggleThread(row, side)}
    on:keydown={(event) => threadNavigationKey(row, side, event)}
  >
    <span class="thread-kind-glyph" aria-hidden="true">{annotationGlyph(annotations)}</span>
    {#if annotations.length > 1}<span class="thread-count" aria-hidden="true">{annotations.length}</span>{/if}
  </button>
{/snippet}

{#snippet threadPanel(row: DiffRow, side: DiffSide, annotations: Annotation[])}
  <aside id={threadId(row, side)} class="inline-thread-popover side-{side}" aria-label={`${annotations.length} inline ${annotations.length === 1 ? 'annotation' : 'annotations'} at ${side} line ${sourceLine(row, side) ?? ''}`}>
    <header>
      <strong>{annotations.length} {annotations.length === 1 ? 'annotation' : 'annotations'}</strong>
      <span>{side} line {sourceLine(row, side)}</span>
      <button class="inline-thread-close" aria-label="Collapse inline annotations" on:click={() => expandedThreadKey = undefined}>×</button>
    </header>
    <div class="inline-thread-items">
      {#each annotations as annotation (annotation.id)}
        <article class="inline-thread-item kind-{annotation.kind}" class:resolved={annotation.state === 'resolved'} class:outdated={annotation.state === 'outdated'}>
          <div class="inline-thread-meta">
            <span class="inline-thread-kind {annotation.kind}">{kindLabel(annotation.kind)}</span>
            <span>{annotationRangeLabel(annotation)}</span>
            {#if annotation.state !== 'open'}<span class="inline-thread-state">{annotation.state}</span>{/if}
            {#if annotation.publishedId}<span class="inline-thread-state published">published</span>{:else if annotation.localOnly}<span class="inline-thread-state">local only</span>{/if}
          </div>
          <p>{annotation.body}</p>
          {#if annotation.labels.length}<div class="inline-thread-labels">{#each annotation.labels as label}<span>{label}</span>{/each}</div>{/if}
          <footer><button disabled={!annotationsEditable} on:click={() => editInlineAnnotation(annotation)}>Edit</button></footer>
        </article>
      {/each}
    </div>
    <footer class="inline-thread-actions"><button disabled={!annotationsEditable} on:click={() => addInlineAnnotation(row, side)}>Add another annotation</button></footer>
  </aside>
{/snippet}

<div class="diff-presentation" class:structural-presentation={mode === 'difftastic'}>
  {#if mode === 'difftastic'}
    <div class="structural-notice" role="status">
      <span class="spark">✦</span>
      <span><strong>Structural diff</strong> · Backend Difftastic adapter · {difftastic?.display === 'inline' ? 'inline' : 'side-by-side'} · {difftastic?.status ?? 'loading'} · Read-only</span>
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
    class:wrap-lines={wrapLines}
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
  {#if activeHunk}
    <div class="visually-hidden" role="status" aria-live="polite">
      Reviewing hunk {activeHunkIndex + 1} of {orderedHunks.length}: {activeHunk.header}
    </div>
  {/if}
  {#if displayedSelection && mode !== 'difftastic'}
    <div class="inline-composer-anchor" role="status" aria-live="polite">
      <span>{rangeDrag ? 'Selecting' : 'Draft attached to'} {displayedSelection.side} lines {displayedSelection.startLine}{displayedSelection.endLine === displayedSelection.startLine ? '' : `–${displayedSelection.endLine}`} · {composerKind}</span>
    </div>
  {/if}
  <div class="virtual-spacer" style:height={`${virtualHeight}px`} style:width={`${renderedWidth}px`}>
    <div bind:this={virtualWindow} class="virtual-window" style:transform={`translateY(${virtualOffset}px)`} style:width={`${renderedWidth}px`}>
      {#if !windowCoversVisible && effectiveTotal > 0}
        <div class="diff-loading" style:height={`${rowHeight}px`}>Loading captured rows…</div>
      {/if}
      {#if mode === 'difftastic' && !difftastic?.fallback && effectiveTotal === 0}
        <div class="structural-empty" role="status">No structural changes detected by Difftastic.</div>
      {/if}
      {#each visibleRows as row, visibleIndex (row.id)}
        {@const virtualRowIndex = effectiveWindowStart + localStart + visibleIndex}
        {@const activeHunkRow = isActiveHunkChange(row, virtualRowIndex)}
        {#if row.kind === 'header'}
          {@const hunk = hunks.find((entry) => entry.id === row.hunkId || entry.id === row.id)}
          <div class="hunk-row" data-virtual-row={virtualRowIndex} role="group" aria-label={`Repository ${repositoryName}, file ${filePath}, collapsed hunk ${row.hunk ?? ''}`} style={rowStyle()}><span>{row.hunk}</span><button aria-label={`Expand context for ${row.hunk ?? 'hunk'}`} on:click={() => hunk && onExpandHunk(hunk)}>⋯ <span class="visually-hidden">Expand context</span></button></div>
        {:else if mode === 'full' && (row.kind === 'deletion_gate' || row.kind === 'addition_gate') && row.omittedBlockId && row.omittedSide}
          {@const omittedStart = row.omittedSide === 'old' ? row.oldLine : row.newLine}
          {@const omittedEnd = row.omittedEndLine ?? omittedStart}
          {@const omittedAction = row.omittedSide === 'old' ? 'deleted' : 'added'}
          {@const omittedSource = row.omittedSide === 'old' ? 'Base' : 'Current'}
          <div class="diff-row" class:active-hunk={activeHunkRow} class:deletion-gate-row={row.omittedSide === 'old'} class:addition-gate-row={row.omittedSide === 'new'} data-virtual-row={virtualRowIndex} data-active-hunk={activeHunkRow ? activeHunk?.id : undefined} class:has-annotation={row.hasAnnotation} class:expanded={row.omittedExpanded} role="group" aria-current={activeHunkRow ? 'location' : undefined} aria-label={`Repository ${repositoryName}, file ${filePath}, Full File ${fullFileSide === 'old' ? 'Base' : fullFileSide === 'new' ? 'Current' : 'Both'} diff. ${row.omittedCount ?? 0} ${omittedAction} ${omittedSource} ${(row.omittedCount ?? 0) === 1 ? 'line' : 'lines'}, lines ${omittedStart ?? ''}${omittedEnd && omittedEnd !== omittedStart ? `–${omittedEnd}` : ''}, ${row.omittedExpanded ? 'expanded' : 'collapsed'}.${row.hasAnnotation ? ' Contains annotations.' : ''}${activeHunkSuffix(activeHunkRow)}`} style={rowStyle()}>
            <span class="annotation-gutter omitted-gate-annotation" aria-hidden="true">{row.hasAnnotation ? '●' : ''}</span>
            <button class:deletion-gate-toggle={row.omittedSide === 'old'} class:addition-gate-toggle={row.omittedSide === 'new'} aria-expanded={row.omittedExpanded ?? false} aria-label={`${row.omittedExpanded ? 'Hide' : 'Show'} ${row.omittedCount ?? 0} ${omittedAction} ${(row.omittedCount ?? 0) === 1 ? 'line' : 'lines'}, ${omittedSource} lines ${omittedStart ?? ''}${omittedEnd && omittedEnd !== omittedStart ? `–${omittedEnd}` : ''}${row.hasAnnotation ? (row.omittedExpanded ? ', contains annotations' : ', annotations hidden in this collapsed range') : ''}`} disabled={!omittedBlocksExpandable} on:click={() => onToggleOmittedBlock(row.omittedBlockId!)}>{row.omittedExpanded ? '⌄' : '›'} {row.omittedCount ?? 0} {omittedAction} {(row.omittedCount ?? 0) === 1 ? 'line' : 'lines'} · {omittedSource} {omittedStart ?? ''}{omittedEnd && omittedEnd !== omittedStart ? `–${omittedEnd}` : ''}{row.hasAnnotation ? (row.omittedExpanded ? ' · contains annotations' : ' · annotations hidden') : ''}</button>
          </div>
        {:else if mode === 'split'}
          {@const oldCovered = currentAnnotations(row, 'old', annotationRevision)}
          {@const newCovered = currentAnnotations(row, 'new', annotationRevision)}
          {@const oldThreads = anchoredAnnotations(row, 'old', oldCovered)}
          {@const newThreads = anchoredAnnotations(row, 'new', newCovered)}
          <div class:active={lineFor(row) === activeLine} class:active-hunk={activeHunkRow} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class:thread-expanded={expandedThreadKey === threadKey(row, 'old') || expandedThreadKey === threadKey(row, 'new')} class="diff-row split-row" data-virtual-row={virtualRowIndex} data-active-hunk={activeHunkRow ? activeHunk?.id : undefined} role="group" aria-current={activeHunkRow ? 'location' : undefined} aria-label={`${rowLabel(row, ['old', 'new'], annotationRevision)}${activeHunkSuffix(activeHunkRow)}`} style={splitRowStyle()}>
            {#if oldThreads.length}
              {@render threadToggle(row, 'old', oldThreads, oldCovered)}
            {:else}
              <button class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class:annotation-range-cell={oldCovered.length > 0} class:question-annotation-range-cell={includesKind(oldCovered, 'question')} class="annotation-gutter" data-side="old" data-line={row.oldLine} aria-label={`Add annotation at old line ${row.oldLine ?? ''}`} aria-pressed={isComposerRange(row, 'old', displayedSelection)} disabled={!row.oldLine} on:focus={() => row.oldLine && (focusedLocation = { side: 'old', line: row.oldLine })} on:pointerdown={(event) => beginRange(row, 'old', event)} on:pointerenter={() => extendRange(row, 'old')} on:click={(event) => clickRange(row, 'old', event)} on:keydown={(event) => annotationKey(row, 'old', event)}>+</button>
            {/if}
            <span class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class:annotation-range-cell={oldCovered.length > 0} class:question-annotation-range-cell={includesKind(oldCovered, 'question')} class="line-number">{row.oldLine ?? ''}</span><span class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class:annotation-range-cell={oldCovered.length > 0} class:question-annotation-range-cell={includesKind(oldCovered, 'question')} class="marker">{row.kind === 'deletion' ? '−' : ''}</span>
            <code class:composer-range-cell-old={isComposerRange(row, 'old', displayedSelection)} class:annotation-range-cell={oldCovered.length > 0} class:question-annotation-range-cell={includesKind(oldCovered, 'question')} use:symbolInteractions={{ row, side: 'old' }} role={symbolContext ? 'group' : undefined} tabindex={symbolContext ? 0 : undefined} aria-label={symbolContext ? `Symbol navigation for old line ${row.oldLine ?? ''}. Use Left and Right arrow keys to choose a symbol, Enter to open its definition, or Shift F10 for more actions.` : undefined}>{@render syntaxRow(row, 'old')}</code>
            <!-- svelte-ignore a11y_no_interactive_element_to_noninteractive_role -- separator is deliberately keyboard-operable -->
            <button class="split-divider" role="separator" aria-orientation="vertical" aria-label="Resize split diff" aria-valuemin="25" aria-valuemax="75" aria-valuenow={Math.round(splitRatio * 100)} on:pointerdown={startSplit} on:keydown={resizeSplitKey}></button>
            {#if newThreads.length}
              {@render threadToggle(row, 'new', newThreads, newCovered)}
            {:else}
              <button class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class:annotation-range-cell={newCovered.length > 0} class:question-annotation-range-cell={includesKind(newCovered, 'question')} class="annotation-gutter" data-side="new" data-line={row.newLine} aria-label={`Add annotation at new line ${row.newLine ?? ''}`} aria-pressed={isComposerRange(row, 'new', displayedSelection)} disabled={!row.newLine} on:focus={() => row.newLine && (focusedLocation = { side: 'new', line: row.newLine })} on:pointerdown={(event) => beginRange(row, 'new', event)} on:pointerenter={() => extendRange(row, 'new')} on:click={(event) => clickRange(row, 'new', event)} on:keydown={(event) => annotationKey(row, 'new', event)}>+</button>
            {/if}
            <span class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class:annotation-range-cell={newCovered.length > 0} class:question-annotation-range-cell={includesKind(newCovered, 'question')} class="line-number">{row.newLine ?? ''}</span><span class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class:annotation-range-cell={newCovered.length > 0} class:question-annotation-range-cell={includesKind(newCovered, 'question')} class="marker">{row.kind === 'addition' ? '+' : ''}</span>
            <code class:composer-range-cell-new={isComposerRange(row, 'new', displayedSelection)} class:annotation-range-cell={newCovered.length > 0} class:question-annotation-range-cell={includesKind(newCovered, 'question')} use:symbolInteractions={{ row, side: 'new' }} role={symbolContext ? 'group' : undefined} tabindex={symbolContext ? 0 : undefined} aria-label={symbolContext ? `Symbol navigation for new line ${row.newLine ?? ''}. Use Left and Right arrow keys to choose a symbol, Enter to open its definition, or Shift F10 for more actions.` : undefined}>{@render syntaxRow(row, 'new')}</code>
            {#if expandedThreadKey === threadKey(row, 'old')}{@render threadPanel(row, 'old', oldThreads)}{/if}
            {#if expandedThreadKey === threadKey(row, 'new')}{@render threadPanel(row, 'new', newThreads)}{/if}
          </div>
        {:else if mode === 'difftastic' && difftastic?.display === 'inline'}
          {@const oldCell = structuralCell(row, 'old')}
          {@const newCell = structuralCell(row, 'new')}
          <div class:active={lineFor(row) === activeLine} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class:modified={row.kind === 'modification'} class="diff-row structural-inline-row" data-structural-display="inline" data-virtual-row={virtualRowIndex} role="group" aria-label={`${rowLabel(row, [row.newLine ? 'new' : 'old'], annotationRevision)} Read-only inline structural presentation.`} style={rowStyle()}>
            <span class="annotation-gutter structural-gutter" aria-hidden="true">•</span><span class="line-number old">{row.oldLine ?? ''}</span><span class="line-number new">{row.newLine ?? ''}</span><span class="marker">{row.oldLine && row.newLine ? '↦' : row.newLine ? '+' : '−'}</span><code>{#if oldCell && newCell && oldCell.text !== newCell.text}<span class="structural-before">{#each structuralSegments(oldCell.text, oldCell.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</span><span class="structural-arrow"> → </span>{/if}{#each structuralSegments((newCell ?? oldCell)?.text ?? '', (newCell ?? oldCell)?.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</code>
          </div>
        {:else if mode === 'difftastic'}
          {@const oldCell = structuralCell(row, 'old')}
          {@const newCell = structuralCell(row, 'new')}
          <div class:active={lineFor(row) === activeLine} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class:modified={row.kind === 'modification'} class="diff-row split-row structural-row" data-virtual-row={virtualRowIndex} role="group" aria-label={`${rowLabel(row, ['old', 'new'], annotationRevision)} Read-only structural presentation.`} style={splitRowStyle()}>
            <button class="annotation-gutter" aria-label={`Annotation disabled for structural old line ${row.oldLine ?? ''}`} disabled>•</button><span class="line-number">{row.oldLine ?? ''}</span><span class="marker">{row.kind === 'deletion' ? '−' : ''}</span><code>{#each structuralSegments(code(row, 'old'), oldCell?.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</code><span class="split-divider"></span><button class="annotation-gutter" aria-label={`Annotation disabled for structural new line ${row.newLine ?? ''}`} disabled>•</button><span class="line-number">{row.newLine ?? ''}</span><span class="marker">{row.kind === 'addition' ? '+' : ''}</span><code>{#each structuralSegments(code(row, 'new'), newCell?.changedSpans) as segment}<span class={segment.class}>{segment.text}</span>{/each}</code>
          </div>
        {:else}
          {@const displaySide: DiffSide = mode === 'full'
            ? (row.oldLine && !row.newLine ? 'old' : 'new')
            : (row.kind === 'deletion' ? 'old' : 'new')}
          {@const covered = currentAnnotations(row, displaySide, annotationRevision)}
          {@const threads = anchoredAnnotations(row, displaySide, covered)}
          <div class:active={lineFor(row) === activeLine} class:active-hunk={activeHunkRow} class:composer-range={isComposerRange(row, displaySide, displayedSelection)} class:annotation-range={covered.length > 0} class:question-annotation-range={includesKind(covered, 'question')} class:thread-expanded={expandedThreadKey === threadKey(row, displaySide)} class:added={row.kind === 'addition'} class:removed={row.kind === 'deletion'} class="diff-row" data-virtual-row={virtualRowIndex} data-active-hunk={activeHunkRow ? activeHunk?.id : undefined} role="group" aria-current={activeHunkRow ? 'location' : undefined} aria-label={`${rowLabel(row, [displaySide], annotationRevision)}${activeHunkSuffix(activeHunkRow)}`} style={rowStyle()}>
            {#if threads.length}
              {@render threadToggle(row, displaySide, threads, covered)}
            {:else}
              <button class="annotation-gutter" data-side={displaySide} data-line={displaySide === 'old' ? row.oldLine : row.newLine} aria-label={`Add annotation at ${displaySide} line ${displaySide === 'old' ? row.oldLine ?? '' : row.newLine ?? ''}`} aria-pressed={isComposerRange(row, displaySide, displayedSelection)} disabled={displaySide === 'old' ? !row.oldLine : !row.newLine} on:focus={() => { const line = displaySide === 'old' ? row.oldLine : row.newLine; if (line) focusedLocation = { side: displaySide, line }; }} on:pointerdown={(event) => beginRange(row, displaySide, event)} on:pointerenter={() => extendRange(row, displaySide)} on:click={(event) => clickRange(row, displaySide, event)} on:keydown={(event) => annotationKey(row, displaySide, event)}>+</button>
            {/if}
            <span class="line-number old">{mode === 'full' && displaySide === 'old' ? row.oldLine ?? '' : (mode === 'full' ? '' : row.oldLine ?? '')}</span>
            <span class="line-number new">{mode === 'full' && displaySide === 'old' ? '' : row.newLine ?? ''}</span>
            <span class="marker">{row.kind === 'addition' ? '+' : row.kind === 'deletion' ? '−' : ' '}</span>
            <code use:symbolInteractions={{ row, side: displaySide }} role={symbolContext ? 'group' : undefined} tabindex={symbolContext ? 0 : undefined} aria-label={symbolContext ? `Symbol navigation for ${displaySide} line ${sourceLine(row, displaySide) ?? ''}. Use Left and Right arrow keys to choose a symbol, Enter to open its definition, or Shift F10 for more actions.` : undefined}>{@render syntaxRow(row, displaySide)}</code>
            {#if expandedThreadKey === threadKey(row, displaySide)}{@render threadPanel(row, displaySide, threads)}{/if}
          </div>
        {/if}
      {/each}
    </div>
  </div>
  </div>
</div>

{#if symbolMenu}
  <div
    class="symbol-context-menu"
    role="menu"
    aria-label={`Symbol actions for ${symbolMenu.request.symbol}`}
    tabindex="-1"
    style:left={`${symbolMenu.x}px`}
    style:top={`${symbolMenu.y}px`}
  >
    <div class="symbol-context-heading">
      <span>Symbol</span>
      <code>{symbolMenu.request.symbol}</code>
    </div>
    <button role="menuitem" on:click={() => chooseSymbolQuery('definitions')}>
      <span>Go to definition</span><kbd>⌘ click</kbd>
    </button>
    <button role="menuitem" on:click={() => chooseSymbolQuery('references')}>
      <span>Find references</span>
    </button>
  </div>
{/if}

<style>
  :global(.symbol-token) {
    border-radius: 2px;
    cursor: pointer;
    text-decoration: underline;
    text-decoration-color: transparent;
    text-underline-offset: 3px;
    transition: background-color 80ms ease, text-decoration-color 80ms ease;
  }

  :global(.symbol-token:hover),
  :global(.symbol-token-active),
  :global(.symbol-token:focus-visible) {
    outline: none;
    background: color-mix(in srgb, #6ea8ff 22%, transparent);
    text-decoration-color: #78aef8;
  }

  .symbol-context-menu {
    position: fixed;
    z-index: 1000;
    width: 216px;
    overflow: hidden;
    border: 1px solid #44536a;
    border-radius: 7px;
    background: #17202c;
    box-shadow: 0 12px 32px #0009;
    color: #dbe6f5;
    font: 12px Inter, system-ui, sans-serif;
  }

  .symbol-context-heading {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 8px 10px 6px;
    border-bottom: 1px solid #2c394b;
    color: #8fa2ba;
  }

  .symbol-context-heading code {
    overflow: hidden;
    color: #dbe6f5;
    font: 600 12px ui-monospace, monospace;
    text-overflow: ellipsis;
  }

  .symbol-context-menu button {
    display: flex;
    width: 100%;
    align-items: center;
    justify-content: space-between;
    border: 0;
    padding: 8px 10px;
    color: inherit;
    background: transparent;
    font: inherit;
    text-align: left;
  }

  .symbol-context-menu button:hover,
  .symbol-context-menu button:focus-visible {
    outline: none;
    background: #27374c;
  }

  .symbol-context-menu kbd {
    color: #8194ad;
    font: 10px ui-monospace, monospace;
  }
</style>
