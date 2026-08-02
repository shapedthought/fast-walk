//! Tests for reading scan CSVs back and comparing them.

use fast_walk::diff::{compare, read_scan_csv};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn reads_the_columns_a_scan_writes() {
    let dir = TempDir::new().unwrap();
    let path = write(
        dir.path(),
        "results.csv",
        "Extension,Qty,Cap Bytes,Avg Bytes\ntxt,3,300,100\nrs,1,50,50\n",
    );

    let totals = read_scan_csv(&path).unwrap();

    assert_eq!(totals["txt"].count, 3);
    assert_eq!(totals["txt"].bytes, 300);
    assert_eq!(totals["rs"].count, 1);
    assert_eq!(totals["rs"].bytes, 50);
}

#[test]
fn columns_are_found_by_name_not_position() {
    // A results file that gained or reordered columns must still read.
    let dir = TempDir::new().unwrap();
    let path = write(
        dir.path(),
        "results.csv",
        "Something Else,Cap Bytes,Extension,Qty\nignored,300,txt,3\n",
    );

    let totals = read_scan_csv(&path).unwrap();

    assert_eq!(totals["txt"].count, 3);
    assert_eq!(totals["txt"].bytes, 300);
}

#[test]
fn a_file_missing_a_required_column_is_rejected_by_name() {
    let dir = TempDir::new().unwrap();
    let path = write(dir.path(), "results.csv", "Extension,Qty\ntxt,3\n");

    let err = read_scan_csv(&path).unwrap_err().to_string();

    assert!(err.contains("Cap Bytes"), "unhelpful error: {err}");
}

#[test]
fn a_non_numeric_count_is_rejected_with_the_line_number() {
    let dir = TempDir::new().unwrap();
    let path = write(
        dir.path(),
        "results.csv",
        "Extension,Qty,Cap Bytes\ntxt,3,300\nrs,lots,50\n",
    );

    let err = read_scan_csv(&path).unwrap_err().to_string();

    assert!(err.contains("line 3"), "should name the row: {err}");
    assert!(err.contains("lots"), "should quote the value: {err}");
}

#[test]
fn a_missing_file_is_rejected_rather_than_read_as_empty() {
    let dir = TempDir::new().unwrap();

    let err = read_scan_csv(&dir.path().join("nope.csv"))
        .unwrap_err()
        .to_string();

    assert!(err.contains("cannot read scan file"), "unhelpful: {err}");
}

#[test]
fn an_empty_results_file_compares_as_having_nothing() {
    let dir = TempDir::new().unwrap();
    let empty = write(dir.path(), "empty.csv", "Extension,Qty,Cap Bytes\n");
    let populated = write(
        dir.path(),
        "full.csv",
        "Extension,Qty,Cap Bytes\ntxt,3,300\n",
    );

    let diff = compare(
        &read_scan_csv(&empty).unwrap(),
        &read_scan_csv(&populated).unwrap(),
    );

    assert_eq!(diff.rows.len(), 1);
    assert!(diff.rows[0].is_new());
    assert_eq!(diff.bytes_delta(), 300);
}

/// Scan a directory with the real binary, naming the output, and return the
/// extensions CSV it wrote.
fn scan_to_csv(tree: &Path, working: &Path, name: &str) -> PathBuf {
    let stem = working.join(name);

    let status = Command::new(env!("CARGO_BIN_EXE_fast-walk"))
        .arg("-p")
        .arg(tree)
        .arg("-o")
        .arg(&stem)
        .status()
        .unwrap();
    assert!(status.success(), "scan failed");

    let csv = stem.with_extension("csv");
    assert!(csv.is_file(), "{} was not written", csv.display());
    csv
}

#[test]
fn a_scan_writes_csvs_that_the_differ_can_read_back() {
    // The real integration risk is the writer and the reader disagreeing about
    // the format, so this goes through the binary rather than constructing
    // CSVs by hand.
    let before_tree = TempDir::new().unwrap();
    fs::write(before_tree.path().join("a.txt"), vec![b'x'; 100]).unwrap();

    let after_tree = TempDir::new().unwrap();
    fs::write(after_tree.path().join("a.txt"), vec![b'x'; 100]).unwrap();
    fs::write(after_tree.path().join("b.txt"), vec![b'x'; 400]).unwrap();
    fs::write(after_tree.path().join("c.mp4"), vec![b'x'; 1000]).unwrap();

    let out = TempDir::new().unwrap();
    let before = scan_to_csv(before_tree.path(), out.path(), "before");
    let after = scan_to_csv(after_tree.path(), out.path(), "after");

    let diff = compare(
        &read_scan_csv(&before).unwrap(),
        &read_scan_csv(&after).unwrap(),
    );

    assert_eq!(diff.count_delta(), 2);
    assert_eq!(diff.bytes_delta(), 1400);

    // mp4 moved the most capacity, so it leads.
    assert_eq!(diff.rows[0].extension, "mp4");
    assert!(diff.rows[0].is_new());
    assert_eq!(diff.rows[0].bytes_delta(), 1000);

    assert_eq!(diff.rows[1].extension, "txt");
    assert_eq!(diff.rows[1].count_delta(), 1);
    assert_eq!(diff.rows[1].bytes_delta(), 400);
}

#[test]
fn the_diff_mode_runs_end_to_end_without_a_path() {
    let tree = TempDir::new().unwrap();
    fs::write(tree.path().join("a.txt"), vec![b'x'; 100]).unwrap();

    let out = TempDir::new().unwrap();
    let csv = scan_to_csv(tree.path(), out.path(), "scan");

    let status = Command::new(env!("CARGO_BIN_EXE_fast-walk"))
        .arg("--diff")
        .arg(&csv)
        .arg(&csv)
        .current_dir(out.path())
        .status()
        .unwrap();

    assert!(status.success(), "--diff should not require --path");
}

#[test]
fn a_run_with_neither_path_nor_diff_is_rejected() {
    let out = TempDir::new().unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_fast-walk"))
        .current_dir(out.path())
        .status()
        .unwrap();

    assert!(!status.success(), "should require one mode or the other");
}
