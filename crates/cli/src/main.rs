use clap::{Parser, Subcommand};
use localreview_domain::{
    BaseReference, ComparisonId, ComparisonOptions, RepositoryId, StoredPath,
};
use localreview_git::{
    discover_repositories, DiscoveryConfig, GitError, GitRepository, WorkingTreeChangeKind,
};
use localreview_persistence::{BackupPolicy, StartupState, StateStore};
use localreview_protocol::{
    read_frame, validate_github_pr_url, validate_identifier, validate_ssh_target, write_frame,
    AgentError, AgentErrorCode, AgentErrorScope, AgentHello, AgentMessage, AgentNotification,
    AgentOperation, AgentProgress, AgentProgressPhase, AgentRequest, AgentResponse, AgentResult,
    AppPaths, AuthProof, DoctorReport, LocalCommand, LocalRequest, LocalResponse,
    ManagedForwardCommand, ManagedForwardRelayCommand, ManagedForwardRelayRequest,
    ManagedForwardRequest, ManagedForwardResponse, ProtocolError, RemoteCapturedFile,
    RemoteChangeLayer, RemoteComparisonCapture, RemoteComparisonOptions, RemoteFileStatus,
    RemoteHead, RemoteLayerSummary, RemoteRepository, RemoteRepositoryRef, RemoteSourceRevision,
    RemoteSourceWindow, RepositoryBaseOverride, RuntimeRecord, WorkspaceSummary,
    MAX_REMOTE_CAPTURE_FILES, MAX_REMOTE_SOURCE_WINDOW_BYTES, PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, IsTerminal, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const APP_READINESS_WAIT: Duration = Duration::from_secs(8);
const APP_READINESS_POLL: Duration = Duration::from_millis(100);
const MANAGED_FORWARD_RELAY_POLL: Duration = Duration::from_millis(20);
const MANAGED_FORWARD_RELAY_TIMEOUT: Duration = Duration::from_secs(5);
const MANAGED_FORWARD_RELAY_SERVER_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_MANAGED_FORWARD_RELAY_SOCKETS: usize = 16;

#[derive(Debug, Parser)]
#[command(
    name = "localreview",
    version,
    about = "Open LocalReview workspaces from a shell"
)]
struct Cli {
    /// Emit a machine-readable acknowledgement or error object.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open a local directory in the desktop app, focusing it if already known.
    Open {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        base: Option<String>,
        /// A workspace-relative baseline override in `path=ref` form. Repeatable.
        #[arg(long = "repo-base")]
        repository_bases: Vec<String>,
    },
    /// Focus an existing live workspace by exact ID or unambiguous name.
    #[command(alias = "focus")]
    Workspace { name_or_id: String },
    /// Open a GitHub.com pull request in an isolated desktop review workspace.
    Pr { github_pr_url: String },
    /// Request a desktop-managed SSH review workspace (`host:/absolute/path`).
    Ssh { target: String },
    /// List desktop-registered workspaces.
    List,
    /// Report whether the desktop endpoint and its local authentication material are usable.
    Doctor,
    /// Inspect LocalReview's read-only global defaults location.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect or explicitly restore LocalReview application data after a corruption report.
    Recover {
        #[command(subcommand)]
        command: RecoveryCommand,
    },
    /// Run the restricted, framed SSH companion protocol on stdin/stdout.
    Agent {
        #[arg(long, required = true)]
        stdio: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RecoveryCommand {
    /// Inspect database health and list recoverable local backup file names.
    Status,
    /// Restore one validated local backup. This refuses a healthy active database.
    Restore {
        /// Exact backup file name reported by `localreview recover status`.
        backup_file_name: String,
        /// Required acknowledgement because restore replaces the active corrupt database.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the OS-specific global config.toml path.
    Path,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            emit_error(&error, error.json_output());
            ExitCode::from(error.exit_code())
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Agent { stdio: true } => run_agent_stdio(),
        Command::Agent { stdio: false } => Err(CliError::Usage("agent requires --stdio".into())),
        Command::Open {
            path,
            base,
            repository_bases,
        } => {
            let overrides = repository_bases
                .iter()
                .map(|value| parse_repository_base(value))
                .collect::<Result<Vec<_>, _>>()?;
            let path = resolve_workspace_path(&path)?;
            let response = forward(
                LocalCommand::OpenWorkspace {
                    path,
                    base,
                    repository_bases: overrides,
                },
                true,
            )?;
            emit_response(response, cli.json)
        }
        Command::Workspace { name_or_id } => {
            validate_identifier(&name_or_id, "workspace selector").or_else(|_| {
                // A human workspace name may contain spaces; validation is
                // performed by the desktop after it has a bounded string.
                if name_or_id.is_empty() || name_or_id.len() > 256 || name_or_id.contains('\0') {
                    Err(ProtocolError::InvalidInput(
                        "invalid workspace selector".into(),
                    ))
                } else {
                    Ok(())
                }
            })?;
            emit_response(
                forward(
                    LocalCommand::FocusWorkspace {
                        selector: name_or_id,
                    },
                    true,
                )?,
                cli.json,
            )
        }
        Command::Pr { github_pr_url } => {
            validate_github_pr_url(&github_pr_url)?;
            emit_response(
                forward(LocalCommand::OpenPullRequest { url: github_pr_url }, true)?,
                cli.json,
            )
        }
        Command::Ssh { target } => {
            validate_ssh_target(&target)?;
            emit_response(
                forward(LocalCommand::OpenSshWorkspace { target }, true)?,
                cli.json,
            )
        }
        Command::List => emit_response(forward(LocalCommand::ListWorkspaces, true)?, cli.json),
        Command::Doctor => emit_response(run_doctor()?, cli.json),
        Command::Config {
            command: ConfigCommand::Path,
        } => {
            let path = AppPaths::discover()?.global_config_path();
            let exists = path.is_file();
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "path": path,
                        "exists": exists,
                    })
                );
            } else {
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::Recover { command } => run_recovery(command, cli.json),
    }
}

fn run_recovery(command: RecoveryCommand, json: bool) -> Result<(), CliError> {
    let paths = AppPaths::discover()?;
    match command {
        RecoveryCommand::Status => match StateStore::open_for_startup(paths.data_dir) {
            Ok(StartupState::Ready(store)) => {
                let diagnostics = store.diagnostics(BackupPolicy::default())?;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": true,
                            "status": "ready",
                            "diagnostics": diagnostics,
                        })
                    );
                } else {
                    println!(
                        "Database: healthy\nBackups: {} retained ({} bytes)",
                        diagnostics.backup_storage.retained_count,
                        diagnostics.backup_storage.retained_bytes
                    );
                }
                Ok(())
            }
            Ok(StartupState::RequiresRecovery(report)) => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "ok": false,
                            "status": "recovery_required",
                            "recovery": report,
                        })
                    );
                } else {
                    eprintln!("Database: recovery required\n{}", report.diagnostic);
                    if report.recoverable_backups.is_empty() {
                        eprintln!("No validated LocalReview backups are available.");
                    } else {
                        eprintln!("Available backups:");
                        for backup in report.recoverable_backups {
                            eprintln!(
                                "  {} ({} bytes, {})",
                                backup.backup_file_name, backup.byte_len, backup.created_at
                            );
                        }
                        eprintln!(
                            "Restore exactly one with: localreview recover restore <backup-file-name> --confirm"
                        );
                    }
                }
                Ok(())
            }
            Err(error) => Err(error.into()),
        },
        RecoveryCommand::Restore {
            backup_file_name,
            confirm,
        } => {
            if !confirm {
                return Err(CliError::Usage(
                    "restore is intentionally explicit; rerun with --confirm after reviewing `localreview recover status`"
                        .into(),
                ));
            }
            let result =
                StateStore::restore_from_backup(paths.data_dir.clone(), &backup_file_name)?;
            match StateStore::open_for_startup(paths.data_dir)? {
                StartupState::Ready(_) => {}
                StartupState::RequiresRecovery(report) => {
                    return Err(CliError::Internal(format!(
                        "validated restore did not produce a healthy database: {}",
                        report.diagnostic
                    )))
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "status": "restored",
                        "result": result,
                    })
                );
            } else {
                println!(
                    "Restored {}. Previous database preserved as {}.",
                    result.restored_backup_file_name,
                    result
                        .preserved_database_file_name
                        .as_deref()
                        .unwrap_or("(no prior database file)")
                );
            }
            Ok(())
        }
    }
}

fn parse_repository_base(value: &str) -> Result<RepositoryBaseOverride, CliError> {
    let (relative_path, base) = value.split_once('=').ok_or_else(|| {
        CliError::Usage("--repo-base must use workspace-relative-path=base-ref".into())
    })?;
    let override_ = RepositoryBaseOverride {
        relative_path: relative_path.into(),
        base: base.into(),
    };
    LocalCommand::OpenWorkspace {
        path: "/placeholder".into(),
        base: None,
        repository_bases: vec![override_.clone()],
    }
    .validate()?;
    Ok(override_)
}

fn resolve_workspace_path(path: &Path) -> Result<String, CliError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let canonical = absolute.canonicalize().map_err(|error| {
        CliError::Usage(format!(
            "cannot open {}: {error}",
            absolute.to_string_lossy()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(CliError::Usage(format!(
            "{} is not a directory",
            canonical.to_string_lossy()
        )));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn forward(command: LocalCommand, may_launch: bool) -> Result<LocalResponse, CliError> {
    if let Some(response) = forward_through_companion_relay(&command)? {
        return Ok(response);
    }
    if let Some(response) = forward_through_managed_ssh_session(&command)? {
        return Ok(response);
    }
    let paths = AppPaths::discover()?;
    let endpoint = match load_endpoint(&paths) {
        Ok(endpoint) => endpoint,
        Err(first_error) if may_launch && cfg!(target_os = "macos") => {
            launch_desktop()?;
            wait_for_endpoint(&paths).map_err(|_| first_error)?
        }
        Err(error) => return Err(error),
    };
    match send_request(&endpoint, command.clone()) {
        Err(CliError::Unavailable(_)) if may_launch && cfg!(target_os = "macos") => {
            // A stale runtime record can survive a desktop crash. Treat a
            // failed connection exactly like a missing endpoint on macOS.
            launch_desktop()?;
            let refreshed_endpoint = wait_for_endpoint(&paths)?;
            send_request(&refreshed_endpoint, command)
        }
        result => result,
    }
}

/// Contacts a companion-owned session socket when this CLI is running in a
/// remote shell. The socket lives in a private (0700) runtime directory, and
/// contains no bearer token; the managed companion process retains the token
/// passed by the desktop over its fixed SSH command.
#[cfg(unix)]
fn forward_through_companion_relay(
    command: &LocalCommand,
) -> Result<Option<LocalResponse>, CliError> {
    use std::os::unix::{fs::FileTypeExt, fs::PermissionsExt, net::UnixStream};

    let LocalCommand::OpenWorkspace {
        path,
        base,
        repository_bases,
    } = command
    else {
        return Ok(None);
    };
    let directory = managed_forward_relay_directory()?;
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CliError::Unavailable(format!(
                "could not inspect managed SSH forwarding sessions: {error}"
            )))
        }
    };
    let mut sockets = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).ok()?;
            if !metadata.file_type().is_socket()
                || metadata.permissions().mode() & 0o077 != 0
                || path.extension().and_then(|value| value.to_str()) != Some("sock")
            {
                return None;
            }
            let modified = metadata.modified().ok();
            Some((modified, path))
        })
        .collect::<Vec<_>>();
    sockets.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    sockets.truncate(MAX_MANAGED_FORWARD_RELAY_SOCKETS);
    if sockets.is_empty() {
        return Ok(None);
    }
    let request = ManagedForwardRelayRequest {
        version: localreview_protocol::REVERSE_FORWARD_VERSION,
        command: ManagedForwardRelayCommand::OpenRemoteWorkspace {
            path: path.clone(),
            base: base.clone(),
            repository_bases: repository_bases.clone(),
        },
    };
    request.validate()?;
    let mut active_sockets = Vec::new();
    for (_, socket) in sockets {
        let result = (|| -> Result<ManagedForwardResponse, CliError> {
            let mut stream = UnixStream::connect(&socket).map_err(|error| {
                CliError::Unavailable(format!(
                    "managed SSH forwarding session is unreachable: {error}"
                ))
            })?;
            stream.set_read_timeout(Some(MANAGED_FORWARD_RELAY_TIMEOUT))?;
            stream.set_write_timeout(Some(MANAGED_FORWARD_RELAY_TIMEOUT))?;
            let probe = ManagedForwardRelayRequest {
                version: localreview_protocol::REVERSE_FORWARD_VERSION,
                command: ManagedForwardRelayCommand::Probe,
            };
            write_frame(&mut stream, &probe)?;
            Ok(read_frame(&mut stream)?)
        })();
        match result {
            Ok(ManagedForwardResponse::Accepted) => active_sockets.push(socket),
            Ok(ManagedForwardResponse::Error { .. }) => {}
            Err(error) => {
                // A crashed companion can leave a socket entry behind. It is
                // safe to remove only an already verified private socket; the
                // token was never stored there.
                let _ = fs::remove_file(&socket);
                let _ = error;
            }
        }
    }
    let socket = match active_sockets.as_slice() {
        [] => {
            return Err(CliError::Unavailable(
                "managed SSH forwarding session is unavailable".into(),
            ))
        }
        [socket] => socket,
        _ => {
            return Err(CliError::Unavailable(
                "multiple managed LocalReview SSH sessions are active for this remote user; close the other review session or reconnect the intended desktop workspace before running `localreview open .`".into(),
            ))
        }
    };
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        CliError::Unavailable(format!(
            "managed SSH forwarding session is unreachable: {error}"
        ))
    })?;
    stream.set_read_timeout(Some(MANAGED_FORWARD_RELAY_TIMEOUT))?;
    stream.set_write_timeout(Some(MANAGED_FORWARD_RELAY_TIMEOUT))?;
    write_frame(&mut stream, &request)?;
    match read_frame(&mut stream)? {
        ManagedForwardResponse::Accepted => Ok(Some(LocalResponse::ForwardedRemoteWorkspace {
            request_id: next_request_id(),
            path: path.clone(),
        })),
        ManagedForwardResponse::Error { message } => Err(CliError::Unavailable(format!(
            "managed SSH forwarding request was rejected: {message}"
        ))),
    }
}

