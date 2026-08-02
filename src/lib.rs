//! Filesystem scanning and per-extension aggregation.
//!
//! The binary is a thin CLI over [`scan`]; keeping the walk and the
//! aggregation here means both can be exercised without going through the
//! terminal output or the CSV writer.

use anyhow::{bail, Context, Result};
use jwalk::{Parallelism, WalkDir};
use rayon::prelude::*;
use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;
use std::time::SystemTime;

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

/// Size bands, chosen around backup behaviour rather than round numbers.
///
/// Per-file overhead dominates backup throughput for small files no matter how
/// fast the link is, so the interesting detail is all at the bottom of the
/// range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SizeBand {
    /// Zero bytes: pure per-file overhead, no payload at all.
    Empty,
    Under4K,
    From4KTo64K,
    From64KTo1M,
    From1MTo16M,
    From16MTo128M,
    Over128M,
}

impl SizeBand {
    /// Every band, smallest first. Declaration order is display order.
    pub const ALL: [SizeBand; 7] = [
        SizeBand::Empty,
        SizeBand::Under4K,
        SizeBand::From4KTo64K,
        SizeBand::From64KTo1M,
        SizeBand::From1MTo16M,
        SizeBand::From16MTo128M,
        SizeBand::Over128M,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SizeBand::Empty => "empty",
            SizeBand::Under4K => "under 4 KB",
            SizeBand::From4KTo64K => "4 KB to 64 KB",
            SizeBand::From64KTo1M => "64 KB to 1 MB",
            SizeBand::From1MTo16M => "1 MB to 16 MB",
            SizeBand::From16MTo128M => "16 MB to 128 MB",
            SizeBand::Over128M => "over 128 MB",
        }
    }
}

/// Place a file size into a [`SizeBand`]. Each boundary belongs to the band
/// above it.
pub fn classify_size(bytes: u64) -> SizeBand {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;

    match bytes {
        0 => SizeBand::Empty,
        b if b < 4 * KB => SizeBand::Under4K,
        b if b < 64 * KB => SizeBand::From4KTo64K,
        b if b < MB => SizeBand::From64KTo1M,
        b if b < 16 * MB => SizeBand::From1MTo16M,
        b if b < 128 * MB => SizeBand::From16MTo128M,
        _ => SizeBand::Over128M,
    }
}

/// What one directory holds, used to find small-file concentrations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryStats {
    pub files: u64,
    pub bytes: u64,
    /// Files at or below the configured small-file threshold.
    pub small_files: u64,
    pub small_bytes: u64,
}

impl DirectoryStats {
    fn add_file(&mut self, size: u64, small_at_or_below: u64) {
        self.files += 1;
        self.bytes += size;
        if size <= small_at_or_below {
            self.small_files += 1;
            self.small_bytes += size;
        }
    }

    fn merge(&mut self, other: DirectoryStats) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.small_files += other.small_files;
        self.small_bytes += other.small_bytes;
    }

    /// Mean file size in bytes, or zero for an empty directory.
    pub fn average_bytes(&self) -> f64 {
        if self.files == 0 {
            0.0
        } else {
            self.bytes as f64 / self.files as f64
        }
    }

    /// Proportion of this directory's files that count as small, from 0 to 1.
    pub fn small_share(&self) -> f64 {
        if self.files == 0 {
            0.0
        } else {
            self.small_files as f64 / self.files as f64
        }
    }
}

/// A directory holding enough small files to be worth targeting separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotspot {
    pub directory: PathBuf,
    pub stats: DirectoryStats,
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

/// Levels given a row of their own in the structure report. A tree deeper
/// than this is counted and its depth still reported exactly, but the entries
/// share one overflow row: the array of counters is fixed so that the walk
/// threads can accumulate into it without locking.
pub const MAX_TRACKED_DEPTH: usize = 64;

/// Path length at which a tree starts to break backup agents on Windows, and
/// the reason path lengths are reported at all.
pub const LONG_PATH_LIMIT: usize = 260;

/// How many subdirectories one directory holds.
///
/// Bands rather than exact counts: the report is meant to describe the shape
/// of a tree to someone who should not see what is in it. Declaration order is
/// display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FanOut {
    /// No subdirectories at all, so a leaf of the tree.
    Leaf,
    One,
    From2To9,
    From10To99,
    From100To999,
    Over1000,
}

impl FanOut {
    /// Every band, fewest subdirectories first. Declaration order is display
    /// order.
    pub const ALL: [FanOut; 6] = [
        FanOut::Leaf,
        FanOut::One,
        FanOut::From2To9,
        FanOut::From10To99,
        FanOut::From100To999,
        FanOut::Over1000,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FanOut::Leaf => "none (leaf)",
            FanOut::One => "1",
            FanOut::From2To9 => "2 to 9",
            FanOut::From10To99 => "10 to 99",
            FanOut::From100To999 => "100 to 999",
            FanOut::Over1000 => "1000 or more",
        }
    }

    /// Position in [`FanOut::ALL`], used to index the lock-free counters.
    fn index(self) -> usize {
        match self {
            FanOut::Leaf => 0,
            FanOut::One => 1,
            FanOut::From2To9 => 2,
            FanOut::From10To99 => 3,
            FanOut::From100To999 => 4,
            FanOut::Over1000 => 5,
        }
    }
}

