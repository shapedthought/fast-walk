//! End-to-end tests for [`fast_walk::scan`] against real directory trees.

use fast_walk::{scan, Bucket, NoProgress, Scan, ScanOptions, NO_EXTENSION};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Write a file of exactly `size` bytes, creating parent directories as needed.
fn write_file(root: &Path, relative: &str, size: usize) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, vec![b'x'; size]).unwrap();
}

fn scan_dir(root: &Path) -> Scan {
    scan(root, &ScanOptions::default(), &NoProgress).unwrap()
}

fn scan_dir_with(root: &Path, options: ScanOptions) -> Scan {
    scan(root, &options, &NoProgress).unwrap()
}

/// Extension -> bucket, for convenient assertions.
fn totals(scan: &Scan) -> HashMap<&str, Bucket> {
    scan.totals
        .iter()
        .map(|(extension, bucket)| (extension.as_str(), *bucket))
        .collect()
}

#[test]
fn counts_files_and_sums_sizes_per_extension() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "a.txt", 10);
    write_file(dir.path(), "b.txt", 5);
    write_file(dir.path(), "c.rs", 3);

    let scan = scan_dir(dir.path());
    let totals = totals(&scan);

    assert_eq!(totals["txt"], Bucket { count: 2, bytes: 15 });
    assert_eq!(totals["rs"], Bucket { count: 1, bytes: 3 });
    assert_eq!(scan.total_files(), 3);
    assert_eq!(scan.total_bytes(), 18);
}

#[test]
fn aggregates_across_nested_directories() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "top.txt", 1);
    write_file(dir.path(), "one/mid.txt", 2);
    write_file(dir.path(), "one/two/deep.txt", 4);
    write_file(dir.path(), "one/two/three/deeper.txt", 8);

    let scan = scan_dir(dir.path());

    assert_eq!(totals(&scan)["txt"], Bucket { count: 4, bytes: 15 });
}

#[test]
fn hidden_files_and_directories_are_counted_by_default() {
    // Regression: jwalk skips hidden entries unless told otherwise, so these
    // were silently missing from the totals.
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "visible.txt", 1);
    write_file(dir.path(), ".hidden.txt", 2);
    write_file(dir.path(), ".hidden_dir/buried.txt", 4);

    let scan = scan_dir(dir.path());

    assert_eq!(scan.total_files(), 3);
    assert_eq!(scan.total_bytes(), 7);
}

#[test]
fn skip_hidden_excludes_hidden_files_and_directories() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "visible.txt", 1);
    write_file(dir.path(), ".hidden.txt", 2);
    write_file(dir.path(), ".hidden_dir/buried.txt", 4);

    let scan = scan_dir_with(
        dir.path(),
        ScanOptions {
            skip_hidden: true,
            ..ScanOptions::default()
        },
    );

    assert_eq!(scan.total_files(), 1);
    assert_eq!(scan.total_bytes(), 1);
}

#[test]
fn extensionless_files_and_dotfiles_share_one_bucket() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "Makefile", 1);
    write_file(dir.path(), "LICENSE", 2);
    write_file(dir.path(), ".gitignore", 4);

    let scan = scan_dir(dir.path());
    let totals = totals(&scan);

    assert_eq!(totals[NO_EXTENSION], Bucket { count: 3, bytes: 7 });
    assert_eq!(totals.len(), 1, "expected a single bucket: {totals:?}");
}

#[test]
fn an_unreadable_path_is_an_error_not_an_empty_scan() {
    // Regression: this used to report zero files and exit successfully.
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does-not-exist");

    let err = scan(&missing, &ScanOptions::default(), &NoProgress)
        .expect_err("scanning a missing path should fail");

    assert!(
        err.to_string().contains("cannot read path"),
        "unexpected error: {err}"
    );
}