#[cfg(not(unix))]
fn forward_through_companion_relay(
    _command: &LocalCommand,
) -> Result<Option<LocalResponse>, CliError> {
    Ok(None)
}

fn forward_through_managed_ssh_session(
    command: &LocalCommand,
) -> Result<Option<LocalResponse>, CliError> {
    let endpoint = env::var("LOCALREVIEW_MANAGED_FORWARD_ENDPOINT").ok();
    let token = env::var("LOCALREVIEW_MANAGED_FORWARD_TOKEN").ok();
    let (endpoint, token) = match (endpoint, token) {
        (Some(endpoint), Some(token)) => (endpoint, token),
        (None, None) => return Ok(None),
        _ => {
            return Err(CliError::Unavailable(
                "the managed LocalReview SSH forwarding session is incomplete".into(),
            ))
        }
    };
    let LocalCommand::OpenWorkspace {
        path,
        base,
        repository_bases,
    } = command
    else {
        return Ok(None);
    };
    let address: SocketAddr = endpoint.parse().map_err(|_| {
        CliError::Usage("managed SSH forwarding endpoint must be a loopback socket address".into())
    })?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(CliError::Usage(
            "managed SSH forwarding endpoint must use loopback with a non-zero port".into(),
        ));
    }
    let request = ManagedForwardRequest {
        version: localreview_protocol::REVERSE_FORWARD_VERSION,
        token_hex: token,
        command: ManagedForwardCommand::OpenRemoteWorkspace {
            path: path.clone(),
            base: base.clone(),
            repository_bases: repository_bases.clone(),
        },
    };
    request.validate()?;
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&address, timeout).map_err(|error| {
        CliError::Unavailable(format!(
            "managed SSH forwarding session is unreachable: {error}"
        ))
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_frame(&mut stream, &request)?;
    let response: ManagedForwardResponse = read_frame(&mut stream)?;
    match response {
        ManagedForwardResponse::Accepted => Ok(Some(LocalResponse::ForwardedRemoteWorkspace {
            request_id: next_request_id(),
            path: path.clone(),
        })),
        ManagedForwardResponse::Error { message } => Err(CliError::Unavailable(format!(
            "managed SSH forwarding request was rejected: {message}"
        ))),
    }
}

fn load_endpoint(paths: &AppPaths) -> Result<Endpoint, CliError> {
    let record = paths.read_runtime_record()?;
    let secret = paths.load_secret()?;
    Ok(Endpoint { record, secret })
}

fn wait_for_endpoint(paths: &AppPaths) -> Result<Endpoint, CliError> {
    let deadline = std::time::Instant::now() + APP_READINESS_WAIT;
    loop {
        if let Ok(endpoint) = load_endpoint(paths) {
            if endpoint.record.socket_path.exists() {
                return Ok(endpoint);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(CliError::Unavailable(
                "LocalReview did not become ready within 8 seconds".into(),
            ));
        }
        thread::sleep(APP_READINESS_POLL);
    }
}

fn launch_desktop() -> Result<(), CliError> {
    #[cfg(target_os = "macos")]
    {
        // This calls Launch Services directly with typed arguments. No shell
        // command string is constructed from user input.
        let status = ProcessCommand::new("open")
            .args(["-a", "LocalReview"])
            .status()
            .map_err(|error| {
                CliError::Unavailable(format!("could not launch LocalReview: {error}"))
            })?;
        if status.success() {
            Ok(())
        } else {
            Err(CliError::Unavailable(
                "macOS could not launch the LocalReview application".into(),
            ))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(CliError::Unavailable(
            "desktop launching is only supported by the macOS CLI".into(),
        ))
    }
}

#[cfg(unix)]
fn send_request(endpoint: &Endpoint, command: LocalCommand) -> Result<LocalResponse, CliError> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(&endpoint.record.socket_path).map_err(|error| {
        CliError::Unavailable(format!(
            "LocalReview is not reachable at {}: {error}",
            endpoint.record.socket_path.display()
        ))
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let mut request = LocalRequest {
        version: PROTOCOL_VERSION,
        request_id: next_request_id(),
        issued_at_unix_secs: unix_seconds(),
        command,
        authentication: AuthProof {
            mac_hex: String::new(),
        },
    };
    request.authentication = endpoint.secret.sign(&request)?;
    write_frame(&mut stream, &request)?;
    let response: LocalResponse = read_frame(&mut stream)?;
    match &response {
        LocalResponse::Error { code, message, .. } => Err(CliError::Remote {
            code: *code,
            message: message.clone(),
        }),
        _ => Ok(response),
    }
}

#[cfg(not(unix))]
fn send_request(_endpoint: &Endpoint, _command: LocalCommand) -> Result<LocalResponse, CliError> {
    Err(CliError::Unavailable(
        "LocalReview forwarding requires a Unix-domain socket on this platform".into(),
    ))
}

fn run_doctor() -> Result<LocalResponse, CliError> {
    let paths = AppPaths::discover()?;
    let endpoint = match load_endpoint(&paths) {
        Ok(endpoint) if endpoint.record.socket_path.exists() => endpoint,
        Ok(_) => {
            return Ok(LocalResponse::Doctor {
                request_id: "doctor".into(),
                report: DoctorReport {
                    desktop_reachable: false,
                    protocol_version: PROTOCOL_VERSION,
                    message: "runtime record exists but the desktop socket is unavailable".into(),
                },
            })
        }
        Err(error) => {
            return Ok(LocalResponse::Doctor {
                request_id: "doctor".into(),
                report: DoctorReport {
                    desktop_reachable: false,
                    protocol_version: PROTOCOL_VERSION,
                    message: format!("desktop endpoint is unavailable: {error}"),
                },
            })
        }
    };
    send_request(&endpoint, LocalCommand::Doctor)
}

fn run_agent_stdio() -> Result<(), CliError> {
    if io::stdin().is_terminal() || io::stdout().is_terminal() {
        return Err(CliError::Usage(
            "agent --stdio requires framed protocol input and output, not an interactive terminal"
                .into(),
        ));
    }
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let writer = Arc::new(Mutex::new(io::stdout()));
    let server = Arc::new(AgentServer::default());
    let active = Arc::new(Mutex::new(HashMap::<String, ActiveAgentRequest>::new()));
    loop {
        let incoming: AgentMessage = match read_frame(&mut reader) {
            Ok(message) => message,
            Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        };
        let AgentMessage::Request(request) = incoming else {
            write_agent_message(
                &writer,
                AgentMessage::Response(AgentResponse {
                    id: "invalid-message".into(),
                    generation: 0,
                    result: AgentResult::Error {
                        error: AgentError::request(
                            AgentErrorCode::InvalidRequest,
                            "the companion accepts only request messages from the desktop",
                            false,
                        ),
                    },
                }),
            )?;
            continue;
        };
        if let Err(error) = request.validate() {
            write_agent_message(
                &writer,
                AgentMessage::Response(AgentResponse {
                    id: request.id,
                    generation: request.generation,
                    result: AgentResult::Error {
                        error: AgentError::request(
                            AgentErrorCode::InvalidRequest,
                            error.to_string(),
                            false,
                        ),
                    },
                }),
            )?;
            continue;
        }

        if let AgentOperation::Cancel { request_id } = &request.operation {
            let target = active
                .lock()
                .expect("agent cancellation registry is not poisoned")
                .get(request_id)
                .cloned();
            if let Some(target) = target.filter(|target| target.generation == request.generation) {
                target.cancelled.store(true, Ordering::Release);
                write_agent_message(
                    &writer,
                    AgentMessage::Response(AgentResponse {
                        id: request.id,
                        generation: request.generation,
                        result: AgentResult::CancelAccepted {
                            request_id: request_id.clone(),
                        },
                    }),
                )?;
            } else {
                write_agent_message(
                    &writer,
                    AgentMessage::Response(AgentResponse {
                        id: request.id,
                        generation: request.generation,
                        result: AgentResult::Error {
                            error: AgentError::request(
                                AgentErrorCode::StaleGeneration,
                                "the requested job is absent or belongs to another generation",
                                false,
                            ),
                        },
                    }),
                )?;
            }
            continue;
        }

        let cancellation = Arc::new(AtomicBool::new(false));
        {
            let mut active = active
                .lock()
                .expect("agent cancellation registry is not poisoned");
            if active.contains_key(&request.id) {
                drop(active);
                write_agent_message(
                    &writer,
                    AgentMessage::Response(AgentResponse {
                        id: request.id,
                        generation: request.generation,
                        result: AgentResult::Error {
                            error: AgentError::request(
                                AgentErrorCode::InvalidRequest,
                                "an active companion request already uses this identifier",
                                false,
                            ),
                        },
                    }),
                )?;
                continue;
            }
            active.insert(
                request.id.clone(),
                ActiveAgentRequest {
                    generation: request.generation,
                    cancelled: cancellation.clone(),
                },
            );
        }
        let output = Arc::clone(&writer);
        let state = Arc::clone(&server);
        let running = Arc::clone(&active);
        thread::spawn(move || {
            let id = request.id.clone();
            let generation = request.generation;
            let _ = emit_agent_progress(
                &output,
                &id,
                generation,
                AgentProgressPhase::Validating,
                0,
                None,
            );
            let result = state.handle(&request, &cancellation, &output);
            let response = AgentResponse {
                id: id.clone(),
                generation,
                result,
            };
            let _ = write_agent_message(&output, AgentMessage::Response(response));
            running
                .lock()
                .expect("agent cancellation registry is not poisoned")
                .remove(&id);
        });
    }
}

struct ManagedForwardRelay {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    socket_path: PathBuf,
}

impl std::fmt::Debug for ManagedForwardRelay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedForwardRelay")
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

impl ManagedForwardRelay {
    #[cfg(unix)]
    fn start(endpoint: String, token: String, session: String) -> Result<Self, CliError> {
        use std::os::unix::{fs::FileTypeExt, fs::PermissionsExt, net::UnixListener};

        validate_managed_forward_credentials(&endpoint, &token, &session)?;
        let directory = managed_forward_relay_directory()?;
        let socket_path = directory.join(format!("{session}.sock"));
        if socket_path.exists() {
            let metadata = fs::symlink_metadata(&socket_path).map_err(CliError::Io)?;
            if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(CliError::Unavailable(
                    "managed SSH forwarding socket path is not a private socket".into(),
                ));
            }
            // The session identifier is cryptographically random and unique
            // per desktop transport. A stale matching entry can only be from
            // a crashed predecessor, not a shell-supplied target.
            fs::remove_file(&socket_path).map_err(CliError::Io)?;
        }
        let listener = UnixListener::bind(&socket_path).map_err(CliError::Io)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .map_err(CliError::Io)?;
        listener.set_nonblocking(true).map_err(CliError::Io)?;
        let stop = Arc::new(AtomicBool::new(false));
        let callback_stop = Arc::clone(&stop);
        let callback_socket = socket_path.clone();
        let worker = thread::Builder::new()
            .name("localreview-managed-forward-relay".into())
            .spawn(move || {
                while !callback_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let response = managed_relay_response(&mut stream, &endpoint, &token);
                            let _ = write_frame(&mut stream, &response);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(MANAGED_FORWARD_RELAY_POLL);
                        }
                        Err(_) => break,
                    }
                }
                let _ = fs::remove_file(callback_socket);
            })
            .map_err(CliError::Io)?;
        Ok(Self {
            stop,
            worker: Some(worker),
            socket_path,
        })
    }

    #[cfg(not(unix))]
    fn start(_endpoint: String, _token: String, _session: String) -> Result<Self, CliError> {
        Err(CliError::Unavailable(
            "managed SSH forwarding requires a Unix companion host".into(),
        ))
    }
}

