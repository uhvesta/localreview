use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read, Write},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

/// A shell-free invocation of the GitHub CLI. Mutating requests carry JSON in
/// `stdin`, never in a shell fragment or interpolated command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhCommand {
    pub arguments: Vec<OsString>,
    pub stdin: Option<Vec<u8>>,
}

impl GhCommand {
    #[must_use]
    pub fn new(arguments: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        Self {
            arguments: arguments.into_iter().map(Into::into).collect(),
            stdin: None,
        }
    }

    #[must_use]
    pub fn with_stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// A safe diagnostic representation. Standard input is intentionally not
    /// rendered because it may contain private review content.
    #[must_use]
    pub fn display(&self) -> String {
        let arguments = self
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        if self.stdin.is_some() {
            format!("gh {arguments} <review-json>")
        } else {
            format!("gh {arguments}")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GhOutput {
    #[must_use]
    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    #[must_use]
    pub fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
}

pub trait GhExecutor: Send + Sync {
    fn execute(&self, command: &GhCommand) -> Result<GhOutput, GhError>;
}

/// `gh` is an external provider boundary, not a daemon owned by the app. A
/// stalled credential helper or network call must not hang the desktop review
/// forever, and provider output must not be able to exhaust it.
const GH_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_GH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct ProcessGhExecutor;

impl GhExecutor for ProcessGhExecutor {
    fn execute(&self, command: &GhCommand) -> Result<GhOutput, GhError> {
        let mut process = Command::new("gh");
        process
            .args(&command.arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if command.stdin.is_some() {
            process.stdin(Stdio::piped());
        }
        let mut child = process.spawn().map_err(|source| GhError::Spawn {
            command: command.display(),
            source,
        })?;
        if let Some(stdin) = &command.stdin {
            let mut pipe = child
                .stdin
                .take()
                .ok_or_else(|| GhError::StdinUnavailable {
                    command: command.display(),
                })?;
            pipe.write_all(stdin)
                .map_err(|source| GhError::WriteStdin {
                    command: command.display(),
                    source,
                })?;
        }
        // Drain both streams concurrently. Waiting for `gh` before reading
        // them can deadlock on a full stderr pipe, while stopping at the limit
        // can deadlock the child for the same reason; `drain_limited` keeps
        // draining and reports the limit only after the command exits.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GhError::PipeUnavailable {
                command: command.display(),
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| GhError::PipeUnavailable {
                command: command.display(),
            })?;
        let stdout_reader = thread::spawn(move || drain_limited(stdout, MAX_GH_OUTPUT_BYTES));
        let stderr_reader = thread::spawn(move || drain_limited(stderr, MAX_GH_OUTPUT_BYTES));
        let deadline = Instant::now() + GH_COMMAND_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(|source| GhError::Wait {
                command: command.display(),
                source,
            })? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(GhError::TimedOut {
                    command: command.display(),
                    timeout: GH_COMMAND_TIMEOUT,
                });
            }
            thread::sleep(Duration::from_millis(25));
        };
        let (stdout, stdout_limited) = stdout_reader
            .join()
            .map_err(|_| GhError::ReaderPanicked {
                command: command.display(),
            })?
            .map_err(|source| GhError::ReadOutput {
                command: command.display(),
                source,
            })?;
        let (stderr, stderr_limited) = stderr_reader
            .join()
            .map_err(|_| GhError::ReaderPanicked {
                command: command.display(),
            })?
            .map_err(|source| GhError::ReadOutput {
                command: command.display(),
                source,
            })?;
        if stdout_limited || stderr_limited {
            return Err(GhError::OutputTooLarge {
                command: command.display(),
                limit: MAX_GH_OUTPUT_BYTES,
            });
        }
        Ok(GhOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout,
            stderr,
        })
    }
}

fn drain_limited(mut reader: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok((bytes, exceeded));
        }
        let remaining = limit.saturating_sub(bytes.len());
        if read <= remaining {
            bytes.extend_from_slice(&buffer[..read]);
        } else {
            bytes.extend_from_slice(&buffer[..remaining]);
            exceeded = true;
        }
    }
}