#[test]
fn an_empty_directory_scans_successfully_with_no_totals() {
    let dir = TempDir::new().unwrap();

    let scan = scan_dir(dir.path());

    assert_eq!(scan.total_files(), 0);
    assert_eq!(scan.total_bytes(), 0);
    assert_eq!(scan.walk_error_count, 0);
    assert!(scan.rows().is_empty());
}

#[test]
fn max_depth_limits_how_far_the_walk_descends() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "top.txt", 1);
    write_file(dir.path(), "one/mid.txt", 2);
    write_file(dir.path(), "one/two/deep.txt", 4);

    // The root is depth 0, so depth 1 covers only its immediate children.
    let shallow = scan_dir_with(
        dir.path(),
        ScanOptions {
            max_depth: 1,
            ..ScanOptions::default()
        },
    );
    assert_eq!(shallow.total_files(), 1);
    assert_eq!(shallow.total_bytes(), 1);

    let deeper = scan_dir_with(
        dir.path(),
        ScanOptions {
            max_depth: 2,
            ..ScanOptions::default()
        },
    );
    assert_eq!(deeper.total_files(), 2);
    assert_eq!(deeper.total_bytes(), 3);
}

#[test]
fn rows_are_sorted_by_descending_count() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "a.rs", 1);
    write_file(dir.path(), "b.rs", 1);
    write_file(dir.path(), "c.rs", 1);
    write_file(dir.path(), "d.md", 1);
    write_file(dir.path(), "e.md", 1);
    write_file(dir.path(), "f.txt", 1);

    let scan = scan_dir(dir.path());
    let order: Vec<&str> = scan.rows().iter().map(|(extension, _)| *extension).collect();

    assert_eq!(order, ["rs", "md", "txt"]);
}

#[cfg(unix)]
#[test]
fn file_names_that_are_not_utf8_do_not_panic() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // Regression: unwrapping `to_str()` aborted the whole scan on such names.
    let dir = TempDir::new().unwrap();
    let name = OsStr::from_bytes(b"bad\xff\xfename.txt");
    fs::write(dir.path().join(name), vec![b'x'; 6]).unwrap();

    let scan = scan_dir(dir.path());

    assert_eq!(totals(&scan)["txt"], Bucket { count: 1, bytes: 6 });
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_counted_as_files() {
    // Counting the link and its target would double-count the bytes.
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "real.txt", 10);
    std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).unwrap();

    let scan = scan_dir(dir.path());

    assert_eq!(totals(&scan)["txt"], Bucket { count: 1, bytes: 10 });
}

#[cfg(unix)]
#[test]
fn a_broken_symlink_does_not_abort_the_scan() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "real.txt", 10);
    std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("dangling.txt")).unwrap();

    let scan = scan_dir(dir.path());

    assert_eq!(scan.total_files(), 1);
    assert_eq!(scan.total_bytes(), 10);
}

#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_rather_than_ignored() {
    use std::os::unix::fs::PermissionsExt;

    // Regression: the failure is recorded on the directory entry rather than
    // yielded as an iterator error, so it was dropped and the totals were
    // quietly incomplete.
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "readable.txt", 10);
    write_file(dir.path(), "locked/hidden.txt", 20);

    let locked = dir.path().join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    // root ignores permission bits, so the fixture would deny nothing there.
    let denied = fs::read_dir(&locked).is_err();
    if denied {
        let scan = scan_dir(dir.path());

        assert_eq!(scan.walk_error_count, 1);
        assert_eq!(scan.walk_errors.len(), 1);
        assert!(
            scan.walk_errors[0].contains("locked"),
            "error should name the directory: {:?}",
            scan.walk_errors
        );
        // The readable part of the tree is still reported.
        assert_eq!(scan.total_files(), 1);
        assert_eq!(scan.total_bytes(), 10);
    }

    // Restore permissions so the temporary directory can be cleaned up.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    if !denied {
        eprintln!("skipped: running with privileges that bypass permission bits");
    }
}
