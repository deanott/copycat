//! System-tray / menu-bar icon, and the winit(eframe) ↔ tray-icon integration.
//!
//! # The event-loop problem
//!
//! `eframe` owns a single `winit` event loop and runs egui on the main thread.
//! `tray-icon` also wants to live near a native run loop, and its requirements
//! differ per OS — so we can't just "create a tray icon" and be done. Two things
//! have to be handled: **where the icon is created** and **how its events reach
//! the egui loop**.
//!
//! ## How events reach egui (all platforms)
//!
//! tray-icon delivers menu clicks and icon clicks through two *process-global*
//! channels: [`tray_icon::menu::MenuEvent::receiver`] and
//! [`tray_icon::TrayIconEvent::receiver`]. We never register a winit user-event;
//! instead the egui app calls [`poll_events`] once per frame to drain those
//! channels non-blockingly (see `app.rs`, which also schedules a periodic
//! `request_repaint_after` so frames keep coming while the window is idle). This
//! decouples event handling from *where* the icon was constructed.
//!
//! ## Where the icon is created (per-OS)
//!
//! - **Linux** (StatusNotifier/AppIndicator over D-Bus, via gtk): the icon needs
//!   a running glib/gtk main loop, which winit does not provide. We spawn a
//!   dedicated thread that calls `gtk::init()`, builds the tray icon, and runs
//!   `gtk::main()` for the process lifetime. The `TrayIcon` is kept alive on that
//!   thread (dropping it would remove the icon). This is the pattern tray-icon's
//!   own winit examples use on Linux.
//! - **macOS**: the icon must be created on the **main thread** and only once the
//!   app run loop (NSApplication) is up. eframe invokes our app-creator closure
//!   on the main thread *after* it has initialised the event loop, so we create
//!   the icon there ([`init`]) and hand the `TrayIcon` back to be stored in the
//!   `App` (also main-thread) to keep it alive. Menu/icon events still arrive via
//!   the global channels above.
//! - **Windows** (bonus): same as macOS — create on the main thread inside the
//!   app closure, store in the `App`.
//!
//! If anything fails (no tray host, no D-Bus, headless CI), we log a warning and
//! return without an icon; the window still runs — graceful degradation.

// Only the Linux path keeps a cross-thread channel to the gtk-owned icon; on
// macOS/Windows the caller holds the `TrayIcon` directly, so importing these
// unconditionally is an unused-import warning (and a `-D warnings` CI failure)
// on every non-Linux target.
#[cfg(target_os = "linux")]
use std::sync::mpsc::Sender;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use copycat_core::view;

use crate::cat_logo;

/// Menu item ids. Stable strings so the (possibly cross-thread) event handler can
/// recognise a click without sharing the `MenuItem` objects themselves.
pub const MENU_OPEN: &str = "open";
pub const MENU_STATUS: &str = "status";
pub const MENU_SETTINGS: &str = "settings";
pub const MENU_QUIT: &str = "quit";
/// "See more…" under the recents. A separate id from [`MENU_OPEN`] even though
/// both open the window: menu ids must be unique, and this one reads as "the
/// list continues" rather than "launch the app".
pub const MENU_SEE_MORE: &str = "see-more";
/// The disabled item labelling the recents block.
const MENU_HEADER: &str = "recent-header";

/// Prefix for the per-entry "re-copy this" items. The entry id is appended, so
/// the id round-trips through tray-icon's global event channel as a plain string
/// without the menu having to hand out `MenuItem` handles.
const MENU_USE_PREFIX: &str = "use:";

/// How many recent entries the tray menu offers. Small on purpose: the menu is a
/// shortcut to the last few things you copied, not a second history window.
pub const TRAY_RECENTS: usize = 5;

/// Longest entry label in the menu, in characters. Menus that grow with their
/// content make the whole panel jump around, so previews are clipped rather than
/// allowed to set the width.
const LABEL_MAX: usize = 48;

/// One clickable recent entry in the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayRecent {
    /// Stable entry id, echoed back in [`TrayAction::Use`].
    pub id: String,
    /// Display text. Must already be masked — see [`recent_label`].
    pub label: String,
    /// Who put it there — see [`recent_meta`]. Answers the question the preview
    /// alone cannot: whether *you* copied this or an agent did, which is the
    /// whole point of recording provenance.
    pub meta: String,
}

