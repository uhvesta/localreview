# LocalReview Product and Technical Specification

Status: Draft for implementation  
Date: 2026-07-21  
Product name: LocalReview (working title)

## 1. Product summary

LocalReview is a local-first desktop code-review application with a companion CLI. It lets a reviewer inspect changes across one or many Git repositories contained in a workspace, annotate code, export structured prompts, review GitHub pull requests in isolated worktrees, and eventually review repositories on machines reached over SSH.

The macOS desktop application is the primary product. The CLI and remote companion must run on macOS and Linux. The Rust core should remain portable enough that a Linux desktop application can be delivered later, but Linux desktop packaging is not initially release-blocking.

The product is a review environment, not a source editor. Its central surface is the diff. Workspace navigation lives on the left; changed files and review summaries live on a collapsible panel on the right.

## 2. Confirmed product decisions

The following decisions are requirements, not unresolved design suggestions:

- The desktop application is required on macOS.
- The CLI and remote companion are required on macOS and Linux.
- A workspace is a directory that may contain multiple Git repositories at subpaths.
- Repository discovery ignores ordinary non-repository directories.
- A workspace has one default base reference, with an optional override per repository.
- Comparisons follow GitHub pull-request semantics: the effective baseline is the merge base of the selected base reference and the current branch.
- Local reviews include committed, staged, unstaged, deleted, and untracked changes when possible. Git-ignored files remain excluded.
- Fetch-on-review is configurable and defaults to off.
- Filesystem changes never replace the active review automatically. The user must press Refresh.
- The far-left workspace rail contains vertical workspace tabs and a horizontal `All | GitHub | Local | SSH` filter.
- The diff/review surface is centered. The file navigator is on the right.
- Both side panels are resizable and independently collapsible so the diff can consume almost the entire window.
- Unified, Split, and Full File modes support syntax highlighting and inline annotations.
- Difftastic is a fourth, initially read-only structural diff mode. Direct annotation in Difftastic is a non-goal for now.
- Zoom changes font sizes only. It does not proportionally scale icons, panels, or the entire application surface.
- Comments, questions, drafts, prompt exports, and past reviews persist across restarts.
- Copying or exporting feedback never clears annotations.
- Clear archives the active annotation set into recoverable review history before starting an empty set.
- Starting a new review archives the prior review rather than overwriting it.
- GitHub feedback is submitted only through an explicit final action and is posted as one native batched review.
- GitHub.com is the only forge required initially.
- Initial GitHub authentication may reuse the GitHub CLI (`gh`) login.
- App-managed PR worktrees are review-only. Editing and source modification are outside product scope.
- SSH support may install or invoke a small LocalReview companion binary on the remote host.
- The CLI primarily opens, forwards, and focuses workspaces in the desktop application. It is not a headless diff renderer.

## 3. Goals

### 3.1 Primary goals

1. Make multi-repository local changes reviewable as one coherent unit.
2. Make changing the common baseline or a single repository's baseline obvious and safe.
3. Provide fast, correct, side-aware diff presentations for conventional and structural review.
4. Preserve review work so comments and questions can be revisited, cleared safely, or exported repeatedly.
5. Turn inline feedback into deterministic prompts suitable for an LLM with or without filesystem access.
6. Make GitHub PR review local, isolated, fast to reopen, and capable of one final native review submission.
7. Extend the same review experience to remote machines without copying or mounting entire workspaces.
8. Keep the center diff responsive on large repositories, files, and changesets.

### 3.2 Secondary goals

- Reduce review friction with keyboard navigation, viewed-state tracking, filtering, and durable position.
- Provide predictable recovery after application, machine, Git, network, or provider failures.
- Keep source code local unless the user explicitly copies, exports, or publishes it.
- Build the core as reusable Rust services shared by desktop, CLI forwarding, and the SSH companion.

## 4. Non-goals

The following are explicitly outside the current product scope:

- Editing source files in the review UI.
- Applying suggestions or patches to a working tree.
- Merge-conflict resolution.
- Git staging, committing, rebasing, or branch management.
- Direct inline annotation within Difftastic output.
- Automatically sending code to an LLM provider.
- Replacing an IDE or terminal.
- Automatically refreshing the active review when files change.
- Automatic fetching by default.
- GitLab, Bitbucket, or other forge integrations in the initial product.
- Cloud-hosted collaboration or mandatory account creation.
- Windows support in the initial product.

The architecture should not prevent later Linux desktop, other forges, configurable external LLM commands, or Difftastic annotations.

## 5. Terminology and domain model

### 5.1 Workspace

A workspace is a durable application record representing one reviewable root. A workspace has a display name, source, repositories, review history, UI state, and settings.

Workspace sources are:

- `LocalDirectory`: a directory on the desktop machine.
- `PullRequest`: a GitHub PR checked out into an isolated review worktree.
- `RemoteDirectory`: a directory accessed through an SSH companion.
- `RemotePullRequest`: a future composition of a GitHub review and remote execution location.

Workspace source filters are tag-based rather than mutually exclusive. A future remote GitHub PR appears under both GitHub and SSH, but only once under All.

### 5.2 Repository

A repository is a discovered or explicitly added Git working tree within a workspace. Identity must not depend only on its directory name. Durable identity uses the workspace-relative path, canonical Git common directory when available, and normalized primary remote URL.

A repository record contains:

- Enabled state.
- Workspace-relative path.
- Canonical worktree path or remote path.
- Current branch or detached-HEAD state.
- Remote information.
- Optional base-reference override.
- Effective base reference.
- Last successfully resolved base SHA.
- Last fetch time and status.
- Discovery and comparison errors.

### 5.3 Comparison

For each enabled repository, a review comparison records:

- Requested base reference, such as `origin/master`.
- Resolved base-tip SHA.
- Merge-base SHA.
- HEAD SHA and branch at capture time.
- Index fingerprint.
- Working-tree snapshot fingerprint.
- Included untracked-file identities.
- Capture time.
- Comparison options, including whitespace and path filters.

