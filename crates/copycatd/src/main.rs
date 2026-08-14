//! copycatd — the always-on local daemon.
//!
//! Owns the temporal store and (when a display is present) the OS clipboard,
//! exposing everything over a single local Unix socket that every front-end
//! (`copycat-cli`, `copycat-mcp`, companions) speaks to. One daemon, one notion
//! of "the current clipboard".
//!
//! Runtime shape:
//!   - probe/attach the OS clipboard (`clip`) — headless if there's no display;
//!   - build shared `State` (temporal store + entry minting);
//!   - start the clipboard watch/write thread;
//!   - serve the IPC socket until Ctrl-C, then clean up the socket file.
//!
//! Config via env: `COPYCAT_SOCK` (socket path), `COPYCAT_DEVICE` (device label),
//! `COPYCAT_CAPACITY` (hot-store size), `RUST_LOG`/`COPYCATD_LOG` (tracing),
//! `COPYCAT_REQUIRE_OS` (refuse to start headless — see below).

mod clip;
mod ipc;
mod server;
mod state;
mod usage;

use std::sync::Arc;

use copycat_core::proto::socket_path;

use crate::state::State;

/// Default hot-store capacity if `COPYCAT_CAPACITY` is unset.
const DEFAULT_CAPACITY: usize = 256;

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Version,
    Help,
    RunDaemon,
}

fn parse_cli_args<I, S>(args: I) -> CliAction
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for arg in args {
        match arg.as_ref() {
            "--version" | "-V" => return CliAction::Version,
            "--help" | "-h" => return CliAction::Help,
            _ => {}
        }
    }
    CliAction::RunDaemon
}

