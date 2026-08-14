//! `copycat-ui` — the tray / menu-bar companion: a tray icon plus an egui
//! window to browse, filter and re-copy clipboard history. Just another socket
//! client of `copycatd`; masking is never decided here — every "show the raw
//! value?" question routes through [`rows::should_mask`].

// In a logic-only build (`--no-default-features`) the GUI never references the
// `rows` helpers, so they'd trip `dead_code`; that's expected there.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]
// Windows release GUI builds link as a windows-subsystem binary so launching the
// app never parks a console window; `attach_parent_console` buys back --help/--version.
#![cfg_attr(
    all(windows, feature = "gui", not(debug_assertions)),
    windows_subsystem = "windows"
)]

// `rows` is always compiled so the unit tests run on a headless box without
// GUI system libraries; everything else is behind the default-on `gui` feature.
mod rows;

#[cfg(feature = "gui")]
mod app;
#[cfg(feature = "gui")]
mod cat_logo;
#[cfg(feature = "gui")]
mod poll;
#[cfg(feature = "gui")]
mod tray;

#[cfg(feature = "gui")]
use std::path::PathBuf;

#[cfg(feature = "gui")]
use anyhow::bail;
#[cfg(feature = "gui")]
use clap::Parser;

#[cfg(feature = "gui")]
use copycat_core::proto::socket_path;

/// Window-icon size. 64 px is what Windows and most Linux shells scale from for
/// both the title bar and the taskbar button.
#[cfg(feature = "gui")]
const WINDOW_ICON_PX: u32 = 64;

#[cfg(feature = "gui")]
#[derive(Parser)]
#[command(
    name = "copycat-ui",
    version,
    about = "Intelligent temporal clipboard — tray/menu-bar GUI"
)]
struct Cli {
    /// Override the daemon socket path (default: $COPYCAT_SOCK, else
    /// $XDG_RUNTIME_DIR/copycat/copycatd.sock, else the temp dir — on macOS
    /// $TMPDIR/copycat/copycatd.sock, since XDG_RUNTIME_DIR is Linux-only).
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Show Secret/PII values in cleartext (debug only). Requires a build with
    /// `--features debug-unsafe`; a hard error otherwise. Matches the CLI flag.
    #[arg(long)]
    insecure: bool,
}

/// Re-attach to the console that launched us, on Windows GUI builds only, so
/// `--help`/`--version` have somewhere to print. Failure at any step is ignored.
/// `SetStdHandle` suffices because Rust queries `GetStdHandle` per write, and it
/// works on both the `-gnu` and `-msvc` CRTs.
#[cfg(all(windows, feature = "gui"))]
fn attach_parent_console() {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    // SAFETY: no pointers involved; returns 0 when the parent has no console,
    // which is the normal GUI-launch case and simply means "nothing to attach".
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }

    // Only claim CONOUT$ for streams with NO inherited handle — otherwise
    // redirected `--version` output is stolen back to the console.
    for slot in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: a plain query, no pointers.
        let existing = unsafe { GetStdHandle(slot) };
        if !existing.is_null() && existing != INVALID_HANDLE_VALUE {
            continue; // inherited a real handle; leave the caller's plumbing alone
        }

        // "CONOUT$" as a UTF-16 C string.
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the
        // call; the two null pointers are the documented "no security attributes
        // / no template file" arguments.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return;
        }
        // SAFETY: `handle` is a valid console handle we just opened.
        unsafe { SetStdHandle(slot, handle) };
    }
}

#[cfg(feature = "gui")]
fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    attach_parent_console();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    if cli.insecure {
        if cfg!(feature = "debug-unsafe") {
            // Runtime half of the two-part gate; the build half is the feature.
            std::env::set_var("COPYCAT_INSECURE", "1");
            tracing::warn!("insecure mode ON: Secret/PII will be shown in cleartext");
        } else {
            bail!("--insecure requires a build with `--features debug-unsafe`");
        }
    }

    let socket = cli.socket.unwrap_or_else(socket_path);
    tracing::info!(socket = %socket.display(), "starting copycat-ui");

    // Same mark the tray draws, rasterised — no image decoder, no second asset.
    // Windows Explorer uses the embedded .ico (build.rs); macOS the .app bundle's.
    let icon = egui::IconData {
        rgba: cat_logo::rasterize(WINDOW_ICON_PX, cat_logo::INK),
        width: WINDOW_ICON_PX,
        height: WINDOW_ICON_PX,
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("copycat — clipboard history")
            .with_inner_size([700.0, 640.0])
            .with_min_inner_size([440.0, 380.0])
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "copycat-ui",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, socket)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed to run: {e}"))
}

/// Logic-only build (`--no-default-features`): no GUI compiled; running it says so.
#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!(
        "copycat-ui was built without the `gui` feature (logic-only). \
         Rebuild with default features (or `--features gui`) for the desktop app."
    );
}