The effective baseline for a normal local review is:

```text
merge-base(selected base reference, HEAD) -> captured working-tree state
```

The target is a read-only synthetic snapshot representing committed, staged, unstaged, deleted, and untracked content without modifying the repository, index, or working tree.

For a GitHub PR, the comparison is the exact base and head commits supplied by the PR, with the merge-base semantics used by GitHub's Files Changed view. Resolved SHAs remain pinned until an explicit refresh.

### 5.4 Review session

A review session is one durable pass over a captured comparison. It owns:

- Per-repository comparisons.
- Changed-file metadata and cached presentation fingerprints.
- Viewed/unviewed state.
- Current file, hunk, scroll position, and mode.
- Annotations and annotation-set history.
- Prompt-export records.
- GitHub publication records.
- Start, refresh, clear, archive, and completion timestamps.

A workspace may own any number of review sessions. Exactly one session is current and mutable at a time; starting New Review freezes the current session and creates the next current session. Frozen sessions remain independently browseable and exportable. Annotations, unfinished drafts, export records, and per-session UI state never leak between sessions.

### 5.5 Annotation

Annotation kinds are:

- `Comment`: actionable review feedback.
- `Question`: a focused question intended for a question prompt or review discussion.
- `Suggestion`: an optional structured replacement expressed as review feedback, never directly applied by the app.
- `FileNote`: a note anchored to a file rather than a line.
- `ReviewNote`: an overall review summary.

Line annotation identity includes repository, snapshot, file identity, old/new side, start and end line, selected source, surrounding context, and an anchor fingerprint. Screen row numbers are never durable anchors.

## 6. Information architecture and window layout

### 6.1 Primary window

```text
+----------------------+-------------------------------------------+----------------------+
| Workspace rail       | Diff and inline review                    | Files / Review       |
|                      |                                           |                      |
| All Github Local SSH | repo / file / comparison                  | Files Comments       |
| Search workspaces    | Unified Split Full File Difftastic        | Outline              |
|                      |                                           |                      |
| Vertical workspace   | Previous/next file and hunk               | Search and filters   |
| tabs                 |                                           | Repository/file tree |
|                      | Code, gutters, and inline composers       | Review progress      |
|                      |                                           | Annotation overview  |
| Add / Open           |                                           |                      |
+----------------------+-------------------------------------------+----------------------+
```

The center diff is the dominant surface. Toolbar controls required during review remain in the center header rather than disappearing with either sidebar.

### 6.2 Left workspace rail

The far-left rail contains:

1. A sticky segmented filter: `All | GitHub | Local | SSH`.
2. Workspace search scoped to the selected filter.
3. Vertically arranged workspace tabs.
4. A bottom Add/Open control.

Each workspace tab shows, when space permits:

- Workspace name or `owner/repository #PR`.
- Local path or SSH host/path.
- Source and location badges.
- Review progress.
- Draft annotation count.
- Connection, refresh-available, or error state.

Workspace ordering is pinned workspaces first, then most recently used. Context-menu operations include pin, rename, archive, duplicate settings, refresh, close, and delete where applicable.

The Add/Open menu offers:

- Open Local Folder.
- Paste GitHub PR URL.
- Connect over SSH.
- Reopen Archived Review.

### 6.3 Center diff surface

The center header contains:

- Repository name.
- File path and change status.
- Effective base and target summary.
- Per-file additions/deletions.
- Current file and hunk position.
- Previous/next file and hunk controls.
- Diff-mode selector.
- Refresh status and action.
- Finish Review action.

Inline annotation composers render in the code surface at their selected range. Closing side panels never closes a composer or loses a selection.

### 6.4 Right files/review panel

The right panel has tabs:

- `Files`: repository tree, changed files, filters, and viewed state.
- `Comments`: comments, questions, suggestions, outdated anchors, and publication status.
- `Outline`: changed functions, types, or document sections derived from Tree-sitter when available.

Files is the default tab. Each file row shows repository, status, path, `+N -N`, annotation count, and viewed state. The tree can group by repository and directory or switch to a flat list.

### 6.5 Resizing and focus mode

- Both side panels are independently resizable.
- Suggested default widths are 240 pixels left and 300 pixels right.
- Suggested bounds are 180-420 pixels left and 240-520 pixels right.
- Both panels can collapse completely.
- Double-clicking a divider restores its default width.
- A Focus Diff command collapses both panels together.
- The application remembers widths and collapsed states per window.
- When the window is too narrow, side panels open as temporary overlays instead of shrinking the center below its usable minimum.
- With the right panel closed, the center header exposes a compact file picker and review-progress button.
- With the left panel closed, a compact workspace button exposes workspace switching.

## 7. Font zoom

Zoom changes font sizes, not whole-application geometry.

Requirements:

- `Command + Plus` on macOS and `Control + Plus` on Linux increases font scale.
- `Command + Minus` or `Control + Minus` decreases font scale.
- `Command + 0` or `Control + 0` restores the default.
- Menu commands and command-palette entries expose the same operations.
- The current percentage is shown briefly after a change and is visible in Settings.
- Zoom affects code, gutters, workspace labels, file navigation, toolbar text, annotations, prompts, and supporting text.
- Icons, panel proportions, and spacing tokens do not scale proportionally.
- Components may reflow to fit enlarged text and must never clip required controls.
- Code row height and gutter width recompute from the selected code font size.
- Zoom preserves the selected file, anchor, and nearest visible source line.
- Default scale is 100%; supported range is 75%-200% in 10% increments.
- The setting persists globally. A future per-workspace override may be added without changing stored review data.
- OS accessibility text and contrast settings remain respected independently.

## 8. Workspace discovery and configuration

### 8.1 Automatic discovery

When a local or remote directory is added, LocalReview recursively discovers Git worktrees. It recognizes both `.git` directories and `.git` files.

Discovery must:

