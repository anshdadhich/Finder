# Publishes a Finder update to users.
#
#  1. Bump "version" in tauri.conf.json (e.g. 0.1.0 -> 0.2.0).
#  2. Optionally drop release notes into notes.txt (repo root).
#  3. Run this script. It builds the NSIS installer, signs it with your
#     updater key (kept in ~\.tauri — never committed), and writes latest.json.
#  4. Create a GitHub release tagged v<version> and upload BOTH the installer
#     and latest.json to it. Users' apps then offer the update on next check.
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$conf = Get-Content (Join-Path $repoRoot "tauri.conf.json") -Raw | ConvertFrom-Json
$version = $conf.package.version

$keyDir = Join-Path $env:USERPROFILE ".tauri"
$key = Join-Path $keyDir "fastseek.key"
if (-not (Test-Path $key)) {
  throw "signing key not found at $key - generate one with: cargo tauri signer generate"
}
$passwordFile = Join-Path $keyDir "fastseek.key.password"
if (-not (Test-Path $passwordFile)) {
  throw "key password not found at $passwordFile"
}
$password = (Get-Content $passwordFile -Raw).Trim()

$env:TAURI_SIGNING_PRIVATE_KEY_PATH = $key
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $password
$env:TAURI_PRIVATE_KEY_PATH = $key
$env:TAURI_PRIVATE_KEY_PASSWORD = $password

Write-Host "==> Building NSIS installer (v$version)..."
cargo tauri build --bundles nsis --features embed-resources
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$installer = Get-ChildItem (Join-Path $repoRoot "target\release\bundle\nsis\*_x64-setup.exe") -ErrorAction Stop |
  Sort-Object LastWriteTime -Descending | Select-Object -First 1

Write-Host "==> Signing $($installer.Name)..."
cargo tauri signer sign -f $key -p $password $installer.FullName
if ($LASTEXITCODE -ne 0) { throw "signing failed" }

$sigPath = "$($installer.FullName).sig"
if (-not (Test-Path $sigPath)) { throw "no signature written at $sigPath" }
$signature = (Get-Content $sigPath | Where-Object { $_ -and -not $_.StartsWith("untrusted comment") } |
  Select-Object -Last 1).Trim()
if (-not $signature) { throw "empty signature in $sigPath" }

$notes = ""
if (Test-Path (Join-Path $repoRoot "notes.txt")) {
  $notes = (Get-Content (Join-Path $repoRoot "notes.txt") -Raw).Trim()
}

$manifest = [ordered]@{
  version  = $version
  notes    = $notes
  pub_date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH\:mm\:ss.fffZ")
  platforms = @{
    "windows-x86_64" = @{
      signature = $signature
      url       = "https://github.com/anshdadhich/Finder/releases/download/v$version/$($installer.Name)"
    }
  }
}

$manifestPath = Join-Path $repoRoot "latest.json"
$manifest | ConvertTo-Json -Depth 4 | Set-Content -Path $manifestPath -Encoding utf8

Write-Host ""
Write-Host "Done. Create a GitHub release tagged v$version and upload:"
Write-Host "  1. $($installer.FullName)"
Write-Host "  2. $manifestPath"
Write-Host "Installed apps will offer v$version on their next update check."
