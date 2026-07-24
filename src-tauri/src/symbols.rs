//! Bounded, repository-relative symbol discovery for the separate navigation
//! window. Candidate lookup is lexical and lazy; only matching source files
//! are parsed with the pinned outline grammars.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
};

use localreview_domain::{is_safe_repository_relative_path, StoredPath};
use localreview_highlight::{outline, OutlineKind};
use localreview_tools::{git_executable, rg_executable};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub(crate) const MAX_SYMBOL_RESULTS: usize = 200;
pub(crate) const MAX_REPOSITORY_FILES: usize = 5_000;
const MAX_CANDIDATE_MATCHES: usize = 2_000;
const MAX_SEARCH_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SOURCE_WINDOW_LINES: u32 = 2_000;
const MAX_PREVIEW_CHARS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoreSymbolLocation {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub preview: String,
    pub kind: String,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CoreSymbolSearch {
    pub definitions: Vec<CoreSymbolLocation>,
    pub references: Vec<CoreSymbolLocation>,
    pub truncated: bool,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoreSourceWindow {
    pub path: String,
    pub fingerprint: String,
    pub start_line: u32,
    pub total_lines: u32,
    pub lines: Vec<(u32, String)>,
    pub line_start_bytes: Vec<u32>,
    pub source: String,
    pub byte_range: (u32, u32),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CoreRepositoryFileList {
    pub files: Vec<String>,
    pub truncated: bool,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Error)]
pub(crate) enum SymbolSearchError {
    #[error("symbol must be one supported identifier token of at most 256 bytes")]
    InvalidSymbol,
    #[error("symbol path must be a safe repository-relative path")]
    InvalidPath,
    #[error("repository worktree is unavailable")]
    RepositoryUnavailable,
    #[error("symbol source is unavailable or escapes its repository")]
    SourceUnavailable,
    #[error("symbol source changed; run the search again")]
    SourceChanged,
    #[error("symbol source is not bounded UTF-8 text")]
    UnsupportedSource,
    #[error("symbol search failed: {0}")]
    SearchFailed(String),
}

#[derive(Clone, Debug)]
struct LexicalMatch {
    path: String,
    line: u32,
    column: u32,
    end_column: u32,
    preview: String,
}

pub(crate) fn validate_symbol(value: &str) -> Result<(), SymbolSearchError> {
    if value.is_empty()
        || value.len() > 256
        || value.contains('\0')
        || value.chars().any(char::is_whitespace)
    {
        return Err(SymbolSearchError::InvalidSymbol);
    }
    let core = value.strip_prefix(['#', '@', '~']).unwrap_or(value);
    let core = core.strip_suffix(['!', '?', '\'']).unwrap_or(core);
    if core.is_empty() || !core.split("::").all(valid_identifier_segment) {
        return Err(SymbolSearchError::InvalidSymbol);
    }
    Ok(())
}

fn valid_identifier_segment(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first == '$' || first.is_alphabetic())
        && characters
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

pub(crate) fn search_repository(
    repository_root: &Path,
    symbol: &str,
) -> Result<CoreSymbolSearch, SymbolSearchError> {
    validate_symbol(symbol)?;
    let root =
        fs::canonicalize(repository_root).map_err(|_| SymbolSearchError::RepositoryUnavailable)?;
    if !root.is_dir() {
        return Err(SymbolSearchError::RepositoryUnavailable);
    }

    let (matches, truncated, mut diagnostics) = match ripgrep_matches(&root, symbol) {
        Ok(result) => result,
        Err(RipgrepUnavailable) => {
            let (matches, truncated) = git_grep_matches(&root, symbol)?;
            (
                matches,
                truncated,
                vec!["ripgrep was unavailable; Git tracked-file search was used.".into()],
            )
        }
    };
    let mut by_path = BTreeMap::<String, Vec<LexicalMatch>>::new();
    for candidate in matches {
        by_path
            .entry(candidate.path.clone())
            .or_default()
            .push(candidate);
    }

    let mut definitions = Vec::new();
    let mut references = Vec::new();
    for (path, matches) in by_path {
        let Some((source, fingerprint)) = read_repository_source(&root, &path)? else {
            diagnostics.push(format!("{path}: skipped non-text or oversized source."));
            continue;
        };
        let path_ref = Path::new(&path);
        let source_lines = source.lines().collect::<Vec<_>>();
        let mut definition_keys = BTreeSet::<(u32, u32)>::new();

        for symbol_entry in outline(path_ref, &source, None)
            .into_iter()
            .filter(|entry| outline_name_contains_symbol(&entry.name, symbol))
        {
            let preview = source_lines
                .get(symbol_entry.start_line.saturating_sub(1) as usize)
                .copied()
                .unwrap_or_default();
            let column = symbol_column(preview, symbol).unwrap_or(1);
            definition_keys.insert((symbol_entry.start_line, column));
            definitions.push(CoreSymbolLocation {
                path: path.clone(),
                line: symbol_entry.start_line,
                column,
                end_line: symbol_entry.end_line,
                end_column: column.saturating_add(
                    u32::try_from(symbol.encode_utf16().count()).unwrap_or(u32::MAX),
                ),
                preview: bounded_preview(preview),
                kind: outline_kind_name(symbol_entry.kind).into(),
                source_fingerprint: fingerprint.clone(),
            });
        }

        for candidate in matches {
            let heuristic_definition = definition_keys.is_empty()
                && looks_like_definition(&candidate.preview, symbol, candidate.column);
            if heuristic_definition {
                definition_keys.insert((candidate.line, candidate.column));
                definitions.push(CoreSymbolLocation {
                    path: path.clone(),
                    line: candidate.line,
                    column: candidate.column,
                    end_line: candidate.line,
                    end_column: candidate.end_column,
                    preview: candidate.preview,
                    kind: "definition".into(),
                    source_fingerprint: fingerprint.clone(),
                });
            } else if !definition_keys.contains(&(candidate.line, candidate.column)) {
                references.push(CoreSymbolLocation {
                    path: path.clone(),
                    line: candidate.line,
                    column: candidate.column,
                    end_line: candidate.line,
                    end_column: candidate.end_column,
                    preview: candidate.preview,
                    kind: "reference".into(),
                    source_fingerprint: fingerprint.clone(),
                });
            }
        }
    }
    definitions.sort_by(location_order);
    definitions.dedup_by(|left, right| {
        left.path == right.path && left.line == right.line && left.column == right.column
    });
    references.sort_by(location_order);
    references.dedup_by(|left, right| {
        left.path == right.path && left.line == right.line && left.column == right.column
    });
    Ok(CoreSymbolSearch {
        definitions,
        references,
        truncated,
        diagnostics,
    })
}

pub(crate) fn read_source_window(
    repository_root: &Path,
    path: &str,
    expected_fingerprint: &str,
    start_line: u32,
    line_count: u32,
) -> Result<CoreSourceWindow, SymbolSearchError> {
    source_window(
        repository_root,
        path,
        Some(expected_fingerprint),
        start_line,
        line_count,
    )
}

pub(crate) fn open_source_window(
    repository_root: &Path,
    path: &str,
    start_line: u32,
    line_count: u32,
) -> Result<CoreSourceWindow, SymbolSearchError> {
    source_window(repository_root, path, None, start_line, line_count)
}

fn source_window(
    repository_root: &Path,
    path: &str,
    expected_fingerprint: Option<&str>,
    start_line: u32,
    line_count: u32,
) -> Result<CoreSourceWindow, SymbolSearchError> {
    if start_line == 0 || line_count == 0 || line_count > MAX_SOURCE_WINDOW_LINES {
        return Err(SymbolSearchError::InvalidPath);
    }
    let root =
        fs::canonicalize(repository_root).map_err(|_| SymbolSearchError::RepositoryUnavailable)?;
    let Some((source, fingerprint)) = read_repository_source(&root, path)? else {
        return Err(SymbolSearchError::UnsupportedSource);
    };
    if expected_fingerprint.is_some_and(|expected| fingerprint != expected) {
        return Err(SymbolSearchError::SourceChanged);
    }
    let source_lines = source.lines().collect::<Vec<_>>();
    let mut source_line_starts = vec![0_usize];
    source_line_starts.extend(
        source
            .match_indices('\n')
            .map(|(offset, _)| offset.saturating_add(1)),
    );
    source_line_starts.truncate(source_lines.len());
    let total_lines = u32::try_from(source_lines.len()).unwrap_or(u32::MAX);
    let start = start_line.saturating_sub(1) as usize;
    let end = start
        .saturating_add(line_count as usize)
        .min(source_lines.len());
    let lines = source_lines[start.min(source_lines.len())..end]
        .iter()
        .enumerate()
        .map(|(index, line)| (start_line.saturating_add(index as u32), (*line).to_owned()))
        .collect();
    let line_start_bytes = source_line_starts[start.min(source_line_starts.len())..end]
        .iter()
        .map(|offset| u32::try_from(*offset).unwrap_or(u32::MAX))
        .collect();
    let byte_start = source_line_starts
        .get(start)
        .copied()
        .unwrap_or(source.len());
    let byte_end = source_line_starts.get(end).copied().unwrap_or(source.len());
    Ok(CoreSourceWindow {
        path: path.to_owned(),
        fingerprint,
        start_line,
        total_lines,
        lines,
        line_start_bytes,
        source,
        byte_range: (
            u32::try_from(byte_start).unwrap_or(u32::MAX),
            u32::try_from(byte_end).unwrap_or(u32::MAX),
        ),
    })
}

pub(crate) fn list_repository_files(
    repository_root: &Path,
    query: Option<&str>,
    limit: usize,
) -> Result<CoreRepositoryFileList, SymbolSearchError> {
    if limit == 0 || limit > MAX_REPOSITORY_FILES {
        return Err(SymbolSearchError::InvalidPath);
    }
    let root =
        fs::canonicalize(repository_root).map_err(|_| SymbolSearchError::RepositoryUnavailable)?;
    if !root.is_dir() {
        return Err(SymbolSearchError::RepositoryUnavailable);
    }
    let needle = query
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let (paths, truncated, mut diagnostics) =
        match repository_files_with_rg(&root, needle.as_deref(), limit) {
            Ok(result) => result,
            Err(RipgrepUnavailable) => {
                let (files, truncated) =
                    repository_files_with_git(&root, needle.as_deref(), limit)?;
                (
                    files,
                    truncated,
                    vec!["ripgrep was unavailable; Git repository files were used.".into()],
                )
            }
        };
    let mut files = paths;
    files.sort();
    files.dedup();
    if needle.is_some() && truncated {
        diagnostics
            .push("Path search covered only the bounded repository-file candidate set.".into());
    }
    Ok(CoreRepositoryFileList {
        files,
        truncated,
        diagnostics,
    })
}

fn ripgrep_matches(
    root: &Path,
    symbol: &str,
) -> Result<(Vec<LexicalMatch>, bool, Vec<String>), RipgrepUnavailable> {
    let mut child = Command::new(rg_executable())
        .current_dir(root)
        .args([
            "--json",
            "--line-number",
            "--column",
            "--fixed-strings",
            "--hidden",
            "--glob",
            "!.git/**",
            "--max-filesize",
            "4M",
            "--no-messages",
            symbol,
            ".",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RipgrepUnavailable)?;
    let stdout = child.stdout.take().ok_or(RipgrepUnavailable)?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut retained_bytes = 0_usize;
    let mut matches = Vec::new();
    let mut truncated = false;
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|_| RipgrepUnavailable)?;
        if read == 0 {
            break;
        }
        retained_bytes = retained_bytes.saturating_add(read);
        if retained_bytes > MAX_SEARCH_EVENT_BYTES || matches.len() >= MAX_CANDIDATE_MATCHES {
            truncated = true;
            let _ = child.kill();
            break;
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let Some(data) = event.get("data") else {
            continue;
        };
        let Some(path) = data
            .pointer("/path/text")
            .and_then(Value::as_str)
            .and_then(normalize_search_path)
        else {
            continue;
        };
        let Some(line_number) = data.get("line_number").and_then(Value::as_u64) else {
            continue;
        };
        let preview = data
            .pointer("/lines/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_end_matches(['\r', '\n']);
        let Some(submatches) = data.get("submatches").and_then(Value::as_array) else {
            continue;
        };
        for submatch in submatches {
            let Some(start) = submatch.get("start").and_then(Value::as_u64) else {
                continue;
            };
            let Some(end) = submatch.get("end").and_then(Value::as_u64) else {
                continue;
            };
            if !is_identifier_occurrence(preview, start as usize, end as usize) {
                continue;
            }
            matches.push(LexicalMatch {
                path: path.clone(),
                line: u32::try_from(line_number).unwrap_or(u32::MAX),
                column: utf16_column(preview, start as usize),
                end_column: utf16_column(preview, end as usize),
                preview: bounded_preview(preview),
            });
        }
    }
    drop(reader);
    let status = child.wait().map_err(|_| RipgrepUnavailable)?;
    if !truncated && !matches!(status.code(), Some(0 | 1)) {
        return Err(RipgrepUnavailable);
    }
    Ok((matches, truncated, Vec::new()))
}

fn repository_files_with_rg(
    root: &Path,
    query: Option<&str>,
    limit: usize,
) -> Result<(Vec<String>, bool, Vec<String>), RipgrepUnavailable> {
    let mut child = Command::new(rg_executable())
        .current_dir(root)
        .args(["--files", "--hidden", "--glob", "!.git/**", "--null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| RipgrepUnavailable)?;
    let stdout = child.stdout.take().ok_or(RipgrepUnavailable)?;
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut files = Vec::new();
    let mut truncated = false;
    loop {
        record.clear();
        let read = reader
            .read_until(0, &mut record)
            .map_err(|_| RipgrepUnavailable)?;
        if read == 0 {
            break;
        }
        retained_bytes = retained_bytes.saturating_add(read);
        if retained_bytes > MAX_SEARCH_EVENT_BYTES {
            truncated = true;
            let _ = child.kill();
            break;
        }
        if record.last() == Some(&0) {
            record.pop();
        }
        let Some(path) = std::str::from_utf8(&record)
            .ok()
            .and_then(normalize_search_path)
        else {
            continue;
        };
        if query.map_or(true, |needle| path.to_lowercase().contains(needle)) {
            files.push(path);
            if files.len() > limit {
                files.truncate(limit);
                truncated = true;
                let _ = child.kill();
                break;
            }
        }
    }
    drop(reader);
    let status = child.wait().map_err(|_| RipgrepUnavailable)?;
    if !truncated && !matches!(status.code(), Some(0 | 1)) {
        return Err(RipgrepUnavailable);
    }
    Ok((files, truncated, Vec::new()))
}

fn repository_files_with_git(
    root: &Path,
    query: Option<&str>,
    limit: usize,
) -> Result<(Vec<String>, bool), SymbolSearchError> {
    let mut child = Command::new(git_executable())
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| SymbolSearchError::SearchFailed(error.to_string()))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        SymbolSearchError::SearchFailed("Git file list had no output pipe".into())
    })?;
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut files = Vec::new();
    let mut truncated = false;
    loop {
        record.clear();
        let read = reader
            .read_until(0, &mut record)
            .map_err(|error| SymbolSearchError::SearchFailed(error.to_string()))?;
        if read == 0 {
            break;
        }
        retained_bytes = retained_bytes.saturating_add(read);
        if retained_bytes > MAX_SEARCH_EVENT_BYTES {
            truncated = true;
            let _ = child.kill();
            break;
        }
        if record.last() == Some(&0) {
            record.pop();
        }
        let Some(path) = std::str::from_utf8(&record)
            .ok()
            .and_then(normalize_search_path)
        else {
            continue;
        };
        if query.map_or(true, |needle| path.to_lowercase().contains(needle)) {
            files.push(path);
            if files.len() > limit {
                files.truncate(limit);
                truncated = true;
                let _ = child.kill();
                break;
            }
        }
    }
    drop(reader);
    let status = child
        .wait()
        .map_err(|error| SymbolSearchError::SearchFailed(error.to_string()))?;
    if !truncated && !status.success() {
        return Err(SymbolSearchError::SearchFailed(
            "Git repository file listing was unavailable".into(),
        ));
    }
    Ok((files, truncated))
}

fn git_grep_matches(
    root: &Path,
    symbol: &str,
) -> Result<(Vec<LexicalMatch>, bool), SymbolSearchError> {
    let mut child = Command::new(git_executable())
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "grep",
            "--untracked",
            "--exclude-standard",
            "-n",
            "-I",
            "-F",
            "-z",
            "-e",
            symbol,
            "--",
            ".",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| SymbolSearchError::SearchFailed(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SymbolSearchError::SearchFailed("Git search had no output pipe".into()))?;
    let mut reader = BufReader::new(stdout);
    let mut record = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut matches = Vec::new();
    let mut truncated = false;
    loop {
        record.clear();
        let read = reader
            .read_until(b'\n', &mut record)
            .map_err(|error| SymbolSearchError::SearchFailed(error.to_string()))?;
        if read == 0 {
            break;
        }
        retained_bytes = retained_bytes.saturating_add(read);
        if retained_bytes > MAX_SEARCH_EVENT_BYTES || matches.len() >= MAX_CANDIDATE_MATCHES {
            truncated = true;
            let _ = child.kill();
            break;
        }
        let separators = record
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == 0).then_some(index))
            .take(2)
            .collect::<Vec<_>>();
        let [path_end, line_end] = separators.as_slice() else {
            continue;
        };
        let Some(path) = std::str::from_utf8(&record[..*path_end])
            .ok()
            .and_then(normalize_search_path)
        else {
            continue;
        };
        let Ok(line_number) = std::str::from_utf8(&record[path_end + 1..*line_end])
            .unwrap_or_default()
            .parse::<u32>()
        else {
            continue;
        };
        let preview = String::from_utf8_lossy(&record[line_end + 1..]);
        let preview = preview.trim_end_matches(['\r', '\n']);
        for (start, end) in symbol_offsets(preview, symbol) {
            matches.push(LexicalMatch {
                path: path.clone(),
                line: line_number,
                column: utf16_column(preview, start),
                end_column: utf16_column(preview, end),
                preview: bounded_preview(preview),
            });
            if matches.len() >= MAX_CANDIDATE_MATCHES {
                truncated = true;
                let _ = child.kill();
                break;
            }
        }
    }
    drop(reader);
    let status = child
        .wait()
        .map_err(|error| SymbolSearchError::SearchFailed(error.to_string()))?;
    if !truncated && !matches!(status.code(), Some(0 | 1)) {
        return Err(SymbolSearchError::SearchFailed(
            "Git tracked-file search was unavailable".into(),
        ));
    }
    Ok((matches, truncated))
}

