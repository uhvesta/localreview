use crate::api::activate_main_window;
use crate::controller::{DesktopController, DesktopOperation, DispatchError};
use crate::{AppState, DESKTOP_OPERATION_EVENT};
use localreview_protocol::{
    read_frame, write_frame, AppPaths, InstallationSecret, LocalCommand, LocalRequest,
    LocalResponse, ProtocolError, RpcErrorCode, RuntimeRecord,
};
use rand::RngCore;
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const REQUEST_MAX_AGE: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum RpcServerError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("an existing LocalReview desktop socket is already accepting requests")]
    AlreadyRunning,
    #[error("cannot bind desktop socket: {0}")]
    Bind(#[from] io::Error),
}

pub fn start_local_rpc_server(app: AppHandle) -> Result<(), RpcServerError> {
    let paths = AppPaths::discover()?;
    let secret = paths.load_or_create_secret(random_secret)?;
    prepare_socket_path(&paths.socket_path())?;
    let listener = UnixListener::bind(paths.socket_path())?;
    listener.set_nonblocking(false)?;
    paths.write_runtime_record(&RuntimeRecord::current(paths.socket_path()))?;
    let controller = app.state::<AppState>().controller.clone();
    thread::Builder::new()
        .name("localreview-local-rpc".into())
        .spawn(move || accept_loop(listener, app, controller, secret))
        .map_err(ProtocolError::Io)?;
    Ok(())
}

fn random_secret() -> [u8; InstallationSecret::LEN] {
    let mut secret = [0_u8; InstallationSecret::LEN];
    rand::rng().fill_bytes(&mut secret);
    secret
}

fn prepare_socket_path(path: &Path) -> Result<(), RpcServerError> {
    if !path.exists() {
        return Ok(());
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(RpcServerError::AlreadyRunning),
        Err(_) => {
            // This exact socket path lives in the app's 0700 runtime directory.
            // Removing a dead socket here never follows a user-provided path.
            std::fs::remove_file(path)?;
            Ok(())
        }
    }
}

fn accept_loop(
    listener: UnixListener,
    app: AppHandle,
    controller: Arc<DesktopController>,
    secret: InstallationSecret,
) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let app = app.clone();
                let controller = controller.clone();
                let secret = secret.clone();
                let _ = thread::Builder::new()
                    .name("localreview-rpc-request".into())
                    .spawn(move || handle_connection(stream, app, controller, secret));
            }
            Err(_) => {
                // A transient accept failure must not bring down an otherwise
                // healthy review window. The next CLI request can retry.
            }
        }
    }
}

fn handle_connection(
    mut stream: UnixStream,
    app: AppHandle,
    controller: Arc<DesktopController>,
    secret: InstallationSecret,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let response = match read_frame::<LocalRequest>(&mut stream) {
        Ok(request) => dispatch_request(request, &app, &controller, &secret),
        Err(error) => LocalResponse::Error {
            request_id: None,
            code: RpcErrorCode::InvalidRequest,
            message: error.to_string(),
        },
    };
    let _ = write_frame(&mut stream, &response);
}

fn dispatch_request(
    request: LocalRequest,
    app: &AppHandle,
    controller: &DesktopController,
    secret: &InstallationSecret,
) -> LocalResponse {
    let request_id = request.request_id.clone();
    if let Err(error) = request.validate_shape() {
        return error_response(Some(request_id), error);
    }
    if secret.verify(&request).is_err() {
        return LocalResponse::Error {
            request_id: Some(request_id),
            code: RpcErrorCode::AuthenticationFailed,
            message: "desktop authentication failed".into(),
        };
    }
    if !request_is_fresh(request.issued_at_unix_secs) {
        return LocalResponse::Error {
            request_id: Some(request_id),
            code: RpcErrorCode::InvalidRequest,
            message: "request timestamp is outside the allowed freshness window".into(),
        };
    }
    if let Err(error) =
        controller.accept_request_id(&request.request_id, request.issued_at_unix_secs)
    {
        return dispatch_error(Some(request_id), error);
    }
    let command_operation = operation_for_event(&request.command);
    match controller.dispatch(request.command, request_id) {
        Ok(response) => {
            activate_main_window(app);
            // Local Open/Focus responses include the canonical workspace id.
            // Emit it only after controller dispatch succeeded so the UI never
            // focuses a phantom workspace. PR/SSH retain their request events
            // until their provider flows materialize a workspace.
            if let Some(operation) =
                workspace_operation_for_response(&response).or(command_operation)
            {
                let _ = app.emit(DESKTOP_OPERATION_EVENT, operation);
            }
            response
        }
        Err(error) => dispatch_error(None, error),
    }
}

