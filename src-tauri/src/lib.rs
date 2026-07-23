//! Tauri's intentionally thin native boundary.  It exposes review-domain
//! commands only; the Svelte UI never receives a generic filesystem or shell
//! capability.

mod activation;
mod api;
mod controller;
#[cfg(unix)]
mod rpc;

use api::{
    abandon_finish_review, apply_repository_base, archive_annotations, archive_workspace,
    clear_annotation_draft, configure_baselines, copy_review_item, copy_to_clipboard,
    delete_annotation, delete_workspace, expand_hunk_context, fetch_repositories, finish_review,
    focus_workspace, generate_prompt, get_annotation_draft, get_captured_blame,
    get_captured_source_range, get_changed_since_previous_review, get_commit_context,
    get_github_conversation, get_github_pull_request, get_github_threads, get_github_update_status,
    get_outline, get_persistence_diagnostics, get_presentation_rows, get_presentation_window,
    get_repository_setup, get_review_file_classifications, get_review_history, get_ui_settings,
    get_workspace_ui_state, list_archived_workspaces, list_workspaces, load_archived_review,
    load_review, open_github_pr, open_in_external_editor, open_ssh_workspace, open_workspace,
    pick_local_folder, preview_finish_review, reconnect_ssh_workspace, refresh_review,
    reopen_archived_workspace, reset_repository_base_overrides, resolve_presentation_location,
    restore_annotations, restore_history_item, save_annotation, save_annotation_draft,
    save_prompt_export, save_ui_settings, save_workspace_ui_state, set_annotation_state,
    set_repository_inclusion, set_viewed, start_new_review, update_workspace_metadata,
};
use controller::DesktopController;
use localreview_persistence::{StartupState, StateStore, DEFAULT_BACKUP_INTERVAL};
use std::{sync::Arc, time::Duration};
use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;

pub(crate) const DESKTOP_OPERATION_EVENT: &str = "localreview://desktop-operation";

pub(crate) struct AppState {
    pub controller: Arc<DesktopController>,
}