- Ignore ordinary directories that are not repositories.
- Stop descending through repository contents by default after finding a repository.
- Avoid `.git`, common dependency caches, build outputs, and user-configured exclusions.
- Avoid following directory symlinks by default to prevent loops and workspace escape.
- Detect duplicate worktrees and normalized remote identities.
- Stream discoveries to the UI rather than blocking until the entire tree is scanned.
- Allow a user to add a repository explicitly if excluded or beyond the scan depth.
- Allow repositories to be disabled without deleting their settings.

### 8.2 Baseline precedence

Baseline resolution follows:

```text
temporary review override
  -> repository override
    -> workspace default
      -> global default
        -> release default
```

The release default is `origin/master`. Explicit CLI/GUI inputs and durable
choices saved by the application remain higher priority than file-provided
defaults. Failure to resolve the default in one repository is isolated to that
repository and does not hide changes from successful repositories.

### 8.3 Global and workspace configuration files

An optional `.localreview.toml` at the workspace root can describe shareable discovery and baseline defaults. Application state remains usable without the file. The application never modifies it without a separate explicit feature.

Example:

```toml
[workspace]
default_base = "origin/master"
discovery_depth = 4
exclude = ["vendor/**", "generated/**"]

[repositories."b"]
base = "origin/HOTFIX-1"

[repositories."experimental/large-repo"]
enabled = false
```

Repository keys are workspace-relative paths, not display names.

The identical schema is accepted as a read-only per-user global file:

- macOS: `~/Library/Application Support/LocalReview/config.toml`
- Linux: `$XDG_CONFIG_HOME/localreview/config.toml`, falling back to
  `~/.config/localreview/config.toml`

`LOCALREVIEW_CONFIG_DIR` overrides the parent directory, and
`localreview config path` reports the effective file. Configuration fields
resolve as workspace file, then global file, then release defaults. A
workspace repository table overlays matching global repository entries
field-by-field. Workspace `exclude` replaces the global relative-prefix list
when present, including when explicitly empty; built-in directory-name safety
exclusions remain active. Explicit CLI/GUI inputs are above every file layer.

## 9. Review setup and baseline controls

Before starting a review, the application shows a repository table containing:

- Include toggle.
- Relative repository path.
- Current branch or detached state.
- Clean/dirty summary.
- Effective base reference.
- Inherited/overridden indicator.
- Resolved base and merge-base SHAs.
- Ahead/behind counts where inexpensive.
- Last fetch time.
- Comparison errors.

The user can:

- Change the workspace default.
- Override one repository.
- Reset an override to inherited.
- Apply a reference to selected repositories.
- Choose a branch, remote branch, tag, commit, or arbitrary revision.
- Fetch one repository or all repositories.
- Continue with successful repositories when another repository fails.

Fetch-on-review is a global setting with a workspace override. It defaults to off. Refresh reads existing local refs unless fetch-on-review is enabled. A separate Refresh and Fetch action always makes network activity explicit.

## 10. Review capture and refresh

### 10.1 Included state

A normal local capture includes:

- Commits after the merge base.
- Staged changes.
- Unstaged changes.
- Deleted paths.
- Untracked, non-ignored files.

Ignored files are excluded. Submodule working-directory changes appear as submodule changes rather than recursively mixing the submodule's files into the parent repository.

The capture process must not mutate Git configuration, refs, the index, or working files.

### 10.2 Explicit refresh

- Filesystem watchers may detect that repository state changed.
- Detection displays a non-blocking `Changes available - Refresh` indicator.
- The active snapshot and rendered diff remain unchanged until the user presses Refresh.
- Refresh captures a new snapshot in the same review session and attempts to re-anchor annotations.
- Refresh can be cancelled.
- The last successful diff remains visible while refresh runs.
- Per-repository errors appear without replacing successful repository output.

### 10.3 Re-anchoring

Annotations are re-anchored using, in order:

1. Exact blob and line identity.
2. Selected-source match near the prior range.
3. Surrounding-context fingerprint.
4. Hunk identity and relative position.

Ambiguous or missing matches become outdated. They remain visible in review history and the Comments panel and can be manually attached to a new range.

## 11. Changed-file navigation

The right Files panel supports:

- Repository and folder grouping.
- Flat-list mode.
- Search by fuzzy path.
- Filters for repository, file status, language, annotation kind, viewed state, generated/vendor classification, and binary/text state.
- Sort by path, repository order, change size, annotation count, or review order.
- Collapse all and expand all.
- Mark viewed/unviewed.
- Bulk mark viewed for selected files.
- Review progress by file and repository.

Navigation commands distinguish files from hunks:

- Previous/next changed file.
- Previous/next hunk, crossing file boundaries when necessary.
- Previous/next annotation.
- Open file by fuzzy search.

Switching mode, collapsing a panel, or zooming preserves file, active hunk, selected annotation range, and nearest visible line.

## 12. Canonical diff model

All four presentations derive from one immutable, side-aware diff document. Diff parsing, line alignment, changed-line construction, and syntax parsing never occur during Svelte component rendering.

Conceptual Rust types:

```rust
struct ReviewDiffDocument {
    comparison_id: ComparisonId,
    file: ReviewFile,
    old: SourceDocument,
    new: SourceDocument,
    hunks: Vec<ReviewHunk>,
    changed_old_lines: RoaringBitmap,
    changed_new_lines: RoaringBitmap,
}

struct ReviewHunk {
    id: StableHunkId,
    header: HunkHeader,
    unified_rows: Vec<UnifiedRow>,
    split_rows: Vec<SplitRow>,
}

struct DiffCell {
    side: DiffSide,
    line_number: u32,
    kind: DiffLineKind,
    source_line_index: u32,
}
```

Invariants:

- Every code cell knows its real source side and line number.
- Context lines carry both old and new line identities where applicable.
- Row and hunk IDs remain stable across equivalent refreshes.
- Annotation anchors never depend on virtual-list indices.
- Presentation data is cached by repository, comparison, file fingerprint, mode, and options.
- Switching presentation mode does not rerun Git or reload unchanged blobs.

