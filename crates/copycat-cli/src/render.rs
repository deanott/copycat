//! Human-readable rendering of daemon responses. Pure functions (no I/O) so they
//! can be unit-tested without a running daemon.

use copycat_core::entry::{Content, Entry};
use copycat_core::proto::Response;
use copycat_core::view::{self, Audience};

/// Render a response as the text to print, or `None` when there's nothing to show
/// (e.g. an empty clipboard, or an error which `main` reports separately).
pub fn human(resp: &Response) -> Option<String> {
    match resp {
        Response::Pong => Some("pong\n".into()),
        Response::Copied { id } => Some(format!("copied ({id})\n")),
        // `paste`: emit the raw payload verbatim so it pipes cleanly.
        Response::Current { entry } => entry.as_ref().map(payload_text),
        Response::Entries { entries } => Some(render_entries(entries)),
        Response::Pinned { id, pinned } => Some(format!(
            "{} {id}\n",
            if *pinned { "pinned" } else { "unpinned" }
        )),
        // Confirm what is now pasteable — but via the masked preview, so a
        // `copycat use` of a password never lands in terminal scrollback.
        Response::Promoted { entry } => Some(format!(
            "now current ({}) — paste with Ctrl/Cmd+V: {}\n",
            entry.id,
            view::preview(entry, Audience::Human)
        )),
        Response::Deleted { id } => Some(format!("deleted {id}\n")),
        Response::Cleared { removed } => Some(format!(
            "cleared {removed} {}\n",
            if *removed == 1 { "entry" } else { "entries" }
        )),
        // A fulfilled `wait`: emit the handed-off payload verbatim, like `paste`.
        Response::Handoff { entry } => Some(payload_text(entry)),
        // `main` reports the timeout to stderr with a non-zero exit.
        Response::WaitTimeout => None,
        Response::Privacy { mode } => {
            Some(format!("privacy: {} — {}\n", mode.label(), mode.describe()))
        }
        Response::Status(s) => Some(render_status(s)),
        Response::Error { .. } => None,
    }
}

/// The verbatim payload of an entry (for `paste`). Non-text entries render a
/// short placeholder rather than raw bytes.
fn payload_text(e: &Entry) -> String {
    match &e.content {
        Content::Text(t) => t.clone(),
        Content::Image { mime, .. } => format!("[image {mime}]\n"),
        Content::Files(fs) => {
            let mut s = fs.join("\n");
            s.push('\n');
            s
        }
    }
}

fn render_entries(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "(clipboard history is empty)\n".into();
    }
    let mut out = String::new();
    for (i, e) in entries.iter().enumerate() {
        out.push_str(&format!(
            "{:>3}. {}{:<8} {:<8}  {}\n",
            i,
            pin_marker(e.pinned),
            format!("{:?}", e.kind),
            view::sensitivity_tag(e.sensitivity),
            view::preview(e, Audience::Human),
        ));
    }
    out
}

fn render_status(s: &copycat_core::proto::DaemonStatus) -> String {
    const CAT_TOP: &str = "  ᗢ";
    const CAT_BOTTOM: &str = "ᓚᘏ ";
    let insecure = if s.insecure {
        "\n           ⚠ mode:   INSECURE (Secret/PII shown in cleartext)"
    } else {
        ""
    };
    // Surface the privacy posture always, not just when it deviates: "am I
    // protected?" should be answerable by looking.
    let privacy = format!("\n           privacy: {}", s.privacy.label());
    format!(
        " {}  COPYCATD v{}\n {}  device:   {}\n           backend:  {}\n           entries:  {}{}{}\n",
        CAT_TOP,
        s.version,
        CAT_BOTTOM,
        s.device,
        s.clipboard.label(),
        s.entries,
        privacy,
        insecure
    )
}