fn read_repository_source(
    root: &Path,
    relative: &str,
) -> Result<Option<(String, String)>, SymbolSearchError> {
    let stored = StoredPath::from(relative);
    if !is_safe_repository_relative_path(&stored) || relative.contains('\\') {
        return Err(SymbolSearchError::InvalidPath);
    }
    let candidate = root.join(relative);
    let target = fs::canonicalize(candidate).map_err(|_| SymbolSearchError::SourceUnavailable)?;
    if !target.starts_with(root) || !target.is_file() {
        return Err(SymbolSearchError::SourceUnavailable);
    }
    let metadata = fs::metadata(&target).map_err(|_| SymbolSearchError::SourceUnavailable)?;
    if metadata.len() > MAX_SOURCE_BYTES as u64 {
        return Ok(None);
    }
    let bytes = fs::read(target).map_err(|_| SymbolSearchError::SourceUnavailable)?;
    if bytes.len() > MAX_SOURCE_BYTES || bytes.contains(&0) {
        return Ok(None);
    }
    let source = String::from_utf8(bytes).map_err(|_| SymbolSearchError::UnsupportedSource)?;
    let fingerprint = hex::encode(Sha256::digest(source.as_bytes()));
    Ok(Some((source, fingerprint)))
}

fn normalize_search_path(path: &str) -> Option<String> {
    let path = path.strip_prefix("./").unwrap_or(path);
    let stored = StoredPath::from(path);
    (is_safe_repository_relative_path(&stored) && !path.contains('\\')).then(|| path.to_owned())
}

