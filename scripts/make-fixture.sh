#!/usr/bin/env bash
#
# Build a directory tree for exercising fast-walk.
#
# The companion to scripts/New-FastWalkFixture.ps1, producing the same tree so
# that a result from one platform can be compared against a result from
# another. With the default settings both produce:
#
#     20,232 files, 1,435,762,672 bytes
#
# Large files are created by setting their length rather than writing bytes,
# which makes them sparse: they cost almost nothing to create or store.
# fast-walk reports apparent size, so the totals still come out as above, but
# `du` will disagree and that is expected.
#
# Usage:
#     ./make-fixture.sh [--root DIR] [--small-file-count N]

set -euo pipefail

root="${FIXTURE_ROOT:-./fastwalk-test}"
small_file_count=20000

while [ $# -gt 0 ]; do
    case "$1" in
        --root) root="$2"; shift 2 ;;
        --small-file-count) small_file_count="$2"; shift 2 ;;
        -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

# Write a file of exactly $2 bytes. Small files are written with the shell's
# own printf, which avoids forking a process per file and matters when there
# are twenty thousand of them. Larger ones get their length set instead, which
# is instant.
small_limit=$((1024 * 1024))

make_file() {
    local path="$1" size="$2"
    mkdir -p "$(dirname "$path")"
    if [ "$size" -le "$small_limit" ]; then
        printf '%*s' "$size" '' > "$path"
    else
        : > "$path"
        truncate -s "$size" "$path"
    fi
}

# `touch -t` is understood by both GNU and BSD; the way to turn an epoch into
# its argument is not, so try each.
stamp_for_epoch() {
    date -d "@$1" +%Y%m%d%H%M.%S 2>/dev/null || date -r "$1" +%Y%m%d%H%M.%S
}

make_aged_file() {
    local path="$1" size="$2" days="$3"
    make_file "$path" "$size"
    touch -t "$(stamp_for_epoch $((now + days * 86400)))" "$path"
}

now=$(date +%s)

# --------------------------------------------------------------------------
# Root
# --------------------------------------------------------------------------

if [ -e "$root" ]; then
    echo "Removing existing $root ..."
    rm -rf "$root"
fi
mkdir -p "$root"
root=$(cd "$root" && pwd)
echo "Building fixture in $root"

# --------------------------------------------------------------------------
# 1. Small-file hotspot: the case that makes backups slow.
# --------------------------------------------------------------------------

echo "  $small_file_count tiny files in small-files/ ..."
mkdir -p "$root/small-files"
extensions=(.log .txt .xml .json .cfg)
i=0
while [ "$i" -lt "$small_file_count" ]; do
    extension="${extensions[$((i % 5))]}"
    # 200 bytes to 8 KB, so the band boundary at 4 KB gets exercised.
    printf '%*s' "$((200 + i % 8000))" '' \
        > "$(printf '%s/small-files/f%06d%s' "$root" "$i" "$extension")"
    i=$((i + 1))
done

# A second, milder hotspot so the ranking has something to order.
echo '  200 small files in some-small-files/ ...'
mkdir -p "$root/some-small-files"
i=0
while [ "$i" -lt 200 ]; do
    make_file "$(printf '%s/some-small-files/s%03d.txt' "$root" "$i")" 1024
    i=$((i + 1))
done

# --------------------------------------------------------------------------
# 2. Large-file control: similar bytes, almost no files.
# --------------------------------------------------------------------------

echo '  large files in large-files/ ...'
for n in 1 2 3 4; do
    make_file "$root/large-files/blob$n.bin" $((256 * 1024 * 1024))
done

# --------------------------------------------------------------------------
# 3. One file in each size band.
# --------------------------------------------------------------------------

echo '  one file per size band in size-bands/ ...'
make_file "$root/size-bands/empty.dat"         0
make_file "$root/size-bands/under-4k.dat"      2048
make_file "$root/size-bands/4k-to-64k.dat"     32768
make_file "$root/size-bands/64k-to-1m.dat"     524288
make_file "$root/size-bands/1m-to-16m.dat"     $((8 * 1024 * 1024))
make_file "$root/size-bands/16m-to-128m.dat"   $((64 * 1024 * 1024))
make_file "$root/size-bands/over-128m.dat"     $((200 * 1024 * 1024))

# --------------------------------------------------------------------------
# 4. One file in each age band, plus a future-dated one.
# --------------------------------------------------------------------------

echo '  one file per age band in age-bands/ ...'
make_aged_file "$root/age-bands/fresh.doc"   4096 -2
make_aged_file "$root/age-bands/recent.doc"  4096 -45
make_aged_file "$root/age-bands/stale.doc"   4096 -200
make_aged_file "$root/age-bands/old.doc"     4096 -500
make_aged_file "$root/age-bands/ancient.doc" 4096 -1500
# Clock skew: fast-walk reports this separately rather than calling it new.
make_aged_file "$root/age-bands/future.doc"  4096 30

# --------------------------------------------------------------------------
# 5. Hidden entries.
# --------------------------------------------------------------------------

echo '  hidden entries ...'
make_file "$root/.dotfile.cfg" 512
make_file "$root/.dotdir/inside-dotdir.txt" 2048

# These two carry the Windows hidden attribute in the PowerShell fixture, which
# has no equivalent here. They are still created so that the file counts match
# across platforms; on Unix they are simply ordinary files.
make_file "$root/attribute-hidden.cfg" 512
make_file "$root/attribute-hidden-dir/inside.txt" 512

# --------------------------------------------------------------------------
# 6. Depth, for --max-depth.
# --------------------------------------------------------------------------

echo '  nested tree for --max-depth ...'
deep="$root"
for level in 1 2 3 4 5 6; do
    deep="$deep/level$level"
    make_file "$deep/at-depth-$level.txt" $((1024 * level))
done

# --------------------------------------------------------------------------
# 7. Awkward names, since these are what tend to break scanners.
# --------------------------------------------------------------------------

echo '  awkward names ...'
make_file "$root/awkward-names/no-extension"       128
make_file "$root/awkward-names/archive.tar.gz"     4096
make_file "$root/awkward-names/spaces in name.txt" 128

# Extensions differing only in case go in separate directories so that this
# still works on a case-insensitive filesystem, where one directory would hold
# a single file rather than two. fast-walk groups by the literal text, so these
# land in two buckets, JPG and jpg.
make_file "$root/awkward-names/case-upper/photo.JPG" 128
make_file "$root/awkward-names/case-lower/photo.jpg" 128

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------

files=$(find "$root" -type f | wc -l | tr -d ' ')

echo ''
echo 'Done.'
echo "  files : $files"
echo ''
echo 'With the default --small-file-count, a scan should report:'
echo '  20,232 files, 1,435,762,672 bytes'
echo ''
echo 'Expected in the report:'
echo '  - a small-file hotspot at small-files/'
echo '  - every size band and every age band populated'
echo '  - one file in the "modified in future" band'
echo '  - .dotfile.cfg and .dotdir dropped by --skip-hidden'
echo '  - JPG and jpg as two separate extensions'