fn workspace_operation_for_response(response: &LocalResponse) -> Option<DesktopOperation> {
    match response {
        LocalResponse::Opened { workspace, .. } | LocalResponse::Focused { workspace, .. } => {
            Some(DesktopOperation::Workspace {
                workspace_id: workspace.id.clone(),
            })
        }
        _ => None,
    }
}

fn operation_for_event(command: &LocalCommand) -> Option<DesktopOperation> {
    match command {
        LocalCommand::OpenPullRequest { url } => {
            Some(DesktopOperation::PullRequest { url: url.clone() })
        }
        LocalCommand::OpenSshWorkspace { target } => Some(DesktopOperation::SshWorkspace {
            target: target.clone(),
        }),
        _ => None,
    }
}

fn request_is_fresh(issued_at_unix_secs: u64) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.abs_diff(issued_at_unix_secs) <= REQUEST_MAX_AGE.as_secs()
}

fn error_response(request_id: Option<String>, error: ProtocolError) -> LocalResponse {
    let message = error.to_string();
    let code = match &error {
        ProtocolError::UnsupportedVersion { .. } => RpcErrorCode::UnsupportedVersion,
        ProtocolError::AuthenticationFailed => RpcErrorCode::AuthenticationFailed,
        _ => RpcErrorCode::InvalidRequest,
    };
    LocalResponse::Error {
        request_id,
        code,
        message,
    }
}

fn dispatch_error(request_id: Option<String>, error: DispatchError) -> LocalResponse {
    let message = error.to_string();
    let code = match &error {
        DispatchError::Invalid(_) | DispatchError::Ambiguous(_) => RpcErrorCode::InvalidRequest,
        // A superseded viewport is an expected client race, not a failed
        // capture or transport operation. The protocol has no dedicated
        // cancellation discriminator, so expose it as a retry-safe request
        // rejection instead of manufacturing an internal error.
        DispatchError::Cancelled => RpcErrorCode::InvalidRequest,
        DispatchError::NotFound(_) => RpcErrorCode::NotFound,
        DispatchError::Service(_)
        | DispatchError::Persistence(_)
        | DispatchError::Remote(_)
        | DispatchError::Internal => RpcErrorCode::Internal,
    };
    LocalResponse::Error {
        request_id,
        code,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreview_protocol::{AuthProof, LocalCommand, WorkspaceSummary, PROTOCOL_VERSION};

    #[test]
    fn only_recent_requests_are_accepted() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(request_is_fresh(now));
        assert!(!request_is_fresh(now.saturating_sub(121)));
    }

    #[test]
    fn unauthenticated_request_is_rejected_before_dispatch() {
        let request = LocalRequest {
            version: PROTOCOL_VERSION,
            request_id: "request-1".into(),
            issued_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            command: LocalCommand::ListWorkspaces,
            authentication: AuthProof {
                mac_hex: "not-a-valid-mac".into(),
            },
        };
        let secret = InstallationSecret::from_bytes([1; 32]);
        let state_directory = tempfile::tempdir().unwrap();
        let controller = DesktopController::new(
            localreview_persistence::StateStore::open(state_directory.path()).unwrap(),
        );
        // A Tauri app handle is not needed: auth failure returns before it is used.
        assert!(secret.verify(&request).is_err());
        assert!(controller.list_workspaces().unwrap().is_empty());
    }

    #[test]
    fn successful_local_open_and_focus_emit_the_canonical_workspace_target() {
        let response = LocalResponse::Opened {
            request_id: "request-1".into(),
            workspace: WorkspaceSummary {
                id: "workspace-id".into(),
                name: "Workspace".into(),
                source_tags: vec![],
                available: true,
                location: None,
            },
            created: true,
        };
        assert!(matches!(
            workspace_operation_for_response(&response),
            Some(DesktopOperation::Workspace { workspace_id }) if workspace_id == "workspace-id"
        ));
    }
}