/// Place a subdirectory count into a [`FanOut`] band.
pub fn classify_fan_out(subdirectories: u64) -> FanOut {
    match subdirectories {
        0 => FanOut::Leaf,
        1 => FanOut::One,
        2..=9 => FanOut::From2To9,
        10..=99 => FanOut::From10To99,
        100..=999 => FanOut::From100To999,
        _ => FanOut::Over1000,
    }
}

/// How many files one directory holds directly, not counting its
/// subdirectories.
///
/// Declaration order is display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileCount {
    /// Holds no files, only subdirectories or nothing at all.
    None,
    From1To9,
    From10To99,
    From100To999,
    From1000To9999,
    Over10000,
}

impl FileCount {
    /// Every band, fewest files first. Declaration order is display order.
    pub const ALL: [FileCount; 6] = [
        FileCount::None,
        FileCount::From1To9,
        FileCount::From10To99,
        FileCount::From100To999,
        FileCount::From1000To9999,
        FileCount::Over10000,
    ];

    pub fn label(self) -> &'static str {
        match self {
            FileCount::None => "none",
            FileCount::From1To9 => "1 to 9",
            FileCount::From10To99 => "10 to 99",
            FileCount::From100To999 => "100 to 999",
            FileCount::From1000To9999 => "1000 to 9999",
            FileCount::Over10000 => "10000 or more",
        }
    }

    /// Position in [`FileCount::ALL`], used to index the lock-free counters.
    fn index(self) -> usize {
        match self {
            FileCount::None => 0,
            FileCount::From1To9 => 1,
            FileCount::From10To99 => 2,
            FileCount::From100To999 => 3,
            FileCount::From1000To9999 => 4,
            FileCount::Over10000 => 5,
        }
    }
}

/// Place a directory's own file count into a [`FileCount`] band.
pub fn classify_file_count(files: u64) -> FileCount {
    match files {
        0 => FileCount::None,
        1..=9 => FileCount::From1To9,
        10..=99 => FileCount::From10To99,
        100..=999 => FileCount::From100To999,
        1000..=9999 => FileCount::From1000To9999,
        _ => FileCount::Over10000,
    }
}

/// A set of directories counted together, and the files that go with them.
///
/// What "the files that go with them" means depends on the report: for a
/// level it is the files sitting at that level, and for a band it is the files
/// held by the directories in the band. Each accessor says which.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirectoryGroup {
    pub directories: u64,
    pub files: u64,
}

impl DirectoryGroup {
    fn is_empty(&self) -> bool {
        self.directories == 0 && self.files == 0
    }
}

/// The shape of the scanned tree, described without naming anything in it.
///
/// Every figure is a count or a length, so the report says how the data is
/// laid out without disclosing a single directory name.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Structure {
    /// Entries per level, indexed by depth. Level 0 is the scan root itself
    /// and level 1 its immediate children, matching
    /// [`ScanOptions::max_depth`]. Files count at the level they sit at, not
    /// at the level of the directory holding them.
    pub levels: Vec<DirectoryGroup>,
    /// Entries deeper than [`MAX_TRACKED_DEPTH`], which get no level of their
    /// own.
    pub beyond_tracked: DirectoryGroup,
    /// Depth of the deepest entry counted. Tracked exactly, including past
    /// [`MAX_TRACKED_DEPTH`].
    pub deepest: usize,
    /// Every directory in the tree, the root included, whether or not it could
    /// be read.
    pub directories: u64,
    /// Directories whose contents were actually listed. Only these appear in
    /// the band reports, since the others have no known contents.
    pub listed_directories: u64,
    /// Listed directories by how many subdirectories they hold, with the files
    /// they hold directly.
    pub fan_out: HashMap<FanOut, DirectoryGroup>,
    /// Listed directories by how many files they hold directly, with those
    /// files.
    pub file_counts: HashMap<FileCount, DirectoryGroup>,
    /// Longest path below the scan root, in bytes of the encoded path. The
    /// root's own prefix is excluded because it changes when the tree is
    /// copied or restored somewhere else.
    pub longest_path: usize,
    /// Entries whose path below the root is longer than [`LONG_PATH_LIMIT`].
    pub long_paths: DirectoryGroup,
}

impl Structure {
    /// Levels paired with their depth, root first, skipping any that hold
    /// nothing. The `files` of a row are the files at that level.
    ///
    /// Anything past [`MAX_TRACKED_DEPTH`] is in `beyond_tracked` rather than
    /// here.
    pub fn level_rows(&self) -> Vec<(usize, DirectoryGroup)> {
        self.levels
            .iter()
            .enumerate()
            .filter(|(_, group)| !group.is_empty())
            .map(|(depth, group)| (depth, *group))
            .collect()
    }

    /// Fan-out bands in display order, skipping any holding no directories.
    /// The `files` of a row are the files held by those directories.
    pub fn fan_out_rows(&self) -> Vec<(FanOut, DirectoryGroup)> {
        FanOut::ALL
            .iter()
            .filter_map(|band| self.fan_out.get(band).map(|group| (*band, *group)))
            .collect()
    }