impl Drop for ManagedForwardRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
fn managed_relay_response(
    stream: &mut std::os::unix::net::UnixStream,
    endpoint: &str,
    token: &str,
) -> ManagedForwardResponse {
    let result = (|| -> Result<(), CliError> {
        stream.set_read_timeout(Some(MANAGED_FORWARD_RELAY_SERVER_TIMEOUT))?;
        stream.set_write_timeout(Some(MANAGED_FORWARD_RELAY_SERVER_TIMEOUT))?;
        let request: ManagedForwardRelayRequest = read_frame(stream)?;
        request.validate()?;
        match request.command {
            ManagedForwardRelayCommand::Probe => Ok(()),
            ManagedForwardRelayCommand::OpenRemoteWorkspace {
                path,
                base,
                repository_bases,
            } => forward_managed_request(endpoint, token, path, base, repository_bases),
        }
    })();
    match result {
        Ok(()) => ManagedForwardResponse::Accepted,
        Err(error) => ManagedForwardResponse::Error {
            // Errors are intentionally generic: the remote user gets a clear
            // action outcome without any token, endpoint, or desktop state.
            message: format!("managed workspace open failed: {error}"),
        },
    }
}

#[cfg(unix)]
fn managed_forward_relay_directory() -> Result<PathBuf, CliError> {
    use std::os::unix::fs::PermissionsExt;

    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })
        .ok_or_else(|| {
            CliError::Unavailable(
                "managed SSH forwarding requires XDG_RUNTIME_DIR or an absolute HOME".into(),
            )
        })?;
    // Keep the Unix-domain socket path short enough for macOS's small
    // `sun_path` limit even when XDG_RUNTIME_DIR itself lives below a long
    // per-user temporary root. The directory is still private (0700).
    let directory = base.join("lr");
    fs::create_dir_all(&directory).map_err(CliError::Io)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).map_err(CliError::Io)?;
    let metadata = fs::symlink_metadata(&directory).map_err(CliError::Io)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(CliError::Unavailable(
            "managed SSH forwarding directory is not a private directory".into(),
        ));
    }
    Ok(directory)
}

