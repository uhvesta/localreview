use std::path::PathBuf;

fn main() {
    let output = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("performance-results.json"));
    if let Err(error) = localreview_desktop::run_performance_harness(&output) {
        eprintln!("LocalReview performance harness failed: {error}");
        std::process::exit(1);
    }
}
