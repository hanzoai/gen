//! Architectural guard: every Rust source and test file stays `<= 1,000` lines.

use std::path::{Path, PathBuf};

const MAX_LINES: usize = 1_000;

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn source_files_under_1000_lines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rs_files(&root.join("src"), &mut files);
    collect_rs_files(&root.join("tests"), &mut files);
    files.sort();
    assert!(!files.is_empty(), "expected .rs files under src/ and tests/");

    let mut violations = Vec::new();
    for file in files {
        let body = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("failed reading {}: {e}", file.display()));
        let lines = body.lines().count();
        if lines > MAX_LINES {
            let rel = file.strip_prefix(root).unwrap_or(&file);
            violations.push(format!("{} has {lines} lines (max {MAX_LINES})", rel.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "Rust files must not exceed {MAX_LINES} lines:\n  {}",
        violations.join("\n  ")
    );
}