#[cfg(unix)]
fn validate_managed_forward_credentials(
    endpoint: &str,
    token: &str,
    session: &str,
) -> Result<(), CliError> {
    let address: SocketAddr = endpoint.parse().map_err(|_| {
        CliError::Usage("managed SSH forwarding endpoint must be a loopback socket address".into())
    })?;
    if !address.ip().is_loopback()
        || address.port() == 0
        || token.len() != 64
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        || session.is_empty()
        || session.len() > 64
        || !session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CliError::Usage(
            "managed SSH forwarding credentials are invalid".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn forward_managed_request(
    endpoint: &str,
    token: &str,
    path: String,
    base: Option<String>,
    repository_bases: Vec<RepositoryBaseOverride>,
) -> Result<(), CliError> {
    let address: SocketAddr = endpoint.parse().map_err(|_| {
        CliError::Usage("managed SSH forwarding endpoint must be a loopback socket address".into())
    })?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(CliError::Usage(
            "managed SSH forwarding endpoint must use loopback with a non-zero port".into(),
        ));
    }
    let request = ManagedForwardRequest {
        version: localreview_protocol::REVERSE_FORWARD_VERSION,
        token_hex: token.into(),
        command: ManagedForwardCommand::OpenRemoteWorkspace {
            path,
            base,
            repository_bases,
        },
    };
    request.validate()?;
    let mut stream =
        TcpStream::connect_timeout(&address, MANAGED_FORWARD_RELAY_TIMEOUT).map_err(|error| {
            CliError::Unavailable(format!(
                "managed SSH forwarding session is unreachable: {error}"
            ))
        })?;
    stream.set_read_timeout(Some(MANAGED_FORWARD_RELAY_TIMEOUT))?;
    stream.set_write_timeout(Some(MANAGED_FORWARD_RELAY_TIMEOUT))?;
    write_frame(&mut stream, &request)?;
    match read_frame(&mut stream)? {
        ManagedForwardResponse::Accepted => Ok(()),
        ManagedForwardResponse::Error { message } => Err(CliError::Unavailable(format!(
            "managed SSH forwarding request was rejected: {message}"
        ))),
    }
}

#[derive(Clone, Debug)]
struct ActiveAgentRequest {
    generation: u64,
    cancelled: Arc<AtomicBool>,
}

/// Remote captures retain only source identities and changed-path allowlists.
/// Source bodies and patches are never materialized eagerly: immutable Git
/// objects and stale-checked worktree files are streamed into bounded windows.
#[derive(Debug)]
struct AgentServer {
    snapshots: Mutex<CaptureSnapshots>,
    capture_sequence: AtomicU64,
    /// Kept solely in the managed companion process. Its token arrives over
    /// framed SSH stdio after handshake and is never reflected into argv,
    /// environment, a remote shell, or a durable file.
    managed_forward_relay: Mutex<Option<ManagedForwardRelay>>,
}

impl Default for AgentServer {
    fn default() -> Self {
        Self {
            snapshots: Mutex::new(CaptureSnapshots::default()),
            capture_sequence: AtomicU64::new(1),
            managed_forward_relay: Mutex::new(None),
        }
    }
}

const MAX_CAPTURE_SNAPSHOTS: usize = 4;

#[derive(Debug, Default)]
struct CaptureSnapshots {
    entries: HashMap<String, CapturedSourceSnapshot>,
    order: VecDeque<String>,
}

#[derive(Clone, Debug)]
struct CapturedSourceSnapshot {
    repository: RemoteRepositoryRef,
    generation: u64,
    head_sha: String,
    merge_base_sha: String,
    /// Only paths in the comparison may be requested. This prevents the
    /// narrow review protocol from becoming an arbitrary remote file reader.
    allowed_paths: HashSet<String>,
    /// Final worktree identities are captured without retaining their bytes.
    /// A later mismatch produces `stale_capture` instead of silently mixing
    /// generations.
    worktree_sources: HashMap<String, WorktreeSourceIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorktreeSourceKind {
    Regular,
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorktreeSourceIdentity {
    kind: WorktreeSourceKind,
    byte_len: u64,
    sha256_hex: String,
}

impl CaptureSnapshots {
    fn insert(&mut self, capture_id: String, snapshot: CapturedSourceSnapshot) {
        while self.entries.len() >= MAX_CAPTURE_SNAPSHOTS {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.order.push_back(capture_id.clone());
        self.entries.insert(capture_id, snapshot);
    }

    fn get(&self, capture_id: &str) -> Option<&CapturedSourceSnapshot> {
        self.entries.get(capture_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceWindowError {
    Cancelled,
    TooLarge,
    Binary,
    Io,
}

#[derive(Debug)]
struct StreamedSourceWindow {
    total_lines: u32,
    byte_len: u64,
    content_sha256_hex: String,
    bytes: Vec<u8>,
    end_of_file: bool,
}

/// Scans a source stream with bounded working memory. Bytes outside the
/// requested line range are hashed and counted but never retained.
fn stream_source_window(
    reader: &mut impl BufRead,
    start_line: u32,
    line_count: u32,
    cancellation: &AtomicBool,
) -> Result<StreamedSourceWindow, SourceWindowError> {
    let first = u64::from(start_line.saturating_sub(1));
    let last = first.saturating_add(u64::from(line_count));
    let mut current_line = 0_u64;
    let mut current_has_bytes = false;
    let mut total_bytes = 0_u64;
    let mut emitted_bytes = 0_usize;
    let mut selected = Vec::new();
    let mut content_hash = Sha256::new();
    loop {
        if cancellation.load(Ordering::Acquire) {
            return Err(SourceWindowError::Cancelled);
        }
        let buffer = reader.fill_buf().map_err(|_| SourceWindowError::Io)?;
        if buffer.is_empty() {
            break;
        }
        content_hash.update(buffer);
        total_bytes = total_bytes.saturating_add(buffer.len() as u64);
        for byte in buffer {
            current_has_bytes = true;
            if *byte == 0 {
                return Err(SourceWindowError::Binary);
            }
            if (first..last).contains(&current_line) {
                emitted_bytes = emitted_bytes.saturating_add(1);
                if emitted_bytes > MAX_REMOTE_SOURCE_WINDOW_BYTES {
                    return Err(SourceWindowError::TooLarge);
                }
                selected.push(*byte);
            }
            if *byte == b'\n' {
                current_line = current_line.saturating_add(1);
                current_has_bytes = false;
            }
        }
        let consumed = buffer.len();
        reader.consume(consumed);
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(SourceWindowError::Cancelled);
    }
    let total_lines = u32::try_from(current_line.saturating_add(u64::from(current_has_bytes)))
        .unwrap_or(u32::MAX);
    // Reject invalid text only after reading all bytes so the reported hash and
    // byte length never depend on lossy or newline-normalizing decoding.
    std::str::from_utf8(&selected).map_err(|_| SourceWindowError::Binary)?;
    let requested_last = u64::from(start_line)
        .saturating_add(u64::from(line_count))
        .saturating_sub(1);
    Ok(StreamedSourceWindow {
        total_lines,
        byte_len: total_bytes,
        content_sha256_hex: hex::encode(content_hash.finalize()),
        bytes: selected,
        end_of_file: requested_last >= u64::from(total_lines),
    })
}

impl AgentServer {
    fn handle(
        &self,
        request: &AgentRequest,
        cancellation: &AtomicBool,
        output: &Arc<Mutex<io::Stdout>>,
    ) -> AgentResult {
        if cancellation.load(Ordering::Acquire) {
            return AgentResult::Cancelled;
        }
        match &request.operation {
            AgentOperation::Handshake { desktop_versions } => {
                if desktop_versions.contains(&PROTOCOL_VERSION) {
                    AgentResult::Handshake {
                        selected_version: PROTOCOL_VERSION,
                        hello: AgentHello::current(
                            env!("CARGO_PKG_VERSION"),
                            env::consts::OS,
                            env::consts::ARCH,
                        ),
                    }
                } else {
                    AgentResult::Error {
                        error: AgentError::request(
                            AgentErrorCode::UnsupportedVersion,
                            format!(
                                "no compatible protocol version; companion supports {PROTOCOL_VERSION}"
                            ),
                            false,
                        ),
                    }
                }
            }
            AgentOperation::Ping => AgentResult::Pong,
            AgentOperation::ConfigureManagedForwardRelay {
                endpoint,
                token_hex,
                session_id,
            } => self.configure_managed_forward_relay(
                endpoint.clone(),
                token_hex.clone(),
                session_id.clone(),
            ),
            AgentOperation::DiscoverRepositories { root, max_depth } => {
                self.discover(request, root, *max_depth, cancellation, output)
            }
            AgentOperation::CaptureComparison {
                repository,
                base,
                options,
            } => self.capture(request, repository, base, options, cancellation, output),
            AgentOperation::ReadSourceWindow {
                capture_id,
                capture_generation,
                repository,
                path,
                revision,
                start_line,
                line_count,
            } => self.read_source_window(
                request,
                capture_id,
                *capture_generation,
                repository,
                path,
                *revision,
                *start_line,
                *line_count,
                cancellation,
                output,
            ),
            AgentOperation::WatchRepositoryChanges {
                repository,
                poll_interval_millis,
            } => self.watch_changes(
                request,
                repository.clone(),
                *poll_interval_millis,
                cancellation,
                output,
            ),
            AgentOperation::WatchWorkspaceChanges {
                repositories,
                poll_interval_millis,
            } => self.watch_workspace_changes(
                request,
                repositories.clone(),
                *poll_interval_millis,
                cancellation,
                output,
            ),
            AgentOperation::Cancel { .. } => AgentResult::Error {
                error: AgentError::request(
                    AgentErrorCode::InvalidRequest,
                    "cancellation is handled by the companion transport",
                    false,
                ),
            },
        }
    }

    fn configure_managed_forward_relay(
        &self,
        endpoint: String,
        token_hex: String,
        session_id: String,
    ) -> AgentResult {
        let relay = match ManagedForwardRelay::start(endpoint, token_hex, session_id) {
            Ok(relay) => relay,
            Err(_) => {
                return AgentResult::Error {
                    error: AgentError::request(
                        AgentErrorCode::Unavailable,
                        "could not initialize the managed reverse-forward relay",
                        true,
                    ),
                }
            }
        };
        let mut active = match self.managed_forward_relay.lock() {
            Ok(active) => active,
            Err(_) => {
                return AgentResult::Error {
                    error: AgentError::request(
                        AgentErrorCode::Internal,
                        "managed reverse-forward relay state is unavailable",
                        true,
                    ),
                }
            }
        };
        // Dropping a prior session relay stops/unlinks it before this one is
        // made discoverable, avoiding a stale same-user forwarding target.
        *active = Some(relay);
        AgentResult::ManagedForwardRelayConfigured
    }

    fn discover(
        &self,
        request: &AgentRequest,
        root: &str,
        max_depth: u16,
        cancellation: &AtomicBool,
        output: &Arc<Mutex<io::Stdout>>,
    ) -> AgentResult {
        let canonical_root = match canonical_remote_root(root) {
            Ok(root) => root,
            Err(error) => {
                return scoped_error(
                    AgentErrorCode::NotFound,
                    AgentErrorScope::WorkspaceRoot(root.into()),
                    error,
                    false,
                )
            }
        };
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::Discovering,
            0,
            None,
        );
        let config = DiscoveryConfig {
            max_depth: usize::from(max_depth),
            ..DiscoveryConfig::default()
        };
        let repositories = match discover_repositories(&canonical_root, &config) {
            Ok(repositories) => repositories,
            Err(error) => {
                return scoped_error(
                    AgentErrorCode::GitFailed,
                    AgentErrorScope::WorkspaceRoot(canonical_root.display().to_string()),
                    error.to_string(),
                    true,
                )
            }
        };
        let total = repositories.len() as u64;
        let mut converted = Vec::with_capacity(repositories.len());
        for (index, repository) in repositories.into_iter().enumerate() {
            if cancellation.load(Ordering::Acquire) {
                return AgentResult::Cancelled;
            }
            let relative_path = relative_wire_path(&repository.relative_path);
            let reference = RemoteRepositoryRef {
                workspace_root: canonical_root.display().to_string(),
                relative_path,
            };
            converted.push(RemoteRepository {
                reference,
                canonical_worktree: repository.identity.worktree.display().to_string(),
                git_common_dir: repository
                    .identity
                    .common_dir
                    .map(|value| value.display().to_string()),
                primary_remote: repository.identity.primary_remote,
                head: remote_head(repository.identity.head),
            });
            let _ = emit_agent_progress(
                output,
                &request.id,
                request.generation,
                AgentProgressPhase::Discovering,
                (index + 1) as u64,
                Some(total),
            );
        }
        AgentResult::Repositories {
            repositories: converted,
        }
    }

    fn capture(
        &self,
        request: &AgentRequest,
        reference: &RemoteRepositoryRef,
        base: &str,
        options: &RemoteComparisonOptions,
        cancellation: &AtomicBool,
        output: &Arc<Mutex<io::Stdout>>,
    ) -> AgentResult {
        let root = match resolve_remote_repository(reference) {
            Ok(root) => root,
            Err(error) => {
                return scoped_error(
                    AgentErrorCode::PathDenied,
                    AgentErrorScope::Repository(reference.clone()),
                    error,
                    false,
                )
            }
        };
        if cancellation.load(Ordering::Acquire) {
            return AgentResult::Cancelled;
        }
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::ResolvingBase,
            0,
            None,
        );
        let repository = GitRepository::open(&root);
        let requested_base = match BaseReference::new(base) {
            Ok(base) => base,
            Err(error) => {
                return scoped_error(
                    AgentErrorCode::InvalidRequest,
                    AgentErrorScope::Repository(reference.clone()),
                    error.to_string(),
                    false,
                )
            }
        };
        let domain_options = comparison_options(options);
        let resolved = match repository.resolve_comparison(
            RepositoryId::new(),
            ComparisonId::new(),
            requested_base,
            domain_options,
        ) {
            Ok(resolved) => resolved,
            Err(_error) if cancellation.load(Ordering::Acquire) => {
                return AgentResult::Cancelled;
            }
            Err(error) => {
                return git_scoped_error(error, AgentErrorScope::Repository(reference.clone()))
            }
        };
        if cancellation.load(Ordering::Acquire) {
            return AgentResult::Cancelled;
        }
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::Capturing,
            0,
            None,
        );
        let manifest = match capture_remote_manifest(
            &root,
            &repository,
            &resolved.merge_base_sha,
            &resolved.head_sha,
            options,
            cancellation,
        ) {
            Ok(captured) => captured,
            Err(_error) if cancellation.load(Ordering::Acquire) => {
                return AgentResult::Cancelled;
            }
            Err(error) => {
                return git_scoped_error(error, AgentErrorScope::Repository(reference.clone()))
            }
        };
        if cancellation.load(Ordering::Acquire) {
            return AgentResult::Cancelled;
        }
        let file_count = manifest.files.len();
        if file_count > MAX_REMOTE_CAPTURE_FILES {
            return scoped_error(
                AgentErrorCode::TooLarge,
                AgentErrorScope::Repository(reference.clone()),
                format!(
                    "remote comparison has {file_count} changed files; the companion limit is {MAX_REMOTE_CAPTURE_FILES}"
                ),
                false,
            );
        }

        let sequence = self.capture_sequence.fetch_add(1, Ordering::Relaxed);
        let capture_id = opaque_capture_id(request.generation, sequence, reference);
        self.snapshots
            .lock()
            .expect("agent capture cache is not poisoned")
            .insert(
                capture_id.clone(),
                CapturedSourceSnapshot {
                    repository: reference.clone(),
                    generation: request.generation,
                    head_sha: resolved.head_sha.as_str().to_owned(),
                    merge_base_sha: resolved.merge_base_sha.as_str().to_owned(),
                    allowed_paths: manifest.allowed_paths,
                    worktree_sources: manifest.worktree_sources,
                },
            );
        let capture = RemoteComparisonCapture {
            capture_id,
            generation: request.generation,
            repository: reference.clone(),
            requested_base: resolved.requested_base.as_str().to_owned(),
            base_tip_sha: resolved.base_tip_sha.as_str().to_owned(),
            merge_base_sha: resolved.merge_base_sha.as_str().to_owned(),
            head_sha: Some(resolved.head_sha.as_str().to_owned()),
            head: remote_head(resolved.head),
            committed: RemoteLayerSummary {
                changed_files: manifest.committed_count,
            },
            staged: RemoteLayerSummary {
                changed_files: manifest.staged_count,
            },
            unstaged: RemoteLayerSummary {
                changed_files: manifest.unstaged_count,
            },
            untracked: RemoteLayerSummary {
                changed_files: manifest.untracked_count,
            },
            files: manifest.files,
        };
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::Complete,
            1,
            Some(1),
        );
        AgentResult::ComparisonCapture { capture }
    }

    #[allow(clippy::too_many_arguments)]
    fn read_source_window(
        &self,
        request: &AgentRequest,
        capture_id: &str,
        capture_generation: u64,
        reference: &RemoteRepositoryRef,
        path: &str,
        revision: RemoteSourceRevision,
        start_line: u32,
        line_count: u32,
        cancellation: &AtomicBool,
        output: &Arc<Mutex<io::Stdout>>,
    ) -> AgentResult {
        let snapshot = self
            .snapshots
            .lock()
            .expect("agent capture cache is not poisoned")
            .get(capture_id)
            .cloned();
        let Some(snapshot) = snapshot else {
            return scoped_error(
                AgentErrorCode::StaleCapture,
                AgentErrorScope::Repository(reference.clone()),
                "the requested capture is no longer retained; refresh the remote comparison before reading source",
                true,
            );
        };
        if snapshot.repository != *reference {
            return scoped_error(
                AgentErrorCode::StaleCapture,
                AgentErrorScope::Repository(reference.clone()),
                "the source request does not belong to the requested immutable capture",
                false,
            );
        }
        if snapshot.generation != capture_generation || request.generation != capture_generation {
            return scoped_error(
                AgentErrorCode::StaleGeneration,
                AgentErrorScope::Repository(reference.clone()),
                "the source request generation does not own this capture",
                true,
            );
        }
        if !snapshot.allowed_paths.contains(path) {
            return scoped_error(
                AgentErrorCode::PathDenied,
                AgentErrorScope::SourcePath {
                    repository: reference.clone(),
                    path: path.into(),
                },
                "the requested path is not part of this comparison capture",
                false,
            );
        }
        let root = match resolve_remote_repository(reference) {
            Ok(root) => root,
            Err(error) => {
                return scoped_error(
                    AgentErrorCode::PathDenied,
                    AgentErrorScope::Repository(reference.clone()),
                    error,
                    false,
                )
            }
        };
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::ReadingSource,
            0,
            Some(1),
        );
        let window = match revision {
            RemoteSourceRevision::Worktree => {
                let Some(identity) = snapshot.worktree_sources.get(path) else {
                    return scoped_error(
                        AgentErrorCode::NotFound,
                        AgentErrorScope::SourcePath {
                            repository: reference.clone(),
                            path: path.into(),
                        },
                        "the requested path has no target source in this capture",
                        false,
                    );
                };
                read_worktree_source_window(
                    &root,
                    path,
                    identity,
                    start_line,
                    line_count,
                    cancellation,
                )
            }
            RemoteSourceRevision::Head | RemoteSourceRevision::MergeBase => {
                let sha = match revision {
                    RemoteSourceRevision::Head => &snapshot.head_sha,
                    RemoteSourceRevision::MergeBase => &snapshot.merge_base_sha,
                    RemoteSourceRevision::Worktree => unreachable!(),
                };
                read_git_source_window(&root, sha, path, start_line, line_count, cancellation)
            }
        };
        let window = match window {
            Ok(window) => window,
            Err(RemoteSourceReadError::Cancelled) => return AgentResult::Cancelled,
            Err(RemoteSourceReadError::Stale) => {
                return scoped_error(
                    AgentErrorCode::StaleCapture,
                    AgentErrorScope::SourcePath {
                        repository: reference.clone(),
                        path: path.into(),
                    },
                    "the remote file changed after capture; press Refresh before reading it",
                    true,
                )
            }
            Err(RemoteSourceReadError::NotFound) => {
                return scoped_error(
                    AgentErrorCode::NotFound,
                    AgentErrorScope::SourcePath {
                        repository: reference.clone(),
                        path: path.into(),
                    },
                    "the requested source path does not exist in this snapshot",
                    false,
                )
            }
            Err(RemoteSourceReadError::Binary) => {
                return scoped_error(
                    AgentErrorCode::BinaryContent,
                    AgentErrorScope::SourcePath {
                        repository: reference.clone(),
                        path: path.into(),
                    },
                    "the requested source path contains binary data",
                    false,
                )
            }
            Err(RemoteSourceReadError::TooLarge) => {
                return scoped_error(
                    AgentErrorCode::TooLarge,
                    AgentErrorScope::SourcePath {
                        repository: reference.clone(),
                        path: path.into(),
                    },
                    format!(
                        "remote source window exceeds {MAX_REMOTE_SOURCE_WINDOW_BYTES} bytes; request a narrower range"
                    ),
                    false,
                )
            }
            Err(RemoteSourceReadError::Git(error)) => {
                return git_scoped_error(
                    error,
                    AgentErrorScope::SourcePath {
                        repository: reference.clone(),
                        path: path.into(),
                    },
                )
            }
        };
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::Complete,
            1,
            Some(1),
        );
        AgentResult::SourceWindow {
            window: RemoteSourceWindow {
                capture_id: capture_id.into(),
                capture_generation,
                repository: reference.clone(),
                path: path.into(),
                revision,
                start_line,
                total_lines: window.total_lines,
                byte_len: window.byte_len,
                content_sha256_hex: window.content_sha256_hex,
                bytes: window.bytes,
                end_of_file: window.end_of_file,
            },
        }
    }

    fn watch_changes(
        &self,
        request: &AgentRequest,
        reference: RemoteRepositoryRef,
        poll_interval_millis: u32,
        cancellation: &AtomicBool,
        output: &Arc<Mutex<io::Stdout>>,
    ) -> AgentResult {
        let root = match resolve_remote_repository(&reference) {
            Ok(root) => root,
            Err(error) => {
                return scoped_error(
                    AgentErrorCode::PathDenied,
                    AgentErrorScope::Repository(reference),
                    error,
                    false,
                )
            }
        };
        let repository = GitRepository::open(&root);
        if let Err(error) = repository.inspect() {
            return git_scoped_error(error, AgentErrorScope::Repository(reference));
        }
        let mut last = match self.watched_fingerprint(&repository, &root) {
            Ok(fingerprint) => fingerprint,
            Err(error) => return git_scoped_error(error, AgentErrorScope::Repository(reference)),
        };
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::Watching,
            0,
            None,
        );
        // The request stays active until cancelled; notifications only signal
        // that the desktop should enable Refresh, never perform it implicitly.
        while !cancellation.load(Ordering::Acquire) {
            if sleep_until_cancelled(
                Duration::from_millis(u64::from(poll_interval_millis)),
                cancellation,
            ) {
                break;
            }
            match self.watched_fingerprint(&repository, &root) {
                Ok(current) => {
                    if current != last {
                        last = current;
                        let _ = write_agent_message(
                            output,
                            AgentMessage::Notification(
                                AgentNotification::FilesystemChangesAvailable {
                                    repository: reference.clone(),
                                    generation: request.generation,
                                },
                            ),
                        );
                    }
                }
                Err(error) => {
                    return git_scoped_error(error, AgentErrorScope::Repository(reference));
                }
            }
        }
        AgentResult::Cancelled
    }

    fn watch_workspace_changes(
        &self,
        request: &AgentRequest,
        references: Vec<RemoteRepositoryRef>,
        poll_interval_millis: u32,
        cancellation: &AtomicBool,
        output: &Arc<Mutex<io::Stdout>>,
    ) -> AgentResult {
        let mut watched = Vec::with_capacity(references.len());
        for reference in references {
            let root = match resolve_remote_repository(&reference) {
                Ok(root) => root,
                Err(error) => {
                    return scoped_error(
                        AgentErrorCode::PathDenied,
                        AgentErrorScope::Repository(reference),
                        error,
                        false,
                    )
                }
            };
            let repository = GitRepository::open(&root);
            if let Err(error) = repository.inspect() {
                return git_scoped_error(error, AgentErrorScope::Repository(reference));
            }
            watched.push((reference, repository, root));
        }
        let notification_repository = watched[0].0.clone();
        let mut last = match self.workspace_watched_fingerprint(&watched) {
            Ok(fingerprint) => fingerprint,
            Err((reference, error)) => {
                return git_scoped_error(error, AgentErrorScope::Repository(reference))
            }
        };
        let _ = emit_agent_progress(
            output,
            &request.id,
            request.generation,
            AgentProgressPhase::Watching,
            0,
            None,
        );
        while !cancellation.load(Ordering::Acquire) {
            if sleep_until_cancelled(
                Duration::from_millis(u64::from(poll_interval_millis)),
                cancellation,
            ) {
                break;
            }
            match self.workspace_watched_fingerprint(&watched) {
                Ok(current) => {
                    if current != last {
                        last = current;
                        let _ = write_agent_message(
                            output,
                            AgentMessage::Notification(
                                AgentNotification::FilesystemChangesAvailable {
                                    repository: notification_repository.clone(),
                                    generation: request.generation,
                                },
                            ),
                        );
                    }
                }
                Err((reference, error)) => {
                    return git_scoped_error(error, AgentErrorScope::Repository(reference));
                }
            }
        }
        AgentResult::Cancelled
    }

    fn workspace_watched_fingerprint(
        &self,
        watched: &[(RemoteRepositoryRef, GitRepository, PathBuf)],
    ) -> Result<String, (RemoteRepositoryRef, GitError)> {
        let mut hash = Sha256::new();
        hash.update(b"localreview-workspace-watch-v1\0");
        for (reference, repository, root) in watched {
            hash.update(reference.relative_path.as_bytes());
            hash.update([0]);
            let fingerprint = self
                .watched_fingerprint(repository, root)
                .map_err(|error| (reference.clone(), error))?;
            hash.update(fingerprint.as_bytes());
            hash.update([0]);
        }
        Ok(hex::encode(hash.finalize()))
    }

    fn watched_fingerprint(
        &self,
        repository: &GitRepository,
        root: &Path,
    ) -> Result<String, GitError> {
        // A watcher deliberately performs no capture, diff, or source read.
        // It hashes bounded porcelain metadata plus filesystem state for only
        // paths already reported by Git, which notices edits to an existing
        // modified/untracked file without moving its bytes across SSH.
        const MAX_WATCHED_PATHS: usize = 50_000;
        let mut statuses = repository.status()?;
        if statuses.len() > MAX_WATCHED_PATHS {
            return Err(GitError::Parse(format!(
                "remote watcher has more than {MAX_WATCHED_PATHS} changed paths"
            )));
        }
        statuses.sort_by(|left, right| left.path.cmp(&right.path));
        let mut hash = Sha256::new();
        hash.update(b"head\0");
        hash.update(watched_head_sha(root)?);
        hash.update([0]);
        append_metadata_fingerprint(&mut hash, root)?;
        for change in statuses {
            hash.update([change.index_status as u8, change.worktree_status as u8]);
            hash.update(change.path.as_str().as_bytes());
            hash.update([0]);
            if let Some(original) = change.original_path {
                hash.update(original.as_str().as_bytes());
            }
            hash.update([0]);
            append_metadata_fingerprint(&mut hash, &root.join(change.path.as_str()))?;
        }
        Ok(hex::encode(hash.finalize()))
    }
}

