//! Filesystem scanning and per-extension aggregation.
//!
//! The binary is a thin CLI over [`scan`]; keeping the walk and the
//! aggregation here means both can be exercised without going through the
//! terminal output or the CSV writer.

use anyhow::{Context, Result};
use jwalk::{Parallelism, WalkDir};
use rayon::prelude::*;
use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
#[cfg(test)]
use std::time::Duration;

pub mod diff;

/// Bucket used for files that have no extension (including dotfiles such as
/// `.gitignore`, which `Path::extension` reports as extensionless).
pub const NO_EXTENSION: &str = "<none>";

/// Number of individual walk errors kept for reporting before the rest are
/// only counted.
pub const MAX_REPORTED_ERRORS: usize = 10;

const SECONDS_PER_DAY: u64 = 60 * 60 * 24;

/// Running totals for a single extension.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub count: u64,
    pub bytes: u64,
}

impl Bucket {
    fn add_file(&mut self, size: u64) {
        self.count += 1;
        self.bytes += size;
    }

    fn merge(&mut self, other: Bucket) {
        self.count += other.count;
        self.bytes += other.bytes;
    }

    /// Mean file size in bytes, or zero for an empty bucket.
    pub fn average_bytes(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.bytes as f64 / self.count as f64
        }
    }
}

/// How old a file is, in the coarse bands used for the age report.
///
/// Declaration order is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AgeBucket {
    /// Modified after the scan began. Usually clock skew between the scanning
    /// host and the file server rather than a genuine age.
    Future,
    UpTo30Days,
    From30To90Days,
    From90DaysTo1Year,
    From1To2Years,
    Over2Years,
    /// The filesystem did not report a modification time.
    Unknown,
}

impl AgeBucket {
    /// Every bucket, in display order.
    pub const ALL: [AgeBucket; 7] = [
        AgeBucket::Future,
        AgeBucket::UpTo30Days,
        AgeBucket::From30To90Days,
        AgeBucket::From90DaysTo1Year,
        AgeBucket::From1To2Years,
        AgeBucket::Over2Years,
        AgeBucket::Unknown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AgeBucket::Future => "modified in future",
            AgeBucket::UpTo30Days => "under 30 days",
            AgeBucket::From30To90Days => "30 to 90 days",
            AgeBucket::From90DaysTo1Year => "90 days to 1 year",
            AgeBucket::From1To2Years => "1 to 2 years",
            AgeBucket::Over2Years => "over 2 years",
            AgeBucket::Unknown => "unknown",
        }
    }
}

/// Place a modification time into an [`AgeBucket`] relative to `now`.
///
/// A year is treated as 365 days; the bands are coarse enough that leap days
/// make no difference. A time later than `now` is reported as
/// [`AgeBucket::Future`] rather than being clamped, since it means the clocks
/// disagree and the age is not trustworthy.
pub fn classify_age(modified: Option<SystemTime>, now: SystemTime) -> AgeBucket {
    let Some(modified) = modified else {
        return AgeBucket::Unknown;
    };

    let Ok(age) = now.duration_since(modified) else {
        return AgeBucket::Future;
    };

    match age.as_secs() / SECONDS_PER_DAY {
        0..=29 => AgeBucket::UpTo30Days,
        30..=89 => AgeBucket::From30To90Days,
        90..=364 => AgeBucket::From90DaysTo1Year,
        365..=729 => AgeBucket::From1To2Years,
        _ => AgeBucket::Over2Years,
    }
}

/// One of the largest files seen during a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargestFile {
    pub path: PathBuf,
    pub bytes: u64,
}

impl Ord for LargestFile {
    /// Greater means "belongs nearer the top of the report": bigger first,
    /// and for equal sizes the earlier path, so output does not depend on the
    /// order threads happened to visit files in.
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.bytes
            .cmp(&other.bytes)
            .then_with(|| other.path.cmp(&self.path))
    }
}

impl PartialOrd for LargestFile {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

/// How the walk should be performed.
pub struct ScanOptions {
    pub max_depth: usize,
    pub threads: usize,
    /// Leave hidden files and directories out of the totals. Off by default,
    /// so dot-directories such as `.git` are counted.
    pub skip_hidden: bool,
    /// How many of the largest files to retain. Zero disables the report and
    /// avoids the per-file path allocation entirely.
    pub top: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: usize::MAX,
            threads: num_cpus::get(),
            skip_hidden: false,
            top: 10,
        }
    }
}

