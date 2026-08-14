//! Policy for how much the daemon records about its own use; `copycatd::usage`
//! owns the writing. `Secret` is masked at every level including
//! [`Level::Preview`] — a log file has none of the socket's protections.

use serde::{Deserialize, Serialize};

/// Environment variable naming the level. See [`Level::from_env`].
pub const ENV_USAGE: &str = "COPYCAT_USAGE";
/// Environment variable naming how many days of logs to keep.
pub const ENV_USAGE_DAYS: &str = "COPYCAT_USAGE_DAYS";

/// How many days of usage logs to keep before deleting the oldest.
pub const DEFAULT_RETENTION_DAYS: u16 = 30;

/// What the daemon writes about each request it handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// **Default.** Nothing is written. No file is created.
    #[default]
    Off,
    /// One content-free record per request: which operation, who asked, how
    /// large the payload was, how long it took.
    Meta,
    /// [`Level::Meta`] plus a truncated preview of the value. `Secret` stays
    /// masked; `Pii` follows the privacy mode, so under `Guarded` personal
    /// data **is** written to disk in cleartext — a deliberate choice.
    Preview,
}

impl Level {
    /// Parse a key (case-insensitive, surrounding whitespace ignored).
    /// Unrecognised input ⇒ `None`, so a caller can distinguish "use the
    /// default" from "you typo'd this".
    pub fn from_key(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" | "false" => Some(Level::Off),
            "meta" | "on" | "1" | "true" | "metadata" => Some(Level::Meta),
            "preview" | "verbose" | "2" | "full" => Some(Level::Preview),
            _ => None,
        }
    }

    /// Whether this level writes anything at all.
    pub fn records(self) -> bool {
        !matches!(self, Level::Off)
    }

    /// Whether a preview of the value may be written.
    pub fn writes_previews(self) -> bool {
        matches!(self, Level::Preview)
    }

    /// Seed from the environment; unparseable values fall back to the default
    /// so a typo in a shell profile can't stop the clipboard working.
    pub fn from_env() -> Level {
        std::env::var(ENV_USAGE)
            .ok()
            .and_then(|v| Level::from_key(&v))
            .unwrap_or_default()
    }
}

/// Retention in days, from the environment. Zero is rejected as a typo — a log
/// that deletes itself the same day is never what someone meant.
pub fn retention_days_from_env() -> u16 {
    std::env::var(ENV_USAGE_DAYS)
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Rough token count, rated by shape: prose 0.16 tokens/char, URL/email 0.26,
/// high-entropy opaque blob 0.75, everything else 0.35. Calibrated against
/// `o200k_base`; keys cost ~5x prose per char, so `chars/4` misestimates the
/// two kinds that matter most. An estimate for ranking, not a billing figure.
pub fn estimate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as f32;
    let has_spaces = text.contains(char::is_whitespace);
    let looks_like_locator = text.starts_with("http") || text.contains('@');
    let entropy = crate::pii::shannon_bits_per_char(text);

    let per_char = if !has_spaces && entropy >= 4.0 {
        0.75
    } else if looks_like_locator && !has_spaces {
        0.26
    } else if has_spaces {
        0.16
    } else {
        0.35
    };
    (chars * per_char).ceil() as u32
}

/// The point at which referring to a value costs less than emitting it:
/// measured, a promote-then-paste pair costs ~25 emitted payload tokens.
pub const BREAK_EVEN_TOKENS: u32 = 25;

/// Did referring to this value pay for itself?
pub fn above_break_even(text: &str) -> bool {
    estimate_tokens(text) >= BREAK_EVEN_TOKENS
}

