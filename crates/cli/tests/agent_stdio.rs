use localreview_protocol::{
    read_frame, write_frame, AgentErrorCode, AgentMessage, AgentOperation, AgentProgressPhase,
    AgentRequest, AgentResponse, AgentResult, RemoteComparisonOptions, RemoteFileStatus,
    RemoteRepositoryRef, RemoteSourceRevision, PROTOCOL_VERSION,
};
use localreview_ssh::{ReverseForwardListener, SshConnectionConfig, SshDestination, SshSession};
use std::fs;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

fn git(path: &Path, arguments: &[&str]) {
    let result = Command::new("git")
        .current_dir(path)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn fixture_repository() -> tempfile::TempDir {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("work");
    let repo = root.join("a");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "master"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test User"]);
    fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-m", "base"]);
    git(&repo, &["switch", "-c", "feature"]);
    fs::write(repo.join("file.txt"), "changed\n").unwrap();
    fs::write(repo.join("draft.txt"), "untracked\n").unwrap();
    fs::create_dir_all(root.join("not-a-repository")).unwrap();
    temporary
}

fn manifest_fixture_repository() -> tempfile::TempDir {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("work");
    let repo = root.join("manifest");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "master"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Test User"]);
    fs::write(repo.join("old-name.txt"), "rename base\n").unwrap();
    fs::write(repo.join("script.sh"), "#!/bin/sh\necho base\n").unwrap();
    fs::write(repo.join("asset.bin"), b"\0base").unwrap();
    fs::write(repo.join("lfs.txt"), "ordinary source\n").unwrap();
    fs::write(repo.join("large.txt"), "base\n").unwrap();
    fs::write(repo.join("wide.txt"), "base\n").unwrap();
    fs::write(repo.join("copy-source.txt"), "copy me exactly\n").unwrap();
    fs::write(repo.join("untouched.txt"), "not in the review\n").unwrap();
    git(
        &repo,
        &[
            "add",
            "old-name.txt",
            "script.sh",
            "asset.bin",
            "lfs.txt",
            "large.txt",
            "wide.txt",
            "copy-source.txt",
            "untouched.txt",
        ],
    );
    let initial_gitlink = "1111111111111111111111111111111111111111";
    let initial_cache_entry = format!("160000,{initial_gitlink},deps/module");
    let result = Command::new("git")
        .current_dir(&repo)
        .args(["update-index", "--add", "--cacheinfo", &initial_cache_entry])
        .output()
        .unwrap();
    assert!(result.status.success());
    git(&repo, &["commit", "-m", "base"]);
    git(&repo, &["switch", "-c", "feature"]);

    fs::write(repo.join("committed.txt"), "feature commit\n").unwrap();
    git(&repo, &["add", "committed.txt"]);
    git(&repo, &["commit", "-m", "feature commit"]);

    git(&repo, &["mv", "old-name.txt", "new-name.txt"]);
    fs::copy(repo.join("copy-source.txt"), repo.join("copy-target.txt")).unwrap();
    git(&repo, &["add", "copy-target.txt"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(repo.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(repo.join("asset.bin"), b"\0changed-binary").unwrap();
    fs::write(
        repo.join("lfs.txt"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef\nsize 12\n",
    )
    .unwrap();
    fs::write(repo.join("wide.txt"), "x".repeat(3 * 1024 * 1024)).unwrap();
    fs::write(
        repo.join("large.txt"),
        "a changed source line that remains safely viewport chunked\n".repeat(80_000),
    )
    .unwrap();
    let changed_gitlink = "2222222222222222222222222222222222222222";
    let cache_entry = format!("160000,{changed_gitlink},deps/module");
    let result = Command::new("git")
        .current_dir(&repo)
        .args(["update-index", "--add", "--cacheinfo", &cache_entry])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git update-index: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    temporary
}

fn companion() -> (
    Child,
    BufWriter<ChildStdin>,
    BufReader<std::process::ChildStdout>,
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .args(["agent", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = BufWriter::new(child.stdin.take().unwrap());
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

fn local_ssh_proxy(temporary: &tempfile::TempDir) -> std::path::PathBuf {
    local_ssh_proxy_with_body(temporary, "exec")
}

fn local_ssh_proxy_with_body(temporary: &tempfile::TempDir, prefix: &str) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let proxy = temporary.path().join("fixture-ssh");
        let binary = env!("CARGO_BIN_EXE_localreview").replace('"', "\\\"");
        fs::write(
            &proxy,
            format!("#!/bin/sh\n{prefix} \"{binary}\" agent --stdio\n"),
        )
        .unwrap();
        fs::set_permissions(&proxy, fs::Permissions::from_mode(0o700)).unwrap();
        proxy
    }
    #[cfg(not(unix))]
    compile_error!("LocalReview SSH companion fixtures require Unix");
}

fn send(writer: &mut BufWriter<ChildStdin>, request: AgentRequest) {
    write_frame(writer, &AgentMessage::Request(request)).unwrap();
    writer.flush().unwrap();
}

fn response_for(
    reader: &mut BufReader<std::process::ChildStdout>,
    request_id: &str,
) -> AgentResponse {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {request_id}"
        );
        match read_frame(reader).unwrap() {
            AgentMessage::Response(response) if response.id == request_id => return response,
            AgentMessage::Progress(progress) => assert!(matches!(
                progress.phase,
                AgentProgressPhase::Validating
                    | AgentProgressPhase::Discovering
                    | AgentProgressPhase::ResolvingBase
                    | AgentProgressPhase::Capturing
                    | AgentProgressPhase::ReadingSource
                    | AgentProgressPhase::Watching
                    | AgentProgressPhase::Complete
            )),
            _ => {}
        }
    }
}

#[test]
fn real_companion_discovers_captures_and_reads_a_fixture_repository() {
    let fixture = fixture_repository();
    let workspace = fixture.path().join("work").to_string_lossy().into_owned();
    let (mut child, mut writer, mut reader) = companion();

    send(
        &mut writer,
        AgentRequest {
            id: "discover".into(),
            generation: 1,
            operation: AgentOperation::DiscoverRepositories {
                root: workspace.clone(),
                max_depth: 3,
            },
        },
    );
    let discovery = response_for(&mut reader, "discover");
    let repositories = match discovery.result {
        AgentResult::Repositories { repositories } => repositories,
        other => panic!("unexpected discovery result: {other:?}"),
    };
    assert_eq!(repositories.len(), 1);
    let reference = repositories[0].reference.clone();
    assert_eq!(reference.relative_path, "a");

    send(
        &mut writer,
        AgentRequest {
            id: "capture".into(),
            generation: 2,
            operation: AgentOperation::CaptureComparison {
                repository: reference.clone(),
                base: "master".into(),
                options: RemoteComparisonOptions::default(),
            },
        },
    );
    let captured = response_for(&mut reader, "capture");
    let capture = match captured.result {
        AgentResult::ComparisonCapture { capture } => capture,
        other => panic!("unexpected capture result: {other:?}"),
    };
    assert!(capture.unstaged.changed_files > 0);
    assert_eq!(capture.generation, 2);
    assert!(capture
        .files
        .iter()
        .any(|file| file.path == "draft.txt" && file.untracked));
    send(
        &mut writer,
        AgentRequest {
            id: "source".into(),
            generation: 2,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 2,
                repository: reference,
                path: "file.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 20,
            },
        },
    );
    let source = response_for(&mut reader, "source");
    assert!(matches!(
        source.result,
        AgentResult::SourceWindow { ref window }
            if window.capture_id == capture.capture_id && window.bytes == b"changed\n"
    ));
    fs::write(
        fixture.path().join("work/a/file.txt"),
        "changed after capture\n",
    )
    .unwrap();
    send(
        &mut writer,
        AgentRequest {
            id: "stale-after-change".into(),
            generation: 2,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 2,
                repository: RemoteRepositoryRef {
                    workspace_root: fixture.path().join("work").to_string_lossy().into_owned(),
                    relative_path: "a".into(),
                },
                path: "file.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 20,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "stale-after-change").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::StaleCapture
    ));
    drop(writer);
    child.wait().unwrap();
}

#[test]
fn remote_capture_is_manifest_only_and_preserves_non_hunk_git_semantics() {
    let fixture = manifest_fixture_repository();
    let workspace = fixture.path().join("work").to_string_lossy().into_owned();
    let (mut child, mut writer, mut reader) = companion();
    let reference = RemoteRepositoryRef {
        workspace_root: workspace,
        relative_path: "manifest".into(),
    };
    send(
        &mut writer,
        AgentRequest {
            id: "manifest-capture".into(),
            generation: 41,
            operation: AgentOperation::CaptureComparison {
                repository: reference.clone(),
                base: "master".into(),
                options: RemoteComparisonOptions::default(),
            },
        },
    );
    let response = response_for(&mut reader, "manifest-capture");
    assert_eq!(response.generation, 41);
    let capture = match response.result {
        AgentResult::ComparisonCapture { capture } => capture,
        other => panic!("unexpected capture result: {other:?}"),
    };
    // Four-ish megabytes of changed source are not retained in the companion.
    // The wire response is a compact layer-aware manifest only.
    assert!(capture.unstaged.changed_files >= 4);
    assert!(capture.committed.changed_files >= 1);
    assert!(capture.staged.changed_files >= 2);
    assert!(capture.capture_id.starts_with("cap-"));
    assert_eq!(capture.capture_id.len(), 36);
    assert!(serde_cbor::to_vec(&capture).unwrap().len() < 128 * 1024);
    let encoded = serde_json::to_string(&capture).unwrap();
    assert!(!encoded.contains("patch"));
    assert!(capture.files.iter().any(|file| {
        file.path == "new-name.txt"
            && file.old_path.as_deref() == Some("old-name.txt")
            && file.status == RemoteFileStatus::Renamed
            && file.similarity_percent.is_some()
            && file.old_object_id.is_some()
            && file
                .layers
                .contains(&localreview_protocol::RemoteChangeLayer::Staged)
    }));
    assert!(capture.files.iter().any(|file| {
        file.path == "copy-target.txt"
            && file.old_path.as_deref() == Some("copy-source.txt")
            && file.status == RemoteFileStatus::Copied
            && file.similarity_percent == Some(100)
    }));
    assert!(capture.files.iter().any(|file| {
        file.path == "script.sh"
            && file.status == RemoteFileStatus::ModeChanged
            && file.old_mode != file.new_mode
    }));
    assert!(capture.files.iter().any(|file| {
        file.path == "asset.bin" && file.binary && file.status == RemoteFileStatus::Modified
    }));
    assert!(capture.files.iter().any(|file| {
        file.path == "lfs.txt" && file.lfs_pointer && file.status == RemoteFileStatus::Modified
    }));
    assert!(capture.files.iter().any(|file| {
        file.path == "deps/module"
            && file.status == RemoteFileStatus::Submodule
            && file.old_mode != file.new_mode
    }));

    // The selected file is fetched in a viewport window from the immutable
    // capture. It is not reread from the remote worktree after capture.
    send(
        &mut writer,
        AgentRequest {
            id: "large-window".into(),
            generation: 41,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 41,
                repository: reference.clone(),
                path: "large.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 4_096,
            },
        },
    );
    let source = response_for(&mut reader, "large-window");
    assert!(matches!(
        source.result,
        AgentResult::SourceWindow { ref window }
            if window.capture_id == capture.capture_id
                && window.bytes.iter().filter(|byte| **byte == b'\n').count() == 4_096
                && window.total_lines == 80_000
    ));

    // A base request for a renamed file is bound to the same immutable
    // comparison. The caller supplies the old path for the base side.
    send(
        &mut writer,
        AgentRequest {
            id: "renamed-base".into(),
            generation: 41,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 41,
                repository: reference.clone(),
                path: "old-name.txt".into(),
                revision: RemoteSourceRevision::MergeBase,
                start_line: 1,
                line_count: 10,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "renamed-base").result,
        AgentResult::SourceWindow { ref window } if window.bytes == b"rename base\n"
    ));

    send(
        &mut writer,
        AgentRequest {
            id: "wide-window".into(),
            generation: 41,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 41,
                repository: reference.clone(),
                path: "wide.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 1,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "wide-window").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::TooLarge
    ));

    send(
        &mut writer,
        AgentRequest {
            id: "unrelated-source".into(),
            generation: 41,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 41,
                repository: reference.clone(),
                path: "untouched.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 1,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "unrelated-source").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::PathDenied
    ));

    send(
        &mut writer,
        AgentRequest {
            id: "wrong-generation".into(),
            generation: 42,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: capture.capture_id.clone(),
                capture_generation: 41,
                repository: reference.clone(),
                path: "large.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 1,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "wrong-generation").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::StaleGeneration
    ));

    send(
        &mut writer,
        AgentRequest {
            id: "stale-window".into(),
            generation: 41,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: "capture-41-unknown".into(),
                capture_generation: 41,
                repository: reference,
                path: "large.txt".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 1,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "stale-window").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::StaleCapture
    ));
    drop(writer);
    child.wait().unwrap();
}

