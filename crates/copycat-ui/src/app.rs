//! The egui history window. Non-trivial decisions (row mapping, filtering,
//! masking) live in the pure helpers in [`crate::rows`]; the `App` wires them to
//! widgets, drains the poll/tray channels, and issues the small synchronous
//! round-trips for user actions.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::ViewportCommand;

use copycat_core::ipc;
use copycat_core::privacy::Mode as PrivacyMode;
use copycat_core::proto::{DaemonStatus, Request, Response};

use crate::cat_logo;
use crate::poll::{self, Update};
use crate::rows::{self, Row};
use crate::tray::{self, TrayAction};

/// How often to wake even when idle, so tray/menu channels are drained promptly.
const IDLE_REPAINT: Duration = Duration::from_millis(200);

/// The copycat accent — **agent** provenance. Not the logo (see `top_bar`)
/// and not selection, which uses egui's own `selection.bg_fill`.
pub(crate) const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x3B, 0x82, 0xF6);
/// Green for entries a *person* copied — far from [`ACCENT`] in hue so human vs
/// agent provenance is separable at a glance.
const HUMAN: egui::Color32 = egui::Color32::from_rgb(0x1E, 0x9E, 0x5A);
/// Warning red for the sensitivity tag and insecure/connection alerts.
const ALERT: egui::Color32 = egui::Color32::from_rgb(0xC0, 0x2A, 0x2A);
/// Amber for the `[PII]`/`[SECRET]` tag in the detail header.
const TAG_AMBER: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x6C, 0x3A);
/// Height of the header cat mark in points — never scales or reflows.
const LOGO_SIZE: f32 = 22.0;

/// Active navigation view tab in copycat-ui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    History,
    Settings,
}

pub struct App {
    socket: PathBuf,
    rx: Receiver<Update>,
    rows: Vec<Row>,
    status: Option<DaemonStatus>,
    /// Last poll/connection error, shown in the status bar. Cleared on success.
    conn_error: Option<String>,
    filter: String,
    /// Selected entry id (survives list reordering better than an index).
    selected: Option<String>,
    /// Transient one-line notice from a user action (re-copy result, clear result, etc.).
    notice: Option<String>,
    /// Active tab (History vs Settings).
    tab: Tab,
    /// Configurable history recall limit for UI display (default 200).
    recall_limit: usize,
    /// Optional channel/lane filter ("" or None = default OS lane).
    channel_lane: String,
    /// Keeps the tray icon alive on macOS/Windows. `None` on Linux (the icon is
    /// owned by the gtk thread) or when tray init failed.
    _tray: Option<tray_icon::TrayIcon>,
    /// Last recents pushed into the tray menu, so it is only rebuilt on change.
    tray_recents: Vec<tray::TrayRecent>,
    /// Set only by the tray's Quit. Distinguishes a real exit from the window
    /// being closed, which [`App::handle_close_request`] turns into a hide.
    quitting: bool,
}

