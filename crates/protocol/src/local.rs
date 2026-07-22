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
            Self::ListWorkspaces | Self::Doctor => {}
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
    let Some(rest) = value.strip_prefix("https://github.com/") else {
        return Err(ProtocolError::InvalidInput(
            "pull request URL must use https://github.com".into(),
        ));
    };
    let parts: Vec<_> = rest.split('/').collect();
    if parts.len() != 4
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
    fn github_pr_urls_are_strict() {
        assert!(validate_github_pr_url("https://github.com/acme/repo/pull/42").is_ok());
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
}
