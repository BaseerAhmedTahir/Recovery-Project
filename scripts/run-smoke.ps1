<#
.SYNOPSIS
  Run the rc-device hardware smoke test against a removable drive.

.DESCRIPTION
  This is the one check that exercises the Windows raw sector-read path:
  FILE_FLAG_NO_BUFFERING, the OVERLAPPED offset plumbing, and the bounce
  buffer that handles a caller-supplied buffer whose address is not
  sector-aligned. None of that is reachable from the file-backed fixtures, and
  it is exactly the kind of code that compiles cleanly and fails at runtime.

  The script only ever READS. It refuses to run against a fixed disk, and
  rc.exe itself refuses the system drive and refuses to start without the
  RC_SMOKE_ALLOW opt-in.

  Requires an Administrator shell: reading raw sectors from a physical device
  is a privileged operation on Windows.

.EXAMPLE
  # From an Administrator PowerShell, with a USB stick or SD card inserted:
  .\scripts\run-smoke.ps1
#>

[CmdletBinding()]
param(
    # Skip the interactive confirmation.
    [switch]$Yes,
    # Target a specific drive index instead of auto-detecting.
    [int]$DriveIndex = -1
)

$ErrorActionPreference = 'Stop'

function Fail($msg) { Write-Host "ERROR: $msg" -ForegroundColor Red; exit 1 }

# --- must be elevated ------------------------------------------------------
$admin = ([Security.Principal.WindowsPrincipal] `
          [Security.Principal.WindowsIdentity]::GetCurrent()
         ).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Fail @"
This must run from an Administrator PowerShell.
Reading raw sectors from a physical device is privileged on Windows.

Right-click PowerShell -> Run as administrator, then re-run this script.
"@
}

# --- locate the binary -----------------------------------------------------
$repo = Split-Path -Parent $PSScriptRoot
$rc = Join-Path $repo 'target\debug\rc.exe'
if (-not (Test-Path $rc)) {
    Write-Host "Building rc.exe..." -ForegroundColor Cyan
    Push-Location $repo
    $env:RUSTUP_HOME = 'D:\Rust\rustup'
    $env:CARGO_HOME  = 'D:\Rust\cargo'
    $env:PATH        = "D:\Rust\cargo\bin;$env:PATH"
    cargo build -p rc-cli
    Pop-Location
}
if (-not (Test-Path $rc)) { Fail "rc.exe not found at $rc" }

# --- find a removable target ----------------------------------------------
# Deliberately restrictive. The whole point is to avoid pointing a raw-device
# read at something that matters, so anything not clearly removable and
# clearly non-empty is excluded rather than merely warned about.
$candidates = Get-CimInstance Win32_DiskDrive | Where-Object {
    $_.Size -gt 0 -and ($_.InterfaceType -eq 'USB' -or $_.MediaType -match 'Removable')
}

if ($DriveIndex -ge 0) {
    $candidates = Get-CimInstance Win32_DiskDrive | Where-Object { $_.Index -eq $DriveIndex }
    if (-not $candidates) { Fail "No disk with index $DriveIndex" }
}

if (-not $candidates) {
    $readers = Get-CimInstance Win32_DiskDrive |
        Where-Object { $_.InterfaceType -eq 'USB' -and $_.Size -eq 0 }
    if ($readers) {
        Fail @"
A USB card reader is present but reports zero bytes, which means no card is
inserted:

$($readers | ForEach-Object { "  index $($_.Index)  $($_.Model)" } | Out-String)
Insert an SD card or a USB stick you do not mind reading from, then re-run.
"@
    }
    Fail @"
No removable drive found.

Insert a USB stick or SD card and re-run. The test only reads a handful of
sectors, but point it at something disposable rather than your only copy of
anything.
"@
}

Write-Host ""
Write-Host "Candidate removable drives:" -ForegroundColor Cyan
$candidates | Select-Object Index, Model, InterfaceType,
    @{n = 'GB'; e = { [math]::Round($_.Size / 1GB, 2) } } |
    Format-Table -AutoSize

$target = $candidates | Select-Object -First 1
$devPath = "\\.\PhysicalDrive$($target.Index)"

# --- refuse anything holding a system or paging volume ---------------------
$sysDisk = (Get-Partition | Where-Object DriveLetter -eq $env:SystemDrive.TrimEnd(':') |
            Select-Object -First 1).DiskNumber
if ($target.Index -eq $sysDisk) {
    Fail "Disk $($target.Index) holds the system volume. Refusing."
}

Write-Host "Target: $devPath  ($($target.Model))" -ForegroundColor Yellow
Write-Host "This READS a few sectors. Nothing is written." -ForegroundColor Yellow

if (-not $Yes) {
    $ans = Read-Host "Proceed? [y/N]"
    if ($ans -notmatch '^[Yy]') { Write-Host "Aborted."; exit 0 }
}

# --- run it ----------------------------------------------------------------
$env:RC_SMOKE_ALLOW = '1'
Write-Host ""
& $rc smoke --device $devPath
$code = $LASTEXITCODE
Remove-Item Env:\RC_SMOKE_ALLOW -ErrorAction SilentlyContinue

Write-Host ""
if ($code -eq 0) {
    Write-Host "SMOKE TEST PASSED." -ForegroundColor Green
    Write-Host "Record this in docs/LIMITATIONS.md section 1.1 and the Milestone 8 gate is clear."
} else {
    Write-Host "SMOKE TEST FAILED (exit $code)." -ForegroundColor Red
    Write-Host "This is the failure worth having now rather than after Milestone 3."
}
exit $code
