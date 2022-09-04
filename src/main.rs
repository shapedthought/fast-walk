use anyhow::Result;
use comfy_table::modifiers::{UTF8_ROUND_CORNERS, UTF8_SOLID_INNER_BORDERS};
use comfy_table::presets::UTF8_FULL;
use comfy_table::*;
use jwalk::{Parallelism, WalkDir};
use spinners::{Spinner, Spinners};
use std::collections::HashMap;
use std::time::Instant;

// use std::thread::sleep;
// use std::time::Duration;
use colored::*;
use num_cpus;
use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
struct Cli {
    #[clap(short, long, value_parser)]
    path: PathBuf,

    #[clap(short, long, default_value_t = usize::MAX, value_parser)]
    max_depth: usize,

    #[clap(short, long, default_value_t = num_cpus::get(), value_parser)]
    threads: usize,
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

    let mut extensions = HashMap::new();
    let mut capacity = HashMap::new();

    let mut sp = Spinner::new(Spinners::Aesthetic, "Processing".into());

    for entry in WalkDir::new(cli.path)
        .sort(true)
        .max_depth(cli.max_depth)
        .parallelism(Parallelism::RayonNewPool(use_threads))
    {
        let entry = &entry?;

        let metadata = entry.metadata()?;

        if metadata.is_file() {
            // let metadata = entry.metadata()?;
            let extention = entry
                .file_name()
                .to_str()
                .unwrap()
                .split(".")
                .collect::<Vec<&str>>();

            let size = entry.metadata()?.len();

            extensions
                .entry(extention.last().unwrap().to_string())
                .and_modify(|counter| *counter += 1)
                .or_insert(1);

            capacity
                .entry(extention.last().unwrap().to_string())
                .and_modify(|counter| *counter += size as i64)
                .or_insert(size as i64);
        }
    }

    let mut count_vec: Vec<(&String, &i32)> = extensions.iter().collect();
    count_vec.sort_by(|a, b| b.1.cmp(a.1));

    let sum_vec: Vec<(&String, &i64)> = capacity.iter().collect();

    // let mut file = File::create("results.txt")?;
    let mut wtr = csv::Writer::from_path("results.csv")?;
    wtr.write_record(&["Extention", "Qty", "Cap Bytes"])?;

    let mut table = Table::new();

    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_ROUND_CORNERS)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_header(vec!["Extension", "Quantity", "Capacity Bytes"]);

    let mut table_index = 0;

    for i in &count_vec {
        let cap = sum_vec.iter().filter(|x| x.0 == i.0).last().unwrap();

        if table_index < 11 {
            table.add_row(vec![i.0.to_string(), i.1.to_string(), cap.1.to_string()]);

            table_index += 1;
        }

        wtr.write_record(&[i.0.to_string(), i.1.to_string(), cap.1.to_string()])?;
    }
    wtr.flush()?;
    let total_files = count_vec.iter().map(|x| x.1).fold(0, |a, x| a + x);
    let total_cap = sum_vec.iter().map(|x| x.1).fold(0, |a, x| a + x);

    // sleep(Duration::from_secs(10));
    sp.stop();

    let files_hour = (total_files as f32 / start.elapsed().as_secs_f32()) * 3600.00;

    println!("\nThat took: {:?}", start.elapsed());
    println!("Estimated files per-hour: {} {}", files_hour, emoji::travel_and_places::sky_and_weather::FIRE.glyph);

    println!(
        "\nTotal Files: {}, Total Cap: {} MB",
        total_files.to_string().green(),
        (total_cap / (1024 * 1024)).to_string().bright_purple()
    );

    println!("{table}");

    Ok(())
}