#[test]
fn real_companion_rejects_bad_paths_versions_and_cancels_watchers() {
    let fixture = fixture_repository();
    let workspace = fixture.path().join("work").to_string_lossy().into_owned();
    let watched_file = fixture.path().join("work/a/draft.txt");
    let (mut child, mut writer, mut reader) = companion();

    send(
        &mut writer,
        AgentRequest {
            id: "bad-version".into(),
            generation: 1,
            operation: AgentOperation::Handshake {
                desktop_versions: vec![PROTOCOL_VERSION + 1],
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "bad-version").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::UnsupportedVersion
    ));

    send(
        &mut writer,
        AgentRequest {
            id: "bad-path".into(),
            generation: 2,
            operation: AgentOperation::ReadSourceWindow {
                capture_id: "capture-2-1".into(),
                capture_generation: 2,
                repository: RemoteRepositoryRef {
                    workspace_root: workspace.clone(),
                    relative_path: "a".into(),
                },
                path: "../outside".into(),
                revision: RemoteSourceRevision::Worktree,
                start_line: 1,
                line_count: 1,
            },
        },
    );
    assert!(matches!(
        response_for(&mut reader, "bad-path").result,
        AgentResult::Error { ref error } if error.code == AgentErrorCode::InvalidRequest
    ));

    send(
        &mut writer,
        AgentRequest {
            id: "watch".into(),
            generation: 3,
            operation: AgentOperation::WatchRepositoryChanges {
                repository: RemoteRepositoryRef {
                    workspace_root: workspace,
                    relative_path: "a".into(),
                },
                poll_interval_millis: 250,
            },
        },
    );
    let watching_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < watching_deadline,
            "watcher did not become active"
        );
        if let AgentMessage::Progress(progress) = read_frame(&mut reader).unwrap() {
            if progress.id == "watch" && progress.phase == AgentProgressPhase::Watching {
                break;
            }
        }
    }
    // An edit to an already-untracked path does not change porcelain status;
    // the watcher fingerprints bounded stat/status metadata and only notifies
    // that Refresh is available. It never captures a new review itself.
    fs::write(watched_file, "untracked changed\n").unwrap();
    let changed_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < changed_deadline,
            "watcher did not report the content change"
        );
        if let AgentMessage::Notification(
            localreview_protocol::AgentNotification::FilesystemChangesAvailable {
                repository, ..
            },
        ) = read_frame(&mut reader).unwrap()
        {
            assert_eq!(repository.relative_path, "a");
            break;
        }
    }
    let watched_repo = fixture.path().join("work/a");
    git(&watched_repo, &["add", "file.txt", "draft.txt"]);
    git(&watched_repo, &["commit", "-m", "watcher head advance"]);
    let head_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < head_deadline,
            "watcher did not report a branch HEAD advance"
        );
        if matches!(
            read_frame(&mut reader).unwrap(),
            AgentMessage::Notification(
                localreview_protocol::AgentNotification::FilesystemChangesAvailable { .. }
            )
        ) {
            break;
        }
    }
    // The watcher is intentionally long-running. Cancel it from a separate
    // request and prove both the acknowledgement and original completion.
    send(
        &mut writer,
        AgentRequest {
            id: "cancel-watch".into(),
            generation: 3,
            operation: AgentOperation::Cancel {
                request_id: "watch".into(),
            },
        },
    );
    let mut accepted = false;
    let mut cancelled = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !accepted || !cancelled {
        assert!(Instant::now() < deadline, "watch cancellation timed out");
        let AgentMessage::Response(response) = read_frame(&mut reader).unwrap() else {
            continue;
        };
        if response.id == "cancel-watch" {
            accepted = matches!(response.result, AgentResult::CancelAccepted { .. });
        }
        if response.id == "watch" {
            cancelled = matches!(response.result, AgentResult::Cancelled);
        }
    }
    drop(writer);
    child.wait().unwrap();
}

