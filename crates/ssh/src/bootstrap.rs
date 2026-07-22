use crate::{
    RemoteAgentProgram, RemoteConnectionInfo, SshConnectionConfig, SshDestination, SshError,
    SshSession,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

/// The platform facts used to select a signed companion artifact. They are
/// discovered on the target host rather than guessed from a desktop SSH alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemotePlatform {
    pub operating_system: String,
    pub architecture: String,
}

/// Embedded Ed25519 public key for LocalReview companion release manifests.
/// It is a verification key only; private signing material is never shipped
/// in the desktop application.  Detached signatures cover the exact manifest
/// bytes received by `CompanionArtifact::from_signed_manifest`.
const COMPANION_MANIFEST_PUBLIC_KEY: [u8; 32] = [
    0xa5, 0x67, 0x7a, 0x75, 0xa8, 0x69, 0x2c, 0x07, 0x79, 0xda, 0xa1, 0xf4, 0x66, 0x83, 0x2e, 0x0b,
    0x60, 0xb2, 0xa0, 0x0d, 0x7c, 0xe3, 0xbe, 0x77, 0x65, 0x9e, 0xe7, 0xbf, 0x99, 0x0b, 0xa2, 0x28,
];
const COMPANION_MANIFEST_KEY_ID: &str = "localreview-release-2026-01";
const COMPANION_MANIFEST_DOMAIN: &[u8] = b"localreview-companion-manifest-v1\0";
const COMPANION_MANIFEST_SCHEMA: u16 = 1;
const MAX_MANIFEST_VALIDITY_SECONDS: u64 = 366 * 24 * 60 * 60;
const CLOCK_SKEW_SECONDS: u64 = 5 * 60;
const MAX_SIGNED_MANIFEST_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReleaseVersion(u64, u64, u64);

impl ReleaseVersion {
    fn parse(value: &str) -> Result<Self, BootstrapError> {
        let mut parts = value.split('.');
        let parsed = Self(
            parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or(BootstrapError::InvalidSignedManifest)?,
            parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or(BootstrapError::InvalidSignedManifest)?,
            parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or(BootstrapError::InvalidSignedManifest)?,
        );
        if parts.next().is_some() {
            return Err(BootstrapError::InvalidSignedManifest);
        }
        Ok(parsed)
    }
}

/// The detached JSON document signed by the release pipeline.  Its bytes are
/// signed verbatim, avoiding ambiguous reserialization/canonicalization rules.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignedCompanionManifest {
    schema_version: u16,
    product: String,
    signing_key_id: String,
    release_version: String,
    protocol_version: u16,
    channel: String,
    issued_at_unix_secs: u64,
    expires_at_unix_secs: u64,
    artifacts: Vec<SignedCompanionManifestArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignedCompanionManifestArtifact {
    operating_system: String,
    architecture: String,
    file_name: String,
    sha256_hex: String,
    byte_len: u64,
}

/// A local companion executable selected from an Ed25519-verified release
/// manifest.  Its fields are private to ensure a caller cannot turn an
/// arbitrary caller-supplied SHA-256 into a trusted bootstrap artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompanionArtifact {
    path: PathBuf,
    version: String,
    operating_system: String,
    architecture: String,
    sha256_hex: String,
    byte_len: u64,
}

