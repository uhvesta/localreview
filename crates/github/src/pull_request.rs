use std::{path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use localreview_domain::GitSha;
use localreview_git::{PinnedGitRevision, PrepareManagedWorktreeRequest, RepositoryLocator};
use serde::Deserialize;
use thiserror::Error;

use crate::{GhCommand, GhError, GhExecutor, GitHubClient};

pub const GITHUB_DOT_COM: &str = "github.com";

/// A GitHub.com PR URL normalized to its canonical review identity. Common
/// browser-copy variants such as `/files`, query strings, fragments, and a
/// trailing slash are accepted, while alternate hosts and non-PR paths remain
/// rejected at the provider boundary.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitHubPullRequestUrl {
    pub owner: String,
    pub repository: String,
    pub number: u64,
}

impl GitHubPullRequestUrl {
    #[must_use]
    pub fn canonical_url(&self) -> String {
        format!(
            "https://{}/{}/{}/pull/{}",
            GITHUB_DOT_COM, self.owner, self.repository, self.number
        )
    }

    #[must_use]
    pub fn repository_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repository)
    }

    pub fn repository_locator(&self) -> Result<RepositoryLocator, PullRequestError> {
        RepositoryLocator::new(GITHUB_DOT_COM, &self.owner, &self.repository)
            .map_err(PullRequestError::InvalidRepositoryLocator)
    }

    #[must_use]
    pub fn clone_url(&self) -> String {
        format!(
            "https://{}/{}/{}.git",
            GITHUB_DOT_COM, self.owner, self.repository
        )
    }
}