fn symbol_offsets(line: &str, symbol: &str) -> Vec<(usize, usize)> {
    line.match_indices(symbol)
        .filter_map(|(start, matched)| {
            let end = start.saturating_add(matched.len());
            is_identifier_occurrence(line, start, end).then_some((start, end))
        })
        .collect()
}

fn is_identifier_occurrence(line: &str, start: usize, end: usize) -> bool {
    let before = line
        .get(..start)
        .and_then(|value| value.chars().next_back());
    let after = line.get(end..).and_then(|value| value.chars().next());
    !before.is_some_and(identifier_continue) && !after.is_some_and(identifier_continue)
}

fn identifier_continue(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphanumeric()
}

fn outline_name_contains_symbol(name: &str, symbol: &str) -> bool {
    symbol_offsets(name, symbol).into_iter().next().is_some()
}

fn symbol_column(line: &str, symbol: &str) -> Option<u32> {
    symbol_offsets(line, symbol)
        .first()
        .map(|(start, _)| utf16_column(line, *start))
}

fn utf16_column(line: &str, byte_offset: usize) -> u32 {
    u32::try_from(
        line.get(..byte_offset)
            .unwrap_or_default()
            .encode_utf16()
            .count()
            .saturating_add(1),
    )
    .unwrap_or(u32::MAX)
}