    /// File-count bands in display order, skipping any holding no directories.
    /// The `files` of a row are the files held by those directories.
    pub fn file_count_rows(&self) -> Vec<(FileCount, DirectoryGroup)> {
        FileCount::ALL
            .iter()
            .filter_map(|band| self.file_counts.get(band).map(|group| (*band, *group)))
            .collect()
    }

    /// Directories counted but never listed, because the depth limit stopped
    /// the walk or the directory could not be read. They are missing from the
    /// band reports, so a report that has any must say so.
    pub fn unlisted_directories(&self) -> u64 {
        self.directories.saturating_sub(self.listed_directories)
    }

    /// Every file the walk saw below the root.
    ///
    /// Taken from the walk rather than from the measuring pass, so it includes
    /// any file that was listed but could not afterwards be measured.
    pub fn files(&self) -> u64 {
        self.file_counts.values().map(|group| group.files).sum()
    }

    /// Mean number of files held directly by a listed directory, or zero if
    /// nothing was listed.
    pub fn mean_files_per_directory(&self) -> f64 {
        if self.listed_directories == 0 {
            0.0
        } else {
            self.files() as f64 / self.listed_directories as f64
        }
    }
}

/// Lock-free accumulator for [`Structure`].
///
/// The walk threads touch this once per directory rather than once per file,
/// so the counters are contended by the number of directories in the tree and
/// not by its size. Every field is a fixed-size counter, so unlike the hotspot
/// map the memory does not grow with the tree.
struct StructureCounters {
    levels: Vec<DirectoryCounters>,
    beyond_tracked: DirectoryCounters,
    deepest: AtomicUsize,
    /// The root is added separately, since no parent lists it.
    directories: AtomicU64,
    listed: AtomicU64,
    fan_out: [DirectoryCounters; FanOut::ALL.len()],
    file_counts: [DirectoryCounters; FileCount::ALL.len()],
    longest_path: AtomicUsize,
    long_path_directories: AtomicU64,
    long_path_files: AtomicU64,
}

#[derive(Default)]
struct DirectoryCounters {
    directories: AtomicU64,
    files: AtomicU64,
}

impl DirectoryCounters {
    fn add(&self, directories: u64, files: u64) {
        self.directories.fetch_add(directories, Ordering::Relaxed);
        self.files.fetch_add(files, Ordering::Relaxed);
    }

    fn take(&self) -> DirectoryGroup {
        DirectoryGroup {
            directories: self.directories.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
        }
    }
}

impl StructureCounters {
    fn new() -> Self {
        Self {
            levels: (0..=MAX_TRACKED_DEPTH)
                .map(|_| DirectoryCounters::default())
                .collect(),
            beyond_tracked: DirectoryCounters::default(),
            deepest: AtomicUsize::new(0),
            directories: AtomicU64::new(0),
            listed: AtomicU64::new(0),
            fan_out: Default::default(),
            file_counts: Default::default(),
            longest_path: AtomicUsize::new(0),
            long_path_directories: AtomicU64::new(0),
            long_path_files: AtomicU64::new(0),
        }
    }

    /// Record one directory whose contents have just been listed.
    ///
    /// `depth` is the directory's own depth, so its children sit one level
    /// below it. Entries that are neither a file nor a directory, such as an
    /// unfollowed symlink, are left out of both counts, matching the way the
    /// rest of the scan treats them.
    fn record(
        &self,
        depth: usize,
        relative_length: usize,
        children: impl IntoIterator<Item = Child>,
    ) {
        let mut files = 0u64;
        let mut subdirectories = 0u64;
        let mut long_files = 0u64;
        let mut long_directories = 0u64;
        let mut longest = 0usize;

        // Children share the directory's path, so their own length only
        // differs by the name and, below the root, one separator.
        let prefix = if relative_length == 0 {
            0
        } else {
            relative_length + 1
        };

        for child in children {
            let length = prefix + child.name_length;

            // Anything counted in neither total is left out of the length
            // figures too, so the longest path reported is always the longest
            // path among the entries the rest of the report describes.
            if child.is_dir {
                subdirectories += 1;
                longest = longest.max(length);
                if length > LONG_PATH_LIMIT {
                    long_directories += 1;
                }
            } else if child.is_file {
                files += 1;
                longest = longest.max(length);
                if length > LONG_PATH_LIMIT {
                    long_files += 1;
                }
            }
        }

        self.listed.fetch_add(1, Ordering::Relaxed);
        self.directories
            .fetch_add(subdirectories, Ordering::Relaxed);
        self.deepest.fetch_max(depth, Ordering::Relaxed);
        self.longest_path.fetch_max(longest, Ordering::Relaxed);
        self.long_path_directories
            .fetch_add(long_directories, Ordering::Relaxed);
        self.long_path_files
            .fetch_add(long_files, Ordering::Relaxed);

        if files > 0 || subdirectories > 0 {
            let level = depth + 1;
            self.deepest.fetch_max(level, Ordering::Relaxed);
            match self.levels.get(level) {
                Some(counters) => counters.add(subdirectories, files),
                None => self.beyond_tracked.add(subdirectories, files),
            }
        }

        self.fan_out[classify_fan_out(subdirectories).index()].add(1, files);
        self.file_counts[classify_file_count(files).index()].add(1, files);
    }

