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
        -m, --max-depth <MAX_DEPTH>    [default: 18446744073709551615]
        -p, --path <PATH>
        -t, --threads <THREADS>        [default: 8]
            --skip-hidden              Skip hidden files and directories

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

Sizes are apparent file sizes rather than space consumed on disk, and a hard
linked file is counted once per name. See
[docs/snapshot-scanning.md](docs/snapshot-scanning.md) for what that means when
interpreting the totals.

The top 10 files are displayed in a table in the terminal. 

<img src="output.png" alt="output" width="300"/>