/// Per-thread accumulator, merged once per rayon worker rather than per file.
#[derive(Default)]
struct Partial {
    extensions: HashMap<String, Bucket>,
    ages: HashMap<AgeBucket, Bucket>,
    /// Min-heap, so the smallest retained file is the one evicted.
    largest: BinaryHeap<Reverse<LargestFile>>,
}

impl Partial {
    /// Offer a file to the largest-files heap.
    ///
    /// The path is produced lazily: building it allocates, and the vast
    /// majority of files in a large scan are never candidates.
    fn consider<F>(&mut self, bytes: u64, top: usize, path: F)
    where
        F: FnOnce() -> PathBuf,
    {
        if top == 0 {
            return;
        }

        if self.largest.len() < top {
            self.largest.push(Reverse(LargestFile { path: path(), bytes }));
            return;
        }

        // Reject without allocating whenever the file cannot possibly place.
        // Equal sizes still go through, so the tie-break decides rather than
        // arrival order.
        match self.largest.peek() {
            Some(Reverse(smallest)) if bytes < smallest.bytes => return,
            None => return,
            _ => {}
        }

        self.absorb(LargestFile { path: path(), bytes }, top);
    }

    fn absorb(&mut self, file: LargestFile, top: usize) {
        self.largest.push(Reverse(file));
        if self.largest.len() > top {
            self.largest.pop();
        }
    }

    fn merge(&mut self, other: Partial, top: usize) {
        for (extension, bucket) in other.extensions {
            self.extensions.entry(extension).or_default().merge(bucket);
        }
        for (age, bucket) in other.ages {
            self.ages.entry(age).or_default().merge(bucket);
        }
        for Reverse(file) in other.largest {
            self.absorb(file, top);
        }
    }
}

/// Progress reporting hook, so the scan itself stays free of terminal output.
pub trait Progress: Sync {
    /// Called once, after the walk finishes and before any file is measured.
    fn files_listed(&self, _total: u64) {}
    /// Called once per file, from whichever worker thread handles it.
    fn file_measured(&self) {}
}

/// A [`Progress`] implementation that reports nothing.
pub struct NoProgress;
impl Progress for NoProgress {}

/// The result of a scan, including what could not be measured.
#[derive(Debug, Default)]
pub struct Scan {
    pub totals: HashMap<String, Bucket>,
    /// Totals by file age. Buckets with no files are absent.
    pub ages: HashMap<AgeBucket, Bucket>,
    /// The largest files found, biggest first, at most `ScanOptions::top`.
    pub largest: Vec<LargestFile>,
    /// First [`MAX_REPORTED_ERRORS`] walk failures, for display.
    pub walk_errors: Vec<String>,
    /// Total number of walk failures, including any beyond those retained.
    pub walk_error_count: u64,
    /// Files listed by the walk that could not subsequently be measured.
    pub unmeasurable_files: u64,
}

impl Scan {
    /// Extensions ordered by descending file count, ties broken by name so
    /// that repeated scans of the same tree produce identical output.
    pub fn rows(&self) -> Vec<(&str, Bucket)> {
        let mut rows: Vec<(&str, Bucket)> = self
            .totals
            .iter()
            .map(|(extension, bucket)| (extension.as_str(), *bucket))
            .collect();
        rows.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        rows
    }

    pub fn total_files(&self) -> u64 {
        self.totals.values().map(|bucket| bucket.count).sum()
    }

    pub fn total_bytes(&self) -> u64 {
        self.totals.values().map(|bucket| bucket.bytes).sum()
    }

    /// Mean size across every file measured, or zero if nothing was measured.
    pub fn average_bytes(&self) -> f64 {
        let count = self.total_files();
        if count == 0 {
            0.0
        } else {
            self.total_bytes() as f64 / count as f64
        }
    }

    /// Age buckets in display order, skipping any that hold no files.
    pub fn age_rows(&self) -> Vec<(AgeBucket, Bucket)> {
        AgeBucket::ALL
            .iter()
            .filter_map(|age| self.ages.get(age).map(|bucket| (*age, *bucket)))
            .collect()
    }
}

/// Extension of a file name, or [`NO_EXTENSION`] if it has none.
///
/// Uses `Path::extension` rather than splitting on '.', so `Makefile` and
/// `.gitignore` are reported as extensionless instead of as their own name.
/// Names that are not valid UTF-8 are lossily converted rather than panicking.
/// A name ending in a bare '.' has an empty extension, which is grouped with
/// the extensionless files rather than becoming a nameless bucket.
pub fn extension_of(file_name: &OsStr) -> String {
    Path::new(file_name)
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| NO_EXTENSION.to_string())
}