impl FromStr for GitHubPullRequestUrl {
    type Err = PullRequestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 2_048 || value.contains('\0') {
            return Err(PullRequestError::InvalidUrl(value.to_owned()));
        }
        let value = value.trim();
        let Some(path) = value.strip_prefix("https://github.com/") else {
            return Err(PullRequestError::InvalidUrl(value.to_owned()));
        };
        let path = path
            .split_once(['?', '#'])
            .map_or(path, |(path, _)| path)
            .trim_end_matches('/');
        let parts = path.split('/').collect::<Vec<_>>();
        let [owner, repository, "pull", number, ..] = parts.as_slice() else {
            return Err(PullRequestError::InvalidUrl(value.to_owned()));
        };
        if !github_owner_segment(owner) || !github_repository_segment(repository) {
            return Err(PullRequestError::InvalidUrl(value.to_owned()));
        }
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| PullRequestError::InvalidUrl(value.to_owned()))?;
        Ok(Self {
            owner: (*owner).to_owned(),
            repository: (*repository).to_owned(),
            number,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestMetadata {
    pub url: GitHubPullRequestUrl,
    pub title: String,
    pub author: Option<String>,
    pub base: PullRequestRef,
    pub head: PullRequestRef,
    pub draft: bool,
    pub state: String,
    pub review_decision: Option<String>,
    pub commits: Vec<PullRequestCommit>,
}

impl PullRequestMetadata {
    #[must_use]
    pub fn pinned_revision(&self) -> PinnedGitRevision {
        PinnedGitRevision {
            base_sha: self.base.sha.clone(),
            head_sha: self.head.sha.clone(),
        }
    }

    #[must_use]
    pub fn head_changed_since(&self, pinned: &PinnedGitRevision) -> bool {
        self.head.sha != pinned.head_sha
    }

    pub fn managed_worktree_request(
        &self,
        review_id: impl Into<String>,
        known_clones: Vec<PathBuf>,
    ) -> Result<PrepareManagedWorktreeRequest, PullRequestError> {
        Ok(PrepareManagedWorktreeRequest {
            review_id: review_id.into(),
            repository: self.url.repository_locator()?,
            clone_url: self.url.clone_url(),
            revision: self.pinned_revision(),
            known_clones,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestRef {
    pub name: String,
    pub sha: GitSha,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestCommit {
    pub oid: GitSha,
    pub message_headline: String,
    pub authored_at: Option<DateTime<Utc>>,
}

impl<E: GhExecutor> GitHubClient<E> {
    /// Resolves metadata only at an explicit open or refresh boundary. Its
    /// result holds exact base/head OIDs for later pool preparation and remains
    /// pinned until the caller requests this method again.
    pub fn pull_request_metadata(
        &self,
        url: &GitHubPullRequestUrl,
    ) -> Result<PullRequestMetadata, PullRequestError> {
        let command = GhCommand::new([
            "pr",
            "view",
            &url.number.to_string(),
            "--repo",
            &url.repository_slug(),
            "--json",
            "number,title,url,author,baseRefName,baseRefOid,headRefName,headRefOid,isDraft,state,reviewDecision,commits",
        ]);
        let output = match self.require(command) {
            Ok(output) => output,
            Err(GhError::CommandFailed { stderr, .. })
                if stderr.contains("Unknown JSON field")
                    && (stderr.contains("baseRefOid") || stderr.contains("headRefOid")) =>
            {
                // Older gh releases do not expose the OID fields through
                // `pr view --json`. The stable REST representation still
                // provides exact base/head SHAs, so fall back without asking
                // the user to replace an otherwise authenticated gh install.
                return self.pull_request_metadata_via_api(url);
            }
            Err(error) => return Err(error.into()),
        };
        let raw = serde_json::from_slice::<RawPullRequest>(&output.stdout)
            .map_err(PullRequestError::MetadataDecode)?;
        raw.into_metadata(url)
    }

    fn pull_request_metadata_via_api(
        &self,
        url: &GitHubPullRequestUrl,
    ) -> Result<PullRequestMetadata, PullRequestError> {
        let endpoint = format!("repos/{}/pulls/{}", url.repository_slug(), url.number);
        let output = self.require(GhCommand::new(["api", "--method", "GET", &endpoint]))?;
        let raw = serde_json::from_slice::<RawApiPullRequest>(&output.stdout)
            .map_err(PullRequestError::MetadataDecode)?;
        raw.into_metadata(url)
    }
}

#[derive(Debug, Deserialize)]
struct RawApiPullRequest {
    number: u64,
    title: String,
    html_url: String,
    user: Option<RawUser>,
    base: RawApiRef,
    head: RawApiRef,
    #[serde(default)]
    draft: bool,
    state: String,
}

#[derive(Debug, Deserialize)]
struct RawApiRef {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

impl RawApiPullRequest {
    fn into_metadata(
        self,
        requested_url: &GitHubPullRequestUrl,
    ) -> Result<PullRequestMetadata, PullRequestError> {
        let returned_url = GitHubPullRequestUrl::from_str(&self.html_url)?;
        if self.number != requested_url.number || returned_url != *requested_url {
            return Err(PullRequestError::MetadataUrlMismatch {
                requested: requested_url.canonical_url(),
                returned: self.html_url,
            });
        }
        Ok(PullRequestMetadata {
            url: requested_url.clone(),
            title: self.title,
            author: self.user.map(|author| author.login),
            base: PullRequestRef {
                name: self.base.name,
                sha: parse_sha(&self.base.sha)?,
            },
            head: PullRequestRef {
                name: self.head.name,
                sha: parse_sha(&self.head.sha)?,
            },
            draft: self.draft,
            state: self.state.to_ascii_uppercase(),
            review_decision: None,
            // The compatibility path intentionally avoids a third unbounded
            // provider call. Commit context remains available from the pinned
            // local Git range after the managed worktree is prepared.
            commits: Vec::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPullRequest {
    number: u64,
    title: String,
    url: String,
    author: Option<RawUser>,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_name: String,
    head_ref_oid: String,
    is_draft: bool,
    state: String,
    review_decision: Option<String>,
    #[serde(default)]
    commits: Vec<RawCommit>,
}

#[derive(Debug, Deserialize)]
struct RawUser {
    login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCommit {
    oid: String,
    #[serde(default)]
    message_headline: String,
    #[serde(default)]
    authored_date: Option<DateTime<Utc>>,
}

impl RawPullRequest {
    fn into_metadata(
        self,
        requested_url: &GitHubPullRequestUrl,
    ) -> Result<PullRequestMetadata, PullRequestError> {
        let returned_url = GitHubPullRequestUrl::from_str(&self.url)?;
        if self.number != requested_url.number || returned_url != *requested_url {
            return Err(PullRequestError::MetadataUrlMismatch {
                requested: requested_url.canonical_url(),
                returned: self.url,
            });
        }
        Ok(PullRequestMetadata {
            url: requested_url.clone(),
            title: self.title,
            author: self.author.map(|author| author.login),
            base: PullRequestRef {
                name: self.base_ref_name,
                sha: parse_sha(&self.base_ref_oid)?,
            },
            head: PullRequestRef {
                name: self.head_ref_name,
                sha: parse_sha(&self.head_ref_oid)?,
            },
            draft: self.is_draft,
            state: self.state,
            review_decision: self.review_decision,
            commits: self
                .commits
                .into_iter()
                .map(|commit| {
                    Ok(PullRequestCommit {
                        oid: parse_sha(&commit.oid)?,
                        message_headline: commit.message_headline,
                        authored_at: commit.authored_date,
                    })
                })
                .collect::<Result<Vec<_>, PullRequestError>>()?,
        })
    }
}

#[derive(Debug, Error)]
pub enum PullRequestError {
    #[error(
        "GitHub pull request URL must be https://github.com/<owner>/<repo>/pull/<number>: {0}"
    )]
    InvalidUrl(String),
    #[error("GitHub CLI error: {0}")]
    Gh(#[from] GhError),
    #[error("could not decode GitHub pull request metadata: {0}")]
    MetadataDecode(#[source] serde_json::Error),
    #[error("GitHub returned metadata for a different pull request: requested {requested}, returned {returned}")]
    MetadataUrlMismatch { requested: String, returned: String },
    #[error("GitHub returned an invalid commit SHA: {0}")]
    InvalidSha(String),
    #[error("invalid managed repository locator: {0}")]
    InvalidRepositoryLocator(#[source] localreview_git::RepositoryPoolError),
}

fn parse_sha(value: &str) -> Result<GitSha, PullRequestError> {
    GitSha::new(value).map_err(|_| PullRequestError::InvalidSha(value.to_owned()))
}

fn github_owner_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn github_repository_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{FixtureGhExecutor, GhOutput};

    use super::*;

    fn fixture_output(body: &str) -> GhOutput {
        GhOutput {
            success: true,
            exit_code: Some(0),
            stdout: body.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn url_parser_normalizes_common_github_browser_links() {
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/42").unwrap();
        assert_eq!(url.owner, "octo");
        for pasted in [
            "https://github.com/octo/repo/pull/42/",
            " https://github.com/octo/repo/pull/42/files ",
            "https://github.com/octo/repo/pull/42?diff=split",
            "https://github.com/octo/repo/pull/42/files#discussion_r123",
        ] {
            assert_eq!(
                GitHubPullRequestUrl::from_str(pasted)
                    .unwrap()
                    .canonical_url(),
                url.canonical_url()
            );
        }
        assert!(GitHubPullRequestUrl::from_str("https://github.com/octo/repo/issues/42").is_err());
        assert!(
            GitHubPullRequestUrl::from_str("https://github.example/octo/repo/pull/42").is_err()
        );
        assert!(GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull").is_err());
    }

    #[test]
    fn metadata_resolution_uses_typed_gh_arguments_and_pins_oids() {
        let base = "0123456789abcdef0123456789abcdef01234567";
        let head = "89abcdef0123456789abcdef0123456789abcdef";
        let fixture = FixtureGhExecutor::with_outputs([fixture_output(&format!(
            r#"{{"number":42,"title":"Review","url":"https://github.com/octo/repo/pull/42","author":{{"login":"octocat"}},"baseRefName":"main","baseRefOid":"{base}","headRefName":"feature","headRefOid":"{head}","isDraft":false,"state":"OPEN","reviewDecision":null,"commits":[{{"oid":"{head}","messageHeadline":"change","authoredDate":"2026-07-21T12:00:00Z"}}]}}"#
        ))]);
        let client = GitHubClient::with_executor(fixture.clone());
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/42").unwrap();
        let metadata = client.pull_request_metadata(&url).unwrap();
        assert_eq!(metadata.pinned_revision().head_sha.as_str(), head);
        assert_eq!(metadata.commits.len(), 1);
        let command = fixture.commands().pop().unwrap();
        assert_eq!(command.arguments[0], "pr");
        assert_eq!(command.arguments[1], "view");
        assert!(command.stdin.is_none());
    }

    #[test]
    fn metadata_resolution_falls_back_to_stable_api_for_older_gh() {
        let base = "0123456789abcdef0123456789abcdef01234567";
        let head = "89abcdef0123456789abcdef0123456789abcdef";
        let fixture = FixtureGhExecutor::with_outputs([
            GhOutput {
                success: false,
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"Unknown JSON field: \"baseRefOid\"".to_vec(),
            },
            fixture_output(&format!(
                r#"{{"number":42,"title":"Review","html_url":"https://github.com/octo/repo/pull/42","user":{{"login":"octocat"}},"base":{{"ref":"main","sha":"{base}"}},"head":{{"ref":"feature","sha":"{head}"}},"draft":false,"state":"open"}}"#
            )),
        ]);
        let client = GitHubClient::with_executor(fixture.clone());
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/42").unwrap();
        let metadata = client.pull_request_metadata(&url).unwrap();
        assert_eq!(metadata.base.sha.as_str(), base);
        assert_eq!(metadata.head.sha.as_str(), head);
        assert_eq!(metadata.state, "OPEN");
        let commands = fixture.commands();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[1].arguments[0], "api");
        assert!(commands.iter().all(|command| command.stdin.is_none()));
    }
}
