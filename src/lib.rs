//! Filesystem scanning and per-extension aggregation.
//!
//! The binary is a thin CLI over [`scan`]; keeping the walk and the
//! aggregation here means both can be exercised without going through the
//! terminal output or the CSV writer.

use anyhow::{Context, Result};
use jwalk::{Parallelism, WalkDir};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bucket used for files that have no extension (including dotfiles such as
/// `.gitignore`, which `Path::extension` reports as extensionless).
pub const NO_EXTENSION: &str = "<none>";

/// Number of individual walk errors kept for reporting before the rest are
/// only counted.
pub const MAX_REPORTED_ERRORS: usize = 10;

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

/// How the walk should be performed.
pub struct ScanOptions {
    pub max_depth: usize,
    pub threads: usize,
    /// Leave hidden files and directories out of the totals. Off by default,
    /// so dot-directories such as `.git` are counted.
    pub skip_hidden: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: usize::MAX,
            threads: num_cpus::get(),
            skip_hidden: false,
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

    // Each rayon thread aggregates into its own map and the maps are merged at
    // the end, so no lock is taken on the per-file path.
    let totals: HashMap<String, Bucket> = files
        .par_iter()
        .fold(
            HashMap::new,
            |mut acc: HashMap<String, Bucket>, entry| {
                progress.file_measured();

                let size = match entry.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(_) => {
                        unmeasurable.fetch_add(1, Ordering::Relaxed);
                        return acc;
                    }
                };

                acc.entry(extension_of(entry.file_name()))
                    .or_default()
                    .add_file(size);

                acc
            },
        )
        .reduce(HashMap::new, |mut acc, partial| {
            for (extension, bucket) in partial {
                acc.entry(extension).or_default().merge(bucket);
            }
            acc
        });

    Ok(Scan {
        totals,
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
}
