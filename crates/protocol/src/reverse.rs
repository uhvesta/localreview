//! Session-scoped reverse forwarding messages.
//!
//! These frames travel only across a loopback listener reached through an SSH
//! reverse tunnel. They cannot name arbitrary desktop actions and the bearer
//! token is generated in memory for one managed SSH session.

use crate::{validate_path, validate_ref, ProtocolError, RepositoryBaseOverride};
use serde::{Deserialize, Serialize};

pub const REVERSE_FORWARD_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedForwardRequest {
    pub version: u16,
    pub token_hex: String,
    pub command: ManagedForwardCommand,
}

/// A request sent by a same-user remote CLI to the companion-owned Unix
/// socket. The bearer token is intentionally absent: it remains in the
/// companion process environment supplied by the desktop's managed SSH
/// command, never in an interactive shell or on disk.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedForwardRelayRequest {
    pub version: u16,
    pub command: ManagedForwardRelayCommand,
}

impl ManagedForwardRelayRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != REVERSE_FORWARD_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                received: self.version,
                supported: REVERSE_FORWARD_VERSION,
            });
        }
        self.command.validate()
    }
}

/// The private Unix relay has one non-mutating probe in addition to the
/// authenticated workspace-open command. A remote CLI uses it to reject
/// ambiguous concurrent desktop sessions before it sends an open request to
/// any of them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedForwardRelayCommand {
    Probe,
    OpenRemoteWorkspace {
        path: String,
        base: Option<String>,
        #[serde(default)]
        repository_bases: Vec<RepositoryBaseOverride>,
    },
}

impl ManagedForwardRelayCommand {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Probe => Ok(()),
            Self::OpenRemoteWorkspace {
                path,
                base,
                repository_bases,
            } => ManagedForwardCommand::OpenRemoteWorkspace {
                path: path.clone(),
                base: base.clone(),
                repository_bases: repository_bases.clone(),
            }
            .validate(),
        }
    }
}

impl ManagedForwardRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != REVERSE_FORWARD_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                received: self.version,
                supported: REVERSE_FORWARD_VERSION,
            });
        }
        if self.token_hex.len() != 64
            || !self.token_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ProtocolError::InvalidInput(
                "managed forward token must be exactly 32 bytes of hex".into(),
            ));
        }
        self.command.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedForwardCommand {
    OpenRemoteWorkspace {
        path: String,
        base: Option<String>,
        #[serde(default)]
        repository_bases: Vec<RepositoryBaseOverride>,
    },
}

impl ManagedForwardCommand {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::OpenRemoteWorkspace {
                path,
                base,
                repository_bases,
            } => {
                validate_path(path, "managed remote workspace path")?;
                if !path.starts_with('/') {
                    return Err(ProtocolError::InvalidInput(
                        "managed remote workspace path must be absolute".into(),
                    ));
                }
                if let Some(base) = base {
                    validate_ref(base, "managed remote base")?;
                }
                if repository_bases.len() > 4_096 {
                    return Err(ProtocolError::InvalidInput(
                        "too many managed remote base overrides".into(),
                    ));
                }
                for override_ in repository_bases {
                    crate::validate_relative_path(&override_.relative_path)?;
                    validate_ref(&override_.base, "managed repository base")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedForwardResponse {
    Accepted,
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_forwarding_is_limited_to_a_remote_workspace_open() {
        let request = ManagedForwardRequest {
            version: REVERSE_FORWARD_VERSION,
            token_hex: "ab".repeat(32),
            command: ManagedForwardCommand::OpenRemoteWorkspace {
                path: "/work/a".into(),
                base: Some("origin/main".into()),
                repository_bases: vec![],
            },
        };
        assert!(request.validate().is_ok());
    }
}