    /// Collapse the counters into the reported [`Structure`].
    ///
    /// `root_is_directory` is false when the scan was pointed at a single
    /// file, which has no level of its own to occupy.
    fn finish(&self, root_is_directory: bool) -> Structure {
        let mut levels: Vec<DirectoryGroup> =
            self.levels.iter().map(DirectoryCounters::take).collect();
        let mut directories = self.directories.load(Ordering::Relaxed);

        if root_is_directory {
            levels[0].directories += 1;
            directories += 1;
        }

        // Trailing levels hold nothing once the deepest entry is passed, and a
        // vector of empty rows is only noise for anything reading this.
        while levels.last().is_some_and(DirectoryGroup::is_empty) {
            levels.pop();
        }

        Structure {
            levels,
            beyond_tracked: self.beyond_tracked.take(),
            deepest: self.deepest.load(Ordering::Relaxed),
            directories,
            listed_directories: self.listed.load(Ordering::Relaxed),
            fan_out: banded(&self.fan_out, FanOut::ALL),
            file_counts: banded(&self.file_counts, FileCount::ALL),
            longest_path: self.longest_path.load(Ordering::Relaxed),
            long_paths: DirectoryGroup {
                directories: self.long_path_directories.load(Ordering::Relaxed),
                files: self.long_path_files.load(Ordering::Relaxed),
            },
        }
    }
}

/// Pair each band with its counters, dropping the bands nothing landed in.
fn banded<B: Copy + Eq + std::hash::Hash, const N: usize>(
    counters: &[DirectoryCounters; N],
    bands: [B; N],
) -> HashMap<B, DirectoryGroup> {
    counters
        .iter()
        .map(DirectoryCounters::take)
        .zip(bands)
        .filter(|(group, _)| !group.is_empty())
        .map(|(group, band)| (band, group))
        .collect()
}

/// The little a directory's child needs to contribute to [`Structure`].
///
/// Reduced to this before counting so the accounting can be unit tested
/// without a filesystem behind it.
struct Child {
    is_dir: bool,
    is_file: bool,
    name_length: usize,
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
    /// How many small-file directories to report. Zero disables the report and
    /// avoids tracking per-directory totals at all.
    pub hotspots: usize,
    /// A file this size or smaller counts as small. Backup throughput is
    /// dominated by per-file overhead below roughly this point.
    pub small_at_or_below: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: usize::MAX,
            threads: num_cpus::get(),
            skip_hidden: false,
            top: 10,
            hotspots: 10,
            small_at_or_below: 64 * 1024,
        }
    }
}

/// Per-thread accumulator, merged once per rayon worker rather than per file.
#[derive(Default)]
struct Partial {
    extensions: HashMap<String, Bucket>,
    ages: HashMap<AgeBucket, Bucket>,
    sizes: HashMap<SizeBand, Bucket>,
    /// Keyed by the directory holding the files, so its size is bounded by the
    /// number of directories rather than the number of files.
    directories: HashMap<PathBuf, DirectoryStats>,
    /// The directory currently being filled. The walk yields files grouped by
    /// directory and siblings share one `Arc`, so holding the last one turns
    /// a hash of the full path per file into a pointer comparison.
    pending: Option<(Arc<Path>, DirectoryStats)>,
    /// Counted for every file regardless of the hotspot setting, so the
    /// headline small-file figure is a whole-scan total rather than a sum over
    /// whichever directories made the report.
    small_files: u64,
    small_bytes: u64,
    /// Min-heap, so the smallest retained file is the one evicted.
    largest: BinaryHeap<Reverse<LargestFile>>,
}

impl Partial {
    /// Record a file against the directory holding it.
    fn record_directory(&mut self, parent: &Arc<Path>, size: u64, small_at_or_below: u64) {
        if let Some((cached, stats)) = &mut self.pending {
            if Arc::ptr_eq(cached, parent) {
                stats.add_file(size, small_at_or_below);
                return;
            }
        }

        // A different directory, so bank the previous one and start again.
        self.flush_pending();

        let mut stats = DirectoryStats::default();
        stats.add_file(size, small_at_or_below);
        self.pending = Some((Arc::clone(parent), stats));
    }

    /// Move the directory being filled into the map. Must be called before
    /// anything reads `directories`.
    fn flush_pending(&mut self) {
        if let Some((path, stats)) = self.pending.take() {
            self.directories
                .entry(path.to_path_buf())
                .or_default()
                .merge(stats);
        }
    }

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
            self.largest.push(Reverse(LargestFile {
                path: path(),
                bytes,
            }));
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

