use anyhow::Result;
use comfy_table::modifiers::{UTF8_ROUND_CORNERS, UTF8_SOLID_INNER_BORDERS};
use comfy_table::presets::UTF8_FULL;
use comfy_table::*;
use fast_walk::diff::{self, Diff};
use fast_walk::{scan, Progress, Scan, ScanOptions};
use std::sync::OnceLock;
use std::time::Instant;
use randomizer::Randomizer;
use indicatif::ProgressBar;


use colored::*;
use std::path::{Path, PathBuf};

use clap::Parser;

const BYTES_PER_MB: f64 = (1024 * 1024) as f64;

/// Number of extensions shown in the terminal table. The CSV always contains
/// every extension.
const TABLE_ROWS: usize = 11;

/// Human-readable size, picking a unit that suits the value.
///
/// Average file sizes are usually in the kilobyte range, so reporting them in
/// megabytes like the capacity column would round almost every row to 0.00.
fn format_size(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Signed size, for diff output. No change carries no sign.
fn format_delta_size(bytes: i64) -> String {
    let sign = match bytes {
        d if d > 0 => "+",
        d if d < 0 => "-",
        _ => "",
    };
    format!("{}{}", sign, format_size(bytes.unsigned_abs() as f64))
}

/// Signed count, for diff output. No change carries no sign.
fn format_delta_count(delta: i64) -> String {
    if delta == 0 {
        "0".to_string()
    } else {
        format!("{:+}", delta)
    }
}

/// Colour a delta by direction, leaving zero unstyled.
fn colour_delta(text: String, delta: i64) -> ColoredString {
    match delta {
        d if d > 0 => text.green(),
        d if d < 0 => text.red(),
        _ => text.normal(),
    }
}

/// Parse a size written as a plain byte count or with a K, M or G suffix, so
/// thresholds can be given as `64K` rather than `65536`.
fn parse_size(input: &str) -> Result<u64, String> {
    const KB: u64 = 1024;

    let text = input.trim();
    if text.is_empty() {
        return Err("expected a size such as 64K, 1MB or 4096".to_string());
    }

    let upper = text.to_ascii_uppercase();
    let (digits, multiplier) = if let Some(rest) = strip_unit(&upper, 'K') {
        (rest, KB)
    } else if let Some(rest) = strip_unit(&upper, 'M') {
        (rest, KB * KB)
    } else if let Some(rest) = strip_unit(&upper, 'G') {
        (rest, KB * KB * KB)
    } else if let Some(rest) = upper.strip_suffix('B') {
        (rest, 1)
    } else {
        (upper.as_str(), 1)
    };

    let value: u64 = digits.trim().parse().map_err(|_| {
        format!("{input:?} is not a size; give a number optionally followed by K, M or G")
    })?;

    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{input:?} is too large to be a size in bytes"))
}

/// Strip a unit letter, accepting both `64K` and `64KB`.
fn strip_unit(text: &str, unit: char) -> Option<&str> {
    text.strip_suffix(&format!("{unit}B"))
        .or_else(|| text.strip_suffix(unit))
}

/// Share of a total, as a percentage string. Guards the empty-scan case.
fn format_share(part: u64, whole: u64) -> String {
    if whole == 0 {
        "0.0%".to_string()
    } else {
        format!("{:.1}%", (part as f64 / whole as f64) * 100.0)
    }
}

#[derive(Parser)]
#[command(version, about = "Scan a filesystem and total up capacity by extension")]
struct Cli {
    /// Directory to scan.
    #[clap(
        short,
        long,
        value_parser,
        required_unless_present = "diff",
        conflicts_with = "diff"
    )]
    path: Option<PathBuf>,

    #[clap(short, long, default_value_t = usize::MAX, value_parser)]
    max_depth: usize,

    #[clap(short, long, default_value_t = num_cpus::get(), value_parser)]
    threads: usize,

    /// Skip hidden files and directories. Off by default, so dot-directories
    /// such as .git are included in the totals.
    #[clap(long)]
    skip_hidden: bool,

    /// How many of the largest files to report. Zero turns the report off.
    #[clap(long, default_value_t = 10, value_parser)]
    top: usize,

    /// How many small-file directories to report. Zero turns the report off.
    #[clap(long, default_value_t = 10, value_parser)]
    hotspots: usize,

    /// Files this size or smaller count as small, for the small-file report.
    /// Accepts a byte count or a K, M or G suffix.
    #[clap(long, default_value = "64K", value_parser = parse_size, value_name = "SIZE")]
    small_under: u64,

    /// Compare two results CSVs from earlier scans instead of scanning.
    #[clap(long, num_args = 2, value_names = ["OLD", "NEW"], value_parser)]
    diff: Option<Vec<PathBuf>>,

}

