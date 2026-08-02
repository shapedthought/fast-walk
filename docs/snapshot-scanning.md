# Scanning a filesystem snapshot

`fast-walk` only ever reads, but it reads a lot: it lists the whole tree and
then issues one `stat` per file. Pointing it at a live production share is
therefore both slow and inaccurate. Pointing it at a snapshot is neither.

This page covers running it safely against SMB and NFS shares.

## Why a snapshot rather than the live share

- **The answer is consistent.** A live tree changes underneath the walk. Files
  listed at the start can be gone before they are measured, and files created
  during the scan are counted or missed depending on timing. The totals end up
  being a blur across the whole scan window rather than a picture of one moment.
- **Production is left alone.** One `stat` per file is a lot of metadata
  traffic. Against a busy file server this competes with real work.
- **Nothing can be changed.** Snapshots are read-only by construction, so a
  mistake in the command line cannot damage anything.

## Rules that apply to both protocols

**Mount read-only.** `fast-walk` never writes to the tree it scans, but
mounting with `-o ro` removes the question entirely.

**Do not let the CSV land on the share.** The results file is written to the
*current working directory*, not next to the scanned path. `cd` somewhere local
first:

```sh
cd ~/scans
fast-walk -p /mnt/snapshot
```

**Size the job before committing to it.** `--max-depth` gives you a cheap feel
for how long a full run will take:

```sh
fast-walk -p /mnt/snapshot -m 3
```

**Turn the thread count down for network mounts.** The default is one thread
per CPU, which is right for local disks. Over SMB or NFS every `stat` is a
network round trip, and too much concurrency saturates the link or the server's
metadata path rather than going faster. Start around `-t 4` and raise it while
watching the server:

```sh
fast-walk -p /mnt/snapshot -t 4
```

**Read the warnings.** A scan that could not see everything says so:

```
warning: 3 entries could not be walked
warning: 12 files could not be measured and were left out of the totals
```

Both mean the totals are an undercount. A non-zero exit status means the path
itself could not be read and nothing was scanned at all.

**Watch for nested snapshot directories.** `fast-walk` includes hidden files and
directories by default, so a scan rooted at a live volume will descend into
`.snapshot` or `.zfs` if they are visible and count every retained snapshot as
though it were live data. This inflates the totals enormously. See the
protocol-specific notes below.

## NFS

Where the snapshots live depends on the server:

| Server | Snapshot path |
| --- | --- |
| NetApp | `<mountpoint>/.snapshot/<name>` |
| ZFS | `<mountpoint>/.zfs/snapshot/<name>` |
| LVM and similar | mount the snapshot device separately |

Mount read-only:

```sh
sudo mkdir -p /mnt/snapshot
sudo mount -t nfs -o ro,soft,timeo=100,retrans=3 \
    nas.example.com:/vol/data /mnt/snapshot
```

`soft` is deliberate. With the default `hard`, a server that stops responding
leaves the scan hung indefinitely and unkillable; with `soft` the operation
fails, the failure surfaces as a `fast-walk` warning, and you know the totals
are incomplete. Soft mounts are the wrong default for read-write workloads that
can lose data, but a read-only scan cannot corrupt anything — the worst case is
a visibly incomplete result.

Then scan the snapshot directly, rather than the live tree:

```sh
cd ~/scans
fast-walk -p /mnt/snapshot/.snapshot/nightly.0 -t 4
```

Pointing `-p` at one snapshot is much better than scanning the live root and
trying to filter afterwards. If you must scan the live root, pass
`--skip-hidden` so `.snapshot` and `.zfs` are not traversed — but be aware that
this also drops ordinary dotfiles from the totals, which on a user home share
can be a large amount of real data.

Whether the snapshot directory is visible at all is a server-side setting
(`snapdir-access` on NetApp, `snapdir` on ZFS). Check with `ls -a` on the mount
point before assuming either way.

Unmount when finished:

```sh
sudo umount /mnt/snapshot
```

## SMB

SMB snapshots are Volume Shadow Copies, exposed to clients as `@GMT` tokens.

To find out which ones exist, ask the server. On a Windows file server:

```powershell
vssadmin list shadows
```

From Linux, mount the share read-only with a credentials file so the password
does not end up in your shell history or in `ps` output:

```sh
sudo mount -t cifs //fileserver/data /mnt/snapshot \
    -o ro,vers=3.0,credentials=/root/.smbcred
```

Where `/root/.smbcred` is `chmod 600` and contains:

```
username=scanner
password=...
domain=EXAMPLE
```

