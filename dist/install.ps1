<#
.SYNOPSIS
copycat installer for Windows -- downloads prebuilt binaries from a GitHub Release.

.DESCRIPTION
The PowerShell counterpart to dist/install.sh. No Rust/cargo required.

    irm https://raw.githubusercontent.com/deanott/copycat/main/dist/install.ps1 | iex

PRIVATE REPO: release assets are not downloadable anonymously. If the GitHub CLI
(`gh`) is installed and logged in, this script uses it automatically. Otherwise set
$env:GITHUB_TOKEN and it will send an Authorization header.

Env overrides:
  $env:COPYCAT_VERSION     install a specific tag instead of latest
  $env:COPYCAT_INSTALL_DIR install location (default: %LOCALAPPDATA%\Programs\copycat)
  $env:GITHUB_TOKEN        token for authenticated download (private repos)

.NOTES
WHAT YOU GET ON WINDOWS: copycatd, copycat, copycat-mcp and the tray GUI
copycat-ui.exe, plus the two setup scripts and the icon.

AFTER INSTALLING, nothing runs yet. Two optional steps:
  install-autostart.ps1 -Tray   start the daemon (and tray) at login
  install-desktop.ps1 -Desktop  Start-menu entry and a desktop icon
Both are copied into the install directory, so they work without a checkout.

TO REMOVE IT: Settings > Apps > copycat, or run this script's installed copy:
  powershell -File "%LOCALAPPDATA%\Programs\copycat\uninstall.ps1" -Uninstall

The IPC is a named pipe (\\.\pipe\copycatd-<user>), not a Unix socket, and it is
locked to your user's SID by a DACL applied when the pipe is created. See
crates/copycatd/src/ipc.rs.

Unlike dist/install.sh, this script VERIFIES the published SHA-256 before installing.
#>

[CmdletBinding()]
param(
  # Remove an existing install: binaries, the Add/Remove Programs entry, the
  # autostart launchers and the shortcuts. Leaves the log directory (see below).
  [switch] $Uninstall,
  # Install location. Falls back to $env:COPYCAT_INSTALL_DIR, then the default.
  [string] $InstallDir,
  # Where the Add/Remove Programs entry lives. Per-user (HKCU) on purpose: this
  # is a per-user install and writing HKLM would need admin. Overridable so
  # dist\windows\test-install.ps1 can exercise uninstall against a scratch key.
  [string] $RegistryRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
  # Overrides for the generated-file locations, for the same testing reason.
  [string] $StartupDir,
  [string] $DataDir,
  [string] $StartMenuDir,
  [string] $DesktopDir
)

#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repo = 'deanott/copycat'
if (-not $InstallDir) {
  $InstallDir = if ($env:COPYCAT_INSTALL_DIR) { $env:COPYCAT_INSTALL_DIR }
                else { Join-Path $env:LOCALAPPDATA 'Programs\copycat' }
}
if (-not $StartupDir)   { $StartupDir   = [Environment]::GetFolderPath('Startup') }
if (-not $DataDir)      { $DataDir      = Join-Path $env:LOCALAPPDATA 'copycat' }
if (-not $StartMenuDir) { $StartMenuDir = Join-Path ([Environment]::GetFolderPath('Programs')) 'copycat' }
if (-not $DesktopDir)   { $DesktopDir   = [Environment]::GetFolderPath('Desktop') }

$AppKey = Join-Path $RegistryRoot 'copycat'
# The GUI is Windows-only and only present in archives built after it started
# shipping, so it is installed when found rather than required.
$Binaries   = 'copycatd.exe', 'copycat.exe', 'copycat-mcp.exe'
$OptionalBins = , 'copycat-ui.exe'

function Say  { param($m) Write-Host $m }
function Die  { param($m) Write-Error "error: $m"; exit 1 }

