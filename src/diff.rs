//! Comparing two scans.
//!
//! Takes the CSVs written by earlier runs rather than rescanning, so two
//! snapshots taken days apart can be compared long after the fact.

use crate::Bucket;
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Columns a scan CSV must carry to be diffable. The average column is derived
/// from these two, so it is not required.
const EXTENSION_COLUMN: &str = "Extension";
const COUNT_COLUMN: &str = "Qty";
const BYTES_COLUMN: &str = "Cap Bytes";

/// How one extension changed between two scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub extension: String,
    pub before: Bucket,
    pub after: Bucket,
}

impl DiffRow {
    pub fn count_delta(&self) -> i64 {
        self.after.count as i64 - self.before.count as i64
    }

    pub fn bytes_delta(&self) -> i64 {
        self.after.bytes as i64 - self.before.bytes as i64
    }

    /// True when the extension is absent from the earlier scan.
    pub fn is_new(&self) -> bool {
        self.before.count == 0 && self.after.count > 0
    }

    /// True when the extension is absent from the later scan.
    pub fn is_gone(&self) -> bool {
        self.after.count == 0 && self.before.count > 0
    }
}

/// The result of comparing two scans.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Only extensions that changed, ordered by the size of the capacity
    /// change so the biggest movers come first.
    pub rows: Vec<DiffRow>,
    pub before_total: Bucket,
    pub after_total: Bucket,
}

impl Diff {
    pub fn count_delta(&self) -> i64 {
        self.after_total.count as i64 - self.before_total.count as i64
    }

    pub fn bytes_delta(&self) -> i64 {
        self.after_total.bytes as i64 - self.before_total.bytes as i64
    }
}

/// Compare two sets of per-extension totals.
///
/// Unchanged extensions are left out: the point of a diff is what moved.
pub fn compare(before: &HashMap<String, Bucket>, after: &HashMap<String, Bucket>) -> Diff {
    let extensions: HashSet<&String> = before.keys().chain(after.keys()).collect();

    let mut rows: Vec<DiffRow> = extensions
        .into_iter()
        .map(|extension| DiffRow {
            extension: extension.clone(),
            before: before.get(extension).copied().unwrap_or_default(),
            after: after.get(extension).copied().unwrap_or_default(),
        })
        .filter(|row| row.before != row.after)
        .collect();

    // Biggest capacity movement first, in either direction; ties by name so
    // that repeated comparisons produce identical output.
    rows.sort_by(|a, b| {
        b.bytes_delta()
            .abs()
            .cmp(&a.bytes_delta().abs())
            .then_with(|| a.extension.cmp(&b.extension))
    });

    Diff {
        rows,
        before_total: total(before),
        after_total: total(after),
    }
}

fn total(totals: &HashMap<String, Bucket>) -> Bucket {
    let mut sum = Bucket::default();
    for bucket in totals.values() {
        sum.count += bucket.count;
        sum.bytes += bucket.bytes;
    }
    sum
}

/// Read the per-extension totals out of a CSV written by a previous scan.
///
/// Columns are located by header name rather than position, so a file that
/// gained columns in a later version still reads.
pub fn read_scan_csv(path: &Path) -> Result<HashMap<String, Bucket>> {
    let mut reader = csv::Reader::from_path(path)
        .with_context(|| format!("cannot read scan file: {}", path.display()))?;

    let headers = reader
        .headers()
        .with_context(|| format!("{} has no header row", path.display()))?
        .clone();

    let extension_at = column_index(&headers, EXTENSION_COLUMN, path)?;
    let count_at = column_index(&headers, COUNT_COLUMN, path)?;
    let bytes_at = column_index(&headers, BYTES_COLUMN, path)?;

    let mut totals: HashMap<String, Bucket> = HashMap::new();

    for (index, record) in reader.records().enumerate() {
        // Header occupies line 1, so the first record is line 2.
        let line = index + 2;
        let record =
            record.with_context(|| format!("{} line {}: malformed row", path.display(), line))?;

        let extension = record
            .get(extension_at)
            .ok_or_else(|| {
                anyhow!(
                    "{} line {}: missing the {} column",
                    path.display(),
                    line,
                    EXTENSION_COLUMN
                )
            })?
            .to_string();

        let count = parse_number(&record, count_at, COUNT_COLUMN, path, line)?;
        let bytes = parse_number(&record, bytes_at, BYTES_COLUMN, path, line)?;

        // A well-formed scan lists each extension once; summing rather than
        // overwriting keeps a hand-edited file from silently losing data.
        let bucket = totals.entry(extension).or_default();
        bucket.count += count;
        bucket.bytes += bytes;
    }

    Ok(totals)
}