fn watched_head_sha(root: &Path) -> Result<Vec<u8>, GitError> {
    let output = ProcessCommand::new("git")
        .current_dir(root)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .map_err(|source| GitError::Spawn {
            command: "git rev-parse --verify HEAD".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(GitError::CommandFailed {
            command: "git rev-parse --verify HEAD".into(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    if output.stdout.len() > 128 {
        return Err(GitError::Parse("remote HEAD object was invalid".into()));
    }
    Ok(output.stdout)
}

fn sleep_until_cancelled(duration: Duration, cancellation: &AtomicBool) -> bool {
    let deadline = std::time::Instant::now() + duration;
    while !cancellation.load(Ordering::Acquire) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
    true
}

const MAX_GIT_METADATA_BYTES: usize = 16 * 1024 * 1024;
const SOURCE_CLASSIFICATION_BYTES: usize = 8 * 1024;

#[derive(Debug)]
struct CapturedRemoteManifest {
    files: Vec<RemoteCapturedFile>,
    allowed_paths: HashSet<String>,
    worktree_sources: HashMap<String, WorktreeSourceIdentity>,
    committed_count: u32,
    staged_count: u32,
    unstaged_count: u32,
    untracked_count: u32,
}

#[derive(Clone, Debug)]
struct RawRemoteEntry {
    path: String,
    old_path: Option<String>,
    status: RemoteFileStatus,
    similarity_percent: Option<u8>,
    old_mode: u32,
    new_mode: u32,
    old_object_id: Option<String>,
    new_object_id: Option<String>,
}

fn capture_remote_manifest(
    root: &Path,
    repository: &GitRepository,
    merge_base: &localreview_domain::GitSha,
    head: &localreview_domain::GitSha,
    options: &RemoteComparisonOptions,
    cancellation: &AtomicBool,
) -> Result<CapturedRemoteManifest, GitError> {
    let aggregate = remote_raw_diff(root, &[merge_base.as_str()], false, options, cancellation)?;
    let committed = remote_raw_diff(
        root,
        &[merge_base.as_str(), head.as_str()],
        false,
        options,
        cancellation,
    )?;
    let staged = remote_raw_diff(root, &[head.as_str()], true, options, cancellation)?;
    let unstaged = remote_raw_diff(root, &[], false, options, cancellation)?;
    let status = repository.status()?;
    if cancellation.load(Ordering::Acquire) {
        return Err(GitError::ConcurrentModification { attempts: 0 });
    }

    let committed_paths = raw_entry_paths(&committed);
    let staged_paths = raw_entry_paths(&staged);
    let unstaged_paths = raw_entry_paths(&unstaged);
    let mut allowed_paths = HashSet::new();
    let mut worktree_sources = HashMap::new();
    let mut files = Vec::with_capacity(aggregate.len().saturating_add(status.len()));

    for entry in aggregate {
        let mut layers = Vec::with_capacity(3);
        if raw_layer_contains(&committed_paths, &entry) {
            layers.push(RemoteChangeLayer::Committed);
        }
        if raw_layer_contains(&staged_paths, &entry) {
            layers.push(RemoteChangeLayer::Staged);
        }
        if raw_layer_contains(&unstaged_paths, &entry) {
            layers.push(RemoteChangeLayer::Unstaged);
        }
        let mut binary = false;
        let mut lfs_pointer = false;
        let mut captured_byte_len = None;
        if entry.status != RemoteFileStatus::Deleted && entry.status != RemoteFileStatus::Submodule
        {
            if let Some((identity, prefix)) = inspect_worktree_source(root, &entry.path)? {
                binary = prefix.contains(&0);
                lfs_pointer = is_lfs_pointer_prefix(&prefix, identity.byte_len);
                captured_byte_len = Some(identity.byte_len);
                worktree_sources.insert(entry.path.clone(), identity);
            }
        } else if entry.status == RemoteFileStatus::Deleted {
            if let Some((prefix, byte_len)) = read_git_object_prefix(
                root,
                merge_base.as_str(),
                entry.old_path.as_deref().unwrap_or(&entry.path),
                cancellation,
            )? {
                binary = prefix.contains(&0);
                lfs_pointer = is_lfs_pointer_prefix(&prefix, byte_len);
            }
        }
        allowed_paths.insert(entry.path.clone());
        if let Some(old_path) = &entry.old_path {
            allowed_paths.insert(old_path.clone());
        }
        files.push(RemoteCapturedFile {
            path: entry.path,
            old_path: entry.old_path,
            status: entry.status,
            similarity_percent: entry.similarity_percent,
            old_mode: entry.old_mode,
            new_mode: entry.new_mode,
            old_object_id: entry.old_object_id,
            new_object_id: entry.new_object_id,
            untracked: false,
            binary,
            lfs_pointer,
            captured_byte_len,
            layers,
        });
    }

    let mut untracked_count = 0_u32;
    for change in status {
        if change.kind != WorkingTreeChangeKind::Untracked {
            continue;
        }
        let path = change.path.as_str().to_owned();
        if !path_matches_filters(&path, &options.path_filters) {
            continue;
        }
        let Some((identity, prefix)) = inspect_worktree_source(root, &path)? else {
            continue;
        };
        untracked_count = untracked_count.saturating_add(1);
        allowed_paths.insert(path.clone());
        let binary = prefix.contains(&0);
        let lfs_pointer = is_lfs_pointer_prefix(&prefix, identity.byte_len);
        let byte_len = identity.byte_len;
        worktree_sources.insert(path.clone(), identity);
        files.push(RemoteCapturedFile {
            path,
            old_path: None,
            status: RemoteFileStatus::Untracked,
            similarity_percent: None,
            old_mode: 0,
            new_mode: 0,
            old_object_id: None,
            new_object_id: None,
            untracked: true,
            binary,
            lfs_pointer,
            captured_byte_len: Some(byte_len),
            layers: vec![RemoteChangeLayer::Untracked],
        });
    }
    if files.len() > MAX_REMOTE_CAPTURE_FILES {
        return Err(GitError::Parse(format!(
            "remote comparison exceeds the {MAX_REMOTE_CAPTURE_FILES}-file manifest limit"
        )));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(CapturedRemoteManifest {
        files,
        allowed_paths,
        worktree_sources,
        committed_count: u32::try_from(committed.len()).unwrap_or(u32::MAX),
        staged_count: u32::try_from(staged.len()).unwrap_or(u32::MAX),
        unstaged_count: u32::try_from(unstaged.len()).unwrap_or(u32::MAX),
        untracked_count,
    })
}

fn raw_entry_paths(entries: &[RawRemoteEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .flat_map(|entry| [Some(entry.path.clone()), entry.old_path.clone()])
        .flatten()
        .collect()
}

fn raw_layer_contains(paths: &BTreeSet<String>, entry: &RawRemoteEntry) -> bool {
    paths.contains(&entry.path)
        || entry
            .old_path
            .as_ref()
            .is_some_and(|path| paths.contains(path))
}

fn path_matches_filters(path: &str, filters: &[String]) -> bool {
    filters.is_empty()
        || filters
            .iter()
            .any(|filter| path == filter || path.starts_with(&format!("{filter}/")))
}

fn remote_raw_diff(
    root: &Path,
    revisions: &[&str],
    cached: bool,
    options: &RemoteComparisonOptions,
    cancellation: &AtomicBool,
) -> Result<Vec<RawRemoteEntry>, GitError> {
    let mut arguments = vec![
        "diff".into(),
        "--raw".into(),
        "-z".into(),
        "--abbrev=64".into(),
        "--find-renames".into(),
        "--find-copies".into(),
        "--find-copies-harder".into(),
        "--no-ext-diff".into(),
    ];
    if cached {
        arguments.push("--cached".into());
    }
    if options.ignore_all_whitespace {
        arguments.push("--ignore-all-space".into());
    }
    if options.ignore_space_at_eol {
        arguments.push("--ignore-space-at-eol".into());
    }
    if options.ignore_cr_at_eol {
        arguments.push("--ignore-cr-at-eol".into());
    }
    arguments.extend(revisions.iter().map(OsString::from));
    if !options.path_filters.is_empty() {
        arguments.push("--".into());
        arguments.extend(options.path_filters.iter().map(OsString::from));
    }
    let bytes = run_git_metadata(root, arguments, cancellation)?;
    parse_remote_raw_diff(&bytes)
}

fn parse_remote_raw_diff(bytes: &[u8]) -> Result<Vec<RawRemoteEntry>, GitError> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(header) = fields.next() {
        let header = std::str::from_utf8(header)
            .map_err(|_| GitError::Parse("remote raw diff header was not ASCII".into()))?;
        let mut parts = header.split_whitespace();
        let old_mode = parse_raw_mode(parts.next(), true)?;
        let new_mode = parse_raw_mode(parts.next(), false)?;
        let old_object_id = parse_raw_object(parts.next(), "old")?;
        let new_object_id = parse_raw_object(parts.next(), "new")?;
        let status_token = parts
            .next()
            .ok_or_else(|| GitError::Parse("remote raw diff status was missing".into()))?;
        if parts.next().is_some() {
            return Err(GitError::Parse(
                "remote raw diff header had extra fields".into(),
            ));
        }
        let status_code = status_token
            .bytes()
            .next()
            .ok_or_else(|| GitError::Parse("remote raw diff status was empty".into()))?;
        let similarity_percent = if matches!(status_code, b'R' | b'C') {
            Some(
                status_token[1..]
                    .parse::<u8>()
                    .ok()
                    .filter(|score| *score <= 100)
                    .ok_or_else(|| {
                        GitError::Parse("remote rename/copy similarity was invalid".into())
                    })?,
            )
        } else {
            None
        };
        let first_path = parse_raw_path(
            fields
                .next()
                .ok_or_else(|| GitError::Parse("remote raw diff path was missing".into()))?,
        )?;
        let (path, old_path) = if matches!(status_code, b'R' | b'C') {
            let target = parse_raw_path(fields.next().ok_or_else(|| {
                GitError::Parse("remote raw rename/copy target was missing".into())
            })?)?;
            (target, Some(first_path))
        } else {
            (first_path, None)
        };
        let status = if old_mode == 0o160000 || new_mode == 0o160000 {
            RemoteFileStatus::Submodule
        } else {
            match status_code {
                b'A' => RemoteFileStatus::Added,
                b'D' => RemoteFileStatus::Deleted,
                b'R' => RemoteFileStatus::Renamed,
                b'C' => RemoteFileStatus::Copied,
                b'T' => RemoteFileStatus::TypeChanged,
                b'M' if old_mode != new_mode => RemoteFileStatus::ModeChanged,
                b'M' => RemoteFileStatus::Modified,
                other => {
                    return Err(GitError::Parse(format!(
                        "unsupported remote raw diff status {:?}",
                        char::from(other)
                    )))
                }
            }
        };
        entries.push(RawRemoteEntry {
            path,
            old_path,
            status,
            similarity_percent,
            old_mode,
            new_mode,
            old_object_id,
            new_object_id,
        });
    }
    Ok(entries)
}

fn parse_raw_mode(value: Option<&str>, old: bool) -> Result<u32, GitError> {
    let value = value.ok_or_else(|| GitError::Parse("remote raw diff mode was missing".into()))?;
    let value = if old {
        value
            .strip_prefix(':')
            .ok_or_else(|| GitError::Parse("remote raw diff header lacked ':'".into()))?
    } else {
        value
    };
    u32::from_str_radix(value, 8)
        .map_err(|_| GitError::Parse("remote raw diff mode was invalid".into()))
}

fn parse_raw_object(value: Option<&str>, label: &str) -> Result<Option<String>, GitError> {
    let value =
        value.ok_or_else(|| GitError::Parse(format!("remote raw {label} object missing")))?;
    if !(4..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::Parse(format!(
            "remote raw {label} object was invalid"
        )));
    }
    Ok((!value.bytes().all(|byte| byte == b'0')).then(|| value.to_ascii_lowercase()))
}

fn parse_raw_path(value: &[u8]) -> Result<String, GitError> {
    let path = std::str::from_utf8(value)
        .map_err(|_| GitError::Parse("remote Git path was not UTF-8".into()))?
        .to_owned();
    localreview_protocol::validate_relative_path(&path)
        .map_err(|error| GitError::Parse(error.to_string()))?;
    Ok(path)
}

fn opaque_capture_id(generation: u64, sequence: u64, reference: &RemoteRepositoryRef) -> String {
    let mut hash = Sha256::new();
    hash.update(b"localreview-remote-capture-v4\0");
    hash.update(std::process::id().to_be_bytes());
    hash.update(generation.to_be_bytes());
    hash.update(sequence.to_be_bytes());
    hash.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    hash.update(reference.workspace_root.as_bytes());
    hash.update([0]);
    hash.update(reference.relative_path.as_bytes());
    format!("cap-{}", &hex::encode(hash.finalize())[..32])
}

fn run_git_metadata(
    root: &Path,
    arguments: Vec<OsString>,
    cancellation: &AtomicBool,
) -> Result<Vec<u8>, GitError> {
    let display = arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let command_display = format!("git -C {} {display}", root.display());
    let mut child = ProcessCommand::new("git")
        .current_dir(root)
        .args(&arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| GitError::Spawn {
            command: command_display.clone(),
            source,
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Parse("Git metadata command did not expose stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitError::Parse("Git metadata command did not expose stderr".into()))?;
    let stderr_join = thread::spawn(move || read_diagnostic(stderr));
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        if cancellation.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_join.join();
            return Err(GitError::ConcurrentModification { attempts: 0 });
        }
        let count = match stdout.read(&mut buffer) {
            Ok(count) => count,
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_join.join();
                return Err(GitError::Spawn {
                    command: command_display,
                    source,
                });
            }
        };
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > MAX_GIT_METADATA_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_join.join();
            return Err(GitError::Parse(format!(
                "Git metadata exceeded the {MAX_GIT_METADATA_BYTES}-byte bound"
            )));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let status = child.wait().map_err(|source| GitError::Spawn {
        command: command_display.clone(),
        source,
    })?;
    let stderr = stderr_join.join().unwrap_or_default();
    if !status.success() {
        return Err(GitError::CommandFailed {
            command: command_display,
            stderr,
        });
    }
    Ok(bytes)
}

fn read_diagnostic(mut reader: impl Read) -> String {
    const LIMIT: usize = 8 * 1024;
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 2 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let remaining = LIMIT.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..count.min(remaining)]);
            }
        }
    }
    String::from_utf8_lossy(&retained).trim().to_owned()
}

fn secure_worktree_path(root: &Path, path: &str) -> Result<PathBuf, GitError> {
    localreview_protocol::validate_relative_path(path)
        .map_err(|error| GitError::Parse(error.to_string()))?;
    let candidate = root.join(path);
    let parent = candidate.parent().unwrap_or(root);
    let canonical_parent = parent.canonicalize().map_err(|source| GitError::File {
        path: parent.to_path_buf(),
        source,
    })?;
    if !canonical_parent.starts_with(root) {
        return Err(GitError::UnsafeRepositoryPath {
            path: StoredPath::from(path),
        });
    }
    Ok(candidate)
}

fn inspect_worktree_source(
    root: &Path,
    path: &str,
) -> Result<Option<(WorktreeSourceIdentity, Vec<u8>)>, GitError> {
    let candidate = secure_worktree_path(root, path)?;
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(GitError::File {
                path: candidate,
                source,
            })
        }
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(&candidate).map_err(|source| GitError::File {
            path: candidate,
            source,
        })?;
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            target.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let bytes = target.to_string_lossy().as_bytes().to_vec();
        let identity = WorktreeSourceIdentity {
            kind: WorktreeSourceKind::Symlink,
            byte_len: bytes.len() as u64,
            sha256_hex: hex::encode(Sha256::digest(&bytes)),
        };
        return Ok(Some((identity, bytes)));
    }
    if !metadata.is_file() {
        return Err(GitError::UnsafeRepositoryPath {
            path: StoredPath::from(path),
        });
    }
    let mut file = fs::File::open(&candidate).map_err(|source| GitError::File {
        path: candidate,
        source,
    })?;
    let mut hash = Sha256::new();
    let mut prefix = Vec::with_capacity(SOURCE_CLASSIFICATION_BYTES);
    let mut total = 0_u64;
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|source| GitError::File {
            path: root.join(path),
            source,
        })?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        total = total.saturating_add(count as u64);
        let wanted = SOURCE_CLASSIFICATION_BYTES.saturating_sub(prefix.len());
        prefix.extend_from_slice(&buffer[..count.min(wanted)]);
    }
    Ok(Some((
        WorktreeSourceIdentity {
            kind: WorktreeSourceKind::Regular,
            byte_len: total,
            sha256_hex: hex::encode(hash.finalize()),
        },
        prefix,
    )))
}

