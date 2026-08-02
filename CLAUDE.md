# Working on fast-walk

Scans a filesystem and reports file counts and capacity broken down by
extension, age, size band, small-file directory and largest file, plus a mode
for comparing two earlier scans. Aimed at storage and backup work: auditing NAS
shares, sizing backup windows, tracking growth between snapshots.

`src/lib.rs` holds the walk and the aggregation; `src/main.rs` is argument
parsing and presentation; `src/diff.rs` compares two results CSVs. The split
exists so the logic can be tested without going through the terminal output.

## Checks

CI runs these three and denies warnings. Run them before pushing:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test`, not `cargo test --all-targets` — the latter looks more thorough
but silently drops doc tests.

## Conventions

**Tests are named for the behaviour they pin, not the function they call.**
`a_root_that_cannot_be_listed_is_an_error_not_a_zero_file_scan`, not
`test_scan_error`. Where a test guards against a specific past bug, a comment
starting `Regression:` says what the bug was.

**Permission tests skip themselves under root.** Root bypasses permission bits,
so a `chmod 000` fixture denies nothing and the assertions pass vacuously. The
tests check `fs::read_dir(...).is_err()` first and print a skip notice
otherwise. When touching these, verify them as an unprivileged user or you are
not testing anything.

**Truncated output says what it left out.** Tables capped at N rows print how
many rows exist and where the full set is. A view that silently truncates reads
as the whole picture.

**Ties are broken deterministically**, by name or path, everywhere ordering is
produced. Results are built in parallel, so without an explicit tie-break the
output varies between runs. There are tests pinning this.

**Commit messages explain why, not what.** The diff shows what changed. The
message should say what was wrong, what was measured, and what was decided
against. Several existing messages carry benchmark numbers and the reasoning
behind a default; keep that up.

## Things that will bite

**`--hotspots` costs about 18% of scan time** and is on by default. It keeps a
running total per directory, so memory grows with directory count.
`--hotspots 0` turns it off and returns to baseline exactly. Everything else —
age bands, size bands, largest files — is free, coming from the `stat` already
performed.

**The structure report is fed from `process_read_dir`, not from the per-file
pass.** jwalk hands that callback one directory's whole listing at a time, on
the walk threads, which is why the report costs nothing measurable: one update
per directory into fixed counters, rather than per file into a growing map.
Moving any of it into the measuring fold would turn a free report into a
per-file one. The per-level counters are a fixed array so the walk threads never
take a lock; levels past `MAX_TRACKED_DEPTH` share one labelled overflow row
while the exact depth is still tracked. Interleaved measurement found no
difference above run-to-run variance at up to 31,886 directories on local APFS;
it has never been measured over SMB or NFS.

**Structure path lengths exclude the scan root's prefix.** The number is meant
to answer "will this tree fit under the restore target", and the prefix it
currently sits under will not be there. A change making it absolute would look
like a bug fix and would break that.

**Sizes are apparent, not allocated.** Sparse files report their full length and
hard links are counted once per name, so totals will not match `du`. This is
deliberate and documented; do not "fix" it without reading
`docs/snapshot-scanning.md`.

**`--skip-hidden` matches a leading dot** and ignores the Windows hidden
attribute, so `attrib +h` files are still counted. Consistent between local and
SMB scans, but surprising if you expect Explorer's meaning.

**Both scan phases must share one thread pool.** The walk and the measuring pass
used to use different pools, so `--threads` governed only the walk and every
thread measurement was tuning the wrong half. If you touch `scan()`, keep the
`pool.install(...)` around the aggregation.

## Measurement discipline

This project has been burned by careless benchmarking more than once.

**Interleave runs and take a median.** A published 1.8x speedup turned out to be
1.25x once the two versions were run alternately instead of one after the other.
Run-to-run variance on the same configuration has been observed at 45%, which is
larger than most effects worth measuring.

**Say what a number does not cover.** Every performance claim in the docs names
the conditions it was measured under.

**Do not reason about network I/O and call it a finding.** Two confident claims
about SMB behaviour were wrong: that every `stat` is a network round trip, and
that oversubscribing threads would help. SMB batches file metadata into the
directory listing, so cost scales with directories, not files, and there is
little latency for extra threads to hide. Both errors are corrected in the docs;
do not reintroduce them from first principles.

## Verified versus reasoned about

`TESTING.md` records which transports and servers have actually been run
against, with the version and date. The SMB path is verified — a scan over SMB
and a local scan of the same tree produce byte-identical output. NFS against a
real NAS, macOS anything, high latency links, and trees larger than twenty
thousand files are not.

Two rules that keep it useful:

- Check it before asserting something about a transport. The thread-count
  guidance in particular reads like it wants "fixing" back to something the
  measurements disproved.
- Accepting a test report includes moving a row in the same change. A report
  that does not move a row out of **Not tried** has not really been accepted.

`scripts/make-fixture.sh` and `scripts/New-FastWalkFixture.ps1` build the same
tree — 20,232 files, 1,435,762,672 bytes — so results from different machines
are comparable. They have been diffed against each other to confirm they agree.
