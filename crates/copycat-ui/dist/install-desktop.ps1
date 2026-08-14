<#
.SYNOPSIS
Register copycat-ui as a desktop app for the CURRENT user on Windows -- the
PowerShell counterpart to install-desktop.sh.

.DESCRIPTION
    powershell -ExecutionPolicy Bypass -File crates\copycat-ui\dist\install-desktop.ps1
    ... -Desktop      # also drop an icon on the Desktop
    ... -Autostart    # also start the daemon + tray at login
    ... -Uninstall    # remove the shortcuts again

Searching "copycat" in the Start menu then launches the tray/history window.
No admin rights: everything lands under the current user's profile.

WHY A COPIED .ICO WHEN THE EXE HAS ONE: copycat-ui.exe does embed this icon as a
resource (crates/copycat-ui/build.rs), so a shortcut would inherit it anyway. The
copy exists because IconLocation is resolved lazily, every time the shell draws
the shortcut: pointing it at a file this installer owns keeps the icon stable if
the binary is later replaced by a build without the resource, and gives the
Add/Remove Programs entry (dist/install.ps1) a DisplayIcon to use.

.PARAMETER Desktop
Also create a Desktop shortcut. Off by default -- the Start menu entry is what
makes it findable; a desktop icon is a matter of taste.

.PARAMETER Autostart
Also run dist\windows\install-autostart.ps1 -Tray, so the daemon and tray come
up at login. Mirrors install-desktop.sh's --autostart.

.PARAMETER Uninstall
Remove the shortcuts, the copied icon, and the tray's autostart entry. Leaves
the daemon's autostart alone (remove that with install-autostart.ps1
-Uninstall) and never touches the binaries.

.PARAMETER CopycatUiBin
Absolute path to copycat-ui.exe. Default: PATH, then
%LOCALAPPDATA%\Programs\copycat, then %USERPROFILE%\.cargo\bin, then this
checkout's target\release.

.PARAMETER StartMenuDir
.PARAMETER DesktopDir
.PARAMETER IconDir
Overrides for the three install locations. Defaults are the real per-user
folders; dist\windows\test-install.ps1 points them at a temp directory so the
tests never touch a developer's actual Start menu.
#>

#Requires -Version 5.1
[CmdletBinding()]
param(
  [switch] $Desktop,
  [switch] $Autostart,
  [switch] $Uninstall,
  [string] $CopycatUiBin,
  [string] $StartMenuDir,
  [string] $DesktopDir,
  [string] $IconDir,
  [string] $StartupDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Say { param($m) Write-Host $m }
function Die { param($m) Write-Error "error: $m"; exit 1 }

$AppName = 'copycat'
$here    = Split-Path -Parent $MyInvocation.MyCommand.Path

if (-not $StartMenuDir) {
  $StartMenuDir = Join-Path ([Environment]::GetFolderPath('Programs')) 'copycat'
}
if (-not $DesktopDir) { $DesktopDir = [Environment]::GetFolderPath('Desktop') }
if (-not $IconDir)    { $IconDir    = Join-Path $env:LOCALAPPDATA 'Programs\copycat' }
if (-not $StartupDir) { $StartupDir = [Environment]::GetFolderPath('Startup') }

$StartLnk = Join-Path $StartMenuDir "$AppName.lnk"
$DeskLnk  = Join-Path $DesktopDir  "$AppName.lnk"
$IconDst  = Join-Path $IconDir     "$AppName.ico"
$TrayVbs  = Join-Path $StartupDir  'copycat-ui.vbs'

# --- uninstall -------------------------------------------------------------
if ($Uninstall) {
  foreach ($f in $StartLnk, $DeskLnk, $IconDst, $TrayVbs) {
    if (Test-Path $f) { Remove-Item $f -Force; Say "removed $f" }
  }
  # Only remove our own Start-menu folder, and only once it is empty.
  if ((Test-Path $StartMenuDir) -and -not (Get-ChildItem $StartMenuDir -Force)) {
    Remove-Item $StartMenuDir -Force
    Say "removed $StartMenuDir"
  }
  Say ''
  Say 'Removed copycat from your Start menu.'
  Say '(The daemon may still start at login: dist\windows\install-autostart.ps1 -Uninstall)'
  exit 0
}

# --- locate the binary -----------------------------------------------------
# Resolve to an absolute path when we can: a .lnk stores a target, so unlike a
# .desktop Exec= there is no PATH lookup at click time to fall back on.
$bin = $null
if ($CopycatUiBin) {
  if (-not (Test-Path $CopycatUiBin)) { Die "'$CopycatUiBin' does not exist" }
  $bin = (Resolve-Path $CopycatUiBin).Path
}
else {
  $onPath = Get-Command 'copycat-ui' -ErrorAction SilentlyContinue
  if ($onPath) { $bin = $onPath.Source }
  else {
    # target\release is searched when this script is run from a checkout, because
    # `cargo build --release -p copycat-ui` is what the docs tell you to run and
    # it lands nowhere else. Without this the installer writes a shortcut to a
    # path that does not exist and only prints a NOTE about it.
    # $here is <repo>\crates\copycat-ui\dist, so three levels up is the repo root.
    $repoRoot = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $here))
    foreach ($dir in (Join-Path $env:LOCALAPPDATA 'Programs\copycat'),
                     (Join-Path $env:USERPROFILE '.cargo\bin'),
                     (Join-Path $repoRoot 'target\release')) {
      $cand = Join-Path $dir 'copycat-ui.exe'
      if (Test-Path $cand) { $bin = $cand; break }
    }
  }
}
$missing = -not $bin
if ($missing) { $bin = Join-Path $env:LOCALAPPDATA 'Programs\copycat\copycat-ui.exe' }