        self.absorb(
            LargestFile {
                path: path(),
                bytes,
            },
            top,
        );
    }

    fn absorb(&mut self, file: LargestFile, top: usize) {
        self.largest.push(Reverse(file));
        if self.largest.len() > top {
            self.largest.pop();
        }
    }

    fn merge(&mut self, mut other: Partial, top: usize) {
        self.flush_pending();
        other.flush_pending();

        for (extension, bucket) in other.extensions {
            self.extensions.entry(extension).or_default().merge(bucket);
        }
        for (age, bucket) in other.ages {
            self.ages.entry(age).or_default().merge(bucket);
        }
        for (band, bucket) in other.sizes {
            self.sizes.entry(band).or_default().merge(bucket);
        }
        for (directory, stats) in other.directories {
            self.directories.entry(directory).or_default().merge(stats);
        }
        self.small_files += other.small_files;
        self.small_bytes += other.small_bytes;
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
    /// Totals by file size. Bands with no files are absent.
    pub sizes: HashMap<SizeBand, Bucket>,
    /// Directories holding the most small files, worst first, at most
    /// `ScanOptions::hotspots`. Directories with no small files are excluded.
    pub hotspots: Vec<Hotspot>,
    /// The threshold the small-file figures were computed against.
    pub small_at_or_below: u64,
    /// Every file at or below the threshold, counted across the whole scan
    /// rather than only the reported hotspots, and independently of whether
    /// the hotspot report was enabled.
    pub small_files: u64,
    pub small_bytes: u64,
    /// The largest files found, biggest first, at most `ScanOptions::top`.
    pub largest: Vec<LargestFile>,
    /// How the tree is laid out, in counts and lengths only.
    pub structure: Structure,
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

    /// Size bands smallest first, skipping any that hold no files.
    pub fn size_rows(&self) -> Vec<(SizeBand, Bucket)> {
        SizeBand::ALL
            .iter()
            .filter_map(|band| self.sizes.get(band).map(|bucket| (*band, *bucket)))
            .collect()
    }

    /// Share of all files that are small, from 0 to 1.
    pub fn small_share(&self) -> f64 {
        let files = self.total_files();
        if files == 0 {
            0.0
        } else {
            self.small_files as f64 / files as f64
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
    let root_metadata =
        std::fs::metadata(path).with_context(|| format!("cannot read path: {}", path.display()))?;

    // One pool for both halves of the scan. Handing jwalk a thread count of
    // its own while letting the measuring phase fall through to rayon's global
    // pool meant --threads only ever governed the walk, and the measuring ran
    // at whatever the global pool defaulted to.
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(options.threads)
            .build()
            .context("cannot build the thread pool")?,
    );

    let mut walk_errors: Vec<String> = Vec::new();
    let mut walk_error_count = 0u64;
    // Set if the scanned directory itself could not be listed, as opposed to
    // one below it. That is not a partial result, it is no result.
    let mut root_unreadable: Option<String> = None;

    // Fed once per directory from the walk threads, before any file is
    // measured, so the shape of the tree costs no syscall of its own.
    let structure = Arc::new(StructureCounters::new());
    let counters = Arc::clone(&structure);
    let root = path.to_path_buf();

    let files: Vec<_> = WalkDir::new(path)
        .sort(true)
        .skip_hidden(options.skip_hidden)
        .max_depth(options.max_depth)
        .process_read_dir(move |depth, directory, _state, children| {
            // The root entry is handed over with no depth of its own and no
            // listing behind it, so there is nothing yet to describe.
            let Some(depth) = depth else {
                return;
            };

            // Measured below the root, since the prefix the tree currently
            // sits under changes the moment it is copied anywhere else.
            let relative_length = directory
                .strip_prefix(&root)
                .map(|below| below.as_os_str().len())
                .unwrap_or_else(|_| directory.as_os_str().len());

            counters.record(
                depth,
                relative_length,
                children
                    .iter()
                    .filter_map(|entry| entry.as_ref().ok())
                    .map(|entry| Child {
                        is_dir: entry.file_type().is_dir(),
                        is_file: entry.file_type().is_file(),
                        name_length: entry.file_name().len(),
                    }),
            );
        })
        .parallelism(Parallelism::RayonExistingPool {
            pool: Arc::clone(&pool),
            // The pool is built here and used by nothing else, so there is
            // always a free thread and no deadlock check is needed.
            busy_timeout: None,
        })
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => {
                // A directory that could not be read is still yielded as an
                // entry, with the failure recorded on the entry itself. Its
                // contents are missing from the totals, so report it.
                if let Some(err) = &entry.read_children_error {
                    if entry.depth() == 0 {
                        // The io error alone; jwalk's own message repeats the
                        // path, which the caller is about to print anyway.
                        root_unreadable = Some(match err.io_error() {
                            Some(io) => io.to_string(),
                            None => err.to_string(),
                        });
                    }
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

    // A directory below the root failing leaves a partial result worth
    // reporting. The root itself failing leaves nothing, and returning zero
    // files with a warning would be indistinguishable from an empty directory
    // to anything reading the exit status.
    if let Some(err) = root_unreadable {
        bail!("cannot list {}: {}", path.display(), err);
    }

    progress.files_listed(files.len() as u64);

    // Files that vanished or became unreadable between the walk and the stat.
    let unmeasurable = AtomicU64::new(0);

    // Ages are measured against a single instant captured before any file is
    // examined, so a long scan does not drift files between buckets.
    let now = SystemTime::now();

    // Each rayon thread aggregates into its own accumulator and these are
    // merged at the end, so no lock is taken on the per-file path. Run on the
    // pool built above rather than rayon's global one, so that --threads
    // governs the measuring as well as the walk.
    let aggregated = pool.install(|| {
        files
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

                acc.sizes
                    .entry(classify_size(size))
                    .or_default()
                    .add_file(size);

                if size <= options.small_at_or_below {
                    acc.small_files += 1;
                    acc.small_bytes += size;
                }

                if options.hotspots > 0 {
                    acc.record_directory(&entry.parent_path, size, options.small_at_or_below);
                }

                acc.consider(size, options.top, || entry.path());

                acc
            })
            .reduce(Partial::default, |mut acc, partial| {
                acc.merge(partial, options.top);
                acc
            })
    });

    // The last directory of the final chunk is still buffered.
    let mut aggregated = aggregated;
    aggregated.flush_pending();

    let mut largest: Vec<LargestFile> = aggregated
        .largest
        .into_iter()
        .map(|Reverse(file)| file)
        .collect();
    largest.sort_by(|a, b| b.cmp(a));

    // Most small files first. A directory holding none is not a hotspot, so it
    // is dropped rather than padding the list.
    let mut hotspots: Vec<Hotspot> = aggregated
        .directories
        .into_iter()
        .filter(|(_, stats)| stats.small_files > 0)
        .map(|(directory, stats)| Hotspot { directory, stats })
        .collect();
    hotspots.sort_by(|a, b| {
        b.stats
            .small_files
            .cmp(&a.stats.small_files)
            .then_with(|| a.directory.cmp(&b.directory))
    });
    hotspots.truncate(options.hotspots);

    Ok(Scan {
        totals: aggregated.extensions,
        ages: aggregated.ages,
        sizes: aggregated.sizes,
        hotspots,
        small_at_or_below: options.small_at_or_below,
        small_files: aggregated.small_files,
        small_bytes: aggregated.small_bytes,
        largest,
        structure: structure.finish(root_metadata.is_dir()),
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

        assert_eq!(
            a,
            Bucket {
                count: 3,
                bytes: 16
            }
        );
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
        scan.ages
            .insert(AgeBucket::Over2Years, Bucket { count: 1, bytes: 1 });
        scan.ages
            .insert(AgeBucket::UpTo30Days, Bucket { count: 2, bytes: 2 });

        let order: Vec<AgeBucket> = scan.age_rows().iter().map(|(age, _)| *age).collect();

        assert_eq!(order, [AgeBucket::UpTo30Days, AgeBucket::Over2Years]);
    }

    #[test]
    fn every_age_bucket_has_a_label() {
        for age in AgeBucket::ALL {
            assert!(!age.label().is_empty());
        }
    }

    #[test]
    fn sizes_land_in_the_expected_band() {
        assert_eq!(classify_size(0), SizeBand::Empty);
        assert_eq!(classify_size(1), SizeBand::Under4K);
        assert_eq!(classify_size(10_000), SizeBand::From4KTo64K);
        assert_eq!(classify_size(500_000), SizeBand::From64KTo1M);
        assert_eq!(classify_size(5 * 1024 * 1024), SizeBand::From1MTo16M);
        assert_eq!(classify_size(64 * 1024 * 1024), SizeBand::From16MTo128M);
        assert_eq!(classify_size(500 * 1024 * 1024), SizeBand::Over128M);
    }

    #[test]
    fn size_band_boundaries_do_not_overlap_or_leave_gaps() {
        const KB: u64 = 1024;
        const MB: u64 = 1024 * KB;

        // Each boundary belongs to the band above it.
        assert_eq!(classify_size(4 * KB - 1), SizeBand::Under4K);
        assert_eq!(classify_size(4 * KB), SizeBand::From4KTo64K);

        assert_eq!(classify_size(64 * KB - 1), SizeBand::From4KTo64K);
        assert_eq!(classify_size(64 * KB), SizeBand::From64KTo1M);

        assert_eq!(classify_size(MB - 1), SizeBand::From64KTo1M);
        assert_eq!(classify_size(MB), SizeBand::From1MTo16M);

        assert_eq!(classify_size(16 * MB - 1), SizeBand::From1MTo16M);
        assert_eq!(classify_size(16 * MB), SizeBand::From16MTo128M);

        assert_eq!(classify_size(128 * MB - 1), SizeBand::From16MTo128M);
        assert_eq!(classify_size(128 * MB), SizeBand::Over128M);
    }

    #[test]
    fn an_empty_file_is_its_own_band_rather_than_merely_small() {
        // Zero-byte files are pure per-file backup overhead with no payload,
        // so lumping them in with "under 4 KB" would hide them.
        assert_eq!(classify_size(0), SizeBand::Empty);
        assert_ne!(classify_size(1), SizeBand::Empty);
    }

    #[test]
    fn every_size_band_has_a_label() {
        for band in SizeBand::ALL {
            assert!(!band.label().is_empty());
        }
    }

    #[test]
    fn size_rows_come_back_smallest_first_and_skip_empty_bands() {
        let mut scan = Scan::default();
        scan.sizes
            .insert(SizeBand::Over128M, Bucket { count: 1, bytes: 1 });
        scan.sizes
            .insert(SizeBand::Under4K, Bucket { count: 2, bytes: 2 });

        let order: Vec<SizeBand> = scan.size_rows().iter().map(|(band, _)| *band).collect();

        assert_eq!(order, [SizeBand::Under4K, SizeBand::Over128M]);
    }

    #[test]
    fn a_directory_counts_files_at_the_threshold_as_small() {
        let mut stats = DirectoryStats::default();

        stats.add_file(64, 64);
        stats.add_file(65, 64);
        stats.add_file(1, 64);

        assert_eq!(stats.files, 3);
        assert_eq!(stats.bytes, 130);
        assert_eq!(stats.small_files, 2);
        assert_eq!(stats.small_bytes, 65);
    }

    #[test]
    fn directory_shares_and_averages_guard_the_empty_case() {
        let empty = DirectoryStats::default();

        assert_eq!(empty.small_share(), 0.0);
        assert_eq!(empty.average_bytes(), 0.0);
        assert!(empty.small_share().is_finite());
        assert!(empty.average_bytes().is_finite());
    }

    #[test]
    fn directory_small_share_is_a_proportion_of_its_own_files() {
        let mut stats = DirectoryStats::default();
        for _ in 0..3 {
            stats.add_file(10, 64);
        }
        stats.add_file(1000, 64);

        assert_eq!(stats.small_share(), 0.75);
    }

    #[test]
    fn scan_small_share_guards_the_empty_case() {
        let scan = Scan::default();

        assert_eq!(scan.small_share(), 0.0);
        assert!(scan.small_share().is_finite());
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

    fn child_dir(name: &str) -> Child {
        Child {
            is_dir: true,
            is_file: false,
            name_length: name.len(),
        }
    }

    fn child_file(name: &str) -> Child {
        Child {
            is_dir: false,
            is_file: true,
            name_length: name.len(),
        }
    }

    #[test]
    fn subdirectory_counts_land_in_the_expected_fan_out_band() {
        assert_eq!(classify_fan_out(0), FanOut::Leaf);
        assert_eq!(classify_fan_out(1), FanOut::One);
        assert_eq!(classify_fan_out(5), FanOut::From2To9);
        assert_eq!(classify_fan_out(50), FanOut::From10To99);
        assert_eq!(classify_fan_out(500), FanOut::From100To999);
        assert_eq!(classify_fan_out(5000), FanOut::Over1000);
    }

    #[test]
    fn fan_out_band_boundaries_do_not_overlap_or_leave_gaps() {
        assert_eq!(classify_fan_out(1), FanOut::One);
        assert_eq!(classify_fan_out(2), FanOut::From2To9);
        assert_eq!(classify_fan_out(9), FanOut::From2To9);
        assert_eq!(classify_fan_out(10), FanOut::From10To99);
        assert_eq!(classify_fan_out(99), FanOut::From10To99);
        assert_eq!(classify_fan_out(100), FanOut::From100To999);
        assert_eq!(classify_fan_out(999), FanOut::From100To999);
        assert_eq!(classify_fan_out(1000), FanOut::Over1000);
    }

    #[test]
    fn file_counts_land_in_the_expected_band() {
        assert_eq!(classify_file_count(0), FileCount::None);
        assert_eq!(classify_file_count(5), FileCount::From1To9);
        assert_eq!(classify_file_count(50), FileCount::From10To99);
        assert_eq!(classify_file_count(500), FileCount::From100To999);
        assert_eq!(classify_file_count(5000), FileCount::From1000To9999);
        assert_eq!(classify_file_count(50_000), FileCount::Over10000);
    }

    #[test]
    fn file_count_band_boundaries_do_not_overlap_or_leave_gaps() {
        assert_eq!(classify_file_count(9), FileCount::From1To9);
        assert_eq!(classify_file_count(10), FileCount::From10To99);
        assert_eq!(classify_file_count(99), FileCount::From10To99);
        assert_eq!(classify_file_count(100), FileCount::From100To999);
        assert_eq!(classify_file_count(999), FileCount::From100To999);
        assert_eq!(classify_file_count(1000), FileCount::From1000To9999);
        assert_eq!(classify_file_count(9999), FileCount::From1000To9999);
        assert_eq!(classify_file_count(10_000), FileCount::Over10000);
    }

    #[test]
    fn a_directory_holding_no_subdirectories_is_a_leaf_not_an_empty_band() {
        // "Holds nothing below it" is the single most common shape in a tree
        // and the one the report is most often read for, so it gets its own
        // name rather than being an unlabelled zero.
        assert_eq!(classify_fan_out(0), FanOut::Leaf);
        assert_eq!(FanOut::Leaf.label(), "none (leaf)");
    }

    #[test]
    fn every_structure_band_has_a_label_and_a_distinct_slot() {
        for band in FanOut::ALL {
            assert!(!band.label().is_empty());
            assert_eq!(FanOut::ALL[band.index()], band);
        }
        for band in FileCount::ALL {
            assert!(!band.label().is_empty());
            assert_eq!(FileCount::ALL[band.index()], band);
        }
    }

    #[test]
    fn a_directorys_children_are_counted_one_level_below_it() {
        let counters = StructureCounters::new();
        counters.record(
            0,
            0,
            [child_dir("sub"), child_file("a.txt"), child_file("b.txt")],
        );

        let structure = counters.finish(true);

        // Level 0 is the root itself; its children sit at level 1.
        assert_eq!(
            structure.level_rows(),
            vec![
                (
                    0,
                    DirectoryGroup {
                        directories: 1,
                        files: 0
                    }
                ),
                (
                    1,
                    DirectoryGroup {
                        directories: 1,
                        files: 2
                    }
                ),
            ]
        );
        assert_eq!(structure.deepest, 1);
    }

    #[test]
    fn the_root_is_counted_as_a_directory_because_no_parent_lists_it() {
        let counters = StructureCounters::new();
        counters.record(0, 0, [child_dir("one"), child_dir("two")]);

        let structure = counters.finish(true);

        assert_eq!(structure.directories, 3);
        assert_eq!(structure.listed_directories, 1);
    }

    #[test]
    fn scanning_a_single_file_describes_no_structure_at_all() {
        let counters = StructureCounters::new();

        let structure = counters.finish(false);

        assert_eq!(structure.directories, 0);
        assert!(structure.level_rows().is_empty());
    }

    #[test]
    fn directories_that_were_never_listed_are_reported_as_missing() {
        // A directory stopped by the depth limit or by a permission failure is
        // known to exist but has no known contents, so it is absent from the
        // band tables and the shortfall has to be visible.
        let counters = StructureCounters::new();
        counters.record(0, 0, [child_dir("listed"), child_dir("blocked")]);
        counters.record(1, 6, [child_file("a.txt")]);

        let structure = counters.finish(true);

        assert_eq!(structure.directories, 3);
        assert_eq!(structure.listed_directories, 2);
        assert_eq!(structure.unlisted_directories(), 1);
    }

    #[test]
    fn entries_that_are_neither_a_file_nor_a_directory_are_counted_as_neither() {
        // Symlinks are not followed, so counting one would either invent a
        // directory or double-count the file it points at.
        let counters = StructureCounters::new();
        counters.record(
            0,
            0,
            [
                child_file("real.txt"),
                Child {
                    is_dir: false,
                    is_file: false,
                    name_length: 8,
                },
            ],
        );

        let structure = counters.finish(true);

        assert_eq!(structure.files(), 1);
        assert_eq!(structure.directories, 1);
        // And they do not set the longest path either, or the longest path
        // would name something absent from every other figure in the report.
        assert_eq!(structure.longest_path, "real.txt".len());
    }

    #[test]
    fn path_lengths_are_measured_below_the_root_not_from_it() {
        // The prefix a tree currently sits under says nothing about the tree,
        // and changes the moment it is copied somewhere else.
        let counters = StructureCounters::new();
        // "sub" is three characters below the root, so "sub/name.txt" is 12.
        counters.record(1, 3, [child_file("name.txt")]);

        assert_eq!(counters.finish(true).longest_path, 12);
    }

    #[test]
    fn a_child_of_the_root_carries_no_leading_separator() {
        let counters = StructureCounters::new();
        counters.record(0, 0, [child_file("name.txt")]);

        assert_eq!(counters.finish(true).longest_path, 8);
    }

    #[test]
    fn paths_past_the_limit_are_counted_by_kind() {
        let counters = StructureCounters::new();
        let deep = LONG_PATH_LIMIT - 4;
        counters.record(
            1,
            deep,
            [
                child_file("just-fits"),
                child_dir("also-over"),
                child_file("x"),
            ],
        );

        let structure = counters.finish(true);

        // deep + 1 separator + name: only the one-character name stays inside.
        assert_eq!(structure.long_paths.files, 1);
        assert_eq!(structure.long_paths.directories, 1);
    }

    #[test]
    fn a_path_exactly_at_the_limit_is_not_over_it() {
        let counters = StructureCounters::new();
        counters.record(1, LONG_PATH_LIMIT - 2, [child_file("x")]);

        let structure = counters.finish(true);

        assert_eq!(structure.longest_path, LONG_PATH_LIMIT);
        assert_eq!(structure.long_paths.files, 0);
    }

    #[test]
    fn a_tree_deeper_than_the_tracked_levels_still_reports_its_depth() {
        // The per-level counters are a fixed array so the walk threads never
        // lock. Anything past it shares one row rather than being dropped.
        let counters = StructureCounters::new();
        counters.record(MAX_TRACKED_DEPTH + 5, 0, [child_file("buried.txt")]);

        let structure = counters.finish(true);

        assert_eq!(structure.deepest, MAX_TRACKED_DEPTH + 6);
        assert_eq!(structure.beyond_tracked.files, 1);
        assert!(structure
            .level_rows()
            .iter()
            .all(|(depth, _)| *depth <= MAX_TRACKED_DEPTH));
    }

    #[test]
    fn structure_rows_come_back_in_display_order_and_skip_empty_bands() {
        let counters = StructureCounters::new();
        counters.record(0, 0, []);
        counters.record(1, 1, [child_file("a"), child_file("b")]);

        let structure = counters.finish(true);

        let fan_out: Vec<FanOut> = structure
            .fan_out_rows()
            .iter()
            .map(|(band, _)| *band)
            .collect();
        let files: Vec<FileCount> = structure
            .file_count_rows()
            .iter()
            .map(|(band, _)| *band)
            .collect();

        assert_eq!(fan_out, [FanOut::Leaf]);
        assert_eq!(files, [FileCount::None, FileCount::From1To9]);
    }

    #[test]
    fn the_mean_files_per_directory_guards_the_empty_case() {
        let structure = Structure::default();

        assert_eq!(structure.mean_files_per_directory(), 0.0);
        assert!(structure.mean_files_per_directory().is_finite());
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
        assert_eq!(
            insert(["a", "b", "c"]),
            vec![file("a", 100), file("b", 100)]
        );
    }
}
