# LocalReview

LocalReview is a local-first macOS code-review desktop app with a macOS/Linux
CLI and SSH companion. A workspace can contain many Git repositories; each one
is reviewed from its GitHub-style merge base to an immutable capture of its
committed, staged, unstaged, deleted, and untracked changes.

The complete product and technical contract lives in
[`PRODUCT_SPEC.md`](PRODUCT_SPEC.md).

## Development

Prerequisites:

- Rust 1.78 or newer
- Node.js 22 or newer
- Git
- `gh`, authenticated with `gh auth login`, for GitHub PR reviews
- macOS 11 or newer for the desktop app

Run the browser development fixture:

```sh
npm --prefix ui install
npm --prefix ui run dev
```

Run the native desktop app:

```sh
npx --yes @tauri-apps/cli@2 dev --config src-tauri/tauri.conf.json
```

Build a locally runnable, ad-hoc-signed macOS app/DMG and the companion CLI:

```sh
APPLE_SIGNING_IDENTITY=- npx --yes @tauri-apps/cli@2 build --config src-tauri/tauri.conf.json --bundles app,dmg --ci
cargo build --release -p localreview-cli
```

The `-` identity creates a valid local ad-hoc signature, but it is not a
distributable production signature. A public DMG must instead set
`APPLE_SIGNING_IDENTITY` to a Developer ID Application identity and provide
Tauri's Apple notarization credentials. Always verify the finished app after
all resources have been bundled:

```sh
codesign --verify --strict \
  target/release/bundle/macos/LocalReview.app/Contents/Resources/localreview-sidecars/difft
codesign --verify --deep --strict target/release/bundle/macos/LocalReview.app
spctl --assess --type execute --verbose target/release/bundle/macos/LocalReview.app
```

The Tauri macOS build automatically provisions the ignored, generated
Difftastic 0.69.0 sidecar before bundling. It downloads the pinned Apple
Silicon and Intel release archives, verifies their SHA-256 digests, creates a
universal executable, and verifies its architectures, version, and final raw
digest. The verified unsigned binary is cached under the ignored `target/`
tree. A copy is placed in Tauri resources and, when
`APPLE_SIGNING_IDENTITY` is set, signed before Tauri signs the enclosing app.
Use `-` for an ad-hoc local signature; a production identity enables hardened
runtime and a trusted timestamp. To provision or verify it independently:

```sh
scripts/package-difftastic-macos.sh --ensure
scripts/package-difftastic-macos.sh --check

APPLE_SIGNING_IDENTITY=- scripts/package-difftastic-macos.sh --ensure
APPLE_SIGNING_IDENTITY=- scripts/package-difftastic-macos.sh --check
```

Run the script without an option to force a clean rebuild. The generated file
at `src-tauri/resources/localreview-sidecars/difft` is intentionally ignored;
do not commit it or the ignored `target/localreview-sidecars/difft-unsigned`
cache, and do not use a user-global `difft` installation. The pinned universal
SHA-256 authenticates only the unsigned cache because code signing necessarily
changes Mach-O bytes. `--check` verifies the cache and the resource mode
selected by `APPLE_SIGNING_IDENTITY`; run it with the same identity setting as
the build. Linux companion assets remain pinned in the Rust adapter and are
packaged by their target's release pipeline.

Run all source gates:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm --prefix ui run check
npm --prefix ui test
npm --prefix ui run build
```

Ordinary source gates do not require the generated sidecar. Release/packaging
CI should provision it and exercise both adapter and desktop integration smoke
tests:

```sh
scripts/package-difftastic-macos.sh --ensure
LOCALREVIEW_TEST_DIFFTASTIC_SIDECAR="$PWD/src-tauri/resources/localreview-sidecars/difft" \
  cargo test -p localreview-difftastic packaged_pinned_sidecar_smoke_when_supplied
