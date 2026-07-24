use crate::{AuthProof, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};

/// A path/ref override provided when opening a workspace.  Paths are always
/// workspace-relative; the desktop validates them before passing work on.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryBaseOverride {
    pub relative_path: String,
    pub base: String,
}

/// Closed, non-shell automation operations exposed by the authenticated local
/// desktop endpoint.  These deliberately use transport-owned primitive types
/// so the CLI protocol does not depend on Tauri/controller implementation
/// structs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProgrammaticOperation {
    ShowWorkspace {
        selector: String,
    },
    ListArchivedWorkspaces,
    ReopenArchivedWorkspace {
        workspace_id: String,
    },
    LoadReview {
        workspace_id: String,
    },
    ReviewFiles {
        workspace_id: String,
    },
    ListAnnotations {
        workspace_id: String,
    },
    ReviewRows {
        file_id: String,
        #[serde(default = "default_diff_mode")]
        mode: String,
    },
    ReviewHunks {
        file_id: String,
    },
    RepositorySetup {
        workspace_id: String,
    },
    ConfigureBaselines {
        workspace_id: String,
        default_base: Option<String>,
        #[serde(default)]
        repository_bases: Vec<ProgrammaticRepositoryBase>,
    },
    SetRepositoryInclusion {
        workspace_id: String,
        repository_ids: Vec<String>,
        enabled: bool,
    },
    StartReview {
        workspace_id: String,
        #[serde(default)]
        fetch: bool,
    },
    RefreshReview {
        workspace_id: String,
        #[serde(default)]
        fetch: bool,
    },
    AddAnnotation {
        workspace_id: String,
        annotation: ProgrammaticAnnotation,
    },
    DeleteAnnotation {
        workspace_id: String,
        annotation_id: String,
    },
    SetAnnotationState {
        workspace_id: String,
        annotation_id: String,
        state: String,
    },
    MarkFileViewed {
        workspace_id: String,
        file_id: String,
        viewed: bool,
    },
    QuerySymbols {
        workspace_id: String,
        repository_id: String,
        symbol: String,
        #[serde(default = "default_symbol_kind")]
        kind: String,
        limit: Option<u16>,
    },
    GeneratePrompt {
        workspace_id: String,
        scope: String,
        #[serde(default)]
        annotation_ids: Vec<String>,
        #[serde(default)]
        path_style: Option<String>,
        #[serde(default)]
        include_diff_hunks: bool,
        #[serde(default)]
        include_git_state: bool,
        #[serde(default)]
        history_id: Option<String>,
    },
    ReviewHistory {
        workspace_id: String,
    },
    ArchiveWorkspace {
        workspace_id: String,
        confirmation: String,
    },
    DeleteWorkspace {
        workspace_id: String,
        confirmation: String,
    },
    GithubInspect {
        workspace_id: String,
    },
    GithubPreviewReview {
        workspace_id: String,
        #[serde(default)]
        annotation_ids: Vec<String>,
        summary: String,
        conclusion: String,
    },
    GithubSubmitReview {
        workspace_id: String,
        preview_token: String,
        confirm: bool,
    },
    SshStatus {
        workspace_id: String,
        #[serde(default)]
        reconnect: bool,
    },
    EffectiveSettings {
        workspace_id: String,
    },
}

