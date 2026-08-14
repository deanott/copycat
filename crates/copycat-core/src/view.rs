//! Shared masking/preview policy — every surface (CLI, GUI, MCP) makes the same
//! masking decision here, once; surfaces own layout only.
//!
//! [`Audience::Human`] masks both `Secret` and `Pii`. [`Audience::Model`] masks
//! per [`crate::privacy::reveal_to_model`]: `Secret` always, `Pii` only under
//! [`privacy::Mode::Strict`](crate::privacy::Mode::Strict). The daemon has
//! already projected its privacy mode onto every entry it hands out, so human
//! surfaces never consult the mode. Model surfaces re-derive the credential
//! floor via [`model_sensitivity`], so `Secret` is withheld even from a daemon
//! running with privacy `Off`.

use crate::entry::{Content, Entry, Origin, Sensitivity};
use crate::privacy::Mode;

/// The mask shown in place of a value that must not be displayed.
pub const MASK: &str = "••••••••";

/// One-line preview length cap, in characters.
pub const PREVIEW_MAX: usize = 72;

/// Who is going to read the rendered value — the only input, besides sensitivity,
/// that changes the masking decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// A person at a terminal or GUI. Masks both `Secret` and `Pii` so neither
    /// lands in scrollback or on screen — whichever of those the daemon's
    /// privacy mode left marked.
    Human,
    /// The LLM behind an MCP client, under the daemon's reported privacy mode.
    /// Masks per [`crate::privacy::reveal_to_model`]: `Secret` always, `Pii`
    /// only under [`Mode::Strict`].
    Model { mode: Mode },
}

/// The single masking decision. `true` ⇒ the value must be hidden and rendered as
/// [`MASK`]. The compile-gated insecure debug mode lifts all masking.
#[inline]
pub fn should_mask(sensitivity: Sensitivity, audience: Audience) -> bool {
    if crate::insecure::reveal_secrets() {
        return false;
    }
    match audience {
        Audience::Human => matches!(sensitivity, Sensitivity::Secret | Sensitivity::Pii),
        Audience::Model { mode } => !crate::privacy::reveal_to_model(sensitivity, mode),
    }
}

/// The sensitivity a **model-facing** surface must judge `entry` by: the higher
/// of what the daemon reported and a local credential re-scan of the payload.
///
/// This is the floor that survives every setting. A daemon running with privacy
/// [`Off`](crate::privacy::Mode::Off) reports its entries as `Normal` — correct
/// for the human who asked for that, and wrong for an LLM, which must never be
/// handed a token no matter whose preference is in play (see `CLAUDE.md`). The
/// re-scan is [`crate::pii::is_secret`], the same detector the daemon used, so
/// this is one policy applied twice, not a second opinion.
///
/// Human surfaces deliberately do **not** call this: overriding the user's own
/// "show me my clipboard" setting on their own screen is not this function's job.
pub fn model_sensitivity(entry: &Entry) -> Sensitivity {
    match entry.sensitivity {
        // Already at the ceiling; no re-scan needed.
        Sensitivity::Secret => Sensitivity::Secret,
        reported => match entry.as_text() {
            Some(t) if crate::pii::is_secret(t) => Sensitivity::Secret,
            _ => reported,
        },
    }
}

/// Canonical sensitivity tag: `""`, `"[PII]"`, or `"[SECRET]"`. Unpadded — a
/// surface that wants column alignment applies its own width (e.g. `{:<8}`).
pub fn sensitivity_tag(s: Sensitivity) -> &'static str {
    match s {
        Sensitivity::Normal => "",
        Sensitivity::Pii => "[PII]",
        Sensitivity::Secret => "[SECRET]",
    }
}

/// Compact provenance badge: `"[author:session]"`, `"[author]"`,
/// `"[session:session]"`, or `""` when neither is present.
pub fn author_badge(author: Option<&str>, session_id: Option<&str>) -> String {
    match (author, session_id) {
        (Some(a), Some(s)) => format!("[{a}:{s}]"),
        (Some(a), None) => format!("[{a}]"),
        (None, Some(s)) => format!("[session:{s}]"),
        (None, None) => String::new(),
    }
}

/// The mark drawn beside an agent-authored entry: copycat's cat head, `ᗢ`.
/// Only agents get a glyph, so the human/agent split survives greyscale and
/// colour-blindness — the mark's presence carries it, colour only reinforces.
pub const AGENT_GLYPH: &str = "ᗢ";

/// The short name to print for an author, e.g. `"you"`, `"claude-3-5"`, `"?"`.
///
/// Every `human:*` author collapses to a single word: from the reader's seat the
/// interesting fact is that a *person* did it, not which of their own surfaces
/// they used (that detail stays in [`author_badge`] / the hover text). Agents get
/// their suffix, because *which* agent is exactly what distinguishes them.
pub fn author_label(author: Option<&str>) -> String {
    match Origin::of(author) {
        Origin::Human => "you".to_string(),
        // `Origin::Agent` guarantees the prefix, so the suffix is well-defined;
        // an empty suffix (bare "agent:") still needs a word to render.
        Origin::Agent => match author.and_then(|a| a.split_once(':')).map(|(_, s)| s) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => "agent".to_string(),
        },
        Origin::Unknown => "?".to_string(),
    }
}