fn looks_like_definition(line: &str, symbol: &str, column: u32) -> bool {
    let start = symbol_offsets(line, symbol)
        .into_iter()
        .find(|(start, _)| utf16_column(line, *start) == column)
        .map_or(0, |(start, _)| start);
    let prefix = line.get(..start).unwrap_or_default().trim_start();
    const DECLARATION_PREFIXES: &[&str] = &[
        "fn ",
        "func ",
        "function ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "interface ",
        "trait ",
        "type ",
        "module ",
        "mod ",
        "namespace ",
        "sub ",
        "resource ",
        "variable ",
        "const ",
        "let ",
        "var ",
    ];
    DECLARATION_PREFIXES
        .iter()
        .any(|keyword| prefix.ends_with(keyword))
        || prefix.ends_with("variable \"")
        || prefix.ends_with("resource \"")
        || prefix.ends_with("output \"")
        || line
            .get(start.saturating_add(symbol.len())..)
            .is_some_and(|suffix| {
                let suffix = suffix.trim_start();
                suffix.starts_with(":=")
                    || suffix.starts_with('=')
                        && (suffix.contains("=>")
                            || suffix.contains("function")
                            || suffix.contains("lambda"))
            })
}

fn outline_kind_name(kind: OutlineKind) -> &'static str {
    match kind {
        OutlineKind::Function => "function",
        OutlineKind::Method => "method",
        OutlineKind::Class => "class",
        OutlineKind::Struct => "struct",
        OutlineKind::Enum => "enum",
        OutlineKind::Interface => "interface",
        OutlineKind::Module => "module",
        OutlineKind::Heading => "heading",
        OutlineKind::Property => "property",
        OutlineKind::Unknown => "unknown",
    }
}

