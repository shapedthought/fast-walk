//! End-to-end tests for [`fast_walk::scan`] against real directory trees.

use fast_walk::{scan, AgeBucket, Bucket, NoProgress, Scan, ScanOptions, SizeBand, NO_EXTENSION};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

const SECONDS_PER_DAY: u64 = 60 * 60 * 24;

/// Write a file and backdate its modification time.
fn write_aged_file(root: &Path, relative: &str, size: usize, days_old: u64) {
    write_file(root, relative, size);

    let when = SystemTime::now() - Duration::from_secs(days_old * SECONDS_PER_DAY);
    fs::File::options()
        .write(true)
        .open(root.join(relative))
        .unwrap()
        .set_modified(when)
        .unwrap();
}

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

#[test]
fn files_are_bucketed_by_how_long_ago_they_were_modified() {
    let dir = TempDir::new().unwrap();
    write_aged_file(dir.path(), "fresh.txt", 10, 1);
    write_aged_file(dir.path(), "recent.txt", 20, 45);
    write_aged_file(dir.path(), "stale.txt", 40, 200);
    write_aged_file(dir.path(), "ancient.txt", 80, 1500);

    let scan = scan_dir(dir.path());

    assert_eq!(scan.ages[&AgeBucket::UpTo30Days], Bucket { count: 1, bytes: 10 });
    assert_eq!(
        scan.ages[&AgeBucket::From30To90Days],
        Bucket { count: 1, bytes: 20 }
    );
    assert_eq!(
        scan.ages[&AgeBucket::From90DaysTo1Year],
        Bucket { count: 1, bytes: 40 }
    );
    assert_eq!(scan.ages[&AgeBucket::Over2Years], Bucket { count: 1, bytes: 80 });
}

#[test]
fn the_age_buckets_account_for_every_file_exactly_once() {
    let dir = TempDir::new().unwrap();
    write_aged_file(dir.path(), "a.txt", 10, 1);
    write_aged_file(dir.path(), "b.txt", 20, 45);
    write_aged_file(dir.path(), "c.txt", 40, 800);

    let scan = scan_dir(dir.path());

    let aged_files: u64 = scan.ages.values().map(|bucket| bucket.count).sum();
    let aged_bytes: u64 = scan.ages.values().map(|bucket| bucket.bytes).sum();

    assert_eq!(aged_files, scan.total_files());
    assert_eq!(aged_bytes, scan.total_bytes());
}

#[test]
fn a_file_modified_in_the_future_is_reported_separately() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "skewed.txt", 10);

    let ahead = SystemTime::now() + Duration::from_secs(7 * SECONDS_PER_DAY);
    fs::File::options()
        .write(true)
        .open(dir.path().join("skewed.txt"))
        .unwrap()
        .set_modified(ahead)
        .unwrap();

    let scan = scan_dir(dir.path());

    assert_eq!(scan.ages[&AgeBucket::Future], Bucket { count: 1, bytes: 10 });
}

#[test]
fn the_largest_files_are_reported_biggest_first() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "small.bin", 10);
    write_file(dir.path(), "huge.bin", 5000);
    write_file(dir.path(), "nested/medium.bin", 900);
    write_file(dir.path(), "tiny.bin", 1);

    let scan = scan_dir_with(
        dir.path(),
        ScanOptions {
            top: 2,
            ..ScanOptions::default()
        },
    );

    let names: Vec<String> = scan
        .largest
        .iter()
        .map(|file| file.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    assert_eq!(names, ["huge.bin", "medium.bin"]);
    assert_eq!(scan.largest[0].bytes, 5000);
    assert_eq!(scan.largest[1].bytes, 900);
}

#[test]
fn asking_for_more_largest_files_than_exist_returns_what_there_is() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "a.bin", 10);
    write_file(dir.path(), "b.bin", 20);

    let scan = scan_dir_with(
        dir.path(),
        ScanOptions {
            top: 50,
            ..ScanOptions::default()
        },
    );

    assert_eq!(scan.largest.len(), 2);
}

#[test]
fn a_top_of_zero_turns_the_largest_files_report_off() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "a.bin", 10);

    let scan = scan_dir_with(
        dir.path(),
        ScanOptions {
            top: 0,
            ..ScanOptions::default()
        },
    );

    assert!(scan.largest.is_empty());
    // The rest of the report is unaffected.
    assert_eq!(scan.total_files(), 1);
}

#[test]
fn equally_sized_files_produce_the_same_list_on_every_run() {
    // Threads finish in whatever order they like, so ties have to be broken by
    // something stable or repeated scans would disagree.
    let dir = TempDir::new().unwrap();
    for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        write_file(dir.path(), &format!("{name}.bin"), 100);
    }

    let options = || ScanOptions {
        top: 3,
        ..ScanOptions::default()
    };

    let first = scan_dir_with(dir.path(), options());
    for _ in 0..5 {
        let again = scan_dir_with(dir.path(), options());
        assert_eq!(first.largest, again.largest);
    }
}