Recent kernels accept a `snapshot=` mount option to mount a specific shadow
copy directly. Where that is unavailable, address the `@GMT` path itself. The
token has to be given exactly — shadow copies generally do not appear in a
normal directory listing of the parent, so you cannot discover them by browsing:

```sh
cd ~/scans
fast-walk -p '/mnt/snapshot/@GMT-2026.08.01-02.00.00' -t 4
```

Or from Windows, against the share directly:

```powershell
cd $HOME\scans
fast-walk.exe -p "\\fileserver\data\@GMT-2026.08.01-02.00.00" -t 4
```

One SMB-specific quirk: SMB is case-insensitive, but `fast-walk` groups by the
literal extension text. `PHOTO.JPG` and `photo.jpg` are reported as two separate
extensions, `JPG` and `jpg`. Sum them yourself when interpreting the results.

## Tracking growth between snapshots

Snapshots are what make growth measurable: two of them are two consistent
pictures of the same share taken at known times, which is exactly what a
comparison needs.

Scan each one, keeping the results CSVs somewhere they will survive:

```sh
cd ~/scans
fast-walk -p /mnt/snapshot/.snapshot/weekly.1 -t 4
mv results-*.csv baseline.csv

fast-walk -p /mnt/snapshot/.snapshot/nightly.0 -t 4
mv results-*.csv current.csv
```

Then compare them. This reads the two files and rescans nothing, so it is
instant and can be run long after the snapshots themselves have expired:

```sh
fast-walk --diff baseline.csv current.csv
```

The output is ordered by how much capacity moved, so whatever is driving growth
appears first. Keeping one CSV per week gives you a growth trend per extension
without needing anything else to store it.

Two things to keep consistent between the scans being compared, or the
difference will include your own change of method rather than real growth: use
the same `--skip-hidden` setting for both, and scan an equivalent path in each
snapshot rather than the live tree in one and a snapshot in the other.

## Planning a backup window

Backup time is not proportional to capacity. Per-file overhead dominates for
small files, so a 2 TB share of small files can take far longer to protect than
a 20 TB share of large ones. The size band table shows the split directly, and
the line beneath it gives the headline figure:

```
65495 files (96.1% of the total) are 64.0 KB or smaller, holding 388.9 MB between them
```

When that share is high, the hotspot report names the directories responsible so
they can be given their own backup job, excluded, or archived rather than being
rescanned nightly along with everything else. Because files count towards the
directory that actually holds them, the listed paths can be used as-is.

Set the threshold to match the software doing the backup, since where per-file
overhead starts to dominate varies:

```sh
fast-walk -p /mnt/snapshot/.snapshot/nightly.0 --small-under 128K --hotspots 25
```

This is the one report that is not free. It keeps a running total per directory,
so memory grows with the number of directories rather than the number of files,
and it measured about 18% of scan time on a 68,000 file tree. On a very large
share, scan once with `--hotspots 0` to get the size bands cheaply, then rerun
with hotspots enabled against the specific subtree the bands implicate.

## Finding what is actually consuming the space

The per-extension totals say `.mp4` is the problem; the largest-files report
says which files to go and look at. It is on by default and costs nothing extra:

```sh
fast-walk -p /mnt/snapshot/.snapshot/nightly.0 --top 50
```

The age report, also produced automatically, is usually the more actionable one
on a NAS: it groups capacity by how long ago each file was last modified, so you
can see at a glance what share of a share has not been touched in over two
years. That is the number that justifies an archive tier. The modification time
comes from the `stat` the scan already performs, so it costs no additional
round trips over the mount.

Watch the `modified in future` band when scanning over a network. Files landing
there mean the file server's clock and the scanning host's clock disagree, which
makes every age figure from that server suspect until it is resolved.

## What the totals do and do not mean

Two things are worth knowing before reporting these numbers to anyone.

**Sizes are apparent sizes, not disk usage.** `fast-walk` reports each file's
length, which is what the file claims to contain, not the space it occupies. A
10 MB sparse file that has only 8 KB allocated is reported as 10 MB. Totals will
not match `du` or the server's own capacity reporting, and on filesystems with
compression or deduplication — common on exactly the appliances you would be
snapshotting — the difference can be large.

**Hard links are counted once per name.** Three names pointing at one 10 KB
inode are reported as three files and 30 KB. Snapshot and backup trees use hard
links heavily, so this can inflate a scan substantially.

Both behaviours are what you want for a question like "how much data do these
files represent"; neither answers "how much space will I free by deleting them".

## Notes

The mount commands above are examples. Available options vary between kernel
versions, distributions, and NAS vendors — check `man mount.cifs` and `man nfs`
on the machine you are scanning from before running them against anything you
care about.
