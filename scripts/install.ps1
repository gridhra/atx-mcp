<#
.SYNOPSIS
  Install a prebuilt atx-mcp binary (Windows x64).

.EXAMPLE
  irm <raw url>/scripts/install.ps1 | iex

.PARAMETER Version
  Release tag to install, e.g. v0.1.0. Defaults to the latest release.

.PARAMETER InstallDir
  Install directory. Defaults to $env:LOCALAPPDATA\Programs\atx-mcp.
#>
param(
  [string]$Version = $env:ATX_VERSION,
  [string]$InstallDir = $(if ($env:ATX_INSTALL_DIR) { $env:ATX_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\atx-mcp" })
)

$ErrorActionPreference = "Stop"

# NOTE: owner is a placeholder until the repo exists (see RELEASING.md step 0).
$Repo = "gridhra/atx-mcp"

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($arch -ne "X64") {
  throw "no prebuilt Windows binary for $arch; build from source: https://github.com/$Repo"
}
$target = "x86_64-pc-windows-msvc"

if (-not $Version) {
  $Version = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest").tag_name
}
if (-not $Version) { throw "could not determine latest release for $Repo" }

$name = "atx-mcp-$($Version.TrimStart('v'))-$target"
$base = "https://github.com/$Repo/releases/download/$Version"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
  Write-Host "Downloading atx-mcp $Version ($target)"
  $zip = Join-Path $tmp "$name.zip"
  Invoke-WebRequest -Uri "$base/$name.zip" -OutFile $zip -UseBasicParsing

  # Verify against the release's SHA256SUMS.
  try {
    $sums = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing).Content
  } catch { $sums = $null }
  if ($sums) {
    $line = ($sums -split "`n") | Where-Object { $_ -match [regex]::Escape("$name.zip") } | Select-Object -First 1
    if (-not $line) { throw "$name.zip not listed in SHA256SUMS" }
    $expected = ($line -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch (expected $expected, got $actual)" }
    Write-Host "Checksum OK"
  }

  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  Copy-Item -Path (Join-Path $tmp "$name\atx-mcp.exe") -Destination (Join-Path $InstallDir "atx-mcp.exe") -Force

  Write-Host "Installed $(Join-Path $InstallDir 'atx-mcp.exe')"
  if (($env:PATH -split ';') -notcontains $InstallDir) {
    Write-Host ""
    Write-Host "Note: $InstallDir is not on your PATH. Add it, or use the full path in your MCP config."
  }
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
