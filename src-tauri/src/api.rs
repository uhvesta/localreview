//! Typed native commands consumed by the Svelte review client.  This is a
//! deliberately small boundary: every operation delegates to the persistent
//! controller; it never exposes shell or arbitrary filesystem access.

use crate::controller::{
    AnnotationDraft, AnnotationView, ApplyRepositoryBaseInput, CapturedBlameInput,
    CapturedBlameView, CapturedCommitContextView, CapturedSourceRange,
    ChangedSincePreviousReviewView, CommitContextInput, ConfigureBaselinesInput,
    CopyReviewItemRequest, DiffRowView, DispatchError, FinishReviewInput, FinishReviewPreview,
    FinishReviewResult, FinishReviewSubmissionInput, GitHubPullRequestContextView,
    GitHubPullRequestUpdateStatusView, ImportedGitHubConversationCommentView,
    ImportedGitHubReviewThreadView, OpenGitHubPullRequestInput, OpenSshWorkspaceInput,
    OpenWorkspaceInput, OutlineSymbolView, PresentationLocation, PresentationRequest,
    PresentationWindow, PromptExportSaveFormat, PromptInput, PromptPreview, RepositoryBaseInput,
    RepositorySelectionInput, RepositorySetupView, ReviewData, ReviewFileClassificationView,
    ReviewHistoryItem, ReviewSettings, SavedPromptExport, SetRepositoryInclusionInput,
    StartOrRefreshInput, WorkspaceUiStatePatch, WorkspaceUiStateView, WorkspaceView,
};
use crate::AppState;
use localreview_persistence::PersistenceDiagnostics;
use serde::Deserialize;
use tauri::{AppHandle, Manager, State};