/// Drives an `indicatif` bar. The bar cannot be built until the walk reports
/// how many files it found, so it is filled in on the first callback.
#[derive(Default)]
struct BarProgress {
    bar: OnceLock<ProgressBar>,
}

impl Progress for BarProgress {
    fn files_listed(&self, total: u64) {
        let _ = self.bar.set(ProgressBar::new(total));
    }

    fn file_measured(&self) {
        if let Some(bar) = self.bar.get() {
            bar.inc(1);
        }
    }
}

impl BarProgress {
    fn finish(&self) {
        if let Some(bar) = self.bar.get() {
            bar.finish_and_clear();
        }
    }
}

fn new_table(headers: Vec<&str>) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_header(headers);
    table
}

/// Note what a table left out, so a truncated view never reads as the whole
/// picture.
fn report_truncation(shown: usize, total: usize, destination: &str) {
    if total > shown {
        println!(
            "Showing {} of {} rows; all {} are in {}",
            shown, total, total, destination
        );
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(files) = &cli.diff {
        return run_diff(&files[0], &files[1]);
    }

    let path = cli
        .path
        .clone()
        .expect("clap requires --path whenever --diff is absent");

    run_scan(&cli, &path)
}

fn run_scan(cli: &Cli, path: &Path) -> Result<()> {
    let start = Instant::now();
    let num = num_cpus::get();

    let use_threads: usize;

    if cli.threads > num {
        use_threads = num;
        println!("Using available threads : {}", num.to_string().red());
    } else {
        use_threads = cli.threads;
        println!("Using {} threads", use_threads);
    }

    let options = ScanOptions {
        max_depth: cli.max_depth,
        threads: use_threads,
        skip_hidden: cli.skip_hidden,
        top: cli.top,
        hotspots: cli.hotspots,
        small_at_or_below: cli.small_under,
    };

    let progress = BarProgress::default();
    let result = scan(path, &options, &progress)?;
    progress.finish();

    for err in &result.walk_errors {
        eprintln!("{} {}", "warning:".yellow(), err);
    }

    let stem = format!("results-{}", Randomizer::ALPHANUMERIC(6).string().unwrap());
    let written = write_scan_csvs(&result, &stem)?;

    let total_files = result.total_files();
    let total_cap = result.total_bytes();

    let files_hour = (total_files as f32 / start.elapsed().as_secs_f32()) * 3600.00;

    println!("\nThat took: {:?}", start.elapsed());
    println!("Estimated files per-hour: {} {}", files_hour, emoji::travel_and_places::sky_and_weather::FIRE.glyph);

    println!(
        "\nTotal Files: {}, Total Cap: {} MB, Average File: {}",
        total_files.to_string().green(),
        format!("{:.2}", total_cap as f64 / BYTES_PER_MB).bright_purple(),
        format_size(result.average_bytes()).cyan()
    );

    if result.walk_error_count > 0 {
        println!(
            "{} {} entries could not be walked",
            "warning:".yellow(),
            result.walk_error_count
        );
    }

    if result.unmeasurable_files > 0 {
        println!(
            "{} {} files could not be measured and were left out of the totals",
            "warning:".yellow(),
            result.unmeasurable_files
        );
    }

    print_extension_table(&result, &written.extensions);
    print_age_table(&result);
    print_size_table(&result);
    print_hotspot_table(&result);
    print_largest_table(&result);

    for file in written.all() {
        println!("Results written to {}", file);
    }

    Ok(())
}

/// Paths of the CSVs a scan produced.
struct WrittenFiles {
    extensions: String,
    ages: String,
    sizes: String,
    hotspots: Option<String>,
    largest: Option<String>,
}

impl WrittenFiles {
    fn all(&self) -> Vec<&str> {
        let mut files = vec![
            self.extensions.as_str(),
            self.ages.as_str(),
            self.sizes.as_str(),
        ];
        files.extend(self.hotspots.as_deref());
        files.extend(self.largest.as_deref());
        files
    }
}

fn write_scan_csvs(result: &Scan, stem: &str) -> Result<WrittenFiles> {
    let extensions = format!("{}.csv", stem);
    let mut wtr = csv::Writer::from_path(&extensions)?;
    wtr.write_record(["Extension", "Qty", "Cap Bytes", "Avg Bytes"])?;
    for (extension, bucket) in result.rows() {
        wtr.write_record(&[
            extension.to_string(),
            bucket.count.to_string(),
            bucket.bytes.to_string(),
            format!("{:.0}", bucket.average_bytes()),
        ])?;
    }
    wtr.flush()?;

    let total_cap = result.total_bytes();
    let ages = format!("{}-age.csv", stem);
    let mut wtr = csv::Writer::from_path(&ages)?;
    wtr.write_record(["Age", "Qty", "Cap Bytes", "Avg Bytes", "Pct Of Cap"])?;
    for (age, bucket) in result.age_rows() {
        wtr.write_record(&[
            age.label().to_string(),
            bucket.count.to_string(),
            bucket.bytes.to_string(),
            format!("{:.0}", bucket.average_bytes()),
            format_share(bucket.bytes, total_cap),
        ])?;
    }
    wtr.flush()?;

    let total_files = result.total_files();
    let sizes = format!("{}-size.csv", stem);
    let mut wtr = csv::Writer::from_path(&sizes)?;
    wtr.write_record(["Size", "Qty", "Pct Of Files", "Cap Bytes", "Pct Of Cap"])?;
    for (band, bucket) in result.size_rows() {
        wtr.write_record(&[
            band.label().to_string(),
            bucket.count.to_string(),
            format_share(bucket.count, total_files),
            bucket.bytes.to_string(),
            format_share(bucket.bytes, total_cap),
        ])?;
    }
    wtr.flush()?;

    let hotspots = if result.hotspots.is_empty() {
        None
    } else {
        let path = format!("{}-hotspots.csv", stem);
        let mut wtr = csv::Writer::from_path(&path)?;
        wtr.write_record([
            "Directory",
            "Files",
            "Small Files",
            "Pct Small",
            "Cap Bytes",
            "Small Cap Bytes",
            "Avg Bytes",
        ])?;
        for hotspot in &result.hotspots {
            wtr.write_record(&[
                hotspot.directory.display().to_string(),
                hotspot.stats.files.to_string(),
                hotspot.stats.small_files.to_string(),
                format_share(hotspot.stats.small_files, hotspot.stats.files),
                hotspot.stats.bytes.to_string(),
                hotspot.stats.small_bytes.to_string(),
                format!("{:.0}", hotspot.stats.average_bytes()),
            ])?;
        }
        wtr.flush()?;
        Some(path)
    };

    let largest = if result.largest.is_empty() {
        None
    } else {
        let path = format!("{}-largest.csv", stem);
        let mut wtr = csv::Writer::from_path(&path)?;
        wtr.write_record(["Bytes", "Path"])?;
        for file in &result.largest {
            wtr.write_record(&[file.bytes.to_string(), file.path.display().to_string()])?;
        }
        wtr.flush()?;
        Some(path)
    };

    Ok(WrittenFiles {
        extensions,
        ages,
        sizes,
        hotspots,
        largest,
    })
}

fn print_extension_table(result: &Scan, csv_path: &str) {
    let rows = result.rows();
    let mut table = new_table(vec!["Extension", "Quantity", "Capacity MB", "Avg Size"]);

    for (extension, bucket) in rows.iter().take(TABLE_ROWS) {
        table.add_row(vec![
            extension.to_string(),
            bucket.count.to_string(),
            format!("{:.2}", bucket.bytes as f64 / BYTES_PER_MB),
            format_size(bucket.average_bytes()),
        ]);
    }

    println!("{table}");
    report_truncation(rows.len().min(TABLE_ROWS), rows.len(), csv_path);
}

fn print_age_table(result: &Scan) {
    let rows = result.age_rows();
    if rows.is_empty() {
        return;
    }

    let total_cap = result.total_bytes();
    let mut table = new_table(vec!["Age", "Quantity", "Capacity MB", "Avg Size", "% of Cap"]);

    for (age, bucket) in &rows {
        table.add_row(vec![
            age.label().to_string(),
            bucket.count.to_string(),
            format!("{:.2}", bucket.bytes as f64 / BYTES_PER_MB),
            format_size(bucket.average_bytes()),
            format_share(bucket.bytes, total_cap),
        ]);
    }

    println!("\nBy age (last modified)");
    println!("{table}");
}

fn print_size_table(result: &Scan) {
    let rows = result.size_rows();
    if rows.is_empty() {
        return;
    }

    let total_cap = result.total_bytes();
    let total_files = result.total_files();
    let mut table = new_table(vec![
        "Size",
        "Quantity",
        "% of Files",
        "Capacity MB",
        "% of Cap",
    ]);

    for (band, bucket) in &rows {
        table.add_row(vec![
            band.label().to_string(),
            bucket.count.to_string(),
            format_share(bucket.count, total_files),
            format!("{:.2}", bucket.bytes as f64 / BYTES_PER_MB),
            format_share(bucket.bytes, total_cap),
        ]);
    }

    println!("\nBy file size");
    println!("{table}");
    println!(
        "{} files ({} of the total) are {} or smaller, holding {} between them",
        result.small_files,
        format_share(result.small_files, result.total_files()),
        format_size(result.small_at_or_below as f64),
        format_size(result.small_bytes as f64)
    );
}

fn print_hotspot_table(result: &Scan) {
    if result.hotspots.is_empty() {
        return;
    }

    let mut table = new_table(vec![
        "Directory",
        "Files",
        "Small",
        "% Small",
        "Capacity MB",
        "Avg Size",
    ]);

    for hotspot in &result.hotspots {
        table.add_row(vec![
            hotspot.directory.display().to_string(),
            hotspot.stats.files.to_string(),
            hotspot.stats.small_files.to_string(),
            format_share(hotspot.stats.small_files, hotspot.stats.files),
            format!("{:.2}", hotspot.stats.bytes as f64 / BYTES_PER_MB),
            format_size(hotspot.stats.average_bytes()),
        ]);
    }

    println!(
        "\nDirectories holding the most files of {} or smaller",
        format_size(result.small_at_or_below as f64)
    );
    println!("{table}");
}

fn print_largest_table(result: &Scan) {
    if result.largest.is_empty() {
        return;
    }

    let mut table = new_table(vec!["Size", "Path"]);

    for file in &result.largest {
        table.add_row(vec![
            format_size(file.bytes as f64),
            file.path.display().to_string(),
        ]);
    }

    println!("\nLargest files");
    println!("{table}");
}

fn run_diff(before_path: &Path, after_path: &Path) -> Result<()> {
    let before = diff::read_scan_csv(before_path)?;
    let after = diff::read_scan_csv(after_path)?;

    let result = diff::compare(&before, &after);

    println!(
        "Comparing {} -> {}",
        before_path.display(),
        after_path.display()
    );

    let file_name = format!("diff-{}.csv", Randomizer::ALPHANUMERIC(6).string().unwrap());
    write_diff_csv(&result, &file_name)?;

    println!(
        "\nFiles: {} -> {} ({})",
        result.before_total.count,
        result.after_total.count,
        colour_delta(format_delta_count(result.count_delta()), result.count_delta())
    );
    println!(
        "Capacity: {} -> {} ({})",
        format_size(result.before_total.bytes as f64),
        format_size(result.after_total.bytes as f64),
        colour_delta(
            format_delta_size(result.bytes_delta()),
            result.bytes_delta()
        )
    );

    if result.rows.is_empty() {
        println!("\nNo extensions changed.");
    } else {
        let mut table = new_table(vec![
            "Extension",
            "Δ Files",
            "Δ Capacity",
            "Cap Before",
            "Cap After",
        ]);

        for row in result.rows.iter().take(TABLE_ROWS) {
            let marker = if row.is_new() {
                " (new)"
            } else if row.is_gone() {
                " (gone)"
            } else {
                ""
            };

            table.add_row(vec![
                format!("{}{}", row.extension, marker),
                colour_delta(format_delta_count(row.count_delta()), row.count_delta()).to_string(),
                colour_delta(format_delta_size(row.bytes_delta()), row.bytes_delta()).to_string(),
                format_size(row.before.bytes as f64),
                format_size(row.after.bytes as f64),
            ]);
        }

        println!("\nChanged extensions, biggest movers first");
        println!("{table}");
        report_truncation(
            result.rows.len().min(TABLE_ROWS),
            result.rows.len(),
            &file_name,
        );
    }

    println!("Results written to {}", file_name);

    Ok(())
}

fn write_diff_csv(result: &Diff, path: &str) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path)?;
    wtr.write_record([
        "Extension",
        "Qty Before",
        "Qty After",
        "Qty Delta",
        "Cap Bytes Before",
        "Cap Bytes After",
        "Cap Bytes Delta",
    ])?;

    for row in &result.rows {
        wtr.write_record(&[
            row.extension.clone(),
            row.before.count.to_string(),
            row.after.count.to_string(),
            row.count_delta().to_string(),
            row.before.bytes.to_string(),
            row.after.bytes.to_string(),
            row.bytes_delta().to_string(),
        ])?;
    }
    wtr.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_below_a_kilobyte_are_shown_as_whole_bytes() {
        assert_eq!(format_size(0.0), "0 B");
        assert_eq!(format_size(1.0), "1 B");
        assert_eq!(format_size(512.4), "512 B");
        assert_eq!(format_size(1023.0), "1023 B");
    }

    #[test]
    fn sizes_step_up_a_unit_every_1024() {
        assert_eq!(format_size(1024.0), "1.0 KB");
        assert_eq!(format_size(1536.0), "1.5 KB");
        assert_eq!(format_size(1024.0 * 1024.0), "1.0 MB");
        assert_eq!(format_size(1024.0 * 1024.0 * 1024.0), "1.0 GB");
    }

    #[test]
    fn very_large_sizes_stop_at_the_largest_unit() {
        let petabyte = 1024.0_f64.powi(5);

        assert_eq!(format_size(petabyte), "1024.0 TB");
    }

    #[test]
    fn a_kilobyte_scale_average_does_not_round_away_to_nothing() {
        // Reporting the average in MB like the capacity column would show
        // this as 0.00, which is why it gets its own unit.
        assert_eq!(format_size(4096.0), "4.0 KB");
    }

    #[test]
    fn deltas_carry_their_sign() {
        assert_eq!(format_delta_size(0), "0 B");
        assert_eq!(format_delta_count(0), "0");
        assert_eq!(format_delta_count(3), "+3");
        assert_eq!(format_delta_count(-3), "-3");
        assert_eq!(format_delta_size(2048), "+2.0 KB");
        assert_eq!(format_delta_size(-2048), "-2.0 KB");
    }

    #[test]
    fn the_most_negative_delta_does_not_overflow_when_negated() {
        // `-i64::MIN` would panic; unsigned_abs is why it does not. 2^63
        // bytes is 2^23 TB.
        assert_eq!(format_delta_size(i64::MIN), "-8388608.0 TB");
    }

    #[test]
    fn shares_are_percentages_of_the_whole() {
        assert_eq!(format_share(1, 4), "25.0%");
        assert_eq!(format_share(0, 100), "0.0%");
        assert_eq!(format_share(100, 100), "100.0%");
    }

    #[test]
    fn a_share_of_an_empty_scan_is_zero_not_a_division_by_zero() {
        assert_eq!(format_share(0, 0), "0.0%");
    }

    #[test]
    fn sizes_parse_as_plain_byte_counts() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("512B"), Ok(512));
    }

    #[test]
    fn size_suffixes_are_accepted_with_or_without_the_b() {
        assert_eq!(parse_size("64K"), Ok(64 * 1024));
        assert_eq!(parse_size("64KB"), Ok(64 * 1024));
        assert_eq!(parse_size("1M"), Ok(1024 * 1024));
        assert_eq!(parse_size("1MB"), Ok(1024 * 1024));
        assert_eq!(parse_size("2G"), Ok(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn size_suffixes_are_case_insensitive_and_tolerate_spacing() {
        assert_eq!(parse_size("64k"), Ok(64 * 1024));
        assert_eq!(parse_size("  64 kb  "), Ok(64 * 1024));
    }

    #[test]
    fn a_size_that_is_not_a_number_is_rejected_with_guidance() {
        let err = parse_size("big").unwrap_err();

        assert!(err.contains("K, M or G"), "unhelpful error: {err}");
    }

    #[test]
    fn an_empty_size_is_rejected() {
        assert!(parse_size("").is_err());
        assert!(parse_size("   ").is_err());
    }

    #[test]
    fn a_size_that_would_overflow_is_rejected_rather_than_wrapping() {
        let err = parse_size("99999999999999999999G").unwrap_err();
        assert!(!err.is_empty());

        let err = parse_size(&format!("{}G", u64::MAX)).unwrap_err();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }
}
