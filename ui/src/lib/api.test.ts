import { beforeEach, describe, expect, it, vi } from 'vitest';
import { copyText, createNativeReviewApi, formatPrompt, makeMockApi } from './api';

describe('browser fallback API', () => {
  beforeEach(() => localStorage.clear());

  it('keeps annotations after prompt generation', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const preview = await api.generatePrompt(review.workspace.id, { scope: 'feedback', portable: true });
    const after = await api.loadReview(review.workspace.id);
    expect(preview.annotationCount).toBeGreaterThan(0);
    expect(after.annotations).toHaveLength(review.annotations.length);
  });

  it('keeps feedback, question, and full prompt scopes strict', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const comment = review.annotations.find((annotation) => annotation.kind === 'comment')!;
    const question = review.annotations.find((annotation) => annotation.kind === 'question')!;
    const suggestion = review.annotations.find((annotation) => annotation.kind === 'suggestion')!;
    const fileNote = { ...comment, id: 'scope-file-note', kind: 'file_note' as const, startLine: 0, endLine: 0, body: 'included file note body' };
    const reviewNote = { ...comment, id: 'scope-review-note', kind: 'review_note' as const, startLine: 0, endLine: 0, body: 'included review note body', selectedSource: '' };
    await api.saveAnnotation(review.workspace.id, fileNote);
    await api.saveAnnotation(review.workspace.id, reviewNote);

    const feedback = await api.generatePrompt(review.workspace.id, { scope: 'feedback', portable: true });
    expect(feedback.title).toBe('Review feedback');
    expect(feedback.annotationCount).toBe(2);
    expect(feedback.content).toContain('# LocalReview feedback');
    expect(feedback.content).toContain(comment.body);
    expect(feedback.content).toContain(suggestion.body);
    expect(feedback.content).not.toContain(question.body);
    expect(feedback.content).not.toContain('included file note body');

    const questions = await api.generatePrompt(review.workspace.id, { scope: 'questions', portable: true });
    expect(questions.title).toBe('Questions for investigation');
    expect(questions.annotationCount).toBe(1);
    expect(questions.content).toContain('# Review questions');
    expect(questions.content).toContain('Answer every included question');
    expect(questions.content).toContain('Question:');
    expect(questions.content).toContain(question.body);
    expect(questions.content).not.toContain(comment.body);

    const full = await api.generatePrompt(review.workspace.id, { scope: 'all', portable: true });
    expect(full.title).toBe('Full review prompt');
    expect(full.annotationCount).toBe(5);
    expect(full.content).toContain('# Full LocalReview prompt');
    expect(full.content).toContain('handle the included file and review notes');
    expect(full.content).toContain(comment.body);
    expect(full.content).toContain(question.body);
    expect(full.content).toContain(suggestion.body);
    expect(full.content).toContain('included file note body');
    expect(full.content).toContain('included review note body');

    const selected = await api.generatePrompt(review.workspace.id, { scope: 'selected', annotationIds: [comment.id, question.id], portable: true });
    expect(selected.annotationCount).toBe(2);
    expect(selected.content).toContain('# Selected review annotations');
    expect(selected.content).toContain("Handle only the selected annotations below according to each annotation's stated kind and intent.");
    expect(selected.content).not.toContain('included file note body');
    expect(selected.content).not.toContain('included review note body');
    expect((await api.getReviewHistory(review.workspace.id)).filter((item) => item.type === 'export').map((item) => item.label)).toEqual(expect.arrayContaining([
      'Review feedback', 'Questions for investigation', 'Full review prompt'
    ]));
  });

  it('reopens a durable prompt export byte-for-byte without touching annotations', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const first = await api.generatePrompt(review.workspace.id, { scope: 'all', portable: true });
    const reopened = await api.generatePrompt(review.workspace.id, { scope: 'feedback', portable: false, historyId: `export:${first.exportId}` });
    expect(reopened).toEqual(first);
    await expect(api.generatePrompt('workspace-api', { scope: 'all', historyId: `export:${first.exportId}` })).rejects.toThrow('does not belong');
    await expect(api.savePromptExport(review.workspace.id, first.exportId, 'json')).resolves.toMatchObject({ saved: false, format: 'json' });
  });

  it('archives a recoverable checkpoint when clearing', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const checkpoint = await api.archiveAnnotations(review.workspace.id);
    const after = await api.loadReview(review.workspace.id);
    expect(checkpoint.annotationCount).toBe(review.annotations.length);
    expect(after.annotations).toEqual([]);
    expect(after.history[0]).toMatchObject({ id: checkpoint.id, type: 'clear' });
  });

  it('persists an immediate undo after clearing', async () => {
    const api = makeMockApi();
    const before = await api.loadReview('workspace-localreview');
    const checkpoint = await api.archiveAnnotations(before.workspace.id);
    const restored = await api.restoreAnnotations(before.workspace.id, before.annotations);
    expect(restored.annotations.map((annotation) => annotation.id)).not.toEqual(before.annotations.map((annotation) => annotation.id));
    expect((await api.getReviewHistory(before.workspace.id)).find((entry) => entry.id === checkpoint.id)?.annotations?.map((annotation) => annotation.id)).toEqual(before.annotations.map((annotation) => annotation.id));
    expect(restored.files.find((file) => file.id === 'file-app')?.annotationCount).toBe(2);
  });

  it('formats prompt content in deterministic repository/file order', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const content = formatPrompt(review, [...review.annotations].reverse(), true);
    expect(content).toContain('# Full LocalReview prompt');
    expect(content.indexOf('ui/src/App.svelte')).toBeLessThan(content.indexOf('ui/src/lib/api.ts'));
    expect(content).not.toContain('Filesystem path:');
  });

  it('keeps prompt paths logical and handles anchorless review notes', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    review.workspace.name = 'file:///private/var/tmp/logical-review';
    review.workspace.location = '/private/var/folders/cache/localreview/reviews/worktree';
    const anchored = review.annotations[0]!;
    const repository = review.repositories.find((item) => item.id === anchored.repositoryId)!;
    repository.path = 'file:///private/var/tmp/cache/repository';
    const reviewNote = {
      ...anchored,
      id: 'review-note-path-test',
      repositoryId: '',
      fileId: '',
      kind: 'review_note' as const,
      startLine: 0,
      endLine: 0,
      body: 'Keep this overall observation portable.',
      selectedSource: ''
    };

    const content = formatPrompt(review, [anchored, reviewNote], false, 'all');
    expect(content).toContain('Workspace: logical-review');
    expect(content).toContain(`Repository \`repository:${repository.id}\``);
    expect(content).toContain(`Logical path: \`repository:${repository.id}/ui/src/App.svelte\``);
    expect(content).toContain('## Overall review');
    expect(content).toContain('Keep this overall observation portable.');
    expect(content).not.toContain('/private/var');
    expect(content).not.toContain('file:///');
    expect(content).not.toContain('Filesystem path:');
  });

  it('persists viewed state and archives before a new review', async () => {
    const api = makeMockApi();
    const before = await api.loadReview('workspace-localreview');
    await api.setViewed(before.workspace.id, 'file-types', true);
    expect((await api.loadReview(before.workspace.id)).files.find((file) => file.id === 'file-types')?.viewed).toBe(true);
    const next = await api.startNewReview(before.workspace.id);
    expect(next.annotations).toEqual([]);
    expect(next.files.every((file) => !file.viewed)).toBe(true);
    expect(next.history.some((item) => item.annotations?.length === before.annotations.length)).toBe(true);
  });

  it('keeps multiple review sessions isolated and durable across API restarts', async () => {
    const firstProcess = makeMockApi();
    const first = await firstProcess.loadReview('workspace-localreview');
    const firstFile = first.files.find((file) => file.id === 'file-types')!;
    await firstProcess.setViewed(first.workspace.id, firstFile.id, true);
    await firstProcess.saveWorkspaceUiState(first.workspace.id, {
      activeFileId: firstFile.id,
      mode: 'split',
      fullFileSide: 'old',
      scrollTop: 420,
      splitRatio: .7,
      rightTab: 'comments',
      selectedAnnotationIds: [first.annotations[0]!.id]
    });
    await firstProcess.saveAnnotationDraft({
      id: 'draft-first-review', workspaceId: first.workspace.id, fileId: firstFile.id,
      repositoryId: firstFile.repositoryId, kind: 'comment', side: 'new',
      startLine: 1, endLine: 1, body: 'belongs only to review one', updatedAt: new Date().toISOString()
    });
    const firstExport = await firstProcess.generatePrompt(first.workspace.id, { scope: 'all', portable: true });
    const second = await firstProcess.startNewReview(first.workspace.id);
    const firstHistory = second.history.find((entry) => entry.type === 'review')!;

    // Recreating the API simulates a browser/webview restart. The native
    // controller test covers reopening the same durable SQLite store.
    const secondProcess = makeMockApi();
    const reopenedSecond = await secondProcess.loadReview(first.workspace.id);
    expect(reopenedSecond.annotations).toEqual([]);
    expect(reopenedSecond.files.every((file) => !file.viewed)).toBe(true);
    expect(await secondProcess.getAnnotationDraft(first.workspace.id)).toBeUndefined();
    expect(await secondProcess.getWorkspaceUiState(first.workspace.id)).toMatchObject({
      mode: 'unified', fullFileSide: 'both', scrollTop: 0, splitRatio: .5, rightTab: 'files'
    });
    expect(await secondProcess.generatePrompt(first.workspace.id, { scope: 'feedback', historyId: `export:${firstExport.exportId}` })).toEqual(firstExport);

    const frozenFirst = await secondProcess.loadArchivedReview(first.workspace.id, firstHistory.id);
    expect(frozenFirst.historical).toBe(true);
    expect(frozenFirst.annotations).toEqual(first.annotations);
    expect(frozenFirst.files.find((file) => file.id === firstFile.id)?.viewed).toBe(true);

    const secondAnnotation = { ...first.annotations[0]!, id: 'second-review-annotation', body: 'belongs only to review two' };
    await secondProcess.saveAnnotation(first.workspace.id, secondAnnotation);
    await secondProcess.saveWorkspaceUiState(first.workspace.id, { mode: 'full', fullFileSide: 'new', scrollTop: 99, splitRatio: .4, rightTab: 'outline' });
    const third = await secondProcess.startNewReview(first.workspace.id);
    expect(third.history.filter((entry) => entry.type === 'review')).toHaveLength(2);

    const thirdProcess = makeMockApi();
    const archived = await Promise.all(third.history.filter((entry) => entry.type === 'review').map((entry) => thirdProcess.loadArchivedReview(first.workspace.id, entry.id)));
    expect(archived.some((session) => session.annotations.some((annotation) => annotation.body === 'belongs only to review two'))).toBe(true);
    expect(archived.some((session) => session.annotations.some((annotation) => annotation.id === first.annotations[0]!.id))).toBe(true);
    expect((await thirdProcess.loadReview(first.workspace.id)).annotations).toEqual([]);
    expect((await thirdProcess.getWorkspaceUiState(first.workspace.id)).mode).toBe('unified');
  });

  it('keeps an unresolved workspace uncaptured across restarts and repairs it on repeated open', async () => {
    const firstProcess = makeMockApi();
    const uncaptured = await firstProcess.openWorkspace({ path: '/work/recover-me', base: 'origin/missing' });
    expect(uncaptured).toMatchObject({ reviewReady: false, defaultBase: 'origin/missing', progress: { viewed: 0, total: 0 } });
    expect(await firstProcess.loadReview(uncaptured.id)).toMatchObject({ files: [], annotations: [], history: [] });

    const restarted = makeMockApi();
    expect((await restarted.loadReview(uncaptured.id)).workspace.reviewReady).toBe(false);
    expect(await restarted.getAnnotationDraft(uncaptured.id)).toBeUndefined();
    expect(await restarted.getWorkspaceUiState(uncaptured.id)).toMatchObject({ mode: 'unified', rightTab: 'files' });

    const repaired = await restarted.openWorkspace({ path: '/work/recover-me', base: 'main' });
    expect(repaired).toMatchObject({ id: uncaptured.id, reviewReady: true, defaultBase: 'main' });
    const captured = await restarted.loadReview(uncaptured.id);
    expect(captured.files.length).toBeGreaterThan(0);
    expect(captured.repositories.every((repository) => repository.base === 'main')).toBe(true);
    expect(captured.history).toEqual([]);

    const secondRestart = makeMockApi();
    expect((await secondRestart.loadReview(uncaptured.id)).workspace).toMatchObject({ reviewReady: true, defaultBase: 'main' });
  });

  it('persists baseline setup without inventing a session until initial capture succeeds', async () => {
    const api = makeMockApi();
    const workspace = await api.openWorkspace({ path: '/work/configure-first', base: 'topic/missing' });
    const configuredMissing = await api.configureBaselines(workspace.id, 'release/missing');
    expect(configuredMissing.workspace).toMatchObject({ reviewReady: false, defaultBase: 'release/missing' });
    expect(configuredMissing.files).toEqual([]);
    await expect(api.startNewReview(workspace.id)).rejects.toThrow('No repository capture succeeded');
    expect((await api.loadReview(workspace.id)).history).toEqual([]);

    const configured = await api.configureBaselines(workspace.id, 'main');
    expect(configured.workspace).toMatchObject({ reviewReady: false, defaultBase: 'main' });
    expect(configured.files).toEqual([]);
    const initial = await api.startNewReview(workspace.id);
    expect(initial.workspace.reviewReady).toBe(true);
    expect(initial.files.length).toBeGreaterThan(0);
    expect(initial.history).toEqual([]);

    await expect(api.openWorkspace({ path: '/work/invalid-base', base: 'bad base' })).rejects.toThrow('safe branch');
    expect((await api.listWorkspaces()).some((item) => item.location === '/work/invalid-base')).toBe(false);
  });

  it('namespaces opened workspace files and saves annotations only to the explicit workspace', async () => {
    const api = makeMockApi();
    const firstWorkspace = await api.openWorkspace({ path: '/work/one', base: 'main' });
    const secondWorkspace = await api.openWorkspace({ path: '/work/two', base: 'main' });
    const first = await api.loadReview(firstWorkspace.id);
    const second = await api.loadReview(secondWorkspace.id);
    const firstIds = new Set(first.files.map((file) => file.id));
    expect(second.files.every((file) => !firstIds.has(file.id))).toBe(true);
    expect(second.repositories.every((repository) => !first.repositories.some((candidate) => candidate.id === repository.id))).toBe(true);

    const firstFile = first.files[0]!;
    const annotation = {
      id: 'opened-workspace-annotation', fileId: firstFile.id, repositoryId: firstFile.repositoryId,
      kind: 'comment' as const, state: 'open' as const, side: 'new' as const, startLine: 1, endLine: 1,
      body: 'belongs to workspace one', selectedSource: 'line one', labels: [], localOnly: false,
      createdAt: '2026-07-22T00:00:00.000Z'
    };
    await expect(api.saveAnnotation(secondWorkspace.id, annotation)).rejects.toThrow('does not belong');
    await expect(api.saveAnnotation(firstWorkspace.id, annotation)).resolves.toEqual(annotation);
    expect((await api.loadReview(firstWorkspace.id)).annotations).toEqual([annotation]);
    expect((await api.loadReview(secondWorkspace.id)).annotations).toEqual([]);

    const restarted = makeMockApi();
    expect((await restarted.loadReview(firstWorkspace.id)).annotations).toEqual([annotation]);
    expect((await restarted.loadReview(secondWorkspace.id)).annotations).toEqual([]);
  });

  it('migrates legacy workspace draft and UI keys into the active review session once', async () => {
    const seed = makeMockApi();
    const workspaceId = 'workspace-localreview';
    const draft = {
      id: 'legacy-draft', workspaceId, fileId: 'file-app', repositoryId: 'repo-desktop',
      kind: 'question' as const, side: 'new' as const, startLine: 76, endLine: 76,
      body: 'legacy browser draft', updatedAt: '2026-07-22T00:00:00.000Z'
    };
    await seed.saveAnnotationDraft(draft);
    const persisted = JSON.parse(localStorage.getItem('localreview.mock.v1')!);
    persisted.annotationDrafts = { [workspaceId]: draft };
    localStorage.setItem('localreview.mock.v1', JSON.stringify(persisted));
    localStorage.setItem('localreview.ui-state.v1', JSON.stringify({
      [workspaceId]: { mode: 'split', fullFileSide: 'old', scrollTop: 321, splitRatio: .6, rightTab: 'comments', activeFileId: 'file-app' }
    }));

    const upgraded = makeMockApi();
    expect(await upgraded.getAnnotationDraft(workspaceId)).toEqual(draft);
    expect(await upgraded.getWorkspaceUiState(workspaceId)).toMatchObject({ mode: 'split', fullFileSide: 'old', scrollTop: 321, rightTab: 'comments' });
    const migratedState = JSON.parse(localStorage.getItem('localreview.mock.v1')!);
    const migratedUi = JSON.parse(localStorage.getItem('localreview.ui-state.v1')!);
    expect(migratedState.annotationDrafts[workspaceId]).toBeUndefined();
    expect(migratedUi[workspaceId]).toBeUndefined();
    expect(Object.keys(migratedState.annotationDrafts)).toEqual([expect.stringMatching(/^workspace-localreview:/)]);
    expect(Object.keys(migratedUi)).toEqual([expect.stringMatching(/^workspace-localreview:/)]);

    await upgraded.startNewReview(workspaceId);
    const restarted = makeMockApi();
    expect(await restarted.getAnnotationDraft(workspaceId)).toBeUndefined();
    expect(await restarted.getWorkspaceUiState(workspaceId)).toMatchObject({ mode: 'unified', scrollTop: 0, rightTab: 'files' });
  });

  it('migrates legacy mock files to navigable hunk counts', async () => {
    const seed = makeMockApi();
    await seed.setViewed('workspace-localreview', 'file-app', true);
    const persisted = JSON.parse(localStorage.getItem('localreview.mock.v1')!);
    for (const review of Object.values(persisted.reviews) as Array<{ files: Array<{ hunkCount?: number }> }>) {
      for (const file of review.files) delete file.hunkCount;
    }
    localStorage.setItem('localreview.mock.v1', JSON.stringify(persisted));

    const upgraded = makeMockApi();
    const review = await upgraded.loadReview('workspace-localreview');
    expect(review.files.every((file) => file.hunkCount > 0)).toBe(true);
    expect(review.files.find((file) => file.id === 'file-app')?.hunkCount).toBe(2);
    const migrated = JSON.parse(localStorage.getItem('localreview.mock.v1')!);
    expect(migrated.reviews['workspace-localreview'].files.every((file: { hunkCount?: number }) => Number.isSafeInteger(file.hunkCount))).toBe(true);
  });

  it('models Full File Current removals as collapsed, expandable deletion gates', async () => {
    const api = makeMockApi();
    const collapsed = await api.getPresentationWindow({
      fileId: 'file-app', mode: 'full', fullFileSide: 'new',
      startRow: 0, endRow: 500, generation: 1
    });
    expect(collapsed.omittedBlocks?.length).toBeGreaterThan(0);
    expect(collapsed.rows.filter((row) => row.kind === 'deletion_gate')).toHaveLength(collapsed.omittedBlocks!.length);
    expect(collapsed.rows.some((row) => row.kind === 'deletion')).toBe(false);
    expect(collapsed.hunks.every((hunk) => hunk.oldLine || hunk.newLine)).toBe(true);

    const firstBlock = collapsed.omittedBlocks![0]!;
    await expect(api.resolvePresentationLocation('file-app', 'full', 'old', firstBlock.startLine))
      .resolves.toMatchObject({ rowIndex: firstBlock.rowIndex, side: 'old', line: firstBlock.startLine });
    await api.saveWorkspaceUiState('workspace-localreview', { expandedFullFileDeletionBlocks: [firstBlock.id] });
    const expanded = await api.getPresentationWindow({
      fileId: 'file-app', mode: 'full', fullFileSide: 'new',
      startRow: 0, endRow: 500, generation: 2
    });
    expect(expanded.omittedBlocks?.find((block) => block.id === firstBlock.id)?.expanded).toBe(true);
    expect(expanded.rows.some((row) => row.kind === 'deletion' && row.oldLine === firstBlock.startLine)).toBe(true);
  });

  it('models Full File Base additions as symmetric collapsed, expandable gates', async () => {
    const api = makeMockApi();
    const collapsed = await api.getPresentationWindow({
      fileId: 'file-app', mode: 'full', fullFileSide: 'old',
      startRow: 0, endRow: 500, generation: 1
    });
    expect(collapsed.omittedBlocks?.length).toBeGreaterThan(0);
    expect(collapsed.omittedBlocks?.every((block) => block.side === 'new')).toBe(true);
    expect(collapsed.rows.filter((row) => row.kind === 'addition_gate')).toHaveLength(collapsed.omittedBlocks!.length);
    expect(collapsed.rows.some((row) => row.kind === 'addition')).toBe(false);

    const firstBlock = collapsed.omittedBlocks![0]!;
    await expect(api.resolvePresentationLocation('file-app', 'full', 'new', firstBlock.startLine))
      .resolves.toMatchObject({ rowIndex: firstBlock.rowIndex, side: 'new', line: firstBlock.startLine });
    await api.saveWorkspaceUiState('workspace-localreview', {
      mode: 'full',
      fullFileSide: 'old',
      expandedFullFileDeletionBlocks: [firstBlock.id]
    });
    const expanded = await api.getPresentationWindow({
      fileId: 'file-app', mode: 'full', fullFileSide: 'old',
      startRow: 0, endRow: 500, generation: 2
    });
    expect(expanded.omittedBlocks?.find((block) => block.id === firstBlock.id)?.expanded).toBe(true);
    expect(expanded.rows.some((row) => row.kind === 'addition' && row.newLine === firstBlock.startLine)).toBe(true);
  });

  it('defaults Full File Both to additions expanded and deletions collapsed, with durable independent controls', async () => {
    const api = makeMockApi();
    const initial = await api.getPresentationWindow({
      fileId: 'file-app', mode: 'full', fullFileSide: 'both',
      startRow: 0, endRow: 500, generation: 1
    });
    const addition = initial.omittedBlocks?.find((block) => block.side === 'new');
    const deletion = initial.omittedBlocks?.find((block) => block.side === 'old');
    expect(addition?.expanded).toBe(true);
    expect(deletion?.expanded).toBe(false);
    expect(initial.rows.some((row) => row.kind === 'addition')).toBe(true);
    expect(initial.rows.some((row) => row.kind === 'deletion')).toBe(false);

    await api.saveWorkspaceUiState('workspace-localreview', {
      mode: 'full',
      fullFileSide: 'both',
      expandedFullFileDeletionBlocks: deletion ? [deletion.id] : [],
      collapsedFullFileAdditionBlocks: addition ? [addition.id] : []
    });
    const restarted = makeMockApi();
    const toggled = await restarted.getPresentationWindow({
      fileId: 'file-app', mode: 'full', fullFileSide: 'both',
      startRow: 0, endRow: 500, generation: 2
    });
    expect(toggled.omittedBlocks?.find((block) => block.side === 'new')?.expanded).toBe(false);
    expect(toggled.omittedBlocks?.find((block) => block.side === 'old')?.expanded).toBe(true);
    expect(toggled.rows.some((row) => row.kind === 'addition' && row.newLine === addition?.startLine)).toBe(false);
    expect(toggled.rows.some((row) => row.kind === 'deletion' && row.oldLine === deletion?.startLine)).toBe(true);
  });

  it('keeps removed workspaces recoverable and can reopen their captured snapshot', async () => {
    const api = makeMockApi();
    const before = await api.loadReview('workspace-localreview');
    await api.archiveWorkspace(before.workspace.id);
    expect((await api.listWorkspaces()).some((workspace) => workspace.id === before.workspace.id)).toBe(false);
    expect(await api.listArchivedWorkspaces()).toEqual(expect.arrayContaining([
      expect.objectContaining({ id: before.workspace.id, archived: true })
    ]));
    await api.reopenArchivedWorkspace(before.workspace.id);
    const reopened = await api.loadReview(before.workspace.id);
    expect(reopened.files).toEqual(before.files);
    expect(reopened.annotations).toEqual(before.annotations);
    expect((await api.listWorkspaces()).some((workspace) => workspace.id === before.workspace.id)).toBe(true);
  });

  it('reactivates an archived local workspace when its path is opened again', async () => {
    const api = makeMockApi();
    const before = await api.loadReview('workspace-localreview');
    await api.archiveWorkspace(before.workspace.id);

    const reopened = await api.openWorkspace({ path: before.workspace.location });
    expect(reopened.id).toBe(before.workspace.id);
    expect(reopened.archived).not.toBe(true);
    expect((await api.listArchivedWorkspaces()).some((workspace) => workspace.id === before.workspace.id)).toBe(false);
    expect((await api.loadReview(reopened.id)).files).toEqual(before.files);
  });

  it('permanently deletes a workspace instead of leaving recoverable history', async () => {
    const api = makeMockApi();
    const before = await api.loadReview('workspace-localreview');
    await api.generatePrompt(before.workspace.id, { scope: 'feedback', portable: true });
    await api.deleteWorkspace(before.workspace.id);

    expect((await api.listWorkspaces()).some((workspace) => workspace.id === before.workspace.id)).toBe(false);
    expect((await api.listArchivedWorkspaces()).some((workspace) => workspace.id === before.workspace.id)).toBe(false);
    await expect(api.loadReview(before.workspace.id)).rejects.toThrow('does not exist');
  });

  it('reconnects only SSH workspaces through the explicit recovery action', async () => {
    const api = makeMockApi();
    const ssh = await api.openSshWorkspace('builder@host:/srv/review');
    await expect(api.reconnectSshWorkspace(ssh.id)).resolves.toMatchObject({ id: ssh.id, connection: 'connected' });
    await expect(api.reconnectSshWorkspace('workspace-localreview')).rejects.toThrow('not an SSH');
  });

  it('opens a previous review as a read-only browsing snapshot', async () => {
    const api = makeMockApi();
    const before = await api.loadReview('workspace-localreview');
    const current = await api.startNewReview(before.workspace.id);
    const archived = current.history.find((entry) => entry.type === 'review' && entry.annotations?.length);
    expect(archived).toBeTruthy();
    const snapshot = await api.loadArchivedReview(before.workspace.id, archived!.id);
    expect(snapshot.historical).toBe(true);
    expect(snapshot.annotations).toEqual(before.annotations);
  });

  it('returns capture-derived classification, blame, commit context, and immutable prior-review metadata', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const classifications = await api.getReviewFileClassifications(review.workspace.id);
    expect(classifications.find((entry) => entry.fileId === 'file-protocol')?.classification.generated).toBe(true);
    const blame = await api.getCapturedBlame(review.workspace.id, 'file-app', 'new', 74, 76);
    expect(blame.lines).toHaveLength(3);
    expect(blame.lines[0]?.revision).toBe(review.repositories[0]?.head);
    const commits = await api.getCommitContext(review.workspace.id, { repositoryId: 'repo-desktop', selectedCommit: review.repositories[0]?.head });
    expect(commits.selectedCommit?.summary.sha).toBe(review.repositories[0]?.head);
    const changed = await api.getChangedSincePreviousReview(review.workspace.id, 'repo-desktop');
    expect(changed.previousComparisonId).toBeTruthy();
    expect(changed.files.some((entry) => entry.kind !== 'unchanged' && entry.currentFileId)).toBe(true);
  });

  it('submits only an opaque Finish Review preview token and preserves the exact reviewed JSON', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const request = { annotationIds: ['annotation-1'], summary: 'Token review', conclusion: 'comment' as const };
    const preview = await api.previewFinishReview(review.workspace.id, request);
    const result = await api.finishReview(review.workspace.id, { previewToken: preview.previewToken });
    expect(result.payloadJson).toBe(preview.payloadJson);
    await expect(api.finishReview(review.workspace.id, { previewToken: preview.previewToken })).rejects.toThrow('exact preview');
  });

  it('releases superseded and closed Finish Review previews without retaining usable tokens', async () => {
    const api = makeMockApi();
    const review = await api.loadReview('workspace-localreview');
    const first = await api.previewFinishReview(review.workspace.id, { annotationIds: ['annotation-1'], summary: 'first edit', conclusion: 'comment' });
    await api.abandonFinishReview(review.workspace.id, { previewToken: first.previewToken });
    await expect(api.finishReview(review.workspace.id, { previewToken: first.previewToken })).rejects.toThrow('exact preview');

    const second = await api.previewFinishReview(review.workspace.id, { annotationIds: ['annotation-1'], summary: 'final edit', conclusion: 'comment' });
    await api.abandonFinishReview(review.workspace.id, { previewToken: second.previewToken });
    await expect(api.finishReview(review.workspace.id, { previewToken: second.previewToken })).rejects.toThrow('exact preview');
  });

  it('maps every privileged native action to its explicit Tauri command', async () => {
    const invoke = vi.fn().mockResolvedValue({});
    const api = createNativeReviewApi(invoke);
    await api.openWorkspace({ path: '/repo', base: 'origin/main' });
    await api.openGitHubPr('https://github.com/acme/repo/pull/42');
    await api.openSshWorkspace('build@host:/srv/repo');
    await api.reconnectSshWorkspace('workspace-1');
    await api.listArchivedWorkspaces();
    await api.reopenArchivedWorkspace('workspace-1');
    await api.archiveWorkspace('workspace-1');
    await api.deleteWorkspace('workspace-1');
    await api.loadArchivedReview('workspace-1', 'review:history-1');
    await api.getReviewFileClassifications('workspace-1');
    await api.getCapturedBlame('workspace-1', 'file-1', 'new', 40, 42);
    await api.getCommitContext('workspace-1', { repositoryId: 'repo-1', maxEntries: 25, includeMergeCommits: false, authorContains: 'Ada', subjectContains: 'parser', selectedCommit: 'deadbeef' });
    await api.getChangedSincePreviousReview('workspace-1', 'repo-1');
    await api.getGitHubUpdateStatus('workspace-1');
    await api.getGitHubPullRequest('workspace-1');
    await api.getGitHubThreads('workspace-1');
    await api.getGitHubConversation('workspace-1');
    await api.getRepositorySetup('workspace-1');
    await api.setRepositoryInclusion('workspace-1', ['repo-1'], false);
    await api.applyRepositoryBase('workspace-1', ['repo-1'], 'origin/release');
    await api.resetRepositoryBaseOverrides('workspace-1', ['repo-1']);
    await api.fetchRepositories('workspace-1', ['repo-1']);
    await api.configureBaselines('workspace-1', 'origin/main', [{ repositoryId: 'repo-1', base: 'origin/release' }]);
    await api.setViewed('workspace-1', 'file-1', true);
    await api.startNewReview('workspace-1', { fetchBeforeCapture: false, comparisonOptions: { ignoreAllWhitespace: true, ignoreSpaceAtEol: false, ignoreCrAtEol: true } });
    await api.refreshReview('workspace-1', { fetchBeforeCapture: true, comparisonOptions: { ignoreAllWhitespace: false, ignoreSpaceAtEol: true, ignoreCrAtEol: true } });
    await api.previewFinishReview('workspace-1', { annotationIds: ['annotation-1'], summary: 'Looks good', conclusion: 'comment' });
    await api.finishReview('workspace-1', { previewToken: 'preview-token-1' });
    await api.abandonFinishReview('workspace-1', { previewToken: 'preview-token-2' });
    await api.abandonFinishReview('workspace-1', { previewToken: 'preview-token-3' }, true);
    await api.restoreHistoryItem('workspace-1', 'history-1');
    await api.savePromptExport('workspace-1', 'export-1', 'markdown');
    await api.getPresentationWindow({ fileId: 'file-1', mode: 'split', startRow: 20, endRow: 80, generation: 4, splitRatio: .55 });
    await api.resolvePresentationLocation('file-1', 'split', 'new', 42);
    await api.getCapturedSourceRange('file-1', 'new', 40, 42);
    const symbolInput = { workspaceId: 'workspace-1', repositoryId: 'repo-1', fileId: 'file-1', comparisonId: 'comparison-1', side: 'new' as const, line: 42, column: 7, symbol: 'launch', initialQuery: 'definitions' as const };
    const symbolQuery = { workspaceId: 'workspace-1', repositoryId: 'repo-1', symbol: 'launch', kind: 'references' as const, limit: 200 };
    const symbolSource = { workspaceId: 'workspace-1', repositoryId: 'repo-1', path: 'src/main.rs', expectedFingerprint: 'fingerprint-1', startLine: 34, lineCount: 17 };
    await api.openSymbolNavigation(symbolInput);
    await api.querySymbolNavigation(symbolQuery);
    await api.getSymbolSource(symbolSource);
    const draft = { id: 'draft-1', workspaceId: 'workspace-1', fileId: 'file-1', repositoryId: 'repo-1', kind: 'comment' as const, side: 'new' as const, startLine: 42, endLine: 42, body: 'unfinished', updatedAt: '2026-07-22T00:00:00Z' };
    await api.saveAnnotationDraft(draft);
    await api.getAnnotationDraft('workspace-1');
    await api.clearAnnotationDraft('workspace-1');
    await api.expandHunk('file-1', 'hunk-1', 30);
    await api.getOutline('file-1', 'new');
    await api.saveAnnotation('workspace-1', { id: 'annotation-1', fileId: 'file-1', repositoryId: 'repo-1', kind: 'comment', state: 'open', side: 'new', startLine: 1, endLine: 1, body: 'note', selectedSource: 'source', labels: [], localOnly: false, createdAt: '2026-07-22T00:00:00Z' });
    await api.saveWorkspaceUiState('workspace-1', { mode: 'full', nearestSourceLine: 42 });
    await api.copyReviewItem('workspace-1', { kind: 'path', fileId: 'file-1' });
    expect(invoke.mock.calls).toEqual(expect.arrayContaining([
      ['open_workspace', { request: { path: '/repo', base: 'origin/main' } }],
      ['open_github_pr', { url: 'https://github.com/acme/repo/pull/42' }],
      ['open_ssh_workspace', { target: 'build@host:/srv/repo' }],
      ['reconnect_ssh_workspace', { workspaceId: 'workspace-1' }],
      ['list_archived_workspaces'],
      ['reopen_archived_workspace', { workspaceId: 'workspace-1' }],
      ['archive_workspace', { workspaceId: 'workspace-1' }],
      ['delete_workspace', { workspaceId: 'workspace-1' }],
      ['load_archived_review', { workspaceId: 'workspace-1', historyId: 'review:history-1' }],
      ['get_review_file_classifications', { workspaceId: 'workspace-1' }],
      ['get_captured_blame', { workspaceId: 'workspace-1', fileId: 'file-1', side: 'new', startLine: 40, endLine: 42 }],
      ['get_commit_context', { workspaceId: 'workspace-1', request: { repositoryId: 'repo-1', maxEntries: 25, includeMergeCommits: false, authorContains: 'Ada', subjectContains: 'parser', selectedCommit: 'deadbeef' } }],
      ['get_changed_since_previous_review', { workspaceId: 'workspace-1', repositoryId: 'repo-1' }],
      ['get_github_update_status', { workspaceId: 'workspace-1' }],
      ['get_github_pull_request', { workspaceId: 'workspace-1' }],
      ['get_github_threads', { workspaceId: 'workspace-1' }],
      ['get_github_conversation', { workspaceId: 'workspace-1' }],
      ['get_repository_setup', { workspaceId: 'workspace-1' }],
      ['set_repository_inclusion', { workspaceId: 'workspace-1', input: { repositoryIds: ['repo-1'], enabled: false } }],
      ['apply_repository_base', { workspaceId: 'workspace-1', input: { repositoryIds: ['repo-1'], base: 'origin/release' } }],
      ['reset_repository_base_overrides', { workspaceId: 'workspace-1', input: { repositoryIds: ['repo-1'] } }],
      ['fetch_repositories', { workspaceId: 'workspace-1', repositoryIds: ['repo-1'] }],
      ['configure_baselines', { workspaceId: 'workspace-1', defaultBase: 'origin/main', repositoryBases: [{ repositoryId: 'repo-1', base: 'origin/release' }] }],
      ['set_viewed', { workspaceId: 'workspace-1', fileId: 'file-1', viewed: true }],
      ['start_new_review', { workspaceId: 'workspace-1', request: { fetchBeforeCapture: false, comparisonOptions: { ignoreAllWhitespace: true, ignoreSpaceAtEol: false, ignoreCrAtEol: true } } }],
      ['refresh_review', { workspaceId: 'workspace-1', request: { fetchBeforeCapture: true, comparisonOptions: { ignoreAllWhitespace: false, ignoreSpaceAtEol: true, ignoreCrAtEol: true } } }],
      ['preview_finish_review', { workspaceId: 'workspace-1', request: { annotationIds: ['annotation-1'], summary: 'Looks good', conclusion: 'comment' } }],
      ['finish_review', { workspaceId: 'workspace-1', submission: { previewToken: 'preview-token-1' } }],
      ['abandon_finish_review', { workspaceId: 'workspace-1', submission: { previewToken: 'preview-token-2' }, confirmPrepared: false }],
      ['abandon_finish_review', { workspaceId: 'workspace-1', submission: { previewToken: 'preview-token-3' }, confirmPrepared: true }],
      ['restore_history_item', { workspaceId: 'workspace-1', historyId: 'history-1' }],
      ['save_prompt_export', { workspaceId: 'workspace-1', exportId: 'export-1', format: 'markdown' }]
      , ['get_presentation_window', { request: { fileId: 'file-1', mode: 'split', startRow: 20, endRow: 80, generation: 4, splitRatio: .55 } }]
      , ['resolve_presentation_location', { fileId: 'file-1', mode: 'split', side: 'new', line: 42 }]
      , ['get_captured_source_range', { fileId: 'file-1', side: 'new', startLine: 40, endLine: 42 }]
      , ['open_symbol_navigation', { input: symbolInput }]
      , ['query_symbol_navigation', { input: symbolQuery }]
      , ['get_symbol_source', { input: symbolSource }]
      , ['save_annotation_draft', { draft }]
      , ['get_annotation_draft', { workspaceId: 'workspace-1' }]
      , ['clear_annotation_draft', { workspaceId: 'workspace-1' }]
      , ['expand_hunk_context', { fileId: 'file-1', hunkId: 'hunk-1', contextLines: 30 }]
      , ['get_outline', { fileId: 'file-1', side: 'new' }]
      , ['save_annotation', { annotation: { id: 'annotation-1', fileId: 'file-1', repositoryId: 'repo-1', kind: 'comment', state: 'open', side: 'new', startLine: 1, endLine: 1, body: 'note', selectedSource: 'source', labels: [], localOnly: false, createdAt: '2026-07-22T00:00:00Z' } }]
      , ['save_workspace_ui_state', { workspaceId: 'workspace-1', state: { mode: 'full', nearestSourceLine: 42 } }]
      , ['copy_review_item', { workspaceId: 'workspace-1', request: { kind: 'path', fileId: 'file-1' } }]
    ]));
  });

  it('durably stores unfinished composers and returns complete captured ranges', async () => {
    const api = makeMockApi();
    const draft = { id: 'draft-1', workspaceId: 'workspace-localreview', fileId: 'file-app', repositoryId: 'repo-desktop', kind: 'question' as const, side: 'new' as const, startLine: 74, endLine: 76, body: 'Why is this listener global?', updatedAt: '2026-07-22T00:00:00Z' };
    await api.saveAnnotationDraft(draft);
    expect(await api.getAnnotationDraft(draft.workspaceId)).toEqual(draft);
    expect(await api.getCapturedSourceRange('file-app', 'new', 74, 76)).toMatchObject({ complete: true });
    await api.clearAnnotationDraft(draft.workspaceId);
    expect(await api.getAnnotationDraft(draft.workspaceId)).toBeUndefined();
  });

  it('stores native-like workspace navigation state in the explicit browser fixture only', async () => {
    const api = makeMockApi();
    await api.saveWorkspaceUiState('workspace-localreview', { activeFileId: 'file-api', mode: 'split', nearestSourceLine: 32, splitRatio: .58, selectedAnnotationIds: [] });
    const reopened = makeMockApi();
    expect(await reopened.getWorkspaceUiState('workspace-localreview')).toMatchObject({ activeFileId: 'file-api', mode: 'split', nearestSourceLine: 32, splitRatio: .58, selectedAnnotationIds: [] });
  });

  it('uses the native clipboard command and propagates failures instead of claiming success', async () => {
    const invoke = vi.fn().mockResolvedValue(undefined);
    await copyText('prompt body', invoke);
    expect(invoke).toHaveBeenCalledWith('copy_to_clipboard', { text: 'prompt body' });
    await expect(copyText('prompt body', vi.fn().mockRejectedValue(new Error('denied')))).rejects.toThrow('denied');
  });
});