fn pin_marker(pinned: bool) -> &'static str {
    if pinned {
        "📌 "
    } else {
        "   "
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copycat_core::entry::{ContentKind, Sensitivity, Source};
    use copycat_core::proto::DaemonStatus;

    fn entry(text: &str) -> Entry {
        Entry::from_text("id1", text, 0, Source::simple("t"))
    }

    #[test]
    fn paste_emits_raw_text() {
        let r = Response::Current {
            entry: Some(entry("hello world")),
        };
        assert_eq!(human(&r).as_deref(), Some("hello world"));
    }

    #[test]
    fn empty_clipboard_shows_nothing() {
        assert_eq!(human(&Response::Current { entry: None }), None);
    }

    #[test]
    fn copied_confirms_with_id() {
        let r = Response::Copied { id: "abc".into() };
        assert_eq!(human(&r).as_deref(), Some("copied (abc)\n"));
    }

    /// Serializes tests that read/mutate the process-global COPYCAT_INSECURE env
    /// (which `preview` consults). Recovers from poisoning so one panicking test
    /// doesn't cascade.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn recall_masks_secrets_in_preview() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // A GitHub token classifies as Secret and must be masked in listings.
        let secret = entry("ghp_0123456789abcdefghij");
        assert_eq!(secret.sensitivity, Sensitivity::Secret);
        let out = render_entries(std::slice::from_ref(&secret));
        assert!(out.contains("[SECRET]"), "tag shown: {out}");
        assert!(out.contains("••••"), "value masked: {out}");
        assert!(!out.contains("ghp_"), "raw secret must not appear: {out}");
    }

    #[test]
    #[cfg(feature = "debug-unsafe")]
    fn insecure_mode_reveals_secret_in_preview() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COPYCAT_INSECURE", "1");
        let out = render_entries(&[entry("ghp_0123456789abcdefghij")]);
        std::env::remove_var("COPYCAT_INSECURE");
        assert!(
            out.contains("ghp_0123456789abcdefghij"),
            "debug reveal: {out}"
        );
    }

    #[test]
    fn recall_collapses_and_truncates_preview() {
        let long = "a".repeat(200);
        let out = render_entries(&[entry(&long)]);
        assert!(out.contains('…'), "long preview truncated: {out}");
        assert!(out.lines().next().unwrap().chars().count() < 120);
    }

    #[test]
    fn recall_empty_is_friendly() {
        assert_eq!(
            human(&Response::Entries { entries: vec![] }).as_deref(),
            Some("(clipboard history is empty)\n")
        );
    }

    #[test]
    fn status_reports_headless_backend() {
        let r = Response::Status(DaemonStatus {
            version: "1.0.0".into(),
            clipboard: copycat_core::proto::ClipboardBackendInfo::Headless,
            entries: 3,
            device: "box".into(),
            insecure: false,
            privacy: copycat_core::privacy::Mode::Guarded,
        });
        let out = human(&r).unwrap();
        assert!(out.contains("headless"));
        assert!(out.contains("entries:  3"));
        assert!(out.contains("box"));
        // The privacy posture is part of "what is this daemon doing".
        assert!(out.contains("privacy: Guarded"), "{out}");
    }

    #[test]
    fn set_privacy_reply_explains_the_new_posture() {
        let out = human(&Response::Privacy {
            mode: copycat_core::privacy::Mode::Off,
        })
        .unwrap();
        assert!(out.contains("privacy: Off"), "{out}");
        assert!(out.contains("Nothing is hidden"), "{out}");
    }

    #[test]
    fn error_renders_as_none() {
        assert_eq!(
            human(&Response::Error {
                message: "x".into()
            }),
            None
        );
    }

    #[test]
    fn handoff_emits_raw_payload() {
        let r = Response::Handoff {
            entry: entry("handed over"),
        };
        assert_eq!(human(&r).as_deref(), Some("handed over"));
    }

    #[test]
    fn wait_timeout_renders_as_none() {
        assert_eq!(human(&Response::WaitTimeout), None);
    }

    #[test]
    fn promoted_confirms_what_is_now_pasteable() {
        let r = Response::Promoted {
            entry: entry("418 Kestrel Row"),
        };
        let out = human(&r).unwrap();
        assert!(out.contains("now current"), "{out}");
        assert!(out.contains("Ctrl/Cmd+V"), "{out}");
        assert!(out.contains("418 Kestrel Row"), "{out}");
    }

    #[test]
    fn promoted_masks_a_secret_in_the_confirmation() {
        // `copycat use <secret>` must put it on the clipboard without printing it.
        let secret = entry("ghp_0123456789abcdefghij");
        assert_eq!(secret.sensitivity, Sensitivity::Secret);
        let out = human(&Response::Promoted { entry: secret }).unwrap();
        assert!(out.contains("now current"), "{out}");
        assert!(out.contains("••••"), "value masked: {out}");
        assert!(
            !out.contains("ghp_"),
            "raw secret must not reach scrollback: {out}"
        );
    }

    #[test]
    fn files_paste_lists_paths() {
        let e = Entry {
            id: "f".into(),
            content: Content::Files(vec!["/a".into(), "/b".into()]),
            kind: ContentKind::Files,
            copied_at: 0,
            source: Source::simple("t"),
            sensitivity: Sensitivity::Normal,
            pinned: false,
        };
        let r = Response::Current { entry: Some(e) };
        assert_eq!(human(&r).as_deref(), Some("/a\n/b\n"));
    }
}