fn column_index(headers: &csv::StringRecord, name: &str, path: &Path) -> Result<usize> {
    headers
        .iter()
        .position(|header| header.trim() == name)
        .ok_or_else(|| {
            anyhow!(
                "{} is not a fast-walk results file: no {:?} column (found: {})",
                path.display(),
                name,
                headers.iter().collect::<Vec<_>>().join(", ")
            )
        })
}

fn parse_number(
    record: &csv::StringRecord,
    at: usize,
    name: &str,
    path: &Path,
    line: usize,
) -> Result<u64> {
    let raw = record.get(at).unwrap_or_default().trim();

    raw.parse::<u64>().map_err(|_| {
        anyhow!(
            "{} line {}: {} is {:?}, which is not a whole number",
            path.display(),
            line,
            name,
            raw
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(entries: &[(&str, u64, u64)]) -> HashMap<String, Bucket> {
        entries
            .iter()
            .map(|(extension, count, bytes)| {
                (
                    (*extension).to_string(),
                    Bucket {
                        count: *count,
                        bytes: *bytes,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn reports_growth_and_shrinkage() {
        let diff = compare(
            &totals(&[("txt", 10, 1000)]),
            &totals(&[("txt", 12, 1500)]),
        );

        assert_eq!(diff.rows.len(), 1);
        assert_eq!(diff.rows[0].count_delta(), 2);
        assert_eq!(diff.rows[0].bytes_delta(), 500);

        let diff = compare(
            &totals(&[("txt", 10, 1000)]),
            &totals(&[("txt", 4, 250)]),
        );

        assert_eq!(diff.rows[0].count_delta(), -6);
        assert_eq!(diff.rows[0].bytes_delta(), -750);
    }

    #[test]
    fn unchanged_extensions_are_left_out() {
        let diff = compare(
            &totals(&[("txt", 10, 1000), ("rs", 5, 500)]),
            &totals(&[("txt", 10, 1000), ("rs", 6, 700)]),
        );

        let listed: Vec<&str> = diff.rows.iter().map(|row| row.extension.as_str()).collect();
        assert_eq!(listed, ["rs"]);
    }

    #[test]
    fn an_extension_only_in_the_later_scan_is_new() {
        let diff = compare(&totals(&[]), &totals(&[("mp4", 3, 900)]));

        assert!(diff.rows[0].is_new());
        assert!(!diff.rows[0].is_gone());
        assert_eq!(diff.rows[0].bytes_delta(), 900);
    }

    #[test]
    fn an_extension_only_in_the_earlier_scan_is_gone() {
        let diff = compare(&totals(&[("mp4", 3, 900)]), &totals(&[]));

        assert!(diff.rows[0].is_gone());
        assert!(!diff.rows[0].is_new());
        assert_eq!(diff.rows[0].bytes_delta(), -900);
    }

    #[test]
    fn rows_are_ordered_by_size_of_change_regardless_of_direction() {
        let diff = compare(
            &totals(&[("small", 1, 10), ("shrank", 1, 5000), ("grew", 1, 100)]),
            &totals(&[("small", 1, 30), ("shrank", 1, 1000), ("grew", 1, 900)]),
        );

        let listed: Vec<&str> = diff.rows.iter().map(|row| row.extension.as_str()).collect();
        // -4000, then +800, then +20.
        assert_eq!(listed, ["shrank", "grew", "small"]);
    }

    #[test]
    fn totals_cover_unchanged_extensions_too() {
        let diff = compare(
            &totals(&[("txt", 10, 1000), ("rs", 5, 500)]),
            &totals(&[("txt", 10, 1000), ("rs", 6, 700)]),
        );

        assert_eq!(diff.before_total, Bucket { count: 15, bytes: 1500 });
        assert_eq!(diff.after_total, Bucket { count: 16, bytes: 1700 });
        assert_eq!(diff.count_delta(), 1);
        assert_eq!(diff.bytes_delta(), 200);
    }

    #[test]
    fn comparing_a_scan_with_itself_reports_nothing() {
        let scan = totals(&[("txt", 10, 1000), ("rs", 5, 500)]);

        let diff = compare(&scan, &scan);

        assert!(diff.rows.is_empty());
        assert_eq!(diff.bytes_delta(), 0);
        assert_eq!(diff.count_delta(), 0);
    }
}