impl CompanionArtifact {
    /// Verifies a detached release-manifest signature with the embedded
    /// release public key, selects exactly one platform artifact, and verifies
    /// the local file before a remote mutation can occur.
    pub fn from_signed_manifest(
        path: PathBuf,
        manifest_bytes: &[u8],
        signature_bytes: &[u8],
        operating_system: impl Into<String>,
        architecture: impl Into<String>,
    ) -> Result<Self, BootstrapError> {
        Self::from_signed_manifest_with_key_at(
            path,
            manifest_bytes,
            signature_bytes,
            operating_system.into(),
            architecture.into(),
            VerifyingKey::from_bytes(&COMPANION_MANIFEST_PUBLIC_KEY)
                .expect("embedded companion signing key is valid"),
            unix_seconds_now()?,
            ReleaseVersion::parse(env!("CARGO_PKG_VERSION"))
                .expect("crate package version is valid"),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_signed_manifest_with_key_at(
        path: PathBuf,
        manifest_bytes: &[u8],
        signature_bytes: &[u8],
        operating_system: String,
        architecture: String,
        verifying_key: VerifyingKey,
        now_unix_secs: u64,
        minimum_version: ReleaseVersion,
    ) -> Result<Self, BootstrapError> {
        if manifest_bytes.is_empty()
            || manifest_bytes.len() > MAX_SIGNED_MANIFEST_BYTES
            || signature_bytes.len() != 64
        {
            return Err(BootstrapError::InvalidSignedManifest);
        }
        let signature = Signature::from_slice(signature_bytes)
            .map_err(|_| BootstrapError::ManifestSignatureInvalid)?;
        let mut signed = Vec::with_capacity(COMPANION_MANIFEST_DOMAIN.len() + manifest_bytes.len());
        signed.extend_from_slice(COMPANION_MANIFEST_DOMAIN);
        signed.extend_from_slice(manifest_bytes);
        verifying_key
            .verify(&signed, &signature)
            .map_err(|_| BootstrapError::ManifestSignatureInvalid)?;
        let manifest: SignedCompanionManifest = serde_json::from_slice(manifest_bytes)
            .map_err(|_| BootstrapError::InvalidSignedManifest)?;
        let release_version = ReleaseVersion::parse(&manifest.release_version)?;
        if manifest.schema_version != COMPANION_MANIFEST_SCHEMA
            || manifest.product != "localreview-companion"
            || manifest.signing_key_id != COMPANION_MANIFEST_KEY_ID
            || manifest.protocol_version != localreview_protocol::PROTOCOL_VERSION
            || manifest.channel != "stable"
            || release_version < minimum_version
            || manifest.artifacts.is_empty()
            || manifest.artifacts.len() > 128
            || manifest.issued_at_unix_secs > now_unix_secs.saturating_add(CLOCK_SKEW_SECONDS)
            || manifest.expires_at_unix_secs < now_unix_secs
            || manifest.expires_at_unix_secs <= manifest.issued_at_unix_secs
            || manifest
                .expires_at_unix_secs
                .saturating_sub(manifest.issued_at_unix_secs)
                > MAX_MANIFEST_VALIDITY_SECONDS
        {
            return Err(BootstrapError::InvalidSignedManifest);
        }
        let mut unique_targets = std::collections::HashSet::new();
        for artifact in &manifest.artifacts {
            let target = (
                normalized_operating_system(&artifact.operating_system)?,
                normalized_architecture(&artifact.architecture)?,
            );
            if !unique_targets.insert(target)
                || artifact.operating_system
                    != normalized_operating_system(&artifact.operating_system)?
                || artifact.architecture != normalized_architecture(&artifact.architecture)?
                || artifact.byte_len == 0
                || artifact.sha256_hex.len() != 64
                || !artifact
                    .sha256_hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || artifact.sha256_hex != artifact.sha256_hex.to_ascii_lowercase()
                || !valid_artifact_file_name(&artifact.file_name)
            {
                return Err(BootstrapError::InvalidSignedManifest);
            }
        }
        let operating_system = normalized_operating_system(&operating_system)?;
        let architecture = normalized_architecture(&architecture)?;
        let matches = manifest
            .artifacts
            .iter()
            .filter(|artifact| {
                artifact.operating_system == operating_system
                    && artifact.architecture == architecture
            })
            .collect::<Vec<_>>();
        let [artifact] = matches.as_slice() else {
            return Err(BootstrapError::ArtifactNotInSignedManifest);
        };
        let artifact = (*artifact).clone();
        if path.file_name().and_then(|value| value.to_str()) != Some(&artifact.file_name) {
            return Err(BootstrapError::InvalidArtifact);
        }
        let candidate = Self {
            path,
            version: manifest.release_version,
            operating_system,
            architecture,
            sha256_hex: artifact.sha256_hex,
            byte_len: artifact.byte_len,
        };
        candidate.validate()?;
        Ok(candidate)
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn operating_system(&self) -> &str {
        &self.operating_system
    }

    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    fn validate(&self) -> Result<(), BootstrapError> {
        if self.version.is_empty()
            || self.operating_system.is_empty()
            || self.architecture.is_empty()
            || self.sha256_hex.len() != 64
            || !self.sha256_hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(BootstrapError::InvalidArtifact);
        }
        let metadata = std::fs::symlink_metadata(&self.path).map_err(BootstrapError::Io)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(BootstrapError::InvalidArtifact);
        }
        if metadata.len() != self.byte_len {
            return Err(BootstrapError::IntegrityMismatch);
        }
        let actual = sha256_file(&self.path)?;
        if !constant_time_equal(
            actual.as_bytes(),
            self.sha256_hex.to_ascii_lowercase().as_bytes(),
        ) {
            return Err(BootstrapError::IntegrityMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompanionProbe {
    Compatible {
        connection: RemoteConnectionInfo,
        platform: Option<RemotePlatform>,
    },
    Incompatible {
        platform: RemotePlatform,
        detail: String,
    },
    MissingOrUnreachable {
        platform: Option<RemotePlatform>,
        detail: String,
    },
}

/// Bootstrap uses only user-owned `.local/bin`, a verified artifact, and a
/// fixed command list. It never asks for sudo, alters global PATH/config, or
/// leaves a secret on the remote host.
#[derive(Clone, Debug)]
pub struct CompanionBootstrapper {
    pub destination: SshDestination,
    pub ssh_program: PathBuf,
    pub install_dir: String,
}

impl CompanionBootstrapper {
    #[must_use]
    pub fn new(destination: SshDestination) -> Self {
        Self {
            destination,
            ssh_program: PathBuf::from("ssh"),
            install_dir: ".local/bin".into(),
        }
    }

    pub fn probe(&self) -> CompanionProbe {
        let platform = self.probe_platform().ok();
        let mut config = SshConnectionConfig::new(self.destination.clone());
        config.ssh_program = self.ssh_program.clone().into_os_string();
        match SshSession::connect(config) {
            Ok(session) => {
                let connection = session.connection.clone();
                drop(session);
                CompanionProbe::Compatible {
                    connection,
                    platform,
                }
            }
            Err(SshError::VersionMismatch { .. }) => CompanionProbe::Incompatible {
                platform: platform.unwrap_or(RemotePlatform {
                    operating_system: "unknown".into(),
                    architecture: "unknown".into(),
                }),
                detail: "the existing LocalReview companion uses an incompatible protocol".into(),
            },
            Err(error) => CompanionProbe::MissingOrUnreachable {
                platform,
                detail: error.to_string(),
            },
        }
    }

    pub fn probe_platform(&self) -> Result<RemotePlatform, BootstrapError> {
        let operating_system = self.run_remote_fixed(["uname", "-s"])?;
        let architecture = self.run_remote_fixed(["uname", "-m"])?;
        let operating_system = normalized_platform_value(&operating_system)?;
        let architecture = normalized_platform_value(&architecture)?;
        let operating_system = normalized_operating_system(&operating_system)
            .map_err(|_| BootstrapError::Remote("remote operating system is unsupported".into()))?;
        let architecture = normalized_architecture(&architecture)
            .map_err(|_| BootstrapError::Remote("remote architecture is unsupported".into()))?;
        Ok(RemotePlatform {
            operating_system,
            architecture,
        })
    }

    /// Installs a verified artifact atomically into `~/.local/bin/localreview`.
    /// The temporary name is derived only from an already validated SHA-256 and
    /// every failure path makes a best-effort deletion of that exact temp file.
    pub fn install_user_local(&self, artifact: &CompanionArtifact) -> Result<(), BootstrapError> {
        artifact.validate()?;
        let platform = self.probe_platform()?;
        if platform.operating_system != artifact.operating_system
            || platform.architecture != artifact.architecture
        {
            return Err(BootstrapError::PlatformMismatch {
                expected_os: artifact.operating_system.clone(),
                expected_arch: artifact.architecture.clone(),
                actual_os: platform.operating_system,
                actual_arch: platform.architecture,
            });
        }
        validate_install_dir(&self.install_dir)?;
        let temporary_name = format!(".localreview-{}.tmp", &artifact.sha256_hex[..16]);
        let temporary_remote = format!("{}/{}", self.install_dir, temporary_name);
        let final_remote = format!("{}/localreview", self.install_dir);
        self.run_remote_fixed(["mkdir", "-p", self.install_dir.as_str()])?;
        self.run_remote_fixed(["chmod", "700", self.install_dir.as_str()])?;
        let result = (|| {
            self.copy_to_remote(&artifact.path, &temporary_remote)?;
            self.verify_remote_hash(&temporary_remote, &artifact.sha256_hex)?;
            self.run_remote_fixed(["chmod", "700", temporary_remote.as_str()])?;
            self.run_remote_fixed(["mv", "-f", temporary_remote.as_str(), final_remote.as_str()])?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.run_remote_fixed(["rm", "-f", temporary_remote.as_str()]);
        }
        result
    }

    pub fn connect_installed(&self) -> Result<SshSession, SshError> {
        let mut config = SshConnectionConfig::new(self.destination.clone());
        config.ssh_program = self.ssh_program.clone().into_os_string();
        config.remote_agent_program = RemoteAgentProgram::UserLocal;
        SshSession::connect(config)
    }

    fn copy_to_remote(&self, local: &Path, remote_path: &str) -> Result<(), BootstrapError> {
        let destination = format!("{}:{}", self.destination.as_str(), remote_path);
        let status = ProcessCommand::new("scp")
            .arg("-S")
            .arg(&self.ssh_program)
            .arg("-p")
            .arg(local)
            .arg(destination)
            .stdin(Stdio::null())
            .status()
            .map_err(BootstrapError::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(BootstrapError::Remote("scp transfer failed".into()))
        }
    }

    fn verify_remote_hash(&self, path: &str, expected: &str) -> Result<(), BootstrapError> {
        // Prefer the native Linux utility and fall back to macOS `shasum`.
        // Both command shapes are fixed and a host with neither is left for
        // manual installation rather than trusting an unverified transfer.
        let output = self
            .run_remote_fixed(["sha256sum", path])
            .or_else(|_| self.run_remote_fixed(["shasum", "-a", "256", path]))?;
        let actual = output.split_whitespace().next().unwrap_or_default();
        if actual.len() != 64 || !constant_time_equal(actual.as_bytes(), expected.as_bytes()) {
            return Err(BootstrapError::IntegrityMismatch);
        }
        Ok(())
    }

    fn run_remote_fixed<const N: usize>(
        &self,
        arguments: [&str; N],
    ) -> Result<String, BootstrapError> {
        let output = ProcessCommand::new(&self.ssh_program)
            .arg(self.destination.as_str())
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .map_err(BootstrapError::Io)?;
        if !output.status.success() {
            return Err(BootstrapError::Remote(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("remote bootstrap command failed")
                    .to_owned(),
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| BootstrapError::Remote("remote bootstrap output was not UTF-8".into()))
    }
}

fn sha256_file(path: &Path) -> Result<String, BootstrapError> {
    let mut file = File::open(path).map_err(BootstrapError::Io)?;
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 32 * 1024];
    loop {
        let count = file.read(&mut bytes).map_err(BootstrapError::Io)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn normalized_platform_value(value: &str) -> Result<String, BootstrapError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BootstrapError::Remote(
            "remote platform response was invalid".into(),
        ));
    }
    Ok(value)
}

fn normalized_manifest_component(value: &str) -> Result<String, BootstrapError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BootstrapError::InvalidSignedManifest);
    }
    Ok(value)
}

fn normalized_operating_system(value: &str) -> Result<String, BootstrapError> {
    match normalized_manifest_component(value)?.as_str() {
        "darwin" | "macos" => Ok("macos".into()),
        "linux" => Ok("linux".into()),
        _ => Err(BootstrapError::InvalidSignedManifest),
    }
}

fn normalized_architecture(value: &str) -> Result<String, BootstrapError> {
    match normalized_manifest_component(value)?.as_str() {
        "arm64" | "aarch64" => Ok("aarch64".into()),
        "amd64" | "x86_64" => Ok("x86_64".into()),
        _ => Err(BootstrapError::InvalidSignedManifest),
    }
}

fn valid_artifact_file_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value != "."
        && value != ".."
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn unix_seconds_now() -> Result<u64, BootstrapError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| BootstrapError::InvalidSignedManifest)
}

fn validate_install_dir(value: &str) -> Result<(), BootstrapError> {
    if value != ".local/bin" {
        return Err(BootstrapError::InvalidInstallDirectory);
    }
    Ok(())
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("companion artifact is invalid")]
    InvalidArtifact,
    #[error("companion release manifest is invalid")]
    InvalidSignedManifest,
    #[error("companion release manifest signature is invalid")]
    ManifestSignatureInvalid,
    #[error("the requested companion platform is absent or ambiguous in the signed manifest")]
    ArtifactNotInSignedManifest,
    #[error("companion artifact hash does not match its signed manifest")]
    IntegrityMismatch,
    #[error("remote platform mismatch: expected {expected_os}/{expected_arch}, got {actual_os}/{actual_arch}")]
    PlatformMismatch {
        expected_os: String,
        expected_arch: String,
        actual_os: String,
        actual_arch: String,
    },
    #[error("the companion install directory must be the user-local .local/bin")]
    InvalidInstallDirectory,
    #[error("remote bootstrap failed: {0}")]
    Remote(String),
    #[error("bootstrap I/O error: {0}")]
    Io(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::io::Write;

    #[test]
    fn artifact_integrity_and_platform_inputs_are_checked_before_remote_mutation() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("localreview");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"companion").unwrap();
        let hash = sha256_file(temporary.path().join("localreview").as_path()).unwrap();
        let manifest = format!(
            "{{\"schemaVersion\":1,\"product\":\"localreview-companion\",\"signingKeyId\":\"{COMPANION_MANIFEST_KEY_ID}\",\"releaseVersion\":\"1.2.3\",\"protocolVersion\":{},\"channel\":\"stable\",\"issuedAtUnixSecs\":900,\"expiresAtUnixSecs\":2000,\"artifacts\":[{{\"operatingSystem\":\"linux\",\"architecture\":\"x86_64\",\"fileName\":\"localreview\",\"sha256Hex\":\"{hash}\",\"byteLen\":9}}]}}",
            localreview_protocol::PROTOCOL_VERSION
        );
        let signing = SigningKey::from_bytes(&[9_u8; 32]);
        let mut signed = COMPANION_MANIFEST_DOMAIN.to_vec();
        signed.extend_from_slice(manifest.as_bytes());
        let signature = signing.sign(&signed).to_bytes();
        let artifact = CompanionArtifact::from_signed_manifest_with_key_at(
            path,
            manifest.as_bytes(),
            &signature,
            "linux".into(),
            "x86_64".into(),
            signing.verifying_key(),
            1_000,
            ReleaseVersion::parse("0.1.0").unwrap(),
        )
        .unwrap();
        assert_eq!(artifact.version(), "1.2.3");
        assert_eq!(artifact.operating_system(), "linux");
        assert_eq!(artifact.architecture(), "x86_64");
        let tampered = CompanionArtifact::from_signed_manifest_with_key_at(
            temporary.path().join("localreview"),
            b"{}",
            &signature,
            "linux".into(),
            "x86_64".into(),
            signing.verifying_key(),
            1_000,
            ReleaseVersion::parse("0.1.0").unwrap(),
        );
        assert!(matches!(
            tampered,
            Err(BootstrapError::ManifestSignatureInvalid)
        ));
        assert!(normalized_platform_value("Linux\n").is_ok());
        assert!(normalized_platform_value("bad value").is_err());
        assert!(validate_install_dir("/usr/local/bin").is_err());
    }

    #[test]
    fn signed_manifest_rejects_expiry_downgrade_and_cross_protocol_signatures() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("localreview");
        std::fs::write(&path, b"companion").unwrap();
        let hash = sha256_file(&path).unwrap();
        let manifest = format!(
            "{{\"schemaVersion\":1,\"product\":\"localreview-companion\",\"signingKeyId\":\"{COMPANION_MANIFEST_KEY_ID}\",\"releaseVersion\":\"0.1.0\",\"protocolVersion\":{},\"channel\":\"stable\",\"issuedAtUnixSecs\":100,\"expiresAtUnixSecs\":200,\"artifacts\":[{{\"operatingSystem\":\"macos\",\"architecture\":\"aarch64\",\"fileName\":\"localreview\",\"sha256Hex\":\"{hash}\",\"byteLen\":9}}]}}",
            localreview_protocol::PROTOCOL_VERSION
        );
        let signing = SigningKey::from_bytes(&[7_u8; 32]);
        let mut signed = COMPANION_MANIFEST_DOMAIN.to_vec();
        signed.extend_from_slice(manifest.as_bytes());
        let signature = signing.sign(&signed).to_bytes();
        assert!(matches!(
            CompanionArtifact::from_signed_manifest_with_key_at(
                path.clone(),
                manifest.as_bytes(),
                &signature,
                "Darwin".into(),
                "arm64".into(),
                signing.verifying_key(),
                300,
                ReleaseVersion::parse("0.1.0").unwrap(),
            ),
            Err(BootstrapError::InvalidSignedManifest)
        ));
        assert!(matches!(
            CompanionArtifact::from_signed_manifest_with_key_at(
                path.clone(),
                manifest.as_bytes(),
                &signature,
                "macos".into(),
                "aarch64".into(),
                signing.verifying_key(),
                150,
                ReleaseVersion::parse("1.0.0").unwrap(),
            ),
            Err(BootstrapError::InvalidSignedManifest)
        ));
        let raw_signature = signing.sign(manifest.as_bytes()).to_bytes();
        assert!(matches!(
            CompanionArtifact::from_signed_manifest_with_key_at(
                path,
                manifest.as_bytes(),
                &raw_signature,
                "macos".into(),
                "aarch64".into(),
                signing.verifying_key(),
                150,
                ReleaseVersion::parse("0.1.0").unwrap(),
            ),
            Err(BootstrapError::ManifestSignatureInvalid)
        ));
    }
}
