# Build the companion app into packaging\dist.
#
#   powershell -ExecutionPolicy Bypass -File packaging\build-android.ps1 [-Release]
#
# Without -Release this makes a debug-signed APK, which installs on your own
# phone after you allow installing from this source. -Release makes an
# *unsigned* release APK: Android will not install it until it is signed with
# your own key (`apksigner`), because there is no key in this repository and
# there should not be.
#
# Requires: the Android SDK (ANDROID_HOME or the default location) and a JDK.
# Gradle downloads its dependencies on the first run; the app itself has no
# network code beyond the USB bridge, which `cargo xtask audit-android`
# enforces and this script runs first.

param([switch]$Release)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$dist = Join-Path $root "packaging\dist"
New-Item -ItemType Directory -Force $dist | Out-Null

if (-not $env:ANDROID_HOME) {
    $sdk = Join-Path $env:LOCALAPPDATA "Android\Sdk"
    if (Test-Path $sdk) { $env:ANDROID_HOME = $sdk } else { throw "set ANDROID_HOME to your Android SDK" }
}
# Gradle's caches and temporary files are large; keep them off a full C:.
if ((Get-PSDrive C).Free -lt 10GB) {
    $onD = Split-Path -Qualifier $root
    if (-not $env:GRADLE_USER_HOME) { $env:GRADLE_USER_HOME = Join-Path $onD "\gradle-home" }
    $tmp = Join-Path $root "target\tmp-build"
    New-Item -ItemType Directory -Force $tmp, $env:GRADLE_USER_HOME | Out-Null
    $env:TMP = $tmp; $env:TEMP = $tmp
    $env:GRADLE_OPTS = "-Djava.io.tmpdir=$tmp"
    Write-Host "  C: is nearly full; using GRADLE_USER_HOME=$env:GRADLE_USER_HOME" -ForegroundColor Yellow
}

Write-Host "== audit ==" -ForegroundColor Cyan
Push-Location $root
cargo xtask audit-android
if ($LASTEXITCODE -ne 0) { throw "the companion-app audit failed; nothing was built" }
Pop-Location

Write-Host "== unit tests (including the bridge protocol golden file) ==" -ForegroundColor Cyan
Push-Location (Join-Path $root "companion-android")
gradle --console=plain :app:testDebugUnitTest
if ($LASTEXITCODE -ne 0) { throw "the companion app's unit tests failed" }

$task = if ($Release) { ":app:assembleRelease" } else { ":app:assembleDebug" }
Write-Host "== $task ==" -ForegroundColor Cyan
gradle --console=plain $task
if ($LASTEXITCODE -ne 0) { throw "the APK build failed" }
Pop-Location

$apk = Get-ChildItem -Recurse -Filter "*.apk" `
    (Join-Path $root "companion-android\app\build\outputs\apk") | Select-Object -First 1
if (-not $apk) { throw "no APK was produced" }
$name = if ($Release) { "recovery-companion-release-unsigned.apk" } else { "recovery-companion-debug.apk" }
Copy-Item $apk.FullName (Join-Path $dist $name)

Write-Host ""
Write-Host "Done: $(Join-Path $dist $name)" -ForegroundColor Green
if ($Release) {
    Write-Host "This APK is unsigned. Sign it with your own key before installing:" -ForegroundColor Yellow
    Write-Host "  apksigner sign --ks <your.keystore> $(Join-Path $dist $name)" -ForegroundColor Yellow
} else {
    Write-Host "Install it with: adb install -r `"$(Join-Path $dist $name)`"" -ForegroundColor Green
}
