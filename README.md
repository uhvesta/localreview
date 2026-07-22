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
codesign --verify --deep --strict target/release/bundle/macos/LocalReview.app
spctl --assess --type execute --verbose target/release/bundle/macos/LocalReview.app
```

The Tauri macOS build automatically provisions the ignored, generated
Difftastic 0.69.0 sidecar before bundling. It downloads the pinned Apple
Silicon and Intel release archives, verifies their SHA-256 digests, creates a
universal executable, and verifies its architectures, version, and final
digest. To provision or verify it independently:

```sh
scripts/package-difftastic-macos.sh --ensure
scripts/package-difftastic-macos.sh --check
```

Run the script without an option to force a clean rebuild. The generated file
at `src-tauri/resources/localreview-sidecars/difft` is intentionally ignored;
do not commit it or use a user-global `difft` installation. Linux companion
assets remain pinned in the Rust adapter and are packaged by their target's
release pipeline.

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
localreview pr <github-pr-url>
localreview ssh <host>:<absolute-path>
localreview list
localreview doctor
localreview recover status
localreview recover restore <backup-file-name> --confirm
localreview agent --stdio
```

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

## Data and privacy

Application state, backups, content-addressed captured blobs, GitHub mirrors,
and managed PR worktrees are kept under the per-user LocalReview data root with
user-only permissions. Source leaves the app only through an explicit copy,
export, or GitHub Finish Review action. Filesystem and remote change
notifications only enable Refresh; they never replace the active capture.
