# fast-walk

Mini project to scan a filesystem to get the quantity and capacity of each extension. 

Compile from source.

- Install Rust https://www.rust-lang.org/tools/install 
- Clone this repo
- Run the following in the following command

cargo:

    cargo build --release

The file will be in the target/release folder.

## How to use

Run app in terminal.

    fast-walk

    USAGE:
        fast-walk [OPTIONS] --path <PATH>

    OPTIONS:
        -h, --help                     Print help information
        -V, --version                  Print version
        -m, --max-depth <MAX_DEPTH>    [default: 18446744073709551615]
        -p, --path <PATH>
        -t, --threads <THREADS>        [default: 8]
            --skip-hidden              Skip hidden files and directories
            --top <TOP>                Largest files to report, 0 disables [default: 10]
            --diff <OLD> <NEW>         Compare two results CSVs instead of scanning

Example running on the same directory as the application, max depth 5, threads 4

    ./fast-walk -p . -m 5 -t 4

App uses the total system threads if threads are not set.

Max depth can be useful for testing speed.

Hidden files and directories are included by default, so dot-directories such as
`.git` count towards the totals. Pass `--skip-hidden` to leave them out.

If a directory cannot be read it is reported as a warning and its contents are
missing from the totals. If the path given to `--path` cannot be read at all,
the tool exits with an error rather than reporting an empty scan.

## Scanning network shares

To run against an SMB or NFS share, scan a snapshot rather than the live tree:
the results are consistent, production is left alone, and there is nothing to
damage. See [docs/snapshot-scanning.md](docs/snapshot-scanning.md) for mounting,
thread counts, and the snapshot directory traps to avoid.

## Output

Tools outputs a CSV of the extensions arranged by quantity, also includes total capacity in bytes.

The extension is the part of the file name after the final ".", so `archive.tar.gz`
counts as `gz`. Files with no extension are grouped under `<none>`; this includes
dotfiles such as `.gitignore`, which are treated as having no extension rather
than an extension of `gitignore`.

Both the table and the CSV report the average file size for each extension, and
the summary line reports the average across every file scanned.

The CSV reports capacity and average size in bytes. The terminal table reports
capacity in MB and scales the average to whichever unit suits it, since average
file sizes are usually far below a megabyte.

A scan writes three CSVs, sharing one name stem:

| File | Contents |
| --- | --- |
| `results-XXXXXX.csv` | totals per extension |
| `results-XXXXXX-age.csv` | totals per age band |
| `results-XXXXXX-largest.csv` | the largest files found |

### By age

Files are also grouped by how long ago they were last modified, into bands of
under 30 days, 30 to 90 days, 90 days to a year, 1 to 2 years, and over 2 years,
with each band's share of total capacity. This is what tells you how much of a
share is cold data.

The modification time comes from the same `stat` the scan already performs, so
the age report costs nothing extra — including over a network mount.

Two bands exist for data that cannot be aged normally. Files modified *after*
the scan started are reported as `modified in future` rather than being counted
as new; this normally means the clocks on the scanning host and the file server
disagree. Files whose modification time the filesystem does not report land in
`unknown`.

### Largest files

The largest files found are listed with their full paths, ten by default.
`--top 50` asks for more, `--top 0` turns the report off. Aggregates tell you
which extension is consuming capacity; this tells you which files to go look at.

### Comparing two scans

Point `--diff` at the extension CSVs from two earlier runs to see what changed
between them. Nothing is rescanned, so snapshots taken weeks apart can be
compared long after the fact:

    fast-walk --diff results-monday.csv results-friday.csv

    Files: 3 -> 4 (+1)
    Capacity: 2.1 MB -> 11.2 MB (+9.1 MB)

    ╭────────────┬─────────┬────────────┬────────────┬───────────╮
    │ Extension  │ Δ Files │ Δ Capacity │ Cap Before │ Cap After │
    ╞════════════╪═════════╪════════════╪════════════╪═══════════╡
    │ mp4        │ +1      │ +8.6 MB    │ 1.9 MB     │ 10.5 MB   │
    │ docx       │ 0       │ +293.0 KB  │ 97.7 KB    │ 390.6 KB  │
    │ pdf (new)  │ +1      │ +293.0 KB  │ 0 B        │ 293.0 KB  │
    │ txt (gone) │ -1      │ -48.8 KB   │ 48.8 KB    │ 0 B       │
    ╰────────────┴─────────┴────────────┴────────────┴───────────╯

Extensions that did not change are left out, and the rest are ordered by how
much capacity moved, in either direction. Extensions present in only one of the
two scans are marked `(new)` or `(gone)`. The comparison is written to a
`diff-XXXXXX.csv` alongside the terminal output.

Columns are matched by header name, so a CSV written by a different version of
the tool still reads as long as it carries `Extension`, `Qty` and `Cap Bytes`.

Sizes are apparent file sizes rather than space consumed on disk, and a hard
linked file is counted once per name. See
[docs/snapshot-scanning.md](docs/snapshot-scanning.md) for what that means when
interpreting the totals.

The top 10 files are displayed in a table in the terminal. 

<img src="output.png" alt="output" width="300"/>

