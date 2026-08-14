<#
.SYNOPSIS
Start copycatd (and optionally the tray UI) at login -- the Windows equivalent of
`systemctl --user enable --now copycatd` / `dist/launchd/install-agent.sh`.

.DESCRIPTION
    powershell -ExecutionPolicy Bypass -File dist\windows\install-autostart.ps1
    powershell -ExecutionPolicy Bypass -File dist\windows\install-autostart.ps1 -Tray
    powershell -ExecutionPolicy Bypass -File dist\windows\install-autostart.ps1 -Uninstall

Writes two generated files and touches nothing else:

  %LOCALAPPDATA%\copycat\start-copycatd.cmd   runs the daemon, appends its log
  <Startup>\copycatd.vbs                      launches that .cmd with no window

WHY A .VBS AND NOT A RUN-KEY ENTRY OR A SHORTCUT: copycatd is a console program
that runs in the foreground and logs to stderr, so every other mechanism parks a
console window on the desktop at each login. `WScript.Shell.Run` with
intWindowStyle 0 is the one no-admin way to start it fully hidden. The .cmd
exists so the log redirect lives in one place instead of being escaped through
VBScript quoting.

Per-user on purpose (HKCU-scope Startup folder, per-user install dir): copycatd
holds one user's cleartext clipboard and its pipe is DACL'd to that user's SID.
A machine-wide autostart would be both wrong and privileged.

.PARAMETER Tray
Also start copycat-ui at login. Off by default: the daemon is the part that has
to be running for anything else to work, and not everyone wants a tray icon.
Windows release archives do ship copycat-ui.exe, so it is usually there.

.PARAMETER Uninstall
Remove the generated files. Leaves an already-running copycatd alone.

.PARAMETER CopycatdBin
Absolute path to copycatd.exe. Default: the first one found on PATH, then
%LOCALAPPDATA%\Programs\copycat, then %USERPROFILE%\.cargo\bin.

.PARAMETER CopycatUiBin
Absolute path to copycat-ui.exe, resolved the same way when -Tray is given.

.PARAMETER StartupDir
.PARAMETER DataDir
Overrides for where the launcher pair is written. Defaults are the real per-user
Startup folder and %LOCALAPPDATA%\copycat; dist\windows\test-install.ps1 points
them at a temp directory so the tests never touch a real login.

.PARAMETER LogMaxBytes
Rotate copycatd.log to copycatd.log.1 once it exceeds this (default 5 MB). One
generation is kept.

.PARAMETER MaxStarts
How many times the launcher restarts copycatd within one login before giving up
(default 10). This is the Windows stand-in for systemd's Restart=on-failure and
launchd's KeepAlive. Bounded deliberately -- see the generated .cmd for why an
unbounded loop is the wrong default here.

.PARAMETER RestartDelaySeconds
Delay between restarts (default 6).

.NOTES
Verify with `copycat status` after a logout/login. The daemon's log is at
%LOCALAPPDATA%\copycat\copycatd.log -- if autostart appears to do nothing, that
file holds the reason (a second instance failing to bind the pipe, most often).
#>

