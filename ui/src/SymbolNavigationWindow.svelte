<script lang="ts">
  import { tick } from 'svelte';
  import { createReviewApi } from './lib/api';
  import { safeSyntaxSegments, type SafeSyntaxSegment } from './lib/syntax';
  import { isNavigableSymbol, symbolWindowRequest } from './lib/symbolNavigation';
  import type {
    DiffPresentationWindow,
    DiffSide,
    RepositoryFileEntry,
    ReviewApi,
    SymbolNavigationKind,
    SymbolNavigationLocation,
    SymbolNavigationOpenRequest,
    SymbolNavigationResult,
    SymbolSourceView
  } from './lib/types';

  type ResultKind = Exclude<SymbolNavigationKind, 'all'>;
  type LeftMode = ResultKind | 'files';
  type NavigationEntry = {
    repositoryId: string;
    path: string;
    line: number;
    column: number;
    sourceFingerprint: string;
    fileId?: string;
    comparisonId?: string;
    side?: DiffSide;
  };
  type MenuTarget = {
    symbol: string;
    line: number;
    column: number;
    x: number;
    y: number;
  };
  type SourceSegment = SafeSyntaxSegment & { navigable: boolean };

  export let request: SymbolNavigationOpenRequest | undefined = symbolWindowRequest(window.location.search);
  export let api: Pick<
    ReviewApi,
    'querySymbolNavigation' | 'getSymbolSource' | 'getRepositoryFiles' |
    'openRepositorySource' | 'getPresentationWindow'
  > = createReviewApi();

  const SOURCE_PAGE_LINES = 1_000;
  const DIFF_PAGE_ROWS = 2_000;
  const IDENTIFIER_PATTERN = /[#@~]?(?:[\p{L}_$][\p{L}\p{N}_$]*)(?:[!?']|::[\p{L}_$][\p{L}\p{N}_$]*)*/gu;

  let currentSymbol = request?.symbol ?? '';
  let activeKind: ResultKind = request?.initialQuery ?? 'definitions';
  let leftMode: LeftMode = activeKind;
  let definitions: SymbolNavigationLocation[] | undefined;
  let references: SymbolNavigationLocation[] | undefined;
  let diagnostics: string[] = [];
  let truncated = false;
  let loadingKind: ResultKind | undefined;
  let queryError = '';
  let filter = '';
  let selected: SymbolNavigationLocation | undefined;
  let source: SymbolSourceView | undefined;
  let sourceError = '';
  let sourceGeneration = 0;
  let sourceLoading = false;
  let repositoryFiles: RepositoryFileEntry[] | undefined;
  let filesLoading = false;
  let filesError = '';
  let filesTruncated = false;
  let fileDiagnostics: string[] = [];
  let current: NavigationEntry | undefined;
  let history: NavigationEntry[] = [];
  let historyIndex = -1;
  let contextMenu: MenuTarget | undefined;
  let showDiffDecorations = false;
  let diffPresentation: DiffPresentationWindow | undefined;
  let diffLoading = false;
  let diffError = '';
  let diffGeneration = 0;
  let diffStartRow = 0;
  let expandedDeletions = new Set<string>();
  let collapsedAdditions = new Set<string>();
  let resultList: HTMLDivElement;

  $: activeLocations = activeKind === 'definitions' ? definitions : references;
  $: shownLocations = (activeLocations ?? []).filter((location) =>
    `${location.path} ${location.preview} ${location.kind}`.toLowerCase().includes(filter.trim().toLowerCase())
  );
  $: shownFiles = (repositoryFiles ?? []).filter((file) =>
    file.path.toLowerCase().includes(filter.trim().toLowerCase())
  );
  $: if (request && leftMode !== 'files' && activeLocations === undefined && loadingKind !== activeKind) {
    void loadSymbols(activeKind);
  }

  async function loadSymbols(kind: ResultKind, selectFirst = true) {
    if (!request || loadingKind) return;
    loadingKind = kind;
    queryError = '';
    try {
      const result: SymbolNavigationResult = await api.querySymbolNavigation({
        workspaceId: request.workspaceId,
        repositoryId: request.repositoryId,
        comparisonId: request.comparisonId,
        symbol: currentSymbol,
        kind,
        limit: 200
      });
      if (kind === 'definitions') definitions = result.definitions;
      else references = result.references;
      diagnostics = result.diagnostics;
      truncated = result.truncated;
      const first = (kind === 'definitions' ? definitions : references)?.[0];
      if (selectFirst && first) await selectLocation(first);
    } catch (error) {
      queryError = error instanceof Error ? error.message : 'Symbol discovery failed.';
      if (kind === 'definitions') definitions = [];
      else references = [];
    } finally {
      loadingKind = undefined;
    }
  }

  function switchMode(mode: LeftMode) {
    leftMode = mode;
    filter = '';
    contextMenu = undefined;
    if (mode === 'files') {
      if (!repositoryFiles) void loadFiles();
      return;
    }
    activeKind = mode;
  }

  async function beginSymbolNavigation(symbol: string, line: number, column: number, kind: ResultKind) {
    if (!isNavigableSymbol(symbol)) return;
    currentSymbol = symbol;
    activeKind = kind;
    leftMode = kind;
    definitions = undefined;
    references = undefined;
    selected = undefined;
    filter = '';
    contextMenu = undefined;
    await loadSymbols(kind);
  }

  async function loadFiles(searchAll = false) {
    if (!request || filesLoading) return;
    filesLoading = true;
    filesError = '';
    try {
      const result = await api.getRepositoryFiles({
        workspaceId: request.workspaceId,
        repositoryId: request.repositoryId,
        comparisonId: request.comparisonId,
        query: searchAll ? filter.trim() || undefined : undefined,
        limit: 5_000
      });
      repositoryFiles = result.files;
      filesTruncated = result.truncated;
      fileDiagnostics = result.diagnostics;
    } catch (error) {
      filesError = error instanceof Error ? error.message : 'Repository files are unavailable.';
      repositoryFiles = [];
    } finally {
      filesLoading = false;
    }
  }

  async function selectLocation(location: SymbolNavigationLocation, pushHistory = true) {
    if (!request) return;
    selected = location;
    await loadVerifiedSource({
      repositoryId: location.repositoryId,
      path: location.path,
      line: location.line,
      column: location.column,
      sourceFingerprint: location.sourceFingerprint,
      fileId: location.fileId,
      comparisonId: location.comparisonId,
      side: location.side
    }, pushHistory);
  }

  async function openFile(file: RepositoryFileEntry) {
    if (!request) return;
    sourceLoading = true;
    sourceError = '';
    showDiffDecorations = false;
    diffPresentation = undefined;
    const generation = ++sourceGeneration;
    try {
      const next = await api.openRepositorySource({
        workspaceId: request.workspaceId,
        repositoryId: request.repositoryId,
        path: file.path,
        startLine: 1,
        lineCount: SOURCE_PAGE_LINES
      });
      if (generation !== sourceGeneration) return;
      source = next;
      current = {
        repositoryId: request.repositoryId,
        path: file.path,
        line: 1,
        column: 1,
        sourceFingerprint: next.sourceFingerprint,
        fileId: file.fileId,
        comparisonId: file.comparisonId,
        side: file.side
      };
      pushCurrentHistory();
    } catch (error) {
      if (generation === sourceGeneration) sourceError = error instanceof Error ? error.message : 'Source is unavailable.';
    } finally {
      if (generation === sourceGeneration) sourceLoading = false;
    }
  }

  async function loadVerifiedSource(entry: NavigationEntry, pushHistory: boolean) {
    if (!request) return;
    sourceLoading = true;
    sourceError = '';
    showDiffDecorations = false;
    diffPresentation = undefined;
    const generation = ++sourceGeneration;
    const startLine = Math.max(1, entry.line - Math.floor(SOURCE_PAGE_LINES / 3));
    try {
      const next = await api.getSymbolSource({
        workspaceId: request.workspaceId,
        repositoryId: entry.repositoryId,
        path: entry.path,
        expectedFingerprint: entry.sourceFingerprint,
        startLine,
        lineCount: SOURCE_PAGE_LINES
      });
      if (generation !== sourceGeneration) return;
      source = next;
      current = { ...entry, sourceFingerprint: next.sourceFingerprint };
      if (pushHistory) pushCurrentHistory();
      await tick();
      document.querySelector(`[data-source-line="${entry.line}"]`)?.scrollIntoView({ block: 'center' });
    } catch (error) {
      if (generation === sourceGeneration) sourceError = error instanceof Error ? error.message : 'Source changed or is unavailable.';
    } finally {
      if (generation === sourceGeneration) sourceLoading = false;
    }
  }

  function pushCurrentHistory() {
    if (!current) return;
    const previous = history[historyIndex];
    if (previous && previous.path === current.path && previous.line === current.line && previous.column === current.column) return;
    history = [...history.slice(0, historyIndex + 1), { ...current }].slice(-100);
    historyIndex = history.length - 1;
  }

  async function moveHistory(offset: -1 | 1) {
    const nextIndex = historyIndex + offset;
    const entry = history[nextIndex];
    if (!entry) return;
    historyIndex = nextIndex;
    await loadVerifiedSource(entry, false);
  }

  async function loadSourcePage(startLine: number) {
    if (!request || !current) return;
    sourceLoading = true;
    sourceError = '';
    const generation = ++sourceGeneration;
    try {
      const next = await api.getSymbolSource({
        workspaceId: request.workspaceId,
        repositoryId: current.repositoryId,
        path: current.path,
        expectedFingerprint: current.sourceFingerprint,
        startLine: Math.max(1, startLine),
        lineCount: SOURCE_PAGE_LINES
      });
      if (generation === sourceGeneration) source = next;
    } catch (error) {
      if (generation === sourceGeneration) sourceError = error instanceof Error ? error.message : 'Source changed or is unavailable.';
    } finally {
      if (generation === sourceGeneration) sourceLoading = false;
    }
  }

  function sourceSegments(line: string, index: number): SourceSegment[] {
    if (!source) return [{ text: line, start: 0, end: line.length, navigable: false }];
    const syntax = safeSyntaxSegments(line, source.lineStartBytes[index], source.tokens);
    return syntax.flatMap((segment) => {
      const parts: SourceSegment[] = [];
      let cursor = 0;
      for (const match of segment.text.matchAll(IDENTIFIER_PATTERN)) {
        const start = match.index ?? 0;
        if (start > cursor) {
          parts.push({
            text: segment.text.slice(cursor, start),
            class: segment.class,
            start: segment.start + cursor,
            end: segment.start + start,
            navigable: false
          });
        }
        const text = match[0];
        parts.push({
          text,
          class: segment.class,
          start: segment.start + start,
          end: segment.start + start + text.length,
          navigable: isNavigableSymbol(text)
        });
        cursor = start + text.length;
      }
      if (cursor < segment.text.length) {
        parts.push({
          text: segment.text.slice(cursor),
          class: segment.class,
          start: segment.start + cursor,
          end: segment.end,
          navigable: false
        });
      }
      return parts.length ? parts : [{ ...segment, navigable: false }];
    });
  }

  function sourceTokenClick(event: MouseEvent, segment: SourceSegment, line: number) {
    if (!(event.metaKey || event.ctrlKey) || !segment.navigable) return;
    event.preventDefault();
    void beginSymbolNavigation(segment.text, line, segment.start + 1, 'definitions');
  }

  function sourceTokenMenu(event: MouseEvent, segment: SourceSegment, line: number) {
    if (!segment.navigable) return;
    event.preventDefault();
    contextMenu = {
      symbol: segment.text,
      line,
      column: segment.start + 1,
      x: event.clientX,
      y: event.clientY
    };
  }

  async function toggleDiffDecorations() {
    if (!current?.fileId) return;
    showDiffDecorations = !showDiffDecorations;
    contextMenu = undefined;
    if (showDiffDecorations && !diffPresentation) await loadDiff(0);
  }

  async function loadDiff(startRow = diffStartRow) {
    if (!current?.fileId) return;
    diffLoading = true;
    diffError = '';
    const generation = ++diffGeneration;
    try {
      const next = await api.getPresentationWindow({
        fileId: current.fileId,
        comparisonId: current.comparisonId,
        mode: 'full',
        startRow,
        endRow: startRow + DIFF_PAGE_ROWS,
        generation,
        fullFileSide: 'both',
        ephemeralExpandedFullFileDeletionBlocks: [...expandedDeletions],
        ephemeralCollapsedFullFileAdditionBlocks: [...collapsedAdditions]
      });
      if (generation !== diffGeneration) return;
      diffPresentation = next;
      diffStartRow = next.startRow;
    } catch (error) {
      if (generation === diffGeneration) diffError = error instanceof Error ? error.message : 'Review presentation is unavailable.';
    } finally {
      if (generation === diffGeneration) diffLoading = false;
    }
  }

  async function toggleBlock(blockId: string, side: DiffSide, expanded: boolean) {
    if (side === 'old') {
      expandedDeletions = new Set(expandedDeletions);
      if (expanded) expandedDeletions.delete(blockId);
      else expandedDeletions.add(blockId);
    } else {
      collapsedAdditions = new Set(collapsedAdditions);
      if (expanded) collapsedAdditions.add(blockId);
      else collapsedAdditions.delete(blockId);
    }
    await loadDiff(diffStartRow);
  }

  async function setAllBlocks(side: DiffSide, visible: boolean) {
    const blocks = diffPresentation?.omittedBlocks?.filter((block) => block.side === side) ?? [];
    if (side === 'old') {
      expandedDeletions = new Set(expandedDeletions);
      for (const block of blocks) visible ? expandedDeletions.add(block.id) : expandedDeletions.delete(block.id);
    } else {
      collapsedAdditions = new Set(collapsedAdditions);
      for (const block of blocks) visible ? collapsedAdditions.delete(block.id) : collapsedAdditions.add(block.id);
    }
    await loadDiff(diffStartRow);
  }

  function resultKey(location: SymbolNavigationLocation) {
    return `${location.repositoryId}:${location.path}:${location.line}:${location.column}:${location.role}`;
  }

  function previewParts(location: SymbolNavigationLocation) {
    const start = Math.max(0, location.column - 1);
    const width = Math.max(1, location.endColumn - location.column);
    return {
      before: location.preview.slice(0, start),
      symbol: location.preview.slice(start, start + width),
      after: location.preview.slice(start + width)
    };
  }

  function resultKeydown(event: KeyboardEvent, location: SymbolNavigationLocation) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      void selectLocation(location);
      return;
    }
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
    event.preventDefault();
    const buttons = [...resultList.querySelectorAll<HTMLButtonElement>('.symbol-result')];
    const currentIndex = buttons.indexOf(event.currentTarget as HTMLButtonElement);
    buttons[Math.max(0, Math.min(buttons.length - 1, currentIndex + (event.key === 'ArrowDown' ? 1 : -1)))]?.focus();
  }

  async function windowKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') contextMenu = undefined;
    if ((event.metaKey || event.ctrlKey) && event.key === '[') {
      event.preventDefault();
      await moveHistory(-1);
    } else if ((event.metaKey || event.ctrlKey) && event.key === ']') {
      event.preventDefault();
      await moveHistory(1);
    } else if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'f') {
      event.preventDefault();
      await tick();
      document.querySelector<HTMLInputElement>('.navigation-filter')?.focus();
    }
  }
