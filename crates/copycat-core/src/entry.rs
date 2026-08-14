//! The fundamental unit of the temporal clipboard.

use serde::{Deserialize, Serialize};

/// A single clipboard entry. The clipboard is an ordered history of these;
/// the "current" clipboard is the one with the most recent `copied_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Stable id (e.g. ULID/UUID) — assigned by the store.
    pub id: String,
    /// What was copied.
    pub content: Content,
    /// Best-guess semantic type, for type-aware actions.
    pub kind: ContentKind,
    /// Unix-millis timestamp the item entered the clipboard. The temporal axis.
    pub copied_at: i64,
    /// Where it came from (which device/app/session).
    pub source: Source,
    /// Set by the PII classifier; drives masking and what may reach the model.
    /// See `crate::pii`.
    pub sensitivity: Sensitivity,
    /// Pinned entries are exempt from cache eviction / expiry.
    pub pinned: bool,
}

impl Entry {
    /// Build a text entry, running content-type detection and PII/secret
    /// classification. The caller supplies `id`, `copied_at` and `source` so this
    /// stays free of clock/random/OS dependencies (and thus unit-testable). New
    /// entries are unpinned.
    pub fn from_text(
        id: impl Into<String>,
        text: impl Into<String>,
        copied_at: i64,
        source: Source,
    ) -> Self {
        let content = Content::Text(text.into());
        let kind = crate::detect::detect_kind(&content);
        let sensitivity = match &content {
            Content::Text(t) => crate::pii::classify(t),
            _ => Sensitivity::Normal,
        };
        Entry {
            id: id.into(),
            content,
            kind,
            copied_at,
            source,
            sensitivity,
            pinned: false,
        }
    }

    /// The text payload, if this entry is text (helper for front-ends).
    pub fn as_text(&self) -> Option<&str> {
        match &self.content {
            Content::Text(t) => Some(t.as_str()),
            _ => None,
        }
    }

    /// The cheap model-facing address for this entry. See [`handle_of`].
    pub fn handle(&self) -> Option<String> {
        handle_of(&self.id)
    }
}

/// Keeps a handle from parsing as a bare integer, which
/// [`crate::proto::Selector`] would resolve as a recency index — a different
/// entry, silently.
pub const HANDLE_PREFIX: char = 'c';

/// A short, stable address for an entry: `c0`, `c1`, … `cz`, `c10`, …
///
/// The canonical id `{millis:013}-{n:06}` tokenizes into ~8 tokens (digit runs
/// shatter), so the handle is the base-36 sequence number instead — derived,
/// never stored, so it cannot drift from the id. Non-canonical ids get `None`
/// (callers fall back to the full id); the shape check is strict because a
/// looser one would map both `a-1` and `b-1` to `c1` and mis-select.
pub fn handle_of(id: &str) -> Option<String> {
    let (millis, seq) = id.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    // `{millis:013}` pads to *at least* 13, so accept longer for far-future clocks.
    if millis.len() < 13 || !digits(millis) || seq.len() != 6 || !digits(seq) {
        return None;
    }
    let seq: u64 = seq.parse().ok()?;
    let mut out = String::new();
    let mut n = seq;
    loop {
        let d = (n % 36) as u32;
        out.insert(
            0,
            char::from_digit(d, 36).expect("d < 36 is always a base-36 digit"),
        );
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.insert(0, HANDLE_PREFIX);
    Some(out)
}

/// Does `selector` name the entry with id `entry_id`? Accepts the full id or
/// its [`handle_of`] form, so a handle works anywhere an id does.
pub fn id_matches(entry_id: &str, selector: &str) -> bool {
    entry_id == selector || handle_of(entry_id).is_some_and(|h| h == selector)
}

/// Zeroize Secret/PII payloads on drop so they don't linger in freed heap.
/// Because `Entry: Drop`, fields cannot be moved out by value — clone instead.
impl Drop for Entry {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if matches!(self.sensitivity, Sensitivity::Secret | Sensitivity::Pii) {
            match &mut self.content {
                Content::Text(t) => t.zeroize(),
                Content::Image {
                    bytes_ref: BytesRef::Inline(b),
                    ..
                } => b.zeroize(),
                _ => {}
            }
        }
    }
}

