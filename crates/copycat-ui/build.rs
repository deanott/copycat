//! Embeds the Windows icon resource into `copycat-ui.exe`.
//!
//! Without this, Explorer, the taskbar and any shortcut created straight to the
//! binary show the generic executable icon: a `.lnk` can point `IconLocation` at
//! a `.ico` file (which `install-desktop.ps1` does), but nothing can give the
//! *binary itself* an icon from outside. Only a linked resource does that.
//!
//! Windows-only and GUI-only. On every other target, and in the logic-only
//! `--no-default-features` build, this is a no-op — so nothing here can break a
//! Linux/macOS build or the headless unit tests.
//!
//! Toolchain note: `winresource` shells out to `rc.exe` (Windows SDK) on `-msvc`
//! targets and to `windres` (mingw binutils) on `-gnu` ones. Neither exists on a
//! stock Linux box, and `rc.exe` is missing on a Windows box without the SDK —
//! so a missing resource compiler is a WARNING, not an error.
//!
//! That matters because the repo's pre-push check is
//! `cargo check --tests --target x86_64-pc-windows-msvc`, run from Linux. Failing
//! hard here would break that check for every developer who does not have a
//! Windows SDK, to protect a cosmetic detail. Releases get the strict treatment
//! instead: `release.yml` sets `COPYCAT_REQUIRE_ICON=1` on the Windows GUI build,
//! so a published binary can never quietly lose its icon.

fn main() {
    // Re-run only when the inputs actually change, rather than on every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=dist/copycat.ico");
    println!("cargo:rerun-if-env-changed=COPYCAT_REQUIRE_ICON");

    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let gui = std::env::var_os("CARGO_FEATURE_GUI").is_some();
    if !windows || !gui {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("dist/copycat.ico")
        // Shown on the file's Details tab and by some shell surfaces.
        .set(
            "FileDescription",
            "copycat - temporal clipboard (tray + history)",
        )
        .set("ProductName", "copycat")
        .set("OriginalFilename", "copycat-ui.exe");

    let Err(e) = res.compile() else { return };

    // Required (release builds): a published binary without its icon is a defect.
    if std::env::var_os("COPYCAT_REQUIRE_ICON").is_some() {
        panic!(
            "COPYCAT_REQUIRE_ICON is set but embedding the icon failed: {e}\n\
             Needs rc.exe (Windows SDK) for -msvc targets, or windres (mingw \
             binutils) for -gnu targets."
        );
    }

    // Otherwise: say so and carry on. The binary is fully functional; only the
    // Explorer/taskbar icon is missing, and the shortcut installer sets its own
    // IconLocation anyway.
    println!(
        "cargo:warning=copycat-ui: building without an embedded icon ({e}). \
         Install rc.exe (Windows SDK) or windres (mingw binutils) to include it."
    );
}