/// The provenance chip trailing a menu row: `"you"`, `"ᗢ claude · 74e82829"`.
///
/// Composed from `copycat_core::view` rather than formatted here, so the tray
/// agrees with the history window and the CLI about what an author is called —
/// this surface owns the layout (the separator, the order) and nothing else.
///
/// Deliberately carries **no age**, unlike the history window's rows. The menu is
/// rebuilt only when its contents change, because a rebuild flickers and on
/// macOS can dismiss the menu while the user has it open; a relative time would
/// tick `now` → `1m` → `2m` on its own and force rebuilds with nothing actually
/// copied. Author and session are fixed for the life of an entry, so they cost
/// no rebuilds at all.
pub fn recent_meta(author: Option<&str>, session_id: Option<&str>) -> String {
    use copycat_core::entry::Origin;
    let who = view::author_label(author);
    // Agents get the cat mark; a person does not. The glyph carries the
    // distinction without relying on colour, which a menu row cannot style.
    let who = match Origin::of(author) {
        Origin::Agent => format!("{} {who}", view::AGENT_GLYPH),
        _ => who,
    };
    // The session only disambiguates when several agents are writing; for a
    // person it is noise, so it rides along only for agent-authored entries.
    match session_id.filter(|_| Origin::of(author) == Origin::Agent) {
        Some(s) => format!("{who} · {}", view::session_label(s)),
        None => who,
    }
}

/// Build a menu label from an already-masked preview.
///
/// The preview arrives from `rows`, which routes every entry through
/// `copycat_core::view` with `Audience::Human`, so a `Secret` is already the
/// mask by the time it gets here — this only clips and flattens. Newlines would
/// otherwise turn one entry into a multi-line menu row.
pub fn recent_label(preview: &str) -> String {
    let flat: String = preview
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > LABEL_MAX {
        let clipped: String = flat.chars().take(LABEL_MAX - 1).collect();
        format!("{clipped}…")
    } else if flat.is_empty() {
        "(empty)".to_string()
    } else {
        flat
    }
}

/// What a tray interaction asks the UI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// Show and raise the history window.
    Open,
    /// Show and raise, and flash the status bar / focus the status line.
    ShowStatus,
    /// Show and raise on the Settings tab.
    ShowSettings,
    /// Make the entry with this id the current clipboard.
    Use(String),
    /// Quit the whole application.
    Quit,
}

/// What should happen when the window's close button is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Cancel the close and hide the window — the app lives on in the menu bar.
    Hide,
    /// Let the process exit. Only ever reached via the tray's Quit.
    Exit,
    /// No close was requested this frame.
    Nothing,
}

/// The menu-bar lifecycle rule, as a pure function.
///
/// A menu-bar app must not exit when its window closes: this process owns the
/// tray icon, so exiting takes the icon with it and leaves the user no way back
/// except Spotlight. eframe's default is the opposite — it exits when the last
/// viewport closes — so the behaviour has to be asked for explicitly.
///
/// Extracted from the `App` because the interesting part is a two-input decision
/// and the uninteresting part is an `egui::Context`, which cannot exist on a
/// headless CI box. Verified behaviourally: an instrumented build confirmed the
/// egui loop keeps running while the viewport is hidden, so hiding does not
/// stall polling or tray updates.
pub fn close_action(close_requested: bool, quitting: bool) -> CloseAction {
    match (close_requested, quitting) {
        (false, _) => CloseAction::Nothing,
        (true, true) => CloseAction::Exit,
        (true, false) => CloseAction::Hide,
    }
}

/// Icon edge in pixels. Larger than the panel needs so it stays crisp on the
/// HiDPI scale factors where the tray upscales its source bitmap.
const ICON_PX: u32 = 64;

/// Build the RGBA tray icon. No asset file and no image decoder: it is
/// scan-converted from the same geometry the window header draws
/// ([`cat_logo::CAT`]), so the two cannot drift apart.
fn build_icon() -> Option<tray_icon::Icon> {
    let rgba = cat_logo::rasterize(ICON_PX, cat_logo::INK);
    tray_icon::Icon::from_rgba(rgba, ICON_PX, ICON_PX).ok()
}