/// A session id trimmed for display: the boilerplate `session_`/`sess_` prefix
/// dropped and the remainder capped, so `"session_73b15e1a9f"` reads `"73b15e1a"`.
///
/// NOTE: this is an *id*, not a title — copycat has no session-title concept
/// today (see `Source::session_id`). If sessions ever gain a human name, it
/// belongs in a new field rather than being parsed back out of the id.
pub fn session_label(session_id: &str) -> String {
    let s = session_id
        .strip_prefix("session_")
        .or_else(|| session_id.strip_prefix("sess_"))
        .unwrap_or(session_id);
    truncate(s, 8)
}

/// How long ago an entry was copied, as a compact chip: `"now"`, `"45s"`,
/// `"12m"`, `"3h"`, `"2d"`.
///
/// Relative rather than wall-clock on purpose. copycat is a *temporal* clipboard
/// whose entries age out like a cache, so "how stale is this" is the question a
/// reader actually has — and it needs no timezone database, keeping this crate
/// dependency-free and the function pure (the caller passes `now_millis`).
pub fn relative_time(copied_at_millis: i64, now_millis: i64) -> String {
    // A clock that stepped backwards (NTP, suspend) must not print "-3s".
    let secs = now_millis.saturating_sub(copied_at_millis).max(0) / 1000;
    match secs {
        0 => "now".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The unmasked one-line preview of an entry's content: whitespace collapsed and
/// capped at [`PREVIEW_MAX`] chars with an ellipsis. Non-text entries render a
/// short placeholder. This never consults sensitivity — callers gate it through
/// [`preview`] (or [`should_mask`]) to decide whether the value may be shown.
pub fn preview_text(content: &Content) -> String {
    let raw = match content {
        Content::Text(t) => t.clone(),
        Content::Image { mime, .. } => format!("<image {mime}>"),
        Content::Files(fs) => format!("<{} file(s)>", fs.len()),
    };
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, PREVIEW_MAX)
}

/// A one-line preview for `entry`, masked to [`MASK`] when [`should_mask`] says so
/// for `audience`, else the collapsed/truncated value.
pub fn preview(entry: &Entry, audience: Audience) -> String {
    if should_mask(entry.sensitivity, audience) {
        return MASK.to_string();
    }
    preview_text(&entry.content)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Content, Entry, Source};

    fn entry(text: &str) -> Entry {
        Entry::from_text("id1", text, 0, Source::simple("t"))
    }

    /// Serializes tests that read the process-global COPYCAT_INSECURE env, which
    /// `reveal_secrets()` consults. Recovers from poisoning so one panicking test
    /// doesn't cascade.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tag_is_unpadded_and_canonical() {
        assert_eq!(sensitivity_tag(Sensitivity::Normal), "");
        assert_eq!(sensitivity_tag(Sensitivity::Pii), "[PII]");
        assert_eq!(sensitivity_tag(Sensitivity::Secret), "[SECRET]");
    }

    #[test]
    fn author_badge_shapes() {
        assert_eq!(
            author_badge(Some("agent:mcp"), Some("s1")),
            "[agent:mcp:s1]"
        );
        assert_eq!(author_badge(Some("human:user"), None), "[human:user]");
        assert_eq!(author_badge(None, Some("s2")), "[session:s2]");
        assert_eq!(author_badge(None, None), "");
    }

    #[test]
    fn author_label_collapses_humans_and_names_agents() {
        use crate::entry::{AUTHOR_CLI, AUTHOR_MCP, AUTHOR_OS_CLIPBOARD, AUTHOR_UI};
        // Every human surface reads as one word — which surface is not the point.
        for a in [AUTHOR_OS_CLIPBOARD, AUTHOR_CLI, AUTHOR_UI] {
            assert_eq!(author_label(Some(a)), "you", "{a}");
        }
        // Agents keep their identity: *which* agent is the distinguishing fact.
        assert_eq!(author_label(Some(AUTHOR_MCP)), "mcp");
        assert_eq!(author_label(Some("agent:claude-3-5")), "claude-3-5");
        assert_eq!(author_label(Some("agent:")), "agent");
    }

    #[test]
    fn author_label_degrades_for_missing_or_odd_authors() {
        // Entries minted before provenance existed must render as *something*.
        assert_eq!(author_label(None), "?");
        assert_eq!(author_label(Some("dean")), "?");
    }

    #[test]
    fn session_label_strips_boilerplate_and_caps_length() {
        assert_eq!(session_label("session_73b15e1a"), "73b15e1a");
        assert_eq!(session_label("sess_abc"), "abc");
        assert_eq!(session_label("plain"), "plain");
        // Long ids are capped, ellipsis included, so the chip can't grow the row.
        let long = session_label("session_0123456789abcdef");
        assert!(long.chars().count() <= 8, "capped: {long}");
        assert!(long.ends_with('…'), "truncated: {long}");
    }

    #[test]
    fn relative_time_buckets_by_magnitude() {
        const S: i64 = 1000;
        assert_eq!(relative_time(0, 0), "now");
        assert_eq!(relative_time(0, 999), "now");
        assert_eq!(relative_time(0, 45 * S), "45s");
        assert_eq!(relative_time(0, 59 * S), "59s");
        assert_eq!(relative_time(0, 60 * S), "1m");
        assert_eq!(relative_time(0, 12 * 60 * S), "12m");
        assert_eq!(relative_time(0, 60 * 60 * S), "1h");
        assert_eq!(relative_time(0, 3 * 60 * 60 * S), "3h");
        assert_eq!(relative_time(0, 24 * 60 * 60 * S), "1d");
        assert_eq!(relative_time(0, 2 * 24 * 60 * 60 * S), "2d");
    }

    #[test]
    fn relative_time_never_goes_negative_on_a_backwards_clock() {
        // NTP correction / suspend can put `now` behind `copied_at`.
        assert_eq!(relative_time(10_000, 0), "now");
    }

    #[test]
    fn preview_collapses_whitespace_and_truncates() {
        let long = "a ".repeat(100);
        let p = preview_text(&Content::Text(long));
        assert!(p.chars().count() <= PREVIEW_MAX, "capped: {p}");
        assert!(p.ends_with('…'), "truncated: {p}");
        assert!(!p.contains("  "), "whitespace collapsed: {p}");
    }

    #[test]
    fn preview_text_placeholders_for_non_text() {
        assert_eq!(
            preview_text(&Content::Files(vec!["/a".into(), "/b".into()])),
            "<2 file(s)>"
        );
    }

    #[test]
    fn human_masks_both_secret_and_pii() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let secret = entry("ghp_0123456789abcdefghij");
        let pii = entry("dean@example.com");
        assert_eq!(secret.sensitivity, Sensitivity::Secret);
        assert_eq!(pii.sensitivity, Sensitivity::Pii);
        assert!(should_mask(secret.sensitivity, Audience::Human));
        assert!(should_mask(pii.sensitivity, Audience::Human));
        assert_eq!(preview(&secret, Audience::Human), MASK);
        assert_eq!(preview(&pii, Audience::Human), MASK);
    }

    #[test]
    fn model_masks_secret_always_but_pii_only_under_strict() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let secret = Sensitivity::Secret;
        let pii = Sensitivity::Pii;
        let normal = Sensitivity::Normal;
        // Secret is masked in every posture, including Off.
        for mode in Mode::ALL {
            assert!(should_mask(secret, Audience::Model { mode }), "{mode}");
            assert!(!should_mask(normal, Audience::Model { mode }), "{mode}");
        }
        // PII: sent by default, withheld only under Strict.
        for mode in Mode::ALL {
            let masked = should_mask(pii, Audience::Model { mode });
            assert_eq!(masked, mode == Mode::Strict, "{mode}");
        }
    }

    #[test]
    fn model_floor_recovers_a_secret_the_daemon_did_not_mark() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // What a privacy-Off daemon sends: a real token reported as Normal.
        let mut e = entry("ghp_0123456789abcdefghij");
        e.sensitivity = Sensitivity::Normal;
        assert_eq!(model_sensitivity(&e), Sensitivity::Secret);
        assert!(should_mask(
            model_sensitivity(&e),
            Audience::Model { mode: Mode::Off }
        ));
        // Ordinary text is not promoted, and a reported Pii is left as-is.
        let plain = entry("just some plain text");
        assert_eq!(model_sensitivity(&plain), Sensitivity::Normal);
        let mut pii = entry("dean@example.com");
        pii.sensitivity = Sensitivity::Pii;
        assert_eq!(model_sensitivity(&pii), Sensitivity::Pii);
    }

    #[test]
    fn normal_is_never_masked() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let e = entry("just some plain text");
        assert_eq!(e.sensitivity, Sensitivity::Normal);
        assert!(!should_mask(e.sensitivity, Audience::Human));
        assert_eq!(preview(&e, Audience::Human), "just some plain text");
    }

    #[test]
    #[cfg(feature = "debug-unsafe")]
    fn insecure_lifts_all_masking() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COPYCAT_INSECURE", "1");
        let secret = entry("ghp_0123456789abcdefghij");
        let unmasked_human = !should_mask(secret.sensitivity, Audience::Human);
        let unmasked_model =
            !should_mask(secret.sensitivity, Audience::Model { mode: Mode::Strict });
        let shown = preview(&secret, Audience::Human);
        std::env::remove_var("COPYCAT_INSECURE");
        assert!(
            unmasked_human && unmasked_model,
            "insecure reveals for all audiences"
        );
        assert_eq!(shown, "ghp_0123456789abcdefghij");
    }
}
