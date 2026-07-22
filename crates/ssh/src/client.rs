use localreview_protocol::{
    read_frame, validate_identifier, AgentHello, AgentMessage, AgentNotification, AgentOperation,
    AgentProgress, AgentRequest, AgentResponse, AgentResult, ProtocolError, PROTOCOL_VERSION,
};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read};
use std::process::{Child, ChildStdin, Command as ProcessCommand, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_LIMIT_BYTES: usize = 4 * 1024;
const MAX_QUEUED_NOTIFICATIONS: usize = 1_024;

/// A validated SSH destination. It is passed as one argument to `ssh`, so a
/// host alias, ProxyJump, key selection, and host verification all continue to
/// be governed by the user's normal OpenSSH configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshDestination(String);

impl SshDestination {
    pub fn new(value: impl Into<String>) -> Result<Self, SshError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 512
            || value.starts_with('-')
            || value.bytes().any(|byte| {
                !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@'))
            })
            || value.matches('@').count() > 1
        {
            return Err(SshError::InvalidDestination);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReverseTunnel {
    /// Desktop loopback listener port. It is never exposed on a non-loopback
    /// interface by the constructed SSH `-R` argument.
    pub local_port: u16,
    /// Port on the remote loopback interface that a managed remote shell uses.
    pub remote_port: u16,
}

/// Ephemeral credentials delivered only over the already-established framed
/// remote companion channel.
///
/// They are deliberately not written to disk or included in an SSH remote
/// command/environment. The companion keeps the bearer token inside its
/// session-scoped Unix relay so interactive remote shells never need it.
#[derive(Clone, Eq, PartialEq)]
pub struct ManagedForwardEnvironment {
    pub endpoint: String,
    pub token_hex: String,
    pub session_id: String,
}

impl std::fmt::Debug for ManagedForwardEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedForwardEnvironment")
            .field("endpoint", &self.endpoint)
            .field("token_hex", &"[redacted]")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl ManagedForwardEnvironment {
    pub fn validate(&self, tunnel: &ReverseTunnel) -> Result<(), SshError> {
        let expected_endpoint = format!("127.0.0.1:{}", tunnel.remote_port);
        if self.endpoint != expected_endpoint
            || self.token_hex.len() != 64
            || !self.token_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.session_id.len() > 64
            || self.session_id.is_empty()
            || !self
                .session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(SshError::InvalidManagedForwardEnvironment);
        }
        Ok(())
    }
}

/// The only remote companion executable locations the desktop is allowed to
/// launch. This remains an enum rather than a free-form command/path so a host
/// profile cannot become a shell-execution escape hatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteAgentProgram {
    PathLookup,
    UserLocal,
}

impl RemoteAgentProgram {
    fn as_str(self) -> &'static str {
        match self {
            Self::PathLookup => "localreview",
            Self::UserLocal => ".local/bin/localreview",
        }
    }
}