fn is_lfs_pointer_prefix(prefix: &[u8], byte_len: u64) -> bool {
    if byte_len > 16 * 1024 {
        return false;
    }
    let Ok(text) = std::str::from_utf8(prefix) else {
        return false;
    };
    text.starts_with("version https://git-lfs.github.com/spec/v1\n")
        && text.lines().any(|line| line.starts_with("oid sha256:"))
        && text.lines().any(|line| line.starts_with("size "))
}

#[derive(Debug)]
enum RemoteSourceReadError {
    Cancelled,
    Stale,
    NotFound,
    Binary,
    TooLarge,
    Git(GitError),
}

impl From<SourceWindowError> for RemoteSourceReadError {
    fn from(error: SourceWindowError) -> Self {
        match error {
            SourceWindowError::Cancelled => Self::Cancelled,
            SourceWindowError::TooLarge => Self::TooLarge,
            SourceWindowError::Binary => Self::Binary,
            SourceWindowError::Io => Self::Git(GitError::Parse(
                "could not stream the requested source".into(),
            )),
        }
    }
}

fn read_worktree_source_window(
    root: &Path,
    path: &str,
    captured: &WorktreeSourceIdentity,
    start_line: u32,
    line_count: u32,
    cancellation: &AtomicBool,
) -> Result<StreamedSourceWindow, RemoteSourceReadError> {
    let window = match captured.kind {
        WorktreeSourceKind::Regular => {
            let candidate = secure_worktree_path(root, path).map_err(RemoteSourceReadError::Git)?;
            let metadata = fs::symlink_metadata(&candidate).map_err(|source| {
                RemoteSourceReadError::Git(GitError::File {
                    path: candidate.clone(),
                    source,
                })
            })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(RemoteSourceReadError::Stale);
            }
            let file = fs::File::open(&candidate).map_err(|source| {
                RemoteSourceReadError::Git(GitError::File {
                    path: candidate,
                    source,
                })
            })?;
            stream_source_window(
                &mut BufReader::new(file),
                start_line,
                line_count,
                cancellation,
            )?
        }
        WorktreeSourceKind::Symlink => {
            let candidate = secure_worktree_path(root, path).map_err(RemoteSourceReadError::Git)?;
            let metadata =
                fs::symlink_metadata(&candidate).map_err(|_| RemoteSourceReadError::Stale)?;
            if !metadata.file_type().is_symlink() {
                return Err(RemoteSourceReadError::Stale);
            }
            let target = fs::read_link(candidate).map_err(|_| RemoteSourceReadError::Stale)?;
            #[cfg(unix)]
            let bytes = {
                use std::os::unix::ffi::OsStrExt;
                target.as_os_str().as_bytes().to_vec()
            };
            #[cfg(not(unix))]
            let bytes = target.to_string_lossy().as_bytes().to_vec();
            stream_source_window(
                &mut BufReader::new(Cursor::new(bytes)),
                start_line,
                line_count,
                cancellation,
            )?
        }
    };
    if window.byte_len != captured.byte_len || window.content_sha256_hex != captured.sha256_hex {
        return Err(RemoteSourceReadError::Stale);
    }
    Ok(window)
}