</script>

<svelte:window on:keydown={windowKeydown} on:click={() => contextMenu = undefined} />

<main class="navigation-window">
  {#if !request}
    <section class="empty window-error" role="alert">
      <strong>Code navigation could not open</strong>
      <p>The native window did not provide a valid captured symbol target.</p>
    </section>
  {:else}
    <header class="navigation-titlebar">
      <div class="history-controls" aria-label="Navigation history">
        <button aria-label="Back" title="Back (⌘[)" disabled={historyIndex <= 0} on:click|stopPropagation={() => moveHistory(-1)}>‹</button>
        <button aria-label="Forward" title="Forward (⌘])" disabled={historyIndex < 0 || historyIndex >= history.length - 1} on:click|stopPropagation={() => moveHistory(1)}>›</button>
      </div>
      <div class="navigation-title">
        <span>CODE NAVIGATION</span>
        <strong>{current?.path ?? currentSymbol}</strong>
        <small>{current ? `${current.path}:${current.line}:${current.column}` : `Captured ${request.side} source`}</small>
      </div>
      <label>
        <span>{leftMode === 'files' ? 'Filter repository paths' : 'Filter symbol results'}</span>
        <input class="navigation-filter" bind:value={filter} placeholder={leftMode === 'files' ? 'src/parser, .rs, README…' : 'Path or source preview'} on:keydown={(event) => {
          if (leftMode === 'files' && event.key === 'Enter') void loadFiles(true);
        }} />
        <kbd>⌘F</kbd>
      </label>
      {#if current?.fileId}
        <button
          class="decoration-toggle"
          class:active={showDiffDecorations}
          aria-pressed={showDiffDecorations}
          on:click|stopPropagation={toggleDiffDecorations}
        >{showDiffDecorations ? 'Hide diff decorations' : 'Show review diff'}</button>
      {/if}
    </header>

    <nav class="navigation-tabs" aria-label="Navigation sidebar">
      <button class:active={leftMode === 'definitions'} on:click={() => switchMode('definitions')}>
        Definitions <span>{definitions?.length ?? '—'}</span>
      </button>
      <button class:active={leftMode === 'references'} on:click={() => switchMode('references')}>
        References <span>{references?.length ?? '—'}</span>
      </button>
      <button class:active={leftMode === 'files'} on:click={() => switchMode('files')}>
        Repository <span>{repositoryFiles?.length ?? '—'}</span>
      </button>
      {#if leftMode !== 'files'}<code>{currentSymbol}</code>{/if}
      {#if truncated || filesTruncated}<small>Bounded results</small>{/if}
    </nav>

    <div class="navigation-layout">
      <aside class="navigation-sidebar">
        {#if leftMode === 'files'}
          {#if filesLoading}
            <div class="loading" role="status"><span></span>Reading repository paths…</div>
          {:else if filesError}
            <div class="empty" role="alert"><strong>Files unavailable</strong><p>{filesError}</p><button on:click={() => loadFiles()}>Retry</button></div>
          {:else if shownFiles.length}
            <div class="file-list">
              {#each shownFiles as file (file.path)}
                <button class:selected={current?.path === file.path} on:click={() => openFile(file)}>
                  <span>⌘</span><strong>{file.path.split('/').at(-1)}</strong>
                  <small>{file.path.includes('/') ? file.path.slice(0, file.path.lastIndexOf('/')) : 'repository root'}{file.fileId ? ' · reviewed' : ''}</small>
                </button>
              {/each}
            </div>
            {#if filesTruncated && filter.trim()}
              <button class="search-all" on:click={() => loadFiles(true)}>Search all repository paths for “{filter.trim()}”</button>
            {/if}
          {:else}
            <div class="empty"><strong>No matching files</strong><p>Press Enter to search the bounded repository index.</p></div>
          {/if}
          {#if fileDiagnostics.length}
            <details class="diagnostics"><summary>File-index notes</summary>{#each fileDiagnostics as note}<p>{note}</p>{/each}</details>
          {/if}
        {:else if loadingKind === activeKind}
          <div class="loading" role="status"><span></span>Finding {activeKind} across the repository…</div>
        {:else if queryError}
          <div class="empty" role="alert"><strong>Search unavailable</strong><p>{queryError}</p><button on:click={() => loadSymbols(activeKind)}>Retry</button></div>
        {:else if shownLocations.length}
          <div bind:this={resultList} class="symbol-results" role="listbox" aria-label={`${shownLocations.length} ${activeKind}`}>
            {#each shownLocations as location (resultKey(location))}
              {@const preview = previewParts(location)}
              <button
                class="symbol-result"
                class:selected={selected && resultKey(selected) === resultKey(location)}
                role="option"
                aria-selected={selected && resultKey(selected) === resultKey(location)}
                on:click={() => selectLocation(location)}
                on:keydown={(event) => resultKeydown(event, location)}
              >
                <span><strong>{location.path}</strong><small>{location.kind} · {location.line}:{location.column}</small></span>
                <code>{preview.before}<mark>{preview.symbol || currentSymbol}</mark>{preview.after}</code>
              </button>
            {/each}
          </div>
        {:else}
          <div class="empty"><strong>No {activeKind} found</strong><p>Try the other result kind or browse the repository.</p></div>
        {/if}
        {#if leftMode !== 'files' && diagnostics.length}
          <details class="diagnostics"><summary>Search notes</summary>{#each diagnostics as note}<p>{note}</p>{/each}</details>
        {/if}
      </aside>

      <section class="editor-panel" aria-label="Repository source editor">
        {#if showDiffDecorations && current?.fileId}
          <header class="editor-toolbar diff-toolbar">
            <strong>{current.path}</strong><span>Full File · Both</span>
            <div>
              <button on:click={() => setAllBlocks('new', true)}>Show all additions</button>
              <button on:click={() => setAllBlocks('new', false)}>Hide all additions</button>
              <button on:click={() => setAllBlocks('old', true)}>Show all deletions</button>
              <button on:click={() => setAllBlocks('old', false)}>Hide all deletions</button>
            </div>
          </header>
          {#if diffLoading}
            <div class="loading" role="status"><span></span>Building immutable review presentation…</div>
          {:else if diffError}
            <div class="empty" role="alert"><strong>Review presentation unavailable</strong><p>{diffError}</p><button on:click={() => loadDiff()}>Retry</button></div>
          {:else if diffPresentation}
            {@const presentation = diffPresentation}
            <div class="source-scroll diff-source">
              {#each presentation.rows as row}
                {#if (row.kind === 'deletion_gate' || row.kind === 'addition_gate') && row.omittedBlockId && row.omittedSide}
                  <button
                    class="diff-gate"
                    class:deletion={row.omittedSide === 'old'}
                    class:addition={row.omittedSide === 'new'}
                    aria-expanded={row.omittedExpanded ?? false}
                    on:click={() => toggleBlock(row.omittedBlockId!, row.omittedSide!, row.omittedExpanded ?? false)}
                  >{row.omittedExpanded ? '⌄' : '›'} {row.omittedExpanded ? 'Hide' : 'Show'} {row.omittedCount} {row.omittedSide === 'old' ? 'deleted' : 'added'} lines · {row.omittedSide === 'old' ? 'Base' : 'Current'} {(row.omittedSide === 'old' ? row.oldLine : row.newLine) ?? ''}–{row.omittedEndLine ?? ''}</button>
                {:else}
                  {@const side = row.newLine !== undefined ? 'new' : 'old'}
                  {@const text = side === 'new' ? (row.newText ?? row.text ?? '') : (row.oldText ?? row.text ?? '')}
                  {@const startByte = side === 'new' ? row.newSourceStartByte : row.oldSourceStartByte}
                  {@const tokens = side === 'new' ? presentation.newTokens : presentation.oldTokens}
                  <div class="diff-line" class:addition={row.kind === 'addition'} class:deletion={row.kind === 'deletion'}>
                    <i>{side === 'old' ? (row.oldLine ?? '') : (row.newLine ?? '')}</i>
                    <b aria-hidden="true">{row.kind === 'addition' ? '+' : row.kind === 'deletion' ? '−' : ' '}</b>
                    <code>{#each safeSyntaxSegments(text, startByte, tokens) as segment}<span class={`tok tok-${segment.class ?? 'plain'}`}>{segment.text || ' '}</span>{/each}</code>
                  </div>
                {/if}
              {/each}
              <footer class="page-controls">
                <button disabled={diffStartRow <= 0} on:click={() => loadDiff(Math.max(0, diffStartRow - DIFF_PAGE_ROWS))}>Previous rows</button>
                <span>Rows {diffStartRow + 1}–{Math.min(presentation.totalRows, diffStartRow + presentation.rows.length)} of {presentation.totalRows}</span>
                <button disabled={diffStartRow + presentation.rows.length >= presentation.totalRows} on:click={() => loadDiff(diffStartRow + presentation.rows.length)}>Next rows</button>
              </footer>
            </div>
          {/if}
        {:else if source}
          {@const sourceView = source}
          <header class="editor-toolbar">
            <strong>{sourceView.path}</strong>
            <span>{sourceView.language ?? 'Plain text'} · Lines {sourceView.startLine}–{sourceView.startLine + Math.max(0, sourceView.lines.length - 1)} of {sourceView.totalLines}</span>
            <small>⌘/Ctrl-click a token · right-click for definitions or references</small>
          </header>
          <div class="source-scroll">
            <pre>{#each sourceView.lines as line, index}
              {@const lineNumber = sourceView.startLine + index}
              <span class:target-line={lineNumber === current?.line} data-source-line={lineNumber}>
                <i>{lineNumber}</i><code>{#each sourceSegments(line, index) as segment}
                  {#if segment.navigable}
                    <button
                      class={`source-token tok tok-${segment.class ?? 'plain'}`}
                      title="⌘/Ctrl-click: definition · right-click: more"
                      on:click={(event) => sourceTokenClick(event, segment, lineNumber)}
                      on:contextmenu={(event) => sourceTokenMenu(event, segment, lineNumber)}
                    >{segment.text}</button>
                  {:else}<span class={`tok tok-${segment.class ?? 'plain'}`}>{segment.text || ' '}</span>{/if}
                {/each}</code>
              </span>
            {/each}</pre>
            <footer class="page-controls">
              <button disabled={sourceView.startLine <= 1 || sourceLoading} on:click={() => loadSourcePage(Math.max(1, sourceView.startLine - SOURCE_PAGE_LINES))}>Previous lines</button>
              <span>{sourceLoading ? 'Loading…' : `${sourceView.lines.length.toLocaleString()} lines loaded lazily`}</span>
              <button disabled={sourceView.startLine + sourceView.lines.length > sourceView.totalLines || sourceLoading} on:click={() => loadSourcePage(sourceView.startLine + sourceView.lines.length)}>Next lines</button>
            </footer>
          </div>
        {:else if sourceLoading}
          <div class="loading" role="status"><span></span>Loading verified source…</div>
        {:else if sourceError}
          <div class="empty" role="alert"><strong>Source changed or unavailable</strong><p>{sourceError}</p></div>
        {:else}
          <div class="empty"><strong>Select a symbol or repository file</strong><p>Source is loaded lazily and remains constrained to this repository.</p></div>
        {/if}
      </section>
    </div>
  {/if}

  {#if contextMenu}
    <div class="context-menu" role="menu" tabindex="-1" style={`left:${contextMenu.x}px;top:${contextMenu.y}px`} on:click|stopPropagation on:keydown|stopPropagation>
      <strong>{contextMenu.symbol}</strong>
      <button role="menuitem" on:click={() => beginSymbolNavigation(contextMenu!.symbol, contextMenu!.line, contextMenu!.column, 'definitions')}>Go to definition</button>
      <button role="menuitem" on:click={() => beginSymbolNavigation(contextMenu!.symbol, contextMenu!.line, contextMenu!.column, 'references')}>Find references</button>
    </div>
  {/if}
</main>

<style>
  :global(html), :global(body), :global(#app) { width: 100%; height: 100%; margin: 0; }
  :global(body) { overflow: hidden; background: #0d131b; color: #dbe5f3; font-family: Inter, system-ui, sans-serif; }
  button, input { font: inherit; }
  button { color: inherit; }
  .navigation-window { display: grid; grid-template-rows: auto auto minmax(0, 1fr); height: 100%; background: #0d131b; }
  .navigation-titlebar { display: flex; min-height: 64px; align-items: center; gap: 12px; padding: 10px 14px; border-bottom: 1px solid #293548; background: #131b26; }
  .history-controls { display: flex; }
  .history-controls button { width: 30px; height: 30px; border: 1px solid #35445a; background: #172230; font-size: 20px; }
  .history-controls button:first-child { border-radius: 6px 0 0 6px; }
  .history-controls button:last-child { border-left: 0; border-radius: 0 6px 6px 0; }
  button:disabled { opacity: .38; }
  .navigation-title { display: grid; min-width: 180px; }
  .navigation-title > span { color: #6f90b8; font-size: 8px; font-weight: 800; letter-spacing: .13em; }
  .navigation-title strong { overflow: hidden; max-width: 30vw; color: #d8e8ff; font: 12px ui-monospace, monospace; text-overflow: ellipsis; white-space: nowrap; }
  .navigation-title small { color: #73869e; font-size: 9px; }
  .navigation-titlebar label { position: relative; display: grid; flex: 1; gap: 3px; max-width: 440px; margin-left: auto; color: #91a2b8; font-size: 9px; }
  .navigation-titlebar input { border: 1px solid #35445a; border-radius: 6px; padding: 7px 40px 7px 9px; outline: none; color: #dbe5f3; background: #0d141d; }
  .navigation-titlebar input:focus { border-color: #5589c8; box-shadow: 0 0 0 2px #5589c833; }
  .navigation-titlebar kbd { position: absolute; right: 8px; bottom: 7px; color: #6f8198; font: 10px ui-monospace, monospace; }
  .decoration-toggle { border: 1px solid #45658c; border-radius: 6px; padding: 8px 11px; background: #1c3048; font-size: 10px; font-weight: 700; white-space: nowrap; }
  .decoration-toggle.active { border-color: #78a8df; color: #eef7ff; background: #285889; box-shadow: 0 0 0 2px #4e8dca33; }
  .navigation-tabs { display: flex; align-items: center; gap: 3px; padding: 6px 12px; border-bottom: 1px solid #263345; background: #111923; }
  .navigation-tabs button { border: 1px solid transparent; border-radius: 5px; padding: 5px 9px; color: #8fa1b7; background: transparent; }
  .navigation-tabs button.active { border-color: #36567e; color: #dceaff; background: #1c2c42; }
  .navigation-tabs button span { margin-left: 4px; color: #6f87a3; font-size: 9px; }
  .navigation-tabs > code { margin-left: 8px; color: #8fb7e8; font-size: 10px; }
  .navigation-tabs > small { margin-left: auto; color: #b69763; }
  .navigation-layout { display: grid; grid-template-columns: minmax(270px, 32%) minmax(0, 1fr); min-height: 0; }
  .navigation-sidebar { display: grid; grid-template-rows: minmax(0, 1fr) auto; min-height: 0; border-right: 1px solid #293548; background: #101720; }
  .symbol-results, .file-list { overflow: auto; }
  .symbol-result, .file-list > button { display: grid; width: 100%; gap: 5px; border: 0; border-bottom: 1px solid #222d3c; padding: 9px 12px; background: transparent; text-align: left; }
  .symbol-result:hover, .file-list > button:hover { background: #172231; }
  .symbol-result.selected, .file-list > button.selected { background: #1d3048; box-shadow: inset 3px 0 #5792dc; }
  .symbol-result > span { display: flex; justify-content: space-between; gap: 8px; }
  .symbol-result strong, .file-list strong { overflow: hidden; font-size: 10px; text-overflow: ellipsis; white-space: nowrap; }
  .symbol-result small, .file-list small { color: #7489a2; font-size: 9px; }
  .symbol-result > code { overflow: hidden; color: #9fb0c5; font: 10px ui-monospace, monospace; text-overflow: ellipsis; white-space: pre; }
  .symbol-result mark { border-radius: 2px; color: #e8f2ff; background: #38699b99; }
  .file-list > button { grid-template-columns: auto minmax(0, 1fr); }
  .file-list > button > span { grid-row: 1 / 3; color: #5f7896; }
  .file-list small { grid-column: 2; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .search-all { margin: 8px; border: 1px solid #36567e; border-radius: 5px; padding: 7px; background: #192b40; font-size: 9px; }
  .editor-panel { display: grid; grid-template-rows: auto minmax(0, 1fr); min-width: 0; min-height: 0; background: #0b1118; }
  .editor-toolbar { display: flex; align-items: center; gap: 12px; min-height: 34px; padding: 0 12px; border-bottom: 1px solid #273346; background: #121b27; font-size: 10px; }
  .editor-toolbar strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .editor-toolbar span, .editor-toolbar small { color: #71849c; }
  .editor-toolbar small { margin-left: auto; }
  .diff-toolbar { flex-wrap: wrap; padding-block: 6px; }
  .diff-toolbar div { display: flex; gap: 4px; margin-left: auto; }
  .diff-toolbar button, .page-controls button { border: 1px solid #35475e; border-radius: 4px; padding: 4px 7px; background: #172536; font-size: 9px; }
  .source-scroll { min-height: 0; overflow: auto; }
  .source-scroll pre { min-width: max-content; margin: 0; padding: 7px 0 28px; font: 11px/1.62 ui-monospace, SFMono-Regular, Menlo, monospace; }
  .source-scroll pre > span { display: grid; grid-template-columns: 58px minmax(0, 1fr); min-height: 18px; }
  .source-scroll pre > span.target-line { background: #25466f88; box-shadow: inset 3px 0 #66a5ef; }
  .source-scroll i, .diff-line i { padding-right: 10px; color: #4f647d; font-style: normal; text-align: right; user-select: none; }
  .source-scroll code { padding-right: 20px; white-space: pre; }
  .source-token { display: inline; border: 0; padding: 0; color: inherit; background: transparent; font: inherit; text-align: inherit; }
  .source-token:hover { text-decoration: underline; text-decoration-color: #6da8ec; text-underline-offset: 2px; }
  .diff-source { font: 11px/1.62 ui-monospace, SFMono-Regular, Menlo, monospace; }
  .diff-line { display: grid; grid-template-columns: 58px 20px minmax(0, 1fr); min-width: max-content; min-height: 18px; }
  .diff-line code { min-width: max-content; padding-right: 24px; white-space: pre; }
  .diff-line b { color: #53677e; font-weight: 400; }
  .diff-line.addition { background: #173d2a; box-shadow: inset 3px 0 #43a66d; }
  .diff-line.deletion { background: #48252b; box-shadow: inset 3px 0 #d15d67; }
  .diff-line.addition b { color: #70d99a; }
  .diff-line.deletion b { color: #ef8990; }
  .diff-gate { width: 100%; border: 0; border-block: 1px solid #46333a; padding: 6px 80px; color: #d3a3aa; background: #2a1d22; text-align: left; font: 10px ui-monospace, monospace; }
  .diff-gate.addition { border-color: #284b38; color: #8bd3a8; background: #17271e; }
  .page-controls { position: sticky; bottom: 0; display: flex; justify-content: center; align-items: center; gap: 10px; padding: 7px; border-top: 1px solid #283548; color: #70849b; background: #111a25f2; font: 9px Inter, sans-serif; }
  .loading, .empty { display: grid; place-content: center; min-height: 160px; padding: 24px; color: #8396ad; text-align: center; }
  .loading { display: flex; align-items: center; justify-content: center; gap: 8px; }
  .loading > span { width: 8px; height: 8px; border: 2px solid #41658e; border-top-color: #85b7f0; border-radius: 50%; animation: spin .8s linear infinite; }
  .empty strong { color: #bdccdd; }
  .empty p { max-width: 420px; margin: 6px 0 0; font-size: 10px; line-height: 1.5; }
  .empty button { justify-self: center; margin-top: 9px; border: 1px solid #3e5877; border-radius: 5px; padding: 5px 9px; background: #1e324a; }
  .diagnostics { padding: 7px 11px; border-top: 1px solid #293548; color: #8fa1b7; font-size: 9px; }
  .diagnostics p { margin: 4px 0; }
  .context-menu { position: fixed; z-index: 20; display: grid; min-width: 190px; overflow: hidden; border: 1px solid #40536d; border-radius: 7px; padding: 4px; background: #17212e; box-shadow: 0 12px 35px #000a; }
  .context-menu strong { padding: 6px 8px; color: #7f94ad; font: 9px ui-monospace, monospace; }
  .context-menu button { border: 0; border-radius: 4px; padding: 7px 8px; background: transparent; text-align: left; font-size: 10px; }
  .context-menu button:hover { background: #263a53; }
  .tok-keyword { color: #d189e8; } .tok-string { color: #9fcb85; } .tok-comment { color: #708196; }
  .tok-function { color: #76afe8; } .tok-type, .tok-constructor { color: #e1bd75; }
  .tok-number, .tok-boolean, .tok-constant { color: #d99b79; } .tok-property, .tok-attribute { color: #85c7d2; }
  .tok-operator, .tok-punctuation { color: #9aaec5; } .tok-variable, .tok-module, .tok-tag { color: #c8d6e6; }
  .window-error { height: 100vh; }
  @keyframes spin { to { transform: rotate(360deg); } }
  @media (max-width: 780px) {
    .navigation-titlebar { flex-wrap: wrap; }
    .navigation-titlebar label { order: 3; min-width: 100%; }
    .navigation-layout { grid-template-columns: 1fr; grid-template-rows: minmax(200px, 38%) minmax(0, 1fr); }
    .navigation-sidebar { border-right: 0; border-bottom: 1px solid #293548; }
    .editor-toolbar small { display: none; }
  }
  @media (prefers-reduced-motion: reduce) { .loading > span { animation: none; } }
</style>
