<script lang="ts">
  import type { Workspace, WorkspaceSource } from './types';

  export let workspaces: Workspace[] = [];
  export let selectedId = '';
  export let collapsed = false;
  export let onSelect: (id: string) => void = () => {};
  export let onOpen: () => void = () => {};
  export let onExpand: () => void = () => {};
  export let onCollapse: () => void = () => {};
  export let onSettings: () => void = () => {};
  export let onDelete: (workspace: Workspace) => void = () => {};
  export let onReconnect: (workspace: Workspace) => void = () => {};
  export let onPin: (workspace: Workspace) => void = () => {};
  export let onRename: (workspace: Workspace) => void = () => {};

  let filter: 'all' | WorkspaceSource = 'all';
  let search = '';
  $: filtered = workspaces.filter((workspace) =>
    (filter === 'all' || workspace.source.includes(filter)) &&
    `${workspace.name} ${workspace.location}`.toLowerCase().includes(search.toLowerCase())
  );
  // A source filter must never leave the central review pointing at a hidden
  // workspace. Workspaces arrive in pinned/MRU order, so the first visible
  // tab is the required most-recent fallback.
  $: if (filtered.length && !filtered.some((workspace) => workspace.id === selectedId)) {
    onSelect(filtered[0].id);
  }

  const sourceIcon: Record<WorkspaceSource, string> = { local: '⌂', github: '⌘', ssh: '↗' };
</script>

<aside class:collapsed class="workspace-rail" aria-label="Workspaces">
  {#if collapsed}
    <button class="rail-icon active" aria-label="Open workspace rail" on:click={onExpand}>◫</button>
  {:else}
    <div class="rail-heading"><span class="wordmark-mark">◈</span><span>LOCALREVIEW</span><button class="icon-button" title="Workspace settings" aria-label="Workspace settings" on:click={onSettings}>⚙</button><button class="icon-button rail-close" title="Close workspace rail" aria-label="Close workspace rail" on:click={onCollapse}>×</button></div>
    <div class="source-filter" role="group" aria-label="Filter workspaces">
      {#each [['all', 'All'], ['github', 'GitHub'], ['local', 'Local'], ['ssh', 'SSH']] as item}
        <button aria-pressed={filter === item[0]} class:active={filter === item[0]} on:click={() => filter = item[0] as typeof filter}>{item[1]}</button>
      {/each}
    </div>
    <label class="search-field"><span>⌕</span><input bind:value={search} placeholder="Search workspaces" aria-label="Search workspaces" /></label>
    <div class="workspace-list" role="tablist" aria-label="Open workspaces">
      {#each filtered as workspace (workspace.id)}
        <div class="workspace-card" class:selected={workspace.id === selectedId}>
          <button class:selected={workspace.id === selectedId} class="workspace-tab" role="tab" aria-selected={workspace.id === selectedId} on:click={() => onSelect(workspace.id)}>
            <div class="workspace-tab-top">
              <span class="workspace-name">{workspace.name}</span>
              {#if workspace.draftCount}<span class="draft-count" aria-label={`${workspace.draftCount} drafts`}>{workspace.draftCount}</span>{/if}
            </div>
            <div class="workspace-location"><span class="source-icon" aria-hidden="true">{sourceIcon[workspace.source[0]]}</span>{workspace.location}</div>
            {#if workspace.detail}<div class="workspace-detail" title={workspace.detail}>{workspace.detail}</div>{/if}
            <div class="workspace-meta">
              <span>{workspace.progress.viewed}/{workspace.progress.total} files</span>
              {#if workspace.refreshAvailable}<span class="refresh-dot">Refresh</span>{/if}
              {#if workspace.connection === 'offline'}<span class="offline-dot">Offline</span>{/if}
              {#if workspace.connection === 'connecting'}<span class="refresh-dot">Connecting</span>{/if}
            </div>
          </button>
          <div class="workspace-card-actions" aria-label={`${workspace.name} actions`}>
            <button class="workspace-action" aria-label={`${workspace.pinned ? 'Unpin' : 'Pin'} ${workspace.name}`} on:click={() => onPin(workspace)}>{workspace.pinned ? 'Unpin' : 'Pin'}</button>
            <button class="workspace-action" aria-label={`Rename ${workspace.name}`} on:click={() => onRename(workspace)}>Rename</button>
            {#if workspace.source.includes('ssh')}
              <button class="workspace-action" aria-label={`Reconnect ${workspace.name}`} on:click={() => onReconnect(workspace)}>Reconnect</button>
            {/if}
            <button class="workspace-action destructive" aria-label={`Remove ${workspace.name} from workspace rail`} on:click={() => onDelete(workspace)}>Remove</button>
          </div>
        </div>
      {/each}
      {#if !filtered.length}<p class="empty-state">No matching workspace.</p>{/if}
    </div>
    <button class="open-workspace" on:click={onOpen}><span>＋</span> Open workspace <kbd>⌘O</kbd></button>
  {/if}
</aside>