/// The actual payload. Images are first-class, not just text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    /// Raw image bytes + mime (e.g. "image/png").
    Image {
        mime: String,
        bytes_ref: BytesRef,
    },
    /// Files copied as paths/URIs.
    Files(Vec<String>),
}

/// Indirection so large blobs (images) aren't carried around inline everywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BytesRef {
    Inline(Vec<u8>),
    /// Key into the blob store on disk.
    Stored(String),
}

/// Semantic content type — populated by `crate::detect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentKind {
    PlainText,
    Url,
    Code,
    Json,
    Email,
    PhoneNumber,
    PostalAddress,
    /// Short codes/OTPs/tokens meant to be typed into a site.
    OneTimeCode,
    Color,
    ErrorMessage,
    Image,
    Files,
    Unknown,
}

/// Origin of an entry — copycat spans devices, sessions, and agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// e.g. "linux-desktop", "ssh:host", "phone:pixel", "claude.ai", "claude-code".
    pub device: String,
    /// Optional app/window that produced it, when known.
    pub app: Option<String>,
    /// Optional author provenance (e.g. "human:user", "agent:claude-3-5", "agent:mcp").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Optional session identifier (e.g. "session_73b15e1a").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Canonical `Source::author` prefixes. The prefix — not the suffix —
/// classifies, so a surface may invent its own suffix. See [`Origin::of`].
pub const AUTHOR_HUMAN_PREFIX: &str = "human:";
/// See [`AUTHOR_HUMAN_PREFIX`].
pub const AUTHOR_AGENT_PREFIX: &str = "agent:";

/// The daemon's watcher seeing an unrequested OS clipboard change — a person
/// pressing Ctrl/Cmd+C in some app.
pub const AUTHOR_OS_CLIPBOARD: &str = "human:os";
/// `copycat copy` with a terminal attached (see [`cli_author`]).
pub const AUTHOR_CLI: &str = "human:cli";
/// `copycat copy` with no terminal, or from an agent that didn't name itself.
pub const AUTHOR_AGENT_CLI: &str = "agent:cli";
/// The tray/history window's "Re-copy" action — a person clicking.
pub const AUTHOR_UI: &str = "human:ui";
/// An MCP tool call, unless the model passes a more specific `author`.
pub const AUTHOR_MCP: &str = "agent:mcp";

/// Who an unflagged `copycat copy` belongs to. A person is claimed only with a
/// tty and no agent announcing itself; otherwise agent — the safe direction to
/// be wrong in.
pub fn cli_author(has_terminal: bool, agent: Option<&str>) -> String {
    match agent.map(str::trim).filter(|a| !a.is_empty()) {
        Some(name) => format!("{AUTHOR_AGENT_PREFIX}{name}"),
        None if has_terminal => AUTHOR_CLI.to_string(),
        None => AUTHOR_AGENT_CLI.to_string(),
    }
}

/// Who put an entry on the clipboard: a person, or an agent acting on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A person. `Source::author` starts with [`AUTHOR_HUMAN_PREFIX`].
    Human,
    /// An agent/model writing through a tool. Starts with [`AUTHOR_AGENT_PREFIX`].
    Agent,
    /// No author recorded, or one in an unrecognised shape.
    Unknown,
}

impl Origin {
    /// Deliberately conservative: neither prefix ⇒ [`Origin::Unknown`], never a
    /// guess — a wrong attribution is worse than an absent one.
    pub fn of(author: Option<&str>) -> Origin {
        match author {
            Some(a) if a.starts_with(AUTHOR_HUMAN_PREFIX) => Origin::Human,
            Some(a) if a.starts_with(AUTHOR_AGENT_PREFIX) => Origin::Agent,
            _ => Origin::Unknown,
        }
    }
}

impl Source {
    pub fn simple(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            app: None,
            author: None,
            session_id: None,
        }
    }

    pub fn with_provenance(
        device: impl Into<String>,
        app: Option<String>,
        author: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            device: device.into(),
            app,
            author,
            session_id,
        }
    }
}