/// Register the packaged glyph subsets as font fallbacks: egui's bundled fonts
/// cover neither `ᓚᘏᗢ` nor `∿`/`∫`, and clipboard *content* can contain them.
fn install_cat_fonts(ctx: &egui::Context) {
    use std::sync::Arc;
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cat-syllabics".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/cat-syllabics.subset.ttf"
        ))),
    );
    fonts.font_data.insert(
        "cat-symbols".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/cat-symbols.subset.ttf"
        ))),
    );
    for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
        let chain = fonts.families.entry(family).or_default();
        chain.push("cat-syllabics".to_owned());
        chain.push("cat-symbols".to_owned());
    }
    ctx.set_fonts(fonts);
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, socket: PathBuf) -> Self {
        install_cat_fonts(&cc.egui_ctx);

        // Background ~1 Hz poller; wakes the UI via the egui context.
        let rx = poll::spawn(socket.clone(), cc.egui_ctx.clone());

        // On macOS/Windows this closure runs on the main thread after eframe has
        // started the event loop — the required moment to create the tray.
        let tray = tray::init();

        Self {
            socket,
            rx,
            rows: Vec::new(),
            status: None,
            conn_error: None,
            filter: String::new(),
            selected: None,
            notice: None,
            tab: Tab::History,
            recall_limit: poll::RECALL_LIMIT,
            channel_lane: String::new(),
            _tray: tray,
            tray_recents: Vec::new(),
            quitting: false,
        }
    }

    /// Fold a poll/refresh snapshot into UI state.
    fn apply(&mut self, update: Update) {
        match update {
            Update::Data { rows, status } => {
                self.rows = rows;
                if status.is_some() {
                    self.status = status;
                }
                self.conn_error = None;
            }
            Update::Error(msg) => {
                // Keep the last good rows/status; just surface the error.
                self.conn_error = Some(msg);
            }
        }
    }

    /// Drain everything the poll thread has queued since the last frame.
    fn drain_poll(&mut self) {
        while let Ok(update) = self.rx.try_recv() {
            self.apply(update);
        }
    }

    /// Handle tray/menu actions (open/raise, status, settings, re-copy, quit).
    fn handle_tray(&mut self, ctx: &egui::Context) {
        for action in tray::poll_events() {
            match action {
                TrayAction::Open => raise_window(ctx),
                TrayAction::ShowStatus => {
                    raise_window(ctx);
                    self.notice = Some(self.status_line());
                }
                TrayAction::ShowSettings => {
                    raise_window(ctx);
                    self.tab = Tab::Settings;
                }
                // Same effect as double-clicking the row in the window: put the
                // entry back on the OS clipboard, ready to paste.
                TrayAction::Use(id) => match self.rows.iter().find(|r| r.id == id) {
                    Some(row) => match row.text.clone() {
                        Some(text) => self.recopy(text),
                        None => {
                            self.notice = Some("that entry isn't text — nothing to re-copy".into())
                        }
                    },
                    // The menu can lag the ring by up to one poll, so a click can
                    // land on an entry that has just been evicted.
                    None => self.notice = Some("that entry is no longer in history".into()),
                },
                // The only route that ends the process; without the flag the
                // close below would just be cancelled again.
                TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
    }

    /// Closing hides: this process owns the tray icon, and eframe defaults to
    /// exiting with the last viewport. Only the tray's Quit ends the process.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        match tray::close_action(requested, self.quitting) {
            // Exit: let it through — this close came from the tray's Quit.
            tray::CloseAction::Exit | tray::CloseAction::Nothing => {}
            tray::CloseAction::Hide => {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }
    }

    /// Push the newest entries into the tray menu, only on change: per-frame
    /// rebuilds flicker and on macOS can dismiss an open menu.
    fn sync_tray_recents(&mut self) {
        let recents: Vec<tray::TrayRecent> = self
            .rows
            .iter()
            .take(tray::TRAY_RECENTS)
            .map(|r| tray::TrayRecent {
                id: r.id.clone(),
                // `r.preview` is already masked for a human audience by `rows`,
                // so a Secret is `••••••••` here and never the real value.
                label: tray::recent_label(&r.preview),
                meta: tray::recent_meta(r.author.as_deref(), r.session_id.as_deref()),
            })
            .collect();
        if recents != self.tray_recents {
            self.tray_recents = recents.clone();
            tray::set_recents(recents, self._tray.as_ref());
        }
    }

    /// Perform a fresh poll with current recall_limit and channel settings.
    fn refresh(&mut self) {
        let ch = if self.channel_lane.trim().is_empty() {
            None
        } else {
            Some(self.channel_lane.trim())
        };
        let snapshot = poll::fetch_with_options(&self.socket, self.recall_limit, ch);
        self.apply(snapshot);
    }

    /// Send `Copy { text }` — makes the entry the current clipboard (the
    /// practical "paste from history" flow) — then refresh so it jumps to top.
    fn recopy(&mut self, text: String) {
        self.notice = Some(
            match ipc::roundtrip(
                &self.socket,
                &Request::Copy {
                    text,
                    channel: None,
                    // Re-copy is a person clicking a button: attributed to the
                    // human, not inherited from the entry's original author.
                    author: Some(copycat_core::entry::AUTHOR_UI.to_string()),
                    session_id: None,
                },
            ) {
                Ok(Response::Copied { id }) => format!("re-copied — now current ({id})"),
                Ok(Response::Error { message }) => format!("copy failed: {message}"),
                Ok(other) => format!("copy: unexpected reply {other:?}"),
                Err(e) => format!("copy failed: {e}"),
            },
        );
        self.refresh();
    }

    /// Change the daemon's privacy posture, then refresh so every row re-renders
    /// under it. Privacy is daemon-authoritative — no local copy of the setting.
    fn set_privacy(&mut self, mode: PrivacyMode) {
        self.notice = Some(
            match ipc::roundtrip(&self.socket, &Request::SetPrivacy { mode }) {
                Ok(Response::Privacy { mode }) => format!("privacy: {}", mode.label()),
                Ok(Response::Error { message }) => format!("privacy change failed: {message}"),
                Ok(other) => format!("privacy: unexpected reply {other:?}"),
                Err(e) => format!("privacy change failed: {e}"),
            },
        );
        self.refresh();
    }

    /// Execute a bulk clear command against the daemon.
    fn execute_clear(&mut self, include_pinned: bool, all_channels: bool) {
        let ch = if all_channels || self.channel_lane.trim().is_empty() {
            None
        } else {
            Some(self.channel_lane.trim().to_string())
        };
        let req = Request::Clear {
            include_pinned,
            all_channels,
            channel: ch,
        };
        match ipc::roundtrip(&self.socket, &req) {
            Ok(Response::Cleared { removed }) => {
                self.notice = Some(format!(
                    "Cleared {removed} {} from storage.",
                    if removed == 1 { "entry" } else { "entries" }
                ));
            }
            Ok(Response::Error { message }) => {
                self.notice = Some(format!("Clear failed: {message}"));
            }
            Ok(other) => {
                self.notice = Some(format!("Clear: unexpected response {other:?}"));
            }
            Err(e) => {
                self.notice = Some(format!("Clear error: {e}"));
            }
        }
        self.refresh();
    }

    /// One-line status summary for the bottom bar / the "Status" tray action.
    fn status_line(&self) -> String {
        match &self.status {
            None => "copycatd: not connected".into(),
            Some(s) => format!(
                "copycatd v{} · {} · {} entries held · device {}",
                s.version,
                s.clipboard.label(),
                s.entries,
                s.device
            ),
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let is_alert = self.status.as_ref().is_some_and(|s| s.insecure);
        // Static vector cat — never animates, moves or reflows.
        //
        // [`cat_logo::INK`] on a dark theme, so the header agrees with the tray
        // and window icons; the body text colour on a light one. The theme is
        // not ours to assume: nothing here calls `set_theme`, so egui keeps
        // `ThemePreference::System`, and INK's own justification (dark title
        // bars, taskbars and panels) does not cover a surface this app paints
        // itself. Hardcoding white put a 1–2 px white line on a gray(248)
        // panel at about 1.04:1 contrast — invisible, on light-mode Windows and
        // macOS, with the tray-style escape hatch now deleted.
        //
        // Not [`ACCENT`] either: accent blue marks *agent* provenance in this
        // window, and spending it on the logo too made both mean less. Tint red
        // only in the (rare, non-flickering) insecure state.
        let logo_color = if is_alert {
            ALERT
        } else if ui.visuals().dark_mode {
            egui::Color32::from_rgb(cat_logo::INK[0], cat_logo::INK[1], cat_logo::INK[2])
        } else {
            ui.visuals().text_color()
        };

        ui.horizontal(|ui| {
            let (logo_rect, logo_resp) = ui.allocate_exact_size(
                egui::Vec2::new(LOGO_SIZE * cat_logo::ASPECT, LOGO_SIZE),
                egui::Sense::hover(),
            );
            cat_logo::paint(ui.painter(), logo_rect, logo_color);
            logo_resp.on_hover_text("copycat — shared temporal clipboard");
            ui.separator();

            // Controls stacked to the right of the logo: tabs/filter, then actions.
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::History, "📋 History");
                    ui.selectable_value(&mut self.tab, Tab::Settings, "⚙ Settings");
                    if self.tab == Tab::History {
                        ui.separator();
                        ui.label("Filter:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .hint_text("type to filter (preview, kind, author, session)…")
                                .desired_width(f32::INFINITY),
                        );
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        self.refresh();
                    }
                    if self.tab == Tab::History
                        && !self.filter.is_empty()
                        && ui.button("Clear filter").clicked()
                    {
                        self.filter.clear();
                    }
                    if let Some(n) = &self.notice {
                        ui.separator();
                        ui.label(egui::RichText::new(n).italics().weak());
                    }
                });
            });
        });
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        let filtered = rows::filter_rows(&self.rows, &self.filter);
        if filtered.is_empty() {
            ui.weak(if self.rows.is_empty() {
                "clipboard history is empty (or daemon unreachable)"
            } else {
                "no entries match the filter"
            });
            return;
        }
        let mut clicked: Option<String> = None;
        let mut promote: Option<Option<String>> = None;
        let now = now_millis();
        // bottom_up inside a ScrollArea clips rows once content exceeds the
        // viewport — keep top-down (newest-first input iterated in reverse) +
        // stick_to_bottom.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for row in filtered.iter().rev() {
                    let is_sel = self.selected.as_deref() == Some(row.id.as_str());
                    // `horizontal` bounds the row to content height; a bare
                    // `with_layout` takes the full remaining height.
                    let mut resp = None;
                    ui.horizontal(|ui| {
                        resp = Some(ui.selectable_label(is_sel, row_label(row)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            provenance_column(ui, row, now)
                        });
                    });
                    let Some(resp) = resp else { continue };
                    let resp = resp.on_hover_text(
                        "double-click to make this the current clipboard (paste-ready)",
                    );
                    if resp.double_clicked() {
                        promote = Some(row.text.clone());
                        clicked = Some(row.id.clone());
                    } else if resp.clicked() {
                        clicked = Some(row.id.clone());
                    }
                }
            });
        if let Some(id) = clicked {
            self.selected = Some(id);
        }
        match promote {
            Some(Some(text)) => self.recopy(text),
            Some(None) => {
                self.notice = Some("that entry isn't text — nothing to re-copy".into());
            }
            None => {}
        }
    }

    fn detail(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected.clone() else {
            ui.weak("select an entry to see details");
            return;
        };
        let Some(row) = self.rows.iter().find(|r| r.id == id).cloned() else {
            ui.weak("entry no longer in history");
            return;
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("#{}", row.index)).monospace());
            if row.pinned {
                ui.label("📌 pinned");
            }
            ui.label(egui::RichText::new(format!("{:?}", row.kind)).strong());
            let tag = rows::sensitivity_tag(row.sensitivity);
            if !tag.is_empty() {
                ui.colored_label(TAG_AMBER, tag);
            }
            // Same origin colouring as the list's right-hand column, so an entry
            // doesn't change identity when you select it.
            let origin = rows::Origin::of(row.author.as_deref());
            let (color, glyph) = origin_style(origin, ui);
            let badge = rows::author_badge(row.author.as_deref(), row.session_id.as_deref());
            if badge.is_empty() {
                ui.weak("[unattributed]")
                    .on_hover_text("no author was recorded for this entry");
            } else if glyph.is_empty() {
                ui.colored_label(color, badge);
            } else {
                ui.colored_label(color, format!("{glyph} {badge}"));
            }
            ui.weak(rows::relative_time(row.copied_at, now_millis()))
                .on_hover_text("how long ago this was copied");
        });

        ui.separator();

        if row.masked {
            ui.weak(format!(
                "{} — value hidden ({}). Build with `--features debug-unsafe` and set \
                 COPYCAT_INSECURE to reveal.",
                rows::MASK,
                rows::sensitivity_tag(row.sensitivity),
            ));
        } else if let Some(text) = &row.text {
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .id_salt("detail-text")
                .show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
                });
        } else {
            ui.weak(format!("(non-text entry: {})", row.preview));
        }

        ui.separator();

        ui.horizontal(|ui| {
            let can_copy = row.text.is_some();
            let btn = ui.add_enabled(can_copy, egui::Button::new("Re-copy"));
            if btn.clicked() {
                if let Some(text) = row.text.clone() {
                    self.recopy(text);
                }
            }
            if !can_copy {
                ui.weak("(re-copy is available for text entries)");
            }
        });
    }

    /// Render the interactive Settings & Capacity Management panel.
    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("⚙ Copycat Storage & MCP Settings");
        ui.add_space(8.0);

        ui.group(|ui| {
            ui.heading("Daemon Status & Connectivity");
            ui.separator();
            match &self.status {
                Some(s) => {
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.colored_label(egui::Color32::GREEN, "● Connected");
                    });
                    ui.label(format!("Daemon Version: v{}", s.version));
                    ui.label(format!("Device Hostname: {}", s.device));
                    ui.label(format!("Storage Entries Held: {}", s.entries));
                    ui.label(format!("Backend Type: {}", s.clipboard.label()));
                    ui.label(format!("Socket Path: {}", self.socket.display()));
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.label("Status:");
                        ui.colored_label(egui::Color32::RED, "● Disconnected / Unreachable");
                    });
                    ui.weak("Daemon socket is not responding. Ensure copycatd is running.");
                }
            }
        });

        ui.add_space(12.0);

        ui.group(|ui| {
            ui.heading("Capacity & Channel Settings");
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("History Recall Limit:");
                let prev_limit = self.recall_limit;
                egui::ComboBox::from_id_salt("recall_limit_combo")
                    .selected_text(format!("{} entries", self.recall_limit))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.recall_limit, 50, "50 entries");
                        ui.selectable_value(&mut self.recall_limit, 100, "100 entries");
                        ui.selectable_value(&mut self.recall_limit, 200, "200 entries");
                        ui.selectable_value(&mut self.recall_limit, 500, "500 entries");
                        ui.selectable_value(&mut self.recall_limit, 1000, "1000 entries");
                    });
                if self.recall_limit != prev_limit {
                    self.refresh();
                }
            });

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                ui.label("Channel Lane:");
                let prev_channel = self.channel_lane.clone();
                ui.add(
                    egui::TextEdit::singleline(&mut self.channel_lane)
                        .hint_text("(empty = default OS lane; or e.g. build, mcp)")
                        .desired_width(200.0),
                );
                if ui.button("Apply Lane").clicked() || self.channel_lane != prev_channel {
                    self.refresh();
                }
            });
            ui.weak("Pass a channel name to inspect private agent-to-agent mailboxes.");
        });

        ui.add_space(12.0);

        ui.group(|ui| {
            ui.heading("Privacy");
            ui.separator();
            // The daemon is the source of truth; binding the posture here also
            // ends the borrow of `self` before an arm calls `set_privacy`.
            let reported = self.status.as_ref().map(|s| s.privacy);
            match reported {
                Some(active) => {
                    let mut chosen = active;
                    ui.horizontal(|ui| {
                        ui.label("Protection:");
                        egui::ComboBox::from_id_salt("privacy_mode_combo")
                            .selected_text(active.label())
                            .show_ui(ui, |ui| {
                                for mode in PrivacyMode::ALL {
                                    ui.selectable_value(&mut chosen, mode, mode.label());
                                }
                            });
                    });
                    ui.weak(active.describe());
                    if active == PrivacyMode::Off {
                        ui.colored_label(
                            ALERT,
                            "⚠ Detection is off — secrets in your history are shown in full.",
                        );
                    }
                    if chosen != active {
                        self.set_privacy(chosen);
                    }
                }
                None => {
                    ui.weak("Connect to copycatd to see or change the privacy mode.");
                }
            }
            ui.weak(
                "Applies daemon-wide and to history you already copied — the CLI and any \
                 connected agent follow the same setting.",
            );
        });

        ui.add_space(12.0);

        ui.group(|ui| {
            ui.heading("Purge & Clear Storage");
            ui.separator();
            ui.label("Manage and free up local clipboard history:");

            ui.horizontal(|ui| {
                if ui.button("Clear Unpinned History").clicked() {
                    self.execute_clear(false, false);
                }
                ui.weak("Removes unpinned entries in the current channel.");
            });

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if ui.button("Purge Pinned & Unpinned").clicked() {
                    self.execute_clear(true, false);
                }
                ui.weak("Removes ALL entries in the current channel.");
            });

            ui.add_space(4.0);

            ui.horizontal(|ui| {
                if ui.button("Purge ALL Channels").clicked() {
                    self.execute_clear(true, true);
                }
                ui.weak("Completely empties history across every named lane.");
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(self.status_line());
            if self.status.as_ref().is_some_and(|s| s.insecure) {
                ui.separator();
                ui.label(
                    egui::RichText::new(" INSECURE ")
                        .strong()
                        .color(egui::Color32::WHITE)
                        .background_color(ALERT),
                )
                .on_hover_text("Daemon is in insecure debug mode: Secret/PII shown in cleartext.");
            }
            // A protection-off state must announce itself, like INSECURE does —
            // the whole point of masking is that you notice when it isn't there.
            if self
                .status
                .as_ref()
                .is_some_and(|s| s.privacy == PrivacyMode::Off)
            {
                ui.separator();
                ui.label(
                    egui::RichText::new(" PRIVACY OFF ")
                        .strong()
                        .color(egui::Color32::WHITE)
                        .background_color(ALERT),
                )
                .on_hover_text(
                    "No secret/PII detection: history is shown in full. Settings → Privacy.",
                );
            }
            if let Some(err) = &self.conn_error {
                ui.separator();
                ui.colored_label(ALERT, format!("⚠ {err}"));
            }
        });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_poll();
        self.sync_tray_recents();
        self.handle_tray(ctx);
        self.handle_close_request(ctx);
        ctx.request_repaint_after(IDLE_REPAINT);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(2.0);
            self.status_bar(ui);
            ui.add_space(2.0);
        });

        match self.tab {
            Tab::History => {
                // Detail pane docked above the status bar.
                egui::Panel::bottom("detail")
                    .resizable(true)
                    .default_size(200.0)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        self.detail(ui);
                    });

                egui::CentralPanel::default().show(ui, |ui| {
                    self.list(ui);
                });
            }
            Tab::Settings => {
                egui::CentralPanel::default().show(ui, |ui| {
                    self.settings_panel(ui);
                });
            }
        }
    }
}