#[test]
fn files_are_bucketed_by_size() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "empty.bin", 0);
    write_file(dir.path(), "tiny.bin", 100);
    write_file(dir.path(), "small.bin", 10_000);
    write_file(dir.path(), "medium.bin", 500_000);

    let scan = scan_dir(dir.path());

    assert_eq!(scan.sizes[&SizeBand::Empty].count, 1);
    assert_eq!(scan.sizes[&SizeBand::Under4K].count, 1);
    assert_eq!(scan.sizes[&SizeBand::From4KTo64K].count, 1);
    assert_eq!(scan.sizes[&SizeBand::From64KTo1M].count, 1);
}

#[test]
fn the_size_bands_account_for_every_file_exactly_once() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "a.bin", 0);
    write_file(dir.path(), "b.bin", 100);
    write_file(dir.path(), "nested/c.bin", 200_000);

    let scan = scan_dir(dir.path());

    let banded_files: u64 = scan.sizes.values().map(|bucket| bucket.count).sum();
    let banded_bytes: u64 = scan.sizes.values().map(|bucket| bucket.bytes).sum();

    assert_eq!(banded_files, scan.total_files());
    assert_eq!(banded_bytes, scan.total_bytes());
}

#[test]
fn the_directory_with_the_most_small_files_is_reported_first() {
    // The backup case: one directory of many tiny files is far more expensive
    // to back up than another holding the same bytes in one file.
    let dir = TempDir::new().unwrap();
    for i in 0..50 {
        write_file(dir.path(), &format!("many_small/f{i}.bin"), 10);
    }
    for i in 0..5 {
        write_file(dir.path(), &format!("few_small/f{i}.bin"), 10);
    }
    write_file(dir.path(), "one_big/blob.bin", 5_000_000);

    let scan = scan_dir(dir.path());

    assert_eq!(scan.hotspots[0].directory, dir.path().join("many_small"));
    assert_eq!(scan.hotspots[0].stats.small_files, 50);
    assert_eq!(scan.hotspots[1].directory, dir.path().join("few_small"));
    assert_eq!(scan.hotspots[1].stats.small_files, 5);
}

#[test]
fn directories_holding_no_small_files_are_not_hotspots() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "big/blob.bin", 5_000_000);
    write_file(dir.path(), "small/f.bin", 10);

    let scan = scan_dir(dir.path());

    let listed: Vec<&Path> = scan
        .hotspots
        .iter()
        .map(|hotspot| hotspot.directory.as_path())
        .collect();

    assert_eq!(listed, [dir.path().join("small").as_path()]);
}

#[test]
fn the_small_file_threshold_is_configurable() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "data/a.bin", 5_000);
    write_file(dir.path(), "data/b.bin", 50_000);

    let strict = scan_dir_with(
        dir.path(),
        ScanOptions {
            small_at_or_below: 10_000,
            ..ScanOptions::default()
        },
    );
    assert_eq!(strict.small_files, 1);

    let loose = scan_dir_with(
        dir.path(),
        ScanOptions {
            small_at_or_below: 100_000,
            ..ScanOptions::default()
        },
    );
    assert_eq!(loose.small_files, 2);
}

#[test]
fn hotspots_are_capped_but_the_small_file_total_is_not() {
    // The headline figure must cover the whole scan, not just the directories
    // that made the list.
    let dir = TempDir::new().unwrap();
    for directory in 0..5 {
        for file in 0..4 {
            write_file(dir.path(), &format!("d{directory}/f{file}.bin"), 10);
        }
    }

    let scan = scan_dir_with(
        dir.path(),
        ScanOptions {
            hotspots: 2,
            ..ScanOptions::default()
        },
    );

    assert_eq!(scan.hotspots.len(), 2);
    assert_eq!(scan.small_files, 20);
    assert_eq!(scan.small_bytes, 200);
}

#[test]
fn turning_hotspots_off_still_reports_the_small_file_total() {
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "data/a.bin", 10);
    write_file(dir.path(), "data/b.bin", 20);

    let scan = scan_dir_with(
        dir.path(),
        ScanOptions {
            hotspots: 0,
            ..ScanOptions::default()
        },
    );

    assert!(scan.hotspots.is_empty());
    assert_eq!(scan.small_files, 2);
    assert_eq!(scan.small_bytes, 30);
    assert_eq!(scan.small_share(), 1.0);
}

#[test]
fn hotspots_are_attributed_to_the_directory_holding_the_files() {
    // Files must count towards their own directory, not an ancestor.
    let dir = TempDir::new().unwrap();
    write_file(dir.path(), "top.bin", 10);
    write_file(dir.path(), "a/b/deep.bin", 10);

    let scan = scan_dir(dir.path());

    let mut listed: Vec<&Path> = scan
        .hotspots
        .iter()
        .map(|hotspot| hotspot.directory.as_path())
        .collect();
    listed.sort();

    let nested = dir.path().join("a/b");
    let mut expected = vec![dir.path(), nested.as_path()];
    expected.sort();

    assert_eq!(listed, expected);
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