fn default_diff_mode() -> String {
    "unified".into()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgrammaticRepositoryBase {
    pub repository_id: Option<String>,
    pub relative_path: Option<String>,
    pub base: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgrammaticAnnotation {
    #[serde(default)]
    pub id: Option<String>,
    pub file_id: String,
    pub kind: String,
    #[serde(default = "default_annotation_side")]
    pub side: String,
    #[serde(default)]
    pub start_line: u32,
    #[serde(default)]
    pub end_line: u32,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_true")]
    pub local_only: bool,
}

fn default_annotation_side() -> String {
    "new".into()
}

fn default_true() -> bool {
    true
}

fn default_symbol_kind() -> String {
    "all".into()
}

/// The set of desktop actions the CLI is permitted to request.  Keeping this
/// enum closed is an important security boundary: it is not a shell protocol.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalCommand {
    OpenWorkspace {
        path: String,
        base: Option<String>,
        #[serde(default)]
        repository_bases: Vec<RepositoryBaseOverride>,
    },
    FocusWorkspace {
        selector: String,
    },
    OpenPullRequest {
        url: String,
    },
    OpenSshWorkspace {
        target: String,
    },
    ListWorkspaces,
    Doctor,
    Programmatic {
        command: ProgrammaticOperation,
    },
}

impl LocalCommand {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::OpenWorkspace {
                path,
                base,
                repository_bases,
            } => {
                validate_path(path, "workspace path")?;
                if let Some(base) = base {
                    validate_ref(base, "base reference")?;
                }
                for override_ in repository_bases {
                    validate_relative_path(&override_.relative_path)?;
                    validate_ref(&override_.base, "repository base reference")?;
                }
            }
            Self::FocusWorkspace { selector } => {
                validate_text(selector, "workspace selector", 256)?
            }
            Self::OpenPullRequest { url } => validate_github_pr_url(url)?,
            Self::OpenSshWorkspace { target } => validate_ssh_target(target)?,
            Self::Programmatic { command } => command.validate()?,
            Self::ListWorkspaces | Self::Doctor => {}
        }
        Ok(())
    }
}