fn git_object_spec(revision: &str, path: &str) -> Result<String, GitError> {
    localreview_domain::GitSha::new(revision)
        .map_err(|error| GitError::Parse(error.to_string()))?;
    localreview_protocol::validate_relative_path(path)
        .map_err(|error| GitError::Parse(error.to_string()))?;
    Ok(format!("{revision}:{path}"))
}

fn read_git_source_window(
    root: &Path,
    revision: &str,
    path: &str,
    start_line: u32,
    line_count: u32,
    cancellation: &AtomicBool,
) -> Result<StreamedSourceWindow, RemoteSourceReadError> {
    let object = git_object_spec(revision, path).map_err(RemoteSourceReadError::Git)?;
    let exists = ProcessCommand::new("git")
        .current_dir(root)
        .args(["cat-file", "-e", object.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| {
            RemoteSourceReadError::Git(GitError::Spawn {
                command: "git cat-file -e <captured-object>".into(),
                source,
            })
        })?;
    if !exists.success() {
        return Err(RemoteSourceReadError::NotFound);
    }
    let mut child = ProcessCommand::new("git")
        .current_dir(root)
        .args(["cat-file", "blob", object.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| {
            RemoteSourceReadError::Git(GitError::Spawn {
                command: "git cat-file blob <captured-object>".into(),
                source,
            })
        })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        RemoteSourceReadError::Git(GitError::Parse(
            "Git source command did not expose stdout".into(),
        ))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        RemoteSourceReadError::Git(GitError::Parse(
            "Git source command did not expose stderr".into(),
        ))
    })?;
    let stderr_join = thread::spawn(move || read_diagnostic(stderr));
    let streamed = stream_source_window(
        &mut BufReader::new(stdout),
        start_line,
        line_count,
        cancellation,
    );
    if streamed.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|source| {
        RemoteSourceReadError::Git(GitError::Spawn {
            command: "git cat-file blob <captured-object>".into(),
            source,
        })
    })?;
    let stderr = stderr_join.join().unwrap_or_default();
    let window = streamed?;
    if !status.success() {
        return Err(RemoteSourceReadError::Git(GitError::CommandFailed {
            command: "git cat-file blob <captured-object>".into(),
            stderr,
        }));
    }
    Ok(window)
}

