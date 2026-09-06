<#
.SYNOPSIS
  Build an NTFS fixture using Microsoft's own NTFS driver, as an independent
  sample against the ntfs-3g-built fixtures.

.DESCRIPTION
  Every existing fixture was written by one script through one mkfs/ntfs-3g
  path. However large that corpus grows, it is a single sample: a systematic
  blind spot that Microsoft's NTFS driver produces and ntfs-3g never does -
  an $ATTRIBUTE_LIST layout, an index-allocation pattern, a resident-attribute
  threshold - cannot be surfaced by it at any size, because the errors are
  correlated.

  This creates a fixed-format VHD, formats it with the real Windows NTFS
  driver, populates it with the same corpus, deletes the same subset, and
  writes the same expected.json shape. Any disagreement between parsing this
  and parsing ntfs-basic.img is a genuine finding about the parser rather than
  about the fixture generator.

  A *fixed* VHD is used deliberately: its payload is raw disk image from byte
  zero, with only a 512-byte footer appended, so rc-device can open it as an
  ordinary image with no VHD support at all.

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

$admin = ([Security.Principal.WindowsPrincipal] `
          [Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Fail "Must run from an Administrator PowerShell (creating a VHD is privileged)." }

$repo     = Split-Path -Parent $PSScriptRoot
$fixtures = Join-Path $repo 'testdata\fixtures'
$vhd      = Join-Path $fixtures 'ntfs-windows.vhd'
$img      = Join-Path $fixtures 'ntfs-windows.img'
$expected = Join-Path $fixtures 'ntfs-windows.expected.json'
$corpusPy = Join-Path $repo 'testdata\corpus\make_corpus.py'
$work     = Join-Path $env:TEMP 'rc-wincorpus'

if (-not (Test-Path $corpusPy)) { Fail "corpus generator not found at $corpusPy" }
$py = (Get-Command python -ErrorAction SilentlyContinue).Source
if (-not $py) { $py = (Get-Command python3 -ErrorAction SilentlyContinue).Source }
if (-not $py) { Fail "python not found on PATH" }

New-Item -ItemType Directory -Force -Path $fixtures | Out-Null

# --- 1. corpus -------------------------------------------------------------
Step "Generating corpus with $py"
if (Test-Path $work) { Remove-Item -Recurse -Force -LiteralPath "\\?\$work" }
$manifest = Join-Path $env:TEMP 'rc-wincorpus.json'
& $py $corpusPy $work > $manifest
if ($LASTEXITCODE -ne 0) { Fail "corpus generation failed" }
$deleted = & $py $corpusPy --deleted-set
Write-Host "    corpus written, $($deleted.Count) files marked for deletion"

# --- 2. create and attach a fixed VHD -------------------------------------
Step "Creating a ${SizeMB}MB fixed VHD"
foreach ($p in @($vhd, $img, $expected)) { if (Test-Path $p) { Remove-Item -Force $p } }

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
    Get-ChildItem -Recurse -File -LiteralPath $work | ForEach-Object {
        $rel = $_.FullName.Substring($work.Length + 1)
        $dst = Join-Path "${Letter}:\" $rel
        $dir = Split-Path -Parent $dst
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        Copy-Item -LiteralPath $_.FullName -Destination $dst -Force
    }

    # Confirm the names survived before recording ground truth about them.
    $onDisk = Get-ChildItem -Recurse -File -LiteralPath "${Letter}:\" |
        ForEach-Object { $_.FullName.Substring(3).Replace('\', '/') }
    $wanted = Get-ChildItem -Recurse -File -LiteralPath $work |
        ForEach-Object { $_.FullName.Substring($work.Length + 1).Replace('\', '/') }
    $missing = Compare-Object $wanted $onDisk | Where-Object SideIndicator -eq '<='
    if ($missing) {
        Fail "these corpus files are not on the volume under their exact names:`n$($missing.InputObject -join "`n")"
    }
    Write-Host "    verified $($wanted.Count) files present under their exact names"

    # --- 4. delete the same subset ----------------------------------------
    Step "Deleting $($deleted.Count) files"
    foreach ($rel in $deleted) {
        $p = Join-Path "${Letter}:\" ($rel -replace '/', '\')
        if (Test-Path -LiteralPath $p) { Remove-Item -LiteralPath $p -Force }
        else { Write-Host "    warning: $rel was not present" -ForegroundColor Yellow }
    }
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
# A fixed VHD is raw image plus a 512-byte footer. Trimming the footer leaves
# an ordinary raw image, so nothing downstream needs to know about VHD at all.
Step "Trimming the VHD footer to leave a raw image"
$fs = [System.IO.File]::Open($vhd, 'Open', 'ReadWrite')
$len = $fs.Length
$fs.SetLength($len - 512)
$fs.Close()
Move-Item -LiteralPath $vhd -Destination $img -Force

# --- 7. ground truth -------------------------------------------------------
Step "Writing expected.json"
$env:RC_MANIFEST = $manifest
$env:RC_IMG      = $img
$env:RC_DELETED  = ($deleted -join "`n")
& $py -c @"
import hashlib, json, os, sys, datetime
manifest = json.load(open(os.environ['RC_MANIFEST'], encoding='utf-8'))
deleted = set(x for x in os.environ['RC_DELETED'].split('\n') if x)
img = os.environ['RC_IMG']
h = hashlib.sha256()
with open(img, 'rb') as fh:
    while True:
        b = fh.read(8 << 20)
        if not b: break
        h.update(b)
files = {p: {'sha256': m['sha256'], 'size': m['size'], 'kind': m['kind'],
             'state': 'deleted' if p in deleted else 'present'}
         for p, m in manifest.items()}
doc = {
  'fixture': 'ntfs-windows',
  'filesystem': 'ntfs',
  'image': os.path.basename(img),
  'image_sha256': h.hexdigest(),
  'image_bytes': os.path.getsize(img),
  'sector_bytes': 512,
  'cluster_bytes': 4096,
  'partitioned': True,
  'generator': {'script': 'make-windows-fixture.ps1', 'version': 1,
                'built_utc': datetime.datetime.now(datetime.timezone.utc)
                             .replace(microsecond=0).isoformat(),
                'driver': 'Microsoft NTFS (independent of ntfs-3g)'},
  'notes': ('Formatted and populated by the Windows NTFS driver rather than '
            'mkfs.ntfs/ntfs-3g, as an independent sample. Disagreement with '
            'ntfs-basic.img indicates a parser problem, not a fixture one. '
            'Partitioned: the volume starts at the partition offset, not LBA 0.'),
  'files': files,
  'expect': {'deleted_count': sum(1 for f in files.values() if f['state'] == 'deleted'),
             'present_count': sum(1 for f in files.values() if f['state'] == 'present')},
}
json.dump(doc, open(os.environ['RC_IMG'].replace('.img', '.expected.json'), 'w',
                    encoding='utf-8'), indent=2, sort_keys=True)
print('    deleted=%d present=%d' % (doc['expect']['deleted_count'],
                                     doc['expect']['present_count']))
"@
Remove-Item Env:\RC_MANIFEST, Env:\RC_IMG, Env:\RC_DELETED -ErrorAction SilentlyContinue

Remove-Item -Recurse -Force -LiteralPath "\\?\$work" -ErrorAction SilentlyContinue
Write-Host ""
Write-Host "Done: $img" -ForegroundColor Green
Write-Host "This fixture is PARTITIONED, so rc-partition must locate the volume first."
