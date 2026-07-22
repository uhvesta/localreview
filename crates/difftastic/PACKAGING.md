# Difftastic sidecar packaging

LocalReview pins Difftastic **0.69.0**. The macOS packaging script downloads
only the matching official Apple Silicon and Intel release archives, verifies
their SHA-256 digests, extracts `difft`, combines the executables with `lipo`,
then verifies the unsigned universal binary's architectures, version, and
final digest. It keeps those authenticated bytes in the ignored
`target/localreview-sidecars/difft-unsigned` cache, copies them into Tauri's
resources, and optionally signs that copy. Run it from the repository root
with:

```sh
scripts/package-difftastic-macos.sh --ensure # reuse only an exactly verified artifact
scripts/package-difftastic-macos.sh --check  # verify without downloading
scripts/package-difftastic-macos.sh          # force a clean rebuild

APPLE_SIGNING_IDENTITY=- scripts/package-difftastic-macos.sh --ensure
APPLE_SIGNING_IDENTITY=- scripts/package-difftastic-macos.sh --check
```

The bundled output is:

```text
src-tauri/resources/localreview-sidecars/difft
```

This generated 237 MB file and its unsigned cache are ignored by Git. The Tauri production
`beforeBuildCommand` runs the script with `--ensure`, and Tauri bundles the
tracked `resources/localreview-sidecars/` directory (including the generated
`difft`). When `APPLE_SIGNING_IDENTITY` is non-empty, the script signs that
resource before Tauri signs the enclosing app. `-` requests an ad-hoc local
signature without a timestamp; any real signing identity enables hardened
runtime and requests Apple's trusted timestamp service. Bundling the directory
lets source-only Cargo checks configure Tauri when the ignored artifact is
absent. The desktop shell passes Tauri's resolved resource directory to
`SidecarLocation::PackagedResource`; the adapter never searches `PATH` and
never downloads a sidecar at runtime. It validates the bundled executable with
`difft --version` before enabling structural mode.

Code signing changes Mach-O bytes, and removing a signature does not restore
the original byte stream. The pinned universal SHA-256 therefore applies only
to `target/localreview-sidecars/difft-unsigned`. `--check` always verifies that
raw cache first, then verifies that the bundled resource is unsigned when
`APPLE_SIGNING_IDENTITY` is empty or strictly signed with the requested
identity when it is set. Use the same identity setting for `--ensure`,
`--check`, and the Tauri build.

Ordinary source-only tests skip real-sidecar smoke coverage when the artifact
is absent. Release/packaging CI must provision it and run:

```sh
LOCALREVIEW_TEST_DIFFTASTIC_SIDECAR="$PWD/src-tauri/resources/localreview-sidecars/difft" \
  cargo test -p localreview-difftastic packaged_pinned_sidecar_smoke_when_supplied
cargo test -p localreview-desktop packaged_pinned_difftastic_flows_through_the_native_window_contract
```

Tauri packaging must sign the final app after the already signed sidecar is
included and must notarize every public DMG. For a local-only artifact, pass
the ad-hoc identity with `APPLE_SIGNING_IDENTITY=-`; production must use a
Developer ID Application identity plus Apple notarization credentials. Verify
the sidecar itself with `codesign --verify --strict`, then verify the finished
bundle with `codesign --verify --deep --strict`. The final signed app, not an
unverified archive or a user-global binary, is the trust boundary for runtime
execution.
