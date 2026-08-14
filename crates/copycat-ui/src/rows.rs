//! Pure, GUI-free row logic — the part that can (and must) be unit-tested.
//!
//! Everything the window renders is derived here first, so the correctness-
//! critical decisions live in plain functions with no egui/winit/socket
//! dependencies. The **masking, tag, badge and preview** vocabulary is shared
//! with the CLI and MCP server through [`copycat_core::view`] (re-exported below
//! so existing call sites keep working); this module only adds the GUI-shaped
//! [`Row`] projection and the history [`filter_rows`] search over it.
//!
//! Filtering matches only the *displayed* text (masked preview, kind label,
//! sensitivity tag, author badge) — never the hidden raw bytes of a Secret/PII
//! entry. So a masked entry can't be fished out of history by typing its secret
//! value; if you can't see it, you can't search it either.

use copycat_core::entry::{ContentKind, Entry, Sensitivity};
use copycat_core::view::{self, Audience};

// Re-exported so `app.rs` reaches the shared vocabulary through `rows::` rather
// than importing core's `view` a second time — there is one place a row's
// rendering tokens come from.
//
// `allow(unused_imports)`: this is a *binary* crate, so a `pub use` nothing in
// the crate names counts as unused, and most of these are named only by the
// `gui`-gated `app.rs`. Without the allow, `--no-default-features` (the headless
// build CI runs the unit tests under) fails on re-exports that are perfectly
// live in the real GUI build.
#[allow(unused_imports)]
pub use copycat_core::entry::Origin;
#[allow(unused_imports)]
pub use copycat_core::view::{
    author_badge, author_label, relative_time, sensitivity_tag, session_label, AGENT_GLYPH, MASK,
};

/// The GUI's masking gate: a human at the window, so both Secret and PII hide.
///
/// Note what this does *not* consult: the user's
/// [privacy mode](copycat_core::privacy). The daemon has already applied it to
/// every entry it sent (`privacy::project`), so under the default `Guarded`
/// posture an email arrives marked `Normal` and is simply shown here. The window
/// masks whatever the daemon marked — it does not hold a second opinion about
/// what is sensitive. Changing the mode is done in the Settings tab, which asks
/// the daemon; see `app::App::set_privacy`.
#[inline]
pub fn should_mask(sensitivity: Sensitivity) -> bool {
    view::should_mask(sensitivity, Audience::Human)
}

/// The GUI's one-line preview: masked for a human audience.
pub fn preview(e: &Entry) -> String {
    view::preview(e, Audience::Human)
}

/// One history entry, flattened to exactly what a row needs to render. Holds no
/// `Entry` (and thus no live `Drop`-zeroizing payload); the raw text is kept only
/// for text entries and only to power the "Re-copy" button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Recency index as returned by `Recall` (0 = current clipboard).
    pub index: usize,
    /// Stable entry id.
    pub id: String,
    /// Pinned entries are exempt from eviction/expiry.
    pub pinned: bool,
    /// Best-guess semantic type.
    pub kind: ContentKind,
    /// Sensitivity classification (drives masking).
    pub sensitivity: Sensitivity,
    /// Unix-millis the entry was copied — rendered as an age chip
    /// ([`copycat_core::view::relative_time`]) in the row's provenance column.
    pub copied_at: i64,
    /// Optional author provenance (e.g. "agent:mcp", "human:user").
    pub author: Option<String>,
    /// Optional session identifier (e.g. "sess_73b15e1a").
    pub session_id: Option<String>,
    /// One-line, already-masked-if-needed preview. Safe to display verbatim.
    pub preview: String,
    /// Whether [`Self::preview`] is the mask (i.e. the real value is hidden).
    pub masked: bool,
    /// Raw text payload, for `Re-copy`. `None` for non-text (image/files) entries.
    /// Present even for masked rows: re-copying puts the value back on the
    /// clipboard (a legitimate user action); masking only governs *display*.
    pub text: Option<String>,
}

/// Map the daemon's recall result (most-recent first) into display rows.
pub fn rows_from_entries(entries: &[Entry]) -> Vec<Row> {
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| Row {
            index: i,
            id: e.id.clone(),
            pinned: e.pinned,
            kind: e.kind,
            sensitivity: e.sensitivity,
            copied_at: e.copied_at,
            author: e.source.author.clone(),
            session_id: e.source.session_id.clone(),
            preview: preview(e),
            masked: should_mask(e.sensitivity),
            text: e.as_text().map(|s| s.to_string()),
        })
        .collect()
}