# --- uninstall -------------------------------------------------------------
# Everything this installer and its two companions can create, in one place.
# Deliberately NOT removed: %LOCALAPPDATA%\copycat\copycatd.log(.1), because logs
# are what you read to find out why something broke, and a user re-running the
# installer to fix a problem should not lose them.
if ($Uninstall) {
  $removed = 0
  $targets = @()
  foreach ($b in ($Binaries + $OptionalBins + 'copycat.ico', 'uninstall.ps1')) {
    $targets += (Join-Path $InstallDir $b)
  }
  $targets += (Join-Path $StartupDir 'copycatd.vbs')
  $targets += (Join-Path $StartupDir 'copycat-ui.vbs')
  $targets += (Join-Path $DataDir 'start-copycatd.cmd')
  $targets += (Join-Path $StartMenuDir 'copycat.lnk')
  $targets += (Join-Path $DesktopDir 'copycat.lnk')

  foreach ($t in $targets) {
    if (Test-Path $t) {
      try { Remove-Item $t -Force; $removed++; Say "removed $t" }
      catch { Say "could not remove $t ($($_.Exception.Message)) - is it running?" }
    }
  }
  # Only our own directories, and only when empty, so a shared parent survives.
  foreach ($d in $StartMenuDir, $InstallDir) {
    if ((Test-Path $d) -and -not (Get-ChildItem $d -Force)) {
      Remove-Item $d -Force; Say "removed $d"
    }
  }
  if (Test-Path $AppKey) { Remove-Item $AppKey -Recurse -Force; Say "removed $AppKey" }

  Say ''
  Say "Uninstalled copycat ($removed items removed)."
  if (Test-Path (Join-Path $DataDir 'copycatd.log')) {
    Say "Logs kept at $DataDir - delete that folder yourself if you want them gone."
  }
  Say 'A running copycatd/copycat-ui is not stopped by this: Stop-Process -Name copycatd,copycat-ui'
  exit 0
}