fn bounded_preview(value: &str) -> String {
    value.trim_end().chars().take(MAX_PREVIEW_CHARS).collect()
}

fn location_order(left: &CoreSymbolLocation, right: &CoreSymbolLocation) -> std::cmp::Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.column.cmp(&right.column))
}

#[derive(Debug)]
struct RipgrepUnavailable;

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn repository() -> TempDir {
        let temporary = TempDir::new().unwrap();
        Command::new(git_executable())
            .current_dir(temporary.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        fs::create_dir_all(temporary.path().join("src")).unwrap();
        fs::write(
            temporary.path().join("src/lib.rs"),
            "pub fn shared_name() {}\nfn caller() { shared_name(); }\n",
        )
        .unwrap();
        fs::write(
            temporary.path().join("src/example.py"),
            "def shared_name():\n    return 1\n\nshared_name()\n",
        )
        .unwrap();
        fs::write(
            temporary.path().join("BUILD.bazel"),
            "def shared_name():\n    pass\n\nshared_name()\n",
        )
        .unwrap();
        fs::write(
            temporary.path().join("main.tf"),
            "variable \"shared_name\" {}\noutput \"result\" { value = var.shared_name }\n",
        )
        .unwrap();
        temporary
    }

    #[test]
    fn lazy_search_classifies_broad_language_definitions_and_references() {
        let repository = repository();
        let result = search_repository(repository.path(), "shared_name").unwrap();
        assert!(result
            .definitions
            .iter()
            .any(|location| location.path == "src/lib.rs" && location.kind == "function"));
        assert!(result
            .definitions
            .iter()
            .any(|location| location.path == "src/example.py"));
        assert!(result
            .definitions
            .iter()
            .any(|location| location.path == "BUILD.bazel"));
        assert!(result
            .definitions
            .iter()
            .any(|location| location.path == "main.tf"));
        assert!(result.references.len() >= 4);
        assert!(result
            .definitions
            .iter()
            .chain(result.references.iter())
            .all(|location| !Path::new(&location.path).is_absolute()));
    }

    #[test]
    fn source_windows_are_fingerprint_checked_and_cannot_escape() {
        let repository = repository();
        let result = search_repository(repository.path(), "shared_name").unwrap();
        let location = result
            .definitions
            .iter()
            .find(|location| location.path == "src/lib.rs")
            .unwrap();
        let window = read_source_window(
            repository.path(),
            &location.path,
            &location.source_fingerprint,
            1,
            10,
        )
        .unwrap();
        assert_eq!(window.lines.len(), 2);
        assert_eq!(window.line_start_bytes.len(), 2);
        assert_eq!(window.byte_range.0, 0);
        let opened = open_source_window(repository.path(), "src/lib.rs", 2, 1).unwrap();
        assert_eq!(opened.start_line, 2);
        assert_eq!(opened.lines[0].1, "fn caller() { shared_name(); }");
        assert_eq!(opened.fingerprint, location.source_fingerprint);
        fs::write(repository.path().join("src/lib.rs"), "fn changed() {}\n").unwrap();
        assert!(matches!(
            read_source_window(
                repository.path(),
                &location.path,
                &location.source_fingerprint,
                1,
                10
            ),
            Err(SymbolSearchError::SourceChanged)
        ));
        assert!(matches!(
            read_source_window(repository.path(), "../secret", "0", 1, 1),
            Err(SymbolSearchError::InvalidPath)
        ));
    }

    #[test]
    fn repository_file_listing_is_bounded_filtered_and_relative() {
        let repository = repository();
        let bounded = list_repository_files(repository.path(), None, 1).unwrap();
        assert_eq!(bounded.files.len(), 1);
        assert!(bounded.truncated);

        let filtered = list_repository_files(repository.path(), Some("BUILD"), 20).unwrap();
        assert_eq!(filtered.files, vec!["BUILD.bazel"]);
        assert!(!filtered.truncated);
        assert!(filtered
            .files
            .iter()
            .all(|path| !Path::new(path).is_absolute()));
    }

    #[test]
    fn identifier_validation_and_boundaries_do_not_match_substrings() {
        assert!(validate_symbol("ClassName_2").is_ok());
        assert!(validate_symbol("$factory").is_ok());
        assert!(validate_symbol("@decorator").is_ok());
        assert!(validate_symbol("ready?").is_ok());
        assert!(validate_symbol("Module::method").is_ok());
        assert!(validate_symbol("élan").is_ok());
        for invalid in ["", "two words", "../escape", "call()", "2fast"] {
            assert!(validate_symbol(invalid).is_err());
        }
        assert_eq!(symbol_offsets("name rename name_2 name", "name").len(), 2);
        assert_eq!(utf16_column("😀 shared_name()", 5), 4);
    }
}