cargo test -p localreview-desktop packaged_pinned_difftastic_flows_through_the_native_window_contract
```

## CLI

The CLI forwards validated requests to the authenticated, same-user desktop
endpoint:

```text
localreview open [path]
localreview open [path] --base <ref>
localreview open [path] --repo-base <relative-path>=<ref>
localreview workspace <name-or-id>
localreview focus <name-or-id> # alias for workspace
localreview pr <github-pr-url>
localreview ssh <host>:<absolute-path>
localreview list
localreview doctor
localreview recover status
localreview recover restore <backup-file-name> --confirm
localreview agent --stdio
```

`open` canonicalizes the folder and reuses its durable workspace when it is
already registered; `workspace`/`focus` selects an existing live workspace
without recapturing it. On macOS the CLI starts LocalReview when necessary. On
Linux it forwards to an already-running authenticated desktop endpoint, or
through the managed SSH reverse channel when invoked in a connected remote
shell.

Workspace lifecycle actions are deliberately separate. **Start new review**
archives the current review round and immediately captures a clean round for
the same workspace. **Archive** removes the workspace from the live rail while
keeping every captured review, inline comment/question, prompt export, and UI
location recoverable in **Review history** after restart. **Delete** requires
typing the workspace name and permanently purges that LocalReview data,
including its entries in retained app backups. Local folders and remote files
are never deleted. An app-owned GitHub PR worktree is removed only when clean,
while the shared repository cache is retained.

GitHub PR reviews expose Feedback, Questions, and Full prompt exports from the
review Actions menu as well as the Comments panel. New exports identify the
canonical `owner/repository#number`, remain pinned to the captured comparison,
and include read-only `gh` commands for retrieving the current PR metadata,
diff, files, reviews, and comments. The prompt explicitly treats provider text
as untrusted context and never authorizes posting or mutating GitHub.

The primary **Copy review prompt** action exports feedback only; questions and
the combined Full scope are always deliberate separate choices. Prompt
formatting defaults persist across restarts: Portable, Qualified, or Absolute
paths, plus opt-in checkboxes for relevant diff hunks and Git revision state.
Absolute local prompts use complete working-tree paths. GitHub PR prompts use
pinned GitHub identities and never expose app-owned cache or worktree paths.

`localreview agent --stdio` is the bounded remote companion protocol, not a
general shell-execution API. A manually installed companion may be placed in
`~/.local/bin/localreview`; managed installs require a release-signed companion
manifest accepted by the desktop bootstrapper.

Recovery never silently resets application state. If startup reports database
corruption, `recover status` prints source-free diagnostics and the exact safe
backup names; `recover restore` validates the selected backup and preserves the
corrupt database and WAL sidecars before replacement.

## Workspace configuration

An optional read-only `.localreview.toml` at the workspace root can share
discovery and baseline defaults:

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

Explicit CLI/GUI inputs take precedence. LocalReview never rewrites this file.

## Syntax highlighting

Unified, Split, and Full File views use pinned Tree-sitter grammars and render
semantic byte spans as escaped text. The native grammar bundle currently
covers Rust, JavaScript, TypeScript/TSX, JSON, Python, Markdown, shell, Swift,
Starlark/Bazel, TOML, YAML, Go, Java, C, C++, HTML/XML/SVG, CSS, Ruby, PHP, C#,
Kotlin, Lua, Scala, R, Elixir, OCaml, SQL, Nix, and Zig. For local Git
worktrees, resolution prefers a recognized `linguist-language` override from
Git's `.gitattributes` rules, then special filenames, extensions, and
shebangs. Remote captures currently use the latter deterministic rules.

An unknown language always renders as safe monospaced plain text. Svelte, Vue,
and Astro also use that fallback until mixed-language injection grammars are
pinned. HCL/Terraform, Dart, Perl, and Dockerfile are recognized but currently
fall back to plain text because their available Rust grammar packages require
an incompatible Tree-sitter ABI or API. This fallback never hides source or
changes diff/annotation geometry.

## Data and privacy

Application state, backups, content-addressed captured blobs, GitHub mirrors,
and managed PR worktrees are kept under the per-user LocalReview data root with
user-only permissions. Source leaves the app only through an explicit copy,
export, or GitHub Finish Review action. Filesystem and remote change
notifications only enable Refresh; they never replace the active capture.