/// Walk `path` and total up file counts and sizes per extension.
///
/// Returns an error only if `path` itself cannot be read. Failures below the
/// root are counted on the returned [`Scan`] so that partial results are
/// usable but visibly incomplete.
pub fn scan(path: &Path, options: &ScanOptions, progress: &dyn Progress) -> Result<Scan> {
    // Fail loudly on a path that does not exist or cannot be read, rather than
    // reporting an empty scan as a success.
    std::fs::metadata(path)
        .with_context(|| format!("cannot read path: {}", path.display()))?;

    let mut walk_errors: Vec<String> = Vec::new();
    let mut walk_error_count = 0u64;

    let files: Vec<_> = WalkDir::new(path)
        .sort(true)
        .skip_hidden(options.skip_hidden)
        .max_depth(options.max_depth)
        .parallelism(Parallelism::RayonNewPool(options.threads))
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => {
                // A directory that could not be read is still yielded as an
                // entry, with the failure recorded on the entry itself. Its
                // contents are missing from the totals, so report it.
                if let Some(err) = &entry.read_children_error {
                    if walk_errors.len() < MAX_REPORTED_ERRORS {
                        walk_errors.push(err.to_string());
                    }
                    walk_error_count += 1;
                }
                Some(entry)
            }
            Err(err) => {
                if walk_errors.len() < MAX_REPORTED_ERRORS {
                    walk_errors.push(err.to_string());
                }
                walk_error_count += 1;
                None
            }
        })
        .filter(|entry| entry.file_type().is_file())
        .collect();

    progress.files_listed(files.len() as u64);

    // Files that vanished or became unreadable between the walk and the stat.
    let unmeasurable = AtomicU64::new(0);

    // Ages are measured against a single instant captured before any file is
    // examined, so a long scan does not drift files between buckets.
    let now = SystemTime::now();

    // Each rayon thread aggregates into its own accumulator and these are
    // merged at the end, so no lock is taken on the per-file path.
    let aggregated = files
        .par_iter()
        .fold(Partial::default, |mut acc: Partial, entry| {
            progress.file_measured();

            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    unmeasurable.fetch_add(1, Ordering::Relaxed);
                    return acc;
                }
            };

            let size = metadata.len();

            acc.extensions
                .entry(extension_of(entry.file_name()))
                .or_default()
                .add_file(size);

            // The modification time comes from the stat already performed, so
            // the age report costs no extra syscalls.
            acc.ages
                .entry(classify_age(metadata.modified().ok(), now))
                .or_default()
                .add_file(size);

            acc.consider(size, options.top, || entry.path());

            acc
        })
        .reduce(Partial::default, |mut acc, partial| {
            acc.merge(partial, options.top);
            acc
        });

    let mut largest: Vec<LargestFile> = aggregated
        .largest
        .into_iter()
        .map(|Reverse(file)| file)
        .collect();
    largest.sort_by(|a, b| b.cmp(a));

    Ok(Scan {
        totals: aggregated.extensions,
        ages: aggregated.ages,
        largest,
        walk_errors,
        walk_error_count,
        unmeasurable_files: unmeasurable.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(name: &str) -> &OsStr {
        OsStr::new(name)
    }

    #[test]
    fn extension_is_the_part_after_the_final_dot() {
        assert_eq!(extension_of(os("notes.txt")), "txt");
        assert_eq!(extension_of(os("archive.tar.gz")), "gz");
    }

    #[test]
    fn files_without_an_extension_are_grouped_together() {
        assert_eq!(extension_of(os("Makefile")), NO_EXTENSION);
        assert_eq!(extension_of(os("LICENSE")), NO_EXTENSION);
    }

    #[test]
    fn dotfiles_are_extensionless_not_their_own_extension() {
        // Regression: splitting on '.' reported these as "gitignore" / "bashrc",
        // creating a bogus extension per dotfile.
        assert_eq!(extension_of(os(".gitignore")), NO_EXTENSION);
        assert_eq!(extension_of(os(".bashrc")), NO_EXTENSION);
        // A dotfile that really does carry an extension still resolves.
        assert_eq!(extension_of(os(".config.json")), "json");
    }

    #[test]
    fn odd_names_do_not_panic() {
        assert_eq!(extension_of(os("")), NO_EXTENSION);
        assert_eq!(extension_of(os(".")), NO_EXTENSION);
        assert_eq!(extension_of(os("..")), NO_EXTENSION);
        assert_eq!(extension_of(os("trailing.")), NO_EXTENSION);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_converted_lossily_rather_than_panicking() {
        use std::os::unix::ffi::OsStrExt;

        // Regression: `.to_str().unwrap()` aborted the entire scan here.
        let valid_extension = OsStr::from_bytes(b"bad\xff\xfename.txt");
        assert_eq!(extension_of(valid_extension), "txt");

        // The extension itself being invalid UTF-8 must also be survivable.
        let invalid_extension = OsStr::from_bytes(b"name.\xff\xfe");
        assert_eq!(extension_of(invalid_extension), "\u{fffd}\u{fffd}");
    }

    #[test]
    fn merging_buckets_sums_counts_and_bytes() {
        let mut a = Bucket::default();
        a.add_file(10);
        a.add_file(5);

        let mut b = Bucket::default();
        b.add_file(1);

        a.merge(b);

        assert_eq!(a, Bucket { count: 3, bytes: 16 });
    }

    fn scan_with(totals: &[(&str, u64, u64)]) -> Scan {
        Scan {
            totals: totals
                .iter()
                .map(|(ext, count, bytes)| {
                    (
                        (*ext).to_string(),
                        Bucket {
                            count: *count,
                            bytes: *bytes,
                        },
                    )
                })
                .collect(),
            ..Scan::default()
        }
    }

    #[test]
    fn rows_are_ordered_by_descending_count() {
        let scan = scan_with(&[("txt", 1, 100), ("rs", 9, 20), ("md", 4, 7)]);

        let order: Vec<&str> = scan.rows().iter().map(|(ext, _)| *ext).collect();
        assert_eq!(order, ["rs", "md", "txt"]);
    }

    #[test]
    fn equal_counts_are_ordered_by_name_so_output_is_stable() {
        let scan = scan_with(&[("zzz", 2, 1), ("aaa", 2, 1), ("mmm", 2, 1)]);

        let order: Vec<&str> = scan.rows().iter().map(|(ext, _)| *ext).collect();
        assert_eq!(order, ["aaa", "mmm", "zzz"]);
    }

    #[test]
    fn totals_sum_across_every_extension() {
        let scan = scan_with(&[("txt", 1, 100), ("rs", 9, 20)]);

        assert_eq!(scan.total_files(), 10);
        assert_eq!(scan.total_bytes(), 120);
    }

    #[test]
    fn average_size_divides_bytes_by_file_count() {
        let bucket = Bucket {
            count: 4,
            bytes: 10,
        };

        assert_eq!(bucket.average_bytes(), 2.5);
    }

    #[test]
    fn average_size_of_an_empty_bucket_is_zero_not_a_division_by_zero() {
        assert_eq!(Bucket::default().average_bytes(), 0.0);
        assert!(Bucket::default().average_bytes().is_finite());
    }

    #[test]
    fn overall_average_is_taken_across_all_extensions_not_per_extension() {
        // Averaging the two per-extension averages would give 55; the mean
        // file is much smaller than that because most files are the small ones.
        let scan = scan_with(&[("txt", 1, 100), ("rs", 9, 90)]);

        assert_eq!(scan.average_bytes(), 19.0);
    }

    #[test]
    fn overall_average_of_an_empty_scan_is_zero() {
        let scan = Scan::default();

        assert_eq!(scan.average_bytes(), 0.0);
        assert!(scan.average_bytes().is_finite());
    }

    /// A fixed instant to measure ages against, so the tests do not depend on
    /// the wall clock.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(30 * 365 * SECONDS_PER_DAY)
    }

    fn days_ago(days: u64) -> Option<SystemTime> {
        Some(now() - Duration::from_secs(days * SECONDS_PER_DAY))
    }

    #[test]
    fn ages_land_in_the_expected_band() {
        assert_eq!(classify_age(days_ago(0), now()), AgeBucket::UpTo30Days);
        assert_eq!(classify_age(days_ago(15), now()), AgeBucket::UpTo30Days);
        assert_eq!(classify_age(days_ago(45), now()), AgeBucket::From30To90Days);
        assert_eq!(
            classify_age(days_ago(200), now()),
            AgeBucket::From90DaysTo1Year
        );
        assert_eq!(classify_age(days_ago(500), now()), AgeBucket::From1To2Years);
        assert_eq!(classify_age(days_ago(3000), now()), AgeBucket::Over2Years);
    }

    #[test]
    fn age_band_boundaries_do_not_overlap_or_leave_gaps() {
        // Each boundary belongs to the older band, and the day before it does
        // not.
        assert_eq!(classify_age(days_ago(29), now()), AgeBucket::UpTo30Days);
        assert_eq!(classify_age(days_ago(30), now()), AgeBucket::From30To90Days);

        assert_eq!(classify_age(days_ago(89), now()), AgeBucket::From30To90Days);
        assert_eq!(
            classify_age(days_ago(90), now()),
            AgeBucket::From90DaysTo1Year
        );

        assert_eq!(
            classify_age(days_ago(364), now()),
            AgeBucket::From90DaysTo1Year
        );
        assert_eq!(classify_age(days_ago(365), now()), AgeBucket::From1To2Years);

        assert_eq!(classify_age(days_ago(729), now()), AgeBucket::From1To2Years);
        assert_eq!(classify_age(days_ago(730), now()), AgeBucket::Over2Years);
    }

    #[test]
    fn a_modification_time_in_the_future_is_flagged_not_treated_as_new() {
        // Clock skew between a scanning host and a file server is common, and
        // silently calling such files "under 30 days" would hide it.
        let ahead = now() + Duration::from_secs(SECONDS_PER_DAY);

        assert_eq!(classify_age(Some(ahead), now()), AgeBucket::Future);
    }

    #[test]
    fn a_missing_modification_time_is_unknown_rather_than_a_guess() {
        assert_eq!(classify_age(None, now()), AgeBucket::Unknown);
    }

    #[test]
    fn age_rows_come_back_in_display_order_and_skip_empty_bands() {
        let mut scan = Scan::default();
        scan.ages.insert(
            AgeBucket::Over2Years,
            Bucket {
                count: 1,
                bytes: 1,
            },
        );
        scan.ages.insert(
            AgeBucket::UpTo30Days,
            Bucket {
                count: 2,
                bytes: 2,
            },
        );

        let order: Vec<AgeBucket> = scan.age_rows().iter().map(|(age, _)| *age).collect();

        assert_eq!(order, [AgeBucket::UpTo30Days, AgeBucket::Over2Years]);
    }

    #[test]
    fn every_age_bucket_has_a_label() {
        for age in AgeBucket::ALL {
            assert!(!age.label().is_empty());
        }
    }

    fn file(path: &str, bytes: u64) -> LargestFile {
        LargestFile {
            path: PathBuf::from(path),
            bytes,
        }
    }

    #[test]
    fn larger_files_sort_ahead_of_smaller_ones() {
        assert!(file("a", 100) > file("b", 10));
    }

    #[test]
    fn equally_sized_files_are_ordered_by_path_so_results_are_reproducible() {
        // "Greater" is what appears first, so the earlier path must win.
        assert!(file("aaa", 100) > file("zzz", 100));
    }

    #[test]
    fn the_heap_keeps_the_largest_and_evicts_the_rest() {
        let mut partial = Partial::default();

        for (path, bytes) in [("a", 10), ("b", 500), ("c", 1), ("d", 90)] {
            partial.consider(bytes, 2, || PathBuf::from(path));
        }

        let mut kept: Vec<LargestFile> = partial
            .largest
            .into_iter()
            .map(|Reverse(file)| file)
            .collect();
        kept.sort_by(|a, b| b.cmp(a));

        assert_eq!(kept, vec![file("b", 500), file("d", 90)]);
    }

    #[test]
    fn a_top_of_zero_keeps_nothing() {
        let mut partial = Partial::default();

        partial.consider(500, 0, || PathBuf::from("a"));

        assert!(partial.largest.is_empty());
    }

    #[test]
    fn equally_sized_files_beyond_the_limit_are_resolved_by_path_not_arrival() {
        // Feeding the same files in opposite orders must give the same answer,
        // otherwise parallel scans would not be reproducible.
        let insert = |order: [&str; 3]| {
            let mut partial = Partial::default();
            for path in order {
                partial.consider(100, 2, || PathBuf::from(path));
            }
            let mut kept: Vec<LargestFile> = partial
                .largest
                .into_iter()
                .map(|Reverse(file)| file)
                .collect();
            kept.sort_by(|a, b| b.cmp(a));
            kept
        };

        assert_eq!(insert(["a", "b", "c"]), insert(["c", "b", "a"]));
        assert_eq!(insert(["a", "b", "c"]), vec![file("a", 100), file("b", 100)]);
    }
}
