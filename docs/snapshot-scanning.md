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

**Do not let the results land on the share.** They are written to the *current
working directory*, not next to the scanned path, so either run from local disk
or point `--output` at it:

```sh
fast-walk -p /mnt/snapshot -o ~/scans/nightly
```

Without `--output` the files are named for the time the scan ran, which means
repeated scans accumulate as a history rather than overwriting each other.

**Size the job before committing to it.** `--max-depth` gives you a cheap feel
for how long a full run will take:

```sh
fast-walk -p /mnt/snapshot -m 3
```

**Do not expect the thread count to buy you throughput.** Sweeping `--threads`
from 4 to 64 against a 20,000 file SMB share on a local network made no
measurable difference, with or without attribute caching: every setting landed
within the run to run noise. SMB returns file metadata batched with the
directory listing, so a scan costs round trips per *directory*, not per file,
and there is very little latency for extra threads to hide.

Lower it if you want to be gentle with a busy server, which is a reasonable
thing to want. Just do not expect it to make the scan faster:

```sh
fast-walk -p /mnt/snapshot -t 4
```

This was measured on a low latency local network. A high latency link would
shift the balance towards per-file waiting, where concurrency should help more,
but that has not been measured.

**Raise `actimeo` for large scans.** This is the setting that did make a
difference. The walk lists the entire tree before measuring any of it, so on a
scan lasting longer than the attribute cache lifetime, every file's metadata is
fetched once for the listing and then again for the measurement. Mounting with
`actimeo=0` rather than the default made the same scan 2.5 times slower.

Small scans finish inside the default cache lifetime and never notice. A share
big enough to take minutes will not, so give the cache a lifetime longer than
the scan:

```sh
sudo mount -t cifs //fileserver/data /mnt/snapshot \
    -o ro,vers=3.0,credentials=/root/.smbcred,actimeo=60
```

A long attribute cache is safe here because a snapshot does not change.

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

You need the `mount.nfs` helper, which comes from `nfs-common` on Debian and
Ubuntu or `nfs-utils` on RHEL and Fedora. Without it, `mount` falls back to the
raw system call and rejects options it cannot parse, reporting them as bad
options rather than as a missing package:

```sh
sudo apt install nfs-common
```

Mount read-only:

```sh
sudo mkdir -p /mnt/snapshot
sudo mount -t nfs -o ro,soft,timeo=100,retrans=3 \
    nas.example.com:/vol/data /mnt/snapshot
mount | grep nfs                   # confirm ro and the version actually took
```

`soft` is deliberate. With the default `hard`, a server that stops responding
leaves the scan hung indefinitely and unkillable; with `soft` the operation
fails, the failure surfaces as a `fast-walk` warning, and you know the totals
are incomplete. Soft mounts are the wrong default for read-write workloads that
can lose data, but a read-only scan cannot corrupt anything — the worst case is
a visibly incomplete result. The trade is that a soft mount reports failures as
a generic `Input/output error`, so when something goes wrong the real cause has
to come from `dmesg` or the server rather than from `errno`.

Pin `nfsvers=3` if the server is not a Unix NAS. Modern Linux negotiates NFSv4
by default, which several implementations handle differently or not at all.

Then scan the snapshot directly, rather than the live tree:

```sh
fast-walk -p /mnt/snapshot/.snapshot/nightly.0 -t 4 -o ~/scans/nightly
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

From Linux, you need `cifs-utils` installed. Without it there is no
`mount.cifs` helper, and since `credentials=` is parsed by that helper rather
than by the kernel, the option is silently ignored: the mount is attempted with
no username or password at all and the server rejects it. The failure looks
like an authentication problem, which sends you looking in the wrong place.

```sh
sudo apt install cifs-utils        # or dnf install cifs-utils
```

Then mount read-only with a credentials file, so the password does not end up
in `ps` output:

```sh
sudo mount -t cifs //fileserver/data /mnt/snapshot \
    -o ro,vers=3.0,credentials=/root/.smbcred
