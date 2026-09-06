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

# Every NTFS volume Windows formats gets a System Volume Information directory
# whose ACL denies even Administrator, and .NET's AllDirectories enumeration
# aborts the entire walk on the first directory it cannot open rather than
# skipping it. So walk the tree explicitly. Denials are collected rather than
# swallowed: a denial we did not expect is a finding, not noise.
function WalkFiles([string]$root, [ref]$denied) {
    $out   = [System.Collections.Generic.List[string]]::new()
    $queue = [System.Collections.Generic.Queue[string]]::new()
    $queue.Enqueue($root)
    while ($queue.Count -gt 0) {
        $dir = $queue.Dequeue()
        try {
            foreach ($f in [System.IO.Directory]::EnumerateFiles($dir))       { $out.Add($f) }
            foreach ($d in [System.IO.Directory]::EnumerateDirectories($dir)) { $queue.Enqueue($d) }
        } catch [System.UnauthorizedAccessException] {
            $denied.Value.Add($dir) | Out-Null
        }
    }
    return $out.ToArray()
}

# Metadata Windows creates on its own. Not part of the corpus, and not counted
# as recovered files - but they are part of what makes this fixture
# independent, because ntfs-3g never produces them.
$WindowsArtifacts = @('System Volume Information', '$RECYCLE.BIN', '$Extend')

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
# The corpus contains CJK, Cyrillic, Arabic and emoji filenames, and
# make_corpus.py writes --deleted-set as raw UTF-8 bytes for that reason. But
# PowerShell decodes a native process's stdout using [Console]::OutputEncoding,
# so a console that is not UTF-8 mangles those names silently: File.Exists()
# then returns false, the file is never deleted, and expected.json claims a
# deletion that never happened. Pin the encoding on both sides, and verify the
# round trip below rather than trusting it.
$env:PYTHONIOENCODING = 'utf-8'
$env:PYTHONUTF8 = '1'
$prevOutEnc = [Console]::OutputEncoding
try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false } catch {}

Step "Generating corpus with $py"
if (Test-Path (Ext $work)) { [System.IO.Directory]::Delete((Ext $work), $true) }
$manifest = Join-Path $env:TEMP 'rc-wincorpus.json'
& $py $corpusPy $work > $manifest
if ($LASTEXITCODE -ne 0) { Fail "corpus generation failed" }
$deleted = @(& $py $corpusPy --deleted-set)
try { [Console]::OutputEncoding = $prevOutEnc } catch {}

# Every name we intend to delete must appear verbatim among the names the
# generator says it wrote. If the two disagree, an encoding boundary corrupted
# something and the ground truth would be a lie - fail here, not silently.
$manifestDoc = [System.IO.File]::ReadAllText($manifest) | ConvertFrom-Json
$known = @{}
foreach ($n in $manifestDoc.PSObject.Properties.Name) { $known[$n] = $true }
$unknown = @($deleted | Where-Object { -not $known.ContainsKey($_) })
if ($unknown) {
    Fail "these names are in the deleted set but not in the corpus manifest, which means a text-encoding boundary corrupted them:`n$($unknown -join "`n")"
}
Write-Host "    corpus written: $($known.Count) files, $($deleted.Count) marked for deletion (names round-trip cleanly)"

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
    $srcDenied = [System.Collections.Generic.List[string]]::new()
    $srcFiles  = @(WalkFiles $workExt ([ref]$srcDenied))
    if ($srcDenied.Count) { Fail "could not read these corpus directories:`n$($srcDenied -join "`n")" }
    $prefixLen = $workExt.Length + 1
    foreach ($src in $srcFiles) {
        $rel = $src.Substring($prefixLen)
        $dst = Ext (Join-Path "${Letter}:\" $rel)
        [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($dst)) | Out-Null
        [System.IO.File]::Copy($src, $dst, $true)
    }
    Write-Host "    copied $($srcFiles.Count) files"

    # Confirm the names survived before recording ground truth about them.
    $volRoot   = Ext "${Letter}:\"
    $volDenied = [System.Collections.Generic.List[string]]::new()
    $onDisk    = @(WalkFiles $volRoot ([ref]$volDenied) |
        ForEach-Object { $_.Substring($volRoot.Length).TrimStart('\').Replace('\', '/') })

    # Only Windows own metadata directories may be unreadable. Anything else
    # denied means the corpus did not land the way we think it did.
    $unexpected = @($volDenied | Where-Object {
        $top = $_.Substring($volRoot.Length).TrimStart('\').Split('\')[0]
        $WindowsArtifacts -notcontains $top
    })
    if ($unexpected) {
        Fail "unexpected access denial under ${Letter}: `n$($unexpected -join "`n")"
    }
    if ($volDenied.Count) {
        $names = ($volDenied | ForEach-Object { $_.Substring($volRoot.Length) }) -join ', '
        Write-Host "    skipped $($volDenied.Count) Windows metadata dir(s): $names"
    }

    $wanted  = @($srcFiles | ForEach-Object { $_.Substring($prefixLen).Replace('\', '/') })
    $missing = @(Compare-Object $wanted $onDisk | Where-Object SideIndicator -eq '<=')
    if ($missing) {
        Fail "these corpus files are not on the volume under their exact names:`n$($missing.InputObject -join "`n")"
    }
    Write-Host "    verified $($wanted.Count) files present under their exact names"

    # Report what Windows added that ntfs-3g would not have. The parser will
    # see these MFT records; they should surface as allocated files and must
    # not be mistaken for corpus entries.
    $extra = @(Compare-Object $wanted $onDisk | Where-Object SideIndicator -eq '=>')
    if ($extra) {
        Write-Host "    plus $($extra.Count) file(s) created by Windows itself:"
        foreach ($e in $extra) { Write-Host "      $($e.InputObject)" }
    }

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
# PowerShell's > redirection writes UTF-8 *with* a BOM, which json.load
# rejects as a stray character before the opening brace. utf-8-sig strips
# it when present and is a no-op when it is not.
manifest = json.load(open(os.environ["RC_MANIFEST"], encoding="utf-8-sig"))
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
              "the volume starts at the partition offset, not at LBA 0. "
              "There is no 'fragmented_file' key: this generator does not "
              "force fragmentation, so use ntfs-basic.img for that case."),
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
