//! Read-only support for shareable workspace configuration.

use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use localreview_domain::{BaseReference, StoredPath};
use localreview_git::DiscoveryConfig;
use serde::Deserialize;
use thiserror::Error;

const CONFIG_FILE_NAME: &str = ".localreview.toml";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceFileConfig {
    pub default_base: Option<BaseReference>,
    pub discovery_depth: Option<usize>,
    pub excluded_relative_prefixes: Vec<PathBuf>,
    pub repositories: BTreeMap<StoredPath, RepositoryFileConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryFileConfig {
    pub base: Option<BaseReference>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Error)]
pub enum WorkspaceConfigError {
    #[error("could not read workspace configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse workspace configuration {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("workspace configuration contains an invalid base reference for {field}: {reason}")]
    InvalidBase { field: String, reason: String },
    #[error("workspace configuration contains an unsafe relative path for {field}: {value}")]
    UnsafePath { field: String, value: String },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    workspace: RawWorkspace,
    #[serde(default)]
    repositories: BTreeMap<String, RawRepository>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    default_base: Option<String>,
    discovery_depth: Option<usize>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepository {
    base: Option<String>,
    enabled: Option<bool>,
}

impl WorkspaceFileConfig {
    pub fn load(root: &Path) -> Result<Option<Self>, WorkspaceConfigError> {
        let path = root.join(CONFIG_FILE_NAME);
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(WorkspaceConfigError::Read { path, source }),
        };
        let raw =
            toml::from_str::<RawConfig>(&source).map_err(|source| WorkspaceConfigError::Parse {
                path: path.clone(),
                source,
            })?;
        Self::try_from_raw(raw).map(Some)
    }

    pub fn apply_discovery(&self, discovery: &mut DiscoveryConfig) {
        if let Some(depth) = self.discovery_depth {
            discovery.max_depth = depth;
        }
        discovery
            .excluded_relative_prefixes
            .extend(self.excluded_relative_prefixes.iter().cloned());
        discovery.excluded_relative_prefixes.sort();
        discovery.excluded_relative_prefixes.dedup();
    }

    fn try_from_raw(raw: RawConfig) -> Result<Self, WorkspaceConfigError> {
        let default_base = raw
            .workspace
            .default_base
            .map(|value| parse_base("workspace.default_base", value))
            .transpose()?;
        let excluded_relative_prefixes = raw
            .workspace
            .exclude
            .into_iter()
            .enumerate()
            .map(|(index, value)| parse_excluded_prefix(index, value))
            .collect::<Result<Vec<_>, _>>()?;
        let repositories = raw
            .repositories
            .into_iter()
            .map(|(path, repository)| {
                let normalized = normalize_relative_path(
                    &format!("repositories.{path}"),
                    path.trim_end_matches("/**"),
                )?;
                let base = repository
                    .base
                    .map(|value| parse_base(&format!("repositories.{path}.base"), value))
                    .transpose()?;
                Ok((
                    StoredPath::new(&normalized),
                    RepositoryFileConfig {
                        base,
                        enabled: repository.enabled,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>, WorkspaceConfigError>>()?;
        Ok(Self {
            default_base,
            discovery_depth: raw.workspace.discovery_depth,
            excluded_relative_prefixes,
            repositories,
        })
    }
}

fn parse_base(field: &str, value: String) -> Result<BaseReference, WorkspaceConfigError> {
    BaseReference::new(value).map_err(|error| WorkspaceConfigError::InvalidBase {
        field: field.to_owned(),
        reason: error.to_string(),
    })
}

fn parse_excluded_prefix(index: usize, value: String) -> Result<PathBuf, WorkspaceConfigError> {
    // Discovery works on directory prefixes. A conventional trailing glob is
    // equivalent to the directory itself; other wildcard syntax would imply a
    // matcher the discovery model does not provide, so reject it explicitly.
    let prefix = value.strip_suffix("/**").unwrap_or(&value);
    if prefix
        .chars()
        .any(|character| matches!(character, '*' | '?' | '[' | ']'))
    {
        return Err(WorkspaceConfigError::UnsafePath {
            field: format!("workspace.exclude[{index}]"),
            value,
        });
    }
    normalize_relative_path(&format!("workspace.exclude[{index}]"), prefix)
}

fn normalize_relative_path(field: &str, value: &str) -> Result<PathBuf, WorkspaceConfigError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(WorkspaceConfigError::UnsafePath {
            field: field.to_owned(),
            value: value.to_owned(),
        });
    }
    let normalized = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<PathBuf>();
    if normalized.as_os_str().is_empty() && value != "." {
        return Err(WorkspaceConfigError::UnsafePath {
            field: field.to_owned(),
            value: value.to_owned(),
        });
    }
    Ok(if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn reads_documented_workspace_configuration_without_modifying_it() {
        let root = TempDir::new().unwrap();
        let source = r#"
[workspace]
default_base = "origin/master"
discovery_depth = 7
exclude = ["vendor/**", "generated/**"]

[repositories."b"]
base = "origin/HOTFIX-1"

[repositories."experimental/large-repo"]
enabled = false
"#;
        let path = root.path().join(CONFIG_FILE_NAME);
        fs::write(&path, source).unwrap();
        let config = WorkspaceFileConfig::load(root.path()).unwrap().unwrap();
        assert_eq!(config.default_base.unwrap().as_str(), "origin/master");
        assert_eq!(config.discovery_depth, Some(7));
        assert_eq!(
            config.excluded_relative_prefixes,
            vec![PathBuf::from("vendor"), PathBuf::from("generated")]
        );
        assert_eq!(
            config.repositories[&StoredPath::from("b")]
                .base
                .as_ref()
                .unwrap()
                .as_str(),
            "origin/HOTFIX-1"
        );
        assert_eq!(
            config.repositories[&StoredPath::from("experimental/large-repo")].enabled,
            Some(false)
        );
        assert_eq!(fs::read_to_string(path).unwrap(), source);
    }

    #[test]
    fn rejects_workspace_escape_and_unsupported_globs() {
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(CONFIG_FILE_NAME),
            "[workspace]\nexclude = [\"../outside/**\"]\n",
        )
        .unwrap();
        assert!(matches!(
            WorkspaceFileConfig::load(root.path()),
            Err(WorkspaceConfigError::UnsafePath { .. })
        ));

        fs::write(
            root.path().join(CONFIG_FILE_NAME),
            "[workspace]\nexclude = [\"vendor/*/cache\"]\n",
        )
        .unwrap();
        assert!(matches!(
            WorkspaceFileConfig::load(root.path()),
            Err(WorkspaceConfigError::UnsafePath { .. })
        ));
    }
}