## 13. Diff views

### 13.1 Unified

- One full-width virtualized table.
- Gutters for old line, new line, change marker, annotation marker, and code.
- Conventional Git hunk order and full-width hunk headers.
- Neutral context rows, red removals, and green additions.
- Syntax foreground colors remain legible above diff backgrounds.
- Side-aware range selection.
- Inline comments and questions.
- Expandable context around hunks.
- Intraline change highlighting where computation remains bounded.

### 13.2 Split

- Before and After panes share one vertical row sequence.
- Context is aligned and neutral on both sides.
- Contiguous changes zip removed and added lines, using explicit empty cells on the shorter side.
- Vertical scrolling is synchronized.
- Horizontal scrolling may be independent.
- The pane divider is adjustable and persistent.
- A selection cannot cross sides.
- Inline annotations retain their old/new side.

### 13.3 Full File

- Shows the complete selected side: Current shows the complete new file and Base shows the complete baseline file.
- Defaults to a review-oriented Both projection that keeps complete file context while representing each changed-side block independently.
- In Both, Current additions are expanded by default and Base deletions are collapsed by default. Separate show/hide-all controls exist for additions and deletions, and every block retains its own chevron.
- Current represents baseline-only deletions as inline, collapsible red gates; Base represents current-only additions as inline, collapsible green gates.
- Each gate labels the omitted side and full line range, supports individual expansion, and participates in global show/hide-all controls.
- Switching Current and Base preserves the closest canonical line at the same viewport offset. Multi-line gates anchor at their midpoint so either version stays visually aligned around the block being inspected.
- Deleted files show the complete baseline file and a clear deleted-file banner.
- Renames display old and new paths.
- A side toggle is available when viewing both versions is meaningful.
- A change minimap indicates hunks and annotations.
- Previous/next hunk scrolls to the relevant full-file line.
- Inline annotations may be placed on changed or unchanged visible lines.

### 13.4 Difftastic

- Runs a pinned, packaged Difftastic binary behind a Rust adapter.
- Supports structural inline or side-by-side display according to available width and Difftastic capabilities.
- Uses an explicit theme background.
- Falls back to a normal line diff when Difftastic does not recognize the language, hits its resource limit, or fails to parse.
- Displays parse/fallback status without treating it as a review failure.
- Remains read-only: no line selection or inline composers in this mode.
- Provides `Show in Unified`, `Show in Split`, and `Show in Full File` actions that preserve the closest canonical file and line position.

Difftastic output is human-oriented and its JSON format is considered unstable. Its adapter must pin an exact version, validate a private normalized schema, and have golden fixtures. The canonical Git diff remains authoritative for file identity, annotations, prompt exports, and GitHub publishing.

## 14. Syntax highlighting

Unified, Split, and Full File use a Rust-side Tree-sitter highlighting service.

Requirements:

- Parse complete old/new documents so multiline syntax is correct.
- Resolve language by exact filename, extension, shebang, and optional attributes.
- Publish plain monospaced rows immediately.
- Apply token spans asynchronously without changing row geometry or scroll position.
- Cache by source fingerprint, side, language, grammar version, and theme.
- Cancel obsolete highlighting when the user changes file, mode, or session.
- Recover to plain text when a grammar is absent or fails.
- Bound cache memory with weighted least-recently-used eviction.
- Pin grammar and query versions.

Initial language coverage should include Swift, Rust, Starlark/Bazel, TOML, JSON, YAML, Markdown, shell, Python, Go, Java, C/C++, JavaScript, TypeScript, HTML, and CSS.

Automatic highlighting may be disabled above configurable size limits, initially 512 KB or 10,000 lines. Large files render immediately as plain text and offer an explicit Enable Highlighting action.

## 15. Rendering and performance architecture

The UI is a Svelte single-page application inside Tauri. Rust owns expensive and privileged operations. Svelte owns interaction and visible presentation.

The frontend must not mount one component per line for an entire large file. It uses fixed or predictably measured row virtualization with overscan. It requests presentation windows from Rust and retains a small client-side cache around the viewport.

Performance requirements:

- Restore cached application and workspace chrome within 250 ms on the reference Mac.
- Show plain rows for a typical already-captured file within 100 ms.
- Maintain responsive 60 fps scrolling on normal review files.
- Keep a 50,000-line full file usable without 50,000 DOM rows.
- Keep a 2 MB patch and 100 changed files navigable.
- Build presentation rows in linear or near-linear time outside the UI thread.
- Avoid serializing complete unchanged sources across Tauri IPC on every mode switch.
- Bound Git, parse, highlighting, and Difftastic concurrency.
- Support cancellation and discard stale job results by generation ID.
- Keep the last successful result visible during background work.

## 16. Annotations and review workflow

### 16.1 Creating annotations

- Click a gutter control to annotate one line.
- Shift-click or drag to select a same-side range.
- Highlight every selected source cell while dragging and after selection; Split highlights only the chosen old/new side.
- Choose Comment, Question, or Suggestion in the composer.
- Compose Markdown with keyboard submission and cancellation.
- Autosave unfinished drafts, including an empty-but-selected source range.
- Restore the draft body, type, labels, exact source range, and Shift-selection anchor after restart.
- Display selected code and anchor metadata before save.

### 16.2 Managing annotations

- Edit, delete, resolve, reopen, and navigate annotations.
- Group by repository and file in the Comments panel.
- Filter by kind, state, publication state, and outdated status.
- Support optional labels such as blocking, important, nit, security, performance, and question.
- Preserve creation and edit history.
- Show whether an item is local-only, included in the next GitHub review, or already published.
- Allow a published item to remain in local history even if its remote representation becomes outdated.

### 16.3 Clear and new review

Copying, saving, or submitting a prompt does not mutate annotations.