/// Sends menu updates to whichever thread owns the tray icon. Set once, by that
/// thread, when the tray comes up; stays empty if the tray never started
/// (headless, no gtk, tray build failed) so the setters degrade to no-ops.
#[cfg(target_os = "linux")]
static ICON_TX: OnceLock<Sender<Vec<TrayRecent>>> = OnceLock::new();

/// Replace the recent-entries section of the menu.
///
/// The `tray` argument is the handle the caller stores — `Some` on macOS /
/// Windows, always `None` on Linux where the icon lives on the gtk thread and
/// the change has to be posted to it instead. Taking the same argument on every
/// platform keeps the call site in `app` free of `cfg`s.
#[cfg(target_os = "linux")]
pub fn set_recents(recents: Vec<TrayRecent>, _tray: Option<&TrayIcon>) {
    if let Some(tx) = ICON_TX.get() {
        let _ = tx.send(recents);
    }
}

/// See the Linux variant above.
#[cfg(not(target_os = "linux"))]
pub fn set_recents(recents: Vec<TrayRecent>, tray: Option<&TrayIcon>) {
    let Some(tray) = tray else { return };
    match build_menu(&recents) {
        Ok(menu) => tray.set_menu(Some(Box::new(menu))),
        Err(e) => tracing::warn!("could not rebuild tray menu: {e}"),
    }
}

/// Construct the menu and the tray icon. Must be called on whatever thread will
/// own the icon (the gtk thread on Linux, the main thread elsewhere).
/// The menu: open, the last few entries (click to re-copy), settings, quit.
///
/// Rebuilt whenever the recents change — `tray-icon` has no API for mutating a
/// live menu's items, so swapping the whole `Menu` is the supported path.
fn build_menu(recents: &[TrayRecent]) -> anyhow::Result<Menu> {
    let menu = Menu::new();
    for item in menu_model(recents) {
        match item {
            MenuRow::Separator => menu.append(&PredefinedMenuItem::separator())?,
            MenuRow::Item { id, label, enabled } => {
                menu.append(&MenuItem::with_id(id, label, enabled, None))?
            }
        }
    }
    Ok(menu)
}

/// One row of the tray menu, before it becomes a native widget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuRow {
    Separator,
    Item {
        id: String,
        label: String,
        enabled: bool,
    },
}

impl MenuRow {
    fn item(id: impl Into<String>, label: impl Into<String>, enabled: bool) -> Self {
        MenuRow::Item {
            id: id.into(),
            label: label.into(),
            enabled,
        }
    }
}

/// The menu's *structure*, derived from the recents and nothing else.
///
/// Split out from [`build_menu`] because a `tray_icon::Menu` cannot be built
/// without a display, and everything worth asserting about this menu — that the
/// heading is there and disabled, that "See more…" appears only when the list is
/// actually truncated, that each row carries its provenance and round-trips its
/// entry id — is structure, not rendering. This half runs in CI on a headless
/// box; `build_menu` is then a mechanical translation with no decisions left in
/// it. Same split as `rows.rs`, for the same reason.
pub fn menu_model(recents: &[TrayRecent]) -> Vec<MenuRow> {
    let mut rows = vec![
        MenuRow::item(MENU_OPEN, "Open copycat", true),
        MenuRow::Separator,
    ];

    if recents.is_empty() {
        // A disabled placeholder rather than nothing: an empty gap between two
        // separators reads as a broken menu.
        rows.push(MenuRow::item("recent-empty", "No recent copies", false));
    } else {
        // A disabled heading, so the block reads as the clipboard rather than as
        // five unexplained strings. It names the top row as the current
        // clipboard because that is the one question the menu exists to answer:
        // what would Ctrl/Cmd+V give me right now?
        rows.push(MenuRow::item(
            MENU_HEADER,
            "Clipboard — top is current",
            false,
        ));
        for r in recents.iter().take(TRAY_RECENTS) {
            // Two spaces rather than a separator character: menu rows have no
            // column alignment, so the gap is the only thing keeping the preview
            // and its provenance visually distinct.
            let label = if r.meta.is_empty() {
                r.label.clone()
            } else {
                format!("{}   {}", r.label, r.meta)
            };
            rows.push(MenuRow::item(
                format!("{MENU_USE_PREFIX}{}", r.id),
                label,
                true,
            ));
        }
        // Only worth offering when the menu is actually truncating.
        if recents.len() >= TRAY_RECENTS {
            rows.push(MenuRow::item(MENU_SEE_MORE, "See more…", true));
        }
    }

    rows.extend([
        MenuRow::Separator,
        MenuRow::item(MENU_SETTINGS, "Settings…", true),
        MenuRow::item(MENU_STATUS, "Status", true),
        MenuRow::Separator,
        MenuRow::item(MENU_QUIT, "Quit", true),
    ]);
    rows
}