impl ProgrammaticOperation {
    fn validate(&self) -> Result<(), ProtocolError> {
        let workspace = |value: &str| validate_identifier(value, "workspace id");
        let repository = |value: &str| validate_identifier(value, "repository id");
        let file = |value: &str| validate_identifier(value, "file id");
        match self {
            Self::ShowWorkspace { selector } => validate_text(selector, "workspace selector", 256)?,
            Self::ListArchivedWorkspaces => {}
            Self::ReopenArchivedWorkspace { workspace_id }
            | Self::LoadReview { workspace_id }
            | Self::ReviewFiles { workspace_id }
            | Self::ListAnnotations { workspace_id }
            | Self::RepositorySetup { workspace_id }
            | Self::StartReview { workspace_id, .. }
            | Self::RefreshReview { workspace_id, .. }
            | Self::ReviewHistory { workspace_id }
            | Self::ArchiveWorkspace { workspace_id, .. }
            | Self::DeleteWorkspace { workspace_id, .. }
            | Self::GithubInspect { workspace_id }
            | Self::GithubSubmitReview { workspace_id, .. }
            | Self::SshStatus { workspace_id, .. }
            | Self::EffectiveSettings { workspace_id } => workspace(workspace_id)?,
            Self::ReviewRows { file_id, mode } => {
                file(file_id)?;
                if !matches!(mode.as_str(), "unified" | "split" | "full" | "difftastic") {
                    return Err(ProtocolError::InvalidInput(
                        "diff mode must be unified, split, full, or difftastic".into(),
                    ));
                }
            }
            Self::ReviewHunks { file_id } => file(file_id)?,
            Self::ConfigureBaselines {
                workspace_id,
                default_base,
                repository_bases,
            } => {
                workspace(workspace_id)?;
                if let Some(base) = default_base {
                    validate_ref(base, "default base")?;
                }
                if repository_bases.len() > 10_000 {
                    return Err(ProtocolError::InvalidInput(
                        "too many repository base overrides".into(),
                    ));
                }
                for item in repository_bases {
                    if let Some(id) = &item.repository_id {
                        repository(id)?;
                    }
                    if let Some(path) = &item.relative_path {
                        validate_relative_path(path)?;
                    }
                    if item.repository_id.is_none() && item.relative_path.is_none() {
                        return Err(ProtocolError::InvalidInput(
                            "repository base requires repositoryId or relativePath".into(),
                        ));
                    }
                    if let Some(base) = &item.base {
                        validate_ref(base, "repository base")?;
                    }
                }
            }
            Self::SetRepositoryInclusion {
                workspace_id,
                repository_ids,
                ..
            } => {
                workspace(workspace_id)?;
                if repository_ids.is_empty() || repository_ids.len() > 10_000 {
                    return Err(ProtocolError::InvalidInput(
                        "repositoryIds must contain between 1 and 10000 values".into(),
                    ));
                }
                for id in repository_ids {
                    repository(id)?;
                }
            }
            Self::AddAnnotation {
                workspace_id,
                annotation,
            } => {
                workspace(workspace_id)?;
                file(&annotation.file_id)?;
                validate_text(&annotation.kind, "annotation kind", 32)?;
                validate_text(&annotation.side, "annotation side", 8)?;
                validate_text(&annotation.body, "annotation body", 1024 * 1024)?;
                if annotation.labels.len() > 100 {
                    return Err(ProtocolError::InvalidInput(
                        "annotation may contain at most 100 labels".into(),
                    ));
                }
                for label in &annotation.labels {
                    validate_text(label, "annotation label", 128)?;
                }
                if let Some(id) = &annotation.id {
                    validate_identifier(id, "annotation id")?;
                }
            }
            Self::DeleteAnnotation {
                workspace_id,
                annotation_id,
            } => {
                workspace(workspace_id)?;
                validate_identifier(annotation_id, "annotation id")?;
            }
            Self::SetAnnotationState {
                workspace_id,
                annotation_id,
                state,
            } => {
                workspace(workspace_id)?;
                validate_identifier(annotation_id, "annotation id")?;
                if !matches!(state.as_str(), "open" | "resolved") {
                    return Err(ProtocolError::InvalidInput(
                        "annotation state must be open or resolved".into(),
                    ));
                }
            }
            Self::MarkFileViewed {
                workspace_id,
                file_id,
                ..
            } => {
                workspace(workspace_id)?;
                file(file_id)?;
            }
            Self::QuerySymbols {
                workspace_id,
                repository_id,
                symbol,
                kind,
                limit,
            } => {
                workspace(workspace_id)?;
                repository(repository_id)?;
                validate_text(symbol, "symbol", 256)?;
                if !matches!(kind.as_str(), "all" | "definitions" | "references") {
                    return Err(ProtocolError::InvalidInput(
                        "symbol kind must be all, definitions, or references".into(),
                    ));
                }
                if limit.is_some_and(|limit| limit == 0 || limit > 500) {
                    return Err(ProtocolError::InvalidInput(
                        "symbol limit must be between 1 and 500".into(),
                    ));
                }
            }
            Self::GeneratePrompt {
                workspace_id,
                scope,
                annotation_ids,
                path_style,
                history_id,
                ..
            } => {
                workspace(workspace_id)?;
                validate_text(scope, "prompt scope", 64)?;
                if annotation_ids.len() > 10_000 {
                    return Err(ProtocolError::InvalidInput(
                        "too many annotation ids".into(),
                    ));
                }
                for id in annotation_ids {
                    validate_identifier(id, "annotation id")?;
                }
                if let Some(style) = path_style {
                    if !matches!(style.as_str(), "portable" | "qualified" | "absolute") {
                        return Err(ProtocolError::InvalidInput(
                            "path style must be portable, qualified, or absolute".into(),
                        ));
                    }
                }
                if let Some(history_id) = history_id {
                    validate_text(history_id, "history id", 256)?;
                }
            }
            Self::GithubPreviewReview {
                workspace_id,
                annotation_ids,
                summary,
                conclusion,
            } => {
                workspace(workspace_id)?;
                validate_text(summary, "review summary", 100_000)?;
                if !matches!(
                    conclusion.as_str(),
                    "comment" | "approve" | "request_changes"
                ) {
                    return Err(ProtocolError::InvalidInput(
                        "conclusion must be comment, approve, or request_changes".into(),
                    ));
                }
                for id in annotation_ids {
                    validate_identifier(id, "annotation id")?;
                }
            }
        }
        Ok(())
    }
}