Pressing Clear:

1. Displays the number and kinds of items being cleared.
2. Creates an immutable annotation-set checkpoint in review history.
3. Starts a new empty active annotation set in the same review session.
4. Offers immediate Undo.

Starting New Review:

1. Archives the active review session and annotation set.
2. Captures a new comparison.
3. Starts with no active annotations.
4. Leaves the prior session fully browseable and exportable.

No normal UI action permanently destroys review history. Permanent retention cleanup is a separate settings operation with explicit confirmation and a backup warning.

## 17. Structured prompt exports

The primary LLM integration is deliberate copy/export. LocalReview does not call a model automatically.

### 17.1 Export scopes

The user can build a prompt from:

- All actionable feedback in the active annotation set.
- All questions in the active annotation set.
- All active comments and questions in separate sections.
- Selected annotations.
- One focused question.
- Any archived annotation set or prior review.

### 17.2 Aggregated feedback prompt

The default format is deterministic and grouped by workspace, repository, and file. It includes:

- Workspace and review identity.
- Repository name and optional local path.
- Requested base and resolved merge-base SHA.
- Target HEAD and snapshot identity.
- File path and rename information.
- Side and line range.
- Comment or suggestion text.
- Selected source.
- Bounded surrounding context.
- Relevant diff hunk.
- Outdated-anchor warning when applicable.

The instruction header asks the recipient to address every actionable item, preserve unrelated behavior, and report how each item was handled. Questions are excluded from this instruction unless explicitly included.

### 17.3 Question prompt

A focused question prompt states that the task is read-only and includes the repository, comparison, path, side, range, selected code, surrounding context, and relevant diff hunk.

Question exports support:

- One-question portable prompt.
- All-questions prompt grouped by repository and file.
- Workspace-aware format with filesystem paths for an agent that can inspect the repository.
- Portable format with more source context for an LLM without filesystem access.

### 17.4 Export behavior

- Preview before copying.
- Select/deselect included items.
- Copy to clipboard as the primary action.
- Optionally save as Markdown or a structured JSON review bundle.
- Estimate prompt size and warn before exceptionally large copies.
- Store an export record containing scope, annotation IDs, template version, and creation time.
- Never clear or mark annotations complete merely because they were exported.
- Never include unrelated terminal output, environment variables, credentials, or ignored files.

## 18. Review history, persistence, and backup

All durable state is stored under the platform application-support directory, not the workspace itself, unless the user explicitly exports a bundle.

Requirements:

- SQLite in WAL mode for metadata and relational history.
- Content-addressed blob storage for retained source excerpts, hunks, and snapshots.
- Transactional schema migrations.
- Automatic local database backups before migrations and at a bounded periodic cadence.
- Backup rotation with configurable retention and size reporting.
- Restore diagnostics when corruption is detected.
- Persist the current and all frozen review sessions, their annotations, unfinished drafts, explicit export inclusion (including “none selected”), export records, per-session UI state, review history, and application settings such as font zoom across restarts.
- Review history browseable by workspace, date, comparison, branch, and PR.
- Archived annotation sets remain exportable.
- A workspace record can be removed from the sidebar without deleting its review history.
- Permanent deletion lists exactly which history, worktree, and cached data will be affected.

## 19. GitHub PR workspaces

### 19.1 Opening a PR

A GitHub PR can be opened by:

- Pasting its URL into the application.
- Dragging a URL into the application.
- Invoking `localreview pr <url>`.
- Opening a registered deep link.

The application parses the host, owner, repository, and PR number; obtains metadata through GitHub; resolves exact base/head SHAs; prepares an isolated worktree; and starts a pinned review session.

### 19.2 Existing clones and shared repository pool

Repository preparation order is:

1. Reuse a healthy known local clone as the Git object source when it is safe and still available.
2. Otherwise locate or create an app-managed bare mirror in the shared OS-level cache.
3. Fetch only the required repository and PR refs when possible.
4. Create a unique detached review worktree in application data.

Suggested paths:

```text
OS cache directory/
  localreview/git-mirrors/<host>/<owner>/<repository>.git

Application data directory/
  localreview/reviews/<review-id>/worktree
  localreview/state.sqlite
  localreview/backups/
```

Active worktrees do not live in an OS-evictable cache. Mirrors are shared, rebuildable, reference-counted by active worktrees, and managed through a cache screen.

### 19.3 PR lifecycle

- PR base/head SHAs are pinned for the active review.
- Remote head updates display `New PR changes available` without changing the review.
- Explicit Refresh PR fetches the new head and attempts annotation re-anchoring.
- The app never edits files or exposes edit/apply operations.
- If an external process dirties an app-managed worktree, the app warns and refuses silent deletion.
- Deleting the PR review removes its worktree and prunes Git worktree metadata.
- The shared mirror remains while useful and can be removed later by safe cache cleanup.
- Startup repair detects orphaned worktree registrations and interrupted cleanup.

## 20. GitHub authentication and review publishing

### 20.1 Authentication

The initial integration reuses the GitHub CLI:

- Detect `gh` and run an authentication/status check.
- Guide the user to `gh auth login` when unavailable or logged out.
- Use the authenticated GitHub.com account for PR metadata and API operations.
- Never read or display raw credential material.
- Keep GitHub access behind a provider interface so in-app OAuth can be added later.

Public PR metadata may use unauthenticated access only when doing so does not create inconsistent behavior or surprising limits.

### 20.2 Importing GitHub state

For PR workspaces, LocalReview should display:

- Title, author, branches, commits, and current review status.
- Existing review threads and their resolved/outdated state.
- Relevant general conversation separately from inline threads.
- Whether the PR head changed since capture.

Imported GitHub comments and unpublished local annotations must remain visually distinct.

### 20.3 Finishing a review

The Finish Review flow:

1. Shows all unpublished local comments and questions selected for publication.
2. Validates every line/file anchor against the current pinned PR head.
3. Warns if GitHub cannot represent an anchor.
4. Accepts an overall review summary.
5. Lets the user choose the native GitHub conclusion: Comment, Approve, or Request Changes.
6. Shows the exact payload and item count.
7. Submits one batched GitHub review only after explicit confirmation.
8. Records remote review/comment identifiers transactionally.

Submission must be retry-safe and must not duplicate already-created comments after a timeout or crash. Partial or ambiguous failures remain visible until reconciled from GitHub.

Questions may be included as normal GitHub review comments. Any annotation can be marked local-only and omitted from publication.

## 21. CLI and desktop forwarding

### 21.1 Scope

The CLI opens or focuses content in the desktop application. It does not need to render diffs or manage review annotations headlessly.

Required commands:

```text
localreview open [path]
localreview open [path] --base <ref>
localreview open [path] --repo-base <relative-path>=<ref>
localreview workspace <name-or-id>
localreview pr <github-pr-url>
localreview ssh <host>:<absolute-path>
localreview list
localreview doctor
localreview agent --stdio
```

Behavior:

- Relative paths resolve against the CLI process working directory before forwarding.
- Opening a registered path focuses its existing workspace rather than duplicating it.
- `workspace` focuses a known workspace by exact ID or unambiguous name.
- `list` prints registered names, IDs, source tags, and availability for discoverability.
- Commands return useful exit codes and support `--json` acknowledgement output for shell integration.
- If the macOS application is not running, a local macOS CLI command launches it and waits for readiness.
- On Linux, the CLI can act as the remote companion and can forward through an established LocalReview SSH session; it does not attempt to launch a Linux GUI initially.

### 21.2 Local transport

The desktop backend exposes an authenticated per-user local RPC endpoint using a Unix-domain socket on macOS/Linux. The CLI discovers the endpoint through a protected runtime record containing no reusable remote credential.

Requirements:

- Same-user filesystem permissions.
- Per-installation authentication secret stored in protected application data.
- Request schema versioning.
- Size and path validation.
- Single application dispatcher with window focus/activation.
- Deep links limited to a strict allowlist of actions and hosts.

Tauri single-instance and deep-link facilities handle OS activation. The private RPC protocol handles structured CLI commands.

### 21.3 Remote shell forwarding

An arbitrary pre-existing SSH shell cannot securely discover a local desktop endpoint without setup. LocalReview therefore supports two explicit paths:

- From the local machine: `localreview ssh host:/path` asks the desktop app to connect and open the remote workspace.
- From a LocalReview-managed SSH session: a session-scoped reverse channel allows the Linux CLI on the remote host to forward `localreview open .` back to the macOS app.

The remote forwarding token is ephemeral, scoped to one SSH connection, and never persisted on the remote host.

## 22. SSH remote workspaces

### 22.1 Architecture

```text
Svelte UI
  <-> Tauri IPC
Local Rust review service
  <-> framed, versioned RPC over SSH stdio
Remote `localreview agent --stdio`
  <-> remote Git repositories and filesystem
```

The application launches the companion using the user's normal SSH configuration, agent, proxy jumps, and host verification. The companion performs discovery, Git operations, status capture, file reads, and optionally Difftastic near the data.

### 22.2 Protocol requirements

- Capability and protocol-version handshake.
- Framed binary messages using CBOR or MessagePack.
- Optional compression for large diffs and source payloads.
- Streaming repository discovery and progress.
- Request cancellation and timeouts.
- Job generation IDs to reject stale results.
- Filesystem-change notifications that only enable the Refresh indicator.
- Selected-file and viewport-oriented content transfer.
- Structured errors without leaking unrelated environment values.
- No generic shell-execution method exposed to the Svelte frontend.

### 22.3 Companion lifecycle

- Detect an existing compatible companion.
- Offer to install or update a signed companion when missing or incompatible.
- Verify platform and architecture.
- Prefer a user-local binary directory requiring no administrator privileges.
- Remove temporary bootstrap artifacts.
- Allow a manually installed companion for locked-down machines.
- Display remote agent version, latency, and connection status in workspace details.

### 22.4 Disconnection behavior

- Preserve the last captured review and annotations offline.
- Disable refresh and uncached file operations while disconnected.
- Reconnect explicitly or with bounded retry when the user requests it.
- Revalidate remote root identity after reconnection.
- Never interpret a transient disconnect as workspace deletion.

## 23. Additional review features

The complete product should include the following quality-of-life capabilities where they fit the core model:

- Viewed/unviewed progress that survives refreshes.
- A `Changed since my previous review` comparison using archived review snapshots.
- Commit-list context and optional commit-by-commit filtering without changing the canonical aggregate review.
- On-demand Git blame and commit details for a selected line.
- Generated, vendored, lockfile, binary, LFS, and submodule classification.
- Configurable whitespace and line-ending comparison options.
- Copy source, source with line numbers, path, hunk, patch, and provider permalink.
- Open a normal local workspace file in a configured external editor; this does not apply to app-managed PR worktrees by default.
- Light/dark/system themes, configurable code font, tab width, and font zoom.
- Command palette and customizable shortcuts.
- Optional Vim-style navigation keys.
- Multi-window support after the single-window workspace-tab workflow is stable.
- Diagnostics export with source contents and sensitive paths redacted by default.
- Signed automatic application updates.

## 24. Keyboard interaction

Final bindings must be configurable and avoid platform conflicts. Proposed defaults:

```text
Cmd/Ctrl + Plus       Increase font size
Cmd/Ctrl + Minus      Decrease font size
Cmd/Ctrl + 0          Reset font size

Cmd/Ctrl + P          Find changed file
Cmd/Ctrl + Shift + P  Command palette
Cmd/Ctrl + Enter      Save active annotation
Escape                Clear selection or dismiss composer

Option/Alt + Up       Previous hunk
Option/Alt + Down     Next hunk
Cmd/Ctrl + [          Previous changed file
Cmd/Ctrl + ]          Next changed file

Cmd/Ctrl + Shift + W  Toggle workspace rail
Cmd/Ctrl + Shift + F  Toggle files/review panel
Cmd/Ctrl + Shift + D  Focus Diff
```