fn build_tray(recents: &[TrayRecent]) -> anyhow::Result<TrayIcon> {
    let menu = build_menu(recents)?;

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("copycat — temporal clipboard");
    if let Some(icon) = build_icon() {
        builder = builder.with_icon(icon);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("build tray icon: {e}"))
}

/// Initialise the tray. See the module docs for the per-OS strategy.
///
/// Returns the live [`TrayIcon`] when the caller must keep it alive (macOS /
/// Windows: store it in the `App`). Returns `None` on Linux (the icon lives on
/// the dedicated gtk thread) and on any failure (graceful degradation).
#[cfg(target_os = "linux")]
pub fn init() -> Option<TrayIcon> {
    // gtk/StatusNotifier needs its own main loop; run it off the winit thread.
    let spawned = std::thread::Builder::new()
        .name("copycat-ui-tray".into())
        .spawn(move || {
            if let Err(e) = gtk::init() {
                tracing::warn!("tray disabled: gtk init failed ({e}); running window-only");
                return;
            }
            // Moved into the timeout closure below, which keeps the icon alive
            // for as long as the gtk loop runs.
            let tray = match build_tray(&[]) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("tray disabled: {e}; running window-only");
                    return;
                }
            };

            // The UI thread cannot touch `tray` — gtk objects belong to this
            // thread — so menu changes arrive as messages and are applied here.
            // Only publish the sender once the icon actually exists, so a failed
            // tray leaves the setter a no-op instead of queueing into the void.
            let (tx, rx) = std::sync::mpsc::channel::<Vec<TrayRecent>>();
            if ICON_TX.set(tx).is_err() {
                tracing::warn!("tray already initialised; ignoring the second init");
                return;
            }
            gtk::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                // Drain: only the last update in a burst matters, but the queue
                // is never deep enough for that to be worth optimising.
                while let Ok(recents) = rx.try_recv() {
                    match build_menu(&recents) {
                        Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                        Err(e) => tracing::warn!("could not rebuild tray menu: {e}"),
                    }
                }
                gtk::glib::ControlFlow::Continue
            });

            gtk::main();
        });
    if let Err(e) = spawned {
        tracing::warn!("tray disabled: could not spawn gtk thread ({e}); running window-only");
    }
    None
}

/// Initialise the tray on macOS / Windows: create on the current (main) thread
/// and return it to be kept alive by the caller.
#[cfg(not(target_os = "linux"))]
pub fn init() -> Option<TrayIcon> {
    match build_tray(&[]) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!("tray disabled: {e}; running window-only");
            None
        }
    }
}