# --- detect platform -> release target triple ---
# Only x86_64-pc-windows-msvc is published. Windows on ARM runs x64 under emulation,
# so we allow it rather than refusing an install that would in fact work.
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
  'AMD64' { $target = 'x86_64-pc-windows-msvc' }
  'ARM64' {
    $target = 'x86_64-pc-windows-msvc'
    Say 'NOTE: no native ARM64 build; installing the x64 binaries (emulated).'
  }
  default { Die "unsupported architecture '$arch' (build from source: see README)" }
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("copycat-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
  $useGh = $false
  if (Get-Command gh -ErrorAction SilentlyContinue) {
    gh auth status *> $null
    if ($LASTEXITCODE -eq 0) { $useGh = $true }
  }

  if ($useGh) {
    Say 'Using GitHub CLI (authenticated -- works for private repos).'
    $tag = $env:COPYCAT_VERSION
    if (-not $tag) {
      $tag = (gh release view -R $Repo --json tagName -q .tagName)
      if ($LASTEXITCODE -ne 0 -or -not $tag) { Die "no release found on $Repo (has one been published?)" }
    }
    $asset = "copycat-$tag-$target.zip"
    gh release download $tag -R $Repo -p $asset -p "$asset.sha256" -D $tmp
    if ($LASTEXITCODE -ne 0) {
      Die "asset '$asset' not in release $tag -- did the release build finish & upload it? (check the Actions run)"
    }
  }
  else {
    Say 'gh not available/authenticated -- trying anonymous download (public repos only).'
    # The token goes in a header, never on a command line where it would be visible
    # to any other process listing the process table.
    $headers = @{ 'User-Agent' = 'copycat-installer' }
    if ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $($env:GITHUB_TOKEN)" }

    $tag = $env:COPYCAT_VERSION
    if (-not $tag) {
      try {
        $rel = Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$Repo/releases/latest"
        $tag = $rel.tag_name
      } catch {
        Die "no release found (private repo? install gh and 'gh auth login', or build from source)"
      }
    }
    $asset = "copycat-$tag-$target.zip"
    Say "Downloading $asset ..."
    $base = "https://github.com/$Repo/releases/download/$tag"
    try {
      Invoke-WebRequest -Headers $headers -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
      Invoke-WebRequest -Headers $headers -Uri "$base/$asset.sha256" -OutFile (Join-Path $tmp "$asset.sha256")
    } catch {
      Die "download failed -- if $Repo is private, install gh and 'gh auth login', or build from source"
    }
  }

  # --- verify the checksum before unpacking anything ---
  $zip = Join-Path $tmp $asset
  $sumFile = Join-Path $tmp "$asset.sha256"
  if (-not (Test-Path $sumFile)) {
    Die "no .sha256 published alongside $asset -- refusing to install unverified binaries"
  }
  # The file is `<hex>  <filename>` as written by sha256sum/shasum.
  $expected = ((Get-Content $sumFile -Raw).Trim() -split '\s+')[0].ToLower()
  $actual = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLower()
  if ($expected -ne $actual) {
    Die "checksum mismatch for $asset`n  expected $expected`n  got      $actual"
  }
  Say "Checksum OK ($actual)"

  # --- extract & install ---
  Expand-Archive -Path $zip -DestinationPath $tmp -Force
  $srcdir = Join-Path $tmp "copycat-$tag-$target"
  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  $installed = @()
  foreach ($b in $Binaries) {
    $src = Join-Path $srcdir $b
    if (-not (Test-Path $src)) { Die "binary '$b' missing from archive" }
    Copy-Item $src (Join-Path $InstallDir $b) -Force
    $installed += $b
  }
  foreach ($b in $OptionalBins) {
    $src = Join-Path $srcdir $b
    if (Test-Path $src) { Copy-Item $src (Join-Path $InstallDir $b) -Force; $installed += $b }
  }
  # The archive carries the two setup scripts and the icon (see release.yml), so
  # a downloaded install can be made findable/always-on without a repo checkout.
  $distSrc = Join-Path $srcdir 'dist'
  if (Test-Path $distSrc) {
    Copy-Item (Join-Path $distSrc '*') $InstallDir -Force -ErrorAction SilentlyContinue
  }
  # UninstallString has to point at something that outlives $tmp, so the
  # installer installs a copy of itself to run in -Uninstall mode.
  Copy-Item $PSCommandPath (Join-Path $InstallDir 'uninstall.ps1') -Force

  # --- register with Add/Remove Programs (per-user) ---
  $size = (Get-ChildItem $InstallDir -File | Measure-Object -Property Length -Sum).Sum
  $icon = if (Test-Path (Join-Path $InstallDir 'copycat.ico')) { Join-Path $InstallDir 'copycat.ico' }
          else { Join-Path $InstallDir 'copycatd.exe' }
  New-Item -Path $AppKey -Force | Out-Null
  $props = @{
    DisplayName     = 'copycat'
    DisplayVersion  = ($tag -replace '^v', '')
    Publisher       = 'copycat'
    InstallLocation = $InstallDir
    DisplayIcon     = $icon
    URLInfoAbout    = "https://github.com/$Repo"
    UninstallString = "powershell -NoProfile -ExecutionPolicy Bypass -File `"$InstallDir\uninstall.ps1`" -Uninstall"
    EstimatedSize   = [int]($size / 1KB)
    NoModify        = 1
    NoRepair        = 1
  }
  foreach ($k in $props.Keys) { New-ItemProperty -Path $AppKey -Name $k -Value $props[$k] -Force | Out-Null }

  Say ''
  Say "Installed $($installed -join ', ') ($tag) -> $InstallDir"
  Say 'Listed in Settings > Apps as "copycat"; remove it there or with:'
  Say "  powershell -File `"$InstallDir\uninstall.ps1`" -Uninstall"

  # PATH is per-user here on purpose: this installs to a per-user location, so a
  # machine-wide PATH edit would be both wrong and privileged.
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notlike "*$InstallDir*") {
    Say "NOTE: $InstallDir is not on your PATH. Add it with:"
    Say "  [Environment]::SetEnvironmentVariable('Path', `"`$env:Path;$InstallDir`", 'User')"
    Say '  (then open a new terminal)'
  }
  Say "Next: run 'copycatd' then 'copycat status'  (walkthrough: docs/TESTING.md)"
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