fn read_git_object_prefix(
    root: &Path,
    revision: &str,
    path: &str,
    cancellation: &AtomicBool,
) -> Result<Option<(Vec<u8>, u64)>, GitError> {
    let object = git_object_spec(revision, path)?;
    let size = run_git_metadata(
        root,
        vec!["cat-file".into(), "-s".into(), object.clone().into()],
        cancellation,
    );
    let size = match size {
        Ok(size) => String::from_utf8(size)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .ok_or_else(|| GitError::Parse("Git object size was invalid".into()))?,
        Err(GitError::CommandFailed { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut child = ProcessCommand::new("git")
        .current_dir(root)
        .args(["cat-file", "blob", object.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| GitError::Spawn {
            command: "git cat-file blob <captured-object>".into(),
            source,
        })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| GitError::Parse("Git object prefix command did not expose stdout".into()))?;
    let mut prefix = vec![0_u8; SOURCE_CLASSIFICATION_BYTES.min(size as usize)];
    let mut count = 0_usize;
    while count < prefix.len() {
        if cancellation.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitError::ConcurrentModification { attempts: 0 });
        }
        let read = stdout
            .read(&mut prefix[count..])
            .map_err(|source| GitError::Spawn {
                command: "git cat-file blob <captured-object>".into(),
                source,
            })?;
        if read == 0 {
            break;
        }
        count += read;
    }
    prefix.truncate(count);
    let _ = child.kill();
    let _ = child.wait();
    Ok(Some((prefix, size)))
}

fn emit_agent_progress(
    output: &Arc<Mutex<io::Stdout>>,
    id: &str,
    generation: u64,
    phase: AgentProgressPhase,
    completed: u64,
    total: Option<u64>,
) -> Result<(), CliError> {
    write_agent_message(
        output,
        AgentMessage::Progress(AgentProgress {
            id: id.into(),
            generation,
            phase,
            completed,
            total,
        }),
    )
}

fn write_agent_message(
    output: &Arc<Mutex<io::Stdout>>,
    message: AgentMessage,
) -> Result<(), CliError> {
    let mut output = output
        .lock()
        .map_err(|_| CliError::Internal("agent stdout lock is poisoned".into()))?;
    write_frame(&mut *output, &message)?;
    Ok(())
}

fn canonical_remote_root(value: &str) -> Result<PathBuf, String> {
    let root = Path::new(value)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !root.is_dir() {
        return Err("the remote workspace root is not a directory".into());
    }
    Ok(root)
}

fn resolve_remote_repository(reference: &RemoteRepositoryRef) -> Result<PathBuf, String> {
    let root = canonical_remote_root(&reference.workspace_root)?;
    let candidate = if reference.relative_path == "." {
        root.clone()
    } else {
        root.join(&reference.relative_path)
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !canonical.starts_with(&root) || !canonical.is_dir() {
        return Err("the requested repository is outside its remote workspace root".into());
    }
    let identity = GitRepository::open(&canonical)
        .inspect()
        .map_err(|error| error.to_string())?;
    let worktree = identity
        .worktree
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !worktree.starts_with(root) {
        return Err("the resolved Git worktree is outside its remote workspace root".into());
    }
    Ok(worktree)
}

fn relative_wire_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn remote_head(head: localreview_domain::HeadState) -> RemoteHead {
    match head {
        localreview_domain::HeadState::Branch(branch) => RemoteHead::Branch(branch),
        localreview_domain::HeadState::Detached(sha) => RemoteHead::Detached(sha.as_str().into()),
        localreview_domain::HeadState::Unborn => RemoteHead::Unborn,
    }
}

fn comparison_options(options: &RemoteComparisonOptions) -> ComparisonOptions {
    ComparisonOptions {
        ignore_all_whitespace: options.ignore_all_whitespace,
        ignore_space_at_eol: options.ignore_space_at_eol,
        ignore_cr_at_eol: options.ignore_cr_at_eol,
        path_filters: options
            .path_filters
            .iter()
            .map(|path| StoredPath::from(path.as_str()))
            .collect(),
    }
}

/// Hashes only path and stat information for the notification watcher.  It
/// intentionally uses `symlink_metadata`, never follows a changed path, and
/// represents a missing path instead of treating a deletion as a watcher
/// error.  The resulting digest is not persisted as review data.
fn append_metadata_fingerprint(hash: &mut Sha256, path: &Path) -> Result<(), GitError> {
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update([0]);
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            hash.update([u8::from(metadata.file_type().is_file())]);
            hash.update([u8::from(metadata.file_type().is_dir())]);
            hash.update([u8::from(metadata.file_type().is_symlink())]);
            hash.update(metadata.len().to_be_bytes());
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok());
            hash.update(
                modified
                    .map_or(0_u64, |value| value.as_secs())
                    .to_be_bytes(),
            );
            hash.update(
                modified
                    .map_or(0_u32, |value| value.subsec_nanos())
                    .to_be_bytes(),
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                hash.update(metadata.mode().to_be_bytes());
                hash.update(metadata.ino().to_be_bytes());
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            hash.update(b"missing");
            Ok(())
        }
        Err(source) => Err(GitError::File {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn scoped_error(
    code: AgentErrorCode,
    scope: AgentErrorScope,
    message: impl Into<String>,
    retryable: bool,
) -> AgentResult {
    AgentResult::Error {
        error: AgentError {
            code,
            scope,
            message: message.into(),
            retryable,
        },
    }
}

fn git_scoped_error(error: GitError, scope: AgentErrorScope) -> AgentResult {
    let (code, retryable) = match error {
        GitError::NotARepository { .. } | GitError::UnsafeRepositoryPath { .. } => {
            (AgentErrorCode::PathDenied, false)
        }
        GitError::UnbornHead | GitError::Parse(_) => (AgentErrorCode::GitFailed, false),
        GitError::Spawn { .. } | GitError::CommandFailed { .. } | GitError::File { .. } => {
            (AgentErrorCode::GitFailed, true)
        }
        GitError::UntrackedFileTooLarge { .. } => (AgentErrorCode::GitFailed, false),
        GitError::ConcurrentModification { .. } => (AgentErrorCode::GitFailed, true),
        GitError::InvalidBlameRange { .. }
        | GitError::InvalidCommitContextLimit { .. }
        | GitError::CommitOutsideComparisonRange { .. } => (AgentErrorCode::InvalidRequest, false),
        GitError::OutputTooLarge { .. } => (AgentErrorCode::TooLarge, false),
    };
    scoped_error(code, scope, error.to_string(), retryable)
}

fn emit_response(response: LocalResponse, json: bool) -> Result<(), CliError> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&response)
                .map_err(|error| CliError::Internal(error.to_string()))?
        );
        return Ok(());
    }
    match response {
        LocalResponse::Opened {
            workspace, created, ..
        } => println!(
            "{} workspace {} ({})",
            if created { "Opened" } else { "Focused" },
            workspace.name,
            workspace.id
        ),
        LocalResponse::Focused { workspace, .. } => {
            println!("Focused workspace {} ({})", workspace.name, workspace.id)
        }
        LocalResponse::PullRequestRequested { url, .. } => println!("Requested pull request {url}"),
        LocalResponse::SshWorkspaceRequested { target, .. } => {
            println!("Requested SSH workspace {target}")
        }
        LocalResponse::ForwardedRemoteWorkspace { path, .. } => {
            println!("Forwarded remote workspace {path} to LocalReview")
        }
        LocalResponse::Workspaces { workspaces, .. } => print_workspaces(&workspaces),
        LocalResponse::Doctor { report, .. } => print_doctor(&report),
        LocalResponse::Error { code, message, .. } => {
            return Err(CliError::Remote { code, message });
        }
    }
    Ok(())
}

fn print_workspaces(workspaces: &[WorkspaceSummary]) {
    if workspaces.is_empty() {
        println!("No registered workspaces.");
        return;
    }
    for workspace in workspaces {
        let tags = workspace
            .source_tags
            .iter()
            .map(|tag| format!("{tag:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{}\t{}\t{}\t{}",
            workspace.id,
            workspace.name,
            tags,
            if workspace.available {
                "available"
            } else {
                "unavailable"
            }
        );
    }
}

fn print_doctor(report: &DoctorReport) {
    println!(
        "Desktop: {}\nProtocol: {}\n{}",
        if report.desktop_reachable {
            "reachable"
        } else {
            "unavailable"
        },
        report.protocol_version,
        report.message
    );
}

fn emit_error(error: &CliError, json: bool) {
    if json {
        let message = serde_json::json!({
            "ok": false,
            "code": error.code(),
            "message": error.to_string(),
        });
        eprintln!("{message}");
    } else {
        eprintln!("localreview: {error}");
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn next_request_id() -> String {
    static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("cli-{}-{timestamp}-{sequence}", std::process::id())
}

struct Endpoint {
    record: RuntimeRecord,
    secret: localreview_protocol::InstallationSecret,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("{0}")]
    Usage(String),
    #[error("desktop unavailable: {0}")]
    Unavailable(String),
    #[error("desktop rejected request ({code:?}): {message}")]
    Remote {
        code: localreview_protocol::RpcErrorCode,
        message: String,
    },
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("LocalReview recovery error: {0}")]
    Persistence(#[from] localreview_persistence::PersistenceError),
    #[error("internal error: {0}")]
    Internal(String),
}

impl CliError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::Unavailable(_) => 3,
            Self::Remote { .. } => 4,
            Self::Protocol(_) => 5,
            Self::Io(_) | Self::Persistence(_) | Self::Internal(_) => 1,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Unavailable(_) => "unavailable",
            Self::Remote { .. } => "remote_error",
            Self::Protocol(_) => "protocol_error",
            Self::Io(_) => "io_error",
            Self::Persistence(_) => "persistence_error",
            Self::Internal(_) => "internal_error",
        }
    }

    fn json_output(&self) -> bool {
        // clap handles parsing before `run`; unavailable and protocol errors
        // still benefit from structured output when callers set --json.
        env::args_os().any(|argument| argument == "--json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_exposes_distinct_new_open_and_existing_focus_flows() {
        let open = Cli::try_parse_from([
            "localreview",
            "open",
            "/tmp/work",
            "--base",
            "origin/main",
            "--repo-base",
            "services/api=origin/HOTFIX-1",
        ])
        .unwrap();
        match open.command {
            Command::Open {
                path,
                base,
                repository_bases,
            } => {
                assert_eq!(path, PathBuf::from("/tmp/work"));
                assert_eq!(base.as_deref(), Some("origin/main"));
                assert_eq!(repository_bases, ["services/api=origin/HOTFIX-1"]);
            }
            other => panic!("expected open command, received {other:?}"),
        }

        for command_name in ["workspace", "focus"] {
            let focus =
                Cli::try_parse_from(["localreview", command_name, "My active workspace"]).unwrap();
            assert!(matches!(
                focus.command,
                Command::Workspace { ref name_or_id } if name_or_id == "My active workspace"
            ));
        }
    }

    #[test]
    fn cli_exposes_global_config_path_without_desktop_forwarding() {
        let parsed = Cli::try_parse_from(["localreview", "config", "path"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Config {
                command: ConfigCommand::Path
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_unix_forwarding_preserves_open_contract_on_macos_and_linux() {
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("desktop.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let secret = localreview_protocol::InstallationSecret::from_bytes([9; 32]);
        let server_secret = secret.clone();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: LocalRequest = read_frame(&mut stream).unwrap();
            server_secret.verify(&request).unwrap();
            assert_eq!(
                request.command,
                LocalCommand::OpenWorkspace {
                    path: "/tmp/work".into(),
                    base: Some("origin/main".into()),
                    repository_bases: vec![RepositoryBaseOverride {
                        relative_path: "services/api".into(),
                        base: "origin/HOTFIX-1".into(),
                    }],
                }
            );
            write_frame(
                &mut stream,
                &LocalResponse::Opened {
                    request_id: request.request_id,
                    workspace: WorkspaceSummary {
                        id: "workspace-1".into(),
                        name: "work".into(),
                        source_tags: vec![localreview_protocol::WorkspaceSourceTag::Local],
                        available: true,
                        location: Some("/tmp/work".into()),
                    },
                    created: true,
                },
            )
            .unwrap();
        });
        let response = send_request(
            &Endpoint {
                record: RuntimeRecord::current(socket_path),
                secret,
            },
            LocalCommand::OpenWorkspace {
                path: "/tmp/work".into(),
                base: Some("origin/main".into()),
                repository_bases: vec![RepositoryBaseOverride {
                    relative_path: "services/api".into(),
                    base: "origin/HOTFIX-1".into(),
                }],
            },
        )
        .unwrap();
        assert!(matches!(
            response,
            LocalResponse::Opened {
                created: true,
                workspace: WorkspaceSummary { ref id, .. },
                ..
            } if id == "workspace-1"
        ));
        worker.join().unwrap();
    }

    #[test]
    fn request_ids_remain_unique_within_one_process_and_timestamp() {
        let first = next_request_id();
        let second = next_request_id();
        assert_ne!(first, second);
        assert!(first.starts_with(&format!("cli-{}-", std::process::id())));
    }

    #[test]
    fn repository_base_parser_preserves_path_and_ref() {
        let parsed = parse_repository_base("services/api=origin/release-1").unwrap();
        assert_eq!(parsed.relative_path, "services/api");
        assert_eq!(parsed.base, "origin/release-1");
        assert!(parse_repository_base("bad-value").is_err());
        assert!(parse_repository_base("../escape=origin/main").is_err());
    }

    #[test]
    fn companion_handshake_and_ping_are_real_protocol_operations() {
        let server = AgentServer::default();
        let output = Arc::new(Mutex::new(io::stdout()));
        let cancellation = AtomicBool::new(false);
        let hello = server.handle(
            &AgentRequest {
                id: "one".into(),
                generation: 7,
                operation: AgentOperation::Handshake {
                    desktop_versions: vec![PROTOCOL_VERSION],
                },
            },
            &cancellation,
            &output,
        );
        assert!(matches!(hello, AgentResult::Handshake { .. }));

        let ping = server.handle(
            &AgentRequest {
                id: "two".into(),
                generation: 8,
                operation: AgentOperation::Ping,
            },
            &cancellation,
            &output,
        );
        assert_eq!(ping, AgentResult::Pong);
    }

    #[test]
    fn source_windows_preserve_crlf_and_final_newline_bytes() {
        let cancellation = AtomicBool::new(false);
        let raw = b"one\r\ntwo\r\nthree".to_vec();
        let mut input = BufReader::new(Cursor::new(raw.clone()));
        let first = stream_source_window(&mut input, 1, 2, &cancellation).unwrap();
        assert_eq!(first.bytes, b"one\r\ntwo\r\n");
        assert_eq!(first.total_lines, 3);
        assert!(!first.end_of_file);
        assert_eq!(first.byte_len, raw.len() as u64);
        assert_eq!(first.content_sha256_hex, hex::encode(Sha256::digest(&raw)));

        let mut input = BufReader::new(Cursor::new(raw));
        let final_window = stream_source_window(&mut input, 3, 2, &cancellation).unwrap();
        assert_eq!(final_window.bytes, b"three");
        assert!(final_window.end_of_file);
    }
}
