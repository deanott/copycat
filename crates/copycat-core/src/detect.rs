//! Content-type detection: tag an entry as url/email/OTP/json/... so surfaces
//! can offer type-specific actions. Cheap deterministic heuristics only,
//! dependency-light so it can run on the hot path.

use crate::entry::{Content, ContentKind};

/// Best-effort, allocation-light classification of a clipboard payload.
pub fn detect_kind(content: &Content) -> ContentKind {
    match content {
        Content::Image { .. } => ContentKind::Image,
        Content::Files(_) => ContentKind::Files,
        Content::Text(t) => detect_text_kind(t),
    }
}

/// Order matters: most specific / cheapest-to-rule-out first; ambiguous cases
/// fall through to `PlainText`.
fn detect_text_kind(text: &str) -> ContentKind {
    let t = text.trim();
    if t.is_empty() {
        return ContentKind::PlainText;
    }

    if is_url(t) {
        return ContentKind::Url;
    }
    if is_email(t) {
        return ContentKind::Email;
    }
    if is_json(t) {
        return ContentKind::Json;
    }
    if is_hex_color(t) {
        return ContentKind::Color;
    }
    if is_one_time_code(t) {
        return ContentKind::OneTimeCode;
    }
    if is_phone(t) {
        return ContentKind::PhoneNumber;
    }
    ContentKind::PlainText
}

fn is_url(t: &str) -> bool {
    // Require a non-empty, whitespace-free remainder after the scheme — measured
    // against the actual prefix, so a minimal `http://x` isn't rejected by the
    // longer `https://` length.
    match t
        .strip_prefix("http://")
        .or_else(|| t.strip_prefix("https://"))
    {
        Some(rest) => !rest.is_empty() && !rest.contains(char::is_whitespace),
        None => false,
    }
}

/// Deliberately conservative single-token email check (not RFC-complete).
fn is_email(t: &str) -> bool {
    if t.contains(char::is_whitespace) {
        return false;
    }
    match t.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    }
}

fn is_json(t: &str) -> bool {
    // Must look like an object/array AND actually parse. Note the parens: without
    // them `&&` binds tighter than `||`, so a `{…}` string would be accepted
    // without ever being validated.
    let shaped =
        (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']'));
    shaped && serde_json::from_str::<serde_json::Value>(t).is_ok()
}

fn is_hex_color(t: &str) -> bool {
    let Some(hex) = t.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 3 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Short all-digit strings of the length sites use for OTP/2FA codes.
fn is_one_time_code(t: &str) -> bool {
    matches!(t.len(), 4..=8) && t.bytes().all(|b| b.is_ascii_digit())
}

/// Loose phone heuristic: mostly digits with phone punctuation, 7..=15 digits.
fn is_phone(t: &str) -> bool {
    let digits = t.bytes().filter(|b| b.is_ascii_digit()).count();
    let allowed = t
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b' ' | b'(' | b')' | b'.'));
    allowed && (7..=15).contains(&digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Content {
        Content::Text(s.to_string())
    }

    #[test]
    fn detects_url() {
        assert_eq!(
            detect_kind(&text("https://example.com/x")),
            ContentKind::Url
        );
        assert_eq!(detect_kind(&text("http://a.b")), ContentKind::Url);
        // minimal http:// host (exactly "https://".len()) is still a URL
        assert_eq!(detect_kind(&text("http://x")), ContentKind::Url);
        // not a URL: has whitespace, or is a bare scheme with no host
        assert_eq!(detect_kind(&text("https://a b")), ContentKind::PlainText);
        assert_eq!(detect_kind(&text("https://")), ContentKind::PlainText);
    }

    #[test]
    fn detects_email() {
        assert_eq!(detect_kind(&text("dean@example.com")), ContentKind::Email);
        assert_eq!(detect_kind(&text("no-at-sign.com")), ContentKind::PlainText);
        assert_eq!(detect_kind(&text("a@b")), ContentKind::PlainText); // domain has no dot
    }

    #[test]
    fn detects_json() {
        assert_eq!(detect_kind(&text(r#"{"a":1}"#)), ContentKind::Json);
        assert_eq!(detect_kind(&text("[1,2,3]")), ContentKind::Json);
    }

    #[test]
    fn detects_color() {
        assert_eq!(detect_kind(&text("#ff8800")), ContentKind::Color);
        assert_eq!(detect_kind(&text("#abc")), ContentKind::Color);
        assert_eq!(detect_kind(&text("#zz")), ContentKind::PlainText);
    }

    #[test]
    fn detects_one_time_code() {
        assert_eq!(detect_kind(&text("123456")), ContentKind::OneTimeCode);
        assert_eq!(detect_kind(&text("12")), ContentKind::PlainText); // too short
    }

    #[test]
    fn detects_phone() {
        assert_eq!(
            detect_kind(&text("+1 (555) 010-1234")),
            ContentKind::PhoneNumber
        );
    }

    #[test]
    fn plain_text_fallback() {
        assert_eq!(
            detect_kind(&text("just some words here")),
            ContentKind::PlainText
        );
        assert_eq!(detect_kind(&text("   ")), ContentKind::PlainText);
    }

    #[test]
    fn images_and_files() {
        let img = Content::Image {
            mime: "image/png".into(),
            bytes_ref: crate::entry::BytesRef::Inline(vec![1, 2, 3]),
        };
        assert_eq!(detect_kind(&img), ContentKind::Image);
        assert_eq!(
            detect_kind(&Content::Files(vec!["/tmp/x".into()])),
            ContentKind::Files
        );
    }
}
