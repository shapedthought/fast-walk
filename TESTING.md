# Testing status

`fast-walk` is aimed at network shares and NAS appliances, and most of those
cannot be reproduced in CI. This page records which combinations have actually
been run and which are still only reasoned about, so that a reader can tell the
difference, and so that anyone who runs one can say so.

The unit and integration tests cover the logic on a local filesystem and run on
every change. Nothing below is about those. This is about transports and
servers.

## What has been run

Dates and versions are when the result was obtained, not when it was written
down. A result against an older version is still worth having; it just may not
still hold.

| What | Client | Server or filesystem | Status | Version | Date |
| --- | --- | --- | --- | --- | --- |
| Local scan | Linux, ext4 | — | Verified | 0.2.0 | 2026-08-02 |
| Local scan | Windows Server, NTFS | — | Verified | 0.2.0 | 2026-08-02 |
| Local scan | macOS, APFS | — | **Not tried** | | |
| SMB | Linux, cifs | Windows Server | Verified, byte identical to a local scan of the same tree | 0.2.0 | 2026-08-02 |
| SMB | macOS, smbfs | any | **Not tried** | | |
| SMB | Windows | Windows Server | **Not tried** | | |
| SMB shadow copy, `snapshot=` mount option | Linux, cifs | Windows Server | Partial: the option is accepted and the `@GMT` form is right, but no shadow copy was successfully reached | 0.2.0 | 2026-08-02 |
| SMB shadow copy, `@GMT` path | Windows | Windows Server | **Not tried** | | |
| NFS | Linux | Windows Server for NFS | Inconclusive: mounts over NFSv3, then every directory listing fails with an I/O error. A fact about that server rather than about fast-walk | 0.2.0 | 2026-08-02 |
| NFS | Linux | NetApp | **Not tried** | | |
| NFS | Linux | ZFS or TrueNAS | **Not tried** | | |
| `.snapshot` directory traversal | any | NetApp | **Not tried** | | |
| `.zfs/snapshot` traversal | any | ZFS | **Not tried** | | |

The single most useful confirmed result is the SMB one: **the same tree scanned
locally on the server and over SMB from another machine produces identical
output across all five reports, down to the byte.** Other people's results can
reinforce or break that.

That run predates the directory structure report, which was the sixth report to
be added. Nothing about it is expected to differ over SMB — it is fed from the
same directory listings as everything else — but that is reasoning, not a
result, and the row above should not be read as covering it.

Findings from those runs that changed the tool or the documentation are written
up in [docs/snapshot-scanning.md](docs/snapshot-scanning.md).

## What would be most useful

Roughly in order of how much they would tell us:

1. **A NetApp or ZFS NFS export.** The NFS guidance is written for these and has
   never met one. The `.snapshot` and `.zfs` traversal warnings in particular
   are reasoning, not observation.
2. **Anything from macOS.** No result at all so far, local or over SMB. macOS
   also writes its own metadata onto shares it browses, which is worth knowing
   the shape of.
3. **A tree with millions of files.** The scan lists everything before measuring
   any of it, so memory grows with file count and attribute caches can expire
   between the two phases. Neither effect has been seen on anything larger than
   twenty thousand files.
4. **A high latency link**, such as a share over a VPN. Every measurement so far
   is from a local network, where round trip time is small enough not to matter.
5. **A shadow copy actually scanned**, by either route.

## Reporting a result

Open an issue using the **Test report** template. It asks for the things that
turned out to matter when interpreting previous runs — the mount command in
particular, since without it a result cannot be reproduced or explained.

Good and bad results are equally welcome. "This combination does not work" is
information; several of the corrections already made came from failures rather
than successes.

### Use the standard fixture

Results are only comparable if the trees are. `scripts/` holds two generators
that build the same tree, so a report can refer to it rather than describing a
private directory:

```sh
./scripts/make-fixture.sh --root /srv/fastwalk-test
```

```powershell
.\scripts\New-FastWalkFixture.ps1 -Root D:\fastwalk-test
```

Both produce **20,232 files and 1,435,762,672 bytes**, and both have been
confirmed to give identical `fast-walk` output to one another. The tree covers
every size band, every age band including a future dated file, hidden entries
under both the dot prefix and the Windows attribute, a small file hotspot, and
names that tend to break scanners.

If a scan of the standard fixture reports anything other than those totals, that
by itself is a result worth reporting.

Large files in the fixture are sparse, so it costs little disk despite the
figure above. `fast-walk` reports apparent size, so the totals hold; `du` will
disagree, which is expected and is documented behaviour.

## Keeping this honest

A page like this is worse than useless if it drifts, because it asserts a
verification status that may be a year stale. Two rules:

- Every row carries the version and date it was obtained under.
- Accepting a test report includes updating the table in the same change. A
  report that does not move a row from **Not tried** to something else has not
  really been accepted.