/// Does a single row match a lowercased, trimmed query? Only the visible surfaces
/// participate (preview, kind label, sensitivity tag, author badge) — never the hidden payload.
fn row_matches(r: &Row, needle: &str) -> bool {
    if r.preview.to_lowercase().contains(needle) {
        return true;
    }
    if format!("{:?}", r.kind).to_lowercase().contains(needle) {
        return true;
    }
    if sensitivity_tag(r.sensitivity)
        .to_lowercase()
        .contains(needle)
    {
        return true;
    }
    if let Some(author) = &r.author {
        if author.to_lowercase().contains(needle) {
            return true;
        }
    }
    if let Some(session_id) = &r.session_id {
        if session_id.to_lowercase().contains(needle) {
            return true;
        }
    }
    false
}

/// Filter rows by the user's query. An empty/whitespace query returns everything.
/// Borrows the input so the caller keeps ownership of the full row set.
pub fn filter_rows<'a>(rows: &'a [Row], query: &str) -> Vec<&'a Row> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return rows.iter().collect();
    }
    rows.iter().filter(|r| row_matches(r, &needle)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use copycat_core::entry::Source;

    fn src() -> Source {
        Source::simple("t")
    }

    fn text_entry(id: &str, text: &str) -> Entry {
        Entry::from_text(id, text, 0, src())
    }

    #[test]
    fn maps_entries_to_rows_preserving_recency_and_metadata() {
        let entries = vec![
            text_entry("a", "https://example.com/x"),
            text_entry("b", "hello world"),
        ];
        let rows = rows_from_entries(&entries);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 0);
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[0].kind, ContentKind::Url);
        assert_eq!(rows[1].index, 1);
        assert_eq!(rows[1].preview, "hello world");
        assert_eq!(rows[1].text.as_deref(), Some("hello world"));
        assert!(!rows[1].masked);
    }

    #[test]
    fn non_text_entries_have_no_recopy_text() {
        use copycat_core::entry::Content;
        let e = Entry {
            id: "f".into(),
            content: Content::Files(vec!["/a".into(), "/b".into()]),
            kind: ContentKind::Files,
            copied_at: 0,
            source: src(),
            sensitivity: Sensitivity::Normal,
            pinned: false,
        };
        let rows = rows_from_entries(std::slice::from_ref(&e));
        assert_eq!(rows[0].text, None);
        assert_eq!(rows[0].preview, "<2 file(s)>");
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let rows = rows_from_entries(&[text_entry("a", "alpha"), text_entry("b", "beta")]);
        assert_eq!(filter_rows(&rows, "   ").len(), 2);
    }

    #[test]
    fn filter_matches_preview_and_kind_case_insensitively() {
        let rows = rows_from_entries(&[
            text_entry("a", "https://example.com"),
            text_entry("b", "just some plain text"),
        ]);
        // matches the preview text
        let m = filter_rows(&rows, "PLAIN");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].id, "b");
        // matches the kind label ("Url")
        let k = filter_rows(&rows, "url");
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].id, "a");
    }

    #[test]
    fn rows_carry_provenance_and_timestamp_for_the_right_hand_column() {
        let e = Entry::from_text(
            "p",
            "value an agent staged",
            1_700_000_000_000,
            Source::with_provenance(
                "t",
                None,
                Some("agent:claude-3-5".into()),
                Some("session_73b15e1a".into()),
            ),
        );
        let rows = rows_from_entries(std::slice::from_ref(&e));
        assert_eq!(rows[0].copied_at, 1_700_000_000_000);
        assert_eq!(rows[0].author.as_deref(), Some("agent:claude-3-5"));
        assert_eq!(rows[0].session_id.as_deref(), Some("session_73b15e1a"));
        // ...and those map to what the row actually draws.
        assert_eq!(Origin::of(rows[0].author.as_deref()), Origin::Agent);
        assert_eq!(author_label(rows[0].author.as_deref()), "claude-3-5");
        assert_eq!(
            session_label(rows[0].session_id.as_ref().unwrap()),
            "73b15e1a"
        );
        assert_eq!(
            relative_time(rows[0].copied_at, rows[0].copied_at + 180_000),
            "3m"
        );
    }

    #[test]
    fn rows_without_provenance_still_render_something() {
        // Entries minted before provenance was wired carry no author/session.
        let rows = rows_from_entries(&[text_entry("old", "legacy entry")]);
        assert_eq!(rows[0].author, None);
        assert_eq!(rows[0].session_id, None);
        assert_eq!(Origin::of(rows[0].author.as_deref()), Origin::Unknown);
        // The label must never be empty or the literal "None" — an unattributed
        // row shows a placeholder, not a gap.
        let label = author_label(rows[0].author.as_deref());
        assert!(!label.is_empty());
        assert_ne!(label, "None");
    }

    #[test]
    fn filter_matches_provenance_author_and_session() {
        let e1 = Entry::from_text(
            "1",
            "val1",
            0,
            Source::with_provenance("t", None, Some("agent:mcp".into()), Some("sess_abc".into())),
        );
        let e2 = Entry::from_text(
            "2",
            "val2",
            0,
            Source::with_provenance("t", None, Some("human:alice".into()), None),
        );
        let rows = rows_from_entries(&[e1, e2]);

        let m_agent = filter_rows(&rows, "agent");
        assert_eq!(m_agent.len(), 1);
        assert_eq!(m_agent[0].id, "1");

        let m_sess = filter_rows(&rows, "sess_abc");
        assert_eq!(m_sess.len(), 1);
        assert_eq!(m_sess[0].id, "1");

        let m_human = filter_rows(&rows, "alice");
        assert_eq!(m_human.len(), 1);
        assert_eq!(m_human[0].id, "2");
    }

    // ---- masking: default-safe --------------------------------------------

    /// Serializes tests that touch the process-global COPYCAT_INSECURE env, which
    /// `reveal_secrets()` consults. Recovers from poisoning like core's tests.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn secrets_are_masked_by_default() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // A GitHub token classifies as Secret.
        let e = text_entry("s", "ghp_0123456789abcdefghij");
        assert_eq!(e.sensitivity, Sensitivity::Secret);
        assert!(should_mask(e.sensitivity));
        let rows = rows_from_entries(std::slice::from_ref(&e));
        assert_eq!(rows[0].preview, MASK);
        assert!(rows[0].masked);
        assert!(
            !rows[0].preview.contains("ghp_"),
            "raw secret must not appear"
        );
        // ...but the raw text is still available for an explicit Re-copy action.
        assert_eq!(rows[0].text.as_deref(), Some("ghp_0123456789abcdefghij"));
    }

    #[test]
    fn an_entry_the_daemon_left_unmarked_renders_in_full() {
        // What the default `Guarded` posture puts on the wire for an email: the
        // daemon projected it to Normal, so the window shows it and the filter
        // can find it. This is the usability half of the privacy dial.
        let mut e = text_entry("p", "dean@example.com");
        e.sensitivity = Sensitivity::Normal;
        let rows = rows_from_entries(std::slice::from_ref(&e));
        assert!(!rows[0].masked);
        assert_eq!(rows[0].preview, "dean@example.com");
        assert_eq!(filter_rows(&rows, "example.com").len(), 1);
    }

    /// An entry the daemon *did* mark as PII (i.e. the `Strict` posture) is
    /// masked for the human at the window, and can't be fished out by filtering.
    #[test]
    fn entries_marked_pii_are_masked() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let e = text_entry("p", "dean@example.com");
        assert_eq!(e.sensitivity, Sensitivity::Pii);
        let rows = rows_from_entries(std::slice::from_ref(&e));
        assert_eq!(rows[0].preview, MASK);
        // filtering can't fish the address out of a masked row
        assert!(filter_rows(&rows, "example.com").is_empty());
    }

    /// Feature-gated exactly like `render.rs`'s reveal test: only a build with
    /// `--features debug-unsafe` (which forwards to core) can lift the mask, and
    /// only when COPYCAT_INSECURE is also set.
    #[test]
    #[cfg(feature = "debug-unsafe")]
    fn insecure_mode_reveals_secret() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COPYCAT_INSECURE", "1");
        let revealed = {
            let e = text_entry("s", "ghp_0123456789abcdefghij");
            assert!(!should_mask(e.sensitivity), "gate lifted with feature+env");
            rows_from_entries(std::slice::from_ref(&e))
        };
        std::env::remove_var("COPYCAT_INSECURE");
        assert_eq!(revealed[0].preview, "ghp_0123456789abcdefghij");
        assert!(!revealed[0].masked);
    }
}
