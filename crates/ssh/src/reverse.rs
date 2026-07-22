use crate::SshError;
use localreview_protocol::{
    read_frame, write_frame, ManagedForwardCommand, ManagedForwardRequest, ManagedForwardResponse,
    ProtocolError, RepositoryBaseOverride, REVERSE_FORWARD_VERSION,
};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

const TOKEN_BYTES: usize = 32;
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// The one operation a remote `localreview open .` may forward through a
/// managed session. The desktop uses the session's known SSH host to turn this
/// into a normal `host:/absolute/path` SSH workspace open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardedRemoteOpen {
    pub path: String,
    pub base: Option<String>,
    pub repository_bases: Vec<RepositoryBaseOverride>,
}

/// In-memory state for one desktop-managed reverse channel. The token is never
/// written to a file, database, remote command line, or application log.
#[derive(Debug)]
pub struct ReverseForwardListener {
    listener: TcpListener,
    token: [u8; TOKEN_BYTES],
    remote_port: u16,
    expires_at: Instant,
}

impl ReverseForwardListener {
    /// Allocates a listener for one managed SSH session. The remote port is a
    /// high, randomly selected loopback port; `ExitOnForwardFailure` makes a
    /// collision a connection failure rather than a silent half-working path.
    pub fn bind_managed(ttl: Duration) -> Result<Self, ReverseForwardError> {
        if ttl.is_zero() {
            return Err(ReverseForwardError::InvalidSession);
        }
        let mut bytes = [0_u8; 2];
        getrandom::getrandom(&mut bytes)
            .map_err(|error| ReverseForwardError::EntropyUnavailable(error.to_string()))?;
        // IANA's dynamic/private range. This avoids privileged and commonly
        // reserved ports while keeping the selected address explicit for -R.
        let remote_port = 49_152 + (u16::from_be_bytes(bytes) % 16_384);
        Self::bind(remote_port, ttl)
    }

    pub fn bind(remote_port: u16, ttl: Duration) -> Result<Self, ReverseForwardError> {
        if remote_port == 0 || ttl.is_zero() {
            return Err(ReverseForwardError::InvalidSession);
        }
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
        listener.set_nonblocking(true)?;
        let mut token = [0_u8; TOKEN_BYTES];
        getrandom::getrandom(&mut token)
            .map_err(|error| ReverseForwardError::EntropyUnavailable(error.to_string()))?;
        Ok(Self {
            listener,
            token,
            remote_port,
            expires_at: Instant::now() + ttl,
        })
    }

    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.listener
            .local_addr()
            .map_or(0, |address| address.port())
    }

    #[must_use]
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Environment passed only to a LocalReview-managed remote shell/session.
    /// It is intentionally returned to the caller instead of being persisted or
    /// automatically injected into arbitrary existing SSH shells.
    #[must_use]
    pub fn managed_environment(&self) -> Vec<(String, String)> {
        vec![
            (
                "LOCALREVIEW_MANAGED_FORWARD_ENDPOINT".into(),
                format!("127.0.0.1:{}", self.remote_port),
            ),
            (
                "LOCALREVIEW_MANAGED_FORWARD_TOKEN".into(),
                hex::encode(self.token),
            ),
        ]
    }

    /// Accepts exactly one forwarded request. It requires a loopback peer,
    /// unexpired session, constant-time token match, and the closed protocol
    /// command; invalid input receives a typed error response where possible.
    pub fn accept_open(
        &self,
        timeout: Duration,
    ) -> Result<ForwardedRemoteOpen, ReverseForwardError> {
        self.accept_open_with(timeout, |_| Ok(()))
    }

    /// Serves exactly one closed-protocol request and acknowledges it only
    /// after the desktop's typed workspace-open handler has accepted it. This
    /// prevents a remote CLI from receiving success for a forward that could
    /// not be committed on the desktop.
    pub fn accept_open_with(
        &self,
        timeout: Duration,
        handler: impl FnOnce(&ForwardedRemoteOpen) -> Result<(), String>,
    ) -> Result<ForwardedRemoteOpen, ReverseForwardError> {
        if timeout.is_zero() {
            return Err(ReverseForwardError::InvalidSession);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= self.expires_at {
                return Err(ReverseForwardError::Expired);
            }
            if Instant::now() >= deadline {
                return Err(ReverseForwardError::TimedOut);
            }
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        return Err(ReverseForwardError::NonLoopbackPeer);
                    }
                    return self.read_open_with(stream, handler);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL)
                }
                Err(error) => return Err(ReverseForwardError::Io(error)),
            }
        }
    }

    fn read_open_with(
        &self,
        mut stream: TcpStream,
        handler: impl FnOnce(&ForwardedRemoteOpen) -> Result<(), String>,
    ) -> Result<ForwardedRemoteOpen, ReverseForwardError> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let request: ManagedForwardRequest = read_frame(&mut stream)?;
        if let Err(error) = request.validate() {
            let _ = write_frame(
                &mut stream,
                &ManagedForwardResponse::Error {
                    message: "managed forward request was invalid".into(),
                },
            );
            return Err(ReverseForwardError::Protocol(error));
        }
        let token =
            hex::decode(&request.token_hex).map_err(|_| ReverseForwardError::TokenRejected)?;
        if !constant_time_equal(&token, &self.token) {
            let _ = write_frame(
                &mut stream,
                &ManagedForwardResponse::Error {
                    message: "managed forward token was rejected".into(),
                },
            );
            return Err(ReverseForwardError::TokenRejected);
        }
        let ManagedForwardCommand::OpenRemoteWorkspace {
            path,
            base,
            repository_bases,
        } = request.command;
        let open = ForwardedRemoteOpen {
            path,
            base,
            repository_bases,
        };
        if let Err(message) = handler(&open) {
            let _ = write_frame(
                &mut stream,
                &ManagedForwardResponse::Error {
                    message: message.clone(),
                },
            );
            return Err(ReverseForwardError::Rejected(message));
        }
        write_frame(&mut stream, &ManagedForwardResponse::Accepted)?;
        Ok(open)
    }
}