mount | grep cifs                  # confirm ro actually took
```

Write the credentials file without putting the password in your shell history
either, which a plain `echo` or `printf` would not achieve:

```sh
read -rp 'username: ' u && read -rsp 'password: ' p && echo
printf 'username=%s\npassword=%s\n' "$u" "$p" | sudo tee /root/.smbcred >/dev/null
sudo chmod 600 /root/.smbcred
unset u p
```

Add a `domain=` line only if the server is domain joined. For a local account
on a workgroup machine it is unnecessary, and a wrong value there is rejected
as an authentication failure. If the mount fails, `sudo dmesg | tail` carries
the real reason: `-2` is a share name that does not exist, `-13` is
authentication.

Reaching a specific shadow copy works differently on the two clients.

From Linux, use the `snapshot=` mount option. Addressing an `@GMT` path under
an ordinary mount does not work: path level `@GMT` traversal is a feature of
the Windows SMB client, and `snapshot=` is what the Linux client provides
instead.

```sh
sudo mount -t cifs //fileserver/data /mnt/snapshot \
    -o ro,vers=3.0,credentials=/root/.smbcred,snapshot=@GMT-2026.08.01-02.00.00
fast-walk -p /mnt/snapshot -o ~/scans/nightly
```

The token has to be exact, and it is in **UTC** while `vssadmin list shadows`
prints local time. Converting by hand invites an off by one timezone; ask the
server for it already converted:

```powershell
Get-CimInstance Win32_ShadowCopy | ForEach-Object {
    '@GMT-{0:yyyy.MM.dd-HH.mm.ss}' -f $_.InstallDate.ToUniversalTime()
}
```

A token that does not match a shadow copy fails the mount with `-2` in
`dmesg`, which reads like a missing share and is easy to misdiagnose. A `-22`
there would mean the option itself was rejected, so the two are worth telling
apart.

Shadow copies do not appear in a normal directory listing of the parent, so
there is no browsing your way to the name.

From Windows, address the `@GMT` path directly against the share:

```powershell
fast-walk.exe -p "\\fileserver\data\@GMT-2026.08.01-02.00.00" -t 4 -o $HOME\scans\nightly
```

One SMB-specific quirk: SMB is case-insensitive, but `fast-walk` groups by the
literal extension text. `PHOTO.JPG` and `photo.jpg` are reported as two separate
extensions, `JPG` and `jpg`. Sum them yourself when interpreting the results.

## Tracking growth between snapshots

Snapshots are what make growth measurable: two of them are two consistent
pictures of the same share taken at known times, which is exactly what a
comparison needs.

Scan each one, naming the output so the files can be found again:

```sh
fast-walk -p /mnt/snapshot/.snapshot/weekly.1  -t 4 -o ~/scans/baseline
fast-walk -p /mnt/snapshot/.snapshot/nightly.0 -t 4 -o ~/scans/current
```

Then compare them. This reads the two files and rescans nothing, so it is
instant and can be run long after the snapshots themselves have expired:

```sh
fast-walk --diff ~/scans/baseline.csv ~/scans/current.csv -o ~/scans/week-on-week
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

## What here has actually been tested

Mount options vary between kernel versions, distributions and NAS vendors, so
check `man mount.cifs` and `man nfs` on the machine you are scanning from
before running any of this against something you care about.

Which combinations have been run, and which are still only reasoned about, is
tracked in [TESTING.md](../TESTING.md) rather than repeated here, so that there
is one list to keep honest instead of two that drift apart.

The short version: the SMB path has been exercised against a Windows Server
share and produces output identical to a local scan of the same tree. The NFS
section has not been exercised against a NAS of any kind, and reaching a shadow
copy through `snapshot=` was not achieved. If you run any of this somewhere new,
a report is very welcome — see TESTING.md for how.

## A difference worth knowing about on Windows

`--skip-hidden` acts on the *name*: an entry counts as hidden if it begins with
a dot. It does not look at the Windows hidden attribute. A file marked hidden
with `attrib +h` is therefore still counted, while `.gitignore` is not.

That is consistent between a local Windows scan and the same share read over
SMB, so it does not make comparisons disagree, but it will surprise anyone who
expects `--skip-hidden` to mean what Explorer means by hidden.