Mode selection shortcuts may be assigned after checking conflicts with workspace and browser conventions.

## 25. Accessibility

- Full keyboard operation for workspace, file, hunk, code-range, and annotation navigation.
- Screen-reader labels include repository, path, side, line, change kind, and annotation count.
- Color is never the only indicator of additions, removals, selection, viewed state, or errors.
- Light and dark themes meet WCAG AA contrast for normal text.
- Enlarged font zoom reflows controls without loss of functionality.
- Reduced-motion settings disable nonessential transitions.
- Focus remains stable when virtualized rows recycle.
- Diff backgrounds and syntax foreground colors remain distinguishable in common color-vision deficiencies.

## 26. Persistence model

Core durable entities include:

- `workspace`
- `workspace_source`
- `workspace_repository`
- `workspace_ui_state`
- `review_session`
- `repository_comparison`
- `review_file`
- `review_snapshot`
- `annotation_set`
- `annotation`
- `annotation_revision`
- `prompt_export`
- `github_pull_request`
- `github_publication`
- `managed_repository_mirror`
- `managed_worktree`
- `ssh_host_profile`
- `application_setting`

Large source blobs and patches are content-addressed outside SQLite. SQLite stores hashes, metadata, ownership, and reference counts. Garbage collection removes unreferenced blobs only after backup and retention rules allow it.

## 27. Rust and Svelte architecture

Suggested repository structure:

```text
crates/
  domain/          durable and presentation-independent types
  git/             discovery, refs, snapshots, worktrees, mirrors
  diff/            canonical diff, line alignment, presentation rows
  highlight/       Tree-sitter languages, tokens, and cache
  difftastic/      pinned sidecar adapter and normalized output
  persistence/     SQLite, blob storage, migrations, and backups
  github/          GitHub.com provider using gh authentication
  protocol/        desktop CLI and SSH RPC schemas
  service/         orchestration, jobs, cancellation, and events
  cli/             local forwarding CLI and remote companion entrypoint

src-tauri/          Tauri application shell and capability declarations
ui/                 Svelte application and virtualized views
```

Architectural rules:

- The domain and service crates do not depend on Tauri or Svelte.
- Git commands are constructed from typed arguments, never shell-concatenated strings.
- The frontend receives narrowly scoped commands and events, not arbitrary filesystem or shell access.
- Provider-specific GitHub fields do not leak into generic annotation identity.
- Local, PR, and SSH sources implement the same review-source interface.
- Long-running work uses cancellable jobs with structured progress.
- Difftastic is a packaged sidecar, not a user-global dependency.
- `git` and `gh` availability are diagnosed clearly; exact packaging decisions may differ by platform.

## 28. Security and privacy

- Source code remains local unless the user explicitly copies, saves, or publishes it.
- Prompt preview shows exactly what source will leave the application through the clipboard or file.
- GitHub submission preview shows exactly what will be posted.
- Clipboard/export actions never include terminal output, environment variables, Git credential data, ignored files, or unrelated source.
- Remote SSH uses normal host-key verification and does not suppress warnings.
- CLI and deep-link inputs are untrusted and strictly validated.
- Local IPC is same-user authenticated.
- Remote forwarding tokens are short-lived and connection-scoped.
- Application logs redact credentials, URL tokens, sensitive query parameters, and source bodies by default.
- Database backups inherit restrictive user-only filesystem permissions.
- Destructive cleanup identifies exact worktrees, cache entries, and history records before execution.

## 29. Error handling and recovery

The application isolates failures at the smallest useful scope:

- One repository failure does not hide other repositories.
- One file parse/highlight failure falls back to plain text.
- Difftastic failure falls back to canonical diff.
- GitHub outage does not block local review or prompt export.
- SSH disconnection preserves the last captured review.
- Refresh failure retains the last successful snapshot.
- Database migration creates a backup before mutation.
- Interrupted PR creation is detected and cleaned or resumed on next launch.
- Ambiguous GitHub publication is reconciled from the remote before retry.

Errors include a human explanation, affected scope, retry action, and diagnostic details suitable for copying without exposing source.

## 30. Testing strategy

### 30.1 Git and comparison tests

- Multiple repositories with inherited and overridden bases.
- GitHub-style merge-base behavior.
- Committed, staged, unstaged, deleted, and untracked changes in one capture.
- Ignored files remain excluded.
- Added, deleted, renamed, copied, binary, mode-only, LFS, and submodule changes.
- Detached HEAD, unborn branch, missing remote, stale ref, and shallow clone.
- Paths containing spaces, Unicode, and unusual bytes supported by the platform.
- Capture proves it does not mutate the index or worktree.

### 30.2 Diff-model tests

- Unified side and line-number correctness.
- Split alignment for 1:1, 1:N, N:1, addition-only, and deletion-only changes.
- Stable hunk and row identity across equivalent refreshes.
- Repeated-line and missing-final-newline behavior.
- Full File symmetric addition/deletion gates, multi-line midpoint alignment, side switching, and rename behavior.
- Annotation anchor creation and re-anchoring.
- Outdated and ambiguous anchors.

### 30.3 Renderer tests

- Light/dark screenshots at narrow, normal, and wide widths.
- Both side panels expanded, collapsed, resized, and overlaying.
- Font zoom from 75% through 200% without clipping or lost position.
- Virtualized scrolling, text selection, fixed gutters, and long horizontal lines.
- Mode switching preserves navigation state.
- Inline composers remain attached while rows recycle.

### 30.4 Prompt tests

- Deterministic ordering across repositories and files.
- Feedback-only, questions-only, mixed, selected, and archived exports.
- Single-question portable and workspace-aware formats.
- Old/new ranges, renames, selected code, context, and hunks.
- Copy/export never clears annotations.
- No terminal output, environment, credentials, or unrelated blobs appear.