$iconSrc = Join-Path $here "$AppName.ico"
if (-not (Test-Path $iconSrc)) { Die "icon not found next to this script: $iconSrc" }

New-Item -ItemType Directory -Path $StartMenuDir -Force | Out-Null
New-Item -ItemType Directory -Path $IconDir -Force | Out-Null
Copy-Item $iconSrc $IconDst -Force

# --- the shortcuts ---------------------------------------------------------
function New-CopycatShortcut {
  param([string] $Path)
  $sh = New-Object -ComObject WScript.Shell
  $lnk = $sh.CreateShortcut($Path)
  $lnk.TargetPath       = $bin
  $lnk.WorkingDirectory = Split-Path -Parent $bin
  $lnk.IconLocation     = "$IconDst,0"
  $lnk.Description      = 'copycat - temporal clipboard (tray + history)'
  # 1 = normal, and it must stay that way.
  #
  # This was 7 (minimised) to stop a console box flashing, back when copycat-ui
  # was a console-subsystem binary. Release GUI builds have no console now
  # (main.rs sets windows_subsystem = "windows"), so 7 buys nothing -- and it
  # risks the opposite bug: eframe creates its window hidden and only calls
  # ShowWindow(SW_SHOW) after the first frame, so that call is the process's
  # FIRST ShowWindow on a top-level window, which is exactly the one Windows
  # replaces with STARTUPINFO.wShowWindow when the launcher set
  # STARTF_USESHOWWINDOW -- as launching a .lnk does. A minimised style could
  # therefore land the history window in the taskbar instead of on screen.
  $lnk.WindowStyle      = 1
  $lnk.Save()
}

New-CopycatShortcut -Path $StartLnk
Say "Installed 'copycat' to your Start menu ($StartLnk)"
Say "  target : $bin"
Say "  icon   : $IconDst"

if ($Desktop) {
  New-CopycatShortcut -Path $DeskLnk
  Say "  desktop: $DeskLnk"
}

if ($Autostart) {
  # Two layouts to support: a repo checkout, where this script sits at
  # crates/copycat-ui/dist/ and the other at dist/windows/; and an unpacked
  # release archive, where release.yml puts both side by side in dist/.
  $candidates = @(
    (Join-Path $here 'install-autostart.ps1'),
    (Join-Path (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $here))) 'dist\windows\install-autostart.ps1')
  )
  $auto = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
  if ($auto) {
    Say ''
    & $auto -Tray
  }
  else {
    Say ''
    Say 'NOTE: -Autostart given but install-autostart.ps1 was not found next to this'
    Say '      script or at dist\windows\ in a repo checkout. Run it yourself:'
    Say '        powershell -File dist\windows\install-autostart.ps1 -Tray'
  }
}

if ($missing) {
  Say ''
  Say 'NOTE: no copycat-ui.exe found on PATH, in the install directory, in'
  Say "      ~/.cargo/bin or in this checkout's target\release, so the shortcut"
  Say "      points at where the installer would put it ($bin)."
  Say '      Build it (cargo build --release -p copycat-ui) and re-run this'
  Say '      script, or pass -CopycatUiBin <path>.'
}
