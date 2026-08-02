//! Tests for where a scan writes its results files.

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Every report a scan produces, as suffixes on the chosen stem.
const REPORTS: [&str; 5] = ["", "-age", "-size", "-hotspots", "-largest"];

fn tree_with_a_file() -> TempDir {
    let tree = TempDir::new().unwrap();
    fs::write(tree.path().join("a.txt"), vec![b'x'; 100]).unwrap();
    tree
}

fn scan(args: &[&Path], working: &Path) -> bool {
    Command::new(env!("CARGO_BIN_EXE_fast-walk"))
        .args(args)
        .current_dir(working)
        .status()
        .unwrap()
        .success()
}

#[test]
fn every_report_is_named_from_the_output_stem() {
    let tree = tree_with_a_file();
    let out = TempDir::new().unwrap();
    let stem = out.path().join("monday");

    assert!(scan(
        &[Path::new("-p"), tree.path(), Path::new("-o"), &stem],
        out.path()
    ));

    for suffix in REPORTS {
        let expected = out.path().join(format!("monday{suffix}.csv"));
        assert!(expected.is_file(), "{} was not written", expected.display());
    }
}

#[test]
fn a_csv_extension_on_the_output_is_not_doubled_up() {
    let tree = tree_with_a_file();
    let out = TempDir::new().unwrap();
    let stem = out.path().join("monday.csv");

    assert!(scan(
        &[Path::new("-p"), tree.path(), Path::new("-o"), &stem],
        out.path()
    ));

    assert!(out.path().join("monday.csv").is_file());
    assert!(out.path().join("monday-age.csv").is_file());
    assert!(
        !out.path().join("monday.csv-age.csv").exists(),
        "the extension should have been stripped before adding suffixes"
    );
}

#[test]
fn the_output_may_name_a_different_directory() {
    let tree = tree_with_a_file();
    let out = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    let stem = elsewhere.path().join("scan");

    // Run from `out`, but write into `elsewhere`.
    assert!(scan(
        &[Path::new("-p"), tree.path(), Path::new("-o"), &stem],
        out.path()
    ));

    assert!(elsewhere.path().join("scan.csv").is_file());
    assert!(
        fs::read_dir(out.path()).unwrap().next().is_none(),
        "nothing should have been written to the working directory"
    );
}

#[test]
fn without_an_output_the_files_are_timestamped() {
    let tree = tree_with_a_file();
    let out = TempDir::new().unwrap();

    assert!(scan(&[Path::new("-p"), tree.path()], out.path()));

    let names: Vec<String> = fs::read_dir(out.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        names.len(),
        REPORTS.len(),
        "expected one file per report: {names:?}"
    );

    // The extensions CSV carries no report suffix, so it has the shortest
    // stem. Picking it by length rather than by directory order keeps this
    // independent of however the filesystem enumerates.
    let stamp = names
        .iter()
        .filter_map(|name| {
            name.strip_prefix("results-")
                .and_then(|rest| rest.strip_suffix(".csv"))
        })
        .min_by_key(|stem| stem.len())
        .unwrap_or_else(|| panic!("no results file written: {names:?}"));

    // results-YYYYMMDD-HHMMSS.csv
    let (date, time) = stamp
        .split_once('-')
        .unwrap_or_else(|| panic!("unexpected stamp: {stamp:?}"));
    assert_eq!(date.len(), 8, "expected YYYYMMDD, got {date:?}");
    assert_eq!(time.len(), 6, "expected HHMMSS, got {time:?}");
    assert!(date.chars().all(|c| c.is_ascii_digit()), "{date:?}");
    assert!(time.chars().all(|c| c.is_ascii_digit()), "{time:?}");

    // Every report shares the one stamp, so a run's files stay together.
    for suffix in REPORTS {
        let expected = format!("results-{stamp}{suffix}.csv");
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
}

#[test]
fn the_diff_output_can_be_named_too() {
    let tree = tree_with_a_file();
    let out = TempDir::new().unwrap();
    let scan_stem = out.path().join("scan");

    assert!(scan(
        &[Path::new("-p"), tree.path(), Path::new("-o"), &scan_stem],
        out.path()
    ));

    let csv = out.path().join("scan.csv");
    let diff_stem = out.path().join("weekly-diff");

    assert!(scan(
        &[Path::new("--diff"), &csv, &csv, Path::new("-o"), &diff_stem,],
        out.path()
    ));

    assert!(out.path().join("weekly-diff.csv").is_file());
}

#[test]
fn an_output_that_cannot_be_written_reports_the_path() {
    let tree = tree_with_a_file();
    let out = TempDir::new().unwrap();
    let stem = out.path().join("no-such-directory/scan");

    let output = Command::new(env!("CARGO_BIN_EXE_fast-walk"))
        .arg("-p")
        .arg(tree.path())
        .arg("-o")
        .arg(&stem)
        .current_dir(out.path())
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "should fail on an unwritable path"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no-such-directory"),
        "error should name the path: {stderr}"
    );
}