impl ReverseTunnel {
    pub fn validate(&self) -> Result<(), SshError> {
        if self.local_port == 0 || self.remote_port == 0 {
            return Err(SshError::InvalidReverseTunnel);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SshConnectionConfig {
    pub destination: SshDestination,
    pub ssh_program: OsString,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub reverse_tunnel: Option<ReverseTunnel>,
    pub remote_agent_program: RemoteAgentProgram,
}

impl SshConnectionConfig {
    #[must_use]
    pub fn new(destination: SshDestination) -> Self {
        Self {
            destination,
            ssh_program: OsString::from("ssh"),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reverse_tunnel: None,
            remote_agent_program: RemoteAgentProgram::PathLookup,
        }
    }

    fn command(&self) -> Result<ProcessCommand, SshError> {
        if self.connect_timeout.is_zero() || self.request_timeout.is_zero() {
            return Err(SshError::InvalidTimeout);
        }
        if let Some(tunnel) = &self.reverse_tunnel {
            tunnel.validate()?;
        }
        let seconds = self.connect_timeout.as_secs().max(1).to_string();
        let mut command = ProcessCommand::new(&self.ssh_program);
        command
            .arg("-o")
            .arg(format!("ConnectTimeout={seconds}"))
            .arg("-o")
            .arg("BatchMode=yes");
        if let Some(tunnel) = &self.reverse_tunnel {
            // A collision or server refusal must fail the managed session
            // before it can claim reverse forwarding is available.
            command.arg("-o").arg("ExitOnForwardFailure=yes");
            command.arg("-R").arg(format!(
                "127.0.0.1:{}:127.0.0.1:{}",
                tunnel.remote_port, tunnel.local_port
            ));
        }
        // The remote program is a fixed token sequence. User input is never
        // interpolated into it or passed through a local shell.
        command
            .arg(self.destination.as_str())
            .args([self.remote_agent_program.as_str(), "agent", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SshConnectionState {
    Connecting,
    Connected {
        agent_version: String,
        latency: Duration,
    },
    Disconnected {
        detail: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteConnectionInfo {
    pub hello: AgentHello,
    pub latency: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshProgressEvent {
    pub progress: AgentProgress,
}

#[derive(Clone)]
pub struct SshCancellation {
    writer: Arc<Mutex<ChildStdin>>,
    state: Arc<Mutex<SshConnectionState>>,
    sequence: Arc<AtomicU64>,
}

impl std::fmt::Debug for SshCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshCancellation")
            .finish_non_exhaustive()
    }
}

impl SshCancellation {
    /// Cancellation is cooperative: the companion acknowledges it immediately
    /// and emits `Cancelled` for the original job as soon as its bounded Git
    /// operation reaches a cancellation point.
    pub fn cancel(&self, request_id: &str, generation: u64) -> Result<String, SshError> {
        validate_identifier(request_id, "cancel request id")?;
        let id = format!(
            "cancel-{}-{}",
            std::process::id(),
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let request = AgentRequest {
            id: id.clone(),
            generation,
            operation: AgentOperation::Cancel {
                request_id: request_id.into(),
            },
        };
        write_request(&self.writer, &request, &self.state)?;
        Ok(id)
    }
}

/// One running SSH stdio transport. It serializes regular request/response
/// calls while allowing a cloned cancellation handle to interrupt them.
#[derive(Debug)]
pub struct SshSession {
    config: SshConnectionConfig,
    child: Mutex<Child>,
    writer: Arc<Mutex<ChildStdin>>,
    incoming: Receiver<Result<AgentMessage, SshError>>,
    notifications: VecDeque<AgentNotification>,
    state: Arc<Mutex<SshConnectionState>>,
    disconnected: Arc<AtomicBool>,
    sequence: Arc<AtomicU64>,
    pub connection: RemoteConnectionInfo,
    stale_messages: u64,
}

impl SshSession {
    pub fn connect(config: SshConnectionConfig) -> Result<Self, SshError> {
        let state = Arc::new(Mutex::new(SshConnectionState::Connecting));
        let command = config.command()?;
        Self::connect_spawned(config, command, state)
    }

    fn connect_spawned(
        config: SshConnectionConfig,
        mut command: ProcessCommand,
        state: Arc<Mutex<SshConnectionState>>,
    ) -> Result<Self, SshError> {
        let mut child = command.spawn().map_err(SshError::Spawn)?;
        let stdin = child.stdin.take().ok_or(SshError::MissingPipe("stdin"))?;
        let stdout = child.stdout.take().ok_or(SshError::MissingPipe("stdout"))?;
        let stderr = child.stderr.take().ok_or(SshError::MissingPipe("stderr"))?;
        let writer = Arc::new(Mutex::new(stdin));
        let (sender, receiver) = mpsc::channel();
        let disconnected = Arc::new(AtomicBool::new(false));
        spawn_reader(
            stdout,
            sender,
            Arc::clone(&state),
            Arc::clone(&disconnected),
        );
        drain_stderr(stderr);
        let mut session = Self {
            config,
            child: Mutex::new(child),
            writer,
            incoming: receiver,
            notifications: VecDeque::new(),
            state,
            disconnected,
            sequence: Arc::new(AtomicU64::new(1)),
            connection: RemoteConnectionInfo {
                hello: AgentHello::current("unverified", "unknown", "unknown"),
                latency: Duration::ZERO,
            },
            stale_messages: 0,
        };
        let started = Instant::now();
        let response = session.request_with_timeout(
            AgentOperation::Handshake {
                desktop_versions: vec![PROTOCOL_VERSION],
            },
            0,
            session.config.connect_timeout,
            |_| {},
        )?;
        let AgentResult::Handshake {
            selected_version,
            hello,
        } = response
        else {
            session.mark_disconnected("remote companion did not return a handshake".into());
            return Err(SshError::InvalidHandshake);
        };
        if selected_version != PROTOCOL_VERSION
            || !hello.protocol_versions.contains(&selected_version)
        {
            session.mark_disconnected(
                "remote companion selected an unsupported protocol version".into(),
            );
            return Err(SshError::VersionMismatch {
                local: PROTOCOL_VERSION,
                remote: selected_version,
            });
        }
        let latency = started.elapsed();
        session.connection = RemoteConnectionInfo {
            hello: hello.clone(),
            latency,
        };
        *session
            .state
            .lock()
            .expect("SSH connection state is not poisoned") = SshConnectionState::Connected {
            agent_version: hello.agent_version,
            latency,
        };
        Ok(session)
    }

    #[must_use]
    pub fn state(&self) -> SshConnectionState {
        self.state
            .lock()
            .expect("SSH connection state is not poisoned")
            .clone()
    }

    /// Becomes true as soon as the SSH reader observes transport closure. It
    /// lets session-owned resources such as reverse listeners expire even when
    /// the UI is idle and no request happens to notice the disconnection.
    #[must_use]
    pub fn disconnection_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.disconnected)
    }

    #[must_use]
    pub fn cancellation(&self) -> SshCancellation {
        SshCancellation {
            writer: Arc::clone(&self.writer),
            state: Arc::clone(&self.state),
            sequence: Arc::clone(&self.sequence),
        }
    }

    /// Delivers the managed reverse-forward credential after SSH has already
    /// established the fixed companion stdio channel. This avoids exposing a
    /// bearer token in remote process argv/environment while keeping the
    /// protocol closed to the one workspace-open relay operation.
    pub fn configure_managed_forward_relay(
        &mut self,
        environment: &ManagedForwardEnvironment,
    ) -> Result<(), SshError> {
        let tunnel = self
            .config
            .reverse_tunnel
            .as_ref()
            .ok_or(SshError::InvalidManagedForwardEnvironment)?;
        environment.validate(tunnel)?;
        let result = self.request_with_timeout(
            AgentOperation::ConfigureManagedForwardRelay {
                endpoint: environment.endpoint.clone(),
                token_hex: environment.token_hex.clone(),
                session_id: environment.session_id.clone(),
            },
            0,
            self.config.connect_timeout,
            |_| {},
        )?;
        if matches!(result, AgentResult::ManagedForwardRelayConfigured) {
            Ok(())
        } else {
            Err(SshError::InvalidManagedForwardRelayResponse)
        }
    }

    #[must_use]
    pub fn take_notifications(&mut self) -> Vec<AgentNotification> {
        self.notifications.drain(..).collect()
    }

    #[must_use]
    pub fn stale_messages_discarded(&self) -> u64 {
        self.stale_messages
    }

    pub fn request(
        &mut self,
        operation: AgentOperation,
        generation: u64,
        on_progress: impl FnMut(SshProgressEvent),
    ) -> Result<AgentResult, SshError> {
        self.request_with_timeout(
            operation,
            generation,
            self.config.request_timeout,
            on_progress,
        )
    }

    pub fn request_with_timeout(
        &mut self,
        operation: AgentOperation,
        generation: u64,
        timeout: Duration,
        on_progress: impl FnMut(SshProgressEvent),
    ) -> Result<AgentResult, SshError> {
        let request_id = format!(
            "ssh-{}-{}",
            std::process::id(),
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        self.request_with_id(request_id, operation, generation, timeout, on_progress)
    }

    /// Starts a request with a caller-supplied identifier so a UI job can keep
    /// that identifier and cancel it from a separate event handler.
    pub fn request_with_id(
        &mut self,
        request_id: String,
        operation: AgentOperation,
        generation: u64,
        timeout: Duration,
        on_progress: impl FnMut(SshProgressEvent),
    ) -> Result<AgentResult, SshError> {
        self.request_with_id_and_notifications(
            request_id,
            operation,
            generation,
            timeout,
            on_progress,
            |_| {},
        )
    }

    /// Like [`Self::request_with_id`], but delivers asynchronous companion
    /// notifications while a long-running typed request (notably a remote
    /// change watcher) is still pending. The callback is intentionally typed
    /// and has no transport access, so it cannot turn a watcher into a remote
    /// command channel.
    pub fn request_with_id_and_notifications(
        &mut self,
        request_id: String,
        operation: AgentOperation,
        generation: u64,
        timeout: Duration,
        mut on_progress: impl FnMut(SshProgressEvent),
        mut on_notification: impl FnMut(AgentNotification),
    ) -> Result<AgentResult, SshError> {
        if timeout.is_zero() {
            return Err(SshError::InvalidTimeout);
        }
        if matches!(self.state(), SshConnectionState::Disconnected { .. }) {
            return Err(SshError::Disconnected(
                "the SSH session is disconnected".into(),
            ));
        }
        let request = AgentRequest {
            id: request_id.clone(),
            generation,
            operation,
        };
        request.validate()?;
        write_request(&self.writer, &request, &self.state)?;
        self.await_response(
            &request_id,
            generation,
            timeout,
            &mut on_progress,
            &mut on_notification,
        )
    }

    pub fn reconnect(&mut self) -> Result<(), SshError> {
        self.terminate_child();
        *self
            .state
            .lock()
            .expect("SSH connection state is not poisoned") = SshConnectionState::Connecting;
        let replacement = Self::connect(self.config.clone())?;
        *self = replacement;
        Ok(())
    }

    fn await_response(
        &mut self,
        request_id: &str,
        generation: u64,
        timeout: Duration,
        on_progress: &mut impl FnMut(SshProgressEvent),
        on_notification: &mut impl FnMut(AgentNotification),
    ) -> Result<AgentResult, SshError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.cancellation().cancel(request_id, generation);
                return Err(SshError::TimedOut {
                    request_id: request_id.into(),
                    timeout,
                });
            }
            let message = match self.next_message(remaining) {
                Ok(message) => message,
                Err(SshError::TimedOut { .. }) => {
                    let _ = self.cancellation().cancel(request_id, generation);
                    return Err(SshError::TimedOut {
                        request_id: request_id.into(),
                        timeout,
                    });
                }
                Err(error) => return Err(error),
            };
            match message {
                AgentMessage::Progress(progress) if progress.id == request_id => {
                    if progress.generation == generation {
                        on_progress(SshProgressEvent { progress });
                    } else {
                        self.stale_messages += 1;
                    }
                }
                AgentMessage::Response(AgentResponse {
                    id,
                    generation: response_generation,
                    result,
                }) if id == request_id => {
                    if response_generation != generation {
                        self.stale_messages += 1;
                        continue;
                    }
                    return Ok(result);
                }
                AgentMessage::Notification(notification) => {
                    on_notification(notification.clone());
                    if self.notifications.len() >= MAX_QUEUED_NOTIFICATIONS {
                        self.notifications.pop_front();
                    }
                    self.notifications.push_back(notification)
                }
                // This session intentionally serializes normal requests. The
                // only concurrent request is a cancellation acknowledgement,
                // which must not be requeued ahead of the original result or
                // it would starve the reader forever.
                AgentMessage::Progress(_)
                | AgentMessage::Response(_)
                | AgentMessage::Request(_) => {
                    self.stale_messages += 1;
                }
            }
        }
    }

    fn next_message(&mut self, timeout: Duration) -> Result<AgentMessage, SshError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) => {
                self.mark_disconnected(error.to_string());
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => Err(SshError::TimedOut {
                request_id: "transport".into(),
                timeout,
            }),
            Err(RecvTimeoutError::Disconnected) => {
                let error = SshError::Disconnected("remote SSH stdout closed".into());
                self.mark_disconnected(error.to_string());
                Err(error)
            }
        }
    }

    fn mark_disconnected(&self, detail: String) {
        self.disconnected.store(true, Ordering::Release);
        *self
            .state
            .lock()
            .expect("SSH connection state is not poisoned") =
            SshConnectionState::Disconnected { detail };
    }

    fn terminate_child(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        self.terminate_child();
    }
}

fn write_request(
    writer: &Arc<Mutex<ChildStdin>>,
    request: &AgentRequest,
    state: &Arc<Mutex<SshConnectionState>>,
) -> Result<(), SshError> {
    let mut writer = writer
        .lock()
        .map_err(|_| SshError::Disconnected("SSH stdin lock is poisoned".into()))?;
    localreview_protocol::write_frame(&mut *writer, &AgentMessage::Request(request.clone()))
        .map_err(|error| {
            *state.lock().expect("SSH connection state is not poisoned") =
                SshConnectionState::Disconnected {
                    detail: error.to_string(),
                };
            SshError::Protocol(error)
        })
}

fn spawn_reader(
    mut stdout: impl Read + Send + 'static,
    sender: Sender<Result<AgentMessage, SshError>>,
    state: Arc<Mutex<SshConnectionState>>,
    disconnected: Arc<AtomicBool>,
) {
    thread::spawn(move || loop {
        let message = read_frame(&mut stdout).map_err(SshError::Protocol);
        let terminal = message.is_err();
        if sender.send(message).is_err() {
            return;
        }
        if terminal {
            disconnected.store(true, Ordering::Release);
            *state.lock().expect("SSH connection state is not poisoned") =
                SshConnectionState::Disconnected {
                    detail: "remote SSH stdout closed".into(),
                };
            return;
        }
    });
}

fn drain_stderr(mut stderr: impl Read + Send + 'static) {
    thread::spawn(move || {
        // SSH diagnostic output is intentionally not forwarded into review
        // payloads. Draining a small bounded buffer prevents a noisy process
        // from blocking while avoiding accidental source/credential logging.
        let mut discarded = [0_u8; STDERR_LIMIT_BYTES];
        while stderr.read(&mut discarded).unwrap_or(0) != 0 {}
    });
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid SSH destination")]
    InvalidDestination,
    #[error("invalid SSH reverse-forward tunnel")]
    InvalidReverseTunnel,
    #[error("invalid managed SSH reverse-forward environment")]
    InvalidManagedForwardEnvironment,
    #[error("remote companion did not acknowledge managed reverse forwarding")]
    InvalidManagedForwardRelayResponse,
    #[error("SSH timeouts must be non-zero")]
    InvalidTimeout,
    #[error("could not spawn SSH: {0}")]
    Spawn(#[source] io::Error),
    #[error("SSH process did not expose {0}")]
    MissingPipe(&'static str),
    #[error("SSH companion protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("SSH companion disconnected: {0}")]
    Disconnected(String),
    #[error("SSH companion request {request_id} timed out after {timeout:?}")]
    TimedOut {
        request_id: String,
        timeout: Duration,
    },
    #[error("SSH companion handshake was invalid")]
    InvalidHandshake,
    #[error("SSH companion protocol mismatch (desktop {local}, companion {remote})")]
    VersionMismatch { local: u16, remote: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_and_reverse_tunnel_are_not_shell_or_option_injection() {
        assert!(SshDestination::new("build-host").is_ok());
        assert!(SshDestination::new("user@build-host").is_ok());
        assert!(SshDestination::new("-oProxyCommand=bad").is_err());
        assert!(SshDestination::new("host name").is_err());
        assert!(SshDestination::new("host:22").is_err());
        assert!(ReverseTunnel {
            local_port: 1,
            remote_port: 1
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn ssh_command_uses_normal_config_and_fixed_agent_tokens() {
        let config = SshConnectionConfig::new(SshDestination::new("configured-alias").unwrap());
        let command = config.command().unwrap();
        let debug = format!("{command:?}");
        assert!(debug.contains("configured-alias"));
        assert!(debug.contains("localreview"));
        assert!(debug.contains("--stdio"));
    }

    #[test]
    fn managed_reverse_tunnel_is_loopback_only_and_keeps_agent_command_fixed() {
        let mut config = SshConnectionConfig::new(SshDestination::new("configured-alias").unwrap());
        config.reverse_tunnel = Some(ReverseTunnel {
            local_port: 41_001,
            remote_port: 51_001,
        });
        let environment = ManagedForwardEnvironment {
            endpoint: "127.0.0.1:51001".into(),
            token_hex: "ab".repeat(32),
            session_id: "session_123".into(),
        };
        environment
            .validate(config.reverse_tunnel.as_ref().unwrap())
            .unwrap();
        let command = config.command().unwrap();
        let debug = format!("{command:?}");
        assert!(debug.contains("ExitOnForwardFailure=yes"));
        assert!(debug.contains("127.0.0.1:51001:127.0.0.1:41001"));
        assert!(debug.contains("localreview"));
        assert!(!debug.contains("LOCALREVIEW_MANAGED_FORWARD_TOKEN"));
        assert!(!debug.contains(&environment.token_hex));
        assert!(format!("{environment:?}").contains("[redacted]"));
    }

    #[test]
    fn managed_reverse_environment_must_match_the_declared_remote_loopback_port() {
        let mut config = SshConnectionConfig::new(SshDestination::new("configured-alias").unwrap());
        config.reverse_tunnel = Some(ReverseTunnel {
            local_port: 41_001,
            remote_port: 51_001,
        });
        let environment = ManagedForwardEnvironment {
            endpoint: "127.0.0.1:51002".into(),
            token_hex: "ab".repeat(32),
            session_id: "session_123".into(),
        };
        assert!(matches!(
            environment.validate(config.reverse_tunnel.as_ref().unwrap()),
            Err(SshError::InvalidManagedForwardEnvironment)
        ));
    }

    #[test]
    fn managed_relay_credentials_are_configured_after_the_fixed_stdio_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("companion.frames");
        let state = Arc::new(Mutex::new(SshConnectionState::Connecting));
        let mut bytes = Vec::new();
        for response in [
            AgentResponse {
                id: format!("ssh-{}-1", std::process::id()),
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: AgentHello::current("test-agent", "linux", "x86_64"),
                },
            },
            AgentResponse {
                id: format!("ssh-{}-2", std::process::id()),
                generation: 0,
                result: AgentResult::ManagedForwardRelayConfigured,
            },
        ] {
            localreview_protocol::write_frame(&mut bytes, &AgentMessage::Response(response))
                .unwrap();
        }
        std::fs::write(&transcript, bytes).unwrap();
        let mut command = ProcessCommand::new("sh");
        command
            .arg("-c")
            .arg("cat \"$1\"; sleep 1")
            .arg("localreview-test-companion")
            .arg(&transcript)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut config = SshConnectionConfig::new(SshDestination::new("test-host").unwrap());
        config.reverse_tunnel = Some(ReverseTunnel {
            local_port: 41_001,
            remote_port: 51_001,
        });
        let mut session = SshSession::connect_spawned(config, command, state).unwrap();
        session
            .configure_managed_forward_relay(&ManagedForwardEnvironment {
                endpoint: "127.0.0.1:51001".into(),
                token_hex: "ab".repeat(32),
                session_id: "session_123".into(),
            })
            .unwrap();
    }

    #[test]
    fn long_running_typed_request_delivers_notifications_before_completion() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = directory.path().join("companion.frames");
        let state = Arc::new(Mutex::new(SshConnectionState::Connecting));
        let handshake_id = format!("ssh-{}-1", std::process::id());
        let mut bytes = Vec::new();
        localreview_protocol::write_frame(
            &mut bytes,
            &AgentMessage::Response(AgentResponse {
                id: handshake_id,
                generation: 0,
                result: AgentResult::Handshake {
                    selected_version: PROTOCOL_VERSION,
                    hello: AgentHello::current("test-agent", "linux", "x86_64"),
                },
            }),
        )
        .unwrap();
        let repository = localreview_protocol::RemoteRepositoryRef {
            workspace_root: "/work".into(),
            relative_path: ".".into(),
        };
        localreview_protocol::write_frame(
            &mut bytes,
            &AgentMessage::Notification(AgentNotification::FilesystemChangesAvailable {
                repository: repository.clone(),
                generation: 1,
            }),
        )
        .unwrap();
        std::fs::write(&transcript, bytes).unwrap();

        let mut command = ProcessCommand::new("sh");
        command
            .arg("-c")
            .arg("cat \"$1\"; sleep 1")
            .arg("localreview-test-companion")
            .arg(&transcript)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let config = SshConnectionConfig::new(SshDestination::new("test-host").unwrap());
        let mut session = SshSession::connect_spawned(config, command, state).unwrap();
        let mut notified = false;
        let result = session.request_with_id_and_notifications(
            "watch-test".into(),
            AgentOperation::WatchRepositoryChanges {
                repository,
                poll_interval_millis: 1_000,
            },
            1,
            Duration::from_millis(25),
            |_| {},
            |_| notified = true,
        );
        assert!(matches!(result, Err(SshError::TimedOut { .. })));
        assert!(
            notified,
            "notification callback must run while watcher awaits response"
        );
    }
}