/// Drain tray-icon's global menu/icon event channels and translate what we find
/// into [`TrayAction`]s. Non-blocking; safe to call every frame.
pub fn poll_events() -> Vec<TrayAction> {
    let mut actions = Vec::new();

    // Menu-item clicks.
    while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
        match event.id.0.as_str() {
            MENU_OPEN | MENU_SEE_MORE => actions.push(TrayAction::Open),
            MENU_STATUS => actions.push(TrayAction::ShowStatus),
            MENU_SETTINGS => actions.push(TrayAction::ShowSettings),
            MENU_QUIT => actions.push(TrayAction::Quit),
            // A recent-entry click carries its entry id in the item id itself.
            id => {
                if let Some(entry) = id.strip_prefix(MENU_USE_PREFIX) {
                    actions.push(TrayAction::Use(entry.to_string()));
                }
            }
        }
    }

    // Left-click on the icon opens the window.
    while let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
        if let tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = event
        {
            actions.push(TrayAction::Open);
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_flattens_newlines_so_one_entry_is_one_menu_row() {
        assert_eq!(recent_label("line one\nline two"), "line one line two");
        assert_eq!(recent_label("tab\tsep"), "tab sep");
    }

    #[test]
    fn label_clips_long_previews_and_marks_the_cut() {
        let long = "x".repeat(LABEL_MAX * 2);
        let out = recent_label(&long);
        assert_eq!(out.chars().count(), LABEL_MAX);
        assert!(out.ends_with('…'), "clipped label should show it was cut");
    }

    #[test]
    fn label_leaves_short_previews_alone() {
        assert_eq!(recent_label("short"), "short");
    }

    #[test]
    fn blank_preview_gets_a_placeholder_not_an_unclickable_gap() {
        assert_eq!(recent_label("   "), "(empty)");
        assert_eq!(recent_label(""), "(empty)");
    }

    /// The masking contract: `recent_label` must never *unmask*. It receives an
    /// already-masked preview from `rows` (Audience::Human), so the mask has to
    /// survive verbatim into the menu.
    #[test]
    fn an_already_masked_preview_stays_masked() {
        let masked = copycat_core::view::MASK;
        assert_eq!(recent_label(masked), masked);
    }

    #[test]
    fn use_ids_round_trip_through_the_menu_id_string() {
        let id = "1785227465136-000000";
        let menu_id = format!("{MENU_USE_PREFIX}{id}");
        assert_eq!(menu_id.strip_prefix(MENU_USE_PREFIX), Some(id));
        // A non-entry id must not be mistaken for one.
        assert_eq!(MENU_QUIT.strip_prefix(MENU_USE_PREFIX), None);
    }

    fn recent(id: &str, label: &str) -> TrayRecent {
        TrayRecent {
            id: id.into(),
            label: label.into(),
            meta: "you".into(),
        }
    }

    /// Every id the menu can emit must be routable back to an action, or a click
    /// silently does nothing — the failure mode that is invisible until a user
    /// reports it.
    #[test]
    fn every_enabled_row_maps_to_an_action() {
        let rs: Vec<_> = (0..5).map(|i| recent(&format!("id-{i}"), "v")).collect();
        for row in menu_model(&rs) {
            let MenuRow::Item { id, enabled, .. } = row else {
                continue;
            };
            if !enabled {
                continue;
            }
            let routable = matches!(
                id.as_str(),
                MENU_OPEN | MENU_SEE_MORE | MENU_STATUS | MENU_SETTINGS | MENU_QUIT
            ) || id.strip_prefix(MENU_USE_PREFIX).is_some();
            assert!(routable, "enabled row {id:?} maps to no action");
        }
    }

    #[test]
    fn the_heading_is_present_and_not_clickable() {
        let model = menu_model(&[recent("a", "hello")]);
        let head = model.iter().find_map(|r| match r {
            MenuRow::Item { id, label, enabled } if id == MENU_HEADER => Some((label, enabled)),
            _ => None,
        });
        let (label, enabled) = head.expect("heading missing from the menu");
        assert!(
            label.contains("Clipboard"),
            "heading should name the clipboard: {label}"
        );
        assert!(!enabled, "heading must not be clickable");
    }

    #[test]
    fn see_more_appears_only_when_the_list_is_truncated() {
        let has_see_more = |n: usize| {
            let rs: Vec<_> = (0..n).map(|i| recent(&format!("id-{i}"), "v")).collect();
            menu_model(&rs)
                .iter()
                .any(|r| matches!(r, MenuRow::Item { id, .. } if id == MENU_SEE_MORE))
        };
        // Offering "See more" when nothing is hidden would be a lie.
        assert!(!has_see_more(1));
        assert!(!has_see_more(TRAY_RECENTS - 1));
        assert!(has_see_more(TRAY_RECENTS));
        assert!(has_see_more(TRAY_RECENTS + 20));
    }

    #[test]
    fn the_menu_never_lists_more_entries_than_it_promises() {
        let rs: Vec<_> = (0..50).map(|i| recent(&format!("id-{i}"), "v")).collect();
        let entries = menu_model(&rs)
            .iter()
            .filter(|r| matches!(r, MenuRow::Item { id, .. } if id.starts_with(MENU_USE_PREFIX)))
            .count();
        assert_eq!(entries, TRAY_RECENTS);
    }

    #[test]
    fn an_empty_history_still_renders_a_usable_menu() {
        let model = menu_model(&[]);
        // No heading and no "See more" when there is nothing to head or continue.
        assert!(!model
            .iter()
            .any(|r| matches!(r, MenuRow::Item { id, .. } if id == MENU_HEADER)));
        assert!(!model
            .iter()
            .any(|r| matches!(r, MenuRow::Item { id, .. } if id == MENU_SEE_MORE)));
        // Quit must survive every state, or the app becomes unquittable.
        assert!(model
            .iter()
            .any(|r| matches!(r, MenuRow::Item { id, .. } if id == MENU_QUIT)));
        assert!(
            model
                .iter()
                .any(|r| matches!(r, MenuRow::Item { enabled, .. } if !enabled)),
            "an empty history needs a placeholder, not a gap"
        );
    }

    #[test]
    fn each_row_carries_its_provenance_and_its_entry_id() {
        let rs = vec![TrayRecent {
            id: "1785315779365-000022".into(),
            label: "presence".into(),
            meta: "\u{16e2} claude".into(),
        }];
        let MenuRow::Item { id, label, .. } = menu_model(&rs)
            .into_iter()
            .find(|r| matches!(r, MenuRow::Item { id, .. } if id.starts_with(MENU_USE_PREFIX)))
            .expect("no entry row")
        else {
            unreachable!()
        };
        assert_eq!(
            id.strip_prefix(MENU_USE_PREFIX),
            Some("1785315779365-000022")
        );
        assert!(label.contains("presence"), "preview missing: {label}");
        assert!(label.contains("claude"), "provenance missing: {label}");
    }

    /// The lifecycle rule, which is the whole point of the menu-bar app.
    #[test]
    fn closing_the_window_hides_it_but_quit_exits() {
        // The red button / Cmd-W: hide, never exit.
        assert_eq!(close_action(true, false), CloseAction::Hide);
        // Only the tray's Quit sets `quitting`, and only that may end the process.
        assert_eq!(close_action(true, true), CloseAction::Exit);
        // No close requested: do nothing at all.
        assert_eq!(close_action(false, false), CloseAction::Nothing);
        assert_eq!(close_action(false, true), CloseAction::Nothing);
    }

    #[test]
    fn meta_names_a_person_without_ceremony() {
        // A person's own copy is the common case; a session id would be noise.
        assert_eq!(recent_meta(Some("human:os"), None), "you");
        assert_eq!(recent_meta(Some("human:cli"), Some("sess_abc")), "you");
    }

    #[test]
    fn meta_marks_an_agent_and_names_its_session() {
        let m = recent_meta(Some("agent:claude"), Some("session_74e82829ff"));
        assert!(
            m.starts_with(view::AGENT_GLYPH),
            "agent needs the mark: {m}"
        );
        assert!(m.contains("claude"), "agent should be named: {m}");
        // Session disambiguates *which* agent run. `view::session_label` clips it
        // so one long id cannot set the width of the whole menu.
        assert!(m.contains("74e8282"), "session should ride along: {m}");
        assert!(
            !m.contains("session_"),
            "boilerplate should be stripped: {m}"
        );
    }

    #[test]
    fn meta_degrades_rather_than_lying_about_provenance() {
        // Pre-provenance entries exist in the ring; they must not read as "you".
        assert_eq!(recent_meta(None, None), "?");
        assert_eq!(recent_meta(Some("dean"), None), "?");
    }

    #[test]
    fn meta_carries_no_age_so_the_menu_does_not_rebuild_on_a_timer() {
        // The guard rationale in `sync_tray_recents`: a rebuild flickers and on
        // macOS can dismiss an open menu. Everything here must be fixed for the
        // life of an entry, so two calls far apart in time agree.
        let a = recent_meta(Some("agent:mcp"), Some("s1"));
        let b = recent_meta(Some("agent:mcp"), Some("s1"));
        assert_eq!(a, b);
        for t in ["now", "1m", "2m", "1h", "ago"] {
            assert!(!a.contains(t), "meta must not embed a relative time: {a}");
        }
    }
}