/// Usage-log directory: XDG state dir (not cache) on Linux; `COPYCAT_USAGE_DIR` overrides.
pub fn usage_dir() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("COPYCAT_USAGE_DIR") {
        return std::path::PathBuf::from(explicit);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    let base = std::path::PathBuf::from(home);
    if cfg!(target_os = "macos") {
        base.join("Library/Logs/copycat/usage")
    } else if cfg!(target_os = "windows") {
        base.join("AppData/Local/copycat/usage")
    } else {
        std::env::var("XDG_STATE_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| base.join(".local/state"))
            .join("copycat/usage")
    }
}

/// Unix millis → RFC3339 UTC (`2026-07-30T10:02:11Z`) via Hinnant
/// civil-from-days; the log's filename and retention key off `ts[..10]`.
pub fn rfc3339_utc(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, sec) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // days -> civil (y, m, d): era starts 0000-03-01 so leap days land at the
    // end of the cycle and the arithmetic stays branch-free.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{sec:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // A leap day, and the end of a century that is not a leap year.
        assert_eq!(rfc3339_utc(1_709_164_800_000), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_785_315_779_365), "2026-07-29T09:02:59Z");
    }

    #[test]
    fn the_date_prefix_is_what_rotation_keys_off() {
        // The writer derives today's filename from ts[..10]; if that slice were
        // ever not a date, every record would land in `usage-unknown.jsonl`.
        let ts = rfc3339_utc(1_785_315_779_365);
        assert_eq!(&ts[..10], "2026-07-29");
        assert_eq!(ts.len(), 20);
    }

    #[test]
    fn pre_epoch_does_not_panic_or_go_negative() {
        // A clock skewed before 1970 must still produce a sortable string
        // rather than an underflow.
        let s = rfc3339_utc(-1);
        assert!(s.starts_with("1969-12-31T23:59:59"), "{s}");
    }

    #[test]
    fn keys_round_trip() {
        assert_eq!(Level::from_key("off"), Some(Level::Off));
        assert_eq!(Level::from_key("meta"), Some(Level::Meta));
        assert_eq!(Level::from_key("preview"), Some(Level::Preview));
    }

    #[test]
    fn the_default_records_nothing() {
        // Opt-in, not opt-out: a clipboard manager must not start writing a
        // durable record of your activity because you upgraded.
        assert_eq!(Level::default(), Level::Off);
        assert!(!Level::default().records());
    }

    #[test]
    fn only_preview_writes_values() {
        assert!(!Level::Off.writes_previews());
        assert!(
            !Level::Meta.writes_previews(),
            "meta must stay content-free"
        );
        assert!(Level::Preview.writes_previews());
    }

    #[test]
    fn a_typo_falls_back_rather_than_failing() {
        assert_eq!(Level::from_key("nonsense"), None);
        assert_eq!(Level::from_key(""), None);
        // Aliases people actually reach for.
        assert_eq!(Level::from_key("VERBOSE"), Some(Level::Preview));
        assert_eq!(Level::from_key(" on "), Some(Level::Meta));
    }

    #[test]
    fn opaque_blobs_cost_far_more_per_char_than_prose() {
        // The whole reason for not using chars/4: same length, ~5x the tokens.
        let key = "sk-2yW4Acq9GFz6Y1t9EwL56nGisiWgNZq6";
        let prose = "the quick brown fox jumps over it..";
        assert_eq!(key.chars().count(), prose.chars().count());
        assert!(
            estimate_tokens(key) > estimate_tokens(prose) * 3,
            "key={} prose={}",
            estimate_tokens(key),
            estimate_tokens(prose)
        );
    }

    #[test]
    fn break_even_splits_the_cases_the_eval_measured() {
        // Long opaque token: by-reference clearly pays.
        assert!(above_break_even(
            "2yW4Acq9GFz6Y1t9EwL56nGisiWgNZq6ITZM5jtgUe52RvEJgwBuNO6n9JEC3Hqd"
        ));
        // Short form-field values: it does not. These are the MiniWoB shapes
        // that cost MORE by-reference than typing them.
        for short in ["Ken", "49755", "Ashland", "6866"] {
            assert!(
                !above_break_even(short),
                "{short} should be below break-even"
            );
        }
    }

    #[test]
    fn empty_text_is_free_not_a_panic() {
        assert_eq!(estimate_tokens(""), 0);
        assert!(!above_break_even(""));
    }

    #[test]
    fn retention_rejects_zero_as_a_typo() {
        // Parsed here rather than via env so the test is not order-dependent.
        let parse = |s: &str| s.trim().parse::<u16>().ok().filter(|d| *d > 0);
        assert_eq!(
            parse("0"),
            None,
            "0 days would delete today's log immediately"
        );
        assert_eq!(parse("7"), Some(7));
        assert_eq!(parse("garbage"), None);
    }
}
