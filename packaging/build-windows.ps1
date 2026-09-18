# Build everything for Windows into packaging\dist.
#
#   powershell -ExecutionPolicy Bypass -File packaging\build-windows.ps1
#
# Produces:
#   dist\RECOVERY-CORE\rc.exe        the command line (with the USB bridge)
#   dist\RECOVERY-CORE\rc-gui.exe    the desktop app (finds rc.exe beside it)
#   dist\RECOVERY-CORE\docs\         LIMITATIONS, PROGRESS, BRIDGE
#   dist\RECOVERY-CORE-setup.exe     the installer, when Tauri can bundle one
#
# Nothing in the result contacts a network. Two things are deliberately not
# bundled (see packaging\README.md): ffmpeg, for video previews, and Android
# platform-tools (adb). Drop either beside rc.exe and it is found.
#
# The build itself downloads crates, npm packages and the NSIS bundler, so the
# machine that builds needs internet once. The result does not.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dist = Join-Path $root "packaging\dist"
$out = Join-Path $dist "RECOVERY-CORE"

# Keep large temporary files off a small system drive.
if (-not $env:TMP -or (Get-PSDrive C).Free -lt 5GB) {
    $tmp = Join-Path $root "target\tmp-build"
    New-Item -ItemType Directory -Force $tmp | Out-Null
    $env:TMP = $tmp
    $env:TEMP = $tmp
}

Write-Host "== engine and command line ==" -ForegroundColor Cyan
Push-Location $root
cargo build --release -p rc-cli --features bridge
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Write-Host "== audits (must pass before anything is packaged) ==" -ForegroundColor Cyan
cargo xtask audit
if ($LASTEXITCODE -ne 0) { throw "the offline audit failed; nothing was packaged" }
Pop-Location

Write-Host "== desktop app ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "gui")
if (-not (Test-Path "node_modules")) { npm install --no-audit --no-fund }
npm run build
if ($LASTEXITCODE -ne 0) { throw "the frontend build failed" }
Push-Location "src-tauri"
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "the GUI build failed" }
Pop-Location
Pop-Location

Write-Host "== collecting ==" -ForegroundColor Cyan
Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $out "docs") | Out-Null
Copy-Item (Join-Path $root "target\release\rc.exe") $out
Copy-Item (Join-Path $root "gui\src-tauri\target\release\rc-gui.exe") $out
Copy-Item (Join-Path $root "docs\LIMITATIONS.md") (Join-Path $out "docs")
Copy-Item (Join-Path $root "docs\PROGRESS.md") (Join-Path $out "docs")
Copy-Item (Join-Path $root "docs\BRIDGE.md") (Join-Path $out "docs")
Copy-Item (Join-Path $root "packaging\README.md") $out

# Optional extras, if this machine has them.
$adb = Join-Path $env:LOCALAPPDATA "Android\Sdk\platform-tools\adb.exe"
if (Test-Path $adb) {
    New-Item -ItemType Directory -Force (Join-Path $out "platform-tools") | Out-Null
    Copy-Item (Split-Path $adb) -Destination (Join-Path $out "platform-tools") -Recurse -Force
    Write-Host "  bundled the adb from this machine's Android SDK" -ForegroundColor Green
} else {
    Write-Host "  no adb found; phone features need Android platform-tools (see README)" -ForegroundColor Yellow
}
$ffmpeg = (Get-Command ffmpeg -ErrorAction SilentlyContinue).Source
if ($ffmpeg) {
    Copy-Item $ffmpeg $out
    Write-Host "  bundled the ffmpeg from this machine's PATH" -ForegroundColor Green
} else {
    Write-Host "  no ffmpeg found; video previews will say so (see README)" -ForegroundColor Yellow
}

Write-Host "== installer ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "gui")
npx tauri build --no-bundle 2>&1 | Out-Null   # ensure the release binary is current
npx tauri build 2>&1 | Tee-Object -Variable bundleLog | Out-Null
$setup = Get-ChildItem -Recurse -Filter "*-setup.exe" `
    (Join-Path $root "gui\src-tauri\target\release\bundle") -ErrorAction SilentlyContinue |
    Select-Object -First 1
Pop-Location
if ($setup) {
    Copy-Item $setup.FullName (Join-Path $dist "RECOVERY-CORE-setup.exe")
    Write-Host "  installer: $(Join-Path $dist 'RECOVERY-CORE-setup.exe')" -ForegroundColor Green
} else {
    Write-Host "  no installer was produced (Tauri's NSIS bundler needs to download NSIS once)." -ForegroundColor Yellow
    Write-Host "  The folder in dist\RECOVERY-CORE runs as it is - copy it anywhere." -ForegroundColor Yellow
}

Write-Host ""
Write-Host "Done: $out" -ForegroundColor Green
Get-ChildItem $out | Select-Object Name, Length | Format-Table