/// Deterministic runner for unit tests and service-level fixture tests. It has
/// no process or network capability and records exact typed calls.
#[derive(Clone, Debug, Default)]
pub struct FixtureGhExecutor {
    responses: Arc<Mutex<VecDeque<Result<GhOutput, FixtureGhError>>>>,
    commands: Arc<Mutex<Vec<GhCommand>>>,
}

#[derive(Clone, Debug, Error)]
#[error("fixture command failed: {message}")]
pub struct FixtureGhError {
    message: String,
}

impl FixtureGhExecutor {
    #[must_use]
    pub fn with_outputs(outputs: impl IntoIterator<Item = GhOutput>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(
                outputs.into_iter().map(Ok).collect::<VecDeque<_>>(),
            )),
            commands: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn push_output(&self, output: GhOutput) {
        self.responses
            .lock()
            .expect("fixture lock")
            .push_back(Ok(output));
    }

    pub fn push_error(&self, message: impl Into<String>) {
        self.responses
            .lock()
            .expect("fixture lock")
            .push_back(Err(FixtureGhError {
                message: message.into(),
            }));
    }

    #[must_use]
    pub fn commands(&self) -> Vec<GhCommand> {
        self.commands.lock().expect("fixture lock").clone()
    }
}

impl GhExecutor for FixtureGhExecutor {
    fn execute(&self, command: &GhCommand) -> Result<GhOutput, GhError> {
        self.commands
            .lock()
            .expect("fixture lock")
            .push(command.clone());
        let response = self
            .responses
            .lock()
            .expect("fixture lock")
            .pop_front()
            .ok_or(GhError::FixtureExhausted)?;
        response.map_err(|error| GhError::Fixture(error.message))
    }
}

#[derive(Debug, Error)]
pub enum GhError {
    #[error("could not run {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("could not provide input to {command}: {source}")]
    WriteStdin {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("could not obtain standard input for {command}")]
    StdinUnavailable { command: String },
    #[error("could not obtain provider output pipe for {command}")]
    PipeUnavailable { command: String },
    #[error("could not wait for {command}: {source}")]
    Wait {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("could not read output from {command}: {source}")]
    ReadOutput {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("provider output reader panicked for {command}")]
    ReaderPanicked { command: String },
    #[error("GitHub CLI timed out after {timeout:?}: {command}")]
    TimedOut { command: String, timeout: Duration },
    #[error("GitHub CLI output exceeded {limit} bytes: {command}")]
    OutputTooLarge { command: String, limit: usize },
    #[error("GitHub CLI command failed ({command}): {stderr}")]
    CommandFailed { command: String, stderr: String },
    #[error("could not parse GitHub CLI output: {0}")]
    Parse(String),
    #[error("test fixture has no response for this command")]
    FixtureExhausted,
    #[error("fixture error: {0}")]
    Fixture(String),
}

#[derive(Clone, Debug)]
pub struct GitHubClient<E = ProcessGhExecutor> {
    executor: E,
}

impl GitHubClient<ProcessGhExecutor> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            executor: ProcessGhExecutor,
        }
    }
}

impl Default for GitHubClient<ProcessGhExecutor> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: GhExecutor> GitHubClient<E> {
    #[must_use]
    pub fn with_executor(executor: E) -> Self {
        Self { executor }
    }

    pub fn authentication_status(&self) -> GitHubAuthStatus {
        let version = match self.execute(GhCommand::new(["--version"])) {
            Ok(output) if output.success => output.stdout_trimmed(),
            Ok(output) => {
                return GitHubAuthStatus {
                    gh_available: false,
                    authenticated: false,
                    account: None,
                    diagnostic: diagnostic_text(&output),
                };
            }
            Err(error) => {
                return GitHubAuthStatus {
                    gh_available: false,
                    authenticated: false,
                    account: None,
                    diagnostic: unavailable_diagnostic(&error),
                };
            }
        };
        match self.execute(GhCommand::new([
            "auth",
            "status",
            "--hostname",
            "github.com",
        ])) {
            Ok(output) if output.success => {
                let rendered = format!("{}\n{}", output.stdout_trimmed(), output.stderr_trimmed());
                GitHubAuthStatus {
                    gh_available: true,
                    authenticated: true,
                    account: account_from_auth_status(&rendered),
                    diagnostic: version,
                }
            }
            Ok(output) => GitHubAuthStatus {
                gh_available: true,
                authenticated: false,
                account: None,
                diagnostic: format!(
                    "{} Run `gh auth login --hostname github.com` to sign in.",
                    diagnostic_text(&output)
                ),
            },
            Err(error) => GitHubAuthStatus {
                gh_available: true,
                authenticated: false,
                account: None,
                diagnostic: format!(
                    "{error}. Run `gh auth login --hostname github.com` to sign in."
                ),
            },
        }
    }

