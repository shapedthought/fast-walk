<#
.SYNOPSIS
    Build a directory tree for exercising fast-walk.

.DESCRIPTION
    Creates a tree that touches every report the tool produces: a small-file
    hotspot, a large-file control directory, one file in each size band, files
    spread across each age band (including one dated into the future), hidden
    entries under both the dot-prefix and the Windows attribute, and some depth.

    Large files are created by setting the file length rather than writing
    bytes, so they cost almost no time. Pass -Sparse to also mark them sparse
    so they cost almost no disk either; note that fast-walk reports apparent
    size, so a sparse 1 GB file is still reported as 1 GB.

.EXAMPLE
    .\New-FastWalkFixture.ps1 -Root D:\fastwalk-test -SmallFileCount 20000

.EXAMPLE
    .\New-FastWalkFixture.ps1 -Root D:\fastwalk-test -Sparse
#>
#Requires -Version 5.1
[CmdletBinding()]
param(
    # Where to build the tree. Created if missing; contents are replaced.
    [string] $Root = 'C:\fastwalk-test',

    # How many tiny files to put in the hotspot directory.
    [int] $SmallFileCount = 20000,

    # Mark the large files sparse so they consume almost no disk.
    [switch] $Sparse
)

$ErrorActionPreference = 'Stop'

# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

function New-Dir {
    param([string] $Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        [void] (New-Item -ItemType Directory -Path $Path -Force)
    }
    return $Path
}

function New-TestFile {
    <#
        Creates a file of an exact size. Small files are written directly;
        anything larger has its length set, which NTFS satisfies without
        writing the bytes out.
    #>
    param(
        [Parameter(Mandatory)] [string] $Path,
        [Parameter(Mandatory)] [long]   $Size,
        [datetime] $LastWrite,
        [switch]   $MakeSparse
    )

    if ($Size -le 1MB) {
        [System.IO.File]::WriteAllBytes($Path, (New-Object byte[] $Size))
    }
    else {
        $stream = [System.IO.File]::Create($Path)
        try {
            if ($MakeSparse) {
                # Ask NTFS to keep the unwritten range unallocated. Needs the
                # volume to be NTFS; harmless to skip if it fails.
                try {
                    $null = & fsutil sparse setflag "$Path" 2>&1
                } catch {
                    Write-Warning "Could not mark $Path sparse: $_"
                }
            }
            $stream.SetLength($Size)
        }
        finally {
            $stream.Dispose()
        }
    }

    if ($PSBoundParameters.ContainsKey('LastWrite')) {
        [System.IO.File]::SetLastWriteTime($Path, $LastWrite)
    }
}

$now = Get-Date

# --------------------------------------------------------------------------
# Root
# --------------------------------------------------------------------------

if (Test-Path -LiteralPath $Root) {
    Write-Host "Removing existing $Root ..."
    # Clear hidden attributes first, or Remove-Item can baulk.
    Get-ChildItem -LiteralPath $Root -Recurse -Force -ErrorAction SilentlyContinue |
        ForEach-Object { $_.Attributes = 'Normal' }
    Remove-Item -LiteralPath $Root -Recurse -Force
}
[void] (New-Dir $Root)
Write-Host "Building fixture in $Root"

# --------------------------------------------------------------------------
# 1. Small-file hotspot: the case that makes backups slow.
# --------------------------------------------------------------------------

$hotspot = New-Dir (Join-Path $Root 'small-files')
Write-Host "  $SmallFileCount tiny files in small-files\ ..."

$extensions = @('.log', '.txt', '.xml', '.json', '.cfg')
for ($i = 0; $i -lt $SmallFileCount; $i++) {
    $extension = $extensions[$i % $extensions.Count]
    $path = Join-Path $hotspot ("f{0:D6}{1}" -f $i, $extension)
    # 200 bytes to 8 KB, so the band boundary at 4 KB gets exercised.
    [System.IO.File]::WriteAllBytes($path, (New-Object byte[] (200 + ($i % 8000))))
}

# A second, milder hotspot so the ranking has something to order.
$mild = New-Dir (Join-Path $Root 'some-small-files')
for ($i = 0; $i -lt 200; $i++) {
    New-TestFile -Path (Join-Path $mild ("s{0:D3}.txt" -f $i)) -Size 1024
}

# --------------------------------------------------------------------------
# 2. Large-file control: similar bytes, almost no files.
# --------------------------------------------------------------------------

$large = New-Dir (Join-Path $Root 'large-files')
Write-Host '  large files in large-files\ ...'
foreach ($n in 1..4) {
    New-TestFile -Path (Join-Path $large "blob$n.bin") -Size (256MB) -MakeSparse:$Sparse
}

# --------------------------------------------------------------------------
# 3. One file in each size band.
# --------------------------------------------------------------------------

$bands = New-Dir (Join-Path $Root 'size-bands')
Write-Host '  one file per size band in size-bands\ ...'
New-TestFile -Path (Join-Path $bands 'empty.dat')      -Size 0
New-TestFile -Path (Join-Path $bands 'under-4k.dat')   -Size 2KB
New-TestFile -Path (Join-Path $bands '4k-to-64k.dat')  -Size 32KB
New-TestFile -Path (Join-Path $bands '64k-to-1m.dat')  -Size 512KB
New-TestFile -Path (Join-Path $bands '1m-to-16m.dat')  -Size 8MB   -MakeSparse:$Sparse
New-TestFile -Path (Join-Path $bands '16m-to-128m.dat') -Size 64MB -MakeSparse:$Sparse
New-TestFile -Path (Join-Path $bands 'over-128m.dat')  -Size 200MB -MakeSparse:$Sparse

