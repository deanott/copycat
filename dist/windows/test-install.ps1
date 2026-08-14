<#
.SYNOPSIS
Tests for the two Windows installers -- dist\windows\install-autostart.ps1 and
crates\copycat-ui\dist\install-desktop.ps1.

.DESCRIPTION
    powershell -ExecutionPolicy Bypass -File dist\windows\test-install.ps1

Exits 0 when every assertion passes, otherwise the number of failures. Run by
the CI `windows` job.

Everything happens inside a temp directory: both installers take -StartupDir /
-DataDir / -StartMenuDir / -DesktopDir / -IconDir overrides precisely so this
script can never write to a real Start menu, Desktop or Startup folder. If you
add an installer path that writes somewhere new, give it an override too -- a
test that touches the developer's real login is worse than no test.

The installers are invoked as CHILD PROCESSES rather than dot-sourced, so exit
codes, parameter binding and Set-StrictMode behave exactly as they do for a user
running them from a shell.

WHAT THIS DOES NOT COVER: that Windows actually runs the Startup entry at login,
and that the generated .vbs starts a real daemon. Both need a login session; see
the manual checks in docs/TESTING.md.
#>

#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repo    = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$autoPs1 = Join-Path $repo 'dist\windows\install-autostart.ps1'
$deskPs1 = Join-Path $repo 'crates\copycat-ui\dist\install-desktop.ps1'
$iconSrc = Join-Path $repo 'crates\copycat-ui\dist\copycat.ico'

$script:failures = 0
$script:checks   = 0

function Check {
  param([string] $What, [scriptblock] $Condition)
  $script:checks++
  $ok = $false
  try { $ok = [bool](& $Condition) } catch { $ok = $false }
  if ($ok) { Write-Host "  ok   $What" }
  else {
    Write-Host "  FAIL $What" -ForegroundColor Red
    $script:failures++
  }
}

function Run-Installer {
  param([string] $Script, [string[]] $ScriptArgs)
  $all = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $Script) + $ScriptArgs
  # Output is captured (not piped through Select-Object, which would tear down
  # the child mid-run) so a passing test stays quiet.
  #
  # ErrorActionPreference is dropped to Continue for the call: with `2>&1` on a
  # native exe, Windows PowerShell wraps each stderr line in an ErrorRecord, and
  # under Stop that *throws* -- so the deliberate failure cases below would abort
  # this script instead of being asserted on. The exit code is the real signal.
  $prev = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { $null = & powershell @all 2>&1 }
  finally { $ErrorActionPreference = $prev }
  return $LASTEXITCODE
}

# A stub for the checks that only ever Test-Path a binary and bake its path into
# a launcher.
#
# NEVER point an executed launcher at one of these. It is a text file named .exe,
# and Windows answers that with a modal "Unsupported 16-Bit Application" dialog
# that blocks the launching cmd until somebody clicks OK -- an interactive
# interruption on a dev box, and on CI a job that hangs to its timeout with no
# output explaining why. Use New-RunnableExe for anything that actually runs.
function New-FakeExe {
  param([string] $Path)
  New-Item -ItemType Directory -Path (Split-Path -Parent $Path) -Force | Out-Null
  Set-Content -Path $Path -Value 'not a real binary' -Encoding ascii
  return $Path
}

# A real, harmless 64-bit executable, for the checks that launch what they built.
# hostname.exe ships with Windows, needs no arguments, writes one line to stdout
# (so it proves the log redirect works) and exits immediately.
function New-RunnableExe {
  param([string] $Path)
  New-Item -ItemType Directory -Path (Split-Path -Parent $Path) -Force | Out-Null
  Copy-Item (Join-Path $env:SystemRoot 'System32\hostname.exe') $Path -Force
  return $Path
}

# The .cmd is decoded by cmd.exe in the OEM console codepage and must not carry a
# UTF-8 BOM (cmd would echo it as stray characters on the first line).
function Test-CmdEncoding {
  param([string] $Path)
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  -not ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
}

