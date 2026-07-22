//! Typed, bounded operations available to an SSH companion.
//!
//! This is intentionally a review protocol, not a remote-control protocol.
//! In particular there is no `exec`, `shell`, arbitrary Git argument, or
//! arbitrary file-system operation in this module.

use crate::{
    validate_identifier, validate_path, validate_ref, validate_relative_path, ProtocolError,
    PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};

/// Requests beyond this depth are rejected before the companion walks the
/// remote tree. This keeps a hostile or accidental client from turning a
/// review request into an unbounded scan.
pub const MAX_REMOTE_DISCOVERY_DEPTH: u16 = 32;
/// A source window is deliberately viewport-sized. Larger source is obtained
/// with more windows rather than one giant request.
pub const MAX_REMOTE_SOURCE_WINDOW_LINES: u32 = 4_096;
/// A line-count limit alone is insufficient because a source file can contain
/// very long lines.  This cap keeps a single viewport result well below the
/// framed-message limit even when every requested line is large.
pub const MAX_REMOTE_SOURCE_WINDOW_BYTES: usize = 2 * 1024 * 1024;
/// Capture is metadata-first.  This bound prevents an accidental massive
/// repository from producing a manifest that consumes the full frame budget.
pub const MAX_REMOTE_CAPTURE_FILES: usize = 50_000;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapability {
    RepositoryDiscovery,
    ComparisonCapture,
    SourceWindows,
    ChangeNotifications,
    Cancellation,
    ZstdCompression,
    ReverseForwarding,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentHello {
    pub protocol_versions: Vec<u16>,
    pub agent_version: String,
    pub capabilities: Vec<AgentCapability>,
    /// OS and architecture are reported separately so bootstrap selection does
    /// not infer either from an SSH host alias.
    pub platform: String,
    pub architecture: String,
}

impl AgentHello {
    #[must_use]
    pub fn current(
        agent_version: impl Into<String>,
        platform: impl Into<String>,
        architecture: impl Into<String>,
    ) -> Self {
        Self {
            protocol_versions: vec![PROTOCOL_VERSION],
            agent_version: agent_version.into(),
            capabilities: vec![
                AgentCapability::RepositoryDiscovery,
                AgentCapability::ComparisonCapture,
                AgentCapability::SourceWindows,
                AgentCapability::ChangeNotifications,
                AgentCapability::Cancellation,
                AgentCapability::ZstdCompression,
                AgentCapability::ReverseForwarding,
            ],
            platform: platform.into(),
            architecture: architecture.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRequest {
    pub id: String,
    /// Each desktop job gets a monotonically increasing generation. A client
    /// must discard a response/progress message from an earlier generation.
    pub generation: u64,
    pub operation: AgentOperation,
}

impl AgentRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier(&self.id, "agent request id")?;
        if self.generation == 0
            && !matches!(
                self.operation,
                AgentOperation::Handshake { .. }
                    | AgentOperation::Ping
                    | AgentOperation::ConfigureManagedForwardRelay { .. }
            )
        {
            return Err(ProtocolError::InvalidInput(
                "review operations require a non-zero generation".into(),
            ));
        }
        self.operation.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteRepositoryRef {
    /// Absolute root supplied when the remote workspace was opened.
    pub workspace_root: String,
    /// `.` represents the workspace root repository. Every other value is a
    /// normalized relative path below `workspace_root`.
    pub relative_path: String,
}

impl RemoteRepositoryRef {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_absolute_remote_path(&self.workspace_root, "remote workspace root")?;
        if self.relative_path == "." {
            return Ok(());
        }
        validate_relative_path(&self.relative_path)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteRepository {
    pub reference: RemoteRepositoryRef,
    pub canonical_worktree: String,
    pub git_common_dir: Option<String>,
    pub primary_remote: Option<String>,
    pub head: RemoteHead,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RemoteHead {
    Branch(String),
    Detached(String),
    Unborn,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteComparisonOptions {
    pub ignore_all_whitespace: bool,
    pub ignore_space_at_eol: bool,
    pub ignore_cr_at_eol: bool,
    #[serde(default)]
    pub path_filters: Vec<String>,
}

impl RemoteComparisonOptions {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.path_filters.len() > 4_096 {
            return Err(ProtocolError::InvalidInput(
                "too many remote comparison path filters".into(),
            ));
        }
        self.path_filters
            .iter()
            .try_for_each(|path| validate_relative_path(path))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteCapturedFile {
    /// Current path for additions, modifications, renames and copies; old
    /// path for deletions.  This mirrors Git's `--raw -z` representation.
    pub path: String,
    /// Set for rename/copy records. It is never inferred from a display patch.
    pub old_path: Option<String>,
    pub status: RemoteFileStatus,
    /// Rename/copy similarity as reported by Git's raw status token. It is
    /// absent for every other status and always in the inclusive 0..=100
    /// range.
    pub similarity_percent: Option<u8>,
    /// Git file modes from the raw diff manifest. A gitlink is represented by
    /// mode `160000` and `status: submodule`, not by following its directory.
    pub old_mode: u32,
    pub new_mode: u32,
    /// Immutable Git object identifiers where Git has one. A worktree-side
    /// object is commonly absent because Git reports an all-zero sentinel.
    pub old_object_id: Option<String>,
    pub new_object_id: Option<String>,
    pub untracked: bool,
    pub binary: bool,
    pub lfs_pointer: bool,
    /// Byte length of immutable captured target bytes, when such bytes exist.
    /// Content itself is deliberately fetched through `read_source_window`.
    pub captured_byte_len: Option<u64>,
    /// The independent review layers in which this path participates. This
    /// preserves staged/unstaged/committed information without sending their
    /// full patch bodies during capture.
    pub layers: Vec<RemoteChangeLayer>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteChangeLayer {
    Committed,
    Staged,
    Unstaged,
    Untracked,
}

/// The complete file-level change classification emitted by the local Git
/// manifest.  It preserves valid changes that have no textual hunk, including
/// mode-only, type, binary, LFS, and gitlink/submodule changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    ModeChanged,
    TypeChanged,
    Submodule,
    Untracked,
}

/// Counts for a metadata-only comparison layer. Protocol v4 deliberately does
/// not compute or return eager patch bodies (or patch-body digests) during a
/// capture. Conventional diffs are derived from requested source viewports.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteLayerSummary {
    pub changed_files: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteComparisonCapture {
    /// Opaque, single-session identifier for the immutable source snapshot.
    /// Every source request must repeat it, so a response from an older
    /// capture cannot silently attach to a newer comparison generation.
    pub capture_id: String,
    /// Review generation which owns this capture. Every source request must
    /// repeat it in addition to the opaque capture ID.
    pub generation: u64,
    pub repository: RemoteRepositoryRef,
    pub requested_base: String,
    pub base_tip_sha: String,
    pub merge_base_sha: String,
    pub head_sha: Option<String>,
    pub head: RemoteHead,
    pub committed: RemoteLayerSummary,
    pub staged: RemoteLayerSummary,
    pub unstaged: RemoteLayerSummary,
    pub untracked: RemoteLayerSummary,
    /// Metadata only. Source bytes are streamed in bounded windows from Git
    /// objects or a stale-checked worktree identity owned by `capture_id`.
    pub files: Vec<RemoteCapturedFile>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSourceRevision {
    Worktree,
    Head,
    MergeBase,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteSourceWindow {
    pub capture_id: String,
    pub capture_generation: u64,
    pub repository: RemoteRepositoryRef,
    pub path: String,
    pub revision: RemoteSourceRevision,
    pub start_line: u32,
    pub total_lines: u32,
    pub byte_len: u64,
    /// SHA-256 of the exact source revision. The desktop can safely combine
    /// separately requested windows only when this value also matches.
    pub content_sha256_hex: String,
    /// Exact UTF-8 bytes for the requested contiguous logical-line window.
    /// Newline sequences are preserved verbatim (including CRLF and a final
    /// newline). The desktop concatenates adjacent windows and verifies the
    /// complete byte length and SHA-256 before it builds a canonical document.
    /// The companion rejects binary/NUL-bearing source before this is sent.
    pub bytes: Vec<u8>,
    pub end_of_file: bool,
}

/// There intentionally is no `exec` or `shell` variant. The service can only
/// ask a companion to carry out review-domain work on a bounded path/range.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentOperation {
    Handshake {
        desktop_versions: Vec<u16>,
    },
    Ping,
    /// Configures a companion-owned, same-user Unix relay for one desktop
    /// managed reverse tunnel. This credential-bearing control frame travels
    /// inside the already-authenticated SSH stdio channel, never in remote
    /// command argv, environment, a shell, or persistent storage.
    ConfigureManagedForwardRelay {
        endpoint: String,
        token_hex: String,
        session_id: String,
    },
    DiscoverRepositories {
        root: String,
        max_depth: u16,
    },
    CaptureComparison {
        repository: RemoteRepositoryRef,
        base: String,
        #[serde(default)]
        options: RemoteComparisonOptions,
    },
    ReadSourceWindow {
        capture_id: String,
        capture_generation: u64,
        repository: RemoteRepositoryRef,
        path: String,
        revision: RemoteSourceRevision,
        /// One-based line number.
        start_line: u32,
        line_count: u32,
    },
    /// Starts a bounded polling watcher. It sends only "refresh available"
    /// notifications; it never refreshes the review itself.
    WatchRepositoryChanges {
        repository: RemoteRepositoryRef,
        poll_interval_millis: u32,
    },
    /// Watches many already-discovered repositories over one dedicated SSH
    /// transport. It emits the same notification-only refresh signal as the
    /// per-repository operation, avoiding one child process per repository.
    WatchWorkspaceChanges {
        repositories: Vec<RemoteRepositoryRef>,
        poll_interval_millis: u32,
    },
    Cancel {
        request_id: String,
    },
}

impl AgentOperation {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Handshake { desktop_versions } => {
                if desktop_versions.is_empty() || desktop_versions.len() > 16 {
                    return Err(ProtocolError::InvalidInput(
                        "desktop protocol versions must contain 1..=16 entries".into(),
                    ));
                }
            }
            Self::Ping => {}
            Self::ConfigureManagedForwardRelay {
                endpoint,
                token_hex,
                session_id,
            } => {
                let address: std::net::SocketAddr = endpoint.parse().map_err(|_| {
                    ProtocolError::InvalidInput(
                        "managed forward endpoint must be a loopback socket address".into(),
                    )
                })?;
                if !address.ip().is_loopback()
                    || address.port() == 0
                    || token_hex.len() != 64
                    || !token_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || session_id.is_empty()
                    || session_id.len() > 64
                    || !session_id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    return Err(ProtocolError::InvalidInput(
                        "managed forward relay configuration is invalid".into(),
                    ));
                }
            }
            Self::DiscoverRepositories { root, max_depth } => {
                validate_absolute_remote_path(root, "remote workspace root")?;
                if *max_depth > MAX_REMOTE_DISCOVERY_DEPTH {
                    return Err(ProtocolError::InvalidInput(format!(
                        "remote discovery depth exceeds {MAX_REMOTE_DISCOVERY_DEPTH}"
                    )));
                }
            }
            Self::CaptureComparison {
                repository,
                base,
                options,
            } => {
                repository.validate()?;
                validate_ref(base, "remote comparison base")?;
                options.validate()?;
            }
            Self::ReadSourceWindow {
                capture_id,
                capture_generation,
                repository,
                path,
                start_line,
                line_count,
                ..
            } => {
                validate_identifier(capture_id, "remote capture id")?;
                if *capture_generation == 0 {
                    return Err(ProtocolError::InvalidInput(
                        "remote capture generation must be non-zero".into(),
                    ));
                }
                repository.validate()?;
                validate_relative_path(path)?;
                if *start_line == 0
                    || *line_count == 0
                    || *line_count > MAX_REMOTE_SOURCE_WINDOW_LINES
                {
                    return Err(ProtocolError::InvalidInput(format!(
                        "remote source window must request 1..={MAX_REMOTE_SOURCE_WINDOW_LINES} lines"
                    )));
                }
            }
            Self::WatchRepositoryChanges {
                repository,
                poll_interval_millis,
            } => {
                repository.validate()?;
                if !(250..=60_000).contains(poll_interval_millis) {
                    return Err(ProtocolError::InvalidInput(
                        "remote watcher interval must be between 250ms and 60s".into(),
                    ));
                }
            }
            Self::WatchWorkspaceChanges {
                repositories,
                poll_interval_millis,
            } => {
                if repositories.is_empty() || repositories.len() > 1_024 {
                    return Err(ProtocolError::InvalidInput(
                        "remote workspace watcher must include 1..=1024 repositories".into(),
                    ));
                }
                let mut seen = std::collections::BTreeSet::new();
                let mut root = None::<&str>;
                for repository in repositories {
                    repository.validate()?;
                    if let Some(root) = root {
                        if root != repository.workspace_root {
                            return Err(ProtocolError::InvalidInput(
                                "remote workspace watcher repositories must share one root".into(),
                            ));
                        }
                    } else {
                        root = Some(&repository.workspace_root);
                    }
                    if !seen.insert((&repository.workspace_root, &repository.relative_path)) {
                        return Err(ProtocolError::InvalidInput(
                            "remote workspace watcher has duplicate repositories".into(),
                        ));
                    }
                }
                if !(250..=60_000).contains(poll_interval_millis) {
                    return Err(ProtocolError::InvalidInput(
                        "remote watcher interval must be between 250ms and 60s".into(),
                    ));
                }
            }
            Self::Cancel { request_id } => validate_identifier(request_id, "cancel request id")?,
        }
        Ok(())
    }
}

fn validate_absolute_remote_path(value: &str, label: &str) -> Result<(), ProtocolError> {
    validate_path(value, label)?;
    if !value.starts_with('/') {
        return Err(ProtocolError::InvalidInput(format!(
            "{label} must be an absolute POSIX path"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentResponse {
    pub id: String,
    pub generation: u64,
    pub result: AgentResult,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentResult {
    Handshake {
        selected_version: u16,
        hello: AgentHello,
    },
    Pong,
    ManagedForwardRelayConfigured,
    Repositories {
        repositories: Vec<RemoteRepository>,
    },
    ComparisonCapture {
        capture: RemoteComparisonCapture,
    },
    SourceWindow {
        window: RemoteSourceWindow,
    },
    Watching,
    CancelAccepted {
        request_id: String,
    },
    Cancelled,
    Error {
        error: AgentError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentError {
    pub code: AgentErrorCode,
    pub scope: AgentErrorScope,
    pub message: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AgentErrorScope {
    Request,
    WorkspaceRoot(String),
    Repository(RemoteRepositoryRef),
    SourcePath {
        repository: RemoteRepositoryRef,
        path: String,
    },
}

impl AgentError {
    #[must_use]
    pub fn request(code: AgentErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            scope: AgentErrorScope::Request,
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    NotFound,
    PathDenied,
    GitFailed,
    BinaryContent,
    StaleCapture,
    StaleGeneration,
    TooLarge,
    Cancelled,
    TimedOut,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "message",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AgentMessage {
    Request(AgentRequest),
    Response(AgentResponse),
    Progress(AgentProgress),
    Notification(AgentNotification),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProgress {
    pub id: String,
    pub generation: u64,
    pub phase: AgentProgressPhase,
    pub completed: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProgressPhase {
    Validating,
    Discovering,
    ResolvingBase,
    Capturing,
    ReadingSource,
    Watching,
    Complete,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentNotification {
    /// This only enables the Refresh indicator. It does not alter an existing
    /// review snapshot, preserving the explicit-refresh invariant.
    FilesystemChangesAvailable {
        repository: RemoteRepositoryRef,
        generation: u64,
    },
    ConnectionStatus {
        connected: bool,
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_has_no_generic_execution_operation() {
        let encoded = serde_cbor::to_vec(&AgentOperation::Ping).unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("shell"));
        assert!(AgentHello::current("test", "linux", "aarch64")
            .protocol_versions
            .contains(&PROTOCOL_VERSION));
    }

    #[test]
    fn managed_relay_configuration_is_generation_zero_and_loopback_scoped() {
        let operation = AgentOperation::ConfigureManagedForwardRelay {
            endpoint: "127.0.0.1:50001".into(),
            token_hex: "ab".repeat(32),
            session_id: "session_123".into(),
        };
        assert!(operation.validate().is_ok());
        assert!(AgentRequest {
            id: "managed-relay".into(),
            generation: 0,
            operation,
        }
        .validate()
        .is_ok());
        assert!(AgentOperation::ConfigureManagedForwardRelay {
            endpoint: "10.0.0.1:50001".into(),
            token_hex: "ab".repeat(32),
            session_id: "session_123".into(),
        }
        .validate()
        .is_err());
    }

    #[test]
    fn remote_paths_and_windows_are_bounded() {
        let ref_ = RemoteRepositoryRef {
            workspace_root: "/work".into(),
            relative_path: "a".into(),
        };
        assert!(AgentOperation::ReadSourceWindow {
            capture_id: "capture-1-1".into(),
            capture_generation: 1,
            repository: ref_.clone(),
            path: "src/lib.rs".into(),
            revision: RemoteSourceRevision::Worktree,
            start_line: 1,
            line_count: 30,
        }
        .validate()
        .is_ok());
        assert!(AgentOperation::ReadSourceWindow {
            capture_id: "capture-1-1".into(),
            capture_generation: 1,
            repository: ref_,
            path: "../secrets".into(),
            revision: RemoteSourceRevision::Worktree,
            start_line: 1,
            line_count: 1,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn workspace_watcher_keeps_many_repositories_on_one_bounded_request() {
        let repositories = (0..100)
            .map(|index| RemoteRepositoryRef {
                workspace_root: "/work".into(),
                relative_path: format!("repo-{index}"),
            })
            .collect::<Vec<_>>();
        assert!(AgentOperation::WatchWorkspaceChanges {
            repositories,
            poll_interval_millis: 1_000,
        }
        .validate()
        .is_ok());
        assert!(AgentOperation::WatchWorkspaceChanges {
            repositories: vec![
                RemoteRepositoryRef {
                    workspace_root: "/work-a".into(),
                    relative_path: "repo".into(),
                },
                RemoteRepositoryRef {
                    workspace_root: "/work-b".into(),
                    relative_path: "repo".into(),
                },
            ],
            poll_interval_millis: 1_000,
        }
        .validate()
        .is_err());
    }

    #[test]
    fn source_window_round_trips_exact_crlf_bytes() {
        let repository = RemoteRepositoryRef {
            workspace_root: "/work".into(),
            relative_path: ".".into(),
        };
        let response = AgentResponse {
            id: "source-1".into(),
            generation: 1,
            result: AgentResult::SourceWindow {
                window: RemoteSourceWindow {
                    capture_id: "capture-1".into(),
                    capture_generation: 1,
                    repository,
                    path: "src/lib.rs".into(),
                    revision: RemoteSourceRevision::Worktree,
                    start_line: 1,
                    total_lines: 2,
                    byte_len: 10,
                    content_sha256_hex: "0".repeat(64),
                    bytes: b"one\r\ntwo\r\n".to_vec(),
                    end_of_file: true,
                },
            },
        };
        let mut encoded = Vec::new();
        crate::write_frame(&mut encoded, &response).unwrap();
        let decoded: AgentResponse = crate::read_frame(&mut encoded.as_slice()).unwrap();
        let AgentResult::SourceWindow { window } = decoded.result else {
            panic!("source window should survive the framed transport");
        };
        assert_eq!(window.bytes, b"one\r\ntwo\r\n");
    }
}
