use anyhow::{Context, Result};
use comfy_table::modifiers::{UTF8_ROUND_CORNERS, UTF8_SOLID_INNER_BORDERS};
use comfy_table::presets::UTF8_FULL;
use comfy_table::*;
use jwalk::{Parallelism, WalkDir};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use randomizer::Randomizer;
use indicatif::ProgressBar;
use rayon::prelude::*;


use colored::*;
use std::path::{Path, PathBuf};

use clap::Parser;

/// Bucket used for files that have no extension (including dotfiles such as
/// `.gitignore`, which `Path::extension` reports as extensionless).
const NO_EXTENSION: &str = "<none>";

const BYTES_PER_MB: f64 = (1024 * 1024) as f64;

/// Number of individual walk errors printed before the rest are summarised.
const MAX_REPORTED_ERRORS: usize = 10;

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

/// Extension of a file name, or `NO_EXTENSION` if it has none.
///
/// Uses `Path::extension` rather than splitting on '.', so `Makefile` and
/// `.gitignore` are reported as extensionless instead of as their own name.
/// Names that are not valid UTF-8 are lossily converted rather than panicking.
fn extension_of(file_name: &OsStr) -> String {
    Path::new(file_name)
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_else(|| NO_EXTENSION.to_string())
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

    // Fail loudly on a path that does not exist or cannot be read, rather than
    // reporting an empty scan as a success.
    std::fs::metadata(&cli.path)
        .with_context(|| format!("cannot read path: {}", cli.path.display()))?;

    let mut walk_errors = 0usize;

    let files: Vec<_> = WalkDir::new(&cli.path)
        .sort(true)
        .skip_hidden(cli.skip_hidden)
        .max_depth(cli.max_depth)
        .parallelism(Parallelism::RayonNewPool(use_threads))
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) => {
                // A directory that could not be read is still yielded as an
                // entry, with the failure recorded on the entry itself. Its
                // contents are missing from the totals, so report it.
                if let Some(err) = &entry.read_children_error {
                    if walk_errors < MAX_REPORTED_ERRORS {
                        eprintln!("{} {}", "warning:".yellow(), err);
                    }
                    walk_errors += 1;
                }
                Some(entry)
            }
            Err(err) => {
                if walk_errors < MAX_REPORTED_ERRORS {
                    eprintln!("{} {}", "warning:".yellow(), err);
                }
                walk_errors += 1;
                None
            }
        })
        .filter(|d| d.file_type().is_file())
        .collect();

    let bar = ProgressBar::new(files.len() as u64);

    // Files that vanished or became unreadable between the walk and the stat.
    let unreadable = AtomicU64::new(0);

    // Each rayon thread aggregates into its own map and the maps are merged at
    // the end, so no lock is taken on the per-file path. Values are
    // (file count, total bytes) keyed by extension.
    let totals: HashMap<String, (u64, u64)> = files
        .par_iter()
        .fold(
            HashMap::new,
            |mut acc: HashMap<String, (u64, u64)>, entry| {
                bar.inc(1);

                let size = match entry.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(_) => {
                        unreadable.fetch_add(1, Ordering::Relaxed);
                        return acc;
                    }
                };

                let slot = acc.entry(extension_of(entry.file_name())).or_insert((0, 0));
                slot.0 += 1;
                slot.1 += size;

                acc
            },
        )
        .reduce(HashMap::new, |mut acc, partial| {
            for (extension, (count, bytes)) in partial {
                let slot = acc.entry(extension).or_insert((0, 0));
                slot.0 += count;
                slot.1 += bytes;
            }
            acc
        });

    // Descending by file count, then by extension so equal counts are stable.
    let mut rows: Vec<(&String, &(u64, u64))> = totals.iter().collect();
    rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then_with(|| a.0.cmp(b.0)));

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

    for (table_index, (extension, (count, bytes))) in rows.iter().enumerate() {
        if table_index < 11 {
            table.add_row(vec![
                extension.to_string(),
                count.to_string(),
                format!("{:.2}", *bytes as f64 / BYTES_PER_MB),
            ]);
        }

        wtr.write_record(&[extension.to_string(), count.to_string(), bytes.to_string()])?;
    }
    wtr.flush()?;

    let total_files: u64 = rows.iter().map(|(_, (count, _))| count).sum();
    let total_cap: u64 = rows.iter().map(|(_, (_, bytes))| bytes).sum();

    let files_hour = (total_files as f32 / start.elapsed().as_secs_f32()) * 3600.00;

    println!("\nThat took: {:?}", start.elapsed());
    println!("Estimated files per-hour: {} {}", files_hour, emoji::travel_and_places::sky_and_weather::FIRE.glyph);

    println!(
        "\nTotal Files: {}, Total Cap: {} MB",
        total_files.to_string().green(),
        format!("{:.2}", total_cap as f64 / BYTES_PER_MB).bright_purple()
    );

    if walk_errors > 0 {
        println!(
            "{} {} entries could not be walked",
            "warning:".yellow(),
            walk_errors
        );
    }

    let unreadable = unreadable.load(Ordering::Relaxed);
    if unreadable > 0 {
        println!(
            "{} {} files could not be measured and were left out of the totals",
            "warning:".yellow(),
            unreadable
        );
    }

    println!("{table}");

    println!("Results written to {}", file_name);

    Ok(())
}