# --------------------------------------------------------------------------
# 4. One file in each age band, plus a future-dated one.
# --------------------------------------------------------------------------

$ages = New-Dir (Join-Path $Root 'age-bands')
Write-Host '  one file per age band in age-bands\ ...'
$agePlan = @(
    @{ Name = 'fresh.doc';   Days = -2    },
    @{ Name = 'recent.doc';  Days = -45   },
    @{ Name = 'stale.doc';   Days = -200  },
    @{ Name = 'old.doc';     Days = -500  },
    @{ Name = 'ancient.doc'; Days = -1500 },
    # Clock skew: fast-walk reports this separately rather than calling it new.
    @{ Name = 'future.doc';  Days = 30    }
)
foreach ($entry in $agePlan) {
    New-TestFile -Path (Join-Path $ages $entry.Name) `
                 -Size 4096 `
                 -LastWrite $now.AddDays($entry.Days)
}

# --------------------------------------------------------------------------
# 5. Hidden entries, both conventions.
# --------------------------------------------------------------------------

Write-Host '  hidden entries (dot-prefixed and attribute-marked) ...'

# Dot-prefixed: this is what --skip-hidden actually acts on.
New-TestFile -Path (Join-Path $Root '.dotfile.cfg') -Size 512
$dotDir = New-Dir (Join-Path $Root '.dotdir')
New-TestFile -Path (Join-Path $dotDir 'inside-dotdir.txt') -Size 2048

# Windows hidden attribute: fast-walk does NOT treat this as hidden, so it
# stays in the totals even with --skip-hidden. That difference is the point
# of including it.
$attrHidden = Join-Path $Root 'attribute-hidden.cfg'
New-TestFile -Path $attrHidden -Size 512
(Get-Item -LiteralPath $attrHidden -Force).Attributes = 'Hidden'

$attrHiddenDir = New-Dir (Join-Path $Root 'attribute-hidden-dir')
New-TestFile -Path (Join-Path $attrHiddenDir 'inside.txt') -Size 512
(Get-Item -LiteralPath $attrHiddenDir -Force).Attributes = 'Directory, Hidden'

# --------------------------------------------------------------------------
# 6. Depth, for --max-depth.
# --------------------------------------------------------------------------

Write-Host '  nested tree for --max-depth ...'
$deep = $Root
foreach ($level in 1..6) {
    $deep = New-Dir (Join-Path $deep "level$level")
    New-TestFile -Path (Join-Path $deep "at-depth-$level.txt") -Size (1KB * $level)
}

# --------------------------------------------------------------------------
# 7. Awkward names, since these are what tend to break scanners.
# --------------------------------------------------------------------------

$awkward = New-Dir (Join-Path $Root 'awkward-names')
New-TestFile -Path (Join-Path $awkward 'no-extension')        -Size 128
New-TestFile -Path (Join-Path $awkward 'archive.tar.gz')      -Size 4096
New-TestFile -Path (Join-Path $awkward 'spaces in name.txt')  -Size 128

# Extensions differing only in case have to live in separate directories.
# NTFS is case-insensitive, so MiXeD.JPG and mixed.jpg in one directory are
# the same file: the second write would silently overwrite the first and the
# fixture would prove nothing. fast-walk groups by the literal text, so from
# separate directories these still land in two buckets, JPG and jpg.
$caseA = New-Dir (Join-Path $awkward 'case-upper')
$caseB = New-Dir (Join-Path $awkward 'case-lower')
New-TestFile -Path (Join-Path $caseA 'photo.JPG') -Size 128
New-TestFile -Path (Join-Path $caseB 'photo.jpg') -Size 128

# A trailing dot is deliberately not tested here: Win32 path normalisation
# strips it, so 'trailing-dot.' would quietly become 'trailing-dot' and the
# case would never be exercised. It is covered by the unit tests instead.

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------

$all = Get-ChildItem -LiteralPath $Root -Recurse -File -Force
$apparent = ($all | Measure-Object -Property Length -Sum).Sum

Write-Host ''
Write-Host 'Done.'
Write-Host ("  files          : {0:N0}" -f $all.Count)
Write-Host ("  apparent size  : {0:N2} GB" -f ($apparent / 1GB))
Write-Host ("  on-disk size   : run 'Get-Item {0} | Select-Object *' or check the drive" -f $Root)
Write-Host ''
Write-Host 'Expected when scanned:'
Write-Host ("  - a hotspot at {0} with ~{1:N0} small files" -f $hotspot, $SmallFileCount)
Write-Host '  - every size band and every age band populated'
Write-Host '  - one file in the "modified in future" band'
Write-Host '  - .dotfile.cfg and .dotdir dropped by --skip-hidden'
Write-Host '  - attribute-hidden.cfg still counted even with --skip-hidden'
Write-Host '  - JPG and jpg reported as two separate extensions'
Write-Host ''
Write-Host 'If this share will be scanned from a Mac, keep Finder away from it:'
Write-Host '  macOS writes .DS_Store, ._AppleDouble and .Spotlight-V100 entries'
Write-Host '  onto SMB shares it browses, which will show up as extra files.'
