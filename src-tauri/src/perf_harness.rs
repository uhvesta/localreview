//! Deterministic, release-profile benchmark of the same controller methods
//! used by Tauri IPC. The companion shell harness separately measures the
//! packaged WebView round trip; this layer isolates native work and outbound
//! serialization so regressions can be attributed instead of guessed.

use crate::controller::{
    DesktopController, OpenWorkspaceInput, PresentationRequest, StartOrRefreshInput,
    SymbolNavigationQuery,
};
use localreview_domain::{RepositoryId, ReviewFileId, WorkspaceId};
use localreview_persistence::StateStore;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};
use uuid::Uuid;

const FIXTURE_LINES: usize = 25_000;
const DEFAULT_ITERATIONS: usize = 24;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessReport {
    schema_version: u32,
    profile: &'static str,
    fixture_lines: usize,
    iterations: usize,
    operations: Vec<OperationReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationReport {
    name: &'static str,
    samples: usize,
    viewport_rows_requested: Option<u32>,
    wall_micros: Distribution,
    native_micros: Distribution,
    serialization_micros: Distribution,
    response_bytes: Distribution,
    response_rows: Distribution,
    syntax_tokens: Distribution,
    omitted_blocks: Distribution,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Distribution {
    min: u64,
    median: u64,
    p95: u64,
    max: u64,
}

#[derive(Debug)]
struct Measurement {
    wall_micros: u64,
    native_micros: u64,
    serialization_micros: u64,
    response_bytes: u64,
    response_rows: u64,
    syntax_tokens: u64,
    omitted_blocks: u64,
}

pub(crate) fn run(output: &Path) -> Result<(), String> {
    let iterations = std::env::var("LOCALREVIEW_PERF_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=1_000).contains(value))
        .unwrap_or(DEFAULT_ITERATIONS);
    let root = unique_temp_root();
    let data = root.join("data");
    let workspace = root.join("workspace");
    fs::create_dir_all(&data).map_err(display_error)?;
    create_fixture(&workspace)?;

    let store = StateStore::open(&data).map_err(display_error)?;
    let controller = DesktopController::new(store);
    let (opened, _) = controller
        .open_local_workspace(OpenWorkspaceInput {
            path: workspace.to_string_lossy().into_owned(),
            base: Some("main".into()),
            repository_bases: Vec::new(),
        })
        .map_err(display_error)?;
    let workspace_id = WorkspaceId(parse_uuid(&opened.id)?);
    let review = controller
        .load_review(workspace_id)
        .map_err(display_error)?;
    let file = review
        .files
        .iter()
        .find(|file| file.path == "bench.rs")
        .ok_or_else(|| "benchmark review did not capture bench.rs".to_owned())?;
    let repository = review
        .repositories
        .first()
        .ok_or_else(|| "benchmark review has no repository".to_owned())?;
    let file_id = ReviewFileId(parse_uuid(&file.id)?);
    let comparison_id = file.comparison_id.clone();
    let repository_id = RepositoryId(parse_uuid(&repository.id)?);
    let mut generation = 1_u64;

    let initial = controller
        .presentation_window(
            presentation_request(
                &file.id,
                Some(&comparison_id),
                generation,
                Vec::new(),
                Vec::new(),
            ),
            Path::new("."),
        )
        .map_err(display_error)?;
    let deletion_ids = initial
        .omitted_blocks
        .iter()
        .filter(|block| block.side == "old")
        .map(|block| block.id.clone())
        .collect::<Vec<_>>();
    let addition_ids = initial
        .omitted_blocks
        .iter()
        .filter(|block| block.side == "new")
        .map(|block| block.id.clone())
        .collect::<Vec<_>>();
    let first_deletion = deletion_ids
        .first()
        .cloned()
        .ok_or_else(|| "benchmark fixture produced no deletion blocks".to_owned())?;
    let first_addition = addition_ids
        .first()
        .cloned()
        .ok_or_else(|| "benchmark fixture produced no addition blocks".to_owned())?;

    let mut operations = Vec::new();
    let mut samples = Vec::new();
    for index in 0..iterations {
        generation += 1;
        let (expanded, collapsed) = if index % 2 == 0 {
            (vec![first_deletion.clone()], Vec::new())
        } else {
            (Vec::new(), vec![first_addition.clone()])
        };
        samples.push(measure_presentation(|| {
            controller.presentation_window(
                presentation_request(
                    &file.id,
                    Some(&comparison_id),
                    generation,
                    expanded,
                    collapsed,
                ),
                Path::new("."),
            )
        })?);
    }
    operations.push(summarize("individual_disclosure", Some(220), samples));

    let mut samples = Vec::new();
    for index in 0..iterations {
        generation += 1;
        let (expanded, collapsed) = if index % 2 == 0 {
            (deletion_ids.clone(), Vec::new())
        } else {
            (Vec::new(), addition_ids.clone())
        };
        samples.push(measure_presentation(|| {
            controller.presentation_window(
                presentation_request(
                    &file.id,
                    Some(&comparison_id),
                    generation,
                    expanded,
                    collapsed,
                ),
                Path::new("."),
            )
        })?);
    }
    operations.push(summarize("expand_collapse_all", Some(220), samples));

    // A new controller has an empty syntax cache while reading the same
    // immutable database. First sample is cold; subsequent samples are exact
    // cached viewport reads through the normal presentation path.
    let highlight_controller =
        DesktopController::new(StateStore::open(&data).map_err(display_error)?);
    generation += 1;
    let cold = measure_presentation(|| {
        highlight_controller.presentation_window(
            PresentationRequest {
                file_id: file.id.clone(),
                comparison_id: Some(comparison_id.clone()),
                mode: "unified".into(),
                start_row: 0,
                end_row: 220,
                generation,
                full_file_side: None,
                split_ratio: None,
                ephemeral_expanded_full_file_deletion_blocks: None,
                ephemeral_collapsed_full_file_addition_blocks: None,
            },
            Path::new("."),
        )
    })?;
    operations.push(summarize("highlight_cold", Some(220), vec![cold]));
    let mut samples = Vec::new();
    for _ in 0..iterations {
        generation += 1;
        samples.push(measure_presentation(|| {
            highlight_controller.presentation_window(
                PresentationRequest {
                    file_id: file.id.clone(),
                    comparison_id: Some(comparison_id.clone()),
                    mode: "unified".into(),
                    start_row: 0,
                    end_row: 220,
                    generation,
                    full_file_side: None,
                    split_ratio: None,
                    ephemeral_expanded_full_file_deletion_blocks: None,
                    ephemeral_collapsed_full_file_addition_blocks: None,
                },
                Path::new("."),
            )
        })?);
    }
    operations.push(summarize("highlight_cached_viewport", Some(220), samples));

    let mut samples = Vec::new();
    for index in 0..usize::max(3, iterations / 4) {
        append_refresh_marker(&workspace.join("bench.rs"), index)?;
        samples.push(measure(|| {
            controller.refresh_review(workspace_id, StartOrRefreshInput::default())
        })?);
    }
    operations.push(summarize("refresh", None, samples));

    let mut samples = Vec::new();
    for _ in 0..iterations {
        samples.push(measure(|| {
            controller.query_symbol_navigation(SymbolNavigationQuery {
                workspace_id: workspace_id.to_string(),
                repository_id: repository_id.to_string(),
                comparison_id: None,
                symbol: "target_symbol".into(),
                kind: "all".into(),
                limit: Some(100),
            })
        })?);
    }
    operations.push(summarize("symbol_navigation", None, samples));

    // Verify the file still resolves through the exact captured path after
    // repeated refreshes; this also prevents the benchmark compiler from
    // treating the IDs above as unused setup.
    let _ = controller.rows(file_id, "unified").map_err(display_error)?;
    let report = HarnessReport {
        schema_version: 1,
        profile: "release",
        fixture_lines: FIXTURE_LINES,
        iterations,
        operations,
    };
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(display_error)?;
    }
    let encoded = serde_json::to_vec_pretty(&report).map_err(display_error)?;
    fs::write(output, encoded).map_err(display_error)?;
    let _ = fs::remove_dir_all(root);
    Ok(())
}

fn presentation_request(
    file_id: &str,
    comparison_id: Option<&str>,
    generation: u64,
    expanded_deletions: Vec<String>,
    collapsed_additions: Vec<String>,
) -> PresentationRequest {
    PresentationRequest {
        file_id: file_id.into(),
        comparison_id: comparison_id.map(Into::into),
        mode: "full".into(),
        start_row: 0,
        end_row: 220,
        generation,
        full_file_side: Some("both".into()),
        split_ratio: None,
        ephemeral_expanded_full_file_deletion_blocks: Some(expanded_deletions),
        ephemeral_collapsed_full_file_addition_blocks: Some(collapsed_additions),
    }
}

fn measure<T, E>(operation: impl FnOnce() -> Result<T, E>) -> Result<Measurement, String>
where
    T: Serialize,
    E: std::fmt::Display,
{
    let wall_started = Instant::now();
    let native_started = Instant::now();
    let value = operation().map_err(display_error)?;
    let native_micros = native_started.elapsed().as_micros();
    let serialization_started = Instant::now();
    let response = serde_json::to_vec(&value).map_err(display_error)?;
    let serialization_micros = serialization_started.elapsed().as_micros();
    Ok(Measurement {
        wall_micros: micros(wall_started.elapsed()),
        native_micros: u64::try_from(native_micros).unwrap_or(u64::MAX),
        serialization_micros: u64::try_from(serialization_micros).unwrap_or(u64::MAX),
        response_bytes: u64::try_from(response.len()).unwrap_or(u64::MAX),
        response_rows: 0,
        syntax_tokens: 0,
        omitted_blocks: 0,
    })
}

fn measure_presentation<E>(
    operation: impl FnOnce() -> Result<crate::controller::PresentationWindow, E>,
) -> Result<Measurement, String>
where
    E: std::fmt::Display,
{
    let wall_started = Instant::now();
    let native_started = Instant::now();
    let value = operation().map_err(display_error)?;
    let native_micros = micros(native_started.elapsed());
    let serialization_started = Instant::now();
    let response = serde_json::to_vec(&value).map_err(display_error)?;
    let serialization_micros = micros(serialization_started.elapsed());
    Ok(Measurement {
        wall_micros: micros(wall_started.elapsed()),
        native_micros,
        serialization_micros,
        response_bytes: u64::try_from(response.len()).unwrap_or(u64::MAX),
        response_rows: u64::try_from(value.rows.len()).unwrap_or(u64::MAX),
        syntax_tokens: u64::try_from(value.old_tokens.len() + value.new_tokens.len())
            .unwrap_or(u64::MAX),
        omitted_blocks: u64::try_from(value.omitted_blocks.len()).unwrap_or(u64::MAX),
    })
}

fn summarize(
    name: &'static str,
    viewport_rows_requested: Option<u32>,
    samples: Vec<Measurement>,
) -> OperationReport {
    OperationReport {
        name,
        samples: samples.len(),
        viewport_rows_requested,
        wall_micros: distribution(samples.iter().map(|sample| sample.wall_micros)),
        native_micros: distribution(samples.iter().map(|sample| sample.native_micros)),
        serialization_micros: distribution(
            samples.iter().map(|sample| sample.serialization_micros),
        ),
        response_bytes: distribution(samples.iter().map(|sample| sample.response_bytes)),
        response_rows: distribution(samples.iter().map(|sample| sample.response_rows)),
        syntax_tokens: distribution(samples.iter().map(|sample| sample.syntax_tokens)),
        omitted_blocks: distribution(samples.iter().map(|sample| sample.omitted_blocks)),
    }
}

fn distribution(values: impl Iterator<Item = u64>) -> Distribution {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    let percentile = |numerator: usize, denominator: usize| {
        let index = values.len().saturating_sub(1).saturating_mul(numerator) / denominator;
        values[index]
    };
    Distribution {
        min: *values.first().unwrap_or(&0),
        median: percentile(50, 100),
        p95: percentile(95, 100),
        max: *values.last().unwrap_or(&0),
    }
}

fn create_fixture(root: &Path) -> Result<(), String> {
    fs::create_dir_all(root).map_err(display_error)?;
    git(root, &["init", "-b", "main"])?;
    git(
        root,
        &["config", "user.email", "performance@example.invalid"],
    )?;
    git(root, &["config", "user.name", "LocalReview Performance"])?;
    let base = fixture_source(false);
    fs::write(root.join("bench.rs"), base).map_err(display_error)?;
    git(root, &["add", "bench.rs"])?;
    git(root, &["commit", "-m", "performance base"])?;
    git(root, &["switch", "-c", "performance-review"])?;
    fs::write(root.join("bench.rs"), fixture_source(true)).map_err(display_error)?;
    Ok(())
}

fn fixture_source(changed: bool) -> String {
    let mut source = String::with_capacity(FIXTURE_LINES * 72);
    source.push_str("pub fn target_symbol(value: usize) -> usize { value + 1 }\n");
    let mut skip = 0_usize;
    for line in 1..FIXTURE_LINES {
        if changed && line % 250 == 0 {
            skip = 4;
        }
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if changed && line % 250 == 50 {
            for added in 0..4 {
                source.push_str(&format!(
                    "pub fn added_{line}_{added}(value: usize) -> usize {{ target_symbol(value) + {added} }}\n"
                ));
            }
        }
        if changed && line % 250 == 100 {
            source.push_str(&format!(
                "pub fn changed_{line}(value: usize) -> usize {{ target_symbol(value) + {} }}\n",
                line + 1
            ));
        } else if line % 4 == 0 {
            source.push_str(&format!(
                "pub static USE_{line}: fn(usize) -> usize = target_symbol;\n"
            ));
        } else {
            source.push_str(&format!(
                "pub fn generated_{line}(value: usize) -> usize {{ value + {line} }}\n"
            ));
        }
    }
    source
}

fn append_refresh_marker(path: &Path, index: usize) -> Result<(), String> {
    let mut source = fs::read_to_string(path).map_err(display_error)?;
    source.push_str(&format!(
        "\npub const REFRESH_MARKER_{index}: usize = {index};\n"
    ));
    fs::write(path, source).map_err(display_error)
}

fn git(root: &Path, arguments: &[&str]) -> Result<(), String> {
    let output = Command::new(localreview_tools::git_executable())
        .current_dir(root)
        .args(arguments)
        .output()
        .map_err(display_error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, String> {
    Uuid::parse_str(value).map_err(display_error)
}

fn unique_temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("localreview-perf-{}", Uuid::new_v4()))
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn micros(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
