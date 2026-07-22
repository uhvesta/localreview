//! Strict parsing for operating-system activation URLs. Deep links are an
//! untrusted convenience boundary, never a generic command transport.

use localreview_protocol::{validate_github_pr_url, validate_identifier};
use tauri::{Emitter, Manager};
use thiserror::Error;
use url::Url;

use crate::{
    controller::{DesktopOperation, OpenGitHubPullRequestInput},
    AppState, DESKTOP_OPERATION_EVENT,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivationCommand {
    OpenPullRequest { url: String },
    FocusWorkspace { selector: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ActivationError {
    #[error("activation URL is invalid")]
    InvalidUrl,
    #[error("activation action is not supported")]
    UnsupportedAction,
    #[error("activation URL contains unsupported authority or path data")]
    InvalidShape,
    #[error("activation URL must contain exactly the expected query value")]
    InvalidQuery,
    #[error("activation value is invalid: {0}")]
    InvalidValue(String),
    #[error("activation could not be completed: {0}")]
    Dispatch(String),
}

/// Applies a validated activation directly at the trusted Rust boundary, then
/// tells the already-loaded UI which durable workspace to focus. Raw URLs are
/// never forwarded to Svelte.
pub(crate) fn dispatch_activation_url(
    app: &tauri::AppHandle,
    value: &str,
) -> Result<(), ActivationError> {
    let command = parse_activation_url(value)?;
    let controller = &app.state::<AppState>().controller;
    let workspace = match command {
        ActivationCommand::OpenPullRequest { url } => controller
            .open_github_pull_request(OpenGitHubPullRequestInput { url })
            .map(|(workspace, _)| workspace),
        ActivationCommand::FocusWorkspace { selector } => controller.focus_workspace(&selector),
    }
    .map_err(|error| ActivationError::Dispatch(error.to_string()))?;
    app.emit(
        DESKTOP_OPERATION_EVENT,
        DesktopOperation::Workspace {
            workspace_id: workspace.id,
        },
    )
    .map_err(|error| ActivationError::Dispatch(error.to_string()))?;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
    Ok(())
}

pub(crate) fn parse_activation_url(value: &str) -> Result<ActivationCommand, ActivationError> {
    const MAX_URL_BYTES: usize = 8 * 1024;
    if value.len() > MAX_URL_BYTES || value.contains('\0') {
        return Err(ActivationError::InvalidUrl);
    }
    let parsed = Url::parse(value).map_err(|_| ActivationError::InvalidUrl)?;
    if parsed.scheme() != "localreview"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
    {
        return Err(ActivationError::InvalidShape);
    }
    let action = parsed
        .host_str()
        .ok_or(ActivationError::UnsupportedAction)?;
    let query = parsed.query_pairs().collect::<Vec<_>>();
    match action {
        "pr" if query.len() == 1 && query[0].0 == "url" => {
            let url = query[0].1.to_string();
            validate_github_pr_url(&url)
                .map_err(|error| ActivationError::InvalidValue(error.to_string()))?;
            Ok(ActivationCommand::OpenPullRequest { url })
        }
        "workspace" if query.len() == 1 && query[0].0 == "id" => {
            let selector = query[0].1.to_string();
            validate_identifier(&selector, "workspace id")
                .map_err(|error| ActivationError::InvalidValue(error.to_string()))?;
            Ok(ActivationCommand::FocusWorkspace { selector })
        }
        "pr" | "workspace" => Err(ActivationError::InvalidQuery),
        _ => Err(ActivationError::UnsupportedAction),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_github_pr_and_workspace_activation_shapes() {
        assert_eq!(
            parse_activation_url(
                "localreview://pr?url=https%3A%2F%2Fgithub.com%2Focto%2Frepo%2Fpull%2F42"
            )
            .unwrap(),
            ActivationCommand::OpenPullRequest {
                url: "https://github.com/octo/repo/pull/42".into()
            }
        );
        assert_eq!(
            parse_activation_url("localreview://workspace?id=74b4adae-2d88-4994-9d25-bf502a49ba15")
                .unwrap(),
            ActivationCommand::FocusWorkspace {
                selector: "74b4adae-2d88-4994-9d25-bf502a49ba15".into()
            }
        );
    }

    #[test]
    fn rejects_hosts_paths_credentials_fragments_and_extra_values() {
        for invalid in [
            "https://github.com/octo/repo/pull/42",
            "localreview://delete?id=workspace",
            "localreview://pr/path?url=https%3A%2F%2Fgithub.com%2Fa%2Fb%2Fpull%2F1",
            "localreview://user@pr?url=https%3A%2F%2Fgithub.com%2Fa%2Fb%2Fpull%2F1",
            "localreview://pr?url=https%3A%2F%2Fgithub.com%2Fa%2Fb%2Fpull%2F1&other=1",
            "localreview://pr?url=https%3A%2F%2Fevil.example%2Fa%2Fb%2Fpull%2F1",
            "localreview://workspace?id=../../escape",
            "localreview://workspace?id=abc#fragment",
        ] {
            assert!(parse_activation_url(invalid).is_err(), "accepted {invalid}");
        }
    }
}
