<#
.SYNOPSIS
  Build an NTFS fixture using Microsoft's own NTFS driver, as an independent
  sample against the ntfs-3g-built fixtures.

.DESCRIPTION
  Every existing fixture was written by one script through one mkfs/ntfs-3g
  path. However large that corpus grows it remains a single sample: a
  systematic blind spot that Microsoft's NTFS driver produces and ntfs-3g never
  does - an $ATTRIBUTE_LIST layout, an index-allocation pattern, a
  resident-attribute threshold - cannot be surfaced by it at any size, because
  the errors are correlated.

  This creates a fixed-format VHD, formats it with the real Windows NTFS
  driver, populates it with the same corpus, deletes the same subset, and
  writes the same expected.json shape. Any disagreement between parsing this
  and parsing ntfs-basic.img is a genuine finding about the parser rather than
  about the fixture generator.

  A *fixed* VHD is used deliberately: its payload is a raw disk image from byte
  zero with only a 512-byte footer appended, so trimming the footer leaves an
  ordinary raw image and rc-device needs no VHD support at all.

  Requires Administrator: creating and attaching a virtual disk is privileged.

.EXAMPLE
  .\scripts\make-windows-fixture.ps1
#>

[CmdletBinding()]
param(
    [int]$SizeMB = 512,
    [string]$Letter = 'Y'
)

$ErrorActionPreference = 'Stop'
function Fail($m) { Write-Host "ERROR: $m" -ForegroundColor Red; exit 1 }
function Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }

# Windows MAX_PATH is 260 characters and the corpus deliberately contains a
# 250-character filename, so even a moderately deep directory pushes the full
# path past the limit. Get-ChildItem will happily *enumerate* such a path while
# Copy-Item cannot open it, which is exactly how the first run of this script
# failed. The \\?\ extended-length prefix lifts the limit, and the .NET file
# APIs honour it reliably where the PowerShell cmdlets do not - so every file
# operation below goes through .NET rather than through Copy-Item/Remove-Item.
function Ext([string]$p) {
    $full = [System.IO.Path]::GetFullPath($p)
    if ($full.StartsWith('\\?\')) { return $full }
    if ($full.StartsWith('\\'))   { return '\\?\UNC\' + $full.Substring(2) }
    return '\\?\' + $full
}

$admin = ([Security.Principal.WindowsPrincipal] `
          [Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Fail "Must run from an Administrator PowerShell (creating a VHD is privileged)." }

$repo     = Split-Path -Parent $PSScriptRoot
$fixtures = Join-Path $repo 'testdata\fixtures'
$vhd      = Join-Path $fixtures 'ntfs-windows.vhd'
$img      = Join-Path $fixtures 'ntfs-windows.img'
$corpusPy = Join-Path $repo 'testdata\corpus\make_corpus.py'
$work     = Join-Path $env:TEMP 'rc-wincorpus'

if (-not (Test-Path $corpusPy)) { Fail "corpus generator not found at $corpusPy" }
$py = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $py) { $py = (Get-Command python3 -ErrorAction SilentlyContinue).Source }
if (-not $py) { Fail "python not found on PATH" }

New-Item -ItemType Directory -Force -Path $fixtures | Out-Null

# --- 1. corpus -------------------------------------------------------------
Step "Generating corpus with $py"
if (Test-Path (Ext $work)) { [System.IO.Directory]::Delete((Ext $work), $true) }
$manifest = Join-Path $env:TEMP 'rc-wincorpus.json'
& $py $corpusPy $work > $manifest
if ($LASTEXITCODE -ne 0) { Fail "corpus generation failed" }
$deleted = @(& $py $corpusPy --deleted-set)
Write-Host "    corpus written, $($deleted.Count) files marked for deletion"

# --- 2. create and attach a fixed VHD -------------------------------------
Step "Creating a ${SizeMB}MB fixed VHD"
foreach ($p in @($vhd, $img)) { if (Test-Path $p) { Remove-Item -Force $p } }
$expected = Join-Path $fixtures 'ntfs-windows.expected.json'
if (Test-Path $expected) { Remove-Item -Force $expected }

$dp = Join-Path $env:TEMP 'rc-mkvhd.txt'
@"
create vdisk file="$vhd" maximum=$SizeMB type=fixed
select vdisk file="$vhd"
attach vdisk
create partition primary
format fs=ntfs quick label=RCWINNTFS
assign letter=$Letter
"@ | Set-Content -Path $dp -Encoding ascii
diskpart /s $dp | Out-String | Write-Verbose
if (-not (Test-Path "${Letter}:\")) { Fail "VHD did not mount at ${Letter}:" }
Write-Host "    mounted at ${Letter}: and formatted NTFS by the Windows driver"

try {
    # --- 3. populate ------------------------------------------------------
    Step "Copying the corpus onto the volume"
    $workExt   = Ext $work
    $srcFiles  = @([System.IO.Directory]::EnumerateFiles($workExt, '*', 'AllDirectories'))
    $prefixLen = $workExt.Length + 1
    foreach ($src in $srcFiles) {
        $rel = $src.Substring($prefixLen)
        $dst = Ext (Join-Path "${Letter}:\" $rel)
        [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($dst)) | Out-Null
        [System.IO.File]::Copy($src, $dst, $true)
    }
    Write-Host "    copied $($srcFiles.Count) files"

    # Confirm the names survived before recording ground truth about them.
    $volRoot = Ext "${Letter}:\"
    $onDisk = @([System.IO.Directory]::EnumerateFiles($volRoot, '*', 'AllDirectories') |
        ForEach-Object { $_.Substring($volRoot.Length).TrimStart('\').Replace('\', '/') })
    $wanted = @($srcFiles | ForEach-Object { $_.Substring($prefixLen).Replace('\', '/') })
    $missing = Compare-Object $wanted $onDisk | Where-Object SideIndicator -eq '<='
    if ($missing) {
        Fail "these corpus files are not on the volume under their exact names:`n$($missing.InputObject -join "`n")"
    }
    Write-Host "    verified $($wanted.Count) files present under their exact names"

    # --- 4. delete the same subset ----------------------------------------
    Step "Deleting $($deleted.Count) files"
    $gone = 0
    foreach ($rel in $deleted) {
        $p = Ext (Join-Path "${Letter}:\" ($rel -replace '/', '\'))
        if ([System.IO.File]::Exists($p)) { [System.IO.File]::Delete($p); $gone++ }
        else { Write-Host "    warning: $rel was not present" -ForegroundColor Yellow }
    }
    Write-Host "    deleted $gone files"
}
finally {
    # --- 5. detach --------------------------------------------------------
    Step "Detaching"
    $dp2 = Join-Path $env:TEMP 'rc-rmvhd.txt'
    @"
select vdisk file="$vhd"
detach vdisk
"@ | Set-Content -Path $dp2 -Encoding ascii
    diskpart /s $dp2 | Out-String | Write-Verbose
}

# --- 6. present it as a plain image ---------------------------------------
Step "Trimming the VHD footer to leave a raw image"
$fs = [System.IO.File]::Open($vhd, 'Open', 'ReadWrite')
$fs.SetLength($fs.Length - 512)
$fs.Close()
Move-Item -LiteralPath $vhd -Destination $img -Force

# --- 7. ground truth -------------------------------------------------------
Step "Writing expected.json"
$env:RC_MANIFEST = $manifest
$env:RC_IMG      = $img
$env:RC_DELETED  = ($deleted -join "`n")
$gt = Join-Path $env:TEMP 'rc-groundtruth.py'
@'
import hashlib, json, os, datetime
manifest = json.load(open(os.environ["RC_MANIFEST"], encoding="utf-8"))
deleted = set(x for x in os.environ["RC_DELETED"].split("\n") if x)
img = os.environ["RC_IMG"]
h = hashlib.sha256()
with open(img, "rb") as fh:
    while True:
        b = fh.read(8 << 20)
        if not b:
            break
        h.update(b)
files = {p: {"sha256": m["sha256"], "size": m["size"], "kind": m["kind"],
             "state": "deleted" if p in deleted else "present"}
         for p, m in manifest.items()}
doc = {
    "fixture": "ntfs-windows",
    "filesystem": "ntfs",
    "image": os.path.basename(img),
    "image_sha256": h.hexdigest(),
    "image_bytes": os.path.getsize(img),
    "sector_bytes": 512,
    "cluster_bytes": 4096,
    "partitioned": True,
    "generator": {
        "script": "make-windows-fixture.ps1",
        "version": 1,
        "built_utc": datetime.datetime.now(datetime.timezone.utc)
                     .replace(microsecond=0).isoformat(),
        "driver": "Microsoft NTFS (independent of mkfs.ntfs/ntfs-3g)",
    },
    "notes": ("Formatted and populated by the Windows NTFS driver rather than "
              "mkfs.ntfs/ntfs-3g, as a sample independent of the other "
              "fixtures. Disagreement with ntfs-basic.img indicates a parser "
              "problem rather than a fixture one. This image is PARTITIONED: "
              "the volume starts at the partition offset, not at LBA 0."),
    "files": files,
    "expect": {
        "deleted_count": sum(1 for f in files.values() if f["state"] == "deleted"),
        "present_count": sum(1 for f in files.values() if f["state"] == "present"),
    },
}
out = img[:-4] + ".expected.json"
with open(out, "w", encoding="utf-8") as fh:
    json.dump(doc, fh, indent=2, sort_keys=True)
print("    deleted=%d present=%d" % (doc["expect"]["deleted_count"],
                                     doc["expect"]["present_count"]))
'@ | Set-Content -Path $gt -Encoding utf8
& $py $gt
Remove-Item Env:\RC_MANIFEST, Env:\RC_IMG, Env:\RC_DELETED -ErrorAction SilentlyContinue

if (Test-Path (Ext $work)) { [System.IO.Directory]::Delete((Ext $work), $true) }
Write-Host ""
Write-Host "Done: $img" -ForegroundColor Green
Write-Host "This fixture is PARTITIONED, so rc-partition must locate the volume first."
