//! Packaged Difftastic sidecar adapter.
//!
//! Difftastic's JSON is deliberately treated as a private, version-pinned wire
//! format. This crate validates it and returns a small stable schema for the
//! review UI. It never uses a user-global `difft` binary or a shell command.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use thiserror::Error;

/// The exact upstream Difftastic release LocalReview packages and accepts.
/// It was selected from the upstream 0.69.0 release published 2026-04-30.
pub const SUPPORTED_DIFFTASTIC_VERSION: &str = "0.69.0";
pub const NORMALIZED_SCHEMA_VERSION: u16 = 1;

/// Verified official archive digests. Packaging downloads one matching archive,
/// verifies it before extraction, then bundles only the executable in Tauri's
/// resources. Runtime does not download executables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarAsset {
    pub target: &'static str,
    pub archive_name: &'static str,
    pub sha256: &'static str,
}

pub const SIDECAR_ASSETS: &[SidecarAsset] = &[
    SidecarAsset {
        target: "aarch64-apple-darwin",
        archive_name: "difft-aarch64-apple-darwin.tar.gz",
        sha256: "c958b87885a5825a356c5899ac7ecdd752a7942084199f2be4bc0bf8c9de8e33",
    },
    SidecarAsset {
        target: "x86_64-apple-darwin",
        archive_name: "difft-x86_64-apple-darwin.tar.gz",
        sha256: "5f5487e7a6e817194a1cef297d2ffb300454371635a4cde865087dbc064730a2",
    },
    SidecarAsset {
        target: "aarch64-unknown-linux-gnu",
        archive_name: "difft-aarch64-unknown-linux-gnu.tar.gz",
        sha256: "abd2f42d2afd424312b4862aa7c7bb0320447670ae22fabcc5159db03e2dccbd",
    },
    SidecarAsset {
        target: "x86_64-unknown-linux-gnu",
        archive_name: "difft-x86_64-unknown-linux-gnu.tar.gz",
        sha256: "038db96a0e8fce69f2554e33e04ff75fbf6f96ea45cb4edb9ed6203a2c4750ff",
    },
    SidecarAsset {
        target: "x86_64-unknown-linux-musl",
        archive_name: "difft-x86_64-unknown-linux-musl.tar.gz",
        sha256: "c120a4315b33e89678d52b47ea0097cdb1fb57b4f3910b4d77cbeee5eecc8ced",
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarLocation {
    /// `resource_dir` is supplied by Tauri's `PathResolver::resource_dir`.
    PackagedResource { resource_dir: PathBuf },
    /// Allows unit/integration tests and a signed bootstrapper to nominate a
    /// concrete file. This is intentionally not a `PATH` lookup.
    Explicit { executable: PathBuf },
}

impl SidecarLocation {
    pub fn resolve(&self) -> Result<PathBuf, DifftasticError> {
        let candidate = match self {
            Self::Explicit { executable } => executable.clone(),
            Self::PackagedResource { resource_dir } => {
                let executable = sidecar_filename();
                let nested = resource_dir.join("localreview-sidecars").join(executable);
                if nested.is_file() {
                    nested
                } else {
                    resource_dir.join(executable)
                }
            }
        };
        if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(DifftasticError::SidecarNotFound(candidate))
        }
    }
}

#[must_use]
pub fn sidecar_filename() -> &'static str {
    if cfg!(windows) {
        "difft.exe"
    } else {
        "difft"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifftasticDisplay {
    Inline,
    SideBySide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifftasticBackground {
    Dark,
    Light,
}

impl DifftasticBackground {
    const fn argument(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifftasticInput {
    /// Display-only, canonical path. It is never passed verbatim as a shell
    /// fragment and remains authoritative even though the temp file differs.
    pub path: PathBuf,
    pub content: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifftasticRequest {
    pub old: DifftasticInput,
    pub new: DifftasticInput,
    pub display: DifftasticDisplay,
    pub background: DifftasticBackground,
    pub width: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifftasticPolicy {
    pub timeout: Duration,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_stderr_bytes: usize,
    pub graph_limit: u32,
    pub parse_error_limit: u32,
}

impl Default for DifftasticPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(8),
            max_input_bytes: 512 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            graph_limit: 250_000,
            parse_error_limit: 0,
        }
    }
}

/// Typed command construction. It is intentionally inspectable in tests and
/// has no string concatenation or shell parsing step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DifftasticCommand {
    pub executable: PathBuf,
    pub display: DifftasticDisplay,
    pub background: DifftasticBackground,
    pub width: u16,
    pub max_input_bytes: usize,
    pub graph_limit: u32,
    pub parse_error_limit: u32,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
}

impl DifftasticCommand {
    #[must_use]
    pub fn arguments(&self) -> Vec<OsString> {
        vec![
            OsString::from("--display"),
            OsString::from("json"),
            OsString::from("--background"),
            OsString::from(self.background.argument()),
            OsString::from("--color"),
            OsString::from("never"),
            OsString::from("--byte-limit"),
            OsString::from(self.max_input_bytes.to_string()),
            OsString::from("--graph-limit"),
            OsString::from(self.graph_limit.to_string()),
            OsString::from("--parse-error-limit"),
            OsString::from(self.parse_error_limit.to_string()),
            OsString::from("--width"),
            OsString::from(self.width.max(20).to_string()),
            self.old_path.as_os_str().to_owned(),
            self.new_path.as_os_str().to_owned(),
        ]
    }

    fn spawn(&self) -> io::Result<Child> {
        let mut command = Command::new(&self.executable);
        command
            .args(self.arguments())
            // Difftastic intentionally gates its unstable JSON format. The
            // adapter pins 0.69.0 and validates its private schema below.
            .env("DFT_UNSTABLE", "yes")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn()
    }
}

#[derive(Clone, Debug)]
pub struct DifftasticCancellation(Arc<AtomicBool>);

impl DifftasticCancellation {
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for DifftasticCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct DifftasticAdapter {
    executable: PathBuf,
    policy: DifftasticPolicy,
}

impl DifftasticAdapter {
    pub fn from_location(
        location: SidecarLocation,
        policy: DifftasticPolicy,
    ) -> Result<Self, DifftasticError> {
        Ok(Self {
            executable: location.resolve()?,
            policy,
        })
    }

    #[must_use]
    pub fn new_for_testing(executable: PathBuf, policy: DifftasticPolicy) -> Self {
        Self { executable, policy }
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Validates the bundled binary, not a system-wide program, before the
    /// review service enables Difftastic mode.
    pub fn verify_pinned_version(&self) -> Result<(), DifftasticError> {
        let mut command = Command::new(&self.executable);
        command
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().map_err(|source| DifftasticError::Launch {
            executable: self.executable.clone(),
            source,
        })?;
        let output = run_bounded(
            child,
            self.policy.timeout,
            self.policy.max_output_bytes.min(16 * 1024),
            self.policy.max_stderr_bytes.min(16 * 1024),
            None,
        )
        .map_err(DifftasticError::VersionCheckFailed)?;
        if output.stdout.exceeded {
            return Err(DifftasticError::VersionCheckFailed(
                DifftasticFallbackReason::OutputLimit,
            ));
        }
        if !output.status.success() {
            return Err(DifftasticError::NonZeroExit {
                status: output.status.code(),
                stderr: redact_output(&output.stderr.bytes),
            });
        }
        let version = String::from_utf8_lossy(&output.stdout.bytes);
        if version_contains_supported(&version) {
            Ok(())
        } else {
            Err(DifftasticError::UnsupportedVersion {
                expected: SUPPORTED_DIFFTASTIC_VERSION,
                actual: version.trim().to_owned(),
            })
        }
    }

    /// Runs structural diffing against private temp snapshots. All failure
    /// modes become a clean canonical-diff fallback instead of a review error.
    #[must_use]
    pub fn render(
        &self,
        request: &DifftasticRequest,
        cancellation: Option<&DifftasticCancellation>,
    ) -> DifftasticOutcome {
        if request.old.content.len() > self.policy.max_input_bytes
            || request.new.content.len() > self.policy.max_input_bytes
        {
            return DifftasticOutcome::fallback(DifftasticFallbackReason::InputTooLarge);
        }
        if std::str::from_utf8(&request.old.content).is_err()
            || std::str::from_utf8(&request.new.content).is_err()
        {
            return DifftasticOutcome::fallback(DifftasticFallbackReason::BinaryInput);
        }
        if cancellation.is_some_and(DifftasticCancellation::is_cancelled) {
            return DifftasticOutcome::fallback(DifftasticFallbackReason::Cancelled);
        }
        let temp = match TempSnapshots::create(request) {
            Ok(temp) => temp,
            Err(error) => {
                return DifftasticOutcome::fallback_with_detail(DifftasticFallbackReason::Io, error)
            }
        };
        let command = DifftasticCommand {
            executable: self.executable.clone(),
            display: request.display,
            background: request.background,
            width: request.width,
            max_input_bytes: self.policy.max_input_bytes,
            graph_limit: self.policy.graph_limit,
            parse_error_limit: self.policy.parse_error_limit,
            old_path: temp.old_path.clone(),
            new_path: temp.new_path.clone(),
        };
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return DifftasticOutcome::fallback_with_detail(
                    DifftasticFallbackReason::SidecarUnavailable,
                    error.to_string(),
                );
            }
        };
        let run = run_bounded(
            child,
            self.policy.timeout,
            self.policy.max_output_bytes,
            self.policy.max_stderr_bytes,
            cancellation,
        );
        let output = match run {
            Ok(output) => output,
            Err(reason) => return DifftasticOutcome::fallback(reason),
        };
        if output.stdout.exceeded {
            return DifftasticOutcome::fallback(DifftasticFallbackReason::OutputLimit);
        }
        if !output.status.success() {
            return DifftasticOutcome::fallback_with_detail(
                DifftasticFallbackReason::ProcessFailure,
                format!(
                    "difftastic exited {:?}: {}",
                    output.status.code(),
                    redact_output(&output.stderr.bytes)
                ),
            );
        }
        let old = String::from_utf8_lossy(&request.old.content);
        let new = String::from_utf8_lossy(&request.new.content);
        match normalize_json_output(&output.stdout.bytes, &old, &new, request.display) {
            Ok(document) => {
                if document.line_oriented {
                    DifftasticOutcome::fallback(DifftasticFallbackReason::LineOrParseFallback)
                } else {
                    DifftasticOutcome::Structural(document)
                }
            }
            Err(error) => DifftasticOutcome::fallback_with_detail(
                DifftasticFallbackReason::InvalidSidecarOutput,
                error.to_string(),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifftasticFallbackReason {
    SidecarUnavailable,
    InputTooLarge,
    BinaryInput,
    OutputLimit,
    Timeout,
    Cancelled,
    ProcessFailure,
    LineOrParseFallback,
    InvalidSidecarOutput,
    Io,
}

/// Tells the caller to preserve its canonical selected file and closest line
/// while switching to Unified, Split, or Full File. No annotation identity is
/// ever derived from Difftastic rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalFallback {
    pub reason: DifftasticFallbackReason,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum DifftasticOutcome {
    Structural(DifftasticPresentation),
    CanonicalFallback(CanonicalFallback),
}

impl DifftasticOutcome {
    fn fallback(reason: DifftasticFallbackReason) -> Self {
        Self::CanonicalFallback(CanonicalFallback {
            reason,
            detail: None,
        })
    }

    fn fallback_with_detail(reason: DifftasticFallbackReason, detail: String) -> Self {
        Self::CanonicalFallback(CanonicalFallback {
            reason,
            detail: Some(detail),
        })
    }
}

/// Stable, private normalized presentation schema. This is the only data the
/// UI should receive; it must not parse Difftastic's upstream JSON itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifftasticPresentation {
    pub schema_version: u16,
    /// The UI layout selected for this normalized structural result. JSON is
    /// layout-independent; the renderer consumes this field without asking
    /// Difftastic to re-run for an inline/side-by-side toggle.
    pub display: DifftasticDisplay,
    pub language: String,
    pub status: DifftasticFileStatus,
    pub line_oriented: bool,
    pub alignment: Vec<DifftasticAlignment>,
    pub chunks: Vec<DifftasticChunk>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifftasticFileStatus {
    Unchanged,
    Changed,
    Created,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifftasticAlignment {
    /// 1-based canonical line numbers. `None` represents an explicit empty
    /// side in a structural side-by-side row.
    pub old_line_number: Option<u32>,
    pub new_line_number: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifftasticChunk {
    pub rows: Vec<DifftasticRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifftasticRow {
    pub old: Option<DifftasticCell>,
    pub new: Option<DifftasticCell>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifftasticCell {
    pub line_number: u32,
    pub text: String,
    pub changed_spans: Vec<DifftasticSpan>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DifftasticSpan {
    pub start_byte: u32,
    pub end_byte: u32,
    pub highlight: DifftasticHighlight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifftasticHighlight {
    Delimiter,
    Normal,
    String,
    Type,
    Comment,
    Keyword,
    TreeSitterError,
}

#[derive(Debug, Error)]
pub enum DifftasticError {
    #[error("packaged difftastic sidecar was not found at {0}")]
    SidecarNotFound(PathBuf),
    #[error("could not start bundled difftastic at {executable}: {source}")]
    Launch {
        executable: PathBuf,
        source: io::Error,
    },
    #[error("difftastic exited unsuccessfully ({status:?}): {stderr}")]
    NonZeroExit { status: Option<i32>, stderr: String },
    #[error("unsupported difftastic version: expected {expected}, got {actual}")]
    UnsupportedVersion {
        expected: &'static str,
        actual: String,
    },
    #[error("could not safely verify packaged difftastic: {0:?}")]
    VersionCheckFailed(DifftasticFallbackReason),
    #[error("invalid pinned difftastic JSON: {0}")]
    InvalidJson(String),
    #[error("invalid pinned difftastic JSON schema: {0}")]
    InvalidSchema(String),
}

struct TempSnapshots {
    _directory: TempDir,
    old_path: PathBuf,
    new_path: PathBuf,
}

impl TempSnapshots {
    fn create(request: &DifftasticRequest) -> Result<Self, String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let old_path = snapshot_path(directory.path(), "old", &request.old.path);
        let new_path = snapshot_path(directory.path(), "new", &request.new.path);
        fs::write(&old_path, &request.old.content).map_err(|error| error.to_string())?;
        fs::write(&new_path, &request.new.content).map_err(|error| error.to_string())?;
        Ok(Self {
            _directory: directory,
            old_path,
            new_path,
        })
    }
}

fn snapshot_path(directory: &Path, side: &str, logical_path: &Path) -> PathBuf {
    let filename = logical_path
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| OsStr::new("review.txt"));
    directory.join(format!("{side}-{}", filename.to_string_lossy()))
}

struct BoundedOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct ProcessOutput {
    status: ExitStatus,
    stdout: BoundedOutput,
    stderr: BoundedOutput,
}

fn run_bounded(
    mut child: Child,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancellation: Option<&DifftasticCancellation>,
) -> Result<ProcessOutput, DifftasticFallbackReason> {
    let stdout = child
        .stdout
        .take()
        .ok_or(DifftasticFallbackReason::ProcessFailure)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(DifftasticFallbackReason::ProcessFailure)?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, stderr_limit));
    let started = Instant::now();
    let status = loop {
        if cancellation.is_some_and(DifftasticCancellation::is_cancelled) {
            let _ = child.kill();
            let _ = child.wait();
            join_readers(stdout_reader, stderr_reader);
            return Err(DifftasticFallbackReason::Cancelled);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            join_readers(stdout_reader, stderr_reader);
            return Err(DifftasticFallbackReason::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                join_readers(stdout_reader, stderr_reader);
                return Err(DifftasticFallbackReason::ProcessFailure);
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| DifftasticFallbackReason::ProcessFailure)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| DifftasticFallbackReason::ProcessFailure)?;
    Ok(ProcessOutput {
        status,
        stdout,
        stderr,
    })
}

fn join_readers(
    stdout: thread::JoinHandle<BoundedOutput>,
    stderr: thread::JoinHandle<BoundedOutput>,
) {
    let _ = stdout.join();
    let _ = stderr.join();
}

fn read_bounded(mut reader: impl Read, limit: usize) -> BoundedOutput {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let remaining = limit.saturating_sub(bytes.len());
                let copied = remaining.min(count);
                bytes.extend_from_slice(&buffer[..copied]);
                exceeded |= copied < count;
            }
        }
    }
    BoundedOutput { bytes, exceeded }
}

fn version_contains_supported(value: &str) -> bool {
    value
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '.'))
        .any(|part| part == SUPPORTED_DIFFTASTIC_VERSION)
}

fn redact_output(bytes: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(bytes);
    let limited: String = rendered.chars().take(1_000).collect();
    limited.replace('\n', " ")
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRoot {
    One(RawFile),
    Many(Vec<RawFile>),
}

#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(default)]
    aligned_lines: Vec<(Option<u32>, Option<u32>)>,
    #[serde(default)]
    chunks: Vec<Vec<RawRow>>,
    language: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct RawRow {
    lhs: Option<RawCell>,
    rhs: Option<RawCell>,
}

#[derive(Debug, Deserialize)]
struct RawCell {
    line_number: u32,
    #[serde(default)]
    changes: Vec<RawChange>,
}

#[derive(Debug, Deserialize)]
struct RawChange {
    start: u32,
    end: u32,
    content: String,
    highlight: RawHighlight,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawHighlight {
    Delimiter,
    Normal,
    String,
    Type,
    Comment,
    Keyword,
    TreeSitterError,
}

impl From<RawHighlight> for DifftasticHighlight {
    fn from(value: RawHighlight) -> Self {
        match value {
            RawHighlight::Delimiter => Self::Delimiter,
            RawHighlight::Normal => Self::Normal,
            RawHighlight::String => Self::String,
            RawHighlight::Type => Self::Type,
            RawHighlight::Comment => Self::Comment,
            RawHighlight::Keyword => Self::Keyword,
            RawHighlight::TreeSitterError => Self::TreeSitterError,
        }
    }
}

fn normalize_json_output(
    bytes: &[u8],
    old_source: &str,
    new_source: &str,
    display: DifftasticDisplay,
) -> Result<DifftasticPresentation, DifftasticError> {
    let root: RawRoot = serde_json::from_slice(bytes)
        .map_err(|error| DifftasticError::InvalidJson(error.to_string()))?;
    let raw = match root {
        RawRoot::One(file) => file,
        RawRoot::Many(mut files) if files.len() == 1 => files.remove(0),
        RawRoot::Many(files) => {
            return Err(DifftasticError::InvalidSchema(format!(
                "expected exactly one result, got {}",
                files.len()
            )));
        }
    };
    let status = parse_status(&raw.status)?;
    let line_oriented = raw.language.eq_ignore_ascii_case("text")
        || raw.language.eq_ignore_ascii_case("binary")
        || raw.language.eq_ignore_ascii_case("plaintext");
    let old_lines = source_lines(old_source);
    let new_lines = source_lines(new_source);
    let alignment = raw
        .aligned_lines
        .into_iter()
        .map(|(old, new)| {
            Ok(DifftasticAlignment {
                old_line_number: validate_line_number(old, &old_lines, "old alignment")?,
                new_line_number: validate_line_number(new, &new_lines, "new alignment")?,
            })
        })
        .collect::<Result<Vec<_>, DifftasticError>>()?;
    let chunks = raw
        .chunks
        .into_iter()
        .map(|rows| {
            rows.into_iter()
                .map(|row| {
                    Ok(DifftasticRow {
                        old: normalize_cell(row.lhs, &old_lines, "old")?,
                        new: normalize_cell(row.rhs, &new_lines, "new")?,
                    })
                })
                .collect::<Result<Vec<_>, DifftasticError>>()
                .map(|rows| DifftasticChunk { rows })
        })
        .collect::<Result<Vec<_>, DifftasticError>>()?;
    Ok(DifftasticPresentation {
        schema_version: NORMALIZED_SCHEMA_VERSION,
        display,
        language: raw.language,
        status,
        line_oriented,
        alignment,
        chunks,
    })
}

fn parse_status(value: &str) -> Result<DifftasticFileStatus, DifftasticError> {
    match value {
        "unchanged" => Ok(DifftasticFileStatus::Unchanged),
        "changed" => Ok(DifftasticFileStatus::Changed),
        "created" => Ok(DifftasticFileStatus::Created),
        "deleted" => Ok(DifftasticFileStatus::Deleted),
        other => Err(DifftasticError::InvalidSchema(format!(
            "unknown file status {other:?}"
        ))),
    }
}

fn source_lines(source: &str) -> Vec<&str> {
    source.split('\n').collect()
}

fn validate_line_number(
    raw: Option<u32>,
    lines: &[&str],
    context: &str,
) -> Result<Option<u32>, DifftasticError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let index = usize::try_from(raw)
        .map_err(|_| DifftasticError::InvalidSchema(format!("{context} line is too large")))?;
    if index >= lines.len() {
        return Err(DifftasticError::InvalidSchema(format!(
            "{context} line {raw} is outside source"
        )));
    }
    raw.checked_add(1)
        .map(Some)
        .ok_or_else(|| DifftasticError::InvalidSchema(format!("{context} line number overflow")))
}

fn normalize_cell(
    raw: Option<RawCell>,
    lines: &[&str],
    side: &str,
) -> Result<Option<DifftasticCell>, DifftasticError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let line_number =
        validate_line_number(Some(raw.line_number), lines, side)?.ok_or_else(|| {
            DifftasticError::InvalidSchema(format!("{side} cell unexpectedly lacks a line number"))
        })?;
    let source = lines[usize::try_from(raw.line_number).unwrap_or_default()];
    let mut spans = Vec::with_capacity(raw.changes.len());
    for change in raw.changes {
        let start = usize::try_from(change.start).map_err(|_| {
            DifftasticError::InvalidSchema(format!("{side} change start is too large"))
        })?;
        let end = usize::try_from(change.end).map_err(|_| {
            DifftasticError::InvalidSchema(format!("{side} change end is too large"))
        })?;
        if start >= end
            || end > source.len()
            || !source.is_char_boundary(start)
            || !source.is_char_boundary(end)
        {
            return Err(DifftasticError::InvalidSchema(format!(
                "{side} change range {start}..{end} is invalid"
            )));
        }
        if source[start..end] != change.content {
            return Err(DifftasticError::InvalidSchema(format!(
                "{side} change content does not match source"
            )));
        }
        spans.push(DifftasticSpan {
            start_byte: change.start,
            end_byte: change.end,
            highlight: change.highlight.into(),
        });
    }
    spans.sort_by_key(|span| (span.start_byte, span.end_byte));
    if spans
        .windows(2)
        .any(|pair| pair[0].end_byte > pair[1].start_byte)
    {
        return Err(DifftasticError::InvalidSchema(format!(
            "{side} change spans overlap"
        )));
    }
    Ok(Some(DifftasticCell {
        line_number,
        text: source.to_owned(),
        changed_spans: spans,
    }))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn fixture_request() -> DifftasticRequest {
        DifftasticRequest {
            old: DifftasticInput {
                path: PathBuf::from("src/example.rs"),
                content: b"fn greet() {\n    println!(\"old\");\n}\n".to_vec(),
            },
            new: DifftasticInput {
                path: PathBuf::from("src/example.rs"),
                content: b"fn greet() {\n    println!(\"new\");\n}\n".to_vec(),
            },
            display: DifftasticDisplay::SideBySide,
            background: DifftasticBackground::Dark,
            width: 120,
        }
    }

    #[test]
    fn normalizes_pinned_json_fixture_to_private_schema() {
        let request = fixture_request();
        let output = include_bytes!("../fixtures/difftastic-0.69.0-rust.json");
        let old = String::from_utf8(request.old.content).expect("fixture is utf8");
        let new = String::from_utf8(request.new.content).expect("fixture is utf8");
        let document = normalize_json_output(output, &old, &new, DifftasticDisplay::SideBySide)
            .expect("valid pinned fixture");
        assert_eq!(document.schema_version, NORMALIZED_SCHEMA_VERSION);
        assert_eq!(document.status, DifftasticFileStatus::Changed);
        assert_eq!(document.language, "Rust");
        assert_eq!(document.alignment[1].old_line_number, Some(2));
        let row = &document.chunks[0].rows[0];
        assert_eq!(row.old.as_ref().map(|cell| cell.line_number), Some(2));
        assert_eq!(
            row.new.as_ref().map(|cell| cell.text.as_str()),
            Some("    println!(\"new\");")
        );
        assert_eq!(
            row.new.as_ref().map(|cell| cell.changed_spans.len()),
            Some(1)
        );
    }

    #[test]
    fn rejects_json_that_cannot_anchor_to_source() {
        let error = normalize_json_output(
            br#"{"language":"Rust","status":"changed","chunks":[[{"lhs":{"line_number":9,"changes":[]}}]]}"#,
            "fn x() {}\n",
            "fn x() {}\n",
            DifftasticDisplay::Inline,
        )
        .expect_err("invalid source line must not leak into UI");
        assert!(matches!(error, DifftasticError::InvalidSchema(_)));
    }

    #[test]
    fn typed_command_has_no_shell_and_preserves_arguments() {
        let command = DifftasticCommand {
            executable: PathBuf::from("/bundle/difft"),
            display: DifftasticDisplay::Inline,
            background: DifftasticBackground::Light,
            width: 0,
            max_input_bytes: 512,
            graph_limit: 7,
            parse_error_limit: 0,
            old_path: PathBuf::from("/tmp/a path;$(unsafe).rs"),
            new_path: PathBuf::from("/tmp/b path.rs"),
        };
        let arguments = command.arguments();
        assert_eq!(arguments[0], "--display");
        assert_eq!(arguments[1], "json");
        assert!(!arguments.iter().any(|arg| arg == "side-by-side-show-both"));
        assert!(!arguments.iter().any(|arg| arg == "inline"));
        assert!(arguments.iter().any(|arg| arg == "20"));
        assert!(arguments
            .iter()
            .any(|arg| arg == "/tmp/a path;$(unsafe).rs"));
    }

    #[test]
    fn version_validation_is_exact_not_prefix_based() {
        assert!(version_contains_supported("Difftastic 0.69.0\n"));
        assert!(!version_contains_supported("Difftastic 0.69.01\n"));
        assert!(!version_contains_supported("Difftastic 0.68.9\n"));
    }

    #[test]
    fn sidecar_manifest_has_mac_and_linux_assets() {
        assert_eq!(SIDECAR_ASSETS.len(), 5);
        assert!(SIDECAR_ASSETS.iter().all(|asset| asset.sha256.len() == 64));
        assert!(SIDECAR_ASSETS
            .iter()
            .any(|asset| asset.target == "aarch64-apple-darwin"));
        assert!(SIDECAR_ASSETS
            .iter()
            .any(|asset| asset.target == "x86_64-unknown-linux-gnu"));
    }

    /// Opt-in real-binary smoke test used by release/packaging CI. It is
    /// skipped for ordinary source-only test runs because the sidecar is not a
    /// developer-global dependency.
    #[test]
    fn packaged_pinned_sidecar_smoke_when_supplied() {
        let Some(executable) = std::env::var_os("LOCALREVIEW_TEST_DIFFTASTIC_SIDECAR") else {
            return;
        };
        let adapter = DifftasticAdapter::new_for_testing(
            PathBuf::from(executable),
            DifftasticPolicy::default(),
        );
        adapter
            .verify_pinned_version()
            .expect("expected pinned sidecar");
        match adapter.render(&fixture_request(), None) {
            DifftasticOutcome::Structural(document) => {
                assert_eq!(document.schema_version, NORMALIZED_SCHEMA_VERSION);
                assert_eq!(document.language, "Rust");
                assert!(!document.chunks.is_empty());
            }
            DifftasticOutcome::CanonicalFallback(reason) => {
                panic!("real pinned sidecar unexpectedly fell back: {reason:?}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_output_limits_fall_back_without_unbounded_capture() {
        let script = test_script("sleep 1\n");
        let adapter = DifftasticAdapter::new_for_testing(
            script.clone(),
            DifftasticPolicy {
                timeout: Duration::from_millis(25),
                ..DifftasticPolicy::default()
            },
        );
        assert_eq!(
            adapter.render(&fixture_request(), None),
            DifftasticOutcome::fallback(DifftasticFallbackReason::Timeout)
        );
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    #[test]
    fn a_real_process_result_is_normalized_or_falls_back() {
        let fixture = include_str!("../fixtures/difftastic-0.69.0-rust.json");
        let script = test_script(&format!(
            "printf '%s' '{}'\n",
            fixture.replace('\'', "'\\''")
        ));
        let adapter =
            DifftasticAdapter::new_for_testing(script.clone(), DifftasticPolicy::default());
        match adapter.render(&fixture_request(), None) {
            DifftasticOutcome::Structural(value) => assert_eq!(value.language, "Rust"),
            DifftasticOutcome::CanonicalFallback(value) => panic!("unexpected fallback: {value:?}"),
        }
        let _ = fs::remove_file(script);
    }

    #[cfg(unix)]
    fn test_script(body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("localreview-difftastic-test-{nonce}.sh"));
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("write test sidecar");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("chmod test sidecar");
        path
    }
}
