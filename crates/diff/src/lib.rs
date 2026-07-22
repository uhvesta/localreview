//! Canonical, side-aware diff data. All renderer modes are projections of the
//! same immutable document, so annotation anchors never depend on UI row IDs.

use std::cmp;

use localreview_domain::{ComparisonId, DiffSide, ReviewFileId, StoredPath};
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFile {
    pub id: ReviewFileId,
    pub path: StoredPath,
    pub old_path: Option<StoredPath>,
    pub status: ReviewFileStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Binary,
    ModeChanged,
    TypeChanged,
    Submodule,
    LfsPointer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDocument {
    pub content: String,
    pub fingerprint: String,
    pub line_count: u32,
    /// Whether the final logical line is newline-terminated.  Empty sources
    /// are considered complete: there is no final line for Git's
    /// `\\ No newline at end of file` marker to describe.
    #[serde(default = "default_true")]
    pub has_final_newline: bool,
}

impl SourceDocument {
    #[must_use]
    pub fn new(content: impl Into<String>) -> Self {
        let content = content.into();
        let line_count = u32::try_from(content.lines().count()).unwrap_or(u32::MAX);
        let fingerprint = blake3::hash(content.as_bytes()).to_hex().to_string();
        let has_final_newline = content.is_empty() || content.ends_with('\n');
        Self {
            content,
            fingerprint,
            line_count,
            has_final_newline,
        }
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.content.lines()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewDiffDocument {
    pub comparison_id: ComparisonId,
    pub file: ReviewFile,
    pub old: SourceDocument,
    pub new: SourceDocument,
    pub hunks: Vec<ReviewHunk>,
    #[serde(with = "bitmap_serde")]
    pub changed_old_lines: RoaringBitmap,
    #[serde(with = "bitmap_serde")]
    pub changed_new_lines: RoaringBitmap,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewHunk {
    pub id: StableHunkId,
    pub header: HunkHeader,
    pub unified_rows: Vec<UnifiedRow>,
    pub split_rows: Vec<SplitRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StableHunkId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkHeader {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub context: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Addition,
    Removal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffCell {
    pub side: DiffSide,
    pub line_number: u32,
    pub kind: DiffLineKind,
    /// Zero based source index, intentionally independent of virtualized rows.
    pub source_line_index: u32,
    pub text: String,
    /// `false` models Git's `\\ No newline at end of file` marker for this
    /// source-side line.  It is side-specific because a replacement can add
    /// or remove the final newline without changing the visible text.
    #[serde(default = "default_true")]
    pub has_trailing_newline: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedRow {
    pub id: String,
    pub kind: DiffLineKind,
    pub old: Option<DiffCell>,
    pub new: Option<DiffCell>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitRow {
    pub id: String,
    pub old: Option<DiffCell>,
    pub new: Option<DiffCell>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullFileRow {
    pub side: DiffSide,
    pub line_number: u32,
    pub text: String,
    pub changed: bool,
    /// Whether this source line has its terminating newline.  Renderers can
    /// use this to faithfully show a missing-final-newline marker without
    /// inventing an extra blank line.
    #[serde(default = "default_true")]
    pub has_trailing_newline: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DiffError {
    #[error("patch did not start with a file header")]
    MissingFileHeader,
    #[error("invalid unified hunk header: {0}")]
    InvalidHunkHeader(String),
    #[error("patch hunk line did not have a recognised prefix")]
    InvalidHunkLine,
    #[error(
        "unified hunk count did not match header: expected -{old_expected} +{new_expected}, found -{old_actual} +{new_actual}"
    )]
    HunkLineCountMismatch {
        old_expected: u32,
        new_expected: u32,
        old_actual: u32,
        new_actual: u32,
    },
    #[error("missing hunk line for `No newline at end of file` marker")]
    MissingNoNewlineTarget,
}

const fn default_true() -> bool {
    true
}

/// Produces a complete canonical document from whole-file sources. It is used
/// for Full File mode and supports stable hunk IDs across an unchanged refresh.
#[must_use]
pub fn document_from_sources(
    comparison_id: ComparisonId,
    file: ReviewFile,
    old_content: impl Into<String>,
    new_content: impl Into<String>,
) -> ReviewDiffDocument {
    let old = SourceDocument::new(old_content);
    let new = SourceDocument::new(new_content);
    let diff = TextDiff::from_lines(&old.content, &new.content);
    let mut changed_old_lines = RoaringBitmap::new();
    let mut changed_new_lines = RoaringBitmap::new();
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(3) {
        let mut rows = Vec::new();
        let mut old_start = None;
        let mut new_start = None;
        let mut old_empty_range_start = None;
        let mut new_empty_range_start = None;
        let mut old_count = 0_u32;
        let mut new_count = 0_u32;
        for operation in group {
            for change in diff.iter_changes(&operation) {
                let tag = change.tag();
                let has_trailing_newline = change.value().ends_with('\n');
                let text = change
                    .value()
                    .strip_suffix('\n')
                    .unwrap_or(change.value())
                    .to_owned();
                let (kind, old_cell, new_cell) = match tag {
                    ChangeTag::Equal => {
                        let old_index = u32::try_from(change.old_index().unwrap_or_default())
                            .unwrap_or(u32::MAX);
                        let new_index = u32::try_from(change.new_index().unwrap_or_default())
                            .unwrap_or(u32::MAX);
                        old_start.get_or_insert(old_index + 1);
                        new_start.get_or_insert(new_index + 1);
                        old_count += 1;
                        new_count += 1;
                        (
                            DiffLineKind::Context,
                            Some(cell(
                                DiffSide::Old,
                                old_index,
                                DiffLineKind::Context,
                                text.clone(),
                                has_trailing_newline,
                            )),
                            Some(cell(
                                DiffSide::New,
                                new_index,
                                DiffLineKind::Context,
                                text,
                                has_trailing_newline,
                            )),
                        )
                    }
                    ChangeTag::Delete => {
                        let old_index = u32::try_from(change.old_index().unwrap_or_default())
                            .unwrap_or(u32::MAX);
                        old_start.get_or_insert(old_index + 1);
                        new_empty_range_start.get_or_insert(
                            u32::try_from(change.new_index().unwrap_or(new.lines().count()))
                                .unwrap_or(u32::MAX),
                        );
                        old_count += 1;
                        changed_old_lines.insert(old_index + 1);
                        (
                            DiffLineKind::Removal,
                            Some(cell(
                                DiffSide::Old,
                                old_index,
                                DiffLineKind::Removal,
                                text,
                                has_trailing_newline,
                            )),
                            None,
                        )
                    }
                    ChangeTag::Insert => {
                        let new_index = u32::try_from(change.new_index().unwrap_or_default())
                            .unwrap_or(u32::MAX);
                        old_empty_range_start.get_or_insert(
                            u32::try_from(change.old_index().unwrap_or(old.lines().count()))
                                .unwrap_or(u32::MAX),
                        );
                        new_start.get_or_insert(new_index + 1);
                        new_count += 1;
                        changed_new_lines.insert(new_index + 1);
                        (
                            DiffLineKind::Addition,
                            None,
                            Some(cell(
                                DiffSide::New,
                                new_index,
                                DiffLineKind::Addition,
                                text,
                                has_trailing_newline,
                            )),
                        )
                    }
                };
                let id = stable_row_id(
                    &file,
                    old_cell.as_ref(),
                    new_cell.as_ref(),
                    kind,
                    rows.len(),
                );
                rows.push(UnifiedRow {
                    id,
                    kind,
                    old: old_cell,
                    new: new_cell,
                });
            }
        }
        if !rows.is_empty() {
            let header = HunkHeader {
                old_start: if old_count == 0 {
                    old_empty_range_start.unwrap_or(0)
                } else {
                    old_start.unwrap_or(1)
                },
                old_count,
                new_start: if new_count == 0 {
                    new_empty_range_start.unwrap_or(0)
                } else {
                    new_start.unwrap_or(1)
                },
                new_count,
                context: None,
            };
            hunks.push(make_hunk(&file, header, rows));
        }
    }
    ReviewDiffDocument {
        comparison_id,
        file,
        old,
        new,
        hunks,
        changed_old_lines,
        changed_new_lines,
    }
}

/// Parses regular Git unified patch text into the canonical row model. Complete
/// source is not available in a patch, so its source documents contain the
/// reconstructed hunk snippets; Git snapshot blobs should be supplied for Full
/// File mode when available.
pub fn parse_unified_patch(
    patch: &str,
    comparison_id: ComparisonId,
) -> Result<Vec<ReviewDiffDocument>, DiffError> {
    let mut documents = Vec::new();
    let mut current: Option<PatchFileBuilder> = None;
    for raw_line in patch.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        if let Some(paths) = line.strip_prefix("diff --git ") {
            if let Some(builder) = current.take() {
                documents.push(builder.finish(comparison_id)?);
            }
            let (old_path, new_path) = parse_diff_git_paths(paths)?;
            current = Some(PatchFileBuilder::new(old_path, new_path));
            continue;
        }
        let Some(builder) = current.as_mut() else {
            continue;
        };
        if let Some(path) = line.strip_prefix("rename from ") {
            builder.old_path = Some(parse_git_path(path)?);
        } else if let Some(path) = line.strip_prefix("rename to ") {
            builder.path = parse_git_path(path)?;
            builder.status = ReviewFileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("--- ") {
            if path == "/dev/null" {
                builder.status = ReviewFileStatus::Added;
            } else {
                builder.old_path = Some(strip_git_patch_prefix(path)?);
            }
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if path == "/dev/null" {
                builder.status = ReviewFileStatus::Deleted;
            } else {
                builder.path = strip_git_patch_prefix(path)?;
            }
        } else if line.starts_with("@@ ") {
            builder.start_hunk(line)?;
        } else if line.starts_with([' ', '+', '-']) {
            builder.push_hunk_line(line)?;
        } else if line == "\\ No newline at end of file" {
            // This describes the preceding source line and does not consume a line number.
            builder.mark_last_line_missing_newline()?;
        }
    }
    if let Some(builder) = current {
        documents.push(builder.finish(comparison_id)?);
    }
    if documents.is_empty() && !patch.is_empty() {
        return Err(DiffError::MissingFileHeader);
    }
    Ok(documents)
}

#[must_use]
pub fn full_file_rows(document: &ReviewDiffDocument, side: DiffSide) -> Vec<FullFileRow> {
    let (source, changed) = match side {
        DiffSide::Old => (&document.old, &document.changed_old_lines),
        DiffSide::New => (&document.new, &document.changed_new_lines),
    };
    source
        .lines()
        .enumerate()
        .map(|(index, text)| {
            let line_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            FullFileRow {
                side,
                line_number,
                text: text.to_owned(),
                changed: changed.contains(line_number),
                has_trailing_newline: line_number < source.line_count || source.has_final_newline,
            }
        })
        .collect()
}

fn cell(
    side: DiffSide,
    source_index: u32,
    kind: DiffLineKind,
    text: String,
    has_trailing_newline: bool,
) -> DiffCell {
    DiffCell {
        side,
        line_number: source_index + 1,
        kind,
        source_line_index: source_index,
        text,
        has_trailing_newline,
    }
}

fn stable_row_id(
    file: &ReviewFile,
    old: Option<&DiffCell>,
    new: Option<&DiffCell>,
    kind: DiffLineKind,
    ordinal: usize,
) -> String {
    let seed = format!(
        "{}\0{:?}\0{}\0{}\0{}\0{}\0{}\0{ordinal}",
        file.path.as_str(),
        kind,
        old.map_or(0, |value| value.line_number),
        new.map_or(0, |value| value.line_number),
        old.or(new).map_or("", |value| value.text.as_str()),
        old.is_some_and(|value| value.has_trailing_newline),
        new.is_some_and(|value| value.has_trailing_newline),
    );
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}

fn make_hunk(file: &ReviewFile, header: HunkHeader, unified_rows: Vec<UnifiedRow>) -> ReviewHunk {
    let mut stable_material = format!(
        "{}\0{}\0{}\0{}\0{}",
        file.path.as_str(),
        header.old_start,
        header.old_count,
        header.new_start,
        header.new_count
    );
    for row in &unified_rows {
        stable_material.push_str(&row.id);
    }
    let id = StableHunkId(
        blake3::hash(stable_material.as_bytes())
            .to_hex()
            .to_string(),
    );
    let split_rows = build_split_rows(&id, &unified_rows);
    ReviewHunk {
        id,
        header,
        unified_rows,
        split_rows,
    }
}

fn build_split_rows(hunk_id: &StableHunkId, unified_rows: &[UnifiedRow]) -> Vec<SplitRow> {
    let mut result = Vec::new();
    let mut removals = Vec::new();
    let mut additions = Vec::new();
    let flush = |result: &mut Vec<SplitRow>,
                 removals: &mut Vec<DiffCell>,
                 additions: &mut Vec<DiffCell>| {
        let length = cmp::max(removals.len(), additions.len());
        for index in 0..length {
            let old = removals.get(index).cloned();
            let new = additions.get(index).cloned();
            let id = split_id(hunk_id, result.len(), old.as_ref(), new.as_ref());
            result.push(SplitRow { id, old, new });
        }
        removals.clear();
        additions.clear();
    };
    for row in unified_rows {
        match row.kind {
            DiffLineKind::Removal => removals.extend(row.old.clone()),
            DiffLineKind::Addition => additions.extend(row.new.clone()),
            DiffLineKind::Context => {
                flush(&mut result, &mut removals, &mut additions);
                let id = split_id(hunk_id, result.len(), row.old.as_ref(), row.new.as_ref());
                result.push(SplitRow {
                    id,
                    old: row.old.clone(),
                    new: row.new.clone(),
                });
            }
        }
    }
    flush(&mut result, &mut removals, &mut additions);
    result
}

fn split_id(
    hunk_id: &StableHunkId,
    ordinal: usize,
    old: Option<&DiffCell>,
    new: Option<&DiffCell>,
) -> String {
    let seed = format!(
        "{}\0{ordinal}\0{}\0{}",
        hunk_id.0,
        old.map_or(0, |cell| cell.line_number),
        new.map_or(0, |cell| cell.line_number)
    );
    blake3::hash(seed.as_bytes()).to_hex().to_string()
}

struct PatchFileBuilder {
    path: StoredPath,
    old_path: Option<StoredPath>,
    status: ReviewFileStatus,
    hunks: Vec<(HunkHeader, Vec<UnifiedRow>)>,
    active_header: Option<HunkHeader>,
    active_rows: Vec<UnifiedRow>,
    old_snippet: String,
    new_snippet: String,
    old_line: u32,
    new_line: u32,
    changed_old: RoaringBitmap,
    changed_new: RoaringBitmap,
}

impl PatchFileBuilder {
    fn new(old_path: StoredPath, new_path: StoredPath) -> Self {
        Self {
            path: new_path.clone(),
            old_path: Some(old_path),
            status: ReviewFileStatus::Modified,
            hunks: Vec::new(),
            active_header: None,
            active_rows: Vec::new(),
            old_snippet: String::new(),
            new_snippet: String::new(),
            old_line: 0,
            new_line: 0,
            changed_old: RoaringBitmap::new(),
            changed_new: RoaringBitmap::new(),
        }
    }

    fn start_hunk(&mut self, line: &str) -> Result<(), DiffError> {
        self.finish_active_hunk()?;
        let header = parse_hunk_header(line)?;
        self.old_line = header.old_start;
        self.new_line = header.new_start;
        self.active_header = Some(header);
        Ok(())
    }

    fn push_hunk_line(&mut self, line: &str) -> Result<(), DiffError> {
        if self.active_header.is_none() {
            return Ok(());
        }
        let (prefix, text) = line.split_at(1);
        let (kind, old, new) = match prefix {
            " " => {
                let old = cell(
                    DiffSide::Old,
                    self.old_line.saturating_sub(1),
                    DiffLineKind::Context,
                    text.to_owned(),
                    true,
                );
                let new = cell(
                    DiffSide::New,
                    self.new_line.saturating_sub(1),
                    DiffLineKind::Context,
                    text.to_owned(),
                    true,
                );
                self.old_line += 1;
                self.new_line += 1;
                self.old_snippet.push_str(text);
                self.old_snippet.push('\n');
                self.new_snippet.push_str(text);
                self.new_snippet.push('\n');
                (DiffLineKind::Context, Some(old), Some(new))
            }
            "-" => {
                let old = cell(
                    DiffSide::Old,
                    self.old_line.saturating_sub(1),
                    DiffLineKind::Removal,
                    text.to_owned(),
                    true,
                );
                self.changed_old.insert(self.old_line);
                self.old_line += 1;
                self.old_snippet.push_str(text);
                self.old_snippet.push('\n');
                (DiffLineKind::Removal, Some(old), None)
            }
            "+" => {
                let new = cell(
                    DiffSide::New,
                    self.new_line.saturating_sub(1),
                    DiffLineKind::Addition,
                    text.to_owned(),
                    true,
                );
                self.changed_new.insert(self.new_line);
                self.new_line += 1;
                self.new_snippet.push_str(text);
                self.new_snippet.push('\n');
                (DiffLineKind::Addition, None, Some(new))
            }
            _ => return Err(DiffError::InvalidHunkLine),
        };
        let file = ReviewFile {
            id: ReviewFileId::new(),
            path: self.path.clone(),
            old_path: self.old_path.clone(),
            status: self.status,
        };
        let id = stable_row_id(
            &file,
            old.as_ref(),
            new.as_ref(),
            kind,
            self.active_rows.len(),
        );
        self.active_rows.push(UnifiedRow { id, kind, old, new });
        Ok(())
    }

    fn mark_last_line_missing_newline(&mut self) -> Result<(), DiffError> {
        let (marks_old, marks_new) = {
            let row = self
                .active_rows
                .last_mut()
                .ok_or(DiffError::MissingNoNewlineTarget)?;
            if let Some(old) = row.old.as_mut() {
                old.has_trailing_newline = false;
            }
            if let Some(new) = row.new.as_mut() {
                new.has_trailing_newline = false;
            }
            (row.old.is_some(), row.new.is_some())
        };
        if !marks_old && !marks_new {
            return Err(DiffError::MissingNoNewlineTarget);
        }
        if marks_old {
            remove_final_newline(&mut self.old_snippet)?;
        }
        if marks_new {
            remove_final_newline(&mut self.new_snippet)?;
        }

        let file = ReviewFile {
            id: ReviewFileId::new(),
            path: self.path.clone(),
            old_path: self.old_path.clone(),
            status: self.status,
        };
        let ordinal = self.active_rows.len().saturating_sub(1);
        let row = self
            .active_rows
            .last_mut()
            .ok_or(DiffError::MissingNoNewlineTarget)?;
        row.id = stable_row_id(&file, row.old.as_ref(), row.new.as_ref(), row.kind, ordinal);
        Ok(())
    }

    fn finish_active_hunk(&mut self) -> Result<(), DiffError> {
        if let Some(header) = self.active_header.take() {
            let (old_actual, new_actual) = hunk_line_counts(&self.active_rows);
            if old_actual != header.old_count || new_actual != header.new_count {
                return Err(DiffError::HunkLineCountMismatch {
                    old_expected: header.old_count,
                    new_expected: header.new_count,
                    old_actual,
                    new_actual,
                });
            }
            self.hunks
                .push((header, std::mem::take(&mut self.active_rows)));
        }
        Ok(())
    }

    fn finish(mut self, comparison_id: ComparisonId) -> Result<ReviewDiffDocument, DiffError> {
        self.finish_active_hunk()?;
        // Mode-only changes, binary patches, pure renames/copies, and
        // submodule pointer changes are legitimate Git file records with no
        // textual hunk. Keep them navigable instead of failing the whole
        // repository capture.
        let file = ReviewFile {
            id: ReviewFileId::new(),
            path: self.path,
            old_path: self.old_path,
            status: self.status,
        };
        let hunks = self
            .hunks
            .into_iter()
            .map(|(header, rows)| make_hunk(&file, header, rows))
            .collect();
        Ok(ReviewDiffDocument {
            comparison_id,
            file,
            old: SourceDocument::new(self.old_snippet),
            new: SourceDocument::new(self.new_snippet),
            hunks,
            changed_old_lines: self.changed_old,
            changed_new_lines: self.changed_new,
        })
    }
}

fn remove_final_newline(snippet: &mut String) -> Result<(), DiffError> {
    if snippet.pop() == Some('\n') {
        Ok(())
    } else {
        Err(DiffError::MissingNoNewlineTarget)
    }
}

fn hunk_line_counts(rows: &[UnifiedRow]) -> (u32, u32) {
    let old = rows
        .iter()
        .filter(|row| row.old.is_some())
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    let new = rows
        .iter()
        .filter(|row| row.new.is_some())
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    (old, new)
}

fn parse_diff_git_paths(value: &str) -> Result<(StoredPath, StoredPath), DiffError> {
    let values = parse_git_header_tokens(value)?;
    let (old, new) = if values.len() == 2 {
        (values[0].clone(), values[1].clone())
    } else {
        // Git emits quoted paths for this case, but accepting old hand-made
        // patches that contain unquoted spaces keeps the parser compatible.
        let split = values
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, token)| token.starts_with("b/").then_some(index))
            .ok_or(DiffError::MissingFileHeader)?;
        (values[..split].join(" "), values[split..].join(" "))
    };
    Ok((
        StoredPath::from(strip_patch_prefix(&old)),
        StoredPath::from(strip_patch_prefix(&new)),
    ))
}

fn strip_git_patch_prefix(value: &str) -> Result<StoredPath, DiffError> {
    let path = parse_git_path(value)?;
    Ok(StoredPath::from(strip_patch_prefix(path.as_str())))
}

fn parse_git_path(value: &str) -> Result<StoredPath, DiffError> {
    // Extended-header paths (`rename from`, `rename to`, `---`, `+++`) are a
    // single path to end-of-line and Git may leave spaces unquoted there.
    // Only decode/tokenize when Git chose double-quoted C-style notation.
    if !value.starts_with('"') {
        return Ok(StoredPath::from(value));
    }
    let tokens = parse_git_header_tokens(value)?;
    let [path]: [String; 1] = tokens
        .try_into()
        .map_err(|_| DiffError::MissingFileHeader)?;
    Ok(StoredPath::from(path))
}

/// Git quotes whitespace/control-heavy patch paths with C-style double
/// quotes.  Decode those paths instead of splitting the human-readable patch
/// header on whitespace.  Normal UTF-8 names remain untouched.
fn parse_git_header_tokens(value: &str) -> Result<Vec<String>, DiffError> {
    let bytes = value.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        if bytes[index] != b'"' {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            tokens.push(value[start..index].to_owned());
            continue;
        }
        index += 1;
        let mut raw = Vec::new();
        let mut closed = false;
        while index < bytes.len() {
            match bytes[index] {
                b'"' => {
                    index += 1;
                    closed = true;
                    break;
                }
                b'\\' => {
                    index += 1;
                    let escaped = *bytes.get(index).ok_or(DiffError::MissingFileHeader)?;
                    match escaped {
                        b'\\' | b'"' => raw.push(escaped),
                        b'n' => raw.push(b'\n'),
                        b't' => raw.push(b'\t'),
                        b'r' => raw.push(b'\r'),
                        b'0'..=b'7' => {
                            let mut value = escaped - b'0';
                            for _ in 0..2 {
                                index += 1;
                                let digit =
                                    *bytes.get(index).ok_or(DiffError::MissingFileHeader)?;
                                if !(b'0'..=b'7').contains(&digit) {
                                    return Err(DiffError::MissingFileHeader);
                                }
                                value = value.saturating_mul(8).saturating_add(digit - b'0');
                            }
                            raw.push(value);
                        }
                        other => raw.push(other),
                    }
                    index += 1;
                }
                byte => {
                    raw.push(byte);
                    index += 1;
                }
            }
        }
        if !closed {
            return Err(DiffError::MissingFileHeader);
        }
        tokens.push(String::from_utf8_lossy(&raw).into_owned());
    }
    Ok(tokens)
}

fn strip_patch_prefix(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

fn parse_hunk_header(line: &str) -> Result<HunkHeader, DiffError> {
    let body = line
        .strip_prefix("@@ ")
        .ok_or_else(|| DiffError::InvalidHunkHeader(line.to_owned()))?;
    let (ranges, context) = body
        .split_once(" @@")
        .ok_or_else(|| DiffError::InvalidHunkHeader(line.to_owned()))?;
    let mut ranges = ranges.split_whitespace();
    let old = parse_range(
        ranges
            .next()
            .ok_or_else(|| DiffError::InvalidHunkHeader(line.to_owned()))?,
        '-',
    )?;
    let new = parse_range(
        ranges
            .next()
            .ok_or_else(|| DiffError::InvalidHunkHeader(line.to_owned()))?,
        '+',
    )?;
    if ranges.next().is_some() {
        return Err(DiffError::InvalidHunkHeader(line.to_owned()));
    }
    let context = context.strip_prefix(' ').unwrap_or(context);
    Ok(HunkHeader {
        old_start: old.0,
        old_count: old.1,
        new_start: new.0,
        new_count: new.1,
        context: (!context.is_empty()).then(|| context.to_owned()),
    })
}

fn parse_range(raw: &str, prefix: char) -> Result<(u32, u32), DiffError> {
    let value = raw
        .strip_prefix(prefix)
        .ok_or_else(|| DiffError::InvalidHunkHeader(raw.to_owned()))?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    let start = start
        .parse()
        .map_err(|_| DiffError::InvalidHunkHeader(raw.to_owned()))?;
    let count = count
        .parse()
        .map_err(|_| DiffError::InvalidHunkHeader(raw.to_owned()))?;
    Ok((start, count))
}

mod bitmap_serde {
    use roaring::RoaringBitmap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &RoaringBitmap,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<RoaringBitmap, D::Error> {
        let values = Vec::<u32>::deserialize(deserializer)?;
        Ok(values.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_file() -> ReviewFile {
        ReviewFile {
            id: ReviewFileId::new(),
            path: StoredPath::from("src/lib.rs"),
            old_path: None,
            status: ReviewFileStatus::Modified,
        }
    }

    #[test]
    fn unified_and_split_keep_real_sides_and_line_numbers() {
        let document = document_from_sources(
            ComparisonId::new(),
            review_file(),
            "one\ntwo\n",
            "one\nthree\nfour\n",
        );
        assert!(document.changed_old_lines.contains(2));
        assert!(document.changed_new_lines.contains(2));
        assert!(document.changed_new_lines.contains(3));
        let hunk = &document.hunks[0];
        let split_changed = hunk
            .split_rows
            .iter()
            .find(|row| row.old.as_ref().is_some_and(|cell| cell.line_number == 2))
            .unwrap();
        assert_eq!(split_changed.old.as_ref().unwrap().side, DiffSide::Old);
        assert_eq!(split_changed.new.as_ref().unwrap().side, DiffSide::New);
        assert_eq!(split_changed.new.as_ref().unwrap().line_number, 2);
        assert!(hunk
            .split_rows
            .iter()
            .any(|row| row.old.is_none()
                && row.new.as_ref().is_some_and(|cell| cell.line_number == 3)));
    }

    #[test]
    fn equivalent_inputs_produce_stable_hunk_ids() {
        let left = document_from_sources(ComparisonId::new(), review_file(), "a\nb\n", "a\nc\n");
        let right = document_from_sources(ComparisonId::new(), review_file(), "a\nb\n", "a\nc\n");
        assert_eq!(left.hunks[0].id, right.hunks[0].id);
    }

    #[test]
    fn full_file_marks_only_new_changed_lines() {
        let document =
            document_from_sources(ComparisonId::new(), review_file(), "a\nb\n", "a\nc\n");
        let rows = full_file_rows(&document, DiffSide::New);
        assert!(!rows[0].changed);
        assert!(rows[1].changed);
    }

    #[test]
    fn patch_parser_uses_git_line_numbers_and_rename_identity() {
        let patch = "diff --git a/old name.rs b/new name.rs\nsimilarity index 90%\nrename from old name.rs\nrename to new name.rs\n@@ -10,2 +10,3 @@ fn demo()\n same\n-old\n+new\n+extra\n";
        let documents = parse_unified_patch(patch, ComparisonId::new()).unwrap();
        let document = &documents[0];
        assert_eq!(document.file.status, ReviewFileStatus::Renamed);
        assert_eq!(document.file.path.as_str(), "new name.rs");
        assert_eq!(
            document.file.old_path.as_ref().unwrap().as_str(),
            "old name.rs"
        );
        assert!(document.changed_old_lines.contains(11));
        assert!(document.changed_new_lines.contains(11));
        assert!(document.changed_new_lines.contains(12));
        assert_eq!(
            document.hunks[0].header.context.as_deref(),
            Some("fn demo()")
        );
    }

    #[test]
    fn patch_parser_decodes_git_quoted_space_and_unicode_paths_and_keeps_no_hunk_file() {
        let patch = "diff --git \"a/old name \\303\\274.rs\" \"b/new name \\303\\274.rs\"\nsimilarity index 100%\nrename from old name ü.rs\nrename to new name ü.rs\ndiff --git a/script b/script\nold mode 100644\nnew mode 100755\n";
        let documents = parse_unified_patch(patch, ComparisonId::new()).unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].file.path.as_str(), "new name ü.rs");
        assert_eq!(
            documents[0].file.old_path.as_ref().unwrap().as_str(),
            "old name ü.rs"
        );
        assert!(documents[0].hunks.is_empty());
        assert_eq!(documents[1].file.path.as_str(), "script");
        assert!(documents[1].hunks.is_empty());
    }

    #[test]
    fn split_alignment_preserves_every_unified_side_for_all_change_shapes() {
        let cases = [
            ("one\n", "two\n", 1_usize, 1_usize),
            ("one\n", "two\nthree\n", 1, 2),
            ("one\ntwo\n", "three\n", 2, 1),
            ("", "one\ntwo\n", 0, 2),
            ("one\ntwo\n", "", 2, 0),
        ];

        for (old, new, expected_old, expected_new) in cases {
            let document = document_from_sources(ComparisonId::new(), review_file(), old, new);
            let hunk = document.hunks.first().unwrap();
            assert_split_is_lossless(hunk);

            let changed = hunk
                .split_rows
                .iter()
                .filter(|row| row.old.is_some() || row.new.is_some())
                .count();
            assert_eq!(changed, expected_old.max(expected_new));
            assert_eq!(
                hunk.split_rows
                    .iter()
                    .filter(|row| row.old.is_none())
                    .count(),
                expected_new.saturating_sub(expected_old)
            );
            assert_eq!(
                hunk.split_rows
                    .iter()
                    .filter(|row| row.new.is_none())
                    .count(),
                expected_old.saturating_sub(expected_new)
            );
        }
    }

    #[test]
    fn whole_source_diff_uses_git_zero_count_hunk_positions() {
        let added =
            document_from_sources(ComparisonId::new(), review_file(), "", "first\nsecond\n");
        assert_eq!(
            added.hunks[0].header,
            HunkHeader {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                context: None,
            }
        );

        let deleted =
            document_from_sources(ComparisonId::new(), review_file(), "first\nsecond\n", "");
        assert_eq!(
            deleted.hunks[0].header,
            HunkHeader {
                old_start: 1,
                old_count: 2,
                new_start: 0,
                new_count: 0,
                context: None,
            }
        );
    }

    #[test]
    fn repeated_lines_keep_real_positions_and_distinct_stable_row_ids() {
        let document = document_from_sources(
            ComparisonId::new(),
            review_file(),
            "same\nsame\nsame\n",
            "same\nother\nother\n",
        );
        let hunk = &document.hunks[0];
        assert_split_is_lossless(hunk);

        let removal_ids = hunk
            .unified_rows
            .iter()
            .filter(|row| row.kind == DiffLineKind::Removal)
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(removal_ids.len(), 2);
        assert_ne!(removal_ids[0], removal_ids[1]);
        assert_eq!(
            hunk.unified_rows
                .iter()
                .filter_map(|row| row.old.as_ref())
                .map(|cell| cell.line_number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let refreshed = document_from_sources(
            ComparisonId::new(),
            review_file(),
            "same\nsame\nsame\n",
            "same\nother\nother\n",
        );
        assert_eq!(document.hunks[0].id, refreshed.hunks[0].id);
        assert_eq!(
            document.hunks[0]
                .unified_rows
                .iter()
                .map(|row| &row.id)
                .collect::<Vec<_>>(),
            refreshed.hunks[0]
                .unified_rows
                .iter()
                .map(|row| &row.id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parser_rejects_hunks_whose_declared_counts_do_not_match_rows() {
        let too_few_rows = "diff --git a/file.rs b/file.rs\n@@ -1,2 +1 @@\n-old\n+new\n";
        assert!(matches!(
            parse_unified_patch(too_few_rows, ComparisonId::new()),
            Err(DiffError::HunkLineCountMismatch {
                old_expected: 2,
                new_expected: 1,
                old_actual: 1,
                new_actual: 1,
            })
        ));

        let too_many_rows = "diff --git a/file.rs b/file.rs\n@@ -1 +1 @@\n-old\n+new\n+extra\n";
        assert!(matches!(
            parse_unified_patch(too_many_rows, ComparisonId::new()),
            Err(DiffError::HunkLineCountMismatch {
                old_expected: 1,
                new_expected: 1,
                old_actual: 1,
                new_actual: 2,
            })
        ));
    }

    #[test]
    fn parser_preserves_missing_final_newline_per_source_side() {
        let patch = "diff --git a/file.rs b/file.rs\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
        let document = parse_unified_patch(patch, ComparisonId::new())
            .unwrap()
            .pop()
            .unwrap();

        assert_eq!(document.old.content, "old");
        assert_eq!(document.new.content, "new");
        assert!(!document.old.has_final_newline);
        assert!(!document.new.has_final_newline);
        let hunk = &document.hunks[0];
        assert!(
            !hunk.unified_rows[0]
                .old
                .as_ref()
                .unwrap()
                .has_trailing_newline
        );
        assert!(
            !hunk.unified_rows[1]
                .new
                .as_ref()
                .unwrap()
                .has_trailing_newline
        );
        assert!(!full_file_rows(&document, DiffSide::Old)[0].has_trailing_newline);
        assert!(!full_file_rows(&document, DiffSide::New)[0].has_trailing_newline);

        let with_added_newline =
            document_from_sources(ComparisonId::new(), review_file(), "tail", "tail\n");
        assert!(!with_added_newline.old.has_final_newline);
        assert!(with_added_newline.new.has_final_newline);
        assert_ne!(
            with_added_newline.old.fingerprint,
            with_added_newline.new.fingerprint
        );
        assert_ne!(
            with_added_newline.hunks[0].unified_rows[0].id,
            with_added_newline.hunks[0].unified_rows[1].id
        );
    }

    #[test]
    fn full_file_rows_cover_deleted_and_renamed_files_with_trailing_empty_lines() {
        let deleted_file = ReviewFile {
            id: ReviewFileId::new(),
            path: StoredPath::from("removed.rs"),
            old_path: Some(StoredPath::from("removed.rs")),
            status: ReviewFileStatus::Deleted,
        };
        let deleted =
            document_from_sources(ComparisonId::new(), deleted_file, "first\nsecond\n", "");
        let old_rows = full_file_rows(&deleted, DiffSide::Old);
        assert_eq!(
            old_rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert!(old_rows.iter().all(|row| row.changed));
        assert!(full_file_rows(&deleted, DiffSide::New).is_empty());

        let renamed_patch = "diff --git a/old name.rs b/new name.rs\nsimilarity index 50%\nrename from old name.rs\nrename to new name.rs\n@@ -1,2 +1,2 @@\n keep\n-old\n+new\n";
        let renamed = parse_unified_patch(renamed_patch, ComparisonId::new())
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(renamed.file.status, ReviewFileStatus::Renamed);
        assert_eq!(renamed.file.path.as_str(), "new name.rs");
        assert_eq!(renamed.file.old_path.unwrap().as_str(), "old name.rs");
        assert_split_is_lossless(&renamed.hunks[0]);

        let trailing_empty = document_from_sources(
            ComparisonId::new(),
            review_file(),
            "first\n\n",
            "first\n\nthird\n",
        );
        let old_rows = full_file_rows(&trailing_empty, DiffSide::Old);
        assert_eq!(old_rows.len(), 2);
        assert_eq!(old_rows[1].text, "");
        assert!(old_rows[1].has_trailing_newline);
        let new_rows = full_file_rows(&trailing_empty, DiffSide::New);
        assert_eq!(
            new_rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "", "third"]
        );
        assert!(new_rows[2].has_trailing_newline);
    }

    #[test]
    fn fifty_thousand_line_two_megabyte_fixture_stays_sparse_and_navigable() {
        let mut old = String::with_capacity(2 * 1024 * 1024);
        let mut new = String::with_capacity(2 * 1024 * 1024);
        for line in 1..=50_000_u32 {
            let original = format!("line {line:05}: stable review fixture payload\n");
            old.push_str(&original);
            if line % 5_000 == 0 {
                new.push_str(&format!("line {line:05}: changed review fixture payload\n"));
            } else {
                new.push_str(&original);
            }
        }
        assert!(old.len() >= 2_000_000 - 250_000);

        let document = document_from_sources(ComparisonId::new(), review_file(), old, new);
        assert_eq!(document.old.line_count, 50_000);
        assert_eq!(document.new.line_count, 50_000);
        assert_eq!(document.changed_old_lines.len(), 10);
        assert_eq!(document.changed_new_lines.len(), 10);
        assert_eq!(full_file_rows(&document, DiffSide::New).len(), 50_000);
        assert!(document.hunks.len() <= 10);
    }

    fn assert_split_is_lossless(hunk: &ReviewHunk) {
        let expected_old = hunk
            .unified_rows
            .iter()
            .filter_map(|row| row.old.as_ref())
            .map(cell_identity)
            .collect::<Vec<_>>();
        let expected_new = hunk
            .unified_rows
            .iter()
            .filter_map(|row| row.new.as_ref())
            .map(cell_identity)
            .collect::<Vec<_>>();
        let actual_old = hunk
            .split_rows
            .iter()
            .filter_map(|row| row.old.as_ref())
            .map(cell_identity)
            .collect::<Vec<_>>();
        let actual_new = hunk
            .split_rows
            .iter()
            .filter_map(|row| row.new.as_ref())
            .map(cell_identity)
            .collect::<Vec<_>>();
        assert_eq!(actual_old, expected_old);
        assert_eq!(actual_new, expected_new);
        assert_eq!(
            hunk.split_rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            hunk.split_rows.len()
        );
    }

    fn cell_identity(cell: &DiffCell) -> (DiffSide, u32, u32, DiffLineKind, &str, bool) {
        (
            cell.side,
            cell.line_number,
            cell.source_line_index,
            cell.kind,
            cell.text.as_str(),
            cell.has_trailing_newline,
        )
    }
}
