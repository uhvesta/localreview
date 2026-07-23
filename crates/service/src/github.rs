//! Durable GitHub pull-request orchestration.
//!
//! `localreview-github` owns typed `gh` calls and the Git crate owns the
//! isolated checkout.  This module is the deliberately narrow seam that
//! joins them to a normal review session.  In particular, it never lets a
//! provider refresh silently change a displayed comparison: head SHA changes
//! only take effect through [`ReviewService::refresh_github_pull_request`].

use std::{collections::BTreeSet, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use localreview_domain::{
    Annotation, BaseReference, ComparisonOptions, DiffSide, GitSha, HeadState, PublicationState,
    Repository, RepositoryId, ReviewSession, StoredPath, Workspace, WorkspaceId, WorkspaceSource,
};
use localreview_git::{normalize_remote_url, ManagedWorktree, RepositoryPool};
use localreview_github::{
    GhExecutor, GitHubClient, GitHubPullRequestUrl, ImportedConversationComment,
    ImportedPullRequestState, ImportedReviewThread, NativeReviewComment, NativeReviewDraft,
    PreparedNativeReview, PullRequestMetadata, ReviewConclusion, ReviewPublishError,
};
use localreview_persistence::GitHubPullRequestRefreshPromotion;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    review_generation_rows, PersistedReviewDocument, ReviewService, ServiceError,
    StartReviewRequest, StartReviewResult,
};

/// An immutable GitHub metadata snapshot plus the one app-managed checkout
/// that produced the local review.  This is purposefully provider-shaped and
/// lives outside the generic domain crate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequestRecord {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub canonical_url: String,
    pub owner: String,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub author: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
    pub pinned_base_sha: GitSha,
    pub pinned_head_sha: GitSha,
    pub draft: bool,
    pub state: String,
    pub review_decision: Option<String>,
    pub commits: Vec<GitHubCommitRecord>,
    pub managed_worktree: ManagedWorktree,
    pub imported_threads: Vec<ImportedReviewThread>,
    pub imported_conversation: Vec<ImportedConversationComment>,
    /// Thread import is useful but must never prevent a user from reviewing a
    /// successfully prepared local checkout.
    pub import_error: Option<String>,
    pub metadata_captured_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCommitRecord {
    pub sha: GitSha,
    pub message_headline: String,
    pub authored_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct OpenGitHubPullRequestRequest {
    pub url: String,
    pub application_default_base: BaseReference,
}

#[derive(Clone, Debug)]
pub struct OpenGitHubPullRequestResult {
    pub workspace: Workspace,
    pub review: GitHubPullRequestRecord,
    pub review_start: Option<StartReviewResult>,
    pub reused_existing_workspace: bool,
}

#[derive(Clone, Debug)]
pub struct RefreshGitHubPullRequestResult {
    pub review: GitHubPullRequestRecord,
    pub review_refresh: StartReviewResult,
    pub head_changed: bool,
}

/// A read-only comparison between the immutable revision currently displayed
/// by a PR review workspace and GitHub's latest PR metadata.
///
/// This is intentionally a volatile status rather than an update to
/// [`GitHubPullRequestRecord`]: checking whether a PR moved must never change
/// the pinned worktree, the active review, imported threads, or the durable
/// metadata snapshot.  Only an explicit
/// [`ReviewService::refresh_github_pull_request`] can promote these current
/// revisions into the review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPullRequestUpdateStatus {
    pub workspace_id: WorkspaceId,
    pub canonical_url: String,
    pub pinned_base_sha: GitSha,
    pub pinned_head_sha: GitSha,
    pub current_base_sha: GitSha,
    pub current_head_sha: GitSha,
    pub base_changed: bool,
    pub head_changed: bool,
    /// The local instant at which the provider metadata was fetched.  It is
    /// deliberately not written to `GitHubPullRequestRecord`.
    pub metadata_fetched_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitHubPublicationStatus {
    /// A locally persisted, immutable preview. It has not reached GitHub and
    /// can safely be abandoned when the Finish Review dialog changes.
    Previewed,
    /// The exact preview has crossed the one-POST boundary. A crash or timeout
    /// in this state is ambiguous and must be reconciled (or deliberately
    /// abandoned by the user) before another POST is permitted.
    Prepared,
    /// GitHub returned a completed client-error response, proving that this
    /// payload was not accepted. Unlike Prepared, this is terminal and never
    /// blocks a corrected preview.
    Rejected,
    Submitted,
    Reconciled,
    /// A user deliberately discarded either an unused preview or an
    /// unreconciled timeout attempt. It never blocks a later review.
    Abandoned,
}

/// Persisted *before* the only POST.  The exact JSON and fingerprint give the
/// recovery path enough information to reconcile a timeout without emitting a
/// second native review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPublicationRecord {
    pub id: String,
    pub review_session_id: localreview_domain::ReviewSessionId,
    pub publication_attempt_id: String,
    /// Opaque capability returned by preview and required by the exact submit
    /// API. It intentionally equals the durable attempt id today, but is
    /// named separately so callers never need to infer that implementation
    /// detail from a payload fingerprint.
    #[serde(default)]
    pub preview_token: String,
    pub status: GitHubPublicationStatus,
    pub annotation_ids: Vec<localreview_domain::AnnotationId>,
    pub pinned_head_sha: GitSha,
    pub request_fingerprint: String,
    /// Stable hash of the dialog intent (selected IDs, summary, and
    /// conclusion). It lets compatibility callers locate only a preview that
    /// they actually rendered; submission still posts the stored JSON.
    #[serde(default)]
    pub preview_request_fingerprint: String,
    /// Hash of the exact selected annotation records at preview time. It
    /// detects an edit, deletion, publication change, or re-anchor between
    /// preview and submit instead of silently posting stale feedback.
    #[serde(default)]
    pub annotation_snapshot_fingerprint: String,
    #[serde(default)]
    pub annotation_set_id: Option<localreview_domain::AnnotationSetId>,
    pub reconciliation_marker: String,
    pub payload_json: String,
    pub remote_review_id: Option<u64>,
    pub remote_html_url: Option<String>,
    pub remote_state: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct FinishGitHubReviewRequest {
    pub workspace_id: WorkspaceId,
    pub annotation_ids: Vec<localreview_domain::AnnotationId>,
    pub summary_markdown: String,
    pub conclusion: ReviewConclusion,
}

#[derive(Clone, Debug)]
pub struct FinishGitHubReviewPreview {
    pub review: GitHubPullRequestRecord,
    pub session: ReviewSession,
    pub annotation_ids: Vec<localreview_domain::AnnotationId>,
    pub prepared: PreparedNativeReview,
    /// Must be passed to [`ReviewService::finish_github_review_preview`].
    pub preview_token: String,
    /// Fingerprint of the exact selected annotations at preview time. The
    /// token submit path verifies it again before the one native POST.
    pub annotation_snapshot_fingerprint: String,
    pub preview_request_fingerprint: String,
    /// True when this is a recovered Prepared attempt whose provider outcome
    /// must be reconciled or explicitly abandoned before any new POST.
    pub requires_reconciliation: bool,
}

/// The exact Finish Review submission contract. The token is issued only
/// after the immutable preview is written to durable state; callers cannot
/// supply a replacement JSON body, conclusion, or annotation list here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishGitHubReviewSubmission {
    pub workspace_id: WorkspaceId,
    pub preview_token: String,
}

#[derive(Clone, Debug)]
pub struct FinishGitHubReviewResult {
    pub publication: GitHubPublicationRecord,
    pub annotation_count: usize,
}

impl ReviewService {
    /// Opens a GitHub.com PR using the authenticated `gh` installation.  A
    /// matching open workspace is focused and keeps its original pinned SHAs;
    /// callers must explicitly refresh before a newer remote head is used.
    pub fn open_github_pull_request(
        &self,
        request: OpenGitHubPullRequestRequest,
    ) -> Result<OpenGitHubPullRequestResult, ServiceError> {
        self.open_github_pull_request_with_client(request, &GitHubClient::new())
    }