#Requires -Version 5.1
[CmdletBinding()]
param(
  [switch] $Tray,
  [switch] $Uninstall,
  [string] $CopycatdBin,
  [string] $CopycatUiBin,
  [string] $StartupDir,
  [string] $DataDir,
  [int] $LogMaxBytes = 5242880,
  [int] $MaxStarts = 10,
  [int] $RestartDelaySeconds = 6
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Say { param($m) Write-Host $m }
function Die { param($m) Write-Error "error: $m"; exit 1 }

if (-not $StartupDir) { $StartupDir = [Environment]::GetFolderPath('Startup') }
if (-not $DataDir)    { $DataDir    = Join-Path $env:LOCALAPPDATA 'copycat' }
$CmdFile    = Join-Path $DataDir 'start-copycatd.cmd'
$LogFile    = Join-Path $DataDir 'copycatd.log'
$DaemonVbs  = Join-Path $StartupDir 'copycatd.vbs'
$TrayVbs    = Join-Path $StartupDir 'copycat-ui.vbs'

# --- uninstall -------------------------------------------------------------
if ($Uninstall) {
  foreach ($f in $DaemonVbs, $TrayVbs, $CmdFile) {
    if (Test-Path $f) { Remove-Item $f -Force; Say "removed $f" }
  }
  Say ''
  Say 'copycat will no longer start at login.'
  Say '(A copycatd started earlier is still running: Stop-Process -Name copycatd)'
  exit 0
}

# --- locate the binaries; the launcher needs absolute paths ----------------
function Find-Binary {
  param([string] $Name, [string] $Override)
  if ($Override) {
    if (-not (Test-Path $Override)) { Die "'$Override' does not exist" }
    return (Resolve-Path $Override).Path
  }
  $onPath = Get-Command $Name -ErrorAction SilentlyContinue
  if ($onPath) { return $onPath.Source }
  foreach ($dir in (Join-Path $env:LOCALAPPDATA 'Programs\copycat'),
                   (Join-Path $env:USERPROFILE '.cargo\bin')) {
    $cand = Join-Path $dir "$Name.exe"
    if (Test-Path $cand) { return $cand }
  }
  return $null
}

$daemon = Find-Binary -Name 'copycatd' -Override $CopycatdBin
if (-not $daemon) {
  Die "no copycatd found. Install it first (dist\install.ps1, or ``cargo build --release``), or pass -CopycatdBin."
}

New-Item -ItemType Directory -Path $DataDir -Force | Out-Null

# --- making paths safe to bake into a .cmd --------------------------------
# cmd.exe decodes a batch file in the OEM console codepage, which on a Western
# install cannot represent (say) a Cyrillic profile name -- and PowerShell's
# `ascii` encoder silently substitutes `?` for every character above 127, which
# turns a wrong path into a valid-looking one. Either way the launcher ends up
# pointing somewhere that does not exist, while the installer prints the correct
# path and exits 0.
#
# So the .cmd never carries a non-ASCII path if it can avoid one:
#   1. Re-express it against an environment variable cmd expands at run time.
#      %LOCALAPPDATA%\copycat is ASCII however the profile is spelled, and this
#      covers the default install and data locations.
#   2. Otherwise fall back to the 8.3 short name, which is ASCII by construction.
#   3. Otherwise give up, keep the long path, and WARN -- silently writing a
#      broken launcher is the one outcome worth ruling out.
# The .vbs has no such problem: it is written UTF-16LE, which wscript reads
# natively (only a UTF-8 BOM is refused).
Add-Type -Namespace CopycatWin -Name Path -MemberDefinition @'
[DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
public static extern uint GetShortPathNameW(string lpszLongPath, System.Text.StringBuilder lpszShortPath, uint cchBuffer);
'@ -ErrorAction SilentlyContinue

function Test-Ascii { param([string] $S) -not ($S.ToCharArray() | Where-Object { [int]$_ -gt 127 }) }

function ConvertTo-CmdSafePath {
  param([string] $Path, [string] $What)

  if (Test-Ascii $Path) { return $Path }

  foreach ($var in 'LOCALAPPDATA', 'APPDATA', 'USERPROFILE', 'ProgramFiles') {
    $root = [Environment]::GetEnvironmentVariable($var)
    if ($root -and $Path.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
      $candidate = "%$var%" + $Path.Substring($root.Length)
      if (Test-Ascii $candidate) { return $candidate }
    }
  }

  # GetShortPathNameW only resolves paths that exist, so make sure the log does.
  if (-not (Test-Path $Path)) {
    New-Item -ItemType File -Path $Path -Force -ErrorAction SilentlyContinue | Out-Null
  }
  if (Test-Path $Path) {
    $sb = New-Object System.Text.StringBuilder 260
    if ([CopycatWin.Path]::GetShortPathNameW($Path, $sb, 260) -gt 0) {
      $short = $sb.ToString()
      if ($short -and (Test-Ascii $short)) { return $short }
    }
  }

  Say "WARNING: $What contains characters the batch launcher may not survive:"
  Say "         $Path"
  Say '         (no environment-variable form and no 8.3 short name available)'
  return $Path
}

$daemonForCmd = ConvertTo-CmdSafePath -Path $daemon  -What 'the copycatd path'
$logForCmd    = ConvertTo-CmdSafePath -Path $LogFile -What 'the log path'

# --- the launcher pair -----------------------------------------------------
# Absolute paths are baked in rather than expanded at login: %LOCALAPPDATA% is
# set for an interactive session, but relying on it here would make the login
# failure mode a silent no-op instead of a log line.
#
# Encodings, and why they differ: the .cmd goes out as OEM, which is what cmd.exe
# decodes a batch file in, with its paths already made ASCII-safe above. The .vbs
# goes out as UTF-16LE, which wscript reads natively -- only a UTF-8 BOM is
# refused, and that is the constraint the earlier "ASCII everywhere" rule was
# really about. Both files' own prose stays ASCII regardless.
@"
@echo off
rem Generated by dist\windows\install-autostart.ps1 - edits will be overwritten.
setlocal
set "LOG=$logForCmd"
set "BIN=$daemonForCmd"
set "MAXLOG=$LogMaxBytes"
set "MAXSTARTS=$MaxStarts"
set /a STARTS=0

rem A copycatd launched by an earlier run inherits this log handle and holds it
rem for its whole lifetime, and cmd opens >> targets without write sharing -- so
rem a second run cannot open the log, and a failed redirect ABORTS the command
rem rather than running it unredirected. That would make a manual re-run a silent
rem no-op. Probe once and divert to a per-run file instead, so the run still
rem happens and still says why somewhere findable.
(echo.) >> "%LOG%" 2>nul || set "LOG=%LOG%.%RANDOM%"

:loop
rem Rotate before starting, so one long-lived daemon cannot grow the log without
rem bound. One generation is kept: this is a diagnostic tail, not an audit trail,
rem and the daemon never logs clipboard content into it.
if exist "%LOG%" for %%A in ("%LOG%") do if %%~zA GTR %MAXLOG% (
  if exist "%LOG%.1" del /q "%LOG%.1"
  move /y "%LOG%" "%LOG%.1" >nul
)

set /a STARTS+=1
echo === copycatd starting (attempt %STARTS%) %DATE% %TIME% === >> "%LOG%"
"%BIN%" >> "%LOG%" 2>&1
echo === copycatd exited (code %ERRORLEVEL%) %DATE% %TIME% === >> "%LOG%"

rem The systemd unit has Restart=on-failure and the launchd agent has KeepAlive;
rem this is the Windows equivalent. Bounded on purpose: the common cause of an
rem immediate exit is another copycatd already holding the pipe, and an unbounded
rem loop would then respawn forever at whatever the delay is, hidden, with only a
rem growing log to show for it.
if %STARTS% GEQ %MAXSTARTS% (
  echo === giving up after %STARTS% starts - fix the cause above, then run: wscript "%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\copycatd.vbs" >> "%LOG%"
  goto :eof
)
rem ping, not timeout: timeout needs console input, which a hidden window has not
rem got, and it fails with "Input redirection is not supported".
ping -n $RestartDelaySeconds 127.0.0.1 >nul
goto loop
"@ | Set-Content -Path $CmdFile -Encoding oem

@"
' Generated by dist\windows\install-autostart.ps1 - edits will be overwritten.
' Starts copycatd at login with no console window (see the .ps1 for why).
CreateObject("WScript.Shell").Run """$CmdFile""", 0, False
"@ | Set-Content -Path $DaemonVbs -Encoding unicode

Say "copycatd will start at login"
Say "  binary   : $daemon"
Say "  launcher : $CmdFile"
Say "  startup  : $DaemonVbs"
Say "  log      : $LogFile"

# --- optional: the tray UI -------------------------------------------------
# Without -Tray, drop any tray entry a previous run left behind rather than
# layering state: re-running this script should describe the whole autostart
# setup, not add to whatever was there before (same reason install-agent.sh
# boots the old launchd agent out before writing a new plist).
if (-not $Tray -and (Test-Path $TrayVbs)) {
  Remove-Item $TrayVbs -Force
  Say "removed the previous copycat-ui autostart entry (no -Tray given)"
}

if ($Tray) {
  $ui = Find-Binary -Name 'copycat-ui' -Override $CopycatUiBin
  if (-not $ui) {
    Say ''
    Say 'NOTE: -Tray given but no copycat-ui.exe found; skipping the tray entry.'
    Say '      The Windows release archive does not ship it -- build it with'
    Say '      `cargo build --release -p copycat-ui`.'
  }
  else {
    @"
' Generated by dist\windows\install-autostart.ps1 - edits will be overwritten.
' Starts the copycat tray UI at login without its console window. copycat-ui is
' a console-subsystem binary (so --help/--version still work from a terminal),
' which is exactly why it needs the hidden Run rather than a plain shortcut.
CreateObject("WScript.Shell").Run """$ui""", 0, False
"@ | Set-Content -Path $TrayVbs -Encoding unicode
    Say ''
    Say "copycat-ui will start at login"
    Say "  binary  : $ui"
    Say "  startup : $TrayVbs"
  }
}

Say ''
Say 'Start it now without waiting for a login:'
Say "  wscript ""$DaemonVbs"""
Say 'Then verify with:  copycat status'
Say 'Uninstall:  powershell -File dist\windows\install-autostart.ps1 -Uninstall'
