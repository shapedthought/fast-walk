use anyhow::Result;
use comfy_table::modifiers::{UTF8_ROUND_CORNERS, UTF8_SOLID_INNER_BORDERS};
use comfy_table::presets::UTF8_FULL;
use comfy_table::*;
use fast_walk::{scan, Progress, ScanOptions};
use std::sync::OnceLock;
use std::time::Instant;
use randomizer::Randomizer;
use indicatif::ProgressBar;


use colored::*;
use std::path::PathBuf;

use clap::Parser;

const BYTES_PER_MB: f64 = (1024 * 1024) as f64;

/// Number of extensions shown in the terminal table. The CSV always contains
/// every extension.
const TABLE_ROWS: usize = 11;

#[derive(Parser)]
struct Cli {
    #[clap(short, long, value_parser)]
    path: PathBuf,

    #[clap(short, long, default_value_t = usize::MAX, value_parser)]
    max_depth: usize,

    #[clap(short, long, default_value_t = num_cpus::get(), value_parser)]
    threads: usize,

    /// Skip hidden files and directories. Off by default, so dot-directories
    /// such as .git are included in the totals.
    #[clap(long)]
    skip_hidden: bool,

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

fn main() -> Result<()> {
    let cli = Cli::parse();
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
    };

    let progress = BarProgress::default();
    let result = scan(&cli.path, &options, &progress)?;
    progress.finish();

    for err in &result.walk_errors {
        eprintln!("{} {}", "warning:".yellow(), err);
    }

    let rows = result.rows();

    let mut table = Table::new();

    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_header(vec!["Extension", "Quantity", "Capacity MB"]);

    let ran_string = Randomizer::ALPHANUMERIC(6).string().unwrap();

    let file_name = format!("results-{}.csv", ran_string);

    let mut wtr = csv::Writer::from_path(&file_name)?;
    wtr.write_record(["Extension", "Qty", "Cap Bytes"])?;

    for (table_index, (extension, bucket)) in rows.iter().enumerate() {
        if table_index < TABLE_ROWS {
            table.add_row(vec![
                extension.to_string(),
                bucket.count.to_string(),
                format!("{:.2}", bucket.bytes as f64 / BYTES_PER_MB),
            ]);
        }

        wtr.write_record(&[
            extension.to_string(),
            bucket.count.to_string(),
            bucket.bytes.to_string(),
        ])?;
    }
    wtr.flush()?;

    let total_files = result.total_files();
    let total_cap = result.total_bytes();

    let files_hour = (total_files as f32 / start.elapsed().as_secs_f32()) * 3600.00;

    println!("\nThat took: {:?}", start.elapsed());
    println!("Estimated files per-hour: {} {}", files_hour, emoji::travel_and_places::sky_and_weather::FIRE.glyph);

    println!(
        "\nTotal Files: {}, Total Cap: {} MB",
        total_files.to_string().green(),
        format!("{:.2}", total_cap as f64 / BYTES_PER_MB).bright_purple()
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

    println!("{table}");

    println!("Results written to {}", file_name);

    Ok(())
}
