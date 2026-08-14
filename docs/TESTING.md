# Try it out: install, drive, and see copycat work

A hands-on walkthrough for your main machine, meaning a real desktop (Linux X11/Wayland or macOS)
where the OS clipboard actually attaches. Every step gives the command, what you should see, and
what to do when it fails. Around 10 minutes.

The README's Configuration section is the reference. This is the guided version.

## Quickstart

```sh
cd ~/copycat
cargo build --release --workspace                       # needs rustup, see step 0
cp target/release/{copycatd,copycat,copycat-mcp} ~/.local/bin/
export PATH="$HOME/.local/bin:$PATH"                    # add to ~/.zshrc to persist
copycatd &
copycat status
```

If that works, jump to step 4. If anything errors, the explained steps below cover every snag.

## 0. Prerequisites

Install Rust via rustup, not your distro's `apt install cargo`, which is almost always too old:

```sh
curl https://sh.rustup.rs -sSf | sh -s -- -y
. "$HOME/.cargo/env"
cargo --version                       # needs 1.95 or newer
```

**Expect** `cargo 1.95` or newer, with `which cargo` pointing under your home.

**If it fails** with `1.57` or `/usr/bin/cargo`, your PATH finds the old system cargo first:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc   # bash: ~/.bashrc
hash -r; cargo --version
```

On Linux the clipboard backend also needs a few dev headers. macOS needs nothing extra.

```sh
sudo apt-get install -y libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libxkbcommon-dev
```

## 1. Build and install the binaries

Three programs: `copycatd` (the daemon), `copycat` (the CLI) and `copycat-mcp` (the bridge to
Claude).

```sh
cd ~/copycat
cargo build --release --workspace
mkdir -p ~/.local/bin
cp target/release/{copycatd,copycat,copycat-mcp} ~/.local/bin/
which copycatd copycat copycat-mcp
```

**Expect** `Finished ... release [optimized]`, then three paths under `~/.local/bin`.

**If it fails** with `feature 'strip' is required` or `package.edition invalid type: map`, your
cargo is too old, so go back to step 0. If `which` finds nothing, `~/.local/bin` is not on your
PATH: add it as in step 0, or run everything by full path from `target/release/` instead.

To skip the Rust toolchain entirely, `dist/install.sh` downloads prebuilt binaries from a
published release. While the repo stays private you need `gh auth login` first.

## 2. Start the daemon

`copycatd` is the always-on process that owns the clipboard. Start it in its own terminal so you
can watch the logs, and open a second terminal for everything below.

```sh
copycatd
```

**Expect** a `backend=Os` line, which means copycat attached your real clipboard:

```
INFO copycatd: copycatd v1.0.0 up device=<hostname> capacity=256 backend=Os { name: "arboard" } socket=/run/user/1000/copycat/copycatd.sock
INFO copycatd::server: listening on ...
```

**If it fails** with `backend=Headless`, it could not reach a clipboard (bare SSH, or a Wayland
compositor without `data-control`). Everything below still works except the Ctrl+C/Ctrl+V
mirroring in step 4.

## 3. Your first commands

```sh
copycat status                       # is the daemon there? what backend?
copycat copy "hello from copycat"
copycat paste
copycat recall -n 5                  # newest first, with an index
```

**Expect** `copycat paste` to print `hello from copycat`, and `status` to report the daemon
version, device, backend and entry count.

**If it fails** with `could not reach copycatd`, the daemon from step 2 is not running, or you set
`COPYCAT_SOCK` for one side and not the other.

## 4. The OS integration

This needs `backend=Os` from step 2, and it is the part worth having.

Select text in any app and press Ctrl+C (Cmd+C on macOS), then run `copycat paste`. It prints what
you just copied. Now run `copycat copy "pasted straight from copycat"`, click into any text field
and press Ctrl+V. It pastes that string. Copy anywhere, recall it; generate a value, paste it.

## 5. Index recall

Recall an already-copied value by index, where 0 is current:

```sh
copycat copy "value A"; copycat copy "value B"; copycat copy "value C"
copycat recall -n 3       # see the indices
copycat paste 0           # -> value C
copycat paste 2           # -> value A
copycat pin 2             # survives eviction, and shows a marker in recall
copycat unpin 2
```

Recalling a value here costs a microsecond. Asking an LLM to regenerate it costs 100 ms to a
second or more, which is the gap copycat exists to close.

## 6. Secret safety

```sh
copycat copy "ghp_ABCDEFGHIJKLMNOP0123"   # a GitHub-token-shaped string
copycat recall -n 3
```

**Expect** the token line masked as `[SECRET]  ••••••••`. `copycat paste` still returns it
verbatim, since that is a clipboard's job, but the MCP side withholds it. See step 7.

## 7. Use it with Claude Code or OpenCode

The daemon from step 2 must be running. Register the MCP server once:

```sh
claude mcp add --transport stdio copycat -- "$(which copycat-mcp)" --stdio
```

For OpenCode, point a local MCP server at the same `copycat-mcp --stdio` command. The repo's
[`.mcp.json`](../.mcp.json) is the worked example.

Then try these prompts. Each exercises the built-in intent model, which decides when to copy versus
read:

- *"Give me a username for a new account based on 'dean the builder' and copy it."* Claude copies
  it, and Ctrl+V pastes it anywhere.
- *"Fix the grammar and put it back on my clipboard: 'i wanted to let u no teh report is redy'"*
- Copy something yourself, then ask *"what did I just copy?"*
- Copy a token, then ask *"read my clipboard"*. Claude reports a secret is present and withheld.

## 8. Two sessions, one clipboard

Open two Claude Code or OpenCode sessions on this machine. Both talk to the same daemon, so a copy
in session A ("copy the string SHARED-123 to my clipboard") reads back in session B. Sharing
between machines is the cross-device sync design, which is not built.

## 9. Run the automated tests

```sh
cargo build --workspace                       # first: links the real binaries
xvfb-run -a cargo test --workspace            # no API key needed
```

The build step matters, because `mcp_smoke` spawns the real `copycatd` binary from the target dir
and `cargo test` alone never links it. `xvfb-run -a` gives test daemons a throwaway X server
instead of your real clipboard; skip it on macOS.

**Expect** `test result: ok` for every crate, covering both end-to-end tests: the daemon's
in-process real-socket e2e, and `mcp_smoke`, which drives the real binaries through actual MCP
calls.

**If it fails** with `missing binary .../copycatd`, you skipped `cargo build --workspace`.

A clean-Debian build verifies the whole thing against the right deps regardless of your host
toolchain. The tests run during the build, so there is nothing to `docker run` afterwards:

```sh
docker build -t copycat:test .
docker build --no-cache -t copycat:test .   # if the test layer says CACHED
```

`act` runs the real CI job locally and needs Docker:

```sh
act -l                 # every job in .github/workflows
act -j build-test      # fmt, logo-asset check, clippy, build, test
```

It reads the committed `.actrc`, which pins `ubuntu-latest` to `catthehacker/ubuntu:act-latest`
because act's smaller default images ship no sudo or apt, and passes `--reuse` so the ~50-package
apt install is not repaid every run. A reused container can hide environment drift, which is fine
here: the `docker build` above is the authoritative clean-environment check. Your own `~/.actrc`
still applies, and command-line flags beat both. The `windows` job cannot run locally.

## 10. Measure the speed

```sh
cargo bench -p copycat-core --bench store_hot_path
```

**Expect** lines like `recall current get(0)  p50=50ns … (~19M ops/s)`.

Reach for this bench when you touch `copycat_core::store`, since it is the only check on the claim
the project rests on. It is a plain `harness = false` bench using `Instant`, warm-up loops and
percentiles, tuned with `COPYCAT_PERF_N` (default 200,000) and `COPYCAT_PERF_CAP` (default 256).
It measures the in-process store. For the socket round-trip and the move-versus-regenerate token
math, use `evals/computer-use/harness/microbench.py`.

## 11. Run it always-on

**Linux**, at `~/.config/systemd/user/copycatd.service`:

```ini
[Unit]
Description=copycat clipboard daemon
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/copycatd
Restart=on-failure
Environment=COPYCATD_LOG=copycatd=info

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now copycatd
journalctl --user -u copycatd -f
```

**macOS:** `sh dist/launchd/install-agent.sh`.

**Windows:** `dist\windows\install-autostart.ps1` (add `-Tray` for the GUI) plus
`crates\copycat-ui\dist\install-desktop.ps1 -Desktop`. Test without logging out:

```powershell
powershell -ExecutionPolicy Bypass -File dist\windows\test-install.ps1     # prints "N checks passed"
powershell -ExecutionPolicy Bypass -File dist\windows\install-autostart.ps1 -Tray
wscript "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup\copycatd.vbs"
copycat status
```

`test-install.ps1` covers what the installers generate, in a temp dir. Three things need your eyes,
because nothing else catches them:

1. **No console window appears.** A black console box means the Startup entry is a shortcut or
   `Run`-key entry rather than the generated `.vbs`.
2. **The tray icon registers.** It lands in the hidden-icons overflow by default on Windows 10, so
   an empty-looking taskbar is not a failure. If it is genuinely missing, `copycat-ui` logs
   `tray disabled: ...` to stderr and keeps running window-only.
3. **`copycat-ui --help` still prints.** Release GUI builds re-attach to the caller's console. The
   shell does not wait for a GUI-subsystem process, so the prompt returns before the text, which is
   normal.

Remove everything from Settings > Apps > copycat, or run the installed `uninstall.ps1 -Uninstall`.
That keeps the log on purpose.

## 12. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `zsh: command not found: copycat` | Not on PATH. Redo step 1, or run by full path. |
| `feature 'strip' is required` during build | Distro cargo too old. Install rustup (step 0). |
| `could not reach copycatd` | Start `copycatd` (step 2). Set `COPYCAT_SOCK` for both sides or neither. |
| `backend=Headless` at startup | No reachable OS clipboard: bare SSH, or a Wayland compositor without `data-control` (GNOME and KDE may not expose it). CLI and recall still work. wlroots compositors work best today. |
| `a copycatd already appears to be listening` | One is already running, so use it. For a second isolated instance, set a different `COPYCAT_SOCK`. |
| Ctrl+V pastes nothing after `copycat copy` | You are on `backend=Headless`, or another clipboard manager is fighting for ownership. Check `copycat status`. |
| Claude doesn't see the tools | Check the daemon runs, `copycat-mcp` is registered with an absolute path, and `copycat status` works from the same shell. |
| Reset everything | Stop the daemon. The store is in-RAM, so history goes on restart. Remove a stray socket with `rm "$XDG_RUNTIME_DIR"/copycat/copycatd.sock`. |
| (Windows) nothing started at login | Read `%LOCALAPPDATA%\copycat\copycatd.log`; the Startup entry runs regardless, so the reason is in there, most often a second instance failing to bind `\\.\pipe\copycatd-<user>`. With no new `=== copycatd starting ===` banner, look for a `copycatd.log.<number>` beside it before concluding the entry is gone: a daemon from an earlier run holds the main log open, so a second run diverts to a per-run file. |
| (Windows) `clipboard is held by another party` | Usually a locked workstation, since the clipboard is unavailable while the session is locked. Unlock and retry. |