#[test]
fn desktop_ssh_client_uses_a_real_companion_fixture_and_can_cancel_a_job() {
    let fixture = fixture_repository();
    let workspace = fixture.path().join("work").to_string_lossy().into_owned();
    let proxy = local_ssh_proxy(&fixture);
    let mut config = SshConnectionConfig::new(SshDestination::new("fixture-host").unwrap());
    config.ssh_program = proxy.into_os_string();
    config.request_timeout = Duration::from_secs(5);
    let mut session = SshSession::connect(config).unwrap();
    let cancellation = session.cancellation();
    let root = workspace.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        cancellation.cancel("watch-from-desktop", 11).unwrap();
    });
    let result = session
        .request_with_id(
            "watch-from-desktop".into(),
            AgentOperation::WatchRepositoryChanges {
                repository: RemoteRepositoryRef {
                    workspace_root: root,
                    relative_path: "a".into(),
                },
                poll_interval_millis: 250,
            },
            11,
            Duration::from_secs(5),
            |_| {},
        )
        .unwrap();
    canceller.join().unwrap();
    assert!(matches!(result, AgentResult::Cancelled));
}

#[test]
fn desktop_ssh_client_handles_high_latency_and_disconnect_without_losing_state() {
    let temporary = tempfile::tempdir().unwrap();
    let delayed_proxy = local_ssh_proxy_with_body(&temporary, "sleep 1; exec");
    let mut delayed = SshConnectionConfig::new(SshDestination::new("fixture-host").unwrap());
    delayed.ssh_program = delayed_proxy.into_os_string();
    delayed.connect_timeout = Duration::from_millis(50);
    assert!(matches!(
        SshSession::connect(delayed),
        Err(localreview_ssh::SshError::TimedOut { .. })
    ));

    // This fixture keeps the real companion long enough to handshake, then
    // closes its stdio. A desktop can retain its last capture while state moves
    // to disconnected and future uncached requests fail safely.
    let disconnect_proxy = temporary.path().join("fixture-ssh-disconnect");
    // Explicit bounded companion process; the arguments passed by SshSession
    // are deliberately ignored by this fixture.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let binary = env!("CARGO_BIN_EXE_localreview").replace('"', "\\\"");
        fs::write(
            &disconnect_proxy,
            format!("#!/bin/sh\n(sleep 1; kill -TERM $$) &\nexec \"{binary}\" agent --stdio\n"),
        )
        .unwrap();
        fs::set_permissions(&disconnect_proxy, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut config = SshConnectionConfig::new(SshDestination::new("fixture-host").unwrap());
    config.ssh_program = disconnect_proxy.into_os_string();
    let mut session = SshSession::connect(config).unwrap();
    std::thread::sleep(Duration::from_millis(1_200));
    assert!(session.request(AgentOperation::Ping, 99, |_| {}).is_err());
    assert!(matches!(
        session.state(),
        localreview_ssh::SshConnectionState::Disconnected { .. }
    ));
}

#[test]
fn remote_cli_open_uses_the_managed_reverse_channel_end_to_end() {
    let fixture = fixture_repository();
    let workspace = fixture.path().join("work").canonicalize().unwrap();
    let listener = ReverseForwardListener::bind(42_424, Duration::from_secs(5)).unwrap();
    let token = listener
        .managed_environment()
        .into_iter()
        .find(|(key, _)| key == "LOCALREVIEW_MANAGED_FORWARD_TOKEN")
        .unwrap()
        .1;
    let local_endpoint = format!("127.0.0.1:{}", listener.local_port());
    let accepted = std::thread::spawn(move || listener.accept_open(Duration::from_secs(5)));
    let output = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .args(["--json", "open"])
        .arg(&workspace)
        .env("LOCALREVIEW_MANAGED_FORWARD_ENDPOINT", local_endpoint)
        .env("LOCALREVIEW_MANAGED_FORWARD_TOKEN", token)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reverse CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let forwarded = accepted.join().unwrap().unwrap();
    assert_eq!(forwarded.path, workspace.to_string_lossy());
}

#[cfg(unix)]
#[test]
fn companion_owned_private_relay_forwards_remote_cli_without_exposing_token_to_shell() {
    let fixture = fixture_repository();
    let workspace = fixture.path().join("work").canonicalize().unwrap();
    // macOS Unix-domain paths have a small `sun_path` limit, so keep this
    // deliberately short while still exercising an isolated XDG runtime dir.
    let runtime = tempfile::Builder::new()
        .prefix("lr-rf-")
        .tempdir_in("/tmp")
        .unwrap();
    let listener = ReverseForwardListener::bind(42_426, Duration::from_secs(5)).unwrap();
    let token = listener
        .managed_environment()
        .into_iter()
        .find(|(key, _)| key == "LOCALREVIEW_MANAGED_FORWARD_TOKEN")
        .unwrap()
        .1;
    let endpoint = format!("127.0.0.1:{}", listener.local_port());
    let session = "managed_relay_test_123";
    let socket = runtime.path().join("lr").join(format!("{session}.sock"));
    let accepted = std::thread::spawn(move || listener.accept_open(Duration::from_secs(5)));
    let mut agent = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .args(["agent", "--stdio"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("LOCALREVIEW_MANAGED_FORWARD_ENDPOINT")
        .env_remove("LOCALREVIEW_MANAGED_FORWARD_TOKEN")
        .env_remove("LOCALREVIEW_MANAGED_FORWARD_SESSION")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = agent.stdin.take().unwrap();
    let mut stdout = BufReader::new(agent.stdout.take().unwrap());
    write_frame(
        &mut stdin,
        &AgentMessage::Request(AgentRequest {
            id: "configure-managed-relay".into(),
            generation: 0,
            operation: AgentOperation::ConfigureManagedForwardRelay {
                endpoint,
                token_hex: token,
                session_id: session.into(),
            },
        }),
    )
    .unwrap();
    stdin.flush().unwrap();
    let configured = loop {
        let message: AgentMessage = read_frame(&mut stdout).unwrap();
        if matches!(
            message,
            AgentMessage::Response(AgentResponse {
                ref id,
                generation: 0,
                ..
            }) if id == "configure-managed-relay"
        ) {
            break message;
        }
    };
    assert!(matches!(
        configured,
        AgentMessage::Response(AgentResponse {
            result: AgentResult::ManagedForwardRelayConfigured,
            ..
        })
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "managed companion did not create its private relay socket"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let permissions =
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&socket).unwrap().permissions());
    assert_eq!(permissions & 0o777, 0o600);
    // Crucially, this invocation has neither bearer credential environment
    // variable. It discovers only the same-user socket and the companion
    // performs the authenticated TCP hop on its behalf.
    let output = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .args(["--json", "open"])
        .arg(&workspace)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("LOCALREVIEW_MANAGED_FORWARD_ENDPOINT")
        .env_remove("LOCALREVIEW_MANAGED_FORWARD_TOKEN")
        .env_remove("LOCALREVIEW_MANAGED_FORWARD_SESSION")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "relay CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let forwarded = accepted.join().unwrap().unwrap();
    assert_eq!(forwarded.path, workspace.to_string_lossy());
    drop(stdin);
    drop(stdout);
    assert!(agent.wait().unwrap().success());
    assert!(
        !socket.exists(),
        "the private relay socket must disappear with the companion session"
    );
}

#[cfg(unix)]
#[test]
fn remote_cli_rejects_ambiguous_concurrent_managed_relays_before_opening_any_workspace() {
    let fixture = fixture_repository();
    let workspace = fixture.path().join("work").canonicalize().unwrap();
    let runtime = tempfile::Builder::new()
        .prefix("lr-rf-")
        .tempdir_in("/tmp")
        .unwrap();
    let start_relay = |session: &str| {
        let socket = runtime.path().join("lr").join(format!("{session}.sock"));
        let mut agent = Command::new(env!("CARGO_BIN_EXE_localreview"))
            .args(["agent", "--stdio"])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = agent.stdin.take().unwrap();
        let mut stdout = BufReader::new(agent.stdout.take().unwrap());
        write_frame(
            &mut stdin,
            &AgentMessage::Request(AgentRequest {
                id: format!("configure-{session}"),
                generation: 0,
                operation: AgentOperation::ConfigureManagedForwardRelay {
                    endpoint: "127.0.0.1:42426".into(),
                    token_hex: "ab".repeat(32),
                    session_id: session.into(),
                },
            }),
        )
        .unwrap();
        stdin.flush().unwrap();
        loop {
            let message: AgentMessage = read_frame(&mut stdout).unwrap();
            if matches!(
                message,
                AgentMessage::Response(AgentResponse {
                    result: AgentResult::ManagedForwardRelayConfigured,
                    ..
                })
            ) {
                break;
            }
        }
        (agent, stdin, stdout, socket)
    };
    let (mut first, first_stdin, first_stdout, first_socket) =
        start_relay("managed_relay_ambiguous_one");
    let (mut second, second_stdin, second_stdout, second_socket) =
        start_relay("managed_relay_ambiguous_two");
    assert!(first_socket.exists());
    assert!(second_socket.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_localreview"))
        .args(["--json", "open"])
        .arg(&workspace)
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("multiple managed LocalReview SSH sessions"),
        "unexpected ambiguity error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(first_stdin);
    drop(first_stdout);
    drop(second_stdin);
    drop(second_stdout);
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert!(!first_socket.exists());
    assert!(!second_socket.exists());
}
