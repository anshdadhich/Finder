# Uploads the freshly built installer to a GitHub release (v<version>) and
# regenerates the winget manifests for that version.
#   - Reads GITHUB_TOKEN from the environment.
#   - uploads: the NSIS installer, its .sig, and latest.json
#   - computes the installer SHA256 into the winget installer manifest
$ErrorActionPreference = "Stop"

$token = $env:GITHUB_TOKEN
if (-not $token) { throw "set GITHUB_TOKEN first" }

$repoRoot = Split-Path -Parent $PSScriptRoot
$conf = Get-Content (Join-Path $repoRoot "tauri.conf.json") -Raw | ConvertFrom-Json
$version = $conf.package.version
$tag = "v$version"

$installer = Get-ChildItem (Join-Path $repoRoot "target\release\bundle\nsis\*_x64-setup.exe") |
  Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $installer) { throw "no installer found" }
$sigPath = "$($installer.FullName).sig"
if (-not (Test-Path $sigPath)) { throw "no .sig file next to installer" }
$manifestPath = Join-Path $repoRoot "latest.json"
if (-not (Test-Path $manifestPath)) { throw "no latest.json" }

$headers = @{ Authorization = "Bearer $token"; "User-Agent" = "finder-release" }

# --- release ------------------------------------------------------------
$existing = $null
try { $existing = Invoke-RestMethod -Uri "https://api.github.com/repos/anshdadhich/Finder/releases/tags/$tag" -Headers $headers } catch { $existing = $null }
if ($existing) {
  Write-Host "=> release $tag already exists (id $($existing.id))"
  $release = $existing
} else {
  $notesPath = Join-Path $repoRoot "notes.txt"
  $notes = if (Test-Path $notesPath) { [System.IO.File]::ReadAllText($notesPath).Trim() } else { "" }
  $body = @{
    tag_name   = $tag
    name       = $tag
    body       = $notes
    draft      = $false
    prerelease = $false
  } | ConvertTo-Json -Depth 4
  $bodyFile = Join-Path $env:TEMP "finder-release-body.json"
  [System.IO.File]::WriteAllBytes($bodyFile, [System.Text.Encoding]::UTF8.GetBytes($body))
  $raw = curl.exe -sS -X POST -H "Authorization: Bearer $token" -H "User-Agent: finder-release" -H "Content-Type: application/json" --data-binary "@$bodyFile" "https://api.github.com/repos/anshdadhich/Finder/releases" | Out-String
  $release = $raw | ConvertFrom-Json
  if (-not $release.id) { throw "create-release failed: $raw" }
  Write-Host "=> created release $tag (id $($release.id))"
}

# --- assets -------------------------------------------------------------
foreach ($asset in @($installer.FullName, $sigPath, $manifestPath)) {
  $name = Split-Path -Leaf $asset
  $existingAssets = Invoke-RestMethod -Uri "https://api.github.com/repos/anshdadhich/Finder/releases/$($release.id)/assets" -Headers $headers
  if ($existingAssets.name -contains $name) { Write-Host "=> asset $name already present"; continue }
  $uploadUri = "https://uploads.github.com/repos/anshdadhich/Finder/releases/$($release.id)/assets?name=$name"
  Write-Host "=> uploading $name"
  $out = curl.exe -sS -X POST -H "Authorization: Bearer $token" -H "User-Agent: finder-release" -H "Content-Type: application/octet-stream" --data-binary "@$asset" $uploadUri | Out-String
  if ($out -notmatch '"state"\s*:\s*"uploaded"') { throw "asset upload failed for $name : $out" }
}

# --- winget manifests ---------------------------------------------------
# Canonical winget-pkgs layout: manifests/a/<Publisher>/<Package>/<version>/
# Template is the last validated manifest set (winget-manifests/a/AnshDadhich/Finder/0.2.1).
$sha = (Get-FileHash -Algorithm SHA256 $installer.FullName).Hash
$templateDir = Join-Path $repoRoot "winget-manifests\a\AnshDadhich\Finder\0.2.1"
$manifestDir = Join-Path $repoRoot "winget-manifests\a\AnshDadhich\Finder\$version"
New-Item -ItemType Directory -Force -Path $manifestDir | Out-Null

(Get-Content -Raw (Join-Path $templateDir "AnshDadhich.Finder.yaml")) |
  ForEach-Object { $_.Replace("PackageVersion: 0.2.1", "PackageVersion: $version"); $_.Replace("/0.2.1", "/$version") } |
  Set-Content -Encoding utf8 (Join-Path $manifestDir "AnshDadhich.Finder.yaml")

(Get-Content -Raw (Join-Path $templateDir "AnshDadhich.Finder.locale.en-US.yaml")) |
  ForEach-Object { $_.Replace("PackageVersion: 0.2.1", "PackageVersion: $version") } |
  Set-Content -Encoding utf8 (Join-Path $manifestDir "AnshDadhich.Finder.locale.en-US.yaml")

$instYaml = (Get-Content -Raw (Join-Path $templateDir "AnshDadhich.Finder.installer.yaml"))
$instYaml = $instYaml.Replace("PackageVersion: 0.2.1", "PackageVersion: $version")
$instYaml = $instYaml.Replace("/0.2.1/", "/$version/")
$instYaml = $instYaml.Replace("Finder_0.2.1_x64-setup.exe", $installer.Name)
$instYaml = [regex]::Replace($instYaml, "(?m)^\s*InstallerSha256:.*$", "    InstallerSha256: $sha")
Set-Content -Encoding utf8 (Join-Path $manifestDir "AnshDadhich.Finder.installer.yaml") $instYaml

Write-Host ""
Write-Host "Done. Release: $tag"
Write-Host "  installer : https://github.com/anshdadhich/Finder/releases/download/$tag/$($installer.Name)"
Write-Host "  sha256    : $sha"
Write-Host "Manifests at $manifestDir (validate with: winget validate $manifestDir)"
Write-Host "Submit to winget-pkgs at github.com/microsoft/winget-pkgs (folder manifests/a/AnshDadhich/Finder/$version/)."