    pub(crate) fn execute(&self, command: GhCommand) -> Result<GhOutput, GhError> {
        self.executor.execute(&command)
    }

    pub(crate) fn require(&self, command: GhCommand) -> Result<GhOutput, GhError> {
        let display = command.display();
        let output = self.execute(command)?;
        if output.success {
            Ok(output)
        } else {
            Err(GhError::CommandFailed {
                command: display,
                stderr: diagnostic_text(&output),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubAuthStatus {
    pub gh_available: bool,
    pub authenticated: bool,
    pub account: Option<String>,
    /// User-safe diagnostics; it never includes stdin payloads or credentials.
    pub diagnostic: String,
}

fn account_from_auth_status(rendered: &str) -> Option<String> {
    rendered.lines().find_map(|line| {
        let (_, account) = line.split_once("account ")?;
        account
            .split_whitespace()
            .next()
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn diagnostic_text(output: &GhOutput) -> String {
    let text = if output.stderr_trimmed().is_empty() {
        output.stdout_trimmed()
    } else {
        output.stderr_trimmed()
    };
    truncate_diagnostic(&text)
}

fn unavailable_diagnostic(error: &GhError) -> String {
    match error {
        GhError::Spawn { source, .. } if source.kind() == io::ErrorKind::NotFound => {
            "GitHub CLI (`gh`) is not installed. Install it, then run `gh auth login --hostname github.com`.".to_owned()
        }
        _ => truncate_diagnostic(&error.to_string()),
    }
}

fn truncate_diagnostic(value: &str) -> String {
    const LIMIT: usize = 512;
    if value.len() <= LIMIT {
        value.to_owned()
    } else {
        let boundary = value
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= LIMIT)
            .last()
            .unwrap_or_default();
        format!("{}…", &value[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(success: bool, stdout: &str, stderr: &str) -> GhOutput {
        GhOutput {
            success,
            exit_code: success.then_some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn authentication_diagnostic_uses_gh_without_exposing_credentials() {
        let fixture = FixtureGhExecutor::with_outputs([
            output(true, "gh version 2.70.0\n", ""),
            output(
                true,
                "",
                "github.com\n  ✓ Logged in to github.com account octocat (keyring)\n",
            ),
        ]);
        let client = GitHubClient::with_executor(fixture.clone());
        let status = client.authentication_status();
        assert!(status.gh_available);
        assert!(status.authenticated);
        assert_eq!(status.account.as_deref(), Some("octocat"));
        assert_eq!(
            fixture.commands()[1].arguments,
            vec!["auth", "status", "--hostname", "github.com"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn fixture_records_stdin_without_a_shell_command() {
        let fixture = FixtureGhExecutor::with_outputs([output(true, "{}", "")]);
        let client = GitHubClient::with_executor(fixture.clone());
        client
            .require(
                GhCommand::new(["api", "--method", "POST", "repos/a/b"])
                    .with_stdin(b"{\"body\":\"review\"}".to_vec()),
            )
            .unwrap();
        let command = fixture.commands().pop().unwrap();
        assert_eq!(command.stdin, Some(b"{\"body\":\"review\"}".to_vec()));
        assert!(!command.display().contains("review\""));
    }

    #[test]
    fn diagnostics_truncate_only_at_utf8_boundaries() {
        let diagnostic = "é".repeat(400);
        let truncated = truncate_diagnostic(&diagnostic);
        assert!(truncated.ends_with('…'));
        assert!(truncated.len() <= 515);
    }

    #[test]
    fn bounded_output_reader_drains_without_retaining_unbounded_provider_output() {
        let (retained, exceeded) = drain_limited(std::io::Cursor::new(b"abcdef"), 3).unwrap();
        assert_eq!(retained, b"abc");
        assert!(exceeded);
    }
}