/// Show and raise the window (used by the tray "Open"/icon-click).
fn raise_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(ViewportCommand::Focus);
}

/// Wall-clock now, in the same unix-millis units as `Entry::copied_at`.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How an entry's origin is coloured, and the mark that precedes its author.
/// The glyph carries the human/agent split without colour, so it survives
/// greyscale and red-green colour deficiency.
fn origin_style(origin: rows::Origin, ui: &egui::Ui) -> (egui::Color32, &'static str) {
    match origin {
        rows::Origin::Human => (HUMAN, ""),
        rows::Origin::Agent => (ACCENT, rows::AGENT_GLYPH),
        // Unattributed: recede into the theme's weak colour.
        rows::Origin::Unknown => (ui.visuals().weak_text_color(), ""),
    }
}

/// The right-hand metadata column for one history row: age, session, author.
/// Must be called inside a `right_to_left` layout — widgets are added
/// rightmost-first, so the visual order is `age · session · author`.
fn provenance_column(ui: &mut egui::Ui, row: &Row, now: i64) {
    let origin = rows::Origin::of(row.author.as_deref());
    let (color, glyph) = origin_style(origin, ui);
    let label = rows::author_label(row.author.as_deref());
    let author = if glyph.is_empty() {
        label
    } else {
        format!("{glyph} {label}")
    };

    // Hover carries the full, unabbreviated provenance.
    let badge = rows::author_badge(row.author.as_deref(), row.session_id.as_deref());
    let hover = if badge.is_empty() {
        "no provenance recorded — this entry predates author tracking, or the \
         client that copied it declined to identify itself"
            .to_string()
    } else {
        format!("copied by {badge}")
    };

    ui.add(
        egui::Label::new(egui::RichText::new(author).small().color(color))
            .selectable(false)
            .wrap_mode(egui::TextWrapMode::Extend),
    )
    .on_hover_text(hover);

    // Shown only when a session was recorded — a human Ctrl+C has none.
    if let Some(session) = &row.session_id {
        ui.add(
            egui::Label::new(
                egui::RichText::new(rows::session_label(session))
                    .small()
                    .weak(),
            )
            .selectable(false)
            .wrap_mode(egui::TextWrapMode::Extend),
        )
        .on_hover_text(format!("session {session}"));
    }

    ui.add(
        egui::Label::new(
            egui::RichText::new(rows::relative_time(row.copied_at, now))
                .small()
                .weak(),
        )
        .selectable(false)
        .wrap_mode(egui::TextWrapMode::Extend),
    )
    .on_hover_text("how long ago this was copied");
}

/// The compact one-line label for a row. Provenance lives in
/// [`provenance_column`] instead, so it can't push the preview around.
fn row_label(row: &Row) -> String {
    let pin = if row.pinned { "📌" } else { "  " };
    let tag = rows::sensitivity_tag(row.sensitivity);
    let tag = if tag.is_empty() {
        String::new()
    } else {
        format!("{tag} ")
    };
    format!(
        "{:>3} {pin} {:<11} {tag}{}",
        row.index,
        format!("{:?}", row.kind),
        row.preview
    )
}