### 30.5 GitHub and worktree tests

- Public and authenticated PR lookup.
- Known-clone reuse and cache-mirror fallback.
- Concurrent PR worktrees for one mirror.
- Safe deletion, dirty-worktree refusal, and orphan repair.
- Head update and explicit refresh.
- Batched comment, approval, and request-changes submissions.
- Invalid/outdated anchors and retry-safe publication.

### 30.6 SSH tests

- Handshake, version mismatch, bootstrap, reconnect, and cancellation.
- High-latency and interrupted connections.
- Remote repository discovery and comparison parity with local behavior.
- Reverse forwarding from a managed SSH session.
- No generic shell execution through frontend messages.

### 30.7 Performance fixtures

- Workspace with at least 100 repositories, including non-repository siblings.
- Review with at least 100 changed files.
- 2 MB multi-hunk patch.
- 50,000-line full file with sparse changes.
- Large untracked file and binary file.
- Repeated refreshes that demonstrate bounded cache and memory growth.

## 31. Delivery sequence

This sequence manages dependencies; it does not redefine the product as only an MVP.

### Stage 1: Core workspace and review model

- Rust domain, persistence, migrations, and backups.
- macOS Tauri/Svelte shell and three-panel layout.
- Workspace rail and source filters.
- Local repository discovery and baseline overrides.
- Complete local state capture and explicit refresh.
- CLI-to-desktop forwarding on macOS.

### Stage 2: Complete local review experience

- Canonical diff model.
- Unified, Split, and Full File virtualized renderers.
- Syntax highlighting, navigation, viewed state, filters, and zoom.
- Inline comments/questions, history, Clear, and New Review.
- Structured feedback and question exports.

### Stage 3: Structural and advanced review

- Pinned Difftastic adapter and read-only view.
- Intraline highlighting, outline, blame, commit context, and generated-file policies.
- Re-anchoring polish and `Changed since previous review`.
- Performance and accessibility release gates.

### Stage 4: GitHub PR workflow

- `gh` authentication adapter.
- PR metadata and known-clone/shared-mirror preparation.
- Isolated review worktrees and lifecycle recovery.
- Existing thread import.
- Finish Review and one batched native submission.

### Stage 5: SSH and Linux companion

- Linux CLI/companion packaging.
- Versioned SSH protocol and companion bootstrap.
- Remote workspace discovery and review parity.
- Managed-session reverse forwarding.
- Remote performance, reconnect, and security hardening.

### Stage 6: Broader product polish

- Multi-window support.
- Optional Linux desktop packaging.
- Additional provider adapters.
- Configurable external command handoff and other explicitly approved integrations.

## 32. Product acceptance criteria

The complete target product is acceptable when:

1. A workspace containing repositories `a` and `b` plus non-repository `c` discovers only `a` and `b`.
2. A workspace default of `origin/master` applies to both until `b` is overridden to `origin/HOTFIX-1`.
3. Each repository uses its GitHub-style merge base and captures committed, staged, unstaged, deleted, and untracked non-ignored content.
4. A detected filesystem change never modifies the displayed review until Refresh is pressed.
5. The left workspace rail filters vertical workspace tabs by All, GitHub, Local, and SSH.
6. The center diff remains primary while the left and right panels resize, collapse, and restore without losing position.
7. The right Files panel navigates repositories/files and the Comments panel navigates review items.
8. Font zoom changes text from 75%-200% without scaling panels or losing the selected source location.
9. Unified, Split, and Full File show the same comparison with correct side-aware anchors and syntax highlighting.
10. Difftastic displays a pinned structural diff and returns cleanly to a canonical annotatable view.
11. Comments, questions, unfinished multi-line drafts, exact ranges, and explicit export selections survive restart; refresh, export, Clear, and New Review follow the history rules. Multiple sequential reviews of one workspace reopen independently with exactly one mutable current review and all prior reviews frozen.
12. The user can copy deterministic prompts for all feedback, all questions, selected items, or one focused question.
13. Pasting a GitHub PR URL creates or reuses Git objects, creates an isolated worktree, and pins the review SHAs.
14. Deleting a PR review safely removes its worktree while retaining a useful shared mirror.
15. Finish Review previews and submits all selected annotations as one native GitHub review.
16. The macOS/Linux CLI can open or focus known workspaces, open PRs, and request SSH workspaces in the macOS app.
17. A remote Linux companion can review an SSH workspace with local-like behavior and no full-workspace synchronization.
18. Large-file, large-patch, accessibility, persistence, recovery, and security test gates pass.

## 33. Deferred decisions

These decisions can be made during interaction design without changing the core product model:

- Final product name and visual identity.
- Exact mode and navigation shortcut assignments.
- Whether a Linux desktop bundle becomes a supported release target.
- Exact automatic-backup cadence and default retention limits.
- Exact cache size limits and cleanup policy.
- Whether future direct LLM integrations are embedded providers or configured external commands.
- The later interaction model for annotations in Difftastic.
- Additional forge priority after GitHub.com.

## 34. Design references

- AvestaCode's existing review prompt formatter and three-view diff specification informed the deterministic prompt ordering, stable side-aware anchors, and canonical-diff approach.
- [Tauri single-instance plugin](https://v2.tauri.app/plugin/single-instance/)
- [Tauri deep-linking plugin](https://v2.tauri.app/plugin/deep-linking/)
- [Tauri sidecar documentation](https://v2.tauri.app/develop/sidecar/)
- [Difftastic project and documented limitations](https://github.com/Wilfred/difftastic)
- [Difftastic display options, including unstable JSON output](https://difftastic.wilfred.me.uk/rustdoc/src/difft/options.rs.html)
- [GitHub pull-request review comment API](https://docs.github.com/en/rest/pulls/comments)
- [GitHub pull-request review workflow](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests/commenting-on-a-pull-request)