    pub fn open_github_pull_request_with_client<E: GhExecutor>(
        &self,
        request: OpenGitHubPullRequestRequest,
        client: &GitHubClient<E>,
    ) -> Result<OpenGitHubPullRequestResult, ServiceError> {
        let url = GitHubPullRequestUrl::from_str(&request.url)?;
        require_authentication(client)?;
        if let Some((workspace, review)) = self.find_open_pr_workspace(&url)? {
            return Ok(OpenGitHubPullRequestResult {
                workspace,
                review,
                review_start: None,
                reused_existing_workspace: true,
            });
        }

        let metadata = client.pull_request_metadata(&url)?;
        let review_id = format!("pr-{}", Uuid::new_v4().simple());
        let pool = self.repository_pool();
        let prepared = pool.prepare(
            metadata
                .managed_worktree_request(review_id, self.known_clone_candidates(&metadata)?)?,
        )?;
        let now = Utc::now();
        let workspace = Workspace {
            id: WorkspaceId::new(),
            display_name: format!("{}/{} #{}", url.owner, url.repository, url.number),
            source: WorkspaceSource::PullRequest {
                url: url.canonical_url(),
                owner: url.owner.clone(),
                repository: url.repository.clone(),
                number: url.number,
                worktree: prepared.record.worktree_path.clone(),
            },
            // A PR repository receives its actual immutable base through the
            // repository override below.  This default remains a harmless UI
            // fallback and never drives PR capture.
            default_base: request.application_default_base.clone(),
            pinned: true,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        let repository = pr_repository(&workspace, &metadata, &prepared.record);
        self.state.upsert_workspace(&workspace)?;
        self.state.upsert_repository(&repository)?;
        let mut review = pr_record(
            &workspace,
            &metadata,
            prepared.record,
            import_state(client, &url),
        );
        self.state
            .save_github_pull_request(&review.id, workspace.id, &review)?;
        self.state.save_managed_worktree(
            &review.managed_worktree.review_id,
            workspace.id,
            &review.managed_worktree,
        )?;
        let review_start = match self.start_local_review(StartReviewRequest {
            workspace_id: workspace.id,
            application_default_base: request.application_default_base,
            temporary_base_overrides: Default::default(),
            options: ComparisonOptions::default(),
        }) {
            Ok(start) => start,
            Err(error) => {
                // The isolated checkout remains registered and recoverable;
                // do not orphan it simply because one local capture failed.
                review.import_error = Some(format!(
                    "Local capture could not start: {error}. Retry the review from the workspace."
                ));
                self.state
                    .save_github_pull_request(&review.id, workspace.id, &review)?;
                return Err(error);
            }
        };
        Ok(OpenGitHubPullRequestResult {
            workspace,
            review,
            review_start: Some(review_start),
            reused_existing_workspace: false,
        })
    }

    /// Explicitly asks GitHub for fresh metadata. A changed pin is prepared,
    /// captured, rendered, and re-anchored before one SQLite promotion makes
    /// it current. The old checkout is retired only after that commit.
    pub fn refresh_github_pull_request(
        &self,
        workspace_id: WorkspaceId,
        application_default_base: BaseReference,
    ) -> Result<RefreshGitHubPullRequestResult, ServiceError> {
        self.refresh_github_pull_request_with_client(
            workspace_id,
            application_default_base,
            &GitHubClient::new(),
        )
    }

    pub fn refresh_github_pull_request_with_client<E: GhExecutor>(
        &self,
        workspace_id: WorkspaceId,
        application_default_base: BaseReference,
        client: &GitHubClient<E>,
    ) -> Result<RefreshGitHubPullRequestResult, ServiceError> {
        let workspace = self
            .state
            .workspace(workspace_id)?
            .ok_or(ServiceError::WorkspaceNotFound(workspace_id))?;
        let mut review = self.github_pull_request(workspace_id)?;
        self.ensure_no_unresolved_github_publication(workspace_id)?;
        require_authentication(client)?;
        let url = GitHubPullRequestUrl::from_str(&review.canonical_url)?;
        let metadata = client.pull_request_metadata(&url)?;
        let head_changed = metadata.head.sha != review.pinned_head_sha
            || metadata.base.sha != review.pinned_base_sha;
        if head_changed {
            let session = self.active_review_session(workspace_id)?.ok_or(
                ServiceError::ReviewSessionNotFound(localreview_domain::ReviewSessionId::new()),
            )?;
            let active_annotation_set = self
                .state
                .active_annotation_set(session.id)?
                .ok_or(ServiceError::NoActiveAnnotationSet(session.id))?;
            let pool = self.repository_pool();
            // Every fallible Git/capture step happens while the old durable
            // checkout and documents are still intact.
            let next = pool.prepare(metadata.managed_worktree_request(
                format!("pr-{}", Uuid::new_v4().simple()),
                self.known_clone_candidates(&metadata)?,
            )?)?;
            match pool.is_dirty(&review.managed_worktree) {
                Ok(false) => {}
                Ok(true) => {
                    let _ = pool.delete(&next.record.review_id);
                    return Err(ServiceError::ManagedWorktreeDirty(
                        review.managed_worktree.worktree_path.as_str().to_owned(),
                    ));
                }
                Err(error) => {
                    let _ = pool.delete(&next.record.review_id);
                    return Err(ServiceError::RepositoryPool(error));
                }
            }
            let mut updated_workspace = workspace.clone();
            updated_workspace.source = WorkspaceSource::PullRequest {
                url: url.canonical_url(),
                owner: url.owner.clone(),
                repository: url.repository.clone(),
                number: url.number,
                worktree: next.record.worktree_path.clone(),
            };
            updated_workspace.updated_at = Utc::now();
            let mut repository = self
                .state
                .repositories(workspace_id)?
                .into_iter()
                .next()
                .ok_or(ServiceError::RepositoryNotFound(RepositoryId::new()))?;
            repository.worktree_path = next.record.worktree_path.clone();
            repository.base_override = Some(
                BaseReference::new(metadata.base.sha.as_str())
                    .expect("validated Git SHA is a safe base reference"),
            );
            repository.current_branch = HeadState::Detached(metadata.head.sha.clone());
            let next_review = pr_record(
                &updated_workspace,
                &metadata,
                next.record.clone(),
                import_state(client, &url),
            );
            let prepared_refresh: Result<(_, _, _), ServiceError> = (|| {
                let git = localreview_git::GitRepository::open(repository.worktree_path.as_str());
                let resolved = git.resolve_comparison(
                    repository.id,
                    localreview_domain::ComparisonId::new(),
                    BaseReference::new(metadata.base.sha.as_str())
                        .expect("a parsed Git SHA is a valid base reference"),
                    ComparisonOptions::default(),
                )?;
                let capture = git.capture_local_comparison(resolved)?;
                let documents = self.build_captured_documents_for_repository(
                    session.id,
                    &capture,
                    &repository,
                )?;
                let previous = self
                    .state
                    .current_comparisons_for_session(session.id)?
                    .into_iter()
                    .find(|comparison| comparison.repository_id == repository.id);
                let annotations = previous.map_or_else(
                    || Ok(Vec::new()),
                    |previous| {
                        self.prepare_reanchored_annotations(
                            session.id,
                            repository.id,
                            previous.id,
                            &capture.comparison,
                            &documents,
                        )
                    },
                )?;
                let generation = self.state.prepare_review_generation(
                    &capture.comparison,
                    &review_generation_rows(documents),
                )?;
                Ok((capture, generation, annotations))
            })();
            let (capture, generation, annotation_updates) = match prepared_refresh {
                Ok(prepared) => prepared,
                Err(error) => {
                    // The new checkout has never been made active. It is safe
                    // to remove if clean; an interrupted cleanup is handled
                    // by the pool's conservative orphan repair.
                    let _ = pool.delete(&next.record.review_id);
                    return Err(error);
                }
            };
            let mut refreshed_session = session.clone();
            refreshed_session.refreshed_at = Some(Utc::now());
            let old_worktree = review.managed_worktree.clone();
            if let Err(error) =
                self.state
                    .promote_github_pull_request_refresh(GitHubPullRequestRefreshPromotion {
                        workspace: &updated_workspace,
                        repository: &repository,
                        github_pull_request_id: &next_review.id,
                        github_pull_request: &next_review,
                        active_worktree_id: &next_review.managed_worktree.review_id,
                        active_worktree: &next_review.managed_worktree,
                        retired_worktree_id: &old_worktree.review_id,
                        retired_worktree: &old_worktree,
                        session: &refreshed_session,
                        generation: &generation,
                        annotations: &annotation_updates,
                    })
            {
                let _ = pool.delete(&next.record.review_id);
                return Err(ServiceError::Persistence(error));
            }
            // The durable review now references the new checkout.  A cleanup
            // failure cannot roll it back: the retired record remains in
            // SQLite so explicit/startup recovery retries it safely later.
            if pool.delete(&old_worktree.review_id).is_ok() {
                // Cleanup bookkeeping is deliberately best-effort here. The
                // new review is already durable; a later recovery pass can
                // clear a leftover retirement marker without risking it.
                let _ = self
                    .state
                    .complete_retired_managed_worktree(&old_worktree.review_id);
            }
            review = next_review;
            let review_refresh = StartReviewResult {
                session: refreshed_session,
                active_annotation_set,
                captures: vec![capture],
                failures: Vec::new(),
            };
            return Ok(RefreshGitHubPullRequestResult {
                review,
                review_refresh,
                head_changed,
            });
        } else {
            // Thread state is mutable GitHub-side but cannot change the local
            // capture.  Refreshing it is safe at the explicit user boundary.
            let imported = import_state(client, &url);
            review.imported_threads = imported.threads;
            review.imported_conversation = imported.conversation;
            review.import_error = imported.error;
            review.metadata_captured_at = Utc::now();
            self.state
                .save_github_pull_request(&review.id, workspace_id, &review)?;
        }
        let session = self.active_review_session(workspace_id)?.ok_or(
            ServiceError::ReviewSessionNotFound(localreview_domain::ReviewSessionId::new()),
        )?;
        let review_refresh = self.refresh_local_review(
            session.id,
            application_default_base,
            Default::default(),
            ComparisonOptions::default(),
        )?;
        Ok(RefreshGitHubPullRequestResult {
            review,
            review_refresh,
            head_changed,
        })
    }

    pub fn github_pull_request(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GitHubPullRequestRecord, ServiceError> {
        self.state
            .github_pull_request_for_workspace(workspace_id)?
            .ok_or(ServiceError::NotGitHubPullRequest { workspace_id })
    }

    /// Checks GitHub for the current PR base and head without changing the
    /// local review.  This is safe to call for a passive "Refresh available"
    /// indicator: it authenticates and reads provider metadata only.  The
    /// caller must invoke `refresh_github_pull_request` to move any pin.
    pub fn github_pull_request_update_status(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<GitHubPullRequestUpdateStatus, ServiceError> {
        self.github_pull_request_update_status_with_client(workspace_id, &GitHubClient::new())
    }

    /// Fixture-friendly form of [`ReviewService::github_pull_request_update_status`].
    pub fn github_pull_request_update_status_with_client<E: GhExecutor>(
        &self,
        workspace_id: WorkspaceId,
        client: &GitHubClient<E>,
    ) -> Result<GitHubPullRequestUpdateStatus, ServiceError> {
        // Read the durable record before invoking the external provider.  In
        // particular, a non-PR workspace fails locally without requiring gh.
        let review = self.github_pull_request(workspace_id)?;
        let url = GitHubPullRequestUrl::from_str(&review.canonical_url)?;
        require_authentication(client)?;
        let metadata = client.pull_request_metadata(&url)?;
        let metadata_fetched_at = Utc::now();

        Ok(GitHubPullRequestUpdateStatus {
            workspace_id,
            canonical_url: review.canonical_url,
            pinned_base_sha: review.pinned_base_sha.clone(),
            pinned_head_sha: review.pinned_head_sha.clone(),
            base_changed: metadata.base.sha != review.pinned_base_sha,
            head_changed: metadata.head.sha != review.pinned_head_sha,
            current_base_sha: metadata.base.sha,
            current_head_sha: metadata.head.sha,
            metadata_fetched_at,
        })
    }

    pub fn github_imported_threads(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ImportedReviewThread>, ServiceError> {
        Ok(self.github_pull_request(workspace_id)?.imported_threads)
    }

    pub fn github_imported_conversation(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<ImportedConversationComment>, ServiceError> {
        Ok(self
            .github_pull_request(workspace_id)?
            .imported_conversation)
    }

    pub fn preview_github_review(
        &self,
        request: FinishGitHubReviewRequest,
    ) -> Result<FinishGitHubReviewPreview, ServiceError> {
        let review = self.github_pull_request(request.workspace_id)?;
        // A Prepared publication is the durable recovery capability for the
        // one request whose provider outcome is unknown. Return that exact
        // payload after restart (even if another session was somehow made
        // active) instead of minting a second preview that could later POST a
        // duplicate review.
        if let Some(record) = self.unresolved_github_publication(request.workspace_id)? {
            let session = self
                .state
                .review_sessions_for_id(record.review_session_id)?
                .ok_or(ServiceError::ReviewSessionNotFound(
                    record.review_session_id,
                ))?;
            return Ok(FinishGitHubReviewPreview {
                review,
                session,
                annotation_ids: record.annotation_ids.clone(),
                prepared: prepared_from_publication(&record),
                preview_token: record.preview_token,
                annotation_snapshot_fingerprint: record.annotation_snapshot_fingerprint,
                preview_request_fingerprint: record.preview_request_fingerprint,
                requires_reconciliation: true,
            });
        }
        let session = self.active_review_session(request.workspace_id)?.ok_or(
            ServiceError::ReviewSessionNotFound(localreview_domain::ReviewSessionId::new()),
        )?;
        let set = self
            .state
            .active_annotation_set(session.id)?
            .ok_or(ServiceError::NoActiveAnnotationSet(session.id))?;
        let requested = request
            .annotation_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let current_comparison = self
            .state
            .current_comparisons_for_session(session.id)?
            .into_iter()
            .next()
            .ok_or(ServiceError::ReviewSessionNotActive(session.id))?;
        let mut selected = self
            .state
            .annotations(set.id)?
            .into_iter()
            .filter(|annotation| {
                // The UI only offers open annotations, but the service is the
                // publication trust boundary. A resolved/outdated item must
                // never reappear in a native review through a stale or crafted
                // command invocation.
                annotation.state == localreview_domain::AnnotationState::Open
                    && annotation.publication_state != PublicationState::LocalOnly
                    && annotation.publication_state != PublicationState::Published
                    && (requested.is_empty() || requested.contains(&annotation.id))
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|annotation| annotation.id);
        if !requested.is_empty() {
            let selected_ids = selected
                .iter()
                .map(|annotation| annotation.id)
                .collect::<BTreeSet<_>>();
            let unavailable = requested
                .difference(&selected_ids)
                .copied()
                .collect::<Vec<_>>();
            if !unavailable.is_empty() {
                return Err(ServiceError::GitHubReviewAnnotationsUnavailable {
                    annotation_ids: unavailable,
                });
            }
        }
        let documents = self.review_documents(session.id)?;
        let comments = selected
            .iter()
            .map(|annotation| {
                let comment =
                    NativeReviewComment::from_annotation(annotation, current_comparison.id)?;
                validate_pinned_pull_request_anchor(annotation, &documents)?;
                Ok(comment)
            })
            .collect::<Result<Vec<_>, ReviewPublishError>>()?;
        let preview_request_fingerprint = preview_request_fingerprint(&request)?;
        let draft = NativeReviewDraft {
            pinned_head_sha: review.pinned_head_sha.clone(),
            publication_attempt_id: Uuid::new_v4().to_string(),
            conclusion: request.conclusion,
            summary_markdown: (!request.summary_markdown.trim().is_empty())
                .then_some(request.summary_markdown),
            comments,
        };
        let prepared = draft.prepare()?;
        let annotation_snapshot_fingerprint = annotation_snapshot_fingerprint(&selected)?;
        let record = GitHubPublicationRecord {
            id: format!("github-publication-{}", Uuid::new_v4()),
            review_session_id: session.id,
            publication_attempt_id: prepared.publication_attempt_id.clone(),
            preview_token: prepared.publication_attempt_id.clone(),
            status: GitHubPublicationStatus::Previewed,
            annotation_ids: selected.iter().map(|annotation| annotation.id).collect(),
            pinned_head_sha: prepared.pinned_head_sha.clone(),
            request_fingerprint: prepared.request_fingerprint.clone(),
            preview_request_fingerprint: preview_request_fingerprint.clone(),
            annotation_snapshot_fingerprint,
            annotation_set_id: Some(set.id),
            reconciliation_marker: prepared.reconciliation_marker.clone(),
            payload_json: prepared.payload_json.clone(),
            remote_review_id: None,
            remote_html_url: None,
            remote_state: None,
            created_at: Utc::now(),
            completed_at: None,
        };
        self.state.save_github_publication(
            &record.id,
            session.id,
            &record.publication_attempt_id,
            &record,
        )?;
        Ok(FinishGitHubReviewPreview {
            review,
            session,
            annotation_ids: record.annotation_ids.clone(),
            prepared,
            preview_token: record.preview_token,
            annotation_snapshot_fingerprint: record.annotation_snapshot_fingerprint,
            preview_request_fingerprint,
            requires_reconciliation: false,
        })
    }

    /// Compatibility entry point for callers that have not yet been upgraded
    /// to pass the preview token. It only consumes an already persisted
    /// preview matching the exact dialog intent; it never regenerates JSON.
    pub fn finish_github_review(
        &self,
        request: FinishGitHubReviewRequest,
    ) -> Result<FinishGitHubReviewResult, ServiceError> {
        self.finish_github_review_with_client(request, &GitHubClient::new())
    }

    pub fn finish_github_review_with_client<E: GhExecutor>(
        &self,
        request: FinishGitHubReviewRequest,
        client: &GitHubClient<E>,
    ) -> Result<FinishGitHubReviewResult, ServiceError> {
        let session = self.active_review_session(request.workspace_id)?.ok_or(
            ServiceError::ReviewSessionNotFound(localreview_domain::ReviewSessionId::new()),
        )?;
        let intent = preview_request_fingerprint(&request)?;
        let preview = self
            .state
            .github_publications_for_session::<GitHubPublicationRecord>(session.id)?
            .into_iter()
            .filter(|publication| {
                publication.status == GitHubPublicationStatus::Previewed
                    && publication.preview_request_fingerprint == intent
            })
            .max_by(|left, right| {
                left.created_at
                    .cmp(&right.created_at)
                    .then_with(|| left.id.cmp(&right.id))
            })
            .ok_or(ServiceError::GitHubReviewPreviewNotFound {
                workspace_id: request.workspace_id,
                preview_token: "matching rendered preview".to_owned(),
            })?;
        self.finish_github_review_preview_with_client(
            FinishGitHubReviewSubmission {
                workspace_id: request.workspace_id,
                preview_token: preview.preview_token,
            },
            client,
        )
    }

    /// Consumes a durable preview capability. There is deliberately no body,
    /// conclusion, annotation list, or arbitrary JSON on this API.
    pub fn finish_github_review_preview(
        &self,
        submission: FinishGitHubReviewSubmission,
    ) -> Result<FinishGitHubReviewResult, ServiceError> {
        self.finish_github_review_preview_with_client(submission, &GitHubClient::new())
    }

    pub fn finish_github_review_preview_with_client<E: GhExecutor>(
        &self,
        submission: FinishGitHubReviewSubmission,
        client: &GitHubClient<E>,
    ) -> Result<FinishGitHubReviewResult, ServiceError> {
        let _publication_guard = self
            .github_publication_lock
            .lock()
            .map_err(|_| ServiceError::GitHubPublicationLockUnavailable)?;
        require_authentication(client)?;
        let review = self.github_pull_request(submission.workspace_id)?;
        let url = GitHubPullRequestUrl::from_str(&review.canonical_url)?;
        if let Some(reconciled) =
            self.reconcile_unresolved_publications(client, &url, submission.workspace_id)?
        {
            return Ok(FinishGitHubReviewResult {
                annotation_count: reconciled.annotation_ids.len(),
                publication: reconciled,
            });
        }
        let session = self.active_review_session(submission.workspace_id)?.ok_or(
            ServiceError::ReviewSessionNotFound(localreview_domain::ReviewSessionId::new()),
        )?;
        let mut record = self
            .state
            .github_publication_by_attempt::<GitHubPublicationRecord>(&submission.preview_token)?
            .filter(|record| record.preview_token == submission.preview_token)
            .ok_or_else(|| ServiceError::GitHubReviewPreviewNotFound {
                workspace_id: submission.workspace_id,
                preview_token: submission.preview_token.clone(),
            })?;
        if record.review_session_id != session.id {
            return Err(ServiceError::GitHubReviewPreviewStale {
                preview_token: submission.preview_token,
                reason: "the active review session changed".to_owned(),
            });
        }
        if record.status != GitHubPublicationStatus::Previewed {
            return Err(ServiceError::GitHubReviewPreviewNotReady {
                preview_token: submission.preview_token,
                reason: format!("publication is {:?}", record.status),
            });
        }
        let active_set = self
            .state
            .active_annotation_set(session.id)?
            .ok_or(ServiceError::NoActiveAnnotationSet(session.id))?;
        if record.annotation_set_id != Some(active_set.id) {
            return Err(ServiceError::GitHubReviewPreviewStale {
                preview_token: submission.preview_token,
                reason: "the active annotation set changed".to_owned(),
            });
        }
        let mut annotations = self
            .state
            .annotations(active_set.id)?
            .into_iter()
            .filter(|annotation| record.annotation_ids.contains(&annotation.id))
            .collect::<Vec<_>>();
        annotations.sort_by_key(|annotation| annotation.id);
        if annotations.len() != record.annotation_ids.len()
            || annotation_snapshot_fingerprint(&annotations)?
                != record.annotation_snapshot_fingerprint
        {
            return Err(ServiceError::GitHubReviewPreviewStale {
                preview_token: submission.preview_token,
                reason: "one or more selected annotations changed after preview".to_owned(),
            });
        }
        let remote = client.pull_request_metadata(&url)?;
        if remote.head.sha != review.pinned_head_sha {
            return Err(ServiceError::GitHubHeadChanged {
                expected: review.pinned_head_sha,
                actual: remote.head.sha,
            });
        }
        if remote.base.sha != review.pinned_base_sha {
            return Err(ServiceError::GitHubBaseChanged {
                expected: review.pinned_base_sha,
                actual: remote.base.sha,
            });
        }
        if record.pinned_head_sha != review.pinned_head_sha {
            return Err(ServiceError::GitHubReviewPreviewStale {
                preview_token: submission.preview_token,
                reason: "the preview pin no longer matches this pull request workspace".to_owned(),
            });
        }
        let prepared = prepared_from_publication(&record);
        // Persist the ambiguous pre-POST state immediately before the one
        // outbound request. A timeout is reconciled by its marker, never by a
        // retrying POST.
        record.status = GitHubPublicationStatus::Prepared;
        self.state.save_github_publication(
            &record.id,
            session.id,
            &record.publication_attempt_id,
            &record,
        )?;
        let submitted = client.submit_native_review(&url, &prepared);
        let submitted = match submitted {
            Ok(value) => value,
            Err(error) if error.is_definitive_provider_rejection() => {
                // A completed 4xx response is not ambiguous: GitHub rejected
                // the request and created no review. Mark the attempt terminal
                // so a corrected preview can be submitted immediately.
                record.status = GitHubPublicationStatus::Rejected;
                record.completed_at = Some(Utc::now());
                self.state.save_github_publication(
                    &record.id,
                    session.id,
                    &record.publication_attempt_id,
                    &record,
                )?;
                return Err(ServiceError::GitHubPublish(error));
            }
            // The pre-POST record remains durable.  A later Finish Review
            // reconciles its hidden marker before any new POST is allowed.
            Err(error) => {
                return Err(ServiceError::GitHubPublicationAmbiguous {
                    preview_token: record.preview_token,
                    reason: error.to_string(),
                });
            }
        };
        let mut completed = record;
        completed.status = GitHubPublicationStatus::Submitted;
        completed.remote_review_id = Some(submitted.id);
        completed.remote_html_url = submitted.html_url;
        completed.remote_state = submitted.state;
        completed.completed_at = Some(Utc::now());
        let published = mark_published(
            self.state.annotations(active_set.id)?,
            &completed.annotation_ids,
        );
        self.state.save_github_publication_and_annotations(
            &completed.id,
            session.id,
            &completed.publication_attempt_id,
            &completed,
            &published,
        )?;
        Ok(FinishGitHubReviewResult {
            annotation_count: completed.annotation_ids.len(),
            publication: completed,
        })
    }

    /// Explicitly discards either an unused preview or an ambiguous timeout
    /// attempt. Discarding a `Prepared` attempt is intentional user consent:
    /// a provider success arriving later can no longer be reconciled before a
    /// subsequent submission is allowed.
    pub fn abandon_github_review_publication(
        &self,
        submission: FinishGitHubReviewSubmission,
    ) -> Result<GitHubPublicationRecord, ServiceError> {
        // Serialize explicit abandonment with the same one-POST boundary. In
        // particular, a second window cannot mark a record Abandoned while
        // its provider request is still in flight and then race a replacement
        // review behind it.
        let _publication_guard = self
            .github_publication_lock
            .lock()
            .map_err(|_| ServiceError::GitHubPublicationLockUnavailable)?;
        let mut record = self
            .state
            .github_publication_by_attempt::<GitHubPublicationRecord>(&submission.preview_token)?
            .filter(|record| record.preview_token == submission.preview_token)
            .ok_or_else(|| ServiceError::GitHubReviewPreviewNotFound {
                workspace_id: submission.workspace_id,
                preview_token: submission.preview_token.clone(),
            })?;
        let session = self
            .state
            .review_sessions_for_id(record.review_session_id)?
            .ok_or(ServiceError::ReviewSessionNotFound(
                record.review_session_id,
            ))?;
        if session.workspace_id != submission.workspace_id {
            return Err(ServiceError::GitHubReviewPreviewStale {
                preview_token: submission.preview_token,
                reason: "the preview belongs to another workspace".to_owned(),
            });
        }
        if !matches!(
            record.status,
            GitHubPublicationStatus::Previewed | GitHubPublicationStatus::Prepared
        ) {
            return Err(ServiceError::GitHubReviewPreviewNotReady {
                preview_token: submission.preview_token,
                reason: format!("publication is {:?}", record.status),
            });
        }
        record.status = GitHubPublicationStatus::Abandoned;
        record.completed_at = Some(Utc::now());
        self.state.save_github_publication(
            &record.id,
            session.id,
            &record.publication_attempt_id,
            &record,
        )?;
        Ok(record)
    }

    /// Archive/delete a PR workspace only after the app-owned checkout is
    /// cleanly removed.  Shared mirrors deliberately remain in the cache.
    pub fn delete_github_pull_request_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<(), ServiceError> {
        let review = self.github_pull_request(workspace_id)?;
        self.ensure_no_unresolved_github_publication(workspace_id)?;
        let pool = self.repository_pool();
        match pool.delete(&review.managed_worktree.review_id) {
            Ok(_) => {}
            Err(localreview_git::RepositoryPoolError::DirtyManagedWorktree { path }) => {
                return Err(ServiceError::ManagedWorktreeDirty(
                    path.display().to_string(),
                ));
            }
            Err(error) => return Err(ServiceError::RepositoryPool(error)),
        }
        self.state
            .remove_managed_worktree(&review.managed_worktree.review_id)?;
        let mut workspace = self
            .state
            .workspace(workspace_id)?
            .ok_or(ServiceError::WorkspaceNotFound(workspace_id))?;
        workspace.archived_at = Some(Utc::now());
        workspace.updated_at = Utc::now();
        self.state.upsert_workspace(&workspace)?;
        Ok(())
    }

    /// Startup recovery is intentionally limited to clean app-managed
    /// worktrees. Dirty unregistered directories are reported for a human
    /// decision instead of being erased.
    pub fn repair_managed_worktree_orphans(
        &self,
    ) -> Result<localreview_git::WorktreeRepairReport, ServiceError> {
        let pool = self.repository_pool();
        // A refresh may have committed the new pin successfully while the old
        // checkout became dirty just before removal. Keep that old record
        // durable until a later conservative cleanup succeeds; never force it.
        for retired in self.state.retired_managed_worktrees::<ManagedWorktree>()? {
            match pool.delete(&retired.review_id) {
                Ok(_)
                | Err(localreview_git::RepositoryPoolError::UnknownManagedWorktree { .. })
                | Err(localreview_git::RepositoryPoolError::MissingManagedWorktree { .. }) => {
                    self.state
                        .complete_retired_managed_worktree(&retired.review_id)?;
                }
                // A dirty or transiently unavailable checkout remains a
                // recoverable record. Do not turn startup repair into an
                // unsafe forced deletion.
                Err(_) => {}
            }
        }
        Ok(pool.repair_orphans()?)
    }

    pub fn unresolved_github_publication(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<GitHubPublicationRecord>, ServiceError> {
        let mut publications = Vec::new();
        for session in self.state.review_sessions(workspace_id)? {
            publications.extend(
                self.state
                    .github_publications_for_session::<GitHubPublicationRecord>(session.id)?
                    .into_iter()
                    .filter(|publication| publication.status == GitHubPublicationStatus::Prepared),
            );
        }
        publications.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(publications.into_iter().next())
    }

    pub fn ensure_no_unresolved_github_publication(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<(), ServiceError> {
        if let Some(publication) = self.unresolved_github_publication(workspace_id)? {
            return Err(ServiceError::GitHubPublicationReconciliationPending {
                preview_token: publication.preview_token,
            });
        }
        Ok(())
    }

    fn reconcile_unresolved_publications<E: GhExecutor>(
        &self,
        client: &GitHubClient<E>,
        url: &GitHubPullRequestUrl,
        workspace_id: WorkspaceId,
    ) -> Result<Option<GitHubPublicationRecord>, ServiceError> {
        let mut reconciled = None;
        while let Some(mut publication) = self.unresolved_github_publication(workspace_id)? {
            let remote = client.reconcile_native_review(url, &publication.request_fingerprint)?;
            let Some(remote) = remote else {
                return Err(ServiceError::GitHubPublicationReconciliationPending {
                    preview_token: publication.preview_token,
                });
            };
            publication.status = GitHubPublicationStatus::Reconciled;
            publication.remote_review_id = Some(remote.id);
            publication.remote_html_url = remote.html_url;
            publication.remote_state = remote.state;
            publication.completed_at = Some(Utc::now());
            let annotation_set_id = publication.annotation_set_id.ok_or_else(|| {
                ServiceError::GitHubPublicationAnnotationSetMissing {
                    preview_token: publication.preview_token.clone(),
                }
            })?;
            let annotations = mark_published(
                self.state.annotations(annotation_set_id)?,
                &publication.annotation_ids,
            );
            self.state.save_github_publication_and_annotations(
                &publication.id,
                publication.review_session_id,
                &publication.publication_attempt_id,
                &publication,
                &annotations,
            )?;
            reconciled = Some(publication);
        }
        Ok(reconciled)
    }

    fn repository_pool(&self) -> RepositoryPool {
        RepositoryPool::new(self.state.root().join("cache"), self.state.root())
    }

    fn find_open_pr_workspace(
        &self,
        url: &GitHubPullRequestUrl,
    ) -> Result<Option<(Workspace, GitHubPullRequestRecord)>, ServiceError> {
        for workspace in self.state.workspaces()? {
            if workspace.archived_at.is_some() {
                continue;
            }
            if let WorkspaceSource::PullRequest { url: existing, .. } = &workspace.source {
                if existing == &url.canonical_url() {
                    if let Some(record) =
                        self.state.github_pull_request_for_workspace(workspace.id)?
                    {
                        return Ok(Some((workspace, record)));
                    }
                }
            }
        }
        Ok(None)
    }

    fn known_clone_candidates(
        &self,
        metadata: &PullRequestMetadata,
    ) -> Result<Vec<PathBuf>, ServiceError> {
        let expected = normalize_remote_url(&metadata.url.clone_url());
        let mut paths = BTreeSet::new();
        for workspace in self.state.workspaces()? {
            // Managed PR worktrees must not be selected as an object source:
            // their lifecycle is controlled by the app and they can vanish
            // when their corresponding review closes.
            if !matches!(workspace.source, WorkspaceSource::LocalDirectory { .. }) {
                continue;
            }
            for repository in self.state.repositories(workspace.id)? {
                if repository
                    .normalized_primary_remote
                    .as_deref()
                    .is_some_and(|remote| remote == expected)
                {
                    paths.insert(PathBuf::from(repository.worktree_path.as_str()));
                }
            }
        }
        Ok(paths.into_iter().collect())
    }
}

fn require_authentication<E: GhExecutor>(client: &GitHubClient<E>) -> Result<(), ServiceError> {
    let status = client.authentication_status();
    if status.gh_available && status.authenticated {
        Ok(())
    } else {
        Err(ServiceError::GitHubAuthentication(status.diagnostic))
    }
}

fn pr_repository(
    workspace: &Workspace,
    metadata: &PullRequestMetadata,
    worktree: &ManagedWorktree,
) -> Repository {
    Repository {
        id: RepositoryId::new(),
        workspace_id: workspace.id,
        relative_path: StoredPath::from("."),
        worktree_path: worktree.worktree_path.clone(),
        git_common_dir: None,
        normalized_primary_remote: Some(normalize_remote_url(&metadata.url.clone_url())),
        enabled: true,
        base_override: Some(
            BaseReference::new(metadata.base.sha.as_str())
                .expect("a parsed Git SHA is a valid non-empty reference"),
        ),
        current_branch: HeadState::Detached(metadata.head.sha.clone()),
        last_resolved_base_sha: None,
        last_fetch_at: None,
        last_fetch_error: None,
        discovery_error: None,
        comparison_error: None,
    }
}

fn pr_record(
    workspace: &Workspace,
    metadata: &PullRequestMetadata,
    managed_worktree: ManagedWorktree,
    imported: ImportState,
) -> GitHubPullRequestRecord {
    GitHubPullRequestRecord {
        id: format!("github-pr-{}", workspace.id),
        workspace_id: workspace.id,
        canonical_url: metadata.url.canonical_url(),
        owner: metadata.url.owner.clone(),
        repository: metadata.url.repository.clone(),
        number: metadata.url.number,
        title: metadata.title.clone(),
        author: metadata.author.clone(),
        base_ref: metadata.base.name.clone(),
        head_ref: metadata.head.name.clone(),
        pinned_base_sha: metadata.base.sha.clone(),
        pinned_head_sha: metadata.head.sha.clone(),
        draft: metadata.draft,
        state: metadata.state.clone(),
        review_decision: metadata.review_decision.clone(),
        commits: metadata
            .commits
            .iter()
            .map(|commit| GitHubCommitRecord {
                sha: commit.oid.clone(),
                message_headline: commit.message_headline.clone(),
                authored_at: commit.authored_at,
            })
            .collect(),
        managed_worktree,
        imported_threads: imported.threads,
        imported_conversation: imported.conversation,
        import_error: imported.error,
        metadata_captured_at: Utc::now(),
    }
}

#[derive(Default)]
struct ImportState {
    threads: Vec<ImportedReviewThread>,
    conversation: Vec<ImportedConversationComment>,
    error: Option<String>,
}

fn import_state<E: GhExecutor>(
    client: &GitHubClient<E>,
    url: &GitHubPullRequestUrl,
) -> ImportState {
    match client.import_pull_request_state(url) {
        Ok(ImportedPullRequestState {
            threads,
            conversation,
        }) => ImportState {
            threads,
            conversation,
            error: None,
        },
        Err(error) => ImportState {
            threads: Vec::new(),
            conversation: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

fn prepared_from_publication(publication: &GitHubPublicationRecord) -> PreparedNativeReview {
    PreparedNativeReview {
        publication_attempt_id: publication.publication_attempt_id.clone(),
        pinned_head_sha: publication.pinned_head_sha.clone(),
        request_fingerprint: publication.request_fingerprint.clone(),
        reconciliation_marker: publication.reconciliation_marker.clone(),
        payload_json: publication.payload_json.clone(),
    }
}

/// GitHub line/side anchors must refer to a line present in the pinned pull
/// request diff. Full File intentionally exposes the entire immutable blob,
/// so source-valid lines outside a displayed hunk remain useful local notes
/// but cannot be included in a native review. Multi-line comments must stay
/// within one hunk on one side; this also prevents silently spanning omitted
/// unchanged source between two hunks.
fn validate_pinned_pull_request_anchor(
    annotation: &Annotation,
    documents: &[PersistedReviewDocument],
) -> Result<(), ReviewPublishError> {
    let invalid = || ReviewPublishError::AnchorOutsidePullRequestDiff {
        annotation_id: annotation.id.to_string(),
    };
    let anchor = annotation.anchor.as_ref().ok_or_else(invalid)?;
    let side = anchor.side.ok_or_else(invalid)?;
    let start = anchor.start_line.ok_or_else(invalid)?;
    let end = anchor.end_line.ok_or_else(invalid)?;
    let document = documents
        .iter()
        .find(|document| {
            document.document.comparison_id == anchor.comparison_id
                && document.document.file.path == anchor.file_path
        })
        .ok_or_else(invalid)?;

    let representable = document.document.hunks.iter().any(|hunk| {
        let has_line = |wanted| {
            hunk.unified_rows.iter().any(|row| {
                let cell = match side {
                    DiffSide::Old => row.old.as_ref(),
                    DiffSide::New => row.new.as_ref(),
                };
                cell.is_some_and(|cell| cell.line_number == wanted)
            })
        };
        has_line(start) && has_line(end)
    });
    representable.then_some(()).ok_or_else(invalid)
}

#[derive(Serialize)]
struct PreviewRequestFingerprint<'a> {
    workspace_id: String,
    annotation_ids: Vec<String>,
    summary_markdown: &'a str,
    conclusion: ReviewConclusion,
}

fn preview_request_fingerprint(
    request: &FinishGitHubReviewRequest,
) -> Result<String, ServiceError> {
    let mut annotation_ids = request
        .annotation_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    annotation_ids.sort();
    annotation_ids.dedup();
    let intent = PreviewRequestFingerprint {
        workspace_id: request.workspace_id.to_string(),
        annotation_ids,
        summary_markdown: &request.summary_markdown,
        conclusion: request.conclusion,
    };
    let encoded = serde_json::to_vec(&intent)
        .map_err(|error| ServiceError::GitHubReviewPreviewSerialization(error.to_string()))?;
    Ok(hex_digest(&encoded))
}

fn annotation_snapshot_fingerprint(annotations: &[Annotation]) -> Result<String, ServiceError> {
    let encoded = serde_json::to_vec(annotations)
        .map_err(|error| ServiceError::GitHubReviewPreviewSerialization(error.to_string()))?;
    Ok(hex_digest(&encoded))
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn mark_published(
    mut annotations: Vec<Annotation>,
    annotation_ids: &[localreview_domain::AnnotationId],
) -> Vec<Annotation> {
    let selected = annotation_ids.iter().copied().collect::<BTreeSet<_>>();
    for annotation in &mut annotations {
        if selected.contains(&annotation.id) {
            annotation.publication_state = PublicationState::Published;
            annotation.updated_at = Utc::now();
        }
    }
    annotations
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command, sync::mpsc, thread, time::Duration};

    use chrono::Utc;
    use localreview_domain::{
        AnnotationAnchor, AnnotationId, AnnotationKind, AnnotationState, LineAnchorInput,
        PublicationState,
    };
    use localreview_github::{FixtureGhExecutor, GhOutput};
    use tempfile::TempDir;

    use super::*;
    use crate::{OpenLocalWorkspaceRequest, ReviewService};
    use localreview_git::DiscoveryConfig;
    use localreview_persistence::StateStore;

    fn git(path: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn gh_output(body: impl AsRef<[u8]>) -> GhOutput {
        GhOutput {
            success: true,
            exit_code: Some(0),
            stdout: body.as_ref().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn gh_failure(stderr: impl AsRef<[u8]>) -> GhOutput {
        GhOutput {
            success: false,
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: stderr.as_ref().to_vec(),
        }
    }

    fn auth_outputs() -> [GhOutput; 2] {
        [
            gh_output("gh version 2.0.0\n"),
            gh_output("github.com\n  ✓ Logged in to github.com account octocat\n"),
        ]
    }

    fn metadata(base: &GitSha, head: &GitSha) -> GhOutput {
        gh_output(format!(
            r#"{{"number":42,"title":"Fixture PR","url":"https://github.com/octo/repo/pull/42","author":{{"login":"octocat"}},"baseRefName":"main","baseRefOid":"{}","headRefName":"feature","headRefOid":"{}","isDraft":false,"state":"OPEN","reviewDecision":null,"commits":[{{"oid":"{}","messageHeadline":"feature","authoredDate":"2026-07-21T12:00:00Z"}}]}}"#,
            base.as_str(),
            head.as_str(),
            head.as_str()
        ))
    }

    fn thread_page() -> GhOutput {
        gh_output(
            r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#,
        )
    }

    fn conversation_page() -> GhOutput {
        gh_output("[]")
    }

    struct Fixture {
        _temporary: TempDir,
        service: ReviewService,
        base: GitSha,
        head: GitSha,
        known_clone: PathBuf,
        local_workspace: Workspace,
    }

    fn fixture() -> Fixture {
        let temporary = TempDir::new().unwrap();
        let clone = temporary.path().join("known-clone");
        fs::create_dir_all(&clone).unwrap();
        git(&clone, &["init", "-b", "main"]);
        git(&clone, &["config", "user.email", "fixture@example.invalid"]);
        git(&clone, &["config", "user.name", "Fixture"]);
        fs::write(
            clone.join("review.rs"),
            "fn base() {}\nconst KEEP_2: u8 = 2;\nconst KEEP_3: u8 = 3;\nconst KEEP_4: u8 = 4;\nconst KEEP_5: u8 = 5;\nconst KEEP_6: u8 = 6;\nconst KEEP_7: u8 = 7;\nconst KEEP_8: u8 = 8;\nconst KEEP_9: u8 = 9;\nconst KEEP_10: u8 = 10;\n",
        )
        .unwrap();
        git(&clone, &["add", "review.rs"]);
        git(&clone, &["commit", "-m", "base"]);
        let base = GitSha::new(git(&clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        fs::write(
            clone.join("review.rs"),
            "fn head() {}\nconst KEEP_2: u8 = 2;\nconst KEEP_3: u8 = 3;\nconst KEEP_4: u8 = 4;\nconst KEEP_5: u8 = 5;\nconst KEEP_6: u8 = 6;\nconst KEEP_7: u8 = 7;\nconst KEEP_8: u8 = 8;\nconst KEEP_9: u8 = 9;\nconst CHANGED_10: u8 = 10;\n",
        )
        .unwrap();
        git(&clone, &["commit", "-am", "head"]);
        let head = GitSha::new(git(&clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        // A local workspace with this canonical remote is a safe known-clone
        // candidate. The pool validates both pin objects before using it.
        git(
            &clone,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/octo/repo.git",
            ],
        );
        let service = ReviewService::new(StateStore::open(temporary.path().join("state")).unwrap());
        let local_workspace = service
            .open_local_workspace(OpenLocalWorkspaceRequest {
                root: clone.clone(),
                display_name: Some("known clone".into()),
                workspace_default_base: None,
                discovery: DiscoveryConfig::default(),
            })
            .unwrap()
            .workspace;
        Fixture {
            _temporary: temporary,
            service,
            base,
            head,
            known_clone: clone,
            local_workspace,
        }
    }

    fn open(fixture: &Fixture) -> OpenGitHubPullRequestResult {
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.extend([
            metadata(&fixture.base, &fixture.head),
            thread_page(),
            conversation_page(),
        ]);
        fixture
            .service
            .open_github_pull_request_with_client(
                OpenGitHubPullRequestRequest {
                    url: "https://github.com/octo/repo/pull/42".into(),
                    application_default_base: BaseReference::default(),
                },
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap()
    }

    fn add_publishable_annotation(
        service: &ReviewService,
        workspace_id: WorkspaceId,
    ) -> (localreview_domain::ReviewSessionId, AnnotationId) {
        add_publishable_annotation_range(service, workspace_id, 1, 1)
    }

    fn add_publishable_annotation_range(
        service: &ReviewService,
        workspace_id: WorkspaceId,
        start_line: u32,
        end_line: u32,
    ) -> (localreview_domain::ReviewSessionId, AnnotationId) {
        let session = service
            .active_review_session(workspace_id)
            .unwrap()
            .unwrap();
        let set = service
            .state
            .active_annotation_set(session.id)
            .unwrap()
            .unwrap();
        let comparison = service
            .state
            .current_comparisons_for_session(session.id)
            .unwrap()
            .pop()
            .unwrap();
        let repository = service
            .state
            .repositories(workspace_id)
            .unwrap()
            .pop()
            .unwrap();
        let annotation = Annotation {
            id: AnnotationId::new(),
            annotation_set_id: set.id,
            kind: AnnotationKind::Comment,
            state: AnnotationState::Open,
            publication_state: PublicationState::IncludedInNextReview,
            labels: Vec::new(),
            body_markdown: "Use the pinned worktree in this fixture.".into(),
            anchor: Some(
                AnnotationAnchor::from_line(LineAnchorInput {
                    comparison_id: comparison.id,
                    repository_id: repository.id,
                    file_path: StoredPath::from("review.rs"),
                    side: localreview_domain::DiffSide::New,
                    start_line,
                    end_line,
                    selected_source: format!("captured lines {start_line}-{end_line}"),
                    surrounding_context: "captured fixture source".into(),
                })
                .unwrap(),
            ),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let id = annotation.id;
        service.state.save_annotation(&annotation).unwrap();
        (session.id, id)
    }

    #[test]
    fn opens_with_a_known_clone_pins_metadata_and_reuses_without_silent_refresh() {
        let fixture = fixture();
        let opened = open(&fixture);
        assert_eq!(
            opened.workspace.source.tags(),
            vec![localreview_domain::WorkspaceSourceTag::GitHub]
        );
        assert!(opened.review_start.is_some());
        assert!(matches!(
            opened.review.managed_worktree.source,
            localreview_git::RepositoryObjectSource::KnownClone { .. }
        ));
        assert_eq!(opened.review.pinned_head_sha, fixture.head);
        assert_eq!(
            fixture
                .service
                .review_documents(opened.review_start.unwrap().session.id)
                .unwrap()
                .len(),
            1
        );

        let reused = fixture
            .service
            .open_github_pull_request_with_client(
                OpenGitHubPullRequestRequest {
                    url: "https://github.com/octo/repo/pull/42".into(),
                    application_default_base: BaseReference::default(),
                },
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(auth_outputs())),
            )
            .unwrap();
        assert!(reused.reused_existing_workspace);
        assert_eq!(reused.review.pinned_head_sha, fixture.head);
        assert_eq!(fixture.service.state.workspaces().unwrap().len(), 2);
        assert_ne!(fixture.local_workspace.id, reused.workspace.id);
    }

    #[test]
    fn read_only_update_status_reports_an_unchanged_pr_without_changing_the_durable_pin() {
        let fixture = fixture();
        let opened = open(&fixture);
        let durable_before = fixture
            .service
            .github_pull_request(opened.workspace.id)
            .unwrap();
        let before_fetch = Utc::now();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(metadata(&fixture.base, &fixture.head));

        let status = fixture
            .service
            .github_pull_request_update_status_with_client(
                opened.workspace.id,
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap();

        assert!(!status.base_changed);
        assert!(!status.head_changed);
        assert_eq!(status.pinned_base_sha, fixture.base);
        assert_eq!(status.pinned_head_sha, fixture.head);
        assert_eq!(status.current_base_sha, fixture.base);
        assert_eq!(status.current_head_sha, fixture.head);
        assert!(status.metadata_fetched_at >= before_fetch);
        assert_eq!(
            fixture
                .service
                .github_pull_request(opened.workspace.id)
                .unwrap(),
            durable_before
        );
    }

    #[test]
    fn read_only_update_status_reports_moved_base_and_head_without_promoting_them() {
        let fixture = fixture();
        let opened = open(&fixture);
        let durable_before = fixture
            .service
            .github_pull_request(opened.workspace.id)
            .unwrap();
        let current_base = GitSha::new("2222222222222222222222222222222222222222").unwrap();
        let current_head = GitSha::new("3333333333333333333333333333333333333333").unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(metadata(&current_base, &current_head));

        let status = fixture
            .service
            .github_pull_request_update_status_with_client(
                opened.workspace.id,
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap();

        assert!(status.base_changed);
        assert!(status.head_changed);
        assert_eq!(status.current_base_sha, current_base);
        assert_eq!(status.current_head_sha, current_head);
        assert_eq!(
            fixture
                .service
                .github_pull_request(opened.workspace.id)
                .unwrap(),
            durable_before
        );
    }

    #[test]
    fn read_only_update_status_provider_error_leaves_the_pinned_review_untouched() {
        let fixture = fixture();
        let opened = open(&fixture);
        let durable_before = fixture
            .service
            .github_pull_request(opened.workspace.id)
            .unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(gh_output("not-json"));

        let error = fixture
            .service
            .github_pull_request_update_status_with_client(
                opened.workspace.id,
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap_err();

        assert!(matches!(error, ServiceError::GitHubPullRequest(_)));
        assert_eq!(
            fixture
                .service
                .github_pull_request(opened.workspace.id)
                .unwrap(),
            durable_before
        );
    }

    #[test]
    fn preview_rejects_full_file_anchors_outside_the_exact_pinned_diff() {
        // Line 5 is omitted between two distant rendered hunks. The second
        // case has individually visible endpoints in those separate hunks;
        // it must not be represented as one GitHub multi-line comment.
        for (start_line, end_line) in [(5, 5), (1, 10)] {
            let fixture = fixture();
            let opened = open(&fixture);
            let workspace_id = opened.workspace.id;
            let (session_id, annotation_id) = add_publishable_annotation_range(
                &fixture.service,
                workspace_id,
                start_line,
                end_line,
            );

            let error = fixture
                .service
                .preview_github_review(FinishGitHubReviewRequest {
                    workspace_id,
                    annotation_ids: vec![annotation_id],
                    summary_markdown: String::new(),
                    conclusion: ReviewConclusion::Comment,
                })
                .unwrap_err();
            assert!(matches!(
                error,
                ServiceError::GitHubPublish(
                    ReviewPublishError::AnchorOutsidePullRequestDiff { .. }
                )
            ));
            assert!(fixture
                .service
                .state
                .github_publications_for_session::<GitHubPublicationRecord>(session_id)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn preview_rejects_resolved_or_unknown_selected_annotations() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (session_id, annotation_id) =
            add_publishable_annotation(&fixture.service, workspace_id);
        let set = fixture
            .service
            .state
            .active_annotation_set(session_id)
            .unwrap()
            .unwrap();
        let mut annotation = fixture
            .service
            .state
            .annotations(set.id)
            .unwrap()
            .pop()
            .unwrap();
        annotation.state = AnnotationState::Resolved;
        annotation.updated_at = Utc::now();
        fixture.service.state.save_annotation(&annotation).unwrap();

        for unavailable in [annotation_id, AnnotationId::new()] {
            let error = fixture
                .service
                .preview_github_review(FinishGitHubReviewRequest {
                    workspace_id,
                    annotation_ids: vec![unavailable],
                    summary_markdown: "Do not silently post only this summary.".into(),
                    conclusion: ReviewConclusion::Comment,
                })
                .unwrap_err();
            assert!(matches!(
                error,
                ServiceError::GitHubReviewAnnotationsUnavailable {
                    annotation_ids
                } if annotation_ids == vec![unavailable]
            ));
        }
        assert!(fixture
            .service
            .state
            .github_publications_for_session::<GitHubPublicationRecord>(session_id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn definitive_http_rejection_is_terminal_and_does_not_strand_later_previews() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (session_id, annotation_id) =
            add_publishable_annotation(&fixture.service, workspace_id);
        let request = || FinishGitHubReviewRequest {
            workspace_id,
            annotation_ids: vec![annotation_id],
            summary_markdown: "provider validation fixture".into(),
            conclusion: ReviewConclusion::Comment,
        };
        let preview = fixture.service.preview_github_review(request()).unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.extend([
            metadata(&fixture.base, &fixture.head),
            gh_failure("gh: Validation Failed (HTTP 422)"),
        ]);
        let executor = FixtureGhExecutor::with_outputs(outputs);
        let error = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token,
                },
                &GitHubClient::with_executor(executor.clone()),
            )
            .unwrap_err();
        assert!(matches!(error, ServiceError::GitHubPublish(_)));
        let attempts = fixture
            .service
            .state
            .github_publications_for_session::<GitHubPublicationRecord>(session_id)
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, GitHubPublicationStatus::Rejected);
        assert_eq!(
            executor
                .commands()
                .iter()
                .filter(|command| command.arguments.iter().any(|argument| argument == "POST"))
                .count(),
            1
        );

        let corrected = fixture.service.preview_github_review(request()).unwrap();
        assert_ne!(corrected.preview_token, attempts[0].preview_token);
    }

    #[test]
    fn a_batched_submit_marks_annotations_published_and_timeout_is_reconciled_without_a_second_post(
    ) {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (session_id, annotation_id) =
            add_publishable_annotation(&fixture.service, workspace_id);
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.extend([
            metadata(&fixture.base, &fixture.head),
            gh_output(r#"{"id":7,"html_url":"https://github.com/octo/repo/pull/42#pullrequestreview-7","state":"COMMENT"}"#),
        ]);
        let executor = FixtureGhExecutor::with_outputs(outputs);
        let client = GitHubClient::with_executor(executor.clone());
        let preview = fixture
            .service
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: vec![annotation_id],
                summary_markdown: "One batched review".into(),
                conclusion: ReviewConclusion::Comment,
            })
            .unwrap();
        let published = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token.clone(),
                },
                &client,
            )
            .unwrap();
        assert_eq!(published.annotation_count, 1);
        assert_eq!(published.publication.remote_review_id, Some(7));
        assert_eq!(
            published.publication.payload_json,
            preview.prepared.payload_json
        );
        assert_eq!(
            fixture
                .service
                .state
                .annotations(
                    fixture
                        .service
                        .state
                        .active_annotation_set(session_id)
                        .unwrap()
                        .unwrap()
                        .id
                )
                .unwrap()[0]
                .publication_state,
            PublicationState::Published
        );
        let commands = executor.commands();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.arguments.iter().any(|arg| arg == "POST"))
                .count(),
            1
        );
        let posted = commands
            .iter()
            .find(|command| command.arguments.iter().any(|arg| arg == "POST"))
            .and_then(|command| command.stdin.as_deref())
            .unwrap();
        assert_eq!(posted, preview.prepared.payload_json.as_bytes());
    }

    #[test]
    fn an_ambiguous_timeout_is_reconciled_before_any_later_submit_attempt() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (session_id, annotation_id) =
            add_publishable_annotation(&fixture.service, workspace_id);
        let request = || FinishGitHubReviewRequest {
            workspace_id,
            annotation_ids: vec![annotation_id],
            summary_markdown: "A retry-safe review".into(),
            conclusion: ReviewConclusion::Comment,
        };
        let preview = fixture.service.preview_github_review(request()).unwrap();
        let mut first_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        first_outputs.push(metadata(&fixture.base, &fixture.head));
        let first_executor = FixtureGhExecutor::with_outputs(first_outputs);
        first_executor.push_error("timeout after request was accepted");
        assert!(fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token.clone(),
                },
                &GitHubClient::with_executor(first_executor.clone()),
            )
            .is_err());
        let pending = fixture
            .service
            .state
            .github_publications_for_session::<GitHubPublicationRecord>(session_id)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, GitHubPublicationStatus::Prepared);

        let marker = pending[0].reconciliation_marker.clone();
        let mut recovery_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        recovery_outputs.push(gh_output(format!(
            r#"[[{{"id":99,"html_url":"https://github.com/octo/repo/pull/42#pullrequestreview-99","state":"COMMENT","body":"recovered\n{}"}}]]"#,
            marker
        )));
        let recovery_executor = FixtureGhExecutor::with_outputs(recovery_outputs);
        let reconciled = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token,
                },
                &GitHubClient::with_executor(recovery_executor.clone()),
            )
            .unwrap();
        assert_eq!(
            reconciled.publication.status,
            GitHubPublicationStatus::Reconciled
        );
        assert_eq!(reconciled.publication.remote_review_id, Some(99));
        assert_eq!(
            recovery_executor
                .commands()
                .iter()
                .filter(|command| command.arguments.iter().any(|argument| argument == "POST"))
                .count(),
            0
        );
        assert_eq!(
            fixture
                .service
                .state
                .annotations(
                    fixture
                        .service
                        .state
                        .active_annotation_set(session_id)
                        .unwrap()
                        .unwrap()
                        .id
                )
                .unwrap()[0]
                .publication_state,
            PublicationState::Published
        );
    }

    #[test]
    fn a_missing_reconciliation_marker_never_reposts_and_can_be_explicitly_abandoned() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (_, annotation_id) = add_publishable_annotation(&fixture.service, workspace_id);
        let request = || FinishGitHubReviewRequest {
            workspace_id,
            annotation_ids: vec![annotation_id],
            summary_markdown: "uncertain transport outcome".into(),
            conclusion: ReviewConclusion::Comment,
        };
        let preview = fixture.service.preview_github_review(request()).unwrap();
        let mut first_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        first_outputs.push(metadata(&fixture.base, &fixture.head));
        let first_executor = FixtureGhExecutor::with_outputs(first_outputs);
        first_executor.push_error("connection closed after request bytes were sent");
        let first_error = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token.clone(),
                },
                &GitHubClient::with_executor(first_executor),
            )
            .unwrap_err();
        assert!(matches!(
            first_error,
            ServiceError::GitHubPublicationAmbiguous { .. }
        ));

        let mut recovery_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        recovery_outputs.push(gh_output("[]"));
        let recovery_executor = FixtureGhExecutor::with_outputs(recovery_outputs);
        let recovery_error = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token.clone(),
                },
                &GitHubClient::with_executor(recovery_executor.clone()),
            )
            .unwrap_err();
        assert!(matches!(
            recovery_error,
            ServiceError::GitHubPublicationReconciliationPending { .. }
        ));
        assert!(recovery_executor
            .commands()
            .iter()
            .all(|command| !command.arguments.iter().any(|argument| argument == "POST")));

        let abandoned = fixture
            .service
            .abandon_github_review_publication(FinishGitHubReviewSubmission {
                workspace_id,
                preview_token: preview.preview_token,
            })
            .unwrap();
        assert_eq!(abandoned.status, GitHubPublicationStatus::Abandoned);
        let replacement = fixture.service.preview_github_review(request()).unwrap();
        assert_eq!(replacement.annotation_ids, vec![annotation_id]);
    }

    #[test]
    fn restart_recovers_the_exact_prepared_attempt_across_session_changes_and_blocks_lifecycle() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let original_worktree =
            PathBuf::from(opened.review.managed_worktree.worktree_path.as_str());
        let (_, annotation_id) = add_publishable_annotation(&fixture.service, workspace_id);
        let original_request = FinishGitHubReviewRequest {
            workspace_id,
            annotation_ids: vec![annotation_id],
            summary_markdown: "durable restart payload".into(),
            conclusion: ReviewConclusion::Comment,
        };
        let original = fixture
            .service
            .preview_github_review(original_request.clone())
            .unwrap();
        let mut first_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        first_outputs.push(metadata(&fixture.base, &fixture.head));
        let first_executor = FixtureGhExecutor::with_outputs(first_outputs);
        first_executor.push_error("connection ended after request bytes were sent");
        assert!(matches!(
            fixture
                .service
                .finish_github_review_preview_with_client(
                    FinishGitHubReviewSubmission {
                        workspace_id,
                        preview_token: original.preview_token.clone(),
                    },
                    &GitHubClient::with_executor(first_executor),
                )
                .unwrap_err(),
            ServiceError::GitHubPublicationAmbiguous { .. }
        ));

        let reopened =
            ReviewService::new(StateStore::open(fixture.service.state().root()).unwrap());
        assert!(matches!(
            reopened
                .ensure_no_unresolved_github_publication(workspace_id)
                .unwrap_err(),
            ServiceError::GitHubPublicationReconciliationPending { ref preview_token }
                if preview_token == &original.preview_token
        ));
        // Refresh and deletion are both stopped before provider access or
        // checkout removal while the one-POST outcome is unresolved.
        let refresh_executor = FixtureGhExecutor::with_outputs([]);
        assert!(matches!(
            reopened
                .refresh_github_pull_request_with_client(
                    workspace_id,
                    BaseReference::default(),
                    &GitHubClient::with_executor(refresh_executor.clone()),
                )
                .unwrap_err(),
            ServiceError::GitHubPublicationReconciliationPending { .. }
        ));
        assert!(refresh_executor.commands().is_empty());
        assert!(matches!(
            reopened
                .delete_github_pull_request_workspace(workspace_id)
                .unwrap_err(),
            ServiceError::GitHubPublicationReconciliationPending { .. }
        ));
        assert!(original_worktree.is_dir());

        // Even if a lower-level caller bypasses the controller's lifecycle
        // guard, recovery searches the entire workspace rather than only the
        // newly active session.
        reopened
            .start_local_review(StartReviewRequest {
                workspace_id,
                application_default_base: BaseReference::default(),
                temporary_base_overrides: Default::default(),
                options: ComparisonOptions::default(),
            })
            .unwrap();
        let recovered = reopened
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: Vec::new(),
                summary_markdown: "different dialog state after restart".into(),
                conclusion: ReviewConclusion::Approve,
            })
            .unwrap();
        assert!(recovered.requires_reconciliation);
        assert_eq!(recovered.preview_token, original.preview_token);
        assert_eq!(
            recovered.prepared.payload_json,
            original.prepared.payload_json
        );
        assert_eq!(recovered.annotation_ids, vec![annotation_id]);

        let mut recovery_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        recovery_outputs.push(gh_output("[]"));
        let recovery_executor = FixtureGhExecutor::with_outputs(recovery_outputs);
        let error = reopened
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: recovered.preview_token.clone(),
                },
                &GitHubClient::with_executor(recovery_executor.clone()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::GitHubPublicationReconciliationPending { ref preview_token }
                if preview_token == &original.preview_token
        ));
        assert!(recovery_executor
            .commands()
            .iter()
            .all(|command| !command.arguments.iter().any(|argument| argument == "POST")));

        reopened
            .abandon_github_review_publication(FinishGitHubReviewSubmission {
                workspace_id,
                preview_token: original.preview_token.clone(),
            })
            .unwrap();
        let replacement = reopened
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: Vec::new(),
                summary_markdown: "replacement".into(),
                conclusion: ReviewConclusion::Comment,
            })
            .unwrap();
        assert_ne!(replacement.preview_token, original.preview_token);
        assert!(!replacement.requires_reconciliation);
    }

    #[test]
    fn cross_session_reconciliation_publishes_the_original_archived_annotation_set() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (original_session_id, annotation_id) =
            add_publishable_annotation(&fixture.service, workspace_id);
        let original_set_id = fixture
            .service
            .state
            .active_annotation_set(original_session_id)
            .unwrap()
            .unwrap()
            .id;
        let preview = fixture
            .service
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: vec![annotation_id],
                summary_markdown: "archive-safe recovery".into(),
                conclusion: ReviewConclusion::Comment,
            })
            .unwrap();
        let mut first_outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        first_outputs.push(metadata(&fixture.base, &fixture.head));
        let first_executor = FixtureGhExecutor::with_outputs(first_outputs);
        first_executor.push_error("timeout after provider accepted request");
        fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token.clone(),
                },
                &GitHubClient::with_executor(first_executor),
            )
            .unwrap_err();
        fixture
            .service
            .start_local_review(StartReviewRequest {
                workspace_id,
                application_default_base: BaseReference::default(),
                temporary_base_overrides: Default::default(),
                options: ComparisonOptions::default(),
            })
            .unwrap();

        let marker = fixture
            .service
            .unresolved_github_publication(workspace_id)
            .unwrap()
            .unwrap()
            .reconciliation_marker;
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(gh_output(format!(
            r#"[[{{"id":101,"html_url":null,"state":"COMMENT","body":"{}"}}]]"#,
            marker
        )));
        let executor = FixtureGhExecutor::with_outputs(outputs);
        let reconciled = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token,
                },
                &GitHubClient::with_executor(executor.clone()),
            )
            .unwrap();
        assert_eq!(
            reconciled.publication.status,
            GitHubPublicationStatus::Reconciled
        );
        assert_eq!(
            fixture.service.state.annotations(original_set_id).unwrap()[0].publication_state,
            PublicationState::Published
        );
        assert!(executor
            .commands()
            .iter()
            .all(|command| !command.arguments.iter().any(|argument| argument == "POST")));
    }

    #[test]
    fn moved_base_with_unchanged_head_is_rejected_before_the_one_post_boundary() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (_, annotation_id) = add_publishable_annotation(&fixture.service, workspace_id);
        let preview = fixture
            .service
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: vec![annotation_id],
                summary_markdown: "base freshness gate".into(),
                conclusion: ReviewConclusion::Comment,
            })
            .unwrap();
        let moved_base = GitSha::new("2222222222222222222222222222222222222222").unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(metadata(&moved_base, &fixture.head));
        let executor = FixtureGhExecutor::with_outputs(outputs);
        let error = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token,
                },
                &GitHubClient::with_executor(executor.clone()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::GitHubBaseChanged { expected, actual }
                if expected == fixture.base && actual == moved_base
        ));
        assert!(executor
            .commands()
            .iter()
            .all(|command| !command.arguments.iter().any(|argument| argument == "POST")));
    }

    #[test]
    fn preview_token_rejects_annotation_mutation_and_can_be_deliberately_abandoned() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (session_id, annotation_id) =
            add_publishable_annotation(&fixture.service, workspace_id);
        let preview = fixture
            .service
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: vec![annotation_id],
                summary_markdown: "immutable preview".into(),
                conclusion: ReviewConclusion::Comment,
            })
            .unwrap();
        let set = fixture
            .service
            .state
            .active_annotation_set(session_id)
            .unwrap()
            .unwrap();
        let mut annotation = fixture
            .service
            .state
            .annotations(set.id)
            .unwrap()
            .pop()
            .unwrap();
        annotation.body_markdown.push_str(" edited after preview");
        annotation.updated_at = Utc::now() + chrono::Duration::seconds(1);
        fixture.service.state.save_annotation(&annotation).unwrap();

        let executor = FixtureGhExecutor::with_outputs(auth_outputs());
        let error = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: preview.preview_token.clone(),
                },
                &GitHubClient::with_executor(executor.clone()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::GitHubReviewPreviewStale { .. }
        ));
        assert!(executor
            .commands()
            .iter()
            .all(|command| !command.arguments.iter().any(|argument| argument == "POST")));

        let abandoned = fixture
            .service
            .abandon_github_review_publication(FinishGitHubReviewSubmission {
                workspace_id,
                preview_token: preview.preview_token,
            })
            .unwrap();
        assert_eq!(abandoned.status, GitHubPublicationStatus::Abandoned);
    }

    #[test]
    fn abandoned_timeout_attempt_does_not_permanently_lock_later_submission() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (_, annotation_id) = add_publishable_annotation(&fixture.service, workspace_id);
        let request = || FinishGitHubReviewRequest {
            workspace_id,
            annotation_ids: vec![annotation_id],
            summary_markdown: "retry after explicit abandon".into(),
            conclusion: ReviewConclusion::Comment,
        };
        let first = fixture.service.preview_github_review(request()).unwrap();
        let first_executor = FixtureGhExecutor::with_outputs([
            auth_outputs()[0].clone(),
            auth_outputs()[1].clone(),
            metadata(&fixture.base, &fixture.head),
        ]);
        first_executor.push_error("timeout after provider accepted request");
        assert!(fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: first.preview_token.clone(),
                },
                &GitHubClient::with_executor(first_executor),
            )
            .is_err());
        fixture
            .service
            .abandon_github_review_publication(FinishGitHubReviewSubmission {
                workspace_id,
                preview_token: first.preview_token,
            })
            .unwrap();

        let second = fixture.service.preview_github_review(request()).unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.extend([
            metadata(&fixture.base, &fixture.head),
            gh_output(r#"{"id":77,"html_url":null,"state":"COMMENT"}"#),
        ]);
        let completed = fixture
            .service
            .finish_github_review_preview_with_client(
                FinishGitHubReviewSubmission {
                    workspace_id,
                    preview_token: second.preview_token,
                },
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap();
        assert_eq!(
            completed.publication.status,
            GitHubPublicationStatus::Submitted
        );
        assert_eq!(completed.publication.remote_review_id, Some(77));
    }

    #[test]
    fn explicit_abandonment_waits_for_the_shared_publication_boundary() {
        let fixture = fixture();
        let opened = open(&fixture);
        let workspace_id = opened.workspace.id;
        let (_, annotation_id) = add_publishable_annotation(&fixture.service, workspace_id);
        let preview = fixture
            .service
            .preview_github_review(FinishGitHubReviewRequest {
                workspace_id,
                annotation_ids: vec![annotation_id],
                summary_markdown: "publication lock fixture".into(),
                conclusion: ReviewConclusion::Comment,
            })
            .unwrap();

        let boundary = fixture.service.github_publication_lock.lock().unwrap();
        let service = fixture.service.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(
                    service.abandon_github_review_publication(FinishGitHubReviewSubmission {
                        workspace_id,
                        preview_token: preview.preview_token,
                    }),
                )
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(boundary);
        assert_eq!(
            result_rx.recv().unwrap().unwrap().status,
            GitHubPublicationStatus::Abandoned
        );
        worker.join().unwrap();
    }

    #[test]
    fn failed_pr_pin_promotion_keeps_old_pin_worktree_and_documents_after_reopen() {
        let mut fixture = fixture();
        let opened = open(&fixture);
        let old_review = opened.review.clone();
        let old_worktree = PathBuf::from(old_review.managed_worktree.worktree_path.as_str());
        let old_session = fixture
            .service
            .active_review_session(opened.workspace.id)
            .unwrap()
            .unwrap();
        let old_documents = fixture.service.review_documents(old_session.id).unwrap();
        fs::write(
            fixture.known_clone.join("review.rs"),
            "fn promotion_failure() {}\n",
        )
        .unwrap();
        git(&fixture.known_clone, &["commit", "-am", "next head"]);
        fixture.head =
            GitSha::new(git(&fixture.known_clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        fixture
            .service
            .state
            .inject_next_atomic_commit_failure_for_test();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.extend([
            metadata(&fixture.base, &fixture.head),
            thread_page(),
            conversation_page(),
        ]);
        let error = fixture
            .service
            .refresh_github_pull_request_with_client(
                opened.workspace.id,
                BaseReference::default(),
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap_err();
        assert!(matches!(error, ServiceError::Persistence(_)));
        assert!(old_worktree.is_dir());
        assert_eq!(
            fixture
                .service
                .github_pull_request(opened.workspace.id)
                .unwrap(),
            old_review
        );
        assert_eq!(
            fixture.service.review_documents(old_session.id).unwrap(),
            old_documents
        );

        let reopened =
            ReviewService::new(StateStore::open(fixture.service.state().root()).unwrap());
        assert_eq!(
            reopened.github_pull_request(opened.workspace.id).unwrap(),
            old_review
        );
        assert_eq!(
            reopened.review_documents(old_session.id).unwrap(),
            old_documents
        );
    }

    #[test]
    fn capture_preparation_failure_keeps_the_old_pr_pin_worktree_and_documents() {
        let mut fixture = fixture();
        let opened = open(&fixture);
        let old_review = opened.review.clone();
        let old_worktree = PathBuf::from(old_review.managed_worktree.worktree_path.as_str());
        let old_session = fixture
            .service
            .active_review_session(opened.workspace.id)
            .unwrap()
            .unwrap();
        let old_documents = fixture.service.review_documents(old_session.id).unwrap();

        // Both objects exist in the known clone, so the pool successfully
        // prepares the next detached checkout. They have no common ancestor,
        // which makes Git's comparison capture fail before SQLite promotion.
        git(
            &fixture.known_clone,
            &["checkout", "--orphan", "unrelated-base"],
        );
        git(&fixture.known_clone, &["rm", "-rf", "."]);
        fs::write(
            fixture.known_clone.join("unrelated.rs"),
            "fn unrelated() {}\n",
        )
        .unwrap();
        git(&fixture.known_clone, &["add", "-A"]);
        git(&fixture.known_clone, &["commit", "-m", "unrelated root"]);
        let unrelated_base =
            GitSha::new(git(&fixture.known_clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        git(&fixture.known_clone, &["checkout", "main"]);
        fs::write(
            fixture.known_clone.join("review.rs"),
            "fn later_head() {}\n",
        )
        .unwrap();
        git(&fixture.known_clone, &["commit", "-am", "later head"]);
        fixture.head =
            GitSha::new(git(&fixture.known_clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(metadata(&unrelated_base, &fixture.head));
        let error = fixture
            .service
            .refresh_github_pull_request_with_client(
                opened.workspace.id,
                BaseReference::default(),
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap_err();
        assert!(matches!(error, ServiceError::Git(_)));
        assert!(old_worktree.is_dir());
        assert_eq!(
            fixture
                .service
                .github_pull_request(opened.workspace.id)
                .unwrap(),
            old_review
        );
        assert_eq!(
            fixture.service.review_documents(old_session.id).unwrap(),
            old_documents
        );
    }

    #[test]
    fn dirty_old_worktree_refuses_pr_refresh_without_moving_pins_or_documents() {
        let mut fixture = fixture();
        let opened = open(&fixture);
        let old_review = opened.review.clone();
        let old_worktree = PathBuf::from(old_review.managed_worktree.worktree_path.as_str());
        let old_session = fixture
            .service
            .active_review_session(opened.workspace.id)
            .unwrap()
            .unwrap();
        let old_documents = fixture.service.review_documents(old_session.id).unwrap();
        fs::write(old_worktree.join("keep-local.txt"), "do not replace\n").unwrap();
        assert_ne!(
            git(&old_worktree, &["status", "--porcelain"]),
            "",
            "the fixture must make the retired checkout dirty"
        );
        fs::write(
            fixture.known_clone.join("review.rs"),
            "fn newer_head() {}\n",
        )
        .unwrap();
        git(&fixture.known_clone, &["commit", "-am", "next head"]);
        fixture.head =
            GitSha::new(git(&fixture.known_clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.push(metadata(&fixture.base, &fixture.head));
        let error = fixture
            .service
            .refresh_github_pull_request_with_client(
                opened.workspace.id,
                BaseReference::default(),
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap_err();
        assert!(matches!(error, ServiceError::ManagedWorktreeDirty(_)));
        assert!(old_worktree.join("keep-local.txt").is_file());
        assert_eq!(
            fixture
                .service
                .github_pull_request(opened.workspace.id)
                .unwrap(),
            old_review
        );
        assert_eq!(
            fixture.service.review_documents(old_session.id).unwrap(),
            old_documents
        );
    }

    #[test]
    fn explicit_head_refresh_replaces_only_a_clean_worktree_then_delete_archives_it() {
        let mut fixture = fixture();
        let opened = open(&fixture);
        let old_worktree = PathBuf::from(opened.review.managed_worktree.worktree_path.as_str());
        fs::write(fixture.known_clone.join("review.rs"), "fn refreshed() {}\n").unwrap();
        git(&fixture.known_clone, &["commit", "-am", "refresh head"]);
        fixture.head =
            GitSha::new(git(&fixture.known_clone, &["rev-parse", "HEAD"]).trim()).unwrap();
        let mut outputs = auth_outputs().into_iter().collect::<Vec<_>>();
        outputs.extend([
            metadata(&fixture.base, &fixture.head),
            thread_page(),
            conversation_page(),
        ]);
        let refreshed = fixture
            .service
            .refresh_github_pull_request_with_client(
                opened.workspace.id,
                BaseReference::default(),
                &GitHubClient::with_executor(FixtureGhExecutor::with_outputs(outputs)),
            )
            .unwrap();
        assert!(refreshed.head_changed);
        assert_eq!(refreshed.review.pinned_head_sha, fixture.head);
        let new_worktree = PathBuf::from(refreshed.review.managed_worktree.worktree_path.as_str());
        assert_ne!(new_worktree, old_worktree);
        assert!(!old_worktree.exists());
        assert!(new_worktree.is_dir());
        assert_eq!(
            fixture
                .service
                .state
                .current_comparisons_for_session(refreshed.review_refresh.session.id)
                .unwrap()[0]
                .head_sha,
            Some(fixture.head.clone())
        );
        let (archived_session_id, archived_annotation_id) =
            add_publishable_annotation(&fixture.service, opened.workspace.id);
        let archived_set_id = fixture
            .service
            .state
            .active_annotation_set(archived_session_id)
            .unwrap()
            .unwrap()
            .id;
        let archived_documents = fixture
            .service
            .review_documents(archived_session_id)
            .unwrap();
        let archived_export = localreview_domain::PromptExportRecord {
            id: localreview_domain::PromptExportId::new(),
            review_session_id: archived_session_id,
            annotation_set_id: archived_set_id,
            annotation_set_ids: vec![archived_set_id],
            scope: localreview_domain::PromptScope::CommentsAndQuestions,
            annotation_ids: vec![archived_annotation_id],
            template_version: crate::PROMPT_TEMPLATE_VERSION,
            rendered_markdown: Some("# Exact GitHub review export\n\nDurable fixture.".into()),
            title: Some("Full review prompt".into()),
            annotation_count: Some(1),
            estimated_tokens: Some(12),
            created_at: Utc::now(),
        };
        fixture
            .service
            .state
            .save_prompt_export(&archived_export)
            .unwrap();

        fs::write(new_worktree.join("do-not-delete.txt"), "dirty\n").unwrap();
        assert!(matches!(
            fixture
                .service
                .delete_github_pull_request_workspace(opened.workspace.id),
            Err(ServiceError::ManagedWorktreeDirty(_))
        ));
        assert!(new_worktree.is_dir());
        fs::remove_file(new_worktree.join("do-not-delete.txt")).unwrap();
        fixture
            .service
            .delete_github_pull_request_workspace(opened.workspace.id)
            .unwrap();
        assert!(!new_worktree.exists());
        assert!(fixture
            .service
            .state
            .workspace(opened.workspace.id)
            .unwrap()
            .unwrap()
            .archived_at
            .is_some());

        let reopened =
            ReviewService::new(StateStore::open(fixture.service.state().root()).unwrap());
        assert_eq!(
            reopened.review_documents(archived_session_id).unwrap(),
            archived_documents,
            "deleting the disposable worktree must not delete the frozen review"
        );
        assert_eq!(
            reopened
                .state
                .annotations(archived_set_id)
                .unwrap()
                .iter()
                .map(|annotation| annotation.id)
                .collect::<Vec<_>>(),
            vec![archived_annotation_id],
            "archived GitHub feedback must remain durable after restart"
        );
        assert_eq!(
            reopened
                .state
                .prompt_export(archived_export.id)
                .unwrap()
                .unwrap()
                .rendered_markdown,
            archived_export.rendered_markdown,
            "exact prompt bytes must survive disposable-worktree deletion and restart"
        );
    }
}