/// Signed payload crossing the Unix socket.  The proof is HMAC-SHA256 over the
/// CBOR serialization of the remaining fields, so a runtime record never has
/// to contain a reusable credential.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalRequest {
    pub version: u16,
    pub request_id: String,
    /// Seconds since Unix epoch.  The desktop rejects stale requests.
    pub issued_at_unix_secs: u64,
    pub command: LocalCommand,
    pub authentication: AuthProof,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSigningPayload<'a> {
    pub version: u16,
    pub request_id: &'a str,
    pub issued_at_unix_secs: u64,
    pub command: &'a LocalCommand,
}

impl LocalRequest {
    pub fn signing_payload(&self) -> LocalSigningPayload<'_> {
        LocalSigningPayload {
            version: self.version,
            request_id: &self.request_id,
            issued_at_unix_secs: self.issued_at_unix_secs,
            command: &self.command,
        }
    }

    pub fn validate_shape(&self) -> Result<(), ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                received: self.version,
                supported: PROTOCOL_VERSION,
            });
        }
        validate_identifier(&self.request_id, "request id")?;
        self.command.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSourceTag {
    Github,
    Local,
    Ssh,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub source_tags: Vec<WorkspaceSourceTag>,
    pub available: bool,
    pub location: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DoctorReport {
    pub desktop_reachable: bool,
    pub protocol_version: u16,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalResponse {
    Opened {
        request_id: String,
        workspace: WorkspaceSummary,
        created: bool,
    },
    Focused {
        request_id: String,
        workspace: WorkspaceSummary,
    },
    PullRequestRequested {
        request_id: String,
        url: String,
    },
    SshWorkspaceRequested {
        request_id: String,
        target: String,
    },
    ForwardedRemoteWorkspace {
        request_id: String,
        path: String,
    },
    Workspaces {
        request_id: String,
        workspaces: Vec<WorkspaceSummary>,
    },
    Doctor {
        request_id: String,
        report: DoctorReport,
    },
    Programmatic {
        request_id: String,
        operation: String,
        data: serde_json::Value,
    },
    Error {
        request_id: Option<String>,
        code: RpcErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcErrorCode {
    AuthenticationFailed,
    InvalidRequest,
    NotFound,
    UnsupportedVersion,
    Unavailable,
    Internal,
}

pub fn validate_identifier(value: &str, label: &str) -> Result<(), ProtocolError> {
    validate_text(value, label, 128)?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProtocolError::InvalidInput(format!(
            "{label} contains unsupported characters"
        )));
    }
    Ok(())
}

pub fn validate_path(value: &str, label: &str) -> Result<(), ProtocolError> {
    validate_text(value, label, 4_096)?;
    if value.contains('\0') {
        return Err(ProtocolError::InvalidInput(format!("{label} contains NUL")));
    }
    Ok(())
}

pub fn validate_relative_path(value: &str) -> Result<(), ProtocolError> {
    validate_path(value, "repository relative path")?;
    if value.starts_with('/')
        || value == "."
        || value.split('/').any(|segment| segment == "..")
        || value.split('\\').any(|segment| segment == "..")
    {
        return Err(ProtocolError::InvalidInput(
            "repository relative path must stay beneath its workspace".into(),
        ));
    }
    Ok(())
}

pub fn validate_ref(value: &str, label: &str) -> Result<(), ProtocolError> {
    validate_text(value, label, 512)?;
    if value.starts_with('-')
        || value.contains('\0')
        || value.contains("..")
        || value.ends_with('.')
        || value.contains("@{")
        || value.contains(' ')
        || value.contains('~')
        || value.contains('^')
        || value.contains(':')
        || value.contains('\\')
        || value.contains('?')
        || value.contains('*')
        || value.contains('[')
    {
        return Err(ProtocolError::InvalidInput(format!(
            "{label} is not a safe Git revision"
        )));
    }
    Ok(())
}

pub fn validate_github_pr_url(value: &str) -> Result<(), ProtocolError> {
    validate_text(value, "GitHub pull request URL", 2_048)?;
    let Some(rest) = value.trim().strip_prefix("https://github.com/") else {
        return Err(ProtocolError::InvalidInput(
            "pull request URL must use https://github.com".into(),
        ));
    };
    let path = rest
        .split_once(['?', '#'])
        .map_or(rest, |(path, _)| path)
        .trim_end_matches('/');
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() < 4
        || parts[0].is_empty()
        || parts[1].is_empty()
        || parts[2] != "pull"
        || parts[3]
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .is_none()
    {
        return Err(ProtocolError::InvalidInput(
            "pull request URL must be github.com/<owner>/<repo>/pull/<number>".into(),
        ));
    }
    Ok(())
}

pub fn validate_ssh_target(value: &str) -> Result<(), ProtocolError> {
    validate_text(value, "SSH workspace target", 4_096)?;
    let Some((host, path)) = value.split_once(':') else {
        return Err(ProtocolError::InvalidInput(
            "SSH target must be host:/absolute/path".into(),
        ));
    };
    if host.is_empty()
        || path.len() < 2
        || !path.starts_with('/')
        || host.bytes().any(|byte| {
            byte.is_ascii_whitespace() || matches!(byte, b'\0' | b'/' | b';' | b'|' | b'&')
        })
    {
        return Err(ProtocolError::InvalidInput(
            "SSH target must be a safe host:/absolute/path value".into(),
        ));
    }
    Ok(())
}

fn validate_text(value: &str, label: &str, max_bytes: usize) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(ProtocolError::InvalidInput(format!(
            "{label} cannot be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(ProtocolError::InvalidInput(format!(
            "{label} exceeds {max_bytes} bytes"
        )));
    }
    if value.contains('\0') {
        return Err(ProtocolError::InvalidInput(format!("{label} contains NUL")));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported protocol version {received}; this installation supports {supported}")]
    UnsupportedVersion { received: u16, supported: u16 },
    #[error("invalid protocol input: {0}")]
    InvalidInput(String),
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("malformed frame: {0}")]
    MalformedFrame(String),
    #[error("frame compression error: {0}")]
    Compression(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_cbor::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pr_urls_accept_browser_copy_variants_on_the_strict_host_and_route() {
        assert!(validate_github_pr_url("https://github.com/acme/repo/pull/42").is_ok());
        assert!(validate_github_pr_url(" https://github.com/acme/repo/pull/42/files ").is_ok());
        assert!(validate_github_pr_url(
            "https://github.com/acme/repo/pull/42/files?diff=split#discussion_r123"
        )
        .is_ok());
        assert!(validate_github_pr_url("http://github.com/acme/repo/pull/42").is_err());
        assert!(validate_github_pr_url("https://github.com/acme/repo/issues/42").is_err());
        assert!(validate_github_pr_url("https://github.com/acme/repo/pull/0").is_err());
    }

    #[test]
    fn repository_overrides_cannot_escape_workspace() {
        assert!(validate_relative_path("nested/repo").is_ok());
        assert!(validate_relative_path("../repo").is_err());
        assert!(validate_relative_path("/repo").is_err());
    }

    #[test]
    fn refs_do_not_accept_option_like_or_revision_expression_inputs() {
        assert!(validate_ref("origin/main", "base").is_ok());
        assert!(validate_ref("--upload-pack=x", "base").is_err());
        assert!(validate_ref("main^{tree}", "base").is_err());
    }

    #[test]
    fn programmatic_surface_remains_closed_and_validates_bounded_inputs() {
        let workspace_id = "12345678-1234-1234-1234-123456789abc".to_owned();
        assert!(LocalCommand::Programmatic {
            command: ProgrammaticOperation::GeneratePrompt {
                workspace_id: workspace_id.clone(),
                scope: "feedback".into(),
                annotation_ids: Vec::new(),
                path_style: Some("absolute".into()),
                include_diff_hunks: false,
                include_git_state: false,
                history_id: None,
            },
        }
        .validate()
        .is_ok());
        assert!(LocalCommand::Programmatic {
            command: ProgrammaticOperation::QuerySymbols {
                workspace_id,
                repository_id: "12345678-1234-1234-1234-123456789abd".into(),
                symbol: "target".into(),
                kind: "shell".into(),
                limit: None,
            },
        }
        .validate()
        .is_err());
    }
}
