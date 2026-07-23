use std::{ffi::OsString, str::FromStr};

use chrono::{DateTime, Utc};
use localreview_domain::{Annotation, ComparisonId, DiffSide, GitSha, StoredPath};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{GhCommand, GhError, GhExecutor, GitHubClient, GitHubPullRequestUrl};

const MAX_IMPORTED_ITEMS: usize = 10_000;
const REVIEW_THREADS_QUERY: &str = r#"
query LocalReviewThreads($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        nodes {
          id isResolved isOutdated path line originalLine startLine originalStartLine diffSide
          comments(first: 100) {
            nodes {
              id body url createdAt updatedAt author { login }
              pullRequestReview { id state author { login } }
            }
            pageInfo { hasNextPage endCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

/// Thread comments are a separately paginated connection.  A PR can have a
/// single thread with more than 100 replies, so importing only the nested
/// first page silently loses review context.
const THREAD_COMMENTS_QUERY: &str = r#"
query LocalReviewThreadComments($id: ID!, $after: String) {
  node(id: $id) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $after) {
        nodes {
          id body url createdAt updatedAt author { login }
          pullRequestReview { id state author { login } }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

/// Existing GitHub state is deliberately separate from local annotations so
/// the caller can render remote threads and unpublished review work distinctly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedPullRequestState {
    pub threads: Vec<ImportedReviewThread>,
    pub conversation: Vec<ImportedConversationComment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedReviewThread {
    pub id: String,
    pub resolved: bool,
    pub outdated: bool,
    pub path: Option<StoredPath>,
    pub line: Option<u32>,
    pub original_line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub side: Option<GitHubLineSide>,
    pub comments: Vec<ImportedReviewComment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedReviewComment {
    pub id: String,
    pub body_markdown: String,
    pub author: Option<String>,
    pub url: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub review: Option<ImportedReviewIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedReviewIdentity {
    pub id: String,
    pub state: Option<String>,
    pub author: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedConversationComment {
    pub id: u64,
    pub body_markdown: String,
    pub author: Option<String>,
    pub url: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl<E: GhExecutor> GitHubClient<E> {
    /// Imports all review-thread pages and issue conversation pages. Existing
    /// review thread state retains GitHub's resolved and outdated flags rather
    /// than trying to infer them from comments or local diffs.
    pub fn import_pull_request_state(
        &self,
        url: &GitHubPullRequestUrl,
    ) -> Result<ImportedPullRequestState, ReviewImportError> {
        Ok(ImportedPullRequestState {
            threads: self.import_review_threads(url)?,
            conversation: self.import_conversation(url)?,
        })
    }

    pub fn import_review_threads(
        &self,
        url: &GitHubPullRequestUrl,
    ) -> Result<Vec<ImportedReviewThread>, ReviewImportError> {
        let mut all_threads = Vec::new();
        let mut after = None::<String>;
        loop {
            let mut arguments: Vec<OsString> = vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={REVIEW_THREADS_QUERY}").into(),
                "-F".into(),
                format!("owner={}", url.owner).into(),
                "-F".into(),
                format!("name={}", url.repository).into(),
                "-F".into(),
                format!("number={}", url.number).into(),
            ];
            if let Some(cursor) = &after {
                arguments.push("-F".into());
                arguments.push(format!("after={cursor}").into());
            }
            let output = self.require(GhCommand::new(arguments))?;
            let page = serde_json::from_slice::<RawThreadPage>(&output.stdout)
                .map_err(ReviewImportError::ThreadDecode)?;
            let raw_threads = page
                .data
                .repository
                .and_then(|repository| repository.pull_request)
                .ok_or(ReviewImportError::PullRequestNotFound)?
                .review_threads;
            for raw_thread in raw_threads.nodes {
                let has_more_comments = raw_thread.comments.page_info.has_next_page;
                let comment_cursor = raw_thread.comments.page_info.end_cursor.clone();
                let mut thread = ImportedReviewThread::from(raw_thread);
                if has_more_comments {
                    let cursor =
                        comment_cursor.ok_or(ReviewImportError::MissingPaginationCursor)?;
                    thread
                        .comments
                        .extend(self.import_thread_comment_pages(&thread.id, cursor)?);
                }
                all_threads.push(thread);
            }
            if all_threads.len() > MAX_IMPORTED_ITEMS {
                return Err(ReviewImportError::PaginationLimitExceeded);
            }
            if !raw_threads.page_info.has_next_page {
                break;
            }
            after = raw_threads.page_info.end_cursor;
            if after.is_none() {
                return Err(ReviewImportError::MissingPaginationCursor);
            }
        }
        Ok(all_threads)
    }

    fn import_thread_comment_pages(
        &self,
        thread_id: &str,
        mut after: String,
    ) -> Result<Vec<ImportedReviewComment>, ReviewImportError> {
        let mut comments = Vec::new();
        loop {
            let output = self.require(GhCommand::new([
                "api",
                "graphql",
                "-f",
                &format!("query={THREAD_COMMENTS_QUERY}"),
                "-F",
                &format!("id={thread_id}"),
                "-F",
                &format!("after={after}"),
            ]))?;
            let page = serde_json::from_slice::<RawThreadCommentPage>(&output.stdout)
                .map_err(ReviewImportError::ThreadDecode)?;
            let connection = page
                .data
                .node
                .ok_or(ReviewImportError::PullRequestNotFound)?
                .comments;
            comments.extend(
                connection
                    .nodes
                    .into_iter()
                    .map(ImportedReviewComment::from),
            );
            if comments.len() > MAX_IMPORTED_ITEMS {
                return Err(ReviewImportError::PaginationLimitExceeded);
            }
            if !connection.page_info.has_next_page {
                return Ok(comments);
            }
            after = connection
                .page_info
                .end_cursor
                .ok_or(ReviewImportError::MissingPaginationCursor)?;
        }
    }

    pub fn import_conversation(
        &self,
        url: &GitHubPullRequestUrl,
    ) -> Result<Vec<ImportedConversationComment>, ReviewImportError> {
        let endpoint = format!(
            "repos/{}/issues/{}/comments",
            url.repository_slug(),
            url.number
        );
        let output = self.require(GhCommand::new([
            "api",
            "--hostname",
            "github.com",
            "--paginate",
            endpoint.as_str(),
        ]))?;
        // `gh api --paginate` writes one JSON array per response page. Parse
        // the concatenated JSON value stream directly; this works on old gh
        // versions that predate `--slurp` and remains bounded by GhExecutor.
        let comments = decode_paginated_arrays::<RawConversationComment>(&output.stdout)
            .map_err(ReviewImportError::ConversationDecode)?
            .into_iter()
            .map(ImportedConversationComment::from)
            .collect::<Vec<_>>();
        if comments.len() > MAX_IMPORTED_ITEMS {
            return Err(ReviewImportError::PaginationLimitExceeded);
        }
        Ok(comments)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum GitHubLineSide {
    Left,
    Right,
}

impl GitHubLineSide {
    #[must_use]
    pub fn from_diff_side(side: DiffSide) -> Self {
        match side {
            DiffSide::Old => Self::Left,
            DiffSide::New => Self::Right,
        }
    }
}

impl FromStr for GitHubLineSide {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "LEFT" => Ok(Self::Left),
            "RIGHT" => Ok(Self::Right),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawThreadPage {
    data: RawThreadData,
}

#[derive(Debug, Deserialize)]
struct RawThreadData {
    repository: Option<RawThreadRepository>,
}

#[derive(Debug, Deserialize)]
struct RawThreadCommentPage {
    data: RawThreadCommentData,
}

#[derive(Debug, Deserialize)]
struct RawThreadCommentData {
    node: Option<RawThreadCommentNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThreadCommentNode {
    comments: RawReviewCommentConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThreadRepository {
    pull_request: Option<RawPullRequestThreads>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPullRequestThreads {
    review_threads: RawThreadConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThreadConnection {
    #[serde(default)]
    nodes: Vec<RawReviewThread>,
    page_info: RawPageInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewThread {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    path: Option<String>,
    line: Option<u32>,
    original_line: Option<u32>,
    start_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: Option<String>,
    comments: RawReviewCommentConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewCommentConnection {
    #[serde(default)]
    nodes: Vec<RawReviewComment>,
    #[serde(default)]
    page_info: RawPageInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewComment {
    id: String,
    body: String,
    url: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    author: Option<RawLogin>,
    pull_request_review: Option<RawReviewIdentity>,
}

#[derive(Debug, Deserialize)]
struct RawLogin {
    login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReviewIdentity {
    id: String,
    state: Option<String>,
    author: Option<RawLogin>,
}

impl From<RawReviewThread> for ImportedReviewThread {
    fn from(thread: RawReviewThread) -> Self {
        Self {
            id: thread.id,
            resolved: thread.is_resolved,
            outdated: thread.is_outdated,
            path: thread.path.map(StoredPath::from),
            line: thread.line,
            original_line: thread.original_line,
            start_line: thread.start_line,
            original_start_line: thread.original_start_line,
            side: thread
                .diff_side
                .as_deref()
                .and_then(|value| GitHubLineSide::from_str(value).ok()),
            comments: thread
                .comments
                .nodes
                .into_iter()
                .map(ImportedReviewComment::from)
                .collect(),
        }
    }
}

impl From<RawReviewComment> for ImportedReviewComment {
    fn from(comment: RawReviewComment) -> Self {
        Self {
            id: comment.id,
            body_markdown: comment.body,
            author: comment.author.map(|author| author.login),
            url: comment.url,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
            review: comment
                .pull_request_review
                .map(|review| ImportedReviewIdentity {
                    id: review.id,
                    state: review.state,
                    author: review.author.map(|author| author.login),
                }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConversationComment {
    id: u64,
    body: String,
    html_url: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    user: Option<RawLogin>,
}

impl From<RawConversationComment> for ImportedConversationComment {
    fn from(comment: RawConversationComment) -> Self {
        Self {
            id: comment.id,
            body_markdown: comment.body,
            author: comment.user.map(|user| user.login),
            url: comment.html_url,
            created_at: comment.created_at,
            updated_at: comment.updated_at,
        }
    }
}

#[derive(Debug, Error)]
pub enum ReviewImportError {
    #[error("GitHub CLI error: {0}")]
    Gh(#[from] GhError),
    #[error("could not decode GitHub review thread response: {0}")]
    ThreadDecode(#[source] serde_json::Error),
    #[error("could not decode GitHub conversation response: {0}")]
    ConversationDecode(#[source] serde_json::Error),
    #[error("GitHub pull request was not found")]
    PullRequestNotFound,
    #[error("GitHub pagination returned a next page without a cursor")]
    MissingPaginationCursor,
    #[error("GitHub response exceeds the configured import limit")]
    PaginationLimitExceeded,
}

/// The native GitHub review conclusion selected in the explicit Finish Review
/// flow. No conclusion is inferred from annotation severity or body text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewConclusion {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeReviewDraft {
    /// Exact PR head captured for this review. GitHub rejects anchors against a
    /// different commit instead of racing against a moving pull request head.
    pub pinned_head_sha: GitSha,
    /// A durable, unique identifier generated and persisted before the first
    /// POST. Reusing this value is what makes crash reconciliation safe;
    /// intentionally identical later reviews must use a new value.
    pub publication_attempt_id: String,
    pub conclusion: ReviewConclusion,
    pub summary_markdown: Option<String>,
    pub comments: Vec<NativeReviewComment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeReviewComment {
    pub body_markdown: String,
    pub path: StoredPath,
    pub line: u32,
    pub side: GitHubLineSide,
    pub start_line: Option<u32>,
    pub start_side: Option<GitHubLineSide>,
}

impl NativeReviewComment {
    /// Converts a local side-aware annotation into a GitHub line anchor. File
    /// notes and review notes deliberately fail because GitHub cannot represent
    /// them as inline review comments.
    pub fn from_annotation(
        annotation: &Annotation,
        expected_comparison_id: ComparisonId,
    ) -> Result<Self, ReviewPublishError> {
        let anchor =
            annotation
                .anchor
                .as_ref()
                .ok_or(ReviewPublishError::UnrepresentableAnnotation {
                    annotation_id: annotation.id.to_string(),
                })?;
        if anchor.outdated {
            return Err(ReviewPublishError::OutdatedAnnotation {
                annotation_id: annotation.id.to_string(),
            });
        }
        if anchor.comparison_id != expected_comparison_id {
            return Err(ReviewPublishError::ComparisonMismatch {
                annotation_id: annotation.id.to_string(),
            });
        }
        let side = anchor
            .side
            .map(GitHubLineSide::from_diff_side)
            .ok_or_else(|| ReviewPublishError::UnrepresentableAnnotation {
                annotation_id: annotation.id.to_string(),
            })?;
        let line =
            anchor
                .end_line
                .ok_or_else(|| ReviewPublishError::UnrepresentableAnnotation {
                    annotation_id: annotation.id.to_string(),
                })?;
        Ok(Self {
            body_markdown: annotation.body_markdown.clone(),
            path: anchor.file_path.clone(),
            line,
            side,
            start_line: anchor.start_line.filter(|start| *start < line),
            start_side: anchor
                .start_line
                .filter(|start| *start < line)
                .map(|_| side),
        })
    }

    fn validate(&self) -> Result<(), ReviewPublishError> {
        if self.body_markdown.trim().is_empty() {
            return Err(ReviewPublishError::EmptyCommentBody);
        }
        validate_github_path(&self.path)?;
        if self.line == 0 {
            return Err(ReviewPublishError::InvalidLineRange);
        }
        match (self.start_line, self.start_side) {
            (None, None) => Ok(()),
            (Some(start), Some(_)) if start > 0 && start < self.line => Ok(()),
            _ => Err(ReviewPublishError::InvalidLineRange),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedNativeReview {
    pub publication_attempt_id: String,
    pub pinned_head_sha: GitSha,
    pub request_fingerprint: String,
    pub reconciliation_marker: String,
    /// Exact native GitHub JSON, including the hidden reconciliation marker,
    /// suitable for an explicit submission preview.
    pub payload_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedNativeReview {
    pub id: u64,
    pub html_url: Option<String>,
    pub state: Option<String>,
    pub request_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciledNativeReview {
    pub id: u64,
    pub html_url: Option<String>,
    pub state: Option<String>,
    pub request_fingerprint: String,
}

impl NativeReviewDraft {
    /// Builds the exact payload before publication. A deterministic hidden
    /// marker lets a caller reconcile an ambiguous timeout before retrying, so
    /// LocalReview never blindly posts a second batched review.
    pub fn prepare(&self) -> Result<PreparedNativeReview, ReviewPublishError> {
        if self.comments.is_empty()
            && self
                .summary_markdown
                .as_deref()
                .unwrap_or(" ")
                .trim()
                .is_empty()
        {
            return Err(ReviewPublishError::EmptyReview);
        }
        for comment in &self.comments {
            comment.validate()?;
        }
        if !valid_publication_attempt_id(&self.publication_attempt_id) {
            return Err(ReviewPublishError::InvalidPublicationAttemptId);
        }
        let intent = ReviewIntent::from(self);
        let intent_json = serde_json::to_vec(&intent).map_err(ReviewPublishError::Encode)?;
        let request_fingerprint = hex_digest(&intent_json);
        let reconciliation_marker = format!("<!-- localreview-review:{request_fingerprint} -->");
        let summary = self.summary_markdown.as_deref().unwrap_or("").trim_end();
        let body = if summary.is_empty() {
            reconciliation_marker.clone()
        } else {
            format!("{summary}\n\n{reconciliation_marker}")
        };
        let payload = ReviewPayload {
            body,
            event: self.conclusion,
            commit_id: self.pinned_head_sha.as_str().to_owned(),
            comments: self
                .comments
                .iter()
                .map(ReviewPayloadComment::from)
                .collect(),
        };
        let payload_json = serde_json::to_string(&payload).map_err(ReviewPublishError::Encode)?;
        Ok(PreparedNativeReview {
            publication_attempt_id: self.publication_attempt_id.clone(),
            pinned_head_sha: self.pinned_head_sha.clone(),
            request_fingerprint,
            reconciliation_marker,
            payload_json,
        })
    }
}

impl<E: GhExecutor> GitHubClient<E> {
    /// Performs exactly one GitHub native review creation request. It never
    /// retries a transport error: callers must use `reconcile_native_review`
    /// with this prepared fingerprint before offering another submission.
    pub fn submit_native_review(
        &self,
        url: &GitHubPullRequestUrl,
        prepared: &PreparedNativeReview,
    ) -> Result<SubmittedNativeReview, ReviewPublishError> {
        let endpoint = format!(
            "repos/{}/pulls/{}/reviews",
            url.repository_slug(),
            url.number
        );
        let output = self.require(
            GhCommand::new([
                "api",
                "--hostname",
                "github.com",
                "--method",
                "POST",
                endpoint.as_str(),
                "--input",
                "-",
            ])
            .with_stdin(prepared.payload_json.as_bytes().to_vec()),
        )?;
        let response = serde_json::from_slice::<RawSubmittedReview>(&output.stdout)
            .map_err(ReviewPublishError::ResponseDecode)?;
        Ok(SubmittedNativeReview {
            id: response.id,
            html_url: response.html_url,
            state: response.state,
            request_fingerprint: prepared.request_fingerprint.clone(),
        })
    }

    /// Searches native review records for the marker from an interrupted or
    /// timed-out submission. A service stores the fingerprint before posting,
    /// then invokes this method before any retry to avoid duplicates.
    pub fn reconcile_native_review(
        &self,
        url: &GitHubPullRequestUrl,
        request_fingerprint: &str,
    ) -> Result<Option<ReconciledNativeReview>, ReviewPublishError> {
        if !valid_fingerprint(request_fingerprint) {
            return Err(ReviewPublishError::InvalidFingerprint);
        }
        let endpoint = format!(
            "repos/{}/pulls/{}/reviews",
            url.repository_slug(),
            url.number
        );
        let output = self.require(GhCommand::new([
            "api",
            "--hostname",
            "github.com",
            "--paginate",
            endpoint.as_str(),
        ]))?;
        let marker = format!("<!-- localreview-review:{request_fingerprint} -->");
        if let Some(review) = decode_paginated_arrays::<RawSubmittedReview>(&output.stdout)
            .map_err(ReviewPublishError::ResponseDecode)?
            .into_iter()
            .find(|review| {
                review
                    .body
                    .as_deref()
                    .is_some_and(|body| body.contains(&marker))
            })
        {
            return Ok(Some(ReconciledNativeReview {
                id: review.id,
                html_url: review.html_url,
                state: review.state,
                request_fingerprint: request_fingerprint.to_owned(),
            }));
        }
        Ok(None)
    }
}

#[derive(Serialize)]
struct ReviewIntent {
    pinned_head_sha: String,
    publication_attempt_id: String,
    conclusion: ReviewConclusion,
    summary_markdown: Option<String>,
    comments: Vec<ReviewIntentComment>,
}

impl From<&NativeReviewDraft> for ReviewIntent {
    fn from(draft: &NativeReviewDraft) -> Self {
        Self {
            pinned_head_sha: draft.pinned_head_sha.as_str().to_owned(),
            publication_attempt_id: draft.publication_attempt_id.clone(),
            conclusion: draft.conclusion,
            summary_markdown: draft.summary_markdown.clone(),
            comments: draft
                .comments
                .iter()
                .map(ReviewIntentComment::from)
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct ReviewIntentComment {
    body_markdown: String,
    path: String,
    line: u32,
    side: GitHubLineSide,
    start_line: Option<u32>,
    start_side: Option<GitHubLineSide>,
}

impl From<&NativeReviewComment> for ReviewIntentComment {
    fn from(comment: &NativeReviewComment) -> Self {
        Self {
            body_markdown: comment.body_markdown.clone(),
            path: comment.path.as_str().to_owned(),
            line: comment.line,
            side: comment.side,
            start_line: comment.start_line,
            start_side: comment.start_side,
        }
    }
}

#[derive(Serialize)]
struct ReviewPayload {
    body: String,
    event: ReviewConclusion,
    commit_id: String,
    comments: Vec<ReviewPayloadComment>,
}

#[derive(Serialize)]
struct ReviewPayloadComment {
    body: String,
    path: String,
    line: u32,
    side: GitHubLineSide,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_side: Option<GitHubLineSide>,
}

impl From<&NativeReviewComment> for ReviewPayloadComment {
    fn from(comment: &NativeReviewComment) -> Self {
        Self {
            body: comment.body_markdown.clone(),
            path: comment.path.as_str().to_owned(),
            line: comment.line,
            side: comment.side,
            start_line: comment.start_line,
            start_side: comment.start_side,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawSubmittedReview {
    id: u64,
    html_url: Option<String>,
    state: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// Decodes both the value stream emitted by `gh api --paginate` and the
/// nested array emitted by newer `gh api --paginate --slurp`. Accepting both
/// keeps persisted fixture/reconciliation behavior compatible across gh
/// upgrades while production no longer requires the newer `--slurp` flag.
fn decode_paginated_arrays<T: DeserializeOwned>(bytes: &[u8]) -> Result<Vec<T>, serde_json::Error> {
    let mut output = Vec::new();
    for value in serde_json::Deserializer::from_slice(bytes).into_iter::<serde_json::Value>() {
        let value = value?;
        let nested = value
            .as_array()
            .and_then(|values| values.first())
            .is_some_and(serde_json::Value::is_array);
        if nested {
            output.extend(
                serde_json::from_value::<Vec<Vec<T>>>(value)?
                    .into_iter()
                    .flatten(),
            );
        } else {
            output.extend(serde_json::from_value::<Vec<T>>(value)?);
        }
    }
    Ok(output)
}

#[derive(Debug, Error)]
pub enum ReviewPublishError {
    #[error("GitHub CLI error: {0}")]
    Gh(#[from] GhError),
    #[error("review has no summary or inline comments")]
    EmptyReview,
    #[error("inline review comments require non-empty Markdown")]
    EmptyCommentBody,
    #[error("GitHub inline comment paths must be safe, relative repository paths: {0}")]
    InvalidPath(StoredPath),
    #[error("GitHub inline comment ranges must be positive and start before the final line")]
    InvalidLineRange,
    #[error("annotation {annotation_id} cannot be represented as a GitHub inline comment")]
    UnrepresentableAnnotation { annotation_id: String },
    #[error("annotation {annotation_id} is outdated and must be re-anchored before publication")]
    OutdatedAnnotation { annotation_id: String },
    #[error("annotation {annotation_id} does not belong to the pinned comparison")]
    ComparisonMismatch { annotation_id: String },
    #[error("annotation {annotation_id} is outside the pinned pull-request diff; keep it local-only or move it into a displayed diff hunk")]
    AnchorOutsidePullRequestDiff { annotation_id: String },
    #[error("publication attempt id must be a durable UUID-like identifier")]
    InvalidPublicationAttemptId,
    #[error("could not encode native GitHub review payload: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("could not decode native GitHub review response: {0}")]
    ResponseDecode(#[source] serde_json::Error),
    #[error("invalid native review reconciliation fingerprint")]
    InvalidFingerprint,
}

impl ReviewPublishError {
    /// A completed HTTP 4xx response proves GitHub rejected the request and
    /// did not create a review. Transport failures, timeouts, HTTP 5xx, and
    /// request-timeout responses remain ambiguous and must be reconciled.
    #[must_use]
    pub fn is_definitive_provider_rejection(&self) -> bool {
        matches!(
            self,
            Self::Gh(GhError::CommandFailed { stderr, .. })
                if definitive_http_client_status(stderr).is_some()
        )
    }
}

fn definitive_http_client_status(diagnostic: &str) -> Option<u16> {
    diagnostic
        .split(|character: char| !character.is_ascii_alphanumeric())
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|parts| {
            if !parts[0].eq_ignore_ascii_case("http") {
                return None;
            }
            let status = parts[1].parse::<u16>().ok()?;
            ((400..500).contains(&status) && status != 408).then_some(status)
        })
}

fn validate_github_path(path: &StoredPath) -> Result<(), ReviewPublishError> {
    let raw = path.as_str();
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.contains('\0')
        // GitHub's review API always addresses repository paths with `/`.
        // On Unix a Windows drive path otherwise looks relative and could
        // leak an app/cache location into a malformed publication payload.
        || raw.contains('\\')
        || raw
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        Err(ReviewPublishError::InvalidPath(path.clone()))
    } else {
        Ok(())
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_fingerprint(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_publication_attempt_id(value: &str) -> bool {
    (16..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::{FixtureGhExecutor, GhOutput};

    use super::*;

    fn output(body: &str) -> GhOutput {
        GhOutput {
            success: true,
            exit_code: Some(0),
            stdout: body.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn imports_resolved_and_outdated_threads_separately_from_conversation() {
        let thread_page = r#"{
          "data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{
            "id":"T1","isResolved":true,"isOutdated":true,"path":"src/lib.rs",
            "line":12,"originalLine":10,"startLine":11,"originalStartLine":9,"diffSide":"RIGHT",
            "comments":{"nodes":[{"id":"C1","body":"remote note","url":"https://example.test/c","createdAt":"2026-07-21T12:00:00Z","updatedAt":null,"author":{"login":"octocat"},"pullRequestReview":{"id":"R1","state":"COMMENTED","author":{"login":"octocat"}}}]}
          }],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}
        }"#;
        let conversation_pages = r#"[{"id":5,"body":"general discussion","html_url":"https://example.test/i","created_at":"2026-07-21T12:00:00Z","updated_at":null,"user":{"login":"hubot"}}][{"id":6,"body":"second page","html_url":"https://example.test/i2","created_at":"2026-07-21T12:01:00Z","updated_at":null,"user":{"login":"octocat"}}]"#;
        let fixture =
            FixtureGhExecutor::with_outputs([output(thread_page), output(conversation_pages)]);
        let client = GitHubClient::with_executor(fixture.clone());
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/4").unwrap();
        let imported = client.import_pull_request_state(&url).unwrap();
        assert_eq!(imported.threads.len(), 1);
        assert!(imported.threads[0].resolved);
        assert!(imported.threads[0].outdated);
        assert_eq!(imported.threads[0].side, Some(GitHubLineSide::Right));
        assert_eq!(imported.conversation[0].author.as_deref(), Some("hubot"));
        assert_eq!(imported.conversation.len(), 2);
        assert_eq!(fixture.commands().len(), 2);
    }

    #[test]
    fn imports_every_page_of_a_large_nested_thread() {
        let first = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{
          "id":"T1","isResolved":false,"isOutdated":false,"path":"src/lib.rs","line":3,
          "originalLine":3,"startLine":null,"originalStartLine":null,"diffSide":"RIGHT",
          "comments":{"nodes":[{"id":"C1","body":"first","url":null,"createdAt":null,"updatedAt":null,"author":null,"pullRequestReview":null}],"pageInfo":{"hasNextPage":true,"endCursor":"cursor-1"}}
        }],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#;
        let next = r#"{"data":{"node":{"comments":{"nodes":[{"id":"C2","body":"second","url":null,"createdAt":null,"updatedAt":null,"author":null,"pullRequestReview":null}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}"#;
        let fixture = FixtureGhExecutor::with_outputs([output(first), output(next), output("[]")]);
        let client = GitHubClient::with_executor(fixture.clone());
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/4").unwrap();
        let imported = client.import_pull_request_state(&url).unwrap();
        assert_eq!(imported.threads[0].comments.len(), 2);
        assert_eq!(imported.threads[0].comments[1].id, "C2");
        assert_eq!(fixture.commands().len(), 3);
    }

    #[test]
    fn native_submission_is_one_batched_post_with_range_anchors_and_reconciliation_marker() {
        let fixture = FixtureGhExecutor::with_outputs([output(
            r#"{"id":99,"html_url":"https://github.com/octo/repo/pull/4#pullrequestreview-99","state":"COMMENTED","body":"ok"}"#,
        )]);
        let client = GitHubClient::with_executor(fixture.clone());
        let draft = NativeReviewDraft {
            pinned_head_sha: GitSha::new("1234567890abcdef1234567890abcdef12345678").unwrap(),
            publication_attempt_id: "018f6af0-8af2-7ad0-a98a-123456789abc".to_owned(),
            conclusion: ReviewConclusion::RequestChanges,
            summary_markdown: Some("Please address both findings.".to_owned()),
            comments: vec![
                NativeReviewComment {
                    body_markdown: "First finding".to_owned(),
                    path: StoredPath::from("src/lib.rs"),
                    line: 12,
                    side: GitHubLineSide::Right,
                    start_line: Some(10),
                    start_side: Some(GitHubLineSide::Right),
                },
                NativeReviewComment {
                    body_markdown: "Deletion finding".to_owned(),
                    path: StoredPath::from("src/old.rs"),
                    line: 8,
                    side: GitHubLineSide::Left,
                    start_line: None,
                    start_side: None,
                },
            ],
        };
        let prepared = draft.prepare().unwrap();
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/4").unwrap();
        let submitted = client.submit_native_review(&url, &prepared).unwrap();
        assert_eq!(submitted.id, 99);
        let commands = fixture.commands();
        assert_eq!(commands.len(), 1, "one native batched review post only");
        let command = &commands[0];
        assert!(command
            .arguments
            .windows(2)
            .any(|window| window == ["--method", "POST"]));
        let payload: serde_json::Value =
            serde_json::from_slice(command.stdin.as_ref().unwrap()).unwrap();
        assert_eq!(payload["event"], "REQUEST_CHANGES");
        assert_eq!(
            payload["commit_id"],
            "1234567890abcdef1234567890abcdef12345678"
        );
        assert_eq!(payload["comments"].as_array().unwrap().len(), 2);
        assert_eq!(payload["comments"][0]["start_line"], 10);
        assert_eq!(payload["comments"][0]["start_side"], "RIGHT");
        assert!(payload["body"]
            .as_str()
            .unwrap()
            .contains(&prepared.reconciliation_marker));
    }

    #[test]
    fn reconciliation_finds_a_previous_timeout_submission_before_retry() {
        let fingerprint = "a".repeat(64);
        let fixture = FixtureGhExecutor::with_outputs([output(&format!(
            r#"[{{"id":41,"html_url":"https://example.test/review/41","state":"COMMENTED","body":"summary\n\n<!-- localreview-review:{fingerprint} -->"}}]"#
        ))]);
        let client = GitHubClient::with_executor(fixture.clone());
        let url = GitHubPullRequestUrl::from_str("https://github.com/octo/repo/pull/4").unwrap();
        let found = client
            .reconcile_native_review(&url, &fingerprint)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, 41);
        assert_eq!(fixture.commands().len(), 1);
    }

    #[test]
    fn annotations_without_a_valid_line_anchor_are_not_silently_published() {
        let now = Utc::now();
        let annotation = Annotation {
            id: localreview_domain::AnnotationId::new(),
            annotation_set_id: localreview_domain::AnnotationSetId::new(),
            kind: localreview_domain::AnnotationKind::FileNote,
            state: localreview_domain::AnnotationState::Open,
            publication_state: localreview_domain::PublicationState::IncludedInNextReview,
            labels: Vec::new(),
            body_markdown: "needs a line".to_owned(),
            anchor: None,
            created_at: now,
            updated_at: now,
        };
        let comparison_id = ComparisonId::new();
        assert!(matches!(
            NativeReviewComment::from_annotation(&annotation, comparison_id),
            Err(ReviewPublishError::UnrepresentableAnnotation { .. })
        ));
    }

    #[test]
    fn native_payload_rejects_host_paths_and_encodes_old_side_ranges_exactly() {
        for path in [
            StoredPath::from("/private/var/tmp/review.rs"),
            StoredPath::from(r"C:\Users\review\src\lib.rs"),
            StoredPath::from("../src/lib.rs"),
        ] {
            let draft = NativeReviewDraft {
                pinned_head_sha: GitSha::new("1234567890abcdef1234567890abcdef12345678").unwrap(),
                publication_attempt_id: "018f6af0-8af2-7ad0-a98a-123456789abc".to_owned(),
                conclusion: ReviewConclusion::Comment,
                summary_markdown: None,
                comments: vec![NativeReviewComment {
                    body_markdown: "must stay repository-relative".to_owned(),
                    path,
                    line: 2,
                    side: GitHubLineSide::Right,
                    start_line: None,
                    start_side: None,
                }],
            };
            assert!(matches!(
                draft.prepare(),
                Err(ReviewPublishError::InvalidPath(_))
            ));
        }

        let draft = NativeReviewDraft {
            pinned_head_sha: GitSha::new("1234567890abcdef1234567890abcdef12345678").unwrap(),
            publication_attempt_id: "018f6af0-8af2-7ad0-a98a-abcdefabcdef".to_owned(),
            conclusion: ReviewConclusion::RequestChanges,
            summary_markdown: Some("Old-side range fixture".to_owned()),
            comments: vec![NativeReviewComment {
                body_markdown: "This deleted range needs another look.".to_owned(),
                path: StoredPath::from("src/removed.rs"),
                line: 8,
                side: GitHubLineSide::Left,
                start_line: Some(6),
                start_side: Some(GitHubLineSide::Left),
            }],
        };
        let payload: serde_json::Value =
            serde_json::from_str(&draft.prepare().unwrap().payload_json).unwrap();
        assert_eq!(payload["comments"][0]["path"], "src/removed.rs");
        assert_eq!(payload["comments"][0]["line"], 8);
        assert_eq!(payload["comments"][0]["side"], "LEFT");
        assert_eq!(payload["comments"][0]["start_line"], 6);
        assert_eq!(payload["comments"][0]["start_side"], "LEFT");
    }

    #[test]
    fn only_completed_http_client_rejections_are_definitive() {
        let rejection = ReviewPublishError::Gh(GhError::CommandFailed {
            command: "gh api --method POST repos/a/b/pulls/1/reviews".into(),
            stderr: "gh: Validation Failed (HTTP 422)".into(),
        });
        assert!(rejection.is_definitive_provider_rejection());

        for error in [
            ReviewPublishError::Gh(GhError::CommandFailed {
                command: "gh api".into(),
                stderr: "connection reset by peer".into(),
            }),
            ReviewPublishError::Gh(GhError::CommandFailed {
                command: "gh api".into(),
                stderr: "gateway failed (HTTP 502)".into(),
            }),
            ReviewPublishError::Gh(GhError::CommandFailed {
                command: "gh api".into(),
                stderr: "request timed out (HTTP 408)".into(),
            }),
        ] {
            assert!(!error.is_definitive_provider_rejection());
        }
    }

    #[test]
    fn identical_later_review_uses_a_distinct_durable_attempt_marker() {
        let base = NativeReviewDraft {
            pinned_head_sha: GitSha::new("1234567890abcdef1234567890abcdef12345678").unwrap(),
            publication_attempt_id: "018f6af0-8af2-7ad0-a98a-111111111111".to_owned(),
            conclusion: ReviewConclusion::Comment,
            summary_markdown: Some("Same review body".to_owned()),
            comments: Vec::new(),
        };
        let mut later = base.clone();
        later.publication_attempt_id = "018f6af0-8af2-7ad0-a98a-222222222222".to_owned();
        let first = base.prepare().unwrap();
        let second = later.prepare().unwrap();
        assert_ne!(first.request_fingerprint, second.request_fingerprint);
        assert_ne!(first.reconciliation_marker, second.reconciliation_marker);
    }

    #[test]
    fn outdated_or_different_comparison_annotations_are_rejected() {
        let comparison_id = ComparisonId::new();
        let other_comparison_id = ComparisonId::new();
        let now = Utc::now();
        let mut anchor =
            localreview_domain::AnnotationAnchor::from_line(localreview_domain::LineAnchorInput {
                comparison_id,
                repository_id: localreview_domain::RepositoryId::new(),
                file_path: StoredPath::from("src/lib.rs"),
                side: DiffSide::New,
                start_line: 1,
                end_line: 1,
                selected_source: "line".to_owned(),
                surrounding_context: "line".to_owned(),
            })
            .unwrap();
        let mut annotation = Annotation {
            id: localreview_domain::AnnotationId::new(),
            annotation_set_id: localreview_domain::AnnotationSetId::new(),
            kind: localreview_domain::AnnotationKind::Comment,
            state: localreview_domain::AnnotationState::Open,
            publication_state: localreview_domain::PublicationState::IncludedInNextReview,
            labels: Vec::new(),
            body_markdown: "finding".to_owned(),
            anchor: Some(anchor.clone()),
            created_at: now,
            updated_at: now,
        };
        assert!(matches!(
            NativeReviewComment::from_annotation(&annotation, other_comparison_id),
            Err(ReviewPublishError::ComparisonMismatch { .. })
        ));
        anchor.outdated = true;
        annotation.anchor = Some(anchor);
        assert!(matches!(
            NativeReviewComment::from_annotation(&annotation, comparison_id),
            Err(ReviewPublishError::OutdatedAnnotation { .. })
        ));
    }

    #[test]
    fn annotation_ranges_map_to_the_exact_github_side_contract() {
        let comparison_id = ComparisonId::new();
        let now = Utc::now();
        let annotation = Annotation {
            id: localreview_domain::AnnotationId::new(),
            annotation_set_id: localreview_domain::AnnotationSetId::new(),
            kind: localreview_domain::AnnotationKind::Comment,
            state: localreview_domain::AnnotationState::Open,
            publication_state: localreview_domain::PublicationState::IncludedInNextReview,
            labels: Vec::new(),
            body_markdown: "Review the removed range.".to_owned(),
            anchor: Some(
                localreview_domain::AnnotationAnchor::from_line(
                    localreview_domain::LineAnchorInput {
                        comparison_id,
                        repository_id: localreview_domain::RepositoryId::new(),
                        file_path: StoredPath::from("src/removed.rs"),
                        side: DiffSide::Old,
                        start_line: 6,
                        end_line: 8,
                        selected_source: "old six through eight".to_owned(),
                        surrounding_context: "old-side fixture".to_owned(),
                    },
                )
                .unwrap(),
            ),
            created_at: now,
            updated_at: now,
        };
        let comment = NativeReviewComment::from_annotation(&annotation, comparison_id).unwrap();
        assert_eq!(comment.path, StoredPath::from("src/removed.rs"));
        assert_eq!(comment.side, GitHubLineSide::Left);
        assert_eq!(comment.line, 8);
        assert_eq!(comment.start_line, Some(6));
        assert_eq!(comment.start_side, Some(GitHubLineSide::Left));
    }
}