fn main() -> anyhow::Result<()> {
    match parse_cli_args(std::env::args().skip(1)) {
        CliAction::Version => {
            println!("copycatd {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        CliAction::Help => {
            println!(
                "copycatd {}\n\
                 Always-on local temporal clipboard daemon for copycat.\n\n\
                 USAGE:\n    copycatd [OPTIONS]\n\n\
                 OPTIONS:\n    \
                 --insecure       Allow cleartext reveal of secrets (requires build with `--features debug-unsafe`)\n    \
                 -V, --version    Print version information\n    \
                 -h, --help       Print help information\n\n\
                 ENVIRONMENT VARIABLES:\n    \
                 COPYCAT_SOCK        Socket path (default: platform runtime dir)\n    \
                 COPYCAT_DEVICE      Device label (default: machine hostname)\n    \
                 COPYCAT_CAPACITY    Hot-store capacity ring size (default: 256)\n    \
                 COPYCAT_PRIVACY     Privacy posture (off|guarded|strict, default: guarded)\n    \
                 COPYCAT_REQUIRE_OS  Refuse to start headless if OS clipboard is unavailable\n    \
                 COPYCATD_LOG        Log filter (default: copycatd=info)",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        CliAction::RunDaemon => {}
    }

    // Lock the process down before anything sensitive lands in memory: private
    // file modes and no core dumps (the daemon holds cleartext secrets in RAM).
    harden_process();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("COPYCATD_LOG")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| "copycatd=info".into()),
        )
        .init();

    resolve_insecure_flag()?;

    // Attach the OS clipboard (or fall back to headless) BEFORE entering the
    // async runtime, so the arboard probe runs on the main thread.
    let mut link = clip::attach();

    // Headless is a legitimate mode (SSH, CI, shared-socket) — but it is a silent
    // downgrade: the daemon comes up fine while no longer owning the real
    // clipboard. That's a trap when copycatd is started at boot by a service
    // manager, where it can win the race against the display server and then sit
    // there uselessly forever. `COPYCAT_REQUIRE_OS=1` says "I expect a real
    // clipboard": exit non-zero instead, so the supervisor can back off and retry
    // until the display is actually there.
    if refuse_headless_start(require_os(), &link.info) {
        anyhow::bail!(
            "COPYCAT_REQUIRE_OS is set but no OS clipboard could be attached \
             (no display?); refusing to start headless"
        );
    }

    let device = std::env::var("COPYCAT_DEVICE").unwrap_or_else(|_| default_device());

    if copycat_core::insecure::reveal_secrets() {
        warn_insecure(&device);
    }
    let capacity = std::env::var("COPYCAT_CAPACITY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CAPACITY);

    // Usage log: off unless asked for. Spawned before the state is shared so the
    // writer thread exists for the daemon's whole life.
    let usage_level = copycat_core::usage::Level::from_env();
    let usage = if usage_level.records() {
        crate::usage::UsageLog::spawn(
            copycat_core::usage::usage_dir(),
            copycat_core::usage::retention_days_from_env(),
        )
    } else {
        crate::usage::UsageLog::disabled()
    };

    let state = Arc::new(
        State::new(
            capacity,
            device.clone(),
            link.info.clone(),
            Some(link.sender.clone()),
        )
        .with_usage(usage, usage_level),
    );

    // Start the clipboard watch/write thread (no-op when headless).
    clip::spawn_watch(&mut link, Arc::clone(&state));

    let path = socket_path();
    tracing::info!(
        "copycatd v{} up — device={device} capacity={capacity} backend={:?} socket={}",
        env!("CARGO_PKG_VERSION"),
        state.backend(),
        path.display()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let serve_path = path.clone();
    let result = runtime.block_on(async move {
        tokio::select! {
            r = server::serve(serve_path, state) => r,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received Ctrl-C, shutting down");
                Ok(())
            }
        }
    });

    // Best-effort socket cleanup so the next start doesn't see a stale file.
    let _ = std::fs::remove_file(&path);
    result
}

/// The startup gate: refuse to come up when the caller demanded a real OS
/// clipboard but only headless was available. Pure (no env, no display) so the
/// backend-selection decision is unit-testable.
fn refuse_headless_start(
    require_os: bool,
    info: &copycat_core::proto::ClipboardBackendInfo,
) -> bool {
    require_os && matches!(info, copycat_core::proto::ClipboardBackendInfo::Headless)
}

/// Whether `COPYCAT_REQUIRE_OS` demands a real OS clipboard. Any value except
/// the usual "off" spellings counts as set, so `COPYCAT_REQUIRE_OS=1` works.
fn require_os() -> bool {
    match std::env::var("COPYCAT_REQUIRE_OS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// Reduce the process's ambient exposure before it holds any clipboard data.
///
/// WHAT THIS ACTUALLY BUYS YOU DIFFERS PER PLATFORM, and the difference is not
/// cosmetic — do not read the unix branch and assume the guarantee is portable:
///
/// | | linux | macos | windows |
/// |---|---|---|---|
/// | created files owner-only | `umask(0077)` | `umask(0077)` | n/a — the pipe DACL does this |
/// | crash artifact suppressed | `RLIMIT_CORE=0` | **partial** — `RLIMIT_CORE` does not govern the macOS crash reporter, which still writes a report to `~/Library/Logs/DiagnosticReports` | WER local dumps are off by default; `SetErrorMode` suppresses the fault dialog |
/// | same-user debugger blocked | `PR_SET_DUMPABLE=0` | **nothing** — `task_for_pid` is unaffected | **nothing** — a same-user process can `OpenProcess` and read RAM |
///
/// The honest summary: on Linux this is real, on macOS and Windows it is thinner
/// than it looks, and on no platform does it stop a process running as the same
/// user from reading the clipboard out of this daemon's memory.
fn harden_process() {
    #[cfg(unix)]
    // SAFETY: plain libc calls with no borrowed state; safe to run once at start.
    unsafe {
        libc::umask(0o077);
        #[cfg(target_os = "linux")]
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        let no_core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &no_core);
    }

    #[cfg(windows)]
    // SAFETY: a single scalar-argument Win32 call with no borrowed state.
    unsafe {
        // Suppress the "program has stopped working" dialog and the WER UI. This
        // is a crash-*visibility* measure, not memory protection: Windows does
        // not write a dump by default, but a crash dialog on a headless/service
        // launch would hang the process holding clipboard data in RAM.
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn SetErrorMode(mode: u32) -> u32;
}

/// Honour `--insecure`. The flag is a hard error unless the binary was built
/// with `--features debug-unsafe`, so a release binary can never be coaxed into
/// revealing secrets. When accepted it sets `COPYCAT_INSECURE` so the shared
/// `copycat_core::insecure` gate (and every front-end that inherits the env)
/// agrees this daemon is insecure.
fn resolve_insecure_flag() -> anyhow::Result<()> {
    if !std::env::args().any(|a| a == "--insecure") {
        return Ok(());
    }
    if cfg!(feature = "debug-unsafe") {
        std::env::set_var("COPYCAT_INSECURE", "1");
        Ok(())
    } else {
        anyhow::bail!(
            "--insecure requires a build with `--features debug-unsafe`; \
             release binaries cannot enable insecure mode"
        )
    }
}

/// Loud, unmissable warning that cleartext secrets are exposed. Printed to
/// stderr *and* the log so it shows up however the daemon is launched.
fn warn_insecure(device: &str) {
    let banner = format!(
        "\n\
         ┌──────────────────────────────────────────────────────────────┐\n\
         │  ⚠  INSECURE DEBUG MODE — Secret/PII shown in CLEARTEXT        │\n\
         │     copycatd on '{device:<44}' │\n\
         │     Never use with real passwords, keys, or personal data.    │\n\
         │     Unset COPYCAT_INSECURE (and drop --insecure) to disable.   │\n\
         └──────────────────────────────────────────────────────────────┘",
    );
    eprintln!("{banner}");
    tracing::warn!("INSECURE DEBUG MODE active: Secret/PII values are shown in cleartext");
}

/// A reasonable default device label: the machine hostname, else "unknown".
fn default_device() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scoped env-var setter; restores the previous value afterwards. These cases
    /// all touch the same key, so they live in one test to avoid racing each other.
    fn with_var(value: Option<&str>, f: impl FnOnce()) {
        let prev = std::env::var_os("COPYCAT_REQUIRE_OS");
        match value {
            Some(v) => std::env::set_var("COPYCAT_REQUIRE_OS", v),
            None => std::env::remove_var("COPYCAT_REQUIRE_OS"),
        }
        f();
        match prev {
            Some(v) => std::env::set_var("COPYCAT_REQUIRE_OS", v),
            None => std::env::remove_var("COPYCAT_REQUIRE_OS"),
        }
    }

    #[test]
    fn require_os_defaults_off_and_honours_off_spellings() {
        // Unset: headless stays a perfectly valid mode (SSH/CI/shared socket).
        with_var(None, || assert!(!require_os()));
        for off in ["0", "false", "no", "", "  "] {
            with_var(Some(off), || {
                assert!(!require_os(), "{off:?} must read as off")
            });
        }
        for on in ["1", "true", "yes"] {
            with_var(Some(on), || assert!(require_os(), "{on:?} must read as on"));
        }
    }

    #[test]
    fn refuses_headless_only_when_required_and_headless() {
        use copycat_core::proto::ClipboardBackendInfo::{Headless, Os};
        let os = Os {
            name: "arboard".into(),
        };
        // Required + headless is the one refuse case.
        assert!(refuse_headless_start(true, &Headless));
        // Required but a real clipboard attached: start.
        assert!(!refuse_headless_start(true, &os));
        // Not required: headless is fine, start either way.
        assert!(!refuse_headless_start(false, &Headless));
        assert!(!refuse_headless_start(false, &os));
    }

    #[test]
    fn parse_cli_args_resolution() {
        assert_eq!(parse_cli_args(["--version"]), CliAction::Version);
        assert_eq!(parse_cli_args(["-V"]), CliAction::Version);
        assert_eq!(parse_cli_args(["--help"]), CliAction::Help);
        assert_eq!(parse_cli_args(["-h"]), CliAction::Help);
        assert_eq!(parse_cli_args(["--insecure"]), CliAction::RunDaemon);
        assert_eq!(parse_cli_args(Vec::<String>::new()), CliAction::RunDaemon);
    }
}