/// How sensitive an entry is. Drives masking and model exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sensitivity {
    /// Safe to keep in plaintext and share with the LLM.
    Normal,
    /// Likely PII (address, email, phone) — masked in listings, zeroized on
    /// drop, redacted to the LLM unless the user explicitly references it.
    Pii,
    /// Secrets (passwords, API keys, private keys) — masked, zeroized on drop,
    /// and must NEVER be sent to the LLM. See `crate::pii` policy.
    Secret,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> Source {
        Source::simple("test")
    }

    #[test]
    fn from_text_detects_kind_and_sensitivity() {
        let e = Entry::from_text("1", "https://example.com/x", 10, src());
        assert_eq!(e.kind, ContentKind::Url);
        assert_eq!(e.sensitivity, Sensitivity::Normal);
        assert_eq!(e.copied_at, 10);
        assert!(!e.pinned);
        assert_eq!(e.as_text(), Some("https://example.com/x"));
    }

    #[test]
    fn from_text_flags_secrets() {
        let e = Entry::from_text("2", "ghp_0123456789abcdefghij", 0, src());
        assert_eq!(e.sensitivity, Sensitivity::Secret);
    }

    #[test]
    fn from_text_flags_pii_email() {
        let e = Entry::from_text("3", "dean@example.com", 0, src());
        assert_eq!(e.sensitivity, Sensitivity::Pii);
        assert_eq!(e.kind, ContentKind::Email);
    }

    #[test]
    fn as_text_none_for_non_text() {
        let e = Entry {
            id: "4".into(),
            content: Content::Files(vec!["/tmp/x".into()]),
            kind: ContentKind::Files,
            copied_at: 0,
            source: src(),
            sensitivity: Sensitivity::Normal,
            pinned: false,
        };
        assert_eq!(e.as_text(), None);
    }

    #[test]
    fn origin_classifies_by_prefix() {
        assert_eq!(Origin::of(Some("human:os")), Origin::Human);
        assert_eq!(Origin::of(Some("human:cli")), Origin::Human);
        assert_eq!(Origin::of(Some("agent:mcp")), Origin::Agent);
        assert_eq!(Origin::of(Some("agent:claude-3-5")), Origin::Agent);
    }

    #[test]
    fn origin_is_unknown_without_a_recognised_prefix() {
        // Older entries carry no author at all.
        assert_eq!(Origin::of(None), Origin::Unknown);
        // A free-form author is not guessed into either side.
        assert_eq!(Origin::of(Some("dean")), Origin::Unknown);
        assert_eq!(Origin::of(Some("")), Origin::Unknown);
        // Near-misses must not classify: the prefix includes the colon.
        assert_eq!(Origin::of(Some("humanoid")), Origin::Unknown);
        assert_eq!(Origin::of(Some("agentic")), Origin::Unknown);
    }

    #[test]
    fn canonical_authors_use_the_canonical_prefixes() {
        for a in [AUTHOR_OS_CLIPBOARD, AUTHOR_CLI, AUTHOR_UI] {
            assert_eq!(Origin::of(Some(a)), Origin::Human, "{a} must read as human");
        }
        for a in [AUTHOR_MCP, AUTHOR_AGENT_CLI] {
            assert_eq!(Origin::of(Some(a)), Origin::Agent, "{a} must read as agent");
        }
    }

    #[test]
    fn cli_author_claims_a_person_only_with_a_terminal() {
        assert_eq!(cli_author(true, None), AUTHOR_CLI);
        assert_eq!(Origin::of(Some(&cli_author(true, None))), Origin::Human);
    }

    #[test]
    fn cli_author_never_claims_a_person_without_one() {
        // A piped/redirected caller — a script, a CI job, an agent shelling out.
        assert_eq!(cli_author(false, None), AUTHOR_AGENT_CLI);
        assert_eq!(Origin::of(Some(&cli_author(false, None))), Origin::Agent);
    }

    #[test]
    fn an_announced_agent_outranks_the_terminal() {
        // A terminal is not proof of a person: agents run inside PTYs too, so a
        // runtime naming itself wins over the weaker tty signal.
        assert_eq!(cli_author(true, Some("claude")), "agent:claude");
        assert_eq!(
            Origin::of(Some(&cli_author(true, Some("claude")))),
            Origin::Agent
        );
    }

    #[test]
    fn a_blank_agent_name_falls_back_rather_than_minting_a_bare_prefix() {
        // An empty/whitespace env var must not produce "agent:", which would
        // render as the wordless "agent" and lose the tty signal entirely.
        for blank in ["", "   "] {
            assert_eq!(cli_author(false, Some(blank)), AUTHOR_AGENT_CLI);
            assert_eq!(cli_author(true, Some(blank)), AUTHOR_CLI);
        }
    }

    #[test]
    fn handle_is_base36_of_the_sequence_number() {
        assert_eq!(handle_of("1785315779365-000000").unwrap(), "c0");
        assert_eq!(handle_of("1785315779365-000009").unwrap(), "c9");
        assert_eq!(handle_of("1785315779365-000010").unwrap(), "ca");
        assert_eq!(handle_of("1785315779365-000035").unwrap(), "cz");
        assert_eq!(handle_of("1785315779365-000036").unwrap(), "c10");
        assert_eq!(handle_of("1785315779365-000022").unwrap(), "cm");
    }

    #[test]
    fn a_handle_stays_short_across_the_whole_ring() {
        // The store's capacity is a few hundred, so every handle a live daemon
        // can mint fits in three characters — the property the cost argument
        // rests on. Checked well past capacity for headroom.
        for seq in 0..5_000u64 {
            let h = handle_of(&format!("1785315779365-{seq:06}")).unwrap();
            assert!(h.len() <= 4, "handle {h} for seq {seq} is too long");
        }
    }

    #[test]
    fn a_handle_never_reads_as_a_recency_index() {
        // The whole point of the prefix: `Selector` resolves a bare integer as an
        // index, so a handle that parsed as one would silently select a different
        // entry. No handle may ever parse as a number.
        for seq in [0u64, 7, 10, 35, 36, 1_000] {
            let h = handle_of(&format!("1785315779365-{seq:06}")).unwrap();
            assert!(
                h.parse::<usize>().is_err(),
                "handle {h} must not parse as an index"
            );
            assert!(h.starts_with(HANDLE_PREFIX));
        }
    }

    #[test]
    fn handles_are_unique_because_the_sequence_is() {
        use std::collections::HashSet;
        let seen: HashSet<_> = (0..1_000u64)
            .map(|seq| handle_of(&format!("1785315779365-{seq:06}")).unwrap())
            .collect();
        assert_eq!(seen.len(), 1_000, "two sequence numbers shared a handle");
    }

    #[test]
    fn a_non_canonical_id_has_no_handle() {
        // Hand-written ids (tests, older entries) must fall back to the full id
        // rather than get an address that resolves to nothing.
        for id in [
            "a",
            "",
            "no-digits",
            "1785315779365-",
            // `id-1` and `a-1` would both reduce to `c1` — must not get handles.
            "id-1",
            "a-1",
            "id-000001",
            // Right shape, wrong widths.
            "178531577936-000022",
            "1785315779365-00022",
            "1785315779365-0000221",
        ] {
            assert_eq!(handle_of(id), None, "{id} should have no handle");
        }
    }

    #[test]
    fn id_matches_accepts_either_form() {
        let id = "1785315779365-000022";
        assert!(id_matches(id, id), "the full id must still select");
        assert!(id_matches(id, "cm"), "the handle must select too");
        assert!(!id_matches(id, "cn"));
        assert!(!id_matches(id, "1785315779365-000023"));
        // An id with no handle is still selectable by its full form.
        assert!(id_matches("a", "a"));
        assert!(!id_matches("a", "c0"));
    }

    #[test]
    fn entry_json_roundtrips() {
        let e = Entry::from_text("5", "hello", 42, src());
        let json = serde_json::to_string(&e).unwrap();
        let back: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "5");
        assert_eq!(back.as_text(), Some("hello"));
        assert_eq!(back.copied_at, 42);
    }
}
