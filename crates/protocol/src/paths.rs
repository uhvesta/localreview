use crate::{InstallationSecret, ProtocolError, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl AppPaths {
    /// The path convention is intentionally shared by the Tauri app and CLI,
    /// rather than delegating to two different platform abstraction crates.
    pub fn discover() -> Result<Self, ProtocolError> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| ProtocolError::InvalidInput("HOME is not available".into()))?;
        // Test and portable-install override shared by the desktop process,
        // forwarding CLI, and companion.  It is intentionally the data root,
        // not merely a database override, so IPC secrets cannot diverge.
        let data_override = env::var_os("LOCALREVIEW_DATA_DIR").map(PathBuf::from);
        let data_dir = if let Some(override_root) = &data_override {
            override_root.clone()
        } else if cfg!(target_os = "macos") {
            home.join("Library/Application Support/LocalReview")
        } else if let Some(xdg_data) = env::var_os("XDG_DATA_HOME") {
            PathBuf::from(xdg_data).join("localreview")
        } else {
            home.join(".local/share/localreview")
        };
        // A portable/test data override remains hermetic unless callers
        // explicitly select a separate config root. Normal installations use
        // each OS's conventional per-user configuration location.
        let config_dir = if let Some(override_root) = env::var_os("LOCALREVIEW_CONFIG_DIR") {
            PathBuf::from(override_root)
        } else if data_override.is_some() {
            data_dir.clone()
        } else if cfg!(target_os = "macos") {
            home.join("Library/Application Support/LocalReview")
        } else if let Some(xdg_config) = env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg_config).join("localreview")
        } else {
            home.join(".config/localreview")
        };

        let runtime_base = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(env::temp_dir);
        // Keep the Unix-socket directory short while separating users whose
        // platform temp directory is shared. The actual directory is created
        // 0700, and the deterministic suffix is derived from the per-user data
        // location rather than an environment-provided username.
        let runtime_dir =
            runtime_base.join(format!("localreview-{:016x}", stable_path_hash(&data_dir)));
        Ok(Self {
            data_dir,
            config_dir,
            runtime_dir,
        })
    }

    pub fn with_roots(data_dir: PathBuf, runtime_dir: PathBuf) -> Self {
        Self {
            config_dir: data_dir.clone(),
            data_dir,
            runtime_dir,
        }
    }

    pub fn global_config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn secret_path(&self) -> PathBuf {
        self.data_dir.join("ipc-secret")
    }

    pub fn runtime_record_path(&self) -> PathBuf {
        self.data_dir.join("desktop-runtime.json")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join("desktop.sock")
    }

    pub fn ensure_private_directories(&self) -> Result<(), ProtocolError> {
        create_private_directory(&self.data_dir)?;
        create_private_directory(&self.runtime_dir)?;
        Ok(())
    }

    pub fn load_or_create_secret<F>(&self, generate: F) -> Result<InstallationSecret, ProtocolError>
    where
        F: FnOnce() -> [u8; InstallationSecret::LEN],
    {
        self.ensure_private_directories()?;
        let path = self.secret_path();
        if path.exists() {
            return read_secret(&path);
        }
        let secret = InstallationSecret::from_bytes(generate());
        match write_private_new_file(&path, secret.to_hex().as_bytes()) {
            Ok(()) => Ok(secret),
            Err(error) if path.exists() => read_secret(&path),
            Err(error) => Err(error),
        }
    }

    pub fn load_secret(&self) -> Result<InstallationSecret, ProtocolError> {
        read_secret(&self.secret_path())
    }

    pub fn write_runtime_record(&self, record: &RuntimeRecord) -> Result<(), ProtocolError> {
        self.ensure_private_directories()?;
        let bytes = serde_json::to_vec(record)
            .map_err(|error| ProtocolError::MalformedFrame(error.to_string()))?;
        write_private_atomic(&self.runtime_record_path(), &bytes)
    }

    pub fn read_runtime_record(&self) -> Result<RuntimeRecord, ProtocolError> {
        let bytes = fs::read(self.runtime_record_path())?;
        let record: RuntimeRecord = serde_json::from_slice(&bytes)
            .map_err(|error| ProtocolError::MalformedFrame(error.to_string()))?;
        if record.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion {
                received: record.protocol_version,
                supported: PROTOCOL_VERSION,
            });
        }
        if record.socket_path.as_os_str().is_empty() {
            return Err(ProtocolError::InvalidInput(
                "runtime record does not contain a socket path".into(),
            ));
        }
        Ok(record)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeRecord {
    pub protocol_version: u16,
    pub socket_path: PathBuf,
    pub process_id: u32,
    pub started_at_unix_secs: u64,
}

impl RuntimeRecord {
    pub fn current(socket_path: PathBuf) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            socket_path,
            process_id: std::process::id(),
            started_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

fn read_secret(path: &Path) -> Result<InstallationSecret, ProtocolError> {
    assert_private_file(path)?;
    let mut raw = String::new();
    File::open(path)?.read_to_string(&mut raw)?;
    InstallationSecret::from_hex(raw.trim())
}

fn write_private_new_file(path: &Path, contents: &[u8]) -> Result<(), ProtocolError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    assert_private_file(path)
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<(), ProtocolError> {
    let parent = path.parent().ok_or_else(|| {
        ProtocolError::InvalidInput("runtime record does not have a parent directory".into())
    })?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    write_private_new_file(&temporary, contents)?;
    fs::rename(&temporary, path)?;
    assert_private_file(path)
}

fn create_private_directory(path: &Path) -> Result<(), ProtocolError> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn assert_private_file(path: &Path) -> Result<(), ProtocolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ProtocolError::InvalidInput(format!(
                "{} must not be accessible to other users",
                path.display()
            )));
        }
    }
    Ok(())
}

fn stable_path_hash(path: &Path) -> u64 {
    // FNV-1a is used solely to keep a Unix-socket path short; it is not an
    // authentication primitive or a security decision.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn secret_and_runtime_record_are_separate_private_files() {
        let temporary = tempdir().unwrap();
        let paths = AppPaths::with_roots(
            temporary.path().join("data"),
            temporary.path().join("runtime"),
        );
        assert_eq!(
            paths.global_config_path(),
            temporary.path().join("data/config.toml")
        );
        let secret = paths.load_or_create_secret(|| [9; 32]).unwrap();
        assert_eq!(secret, paths.load_secret().unwrap());

        let record = RuntimeRecord::current(paths.socket_path());
        paths.write_runtime_record(&record).unwrap();
        assert_eq!(record, paths.read_runtime_record().unwrap());
        assert!(!std::fs::read_to_string(paths.runtime_record_path())
            .unwrap()
            .contains(&secret.to_hex()));
    }
}
