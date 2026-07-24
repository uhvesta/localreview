//! Deterministic discovery for external tools used by LocalReview.
//!
//! GUI applications launched by macOS Launch Services inherit a deliberately
//! small `PATH`, which commonly omits Homebrew and MacPorts. Resolve tools to
//! absolute paths before spawning them while retaining normal `PATH` behavior
//! for shells, Linux desktops, and custom installations.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
};

pub const GH_PATH_ENV: &str = "LOCALREVIEW_GH_PATH";
pub const GIT_PATH_ENV: &str = "LOCALREVIEW_GIT_PATH";
pub const RG_PATH_ENV: &str = "LOCALREVIEW_RG_PATH";

/// Resolve the Git executable used for repository discovery, capture, and
/// managed worktrees.
#[must_use]
pub fn git_executable() -> PathBuf {
    resolve_executable(
        "git",
        env::var_os(GIT_PATH_ENV),
        env::var_os("PATH"),
        git_well_known_paths(),
    )
}

/// Resolve the GitHub CLI used by the compatibility GitHub provider.
#[must_use]
pub fn gh_executable() -> PathBuf {
    resolve_executable(
        "gh",
        env::var_os(GH_PATH_ENV),
        env::var_os("PATH"),
        gh_well_known_paths(),
    )
}

/// Resolve ripgrep for lazy codebase symbol discovery. The caller retains a
/// Git-backed fallback when ripgrep is not installed.
#[must_use]
pub fn rg_executable() -> PathBuf {
    resolve_executable(
        "rg",
        env::var_os(RG_PATH_ENV),
        env::var_os("PATH"),
        rg_well_known_paths(),
    )
}

fn resolve_executable(
    name: &str,
    explicit_path: Option<OsString>,
    path_value: Option<OsString>,
    well_known_paths: &[&str],
) -> PathBuf {
    // An explicit override is authoritative even when it is currently
    // invalid. Returning it lets the spawn error identify the configured path
    // instead of silently selecting a different installation.
    if let Some(explicit_path) = explicit_path.filter(|value| !value.is_empty()) {
        return PathBuf::from(explicit_path);
    }

    if let Some(path) = executable_on_path(name, path_value.as_deref()) {
        return path;
    }

    if let Some(path) = well_known_paths
        .iter()
        .map(Path::new)
        .find(|path| is_usable_executable(path))
    {
        return path.to_path_buf();
    }

    // Preserve std::process::Command's normal error and PATH behavior if the
    // installation appears after resolution or uses an unexpected location.
    PathBuf::from(name)
}

fn executable_on_path(name: &str, path_value: Option<&OsStr>) -> Option<PathBuf> {
    let path_value = path_value?;
    env::split_paths(path_value)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(name))
        .find(|path| is_usable_executable(path))
}

fn is_usable_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
fn git_well_known_paths() -> &'static [&'static str] {
    &[
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
        "/opt/local/bin/git",
    ]
}

#[cfg(target_os = "linux")]
fn git_well_known_paths() -> &'static [&'static str] {
    &[
        "/usr/bin/git",
        "/usr/local/bin/git",
        "/bin/git",
        "/home/linuxbrew/.linuxbrew/bin/git",
        "/run/current-system/sw/bin/git",
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn git_well_known_paths() -> &'static [&'static str] {
    &[]
}

#[cfg(target_os = "macos")]
fn gh_well_known_paths() -> &'static [&'static str] {
    &[
        "/opt/homebrew/bin/gh",
        "/usr/local/bin/gh",
        "/opt/local/bin/gh",
    ]
}

#[cfg(target_os = "macos")]
fn rg_well_known_paths() -> &'static [&'static str] {
    &[
        "/opt/homebrew/bin/rg",
        "/usr/local/bin/rg",
        "/opt/local/bin/rg",
    ]
}

#[cfg(target_os = "linux")]
fn rg_well_known_paths() -> &'static [&'static str] {
    &[
        "/usr/bin/rg",
        "/usr/local/bin/rg",
        "/home/linuxbrew/.linuxbrew/bin/rg",
        "/run/current-system/sw/bin/rg",
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rg_well_known_paths() -> &'static [&'static str] {
    &[]
}

#[cfg(target_os = "linux")]
fn gh_well_known_paths() -> &'static [&'static str] {
    &[
        "/usr/bin/gh",
        "/usr/local/bin/gh",
        "/snap/bin/gh",
        "/home/linuxbrew/.linuxbrew/bin/gh",
        "/run/current-system/sw/bin/gh",
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn gh_well_known_paths() -> &'static [&'static str] {
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(path: &Path) {
        fs::write(path, b"executable").unwrap();
    }

    #[test]
    fn explicit_override_is_authoritative() {
        let configured = PathBuf::from("/configured/tools/gh");
        assert_eq!(
            resolve_executable("gh", Some(configured.clone().into_os_string()), None, &[]),
            configured
        );
    }

    #[test]
    fn path_installation_precedes_well_known_fallback() {
        let temporary = tempfile::tempdir().unwrap();
        let path_directory = temporary.path().join("path");
        let fallback_directory = temporary.path().join("fallback");
        fs::create_dir_all(&path_directory).unwrap();
        fs::create_dir_all(&fallback_directory).unwrap();
        let path_tool = path_directory.join("gh");
        let fallback_tool = fallback_directory.join("gh");
        make_executable(&path_tool);
        make_executable(&fallback_tool);
        let path_value = env::join_paths([&path_directory]).unwrap();
        let fallback = fallback_tool.to_string_lossy().into_owned();

        assert_eq!(
            resolve_executable("gh", None, Some(path_value), &[fallback.as_str()]),
            path_tool
        );
    }

    #[test]
    fn well_known_path_repairs_a_minimal_desktop_path() {
        let temporary = tempfile::tempdir().unwrap();
        let minimal_path = temporary.path().join("system-bin");
        fs::create_dir_all(&minimal_path).unwrap();
        let homebrew = temporary.path().join("homebrew-gh");
        make_executable(&homebrew);
        let path_value = env::join_paths([&minimal_path]).unwrap();
        let homebrew = homebrew.to_string_lossy().into_owned();

        assert_eq!(
            resolve_executable("gh", None, Some(path_value), &[homebrew.as_str()]),
            PathBuf::from(homebrew)
        );
    }

    #[test]
    fn non_executable_candidates_are_ignored() {
        let temporary = tempfile::tempdir().unwrap();
        let candidate = temporary.path().join("gh");
        fs::write(&candidate, b"not executable").unwrap();
        let candidate = candidate.to_string_lossy().into_owned();

        assert_eq!(
            resolve_executable("gh", None, None, &[candidate.as_str()]),
            PathBuf::from("gh")
        );
    }
}