# The .vbs is UTF-16LE with a BOM, which wscript reads natively. This is what
# lets a non-ASCII path be embedded losslessly; a UTF-8 BOM is the one wscript
# refuses outright.
function Test-VbsEncoding {
  param([string] $Path)
  $bytes = [System.IO.File]::ReadAllBytes($Path)
  $bytes.Length -ge 2 -and $bytes[0] -eq 0xFF -and $bytes[1] -eq 0xFE
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("copycat-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
  Write-Host ''
  Write-Host 'shipped scripts parse and are ASCII'

  # This is not pedantry about characters. dist/install.ps1 shipped with em-dashes
  # and no BOM, and Windows PowerShell 5.1 reads a BOM-less .ps1 in the system ANSI
  # codepage: the third byte of a UTF-8 em-dash is 0x94, which CP1252 maps to a
  # smart quote, which PowerShell honours as a STRING DELIMITER. The result was six
  # parse errors and an installer that could not run at all via `-File` -- invisible
  # to anyone testing the documented `irm | iex` path, which decodes as UTF-8.
  #
  # Either fix works (ASCII, or a BOM); this asserts the one the repo chose, and
  # then proves the file actually parses.
  foreach ($rel in 'dist\install.ps1', 'dist\windows\install-autostart.ps1',
                   'dist\windows\test-install.ps1',
                   'crates\copycat-ui\dist\install-desktop.ps1') {
    $path = Join-Path $repo $rel
    Check "$rel is ASCII (or has a BOM)" {
      $bytes = [System.IO.File]::ReadAllBytes($path)
      $hasBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
      $hasBom -or -not ($bytes | Where-Object { $_ -gt 127 })
    }
    Check "$rel parses" {
      $errs = $null
      [void][System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$errs)
      -not $errs
    }
  }

  $startup = Join-Path $tmp 'Startup'
  $data    = Join-Path $tmp 'Data'
  $daemon  = New-FakeExe (Join-Path $tmp 'bin\copycatd.exe')
  $ui      = New-FakeExe (Join-Path $tmp 'bin\copycat-ui.exe')
  New-Item -ItemType Directory -Path $startup, $data -Force | Out-Null

  $cmdFile   = Join-Path $data    'start-copycatd.cmd'
  $daemonVbs = Join-Path $startup 'copycatd.vbs'
  $trayVbs   = Join-Path $startup 'copycat-ui.vbs'
  $common    = @('-StartupDir', $startup, '-DataDir', $data, '-CopycatdBin', $daemon)

  Write-Host ''
  Write-Host 'install-autostart.ps1'

  $code = Run-Installer $autoPs1 $common
  Check 'exits 0'                      { $code -eq 0 }
  Check 'writes the .cmd launcher'     { Test-Path $cmdFile }
  Check 'writes the Startup .vbs'      { Test-Path $daemonVbs }
  Check 'no tray entry without -Tray'  { -not (Test-Path $trayVbs) }

  $cmd = Get-Content $cmdFile -Raw
  $vbs = Get-Content $daemonVbs -Raw
  Check 'bakes in the absolute daemon path' { $cmd -match [regex]::Escape($daemon) }
  Check 'redirects the log into DataDir'    { $cmd -match [regex]::Escape($data) }
  Check '.vbs points at the .cmd'           { $vbs -match [regex]::Escape($cmdFile) }
  # The whole point of the .vbs: window style 0 and no waiting.
  Check '.vbs runs hidden and async'        { $vbs -match ',\s*0\s*,\s*False' }

  Check '.cmd has no UTF-8 BOM'             { Test-CmdEncoding $cmdFile }
  Check '.vbs is UTF-16LE with a BOM'       { Test-VbsEncoding $daemonVbs }

  # The Windows stand-in for Restart=on-failure / KeepAlive, and the only thing
  # keeping copycatd.log from growing without bound.
  Check '.cmd restarts the daemon'          { $cmd -match ':loop' -and $cmd -match 'goto loop' }
  Check '.cmd bounds the restart count'     { $cmd -match 'MAXSTARTS' -and $cmd -match 'giving up' }
  # `timeout` needs console input, which a hidden window has not got.
  Check '.cmd sleeps with ping, not timeout' { $cmd -match 'ping -n' -and $cmd -notmatch '\btimeout /t' }
  Check '.cmd rotates the log'              { $cmd -match 'MAXLOG' -and $cmd -match 'move /y' }

  $before = Get-Content $daemonVbs -Raw
  $null = Run-Installer $autoPs1 $common
  Check 'is idempotent'                     { (Get-Content $daemonVbs -Raw) -eq $before }

  $code = Run-Installer $autoPs1 ($common + @('-Tray', '-CopycatUiBin', $ui))
  Check '-Tray exits 0'                     { $code -eq 0 }
  Check '-Tray writes the tray entry'       { Test-Path $trayVbs }
  Check '-Tray points at copycat-ui'        { (Get-Content $trayVbs -Raw) -match [regex]::Escape($ui) }

  # Re-running plainly must describe the whole setup, not layer on the old one.
  $null = Run-Installer $autoPs1 $common
  Check 'a plain re-run drops the tray entry' { -not (Test-Path $trayVbs) }

  $code = Run-Installer $autoPs1 ($common + '-Uninstall')
  Check '-Uninstall exits 0'                { $code -eq 0 }
  Check '-Uninstall removes the .cmd'       { -not (Test-Path $cmdFile) }
  Check '-Uninstall removes the .vbs'       { -not (Test-Path $daemonVbs) }

  # A missing binary must fail loudly: a launcher baked with a bad path would
  # "install" fine and then do nothing at every login, with no error anywhere.
  $code = Run-Installer $autoPs1 @('-StartupDir', $startup, '-DataDir', $data,
                                   '-CopycatdBin', (Join-Path $tmp 'nope\copycatd.exe'))
  Check 'fails when copycatd is missing'    { $code -ne 0 }
  Check 'and writes nothing on failure'     { -not (Test-Path $daemonVbs) }

  Write-Host ''
  Write-Host 'non-ASCII profile path'

  # The failure this guards against: PowerShell's `ascii` encoder substitutes `?`
  # for every character above 127 WITHOUT erroring, and cmd.exe decodes a batch
  # file in the OEM codepage, which on a Western install cannot represent (say) a
  # Cyrillic profile name. Either way the launcher points at a path that does not
  # exist while the installer prints the right one and exits 0 -- a silent no-op
  # at every login, for every user outside en-US. The earlier version of this
  # suite asserted all-bytes-ASCII on the generated files, which locked the bug in
  # rather than catching it, because a temp path is always ASCII.
  # Built from code points so THIS file stays ASCII (the suite asserts that of
  # itself, above). It spells a u-umlaut plus five Cyrillic letters: one character
  # CP1252 can represent and five it cannot, which is the case OEM encoding alone
  # does not fix.
  $intlName = 'M' + [char]0xFC + 'ller-' + [char]0x041E + [char]0x043B + [char]0x044C +
              [char]0x0433 + [char]0x0430
  $intl     = Join-Path $tmp $intlName
  # Runnable, not a stub: this is the one check that executes the launcher.
  $intlBin  = New-RunnableExe (Join-Path $intl 'bin\copycatd.exe')
  $intlData = Join-Path $intl 'Data'
  $intlUp   = Join-Path $intl 'Startup'
  New-Item -ItemType Directory -Path $intlData, $intlUp -Force | Out-Null

  $code = Run-Installer $autoPs1 @('-StartupDir', $intlUp, '-DataDir', $intlData,
                                   '-CopycatdBin', $intlBin,
                                   '-MaxStarts', '1', '-RestartDelaySeconds', '1')
  $intlCmd = Join-Path $intlData 'start-copycatd.cmd'
  $intlLog = Join-Path $intlData 'copycatd.log'
  Check 'exits 0 under a non-ASCII path'   { $code -eq 0 }
  Check 'writes the launcher'              { Test-Path $intlCmd }
  # The tell-tale of the old bug: a literal '?' where a character used to be.
  Check 'no "?" corruption in the .cmd'    { (Get-Content $intlCmd -Raw -Encoding oem) -notmatch '\?' }

  # The assertion that actually matters: run it and see whether the redirect
  # lands. Static checks cannot tell a correct path from a plausible one.
  #
  # The log is removed first because the installer itself creates it (empty) when
  # it needs GetShortPathNameW to resolve, which only works on a path that
  # exists -- so asserting mere existence afterwards would pass without the
  # launcher having run at all. Wait for the BANNER, not for the file.
  Remove-Item $intlLog -Force -ErrorAction SilentlyContinue
  $null = & wscript.exe (Join-Path $intlUp 'copycatd.vbs')
  $waited = 0
  while ($waited -lt 25) {
    if ((Test-Path $intlLog) -and ((Get-Content $intlLog -Raw -ErrorAction SilentlyContinue) -match 'copycatd starting')) { break }
    Start-Sleep -Milliseconds 400; $waited++
  }
  Check 'the launcher actually runs there' { Test-Path $intlLog }
  Check 'and logs where it said it would'  { (Get-Content $intlLog -Raw) -match 'copycatd starting' }

  Write-Host ''
  Write-Host 'install-desktop.ps1'

  $startMenu = Join-Path $tmp 'StartMenu'
  $desktop   = Join-Path $tmp 'Desktop'
  $iconDir   = Join-Path $tmp 'Icons'
  New-Item -ItemType Directory -Path $desktop -Force | Out-Null
  $deskCommon = @('-StartMenuDir', $startMenu, '-DesktopDir', $desktop,
                  '-IconDir', $iconDir, '-StartupDir', $startup)

  $startLnk = Join-Path $startMenu 'copycat.lnk'
  $deskLnk  = Join-Path $desktop   'copycat.lnk'
  $iconDst  = Join-Path $iconDir   'copycat.ico'

  Check 'the icon asset exists'             { Test-Path $iconSrc }
  # Catches a corrupted or truncated commit of the binary asset: ICONDIR is
  # reserved=0, type=1 (icon), then the image count.
  Check 'the icon is a real multi-size .ico' {
    $b = [System.IO.File]::ReadAllBytes($iconSrc)
    $b.Length -gt 1000 -and $b[0] -eq 0 -and $b[1] -eq 0 -and $b[2] -eq 1 -and $b[3] -eq 0 -and $b[4] -ge 2
  }

  $code = Run-Installer $deskPs1 $deskCommon
  Check 'exits 0'                           { $code -eq 0 }
  Check 'creates the Start menu shortcut'   { Test-Path $startLnk }
  Check 'copies the icon next to the binary'{ Test-Path $iconDst }
  Check 'no Desktop icon without -Desktop'  { -not (Test-Path $deskLnk) }

  $sh = New-Object -ComObject WScript.Shell
  $lnk = $sh.CreateShortcut($startLnk)
  Check 'the shortcut targets copycat-ui'   { $lnk.TargetPath -match 'copycat-ui\.exe$' }
  Check 'the shortcut uses the copied icon' { $lnk.IconLocation -match [regex]::Escape($iconDst) }
  # Must be normal, not minimised: a shortcut's ShowCmd overrides the first
  # ShowWindow a process makes, and for eframe that is the one showing the
  # history window. See the comment at the WindowStyle assignment.
  Check 'the shortcut starts normal'        { $lnk.WindowStyle -eq 1 }

  $code = Run-Installer $deskPs1 ($deskCommon + '-Desktop')
  Check '-Desktop exits 0'                  { $code -eq 0 }
  Check '-Desktop creates the icon'         { Test-Path $deskLnk }

  $code = Run-Installer $deskPs1 ($deskCommon + '-Uninstall')
  Check '-Uninstall exits 0'                { $code -eq 0 }
  Check '-Uninstall removes both shortcuts' { -not (Test-Path $startLnk) -and -not (Test-Path $deskLnk) }
  Check '-Uninstall removes the icon'       { -not (Test-Path $iconDst) }

  Write-Host ''
  Write-Host 'install.ps1 -Uninstall'

  # Only the uninstall half is testable here: installing means downloading a
  # GitHub release. So stage what a real install leaves behind and assert the
  # uninstaller finds all of it -- the failure that matters is an uninstall that
  # silently leaves files or an Add/Remove Programs entry pointing at nothing.
  $instDir  = Join-Path $tmp 'Installed'
  $regRoot  = 'HKCU:\Software\copycat-test-uninstall'
  $appKey   = Join-Path $regRoot 'copycat'
  New-Item -ItemType Directory -Path $instDir, $startMenu -Force | Out-Null
  foreach ($f in 'copycatd.exe','copycat.exe','copycat-mcp.exe','copycat-ui.exe','copycat.ico','uninstall.ps1') {
    Set-Content -Path (Join-Path $instDir $f) -Value 'x' -Encoding ascii
  }
  Set-Content -Path (Join-Path $startup 'copycatd.vbs')  -Value 'x' -Encoding ascii
  Set-Content -Path (Join-Path $startup 'copycat-ui.vbs') -Value 'x' -Encoding ascii
  Set-Content -Path (Join-Path $data 'start-copycatd.cmd') -Value 'x' -Encoding ascii
  Set-Content -Path (Join-Path $data 'copycatd.log') -Value 'a log line' -Encoding ascii
  Set-Content -Path $startLnk -Value 'x' -Encoding ascii
  Set-Content -Path $deskLnk  -Value 'x' -Encoding ascii
  New-Item -Path $appKey -Force | Out-Null
  New-ItemProperty -Path $appKey -Name 'DisplayName' -Value 'copycat' -Force | Out-Null

  $code = Run-Installer (Join-Path $repo 'dist\install.ps1') @(
    '-Uninstall', '-InstallDir', $instDir, '-RegistryRoot', $regRoot,
    '-StartupDir', $startup, '-DataDir', $data,
    '-StartMenuDir', $startMenu, '-DesktopDir', $desktop)

  Check 'exits 0'                            { $code -eq 0 }
  Check 'removes the binaries'               { -not (Test-Path (Join-Path $instDir 'copycatd.exe')) }
  Check 'removes the tray GUI'               { -not (Test-Path (Join-Path $instDir 'copycat-ui.exe')) }
  Check 'removes the autostart launchers'    { -not (Test-Path (Join-Path $startup 'copycatd.vbs')) -and
                                               -not (Test-Path (Join-Path $data 'start-copycatd.cmd')) }
  Check 'removes the shortcuts'              { -not (Test-Path $startLnk) -and -not (Test-Path $deskLnk) }
  Check 'removes the Add/Remove entry'       { -not (Test-Path $appKey) }
  # Logs are diagnostics: losing them to an uninstall-and-retry is worse than
  # leaving a folder behind, so this one must SURVIVE.
  Check 'keeps the log'                      { Test-Path (Join-Path $data 'copycatd.log') }

  Remove-Item $regRoot -Recurse -Force -ErrorAction SilentlyContinue
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

Write-Host ''
if ($script:failures -eq 0) {
  Write-Host "$($script:checks) checks passed"
  exit 0
}
Write-Host "$($script:failures) of $($script:checks) checks FAILED" -ForegroundColor Red
exit $script:failures