/// Forwards an already canonicalized remote working directory through the
/// active managed session. It is called by the Linux CLI only when both
/// ephemeral environment variables are present.
pub fn forward_managed_open(
    endpoint: &str,
    token_hex: &str,
    path: String,
    base: Option<String>,
    repository_bases: Vec<RepositoryBaseOverride>,
    timeout: Duration,
) -> Result<(), ReverseForwardError> {
    validate_loopback_endpoint(endpoint)?;
    let request = ManagedForwardRequest {
        version: REVERSE_FORWARD_VERSION,
        token_hex: token_hex.into(),
        command: ManagedForwardCommand::OpenRemoteWorkspace {
            path,
            base,
            repository_bases,
        },
    };
    request.validate()?;
    let mut stream = TcpStream::connect_timeout(
        &endpoint
            .parse()
            .map_err(|_| ReverseForwardError::InvalidEndpoint)?,
        timeout,
    )?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_frame(&mut stream, &request)?;
    match read_frame(&mut stream)? {
        ManagedForwardResponse::Accepted => Ok(()),
        ManagedForwardResponse::Error { message } => Err(ReverseForwardError::Rejected(message)),
    }
}

fn validate_loopback_endpoint(value: &str) -> Result<(), ReverseForwardError> {
    let address: SocketAddr = value
        .parse()
        .map_err(|_| ReverseForwardError::InvalidEndpoint)?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(ReverseForwardError::InvalidEndpoint);
    }
    Ok(())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[derive(Debug, thiserror::Error)]
pub enum ReverseForwardError {
    #[error("managed reverse forwarding requires a non-zero port and TTL")]
    InvalidSession,
    #[error("managed reverse forwarding endpoint must be loopback with a non-zero port")]
    InvalidEndpoint,
    #[error("managed reverse forwarding token was rejected")]
    TokenRejected,
    #[error("managed reverse forwarding could not obtain OS entropy: {0}")]
    EntropyUnavailable(String),
    #[error("managed reverse forwarding session has expired")]
    Expired,
    #[error("managed reverse forwarding timed out")]
    TimedOut,
    #[error("managed reverse forwarding peer was not loopback")]
    NonLoopbackPeer,
    #[error("managed reverse forwarding request was rejected: {0}")]
    Rejected(String),
    #[error("managed reverse forwarding protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("managed reverse forwarding I/O error: {0}")]
    Io(#[from] io::Error),
}

impl From<ReverseForwardError> for SshError {
    fn from(error: ReverseForwardError) -> Self {
        SshError::Disconnected(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn managed_reverse_forwarding_is_exercisable_and_token_scoped() {
        let listener = ReverseForwardListener::bind(42_424, Duration::from_secs(2)).unwrap();
        let environment = listener.managed_environment();
        let endpoint = format!("127.0.0.1:{}", listener.local_port());
        let token = environment
            .iter()
            .find(|(key, _)| key == "LOCALREVIEW_MANAGED_FORWARD_TOKEN")
            .unwrap()
            .1
            .clone();
        let join = thread::spawn(move || {
            forward_managed_open(
                &endpoint,
                &token,
                "/remote/work".into(),
                Some("origin/main".into()),
                vec![],
                Duration::from_secs(1),
            )
        });
        let open = listener.accept_open(Duration::from_secs(1)).unwrap();
        join.join().unwrap().unwrap();
        assert_eq!(open.path, "/remote/work");
        assert_eq!(open.base.as_deref(), Some("origin/main"));
        assert!(listener
            .managed_environment()
            .iter()
            .all(|(key, _)| key.starts_with("LOCALREVIEW_MANAGED_FORWARD_")));
    }

    #[test]
    fn token_is_not_reusable_without_exact_match() {
        let listener = ReverseForwardListener::bind(42_425, Duration::from_secs(1)).unwrap();
        let endpoint = format!("127.0.0.1:{}", listener.local_port());
        let join = thread::spawn(move || {
            forward_managed_open(
                &endpoint,
                &"00".repeat(32),
                "/remote/work".into(),
                None,
                vec![],
                Duration::from_secs(1),
            )
        });
        assert!(matches!(
            listener.accept_open(Duration::from_secs(1)),
            Err(ReverseForwardError::TokenRejected)
        ));
        assert!(join.join().unwrap().is_err());
    }
}
