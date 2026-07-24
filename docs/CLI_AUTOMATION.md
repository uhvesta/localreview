# CLI automation contract

`localreview` is an authenticated client of the running desktop process. It
does not open the SQLite database itself and does not recalculate review state.
Every stateful command below is validated by the same native controller used by
the Svelte UI. This keeps CLI and desktop actions ordered in one authoritative
history.

Use `--json` for scripts. Success is one `LocalResponse` JSON object on stdout;
failure is one object on stderr with `ok: false`, a stable `code`, and a
human-readable `message`.

| Exit | Meaning |
| ---: | --- |
| `0` | Command completed |
| `1` | Local I/O, persistence, or unexpected internal failure |
| `2` | CLI usage or required confirmation missing |
| `3` | Desktop unavailable |
| `4` | Desktop rejected the operation |
| `5` | Authentication, framing, or protocol failure |

## Capability matrix

| Workflow | CLI | Machine readable | Notes |
| --- | --- | :---: | --- |
| List/open/focus/show workspaces | `list`, `open`, `workspace`, `show` | Yes | `open` reuses a canonical local workspace |
| List/reopen archived workspaces | `archived list`, `archived reopen` | Yes | No recapture while reopening |
| Inspect active review/files/annotations | `review show`, `review files`, `annotations list` | Yes | Includes immutable comparison IDs |
| Inspect diff rows and hunk headers | `review rows --mode …`, `review hunks` | Yes | Unified, split, full, and difftastic-compatible rows |
| Inspect repository setup | `repo list` | Yes | Includes actionable configuration issues |
| Workspace/per-repository bases | `repo configure` | Yes | Empty `=ref` value resets an override |
| Include/exclude repositories | `repo include`, `repo exclude` | Yes | Repository IDs come from `repo list` |
| Start/archive prior review round | `review start` | Yes | `--fetch` is explicit |
| Refresh active review | `review refresh` | Yes | Preserves annotations according to native refresh rules |
| Multi-line feedback/questions/suggestions | `annotate` | Yes | `--start-line` and `--end-line` are inclusive |
| File/review notes | `annotate --kind file_note/review_note` | Yes | File ID still pins ownership; line range must be zero |
| Script/file/stdin annotation bodies | `annotate --body`, `--body-file`, or `--body -` | Yes | UTF-8, bounded to 1 MiB |
| List/update/delete/resolve annotations | `annotations …`, `annotate --id …` | Yes | Updates retain the durable ID |
| Mark files viewed/unviewed | `viewed [--clear]` | Yes | Uses active review UI state |
| Query definitions/references | `symbol --kind …` | Yes | Lazy, repository-owned, bounded results |
| Feedback/questions/full prompt | `prompt --scope …` | Yes | Prompt export is durably recorded |
| Path/diff/Git prompt options | `prompt --path-style …` | Yes | Diff hunks and Git state are opt-in |
| Review/export history | `review history` | Yes | Returns durable checkpoints, exports, and rounds |
| Archive workspace | `archive --confirm` | Yes | Review history remains recoverable |
| Delete workspace state | `delete --confirm-name <exact-name>` | Yes | Native clean-worktree safeguards still apply |
| Import/open GitHub PR | `pr <url>` | Yes | Uses native provider/cache path |
| Inspect PR/threads/conversation | `github inspect` | Yes | Read-only |
| Preview batched GitHub review | `github preview` | Yes | Produces a durable opaque preview token |
| Submit batched GitHub review | `github submit --confirm` | Yes | Only the preview token crosses submit boundary |
| Open SSH workspace | `ssh <host:/path>` | Yes | Restricted companion protocol |
| SSH status/reconnect | `ssh-status [--reconnect]` | Yes | No capture unless separately refreshed |
| Global config location | `config path` | Yes | Does not require desktop |
| Effective durable settings | `config effective` | Yes | Shows precedence, workspace, repository and UI values |

Not exposed intentionally:

- Clipboard and native save dialogs: scripts receive prompt bytes on stdout.
- External-editor launching and window layout mutations: these are interactive
  presentation actions, not review-state automation.
- Arbitrary Git, shell, filesystem, SSH, or GitHub API calls.
- GitHub submission without a prior native preview and explicit `--confirm`.
- Permanent deletion without the exact current workspace display name.

## Examples

```sh
localreview --json open ~/work --base origin/main \
  --repo-base b=origin/HOTFIX-1

workspace_id="$(
  localreview --json list |
  jq -r '.workspaces[] | select(.name == "work") | .id'
)"

localreview --json repo list "$workspace_id"
localreview --json review start "$workspace_id"

review_json="$(localreview --json review show "$workspace_id")"
file_id="$(jq -r '.data.files[0].id' <<<"$review_json")"

localreview --json annotate "$workspace_id" "$file_id" \
  --kind comment --side new --start-line 12 --end-line 16 \
  --body 'Please simplify this range.'

localreview --json annotate "$workspace_id" "$file_id" \
  --kind question --side new --start-line 22 --end-line 22 \
  --body-file ./question.md

printf '%s\n' 'Please simplify this range.' |
  localreview --json annotate "$workspace_id" "$file_id" \
    --kind comment --side new --start-line 12 --end-line 16 --body -

localreview --json annotations list "$workspace_id"
localreview --json symbol "$workspace_id" <repository-id> changed_one \
  --kind definitions

localreview prompt "$workspace_id" --scope feedback --path-style absolute
localreview prompt "$workspace_id" --scope questions --path-style qualified
localreview prompt "$workspace_id" --scope full --path-style portable

localreview --json github preview "$workspace_id" \
  --annotation <annotation-id> --conclusion comment \
  --summary 'Local review'
# Inspect the returned payload and previewToken before the only write:
localreview --json github submit "$workspace_id" <preview-token> --confirm
```

JSON field names are camelCase within `data`. The outer response is tagged
with `programmatic`, includes the request ID and stable operation name, and is
versioned by the desktop/CLI protocol handshake.