pub fn run() {
    let state_root = match application_data_root() {
        Ok(path) => path,
        Err(error) => {
            present_startup_error(
                "LocalReview could not locate its application-data directory",
                &error,
            );
            return;
        }
    };
    let state_store = match StateStore::open_for_startup(&state_root) {
        Ok(StartupState::Ready(store)) => store,
        Ok(StartupState::RequiresRecovery(report)) => {
            let backups = if report.recoverable_backups.is_empty() {
                "No validated LocalReview backups were found.".to_owned()
            } else {
                let names = report
                    .recoverable_backups
                    .iter()
                    .map(|backup| backup.backup_file_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Available backup files: {names}.")
            };
            present_startup_error(
                "LocalReview needs explicit database recovery",
                &format!(
                    "{}\n\nLocalReview left the original database untouched. {backups}\n\nRun `localreview recover status`, then restore one listed backup with `localreview recover restore <backup-file-name> --confirm`.",
                    report.diagnostic
                ),
            );
            return;
        }
        Err(error) => {
            present_startup_error(
                "LocalReview could not initialize application data",
                &format!(
                    "Data directory: {}\n\n{error}\n\nCheck that the directory exists, is writable by this user, and is not a temporary location. You can set LOCALREVIEW_DATA_DIR to an explicit durable directory.",
                    state_root.display()
                ),
            );
            return;
        }
    };
    let controller = DesktopController::new(state_store);
    // Never remove a dirty checkout during startup. The repair routine only
    // fixes safe registry/orphan cases; failures stay non-fatal so an
    // unrelated Git issue cannot prevent the desktop from launching.
    let _ = controller.repair_managed_worktree_orphans();
    // Single-instance must be the first plugin so operating-system activation
    // and deep-link argv are delivered to the already-running review window.
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(
            |app, arguments, _cwd| {
                for argument in arguments
                    .iter()
                    .filter(|argument| argument.starts_with("localreview://"))
                {
                    let _ = activation::dispatch_activation_url(app, argument);
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            },
        ))
        .plugin(tauri_plugin_deep_link::init())
        .manage(AppState {
            controller: Arc::new(controller),
        })
        .invoke_handler(tauri::generate_handler![
            pick_local_folder,
            open_workspace,
            open_github_pr,
            open_ssh_workspace,
            reconnect_ssh_workspace,
            focus_workspace,
            list_workspaces,
            list_archived_workspaces,
            reopen_archived_workspace,
            archive_workspace,
            update_workspace_metadata,
            get_persistence_diagnostics,
            load_review,
            load_archived_review,
            get_review_file_classifications,
            get_captured_blame,
            get_commit_context,
            get_changed_since_previous_review,
            get_github_update_status,
            get_presentation_window,
            get_presentation_rows,
            resolve_presentation_location,
            get_captured_source_range,
            expand_hunk_context,
            get_outline,
            save_annotation,
            get_annotation_draft,
            save_annotation_draft,
            clear_annotation_draft,
            delete_annotation,
            set_annotation_state,
            archive_annotations,
            restore_annotations,
            restore_history_item,
            generate_prompt,
            save_prompt_export,
            get_review_history,
            set_viewed,
            get_repository_setup,
            set_repository_inclusion,
            apply_repository_base,
            reset_repository_base_overrides,
            fetch_repositories,
            configure_baselines,
            start_new_review,
            refresh_review,
            preview_finish_review,
            finish_review,
            abandon_finish_review,
            delete_workspace,
            get_github_pull_request,
            get_github_threads,
            get_github_conversation,
            get_ui_settings,
            save_ui_settings,
            get_workspace_ui_state,
            save_workspace_ui_state,
            copy_review_item,
            open_in_external_editor,
            copy_to_clipboard,
        ])
        .setup(|app| {
            app.state::<AppState>()
                .controller
                .attach_app_handle(app.handle().clone())
                .map_err(|error| {
                    Box::<dyn std::error::Error>::from(format!(
                        "could not attach LocalReview desktop events: {error}"
                    ))
                })?;
            let maintenance_store = app.state::<AppState>().controller.state().clone();
            let _ = maintenance_store.backup_if_due(DEFAULT_BACKUP_INTERVAL);
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(60 * 60));
                let _ = maintenance_store.backup_if_due(DEFAULT_BACKUP_INTERVAL);
            });
            #[cfg(unix)]
            rpc::start_local_rpc_server(app.handle().clone()).map_err(|error| {
                Box::<dyn std::error::Error>::from(format!(
                    "could not start LocalReview forwarding endpoint: {error}"
                ))
            })?;

            if let Some(urls) = app.deep_link().get_current()? {
                for url in urls {
                    let _ = activation::dispatch_activation_url(app.handle(), url.as_str());
                }
            }
            let activation_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    let _ = activation::dispatch_activation_url(&activation_handle, url.as_str());
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                window.show()?;
                window.set_focus()?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running LocalReview desktop application");
}

fn application_data_root() -> Result<std::path::PathBuf, String> {
    // The database, content store, forwarding secret, and desktop runtime
    // record must be one private OS-level root. AppPaths also honours
    // LOCALREVIEW_DATA_DIR for hermetic tests and portable installs.
    localreview_protocol::AppPaths::discover()
        .map(|paths| paths.data_dir)
        .map_err(|error| {
            format!(
                "The OS application-data location is unavailable: {error}. LocalReview will not fall back to a temporary directory because review history and forwarding credentials must survive restarts."
            )
        })
}

fn present_startup_error(title: &str, message: &str) {
    eprintln!("{title}: {message}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(title)
        .set_description(message)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

#[cfg(test)]
mod tests {
    #[test]
    fn packaged_main_window_is_visible_on_direct_launch() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            config["app"]["windows"][0]["visible"],
            serde_json::Value::Bool(true),
            "the packaged app must not depend on a later activation event to reveal its main window"
        );
    }
}