/// The reviewed frontend may invoke only these names. Keep this list beside
/// the commands so a rename cannot silently turn native interactions into
/// browser/mock behavior.
#[allow(dead_code)]
pub const REVIEW_API_COMMANDS: &[&str] = &[
    "pick_local_folder",
    "open_workspace",
    "open_github_pr",
    "open_ssh_workspace",
    "reconnect_ssh_workspace",
    "focus_workspace",
    "list_workspaces",
    "list_archived_workspaces",
    "reopen_archived_workspace",
    "archive_workspace",
    "update_workspace_metadata",
    "get_persistence_diagnostics",
    "load_review",
    "load_archived_review",
    "get_review_file_classifications",
    "get_captured_blame",
    "get_commit_context",
    "get_changed_since_previous_review",
    "get_github_update_status",
    "get_presentation_window",
    "get_presentation_rows",
    "resolve_presentation_location",
    "get_captured_source_range",
    "expand_hunk_context",
    "get_outline",
    "save_annotation",
    "get_annotation_draft",
    "save_annotation_draft",
    "clear_annotation_draft",
    "delete_annotation",
    "set_annotation_state",
    "archive_annotations",
    "restore_annotations",
    "restore_history_item",
    "generate_prompt",
    "save_prompt_export",
    "get_review_history",
    "set_viewed",
    "get_repository_setup",
    "set_repository_inclusion",
    "apply_repository_base",
    "reset_repository_base_overrides",
    "fetch_repositories",
    "configure_baselines",
    "start_new_review",
    "refresh_review",
    "preview_finish_review",
    "finish_review",
    "abandon_finish_review",
    "delete_workspace",
    "get_github_pull_request",
    "get_github_threads",
    "get_github_conversation",
    "get_ui_settings",
    "save_ui_settings",
    "get_workspace_ui_state",
    "save_workspace_ui_state",
    "copy_review_item",
    "open_in_external_editor",
    "copy_to_clipboard",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenWorkspaceRequest {
    pub path: String,
    pub base: Option<String>,
    #[serde(default)]
    pub repository_bases: Vec<RepositoryBaseInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FocusWorkspaceRequest {
    pub selector: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PartialReviewSettings {
    pub last_workspace_id: Option<String>,
    pub font_scale: Option<f64>,
    pub left_width: Option<u32>,
    pub right_width: Option<u32>,
    pub left_collapsed: Option<bool>,
    pub right_collapsed: Option<bool>,
    pub fetch_on_review: Option<bool>,
    pub theme: Option<String>,
    pub code_font: Option<String>,
    pub external_editor: Option<String>,
    pub tab_width: Option<u8>,
    pub show_whitespace: Option<bool>,
    pub wrap_lines: Option<bool>,
    pub vim_navigation: Option<bool>,
    pub prompt_path_style: Option<String>,
    pub prompt_include_diff_hunks: Option<bool>,
    pub prompt_include_git_state: Option<bool>,
    pub shortcuts: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_preview_token: Option<String>,
}

impl From<DispatchError> for ApiError {
    fn from(error: DispatchError) -> Self {
        let message = error.to_string();
        let recovery_preview_token = match &error {
            DispatchError::Service(
                localreview_service::ServiceError::GitHubPublicationAmbiguous {
                    preview_token, ..
                },
            )
            | DispatchError::Service(
                localreview_service::ServiceError::GitHubPublicationReconciliationPending {
                    preview_token,
                },
            ) => Some(preview_token.clone()),
            _ => None,
        };
        let code = match &error {
            DispatchError::Invalid(_) => "invalid_request",
            DispatchError::Cancelled => "cancelled",
            DispatchError::NotFound(_) => "not_found",
            DispatchError::Ambiguous(_) => "ambiguous_workspace",
            DispatchError::Service(
                localreview_service::ServiceError::GitHubPublicationAmbiguous { .. },
            ) => "github_publication_ambiguous",
            DispatchError::Service(
                localreview_service::ServiceError::GitHubPublicationReconciliationPending {
                    ..
                },
            ) => "github_publication_reconciliation_pending",
            DispatchError::Service(_)
            | DispatchError::Persistence(_)
            | DispatchError::Remote(_)
            | DispatchError::Internal => "internal",
        };
        Self {
            code,
            message,
            recovery_preview_token,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedFolder {
    pub path: Option<String>,
}

#[tauri::command]
pub fn pick_local_folder() -> PickedFolder {
    PickedFolder {
        path: rfd::FileDialog::new()
            .pick_folder()
            .map(|path| path.to_string_lossy().into_owned()),
    }
}

#[tauri::command]
pub async fn open_workspace(
    request: OpenWorkspaceRequest,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<WorkspaceView, ApiError> {
    let controller = std::sync::Arc::clone(&state.controller);
    let (workspace, _) = tauri::async_runtime::spawn_blocking(move || {
        controller.open_local_workspace(OpenWorkspaceInput {
            path: request.path,
            base: request.base,
            repository_bases: request.repository_bases,
        })
    })
    .await
    .map_err(|error| ApiError {
        code: "local_open_worker_failed",
        message: format!("local workspace open worker stopped unexpectedly: {error}"),
        recovery_preview_token: None,
    })?
    .map_err(ApiError::from)?;
    activate_main_window(&app);
    Ok(workspace)
}

#[tauri::command]
pub fn update_workspace_metadata(
    workspace_id: String,
    name: Option<String>,
    pinned: Option<bool>,
    state: State<'_, AppState>,
) -> Result<WorkspaceView, ApiError> {
    state
        .controller
        .update_workspace_metadata(parse_workspace(&workspace_id)?, name, pinned)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_persistence_diagnostics(
    state: State<'_, AppState>,
) -> Result<PersistenceDiagnostics, ApiError> {
    state
        .controller
        .persistence_diagnostics()
        .map_err(ApiError::from)
}

/// Opens a GitHub.com pull-request URL through the native provider.
/// The URL is the only input; neither the frontend nor CLI can supply a local
/// checkout path or arbitrary `gh` arguments.
#[tauri::command]
pub async fn open_github_pr(
    url: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<WorkspaceView, ApiError> {
    let controller = std::sync::Arc::clone(&state.controller);
    let (workspace, _) = tauri::async_runtime::spawn_blocking(move || {
        controller.open_github_pull_request(OpenGitHubPullRequestInput { url })
    })
    .await
    .map_err(|error| ApiError {
        code: "github_open_worker_failed",
        message: format!("GitHub PR open worker stopped unexpectedly: {error}"),
        recovery_preview_token: None,
    })?
    .map_err(ApiError::from)?;
    activate_main_window(&app);
    Ok(workspace)
}

/// Connects through the typed SSH companion. `target` is strictly parsed as
/// `host:/absolute/path`; no remote command, shell, or local checkout path is
/// accepted from the frontend.
#[tauri::command]
pub async fn open_ssh_workspace(
    target: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<WorkspaceView, ApiError> {
    let controller = std::sync::Arc::clone(&state.controller);
    let (workspace, _) = tauri::async_runtime::spawn_blocking(move || {
        controller.open_ssh_workspace(OpenSshWorkspaceInput { target })
    })
    .await
    .map_err(|error| ApiError {
        code: "ssh_open_worker_failed",
        message: format!("SSH workspace open worker stopped unexpectedly: {error}"),
        recovery_preview_token: None,
    })?
    .map_err(ApiError::from)?;
    activate_main_window(&app);
    Ok(workspace)
}

#[tauri::command]
pub async fn reconnect_ssh_workspace(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<WorkspaceView, ApiError> {
    let workspace_id = parse_workspace(&workspace_id)?;
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || controller.reconnect_ssh_workspace(workspace_id))
        .await
        .map_err(|error| ApiError {
            code: "ssh_reconnect_worker_failed",
            message: format!("SSH workspace reconnect worker stopped unexpectedly: {error}"),
            recovery_preview_token: None,
        })?
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn focus_workspace(
    request: FocusWorkspaceRequest,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<WorkspaceView, ApiError> {
    let workspace = state
        .controller
        .focus_workspace(&request.selector)
        .map_err(ApiError::from)?;
    activate_main_window(&app);
    Ok(workspace)
}

#[tauri::command]
pub fn list_workspaces(state: State<'_, AppState>) -> Result<Vec<WorkspaceView>, ApiError> {
    state.controller.list_workspaces().map_err(ApiError::from)
}

/// Lists recoverable workspace snapshots that were removed from the live
/// rail.  This returns only durable review metadata; no checkout, fetch, or
/// SSH connection is started merely to show history.
#[tauri::command]
pub fn list_archived_workspaces(
    state: State<'_, AppState>,
) -> Result<Vec<WorkspaceView>, ApiError> {
    state
        .controller
        .list_archived_workspaces()
        .map_err(ApiError::from)
}

/// Restores an archived workspace to the live rail so its captured diff and
/// annotations can be browsed again.  GitHub worktrees are not recreated by
/// this command; historical documents stay pinned and read-only until an
/// explicit future refresh/open flow is chosen.
#[tauri::command]
pub fn reopen_archived_workspace(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<WorkspaceView, ApiError> {
    state
        .controller
        .reopen_archived_workspace(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn load_review(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    state
        .controller
        .load_review(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

/// Loads one archived `review:<uuid>` history entry as a read-only immutable
/// browsing surface.  It does not restore its annotations into the active
/// review and cannot trigger capture/fetch/worktree mutation.
#[tauri::command]
pub fn load_archived_review(
    workspace_id: String,
    history_id: String,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    state
        .controller
        .load_archived_review(parse_workspace(&workspace_id)?, &history_id)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_review_file_classifications(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ReviewFileClassificationView>, ApiError> {
    state
        .controller
        .review_file_classifications(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_captured_blame(
    workspace_id: String,
    file_id: String,
    side: String,
    start_line: u32,
    end_line: u32,
    state: State<'_, AppState>,
) -> Result<CapturedBlameView, ApiError> {
    state
        .controller
        .captured_blame(
            parse_workspace(&workspace_id)?,
            CapturedBlameInput {
                file_id,
                side,
                start_line,
                end_line,
            },
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_commit_context(
    workspace_id: String,
    request: CommitContextInput,
    state: State<'_, AppState>,
) -> Result<CapturedCommitContextView, ApiError> {
    state
        .controller
        .captured_commit_context(parse_workspace(&workspace_id)?, request)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_changed_since_previous_review(
    workspace_id: String,
    repository_id: String,
    state: State<'_, AppState>,
) -> Result<ChangedSincePreviousReviewView, ApiError> {
    state
        .controller
        .changed_since_previous_review(parse_workspace(&workspace_id)?, repository_id)
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn get_github_update_status(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<GitHubPullRequestUpdateStatusView, ApiError> {
    let workspace_id = parse_workspace(&workspace_id)?;
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || {
        controller.github_pull_request_update_status(workspace_id)
    })
    .await
    .map_err(|error| ApiError {
        code: "github_status_worker_failed",
        message: format!("GitHub status worker stopped unexpectedly: {error}"),
        recovery_preview_token: None,
    })?
    .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_presentation_rows(
    file_id: String,
    mode: String,
    state: State<'_, AppState>,
) -> Result<Vec<DiffRowView>, ApiError> {
    state
        .controller
        .rows(parse_file(&file_id)?, &mode)
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn get_presentation_window(
    request: PresentationRequest,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<PresentationWindow, ApiError> {
    let resource_dir = app.path().resource_dir().map_err(|error| ApiError {
        code: "resource_unavailable",
        message: format!("could not resolve packaged presentation resources: {error}"),
        recovery_preview_token: None,
    })?;
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || {
        controller.presentation_window(request, &resource_dir)
    })
    .await
    .map_err(|error| ApiError {
        code: "presentation_worker_failed",
        message: format!("presentation worker stopped unexpectedly: {error}"),
        recovery_preview_token: None,
    })?
    .map_err(ApiError::from)
}

/// Resolves a source line against the complete native presentation. Command
/// arguments are intentionally flat so the Svelte adapter can use Tauri's
/// normal `{ fileId, mode, side, line }` invocation shape.
#[tauri::command]
pub async fn resolve_presentation_location(
    file_id: String,
    comparison_id: Option<String>,
    mode: String,
    side: String,
    line: u32,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<PresentationLocation, ApiError> {
    let file_id = parse_file(&file_id)?;
    let comparison_id = comparison_id.as_deref().map(parse_comparison).transpose()?;
    let side = parse_side(&side)?;
    let resource_dir = app.path().resource_dir().map_err(|error| ApiError {
        code: "resource_unavailable",
        message: format!("could not resolve packaged presentation resources: {error}"),
        recovery_preview_token: None,
    })?;
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || {
        controller.resolve_presentation_location(
            file_id,
            comparison_id,
            &mode,
            side,
            line,
            &resource_dir,
        )
    })
    .await
    .map_err(|error| ApiError {
        code: "presentation_worker_failed",
        message: format!("presentation worker stopped unexpectedly: {error}"),
        recovery_preview_token: None,
    })?
    .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_captured_source_range(
    file_id: String,
    comparison_id: Option<String>,
    side: String,
    start_line: u32,
    end_line: u32,
    state: State<'_, AppState>,
) -> Result<CapturedSourceRange, ApiError> {
    state
        .controller
        .captured_source_range(
            parse_file(&file_id)?,
            comparison_id.as_deref().map(parse_comparison).transpose()?,
            parse_side(&side)?,
            start_line,
            end_line,
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn expand_hunk_context(
    file_id: String,
    comparison_id: Option<String>,
    hunk_id: String,
    context_lines: u32,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .expand_hunk_context(
            parse_file(&file_id)?,
            comparison_id.as_deref().map(parse_comparison).transpose()?,
            &hunk_id,
            context_lines,
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn get_outline(
    file_id: String,
    comparison_id: Option<String>,
    side: String,
    state: State<'_, AppState>,
) -> Result<Vec<OutlineSymbolView>, ApiError> {
    let file_id = parse_file(&file_id)?;
    let comparison_id = comparison_id.as_deref().map(parse_comparison).transpose()?;
    let side = parse_side(&side)?;
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || controller.outline(file_id, comparison_id, side))
        .await
        .map_err(|error| ApiError {
            code: "presentation_worker_failed",
            message: format!("outline worker stopped unexpectedly: {error}"),
            recovery_preview_token: None,
        })?
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn save_annotation(
    annotation: AnnotationView,
    state: State<'_, AppState>,
) -> Result<AnnotationView, ApiError> {
    state
        .controller
        .save_annotation(annotation)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_annotation_draft(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Option<AnnotationDraft>, ApiError> {
    state
        .controller
        .annotation_draft(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn save_annotation_draft(
    draft: AnnotationDraft,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .save_annotation_draft(draft)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn clear_annotation_draft(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .clear_annotation_draft(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn archive_annotations(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<ReviewHistoryItem, ApiError> {
    state
        .controller
        .archive_annotations(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn restore_history_item(
    workspace_id: String,
    history_id: String,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    state
        .controller
        .restore_history_item(parse_workspace(&workspace_id)?, &history_id)
        .map_err(ApiError::from)
}

/// Compatibility command for clients that still keep a locally rendered
/// checkpoint. It persists the supplied annotations into the current active
/// set through the same strict anchor validation as normal saves.
#[tauri::command]
pub fn restore_annotations(
    workspace_id: String,
    annotations: Vec<AnnotationView>,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    state
        .controller
        .restore_annotations(parse_workspace(&workspace_id)?, annotations)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn delete_annotation(
    workspace_id: String,
    annotation_id: String,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .delete_annotation(
            parse_workspace(&workspace_id)?,
            parse_annotation(&annotation_id)?,
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn set_annotation_state(
    workspace_id: String,
    annotation_id: String,
    state: String,
    app_state: State<'_, AppState>,
) -> Result<AnnotationView, ApiError> {
    app_state
        .controller
        .set_annotation_state(
            parse_workspace(&workspace_id)?,
            parse_annotation(&annotation_id)?,
            &state,
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn generate_prompt(
    workspace_id: String,
    request: PromptInput,
    state: State<'_, AppState>,
) -> Result<PromptPreview, ApiError> {
    state
        .controller
        .generate_prompt(parse_workspace(&workspace_id)?, request)
        .map_err(ApiError::from)
}

/// Writes only an already-durable prompt export through a native, user-chosen
/// save dialog. The webview cannot supply a path or arbitrary file contents.
#[tauri::command]
pub fn save_prompt_export(
    workspace_id: String,
    export_id: String,
    format: PromptExportSaveFormat,
    state: State<'_, AppState>,
) -> Result<SavedPromptExport, ApiError> {
    state
        .controller
        .save_prompt_export(parse_workspace(&workspace_id)?, &export_id, format)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_review_history(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ReviewHistoryItem>, ApiError> {
    state
        .controller
        .history(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn set_viewed(
    workspace_id: String,
    file_id: String,
    viewed: bool,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .set_viewed(
            parse_workspace(&workspace_id)?,
            parse_file(&file_id)?,
            viewed,
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_repository_setup(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<RepositorySetupView>, ApiError> {
    state
        .controller
        .repository_setup(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn set_repository_inclusion(
    workspace_id: String,
    input: SetRepositoryInclusionInput,
    state: State<'_, AppState>,
) -> Result<Vec<RepositorySetupView>, ApiError> {
    state
        .controller
        .set_repository_inclusion(parse_workspace(&workspace_id)?, input)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn apply_repository_base(
    workspace_id: String,
    input: ApplyRepositoryBaseInput,
    state: State<'_, AppState>,
) -> Result<Vec<RepositorySetupView>, ApiError> {
    state
        .controller
        .apply_repository_base(parse_workspace(&workspace_id)?, input)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn reset_repository_base_overrides(
    workspace_id: String,
    input: RepositorySelectionInput,
    state: State<'_, AppState>,
) -> Result<Vec<RepositorySetupView>, ApiError> {
    state
        .controller
        .reset_repository_base_overrides(parse_workspace(&workspace_id)?, input)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn fetch_repositories(
    workspace_id: String,
    repository_ids: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> Result<Vec<RepositorySetupView>, ApiError> {
    state
        .controller
        .fetch_repositories(parse_workspace(&workspace_id)?, repository_ids)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn configure_baselines(
    workspace_id: String,
    default_base: Option<String>,
    repository_bases: Option<Vec<RepositoryBaseInput>>,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    state
        .controller
        .configure_baselines(
            parse_workspace(&workspace_id)?,
            ConfigureBaselinesInput {
                default_base,
                repository_bases: repository_bases.unwrap_or_default(),
            },
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn start_new_review(
    workspace_id: String,
    request: Option<StartOrRefreshInput>,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    let workspace_id = parse_workspace(&workspace_id)?;
    let request = request.unwrap_or_default();
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || controller.start_new_review(workspace_id, request))
        .await
        .map_err(|error| ApiError {
            code: "review_worker_failed",
            message: format!("review capture worker stopped unexpectedly: {error}"),
            recovery_preview_token: None,
        })?
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn refresh_review(
    workspace_id: String,
    request: Option<StartOrRefreshInput>,
    state: State<'_, AppState>,
) -> Result<ReviewData, ApiError> {
    let workspace_id = parse_workspace(&workspace_id)?;
    let request = request.unwrap_or_default();
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || controller.refresh_review(workspace_id, request))
        .await
        .map_err(|error| ApiError {
            code: "refresh_worker_failed",
            message: format!("refresh worker stopped unexpectedly: {error}"),
            recovery_preview_token: None,
        })?
        .map_err(ApiError::from)
}

#[tauri::command]
pub async fn finish_review(
    workspace_id: String,
    submission: FinishReviewSubmissionInput,
    state: State<'_, AppState>,
) -> Result<FinishReviewResult, ApiError> {
    let workspace_id = parse_workspace(&workspace_id)?;
    let controller = std::sync::Arc::clone(&state.controller);
    tauri::async_runtime::spawn_blocking(move || controller.finish_review(workspace_id, submission))
        .await
        .map_err(|error| ApiError {
            code: "github_review_worker_failed",
            message: format!("GitHub review worker stopped unexpectedly: {error}"),
            recovery_preview_token: None,
        })?
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn abandon_finish_review(
    workspace_id: String,
    submission: FinishReviewSubmissionInput,
    confirm_prepared: bool,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .abandon_finish_review(
            parse_workspace(&workspace_id)?,
            submission,
            confirm_prepared,
        )
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn preview_finish_review(
    workspace_id: String,
    request: FinishReviewInput,
    state: State<'_, AppState>,
) -> Result<FinishReviewPreview, ApiError> {
    state
        .controller
        .preview_finish_review(parse_workspace(&workspace_id)?, request)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn archive_workspace(workspace_id: String, state: State<'_, AppState>) -> Result<(), ApiError> {
    state
        .controller
        .archive_workspace(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn delete_workspace(workspace_id: String, state: State<'_, AppState>) -> Result<(), ApiError> {
    state
        .controller
        .delete_workspace(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_github_pull_request(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<GitHubPullRequestContextView, ApiError> {
    state
        .controller
        .github_pull_request(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_github_threads(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ImportedGitHubReviewThreadView>, ApiError> {
    state
        .controller
        .github_threads(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_github_conversation(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ImportedGitHubConversationCommentView>, ApiError> {
    state
        .controller
        .github_conversation(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_ui_settings(state: State<'_, AppState>) -> Result<ReviewSettings, ApiError> {
    state.controller.get_settings().map_err(ApiError::from)
}

#[tauri::command]
pub fn save_ui_settings(
    settings: PartialReviewSettings,
    state: State<'_, AppState>,
) -> Result<ReviewSettings, ApiError> {
    let mut merged = state.controller.get_settings().map_err(ApiError::from)?;
    if let Some(value) = settings.last_workspace_id {
        merged.last_workspace_id = Some(value);
    }
    if let Some(value) = settings.font_scale {
        merged.font_scale = value;
    }
    if let Some(value) = settings.left_width {
        merged.left_width = value;
    }
    if let Some(value) = settings.right_width {
        merged.right_width = value;
    }
    if let Some(value) = settings.left_collapsed {
        merged.left_collapsed = value;
    }
    if let Some(value) = settings.right_collapsed {
        merged.right_collapsed = value;
    }
    if let Some(value) = settings.fetch_on_review {
        merged.fetch_on_review = value;
    }
    if let Some(value) = settings.theme {
        merged.theme = value;
    }
    if let Some(value) = settings.code_font {
        merged.code_font = value;
    }
    if let Some(value) = settings.external_editor {
        merged.external_editor = value;
    }
    if let Some(value) = settings.tab_width {
        merged.tab_width = value;
    }
    if let Some(value) = settings.show_whitespace {
        merged.show_whitespace = value;
    }
    if let Some(value) = settings.wrap_lines {
        merged.wrap_lines = value;
    }
    if let Some(value) = settings.vim_navigation {
        merged.vim_navigation = value;
    }
    if let Some(value) = settings.prompt_path_style {
        merged.prompt_path_style = value;
    }
    if let Some(value) = settings.prompt_include_diff_hunks {
        merged.prompt_include_diff_hunks = value;
    }
    if let Some(value) = settings.prompt_include_git_state {
        merged.prompt_include_git_state = value;
    }
    if let Some(value) = settings.shortcuts {
        merged.shortcuts = value;
    }
    state
        .controller
        .save_settings(merged)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn get_workspace_ui_state(
    workspace_id: String,
    state: State<'_, AppState>,
) -> Result<WorkspaceUiStateView, ApiError> {
    state
        .controller
        .workspace_ui_state(parse_workspace(&workspace_id)?)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn save_workspace_ui_state(
    workspace_id: String,
    state: WorkspaceUiStatePatch,
    app_state: State<'_, AppState>,
) -> Result<WorkspaceUiStateView, ApiError> {
    app_state
        .controller
        .save_workspace_ui_state(parse_workspace(&workspace_id)?, state)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn copy_review_item(
    workspace_id: String,
    request: CopyReviewItemRequest,
    state: State<'_, AppState>,
) -> Result<String, ApiError> {
    state
        .controller
        .copy_review_item(parse_workspace(&workspace_id)?, request)
        .map_err(ApiError::from)
}

#[tauri::command]
pub fn open_in_external_editor(
    workspace_id: String,
    file_id: String,
    line: Option<u32>,
    state: State<'_, AppState>,
) -> Result<(), ApiError> {
    state
        .controller
        .open_in_external_editor(parse_workspace(&workspace_id)?, parse_file(&file_id)?, line)
        .map_err(ApiError::from)
}

#[tauri::command]
/// Tauri invokes command arguments as a single flat object. Keeping this
/// parameter flat (`{ text }`) matches the native adapter and prevents a
/// silent clipboard no-op caused by expecting `{ request: { text } }`.
pub fn copy_to_clipboard(text: String) -> Result<(), ApiError> {
    let mut clipboard = arboard::Clipboard::new().map_err(|error| ApiError {
        code: "clipboard_unavailable",
        message: error.to_string(),
        recovery_preview_token: None,
    })?;
    clipboard.set_text(text).map_err(|error| ApiError {
        code: "clipboard_failed",
        message: error.to_string(),
        recovery_preview_token: None,
    })
}

pub(crate) fn activate_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn parse_workspace(value: &str) -> Result<localreview_domain::WorkspaceId, ApiError> {
    uuid::Uuid::parse_str(value)
        .map(localreview_domain::WorkspaceId)
        .map_err(|_| ApiError {
            code: "invalid_request",
            message: "workspaceId is invalid".into(),
            recovery_preview_token: None,
        })
}

fn parse_file(value: &str) -> Result<localreview_domain::ReviewFileId, ApiError> {
    uuid::Uuid::parse_str(value)
        .map(localreview_domain::ReviewFileId)
        .map_err(|_| ApiError {
            code: "invalid_request",
            message: "fileId is invalid".into(),
            recovery_preview_token: None,
        })
}

fn parse_comparison(value: &str) -> Result<localreview_domain::ComparisonId, ApiError> {
    uuid::Uuid::parse_str(value)
        .map(localreview_domain::ComparisonId)
        .map_err(|_| ApiError {
            code: "invalid_request",
            message: "comparisonId is invalid".into(),
            recovery_preview_token: None,
        })
}

fn parse_annotation(value: &str) -> Result<localreview_domain::AnnotationId, ApiError> {
    uuid::Uuid::parse_str(value)
        .map(localreview_domain::AnnotationId)
        .map_err(|_| ApiError {
            code: "invalid_request",
            message: "annotationId is invalid".into(),
            recovery_preview_token: None,
        })
}

fn parse_side(value: &str) -> Result<localreview_domain::DiffSide, ApiError> {
    match value {
        "old" => Ok(localreview_domain::DiffSide::Old),
        "new" => Ok(localreview_domain::DiffSide::New),
        _ => Err(ApiError {
            code: "invalid_request",
            message: "side must be old or new".into(),
            recovery_preview_token: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatWorkspaceArgs {
        workspace_id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatRowsArgs {
        file_id: String,
        mode: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatClipboardArgs {
        text: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatPullRequestArgs {
        url: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatPresentationArgs {
        request: PresentationRequest,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatPresentationLocationArgs {
        file_id: String,
        mode: String,
        side: String,
        line: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatCapturedSourceRangeArgs {
        file_id: String,
        side: String,
        start_line: u32,
        end_line: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatCapturedBlameArgs {
        workspace_id: String,
        file_id: String,
        side: String,
        start_line: u32,
        end_line: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatCommitContextArgs {
        workspace_id: String,
        request: CommitContextInput,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatChangedSincePreviousArgs {
        workspace_id: String,
        repository_id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatFinishReviewArgs {
        workspace_id: String,
        submission: FinishReviewSubmissionInput,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatDraftArgs {
        draft: AnnotationDraft,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatExpandArgs {
        file_id: String,
        hunk_id: String,
        context_lines: u32,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatOutlineArgs {
        file_id: String,
        side: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatAnnotationStateArgs {
        workspace_id: String,
        annotation_id: String,
        state: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatWorkspaceUiArgs {
        workspace_id: String,
        state: WorkspaceUiStatePatch,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatCopyItemArgs {
        workspace_id: String,
        request: CopyReviewItemRequest,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct FlatEditorArgs {
        workspace_id: String,
        file_id: String,
        line: Option<u32>,
    }

    #[test]
    fn frontend_invoke_names_and_flat_argument_json_match_the_native_contract() {
        assert_eq!(
            REVIEW_API_COMMANDS,
            [
                "pick_local_folder",
                "open_workspace",
                "open_github_pr",
                "open_ssh_workspace",
                "reconnect_ssh_workspace",
                "focus_workspace",
                "list_workspaces",
                "list_archived_workspaces",
                "reopen_archived_workspace",
                "archive_workspace",
                "update_workspace_metadata",
                "get_persistence_diagnostics",
                "load_review",
                "load_archived_review",
                "get_review_file_classifications",
                "get_captured_blame",
                "get_commit_context",
                "get_changed_since_previous_review",
                "get_github_update_status",
                "get_presentation_window",
                "get_presentation_rows",
                "resolve_presentation_location",
                "get_captured_source_range",
                "expand_hunk_context",
                "get_outline",
                "save_annotation",
                "get_annotation_draft",
                "save_annotation_draft",
                "clear_annotation_draft",
                "delete_annotation",
                "set_annotation_state",
                "archive_annotations",
                "restore_annotations",
                "restore_history_item",
                "generate_prompt",
                "save_prompt_export",
                "get_review_history",
                "set_viewed",
                "get_repository_setup",
                "set_repository_inclusion",
                "apply_repository_base",
                "reset_repository_base_overrides",
                "fetch_repositories",
                "configure_baselines",
                "start_new_review",
                "refresh_review",
                "preview_finish_review",
                "finish_review",
                "abandon_finish_review",
                "delete_workspace",
                "get_github_pull_request",
                "get_github_threads",
                "get_github_conversation",
                "get_ui_settings",
                "save_ui_settings",
                "get_workspace_ui_state",
                "save_workspace_ui_state",
                "copy_review_item",
                "open_in_external_editor",
                "copy_to_clipboard",
            ]
        );
        let workspace: FlatWorkspaceArgs =
            serde_json::from_str(r#"{"workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b"}"#)
                .unwrap();
        assert_eq!(workspace.workspace_id.len(), 36);
        let rows: FlatRowsArgs = serde_json::from_str(
            r#"{"fileId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","mode":"unified"}"#,
        )
        .unwrap();
        assert_eq!(rows.mode, "unified");
        assert_eq!(rows.file_id.len(), 36);
        let clipboard: FlatClipboardArgs =
            serde_json::from_str(r#"{"text":"review prompt"}"#).unwrap();
        assert_eq!(clipboard.text, "review prompt");
        assert!(serde_json::from_str::<FlatClipboardArgs>(r#"{"request":{"text":"no"}}"#).is_err());
        let pull_request: FlatPullRequestArgs =
            serde_json::from_str(r#"{"url":"https://github.com/acme/repo/pull/42"}"#).unwrap();
        assert_eq!(pull_request.url, "https://github.com/acme/repo/pull/42");
        assert!(
            serde_json::from_str::<FlatPullRequestArgs>(r#"{"request":{"url":"no"}}"#).is_err()
        );
        let presentation: FlatPresentationArgs = serde_json::from_str(
            r#"{"request":{"fileId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","mode":"split","startRow":20,"endRow":80,"generation":4,"fullFileSide":"new","splitRatio":0.55}}"#,
        )
        .unwrap();
        assert_eq!(presentation.request.generation, 4);
        let location: FlatPresentationLocationArgs = serde_json::from_str(
            r#"{"fileId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","mode":"difftastic","side":"new","line":4000}"#,
        )
        .unwrap();
        assert_eq!(
            (location.mode, location.side, location.line),
            ("difftastic".into(), "new".into(), 4000)
        );
        assert_eq!(location.file_id.len(), 36);
        let range: FlatCapturedSourceRangeArgs = serde_json::from_str(
            r#"{"fileId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","side":"old","startLine":40,"endLine":42}"#,
        )
        .unwrap();
        assert_eq!(
            (range.side, range.start_line, range.end_line),
            ("old".into(), 40, 42)
        );
        assert_eq!(range.file_id.len(), 36);
        let blame: FlatCapturedBlameArgs = serde_json::from_str(
            r#"{"workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","fileId":"b7a47dd5-4714-494e-8a56-2042dd7f6c3b","side":"new","startLine":40,"endLine":42}"#,
        )
        .unwrap();
        assert_eq!(
            (blame.side, blame.start_line, blame.end_line),
            ("new".into(), 40, 42)
        );
        assert_eq!((blame.workspace_id.len(), blame.file_id.len()), (36, 36));
        let commit_context: FlatCommitContextArgs = serde_json::from_str(
            r#"{"workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","request":{"repositoryId":"b7a47dd5-4714-494e-8a56-2042dd7f6c3b","maxEntries":100,"includeMergeCommits":false,"authorContains":"Ada","subjectContains":"parser","selectedCommit":"0123456789012345678901234567890123456789"}}"#,
        )
        .unwrap();
        assert_eq!(commit_context.workspace_id.len(), 36);
        assert_eq!(commit_context.request.max_entries, Some(100));
        let changed: FlatChangedSincePreviousArgs = serde_json::from_str(
            r#"{"workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","repositoryId":"b7a47dd5-4714-494e-8a56-2042dd7f6c3b"}"#,
        )
        .unwrap();
        assert_eq!(
            (changed.workspace_id.len(), changed.repository_id.len()),
            (36, 36)
        );
        let finish: FlatFinishReviewArgs = serde_json::from_str(
            r#"{"workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","submission":{"previewToken":"preview-capability"}}"#,
        )
        .unwrap();
        assert_eq!(finish.workspace_id.len(), 36);
        assert_eq!(finish.submission.preview_token, "preview-capability");
        assert!(serde_json::from_str::<FlatFinishReviewArgs>(
            r#"{"workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","submission":{"previewToken":"preview-capability","summary":"must not cross submit boundary"}}"#,
        )
        .is_err());
        let draft: FlatDraftArgs = serde_json::from_str(
            r#"{"draft":{"id":"draft-1","workspaceId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","fileId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","repositoryId":"a7a47dd5-4714-494e-8a56-2042dd7f6c3b","kind":"comment","side":"new","startLine":42,"endLine":42,"body":"unfinished","updatedAt":"2026-07-22T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(draft.draft.body, "unfinished");
        let expand: FlatExpandArgs =
            serde_json::from_str(r#"{"fileId":"file","hunkId":"hunk-1","contextLines":30}"#)
                .unwrap();
        assert_eq!(
            (expand.file_id, expand.hunk_id, expand.context_lines),
            ("file".into(), "hunk-1".into(), 30)
        );
        let outline: FlatOutlineArgs =
            serde_json::from_str(r#"{"fileId":"file","side":"new"}"#).unwrap();
        assert_eq!(
            (outline.file_id, outline.side),
            ("file".into(), "new".into())
        );
        let state_args: FlatAnnotationStateArgs = serde_json::from_str(
            r#"{"workspaceId":"workspace","annotationId":"annotation","state":"resolved"}"#,
        )
        .unwrap();
        assert_eq!(
            (
                state_args.workspace_id,
                state_args.annotation_id,
                state_args.state
            ),
            ("workspace".into(), "annotation".into(), "resolved".into())
        );
        let ui_args: FlatWorkspaceUiArgs = serde_json::from_str(
            r#"{"workspaceId":"workspace","state":{"activeFileId":"file","mode":"full","fullFileSide":null,"nearestSourceLine":42,"nearestSourceSide":null,"scrollTop":null,"splitRatio":0.58,"rightTab":null}}"#,
        )
        .unwrap();
        assert_eq!(ui_args.workspace_id, "workspace");
        assert_eq!(ui_args.state.mode.as_deref(), Some("full"));
        let copy: FlatCopyItemArgs = serde_json::from_str(
            r#"{"workspaceId":"workspace","request":{"kind":"path","fileId":"file","side":null,"startLine":null,"endLine":null}}"#,
        )
        .unwrap();
        assert_eq!(copy.request.kind, "path");
        assert_eq!(copy.workspace_id, "workspace");
        let editor: FlatEditorArgs =
            serde_json::from_str(r#"{"workspaceId":"workspace","fileId":"file","line":19}"#)
                .unwrap();
        assert_eq!(
            (editor.workspace_id, editor.file_id, editor.line),
            ("workspace".into(), "file".into(), Some(19))
        );
        let settings: PartialReviewSettings =
            serde_json::from_str(r#"{"lastWorkspaceId":"workspace-last","fontScale":1.2,"leftCollapsed":true,"theme":"light","codeFont":"JetBrains Mono","tabWidth":4,"showWhitespace":true,"wrapLines":true,"vimNavigation":true,"promptPathStyle":"qualified","promptIncludeDiffHunks":true,"promptIncludeGitState":true,"shortcuts":{"nextHunk":"Alt+J"}}"#).unwrap();
        assert_eq!(
            settings.last_workspace_id.as_deref(),
            Some("workspace-last")
        );
        assert_eq!(settings.font_scale, Some(1.2));
        assert_eq!(settings.left_collapsed, Some(true));
        assert_eq!(settings.theme.as_deref(), Some("light"));
        assert_eq!(settings.tab_width, Some(4));
        assert_eq!(settings.wrap_lines, Some(true));
        assert_eq!(settings.prompt_path_style.as_deref(), Some("qualified"));
        assert_eq!(settings.prompt_include_diff_hunks, Some(true));
        assert_eq!(settings.prompt_include_git_state, Some(true));
        assert_eq!(
            settings
                .shortcuts
                .as_ref()
                .and_then(|value| value.get("nextHunk"))
                .map(String::as_str),
            Some("Alt+J")
        );
        let picked = PickedFolder { path: None };
        assert_eq!(
            serde_json::to_value(picked).unwrap(),
            serde_json::json!({"path": null})
        );
    }

    #[test]
    fn publication_recovery_errors_have_stable_machine_readable_codes() {
        let ambiguous = ApiError::from(DispatchError::Service(
            localreview_service::ServiceError::GitHubPublicationAmbiguous {
                preview_token: "preview-1".into(),
                reason: "transport closed".into(),
            },
        ));
        assert_eq!(ambiguous.code, "github_publication_ambiguous");
        assert_eq!(
            ambiguous.recovery_preview_token.as_deref(),
            Some("preview-1")
        );

        let pending = ApiError::from(DispatchError::Service(
            localreview_service::ServiceError::GitHubPublicationReconciliationPending {
                preview_token: "preview-1".into(),
            },
        ));
        assert_eq!(pending.code, "github_publication_reconciliation_pending");
        assert_eq!(pending.recovery_preview_token.as_deref(), Some("preview-1"));
    }

    #[test]
    fn refresh_command_stays_async_and_offloads_blocking_controller_work() {
        // Tauri executes a synchronous command body inline in invoke dispatch.
        // Keep this small source-level contract beside the invoke-name contract
        // so a future signature cleanup cannot silently put Git capture back on
        // the WebView/event-loop path.
        let source = include_str!("api.rs");
        let start = source
            .find("pub async fn refresh_review(")
            .expect("refresh_review must remain an async Tauri command");
        let tail = &source[start..];
        let end = tail
            .find("\n#[tauri::command]")
            .expect("refresh_review should be followed by another command");
        let body = &tail[..end];
        assert!(body.contains("Arc::clone(&state.controller)"));
        assert!(body.contains("spawn_blocking(move ||"));
        assert!(body.contains("refresh_worker_failed"));
    }

    #[test]
    fn github_network_boundaries_stay_off_the_tauri_invoke_thread() {
        let source = include_str!("api.rs");
        for (signature, worker_code) in [
            ("pub async fn open_github_pr(", "github_open_worker_failed"),
            (
                "pub async fn get_github_update_status(",
                "github_status_worker_failed",
            ),
            ("pub async fn finish_review(", "github_review_worker_failed"),
        ] {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("{signature} must remain async"));
            let tail = &source[start..];
            let end = tail.find("\n#[tauri::command]").unwrap_or(tail.len());
            let body = &tail[..end];
            assert!(body.contains("Arc::clone(&state.controller)"));
            assert!(body.contains("spawn_blocking(move ||"));
            assert!(body.contains(worker_code));
        }
    }

    #[test]
    fn workspace_open_boundaries_stay_off_the_tauri_invoke_thread() {
        let source = include_str!("api.rs");
        for (signature, worker_code) in [
            ("pub async fn open_workspace(", "local_open_worker_failed"),
            ("pub async fn open_ssh_workspace(", "ssh_open_worker_failed"),
            (
                "pub async fn reconnect_ssh_workspace(",
                "ssh_reconnect_worker_failed",
            ),
        ] {
            let start = source
                .find(signature)
                .unwrap_or_else(|| panic!("{signature} must remain async"));
            let tail = &source[start..];
            let end = tail.find("\n#[tauri::command]").unwrap_or(tail.len());
            let body = &tail[..end];
            assert!(body.contains("Arc::clone(&state.controller)"));
            assert!(body.contains("spawn_blocking(move ||"));
            assert!(body.contains(worker_code));
        }
    }
}
