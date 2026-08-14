# dist/

Everything that installs, packages or containerises copycat.

| File | What it does |
|---|---|
| [`install.sh`](install.sh) | Linux/macOS installer. Downloads the right archive from the latest release, verifies the published SHA-256, and drops `copycatd`/`copycat`/`copycat-mcp` in `~/.local/bin`. |
| [`install.ps1`](install.ps1) | Windows (PowerShell 5.1+) installer. Same job into `%LOCALAPPDATA%\Programs\copycat`, tray GUI included. |
| [`systemd/copycatd.service`](systemd/copycatd.service) | `systemd --user` unit for an always-on daemon on Linux. |
| [`launchd/`](launchd/) | macOS user agent: a plist template plus `install-agent.sh`, which fills in absolute paths and loads it. |
| [`windows/install-autostart.ps1`](windows/install-autostart.ps1) | Login autostart on Windows. `-Tray` also starts `copycat-ui`. |
| [`windows/test-install.ps1`](windows/test-install.ps1) | Checks both Windows installers in a temp directory, and prints the count it ran. The CI `windows` job runs it. |

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/deanott/copycat/main/dist/install.sh | sh
```

```powershell
irm https://raw.githubusercontent.com/deanott/copycat/main/dist/install.ps1 | iex
```

Both take `COPYCAT_VERSION`, `COPYCAT_INSTALL_DIR` and `GITHUB_TOKEN` as overrides, and both
hard-fail on a missing `.sha256`. `install.sh` uses `gh` when you have it logged in, the only way
this works while the repo stays private.

The Windows install registers an Add/Remove Programs entry and drops a copy of itself as
`uninstall.ps1`, since `UninstallString` has to outlive the temp directory. Uninstalling removes
the binaries, shortcuts, launchers and registry entry, and keeps `copycatd.log`.

```powershell
powershell -File "$env:LOCALAPPDATA\Programs\copycat\uninstall.ps1" -Uninstall
```

## Always-on daemon

**Linux.** The unit runs `%h/.cargo/bin/copycatd`, so point `ExecStart` at `%h/.local/bin/copycatd`
if you used `install.sh`.

```sh
install -Dm644 dist/systemd/copycatd.service ~/.config/systemd/user/copycatd.service
systemctl --user daemon-reload
systemctl --user enable --now copycatd
```

**macOS.** The script finds `copycatd` on `PATH`, then `~/.local/bin`, then `~/.cargo/bin`, and
`COPYCATD_BIN` overrides that. Logs land in `~/Library/Logs/copycat/copycatd.log`. It installs a
user agent with `LimitLoadToSessionType = Aqua`, because `NSPasteboard` exists only inside a GUI
login session, and it is a script rather than a file you copy because launchd expands neither `%h`
nor `~`. The tray GUI is not covered; run `copycat-ui` yourself.

```sh
sh dist/launchd/install-agent.sh              # install, start now and at login
sh dist/launchd/install-agent.sh --uninstall
launchctl bootout gui/$(id -u)/io.github.deanott.copycatd   # stop; KeepAlive ignores kill
```

**Windows:**

```powershell
powershell -ExecutionPolicy Bypass -File dist\windows\install-autostart.ps1 -Tray
powershell -ExecutionPolicy Bypass -File dist\windows\install-autostart.ps1 -Uninstall
wscript "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup\copycatd.vbs"   # verify now
```

It writes two generated files (three with `-Tray`) and no registry key: a
`%LOCALAPPDATA%\copycat\start-copycatd.cmd` that runs the daemon and appends stderr to
`copycatd.log`, plus a `.vbs` per process in the Startup folder that launches it with
`intWindowStyle 0`, the one mechanism that starts a console process genuinely hidden without admin
rights. Task Scheduler's "whether logged on or not" would put the daemon in session 0, which has no
interactive clipboard.

The `.cmd` restarts the daemon after `-RestartDelaySeconds` (default 6) up to `-MaxStarts` times
(default 10) per login, then logs that it gave up, and rotates the log past `-LogMaxBytes` (default
5 MB). Nothing restarts the daemon after that, and nothing runs it before the next login.

## Start menu and desktop icon (Windows)

`-Autostart` also runs `install-autostart.ps1 -Tray`; `-Uninstall` reverses it. It writes
`copycat.lnk` into the per-user Start menu and copies `copycat.ico` next to the binary, so the
shortcut survives deleting the checkout. That `.ico` and the macOS
`dist/macos/copycat-icon-1024.png` are peers, both generated from
`scripts/logo/mark.py` by `scripts/logo/build.py`, and `crates/copycat-ui/build.rs`
embeds the `.ico` into the exe. A missing `rc.exe`/`windres` is a warning locally so the Linux pre-push
Windows check still passes; `release.yml` sets `COPYCAT_REQUIRE_ICON=1` to make it fatal there.

```powershell
powershell -ExecutionPolicy Bypass -File crates\copycat-ui\dist\install-desktop.ps1 -Desktop
```

## Release assets the installers expect

For a tag `vX.Y.Z`, every target in `release.yml`'s matrix uploads
`copycat-vX.Y.Z-<target>.tar.gz` (`.zip` on Windows) and its `.sha256`. `fail-fast: false` lets a
partial release publish, so check every leg uploaded both before you announce a tag.
