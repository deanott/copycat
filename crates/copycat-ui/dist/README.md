# copycat-ui desktop integration

These files make `copycat-ui` (the tray / clipboard-history window) show up as a
real app in your Linux desktop's launcher/search, instead of only existing as a
binary.

| File | What it is |
|------|------------|
| `copycat.svg` | The app icon (scalable; installed into the hicolor icon theme). |
| `copycat.ico` | The same mark for Windows shortcuts, 16-256 px. |

Both are **generated**: run `python3 scripts/logo/build.py` after editing
`scripts/logo/mark.py`, and never hand-edit them. Every size is rendered from
the master at its own resolution, so none is a downscale of another, and CI
re-derives all of them and fails if they have drifted.

| `copycat-ui.desktop` | The launcher entry (`Name=copycat`, so searching "copycat" finds it). |
| `install-desktop.sh` | Per-user installer for the Linux two above (no root). |
| `install-desktop.ps1` | The Windows counterpart: Start-menu shortcut, `-Desktop` icon, `-Autostart`, `-Uninstall`. |

## Install (Linux)

1. Build & install the binary (needs a GUI toolchain; see deps below):

   ```sh
   cargo install --path crates/copycat-ui --features gui
   ```

2. Register it with your desktop:

   ```sh
   sh crates/copycat-ui/dist/install-desktop.sh              # add to app menu
   sh crates/copycat-ui/dist/install-desktop.sh --autostart  # + launch on login
   sh crates/copycat-ui/dist/install-desktop.sh --uninstall  # remove it
   ```

Now search **copycat** in your application menu and it launches the tray + history
window. It talks to a running `copycatd` over the local socket, so start the daemon
(or autostart it) so there's history to show.

## GUI system libraries (Debian/Ubuntu)

The `gui` feature links GTK3 (tray), GL/EGL (egui/eframe) and x11/wayland (winit):

```sh
sudo apt-get install -y \
  pkg-config libgtk-3-dev libayatana-appindicator3-dev \
  libgl1-mesa-dev libegl1-mesa-dev \
  libx11-dev libxrandr-dev libxi-dev libxcursor-dev libwayland-dev
```

(These mirror the `build-test` job in `.github/workflows/ci.yml`.)

## macOS

`install-desktop.sh` is Linux-only. On macOS the tray lives in the menu bar once
`copycat-ui` runs; `dist/macos/build-app.sh` builds the `.app` bundle.

## Windows

No extra system libraries; the GTK/GL list above is Linux-only. Windows release archives ship
`copycat-ui.exe`, so building from source is optional. `-Autostart` also runs
[`dist\windows\install-autostart.ps1`](../../../dist/README.md#always-on-daemon).

```powershell
cargo build --release -p copycat-ui
powershell -ExecutionPolicy Bypass -File crates\copycat-ui\dist\install-desktop.ps1 -Desktop
```

The installer looks for the binary in `target\release`, then PATH,
`%LOCALAPPDATA%\Programs\copycat` and `~/.cargo/bin`, and says so when it has to guess.
`-CopycatUiBin <path>` overrides that.

Release builds link as a windows-subsystem binary, so the app opens no console window.
`--help` and `--version` still print: the binary re-attaches to the parent console and claims
`CONOUT$` only for streams with no handle, leaving a caller's redirection alone. The shell does
not wait for a GUI-subsystem process, so the prompt returns before the text. Debug builds keep
their console.
