//! PII / secret classification: the deterministic hot-path classifier — cheap,
//! dependency-light pattern checks run on every entry so it can be masked on
//! every surface and scrubbed from RAM on drop. It deliberately errs toward
//! *over*-classifying: better to mask something harmless than leak a secret.
//!
//! Classification only — the user's posture (what is acted on) lives in
//! [`crate::privacy`], whose tiers map onto [`is_secret`] and [`is_personal`].

use crate::entry::Sensitivity;

/// Classify a text payload's sensitivity at **full fidelity** — every detector
/// this module has, regardless of the active privacy mode. Most sensitive wins.
///
/// This is what the daemon stores on an [`Entry`](crate::entry::Entry), because
/// the true classification is what drives zeroize-on-drop and what lets a later
/// privacy-mode change re-scope entries that are already in history. What a
/// given mode *does* with it is [`crate::privacy::project`].
pub fn classify(text: &str) -> Sensitivity {
    let t = text.trim();
    if is_secret(t) {
        Sensitivity::Secret
    } else if is_personal(t) {
        Sensitivity::Pii
    } else {
        Sensitivity::Normal
    }
}

/// Secrets: credentials/keys that must NEVER reach the LLM in any mode (see
/// [`crate::privacy::reveal_to_model`]). The
/// [`Guarded`](crate::privacy::Mode::Guarded) tier is exactly this predicate.
pub fn is_secret(text: &str) -> bool {
    looks_like_secret(text.trim())
}

/// Personal data: the extra ground the [`Strict`](crate::privacy::Mode::Strict)
/// tier covers — emails, phone numbers, card-shaped numbers, street addresses.
/// Deliberately *not* consulted by the default mode.
pub fn is_personal(text: &str) -> bool {
    looks_like_pii(text.trim())
}

fn looks_like_secret(t: &str) -> bool {
    // PEM private keys.
    if t.contains("PRIVATE KEY-----") {
        return true;
    }
    // Inline URL credentials (DSNs / basic-auth URLs). Checked on the raw text
    // FIRST because the `:`-splitting token scan below shreds the authority
    // (`scheme://user:pass@host` splits into `scheme`, `//user`, `pass@host`, …),
    // so the password would never be seen as one token.
    if has_inline_url_credential(t) {
        return true;
    }
    // `key=value` / `key: value` where a secret-ish key names a long value — e.g.
    // `aws_secret_access_key=…` or a `.env` line whose value has no known prefix.
    if has_labeled_secret_value(t) {
        return true;
    }
    // A lone opaque high-entropy blob — a key with no recognisable prefix.
    if is_opaque_credential(t) {
        return true;
    }
    // Scan every token AND the value side of `key=value` / `key: value`, so the
    // shapes people actually copy are caught, not just a bare token pasted alone:
    // `export GITHUB_TOKEN=ghp_…`, `Authorization: Bearer sk-…`, `password: sk-…`.
    t.split(|c: char| c.is_whitespace() || c == '=' || c == ':')
        .any(is_secret_token)
}

/// The entropy heuristic: **the whole payload** is one opaque, random-looking
/// token in a key/base64 alphabet. Catches the credential shape no prefix list
/// ever will — a raw 32-byte key, a session cookie, a generated password.
///
/// This is the one detector here that is a *guess*, so it is fenced in hard:
///
/// - **Whole payload only.** A blob embedded in prose, JSON or code is far more
///   often a hash, id or piece of data than a credential; a blob copied *on its
///   own* is usually a credential someone is about to paste into a config.
/// - **Mixed case + digits required.** This is what excludes the two big
///   false-positive families outright: hex digests (git SHAs, md5/sha256 — no
///   uppercase) and UUIDs (no uppercase either).
/// - **24..=200 chars.** Shorter is not meaningfully high-entropy; longer is a
///   data blob (an inlined image, a PEM body, a base64'd file), and masking the
///   thing a user deliberately copied to paste elsewhere is the failure mode
///   that makes a feature like this hated.
///
/// It still has honest false positives, and they are unavoidable: **any**
/// base64 of ordinary bytes is indistinguishable from a key by shape alone, so
/// a copied `SnVzdCBhIG5vcm1hbCBzZW50ZW5jZQ==` reads as Secret. That costs a
/// masked preview and nothing else — the value is stored intact and re-copy
/// still works. Users who find it obtrusive have a dial:
/// [`Off`](crate::privacy::Mode::Off).
fn is_opaque_credential(t: &str) -> bool {
    const MIN_LEN: usize = 24;
    const MAX_LEN: usize = 200;
    // Bits per character below which a string is too structured/repetitive to be
    // a key. Random base64 lands near 5.5, hex near 4; english text near 4 too,
    // but text can't reach here (it has spaces, or fails the class check).
    const MIN_ENTROPY: f64 = 3.5;

    let len = t.chars().count();
    if !(MIN_LEN..=MAX_LEN).contains(&len) {
        return false;
    }
    // The base64/base64url/hex alphabet plus the separators real tokens use.
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_' | '.'))
    {
        return false;
    }
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = t.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    if !(has_lower && has_upper && has_digit) {
        return false;
    }
    shannon_bits_per_char(t) >= MIN_ENTROPY
}

/// Shannon entropy of `s` in bits per character, over its own character
/// distribution. Cheap (one 128-slot histogram) and allocation-free for the
/// ASCII inputs this is called with.
pub(crate) fn shannon_bits_per_char(s: &str) -> f64 {
    let mut counts = [0u32; 128];
    let mut total = 0u32;
    for b in s.bytes() {
        counts[(b & 0x7f) as usize] += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    let total = f64::from(total);
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / total;
            -p * p.log2()
        })
        .sum()
}

/// True when any whitespace-delimited token looks like `scheme://user:password@host`
/// — a connection string (DSN) or HTTP basic-auth URL with inline credentials.
///
/// The `:` check is restricted to the **authority** (the part after `://` and
/// before the first `/`, `?` or `#`): a colon in the path or query — e.g.
/// `https://host.com/a:b` — is NOT a credential and must stay Normal. Requires
/// a non-empty userinfo that actually contains a `:` (i.e. `user:password`, not a
/// bare `user@host`).
fn has_inline_url_credential(t: &str) -> bool {
    t.split_whitespace().any(|tok| {
        let Some((_scheme, rest)) = tok.split_once("://") else {
            return false;
        };
        // Authority ends at the first path/query/fragment delimiter.
        let authority = match rest.find(['/', '?', '#']) {
            Some(end) => &rest[..end],
            None => rest,
        };
        // Inline credentials: `userinfo@host`, with a non-empty `user:password`.
        match authority.split_once('@') {
            Some((userinfo, _host)) => !userinfo.is_empty() && userinfo.contains(':'),
            None => false,
        }
    })
}

/// True when a `key=value` / `key: value` line pairs a credential-ish key name
/// with a value long enough (>= 20 chars) to plausibly be a secret. Complements
/// the token scan, which only fires on values carrying a known provider prefix —
/// this catches high-entropy values with no recognisable prefix, e.g.
/// `aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEX…` and `.env` lines.
///
/// The key is required to be a single token (no internal whitespace) so ordinary
/// prose that merely contains a colon (`Note: some long sentence here …`) does
/// not trip the check.
fn has_labeled_secret_value(t: &str) -> bool {
    const SECRET_KEY_NAMES: [&str; 7] = [
        "aws_secret",
        "secret_access_key",
        "secret",
        "password",
        "api_key",
        "apikey",
        "token",
    ];
    t.lines().any(|line| {
        // Split on the first '=' or ':' — the key/value boundary.
        let Some((key, value)) = line.split_once(['=', ':']) else {
            return false;
        };
        let key = key.trim();
        let value = value.trim();
        if key.contains(char::is_whitespace) || value.len() < 20 {
            return false;
        }
        let key_lower = key.to_ascii_lowercase();
        SECRET_KEY_NAMES.iter().any(|n| key_lower.contains(n))
    })
}

/// Whether a single already-split token is a credential/key.
fn is_secret_token(tok: &str) -> bool {
    // Common provider token prefixes. Kept current with today's formats: AWS
    // access key; GitHub classic/OAuth/server/user/refresh + fine-grained PAT;
    // GitLab PAT; Slack bot/user/config/app; OpenAI + Stripe secret/restricted
    // (live/test); Google API key + OAuth access token. The `_`/`-` inside these
    // prefixes is fine — the tokenizer does NOT split on `_`/`/`, so the whole
    // token stays intact for the `starts_with` check.
    const SECRET_PREFIXES: [&str; 19] = [
        "AKIA",
        "ghp_",
        "gho_",
        "ghs_",
        "ghu_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxe-",
        "xapp-",
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "rk_test_",
        "AIza",
        "ya29.",
    ];
    if SECRET_PREFIXES.iter().any(|p| tok.starts_with(p)) && tok.len() >= 12 {
        return true;
    }
    // JWT: three dot-separated base64url segments, header begins with "eyJ".
    tok.starts_with("eyJ") && tok.matches('.').count() == 2
}

/// PII: personal data — the ground only the [`Strict`](crate::privacy::Mode::Strict)
/// tier covers; under the default [`Guarded`](crate::privacy::Mode::Guarded)
/// posture the daemon projects this away entirely.
///
/// Every detector below is deliberately narrow. These fire on data that is
/// usually the user's *own*, copied in order to use, and masked in their own
/// history window — a false positive is felt immediately and repeatedly.
/// Under-detecting personal data is the cheaper error; that is the opposite of
/// the [`is_secret`] bias.
fn looks_like_pii(t: &str) -> bool {
    if is_single_token_email(t) {
        return true;
    }
    if is_credit_card(t) {
        return true;
    }
    if has_phone_number(t) {
        return true;
    }
    if has_street_address(t) {
        return true;
    }
    false
}

/// Phone-shaped: either an explicitly international `+…` number (8–15 digits, as
/// per E.164), or a *separated* 10/11-digit national number written in three or
/// more groups ending in four digits — `(555) 123-4567`, `555.123.4567`,
/// `020 7946 0958`.
///
/// The trailing-group-of-four rule is what keeps the everyday collisions out:
/// `2026-07-26` (a date), `192.168.1.100` (an IP) and `1.2.3.4` (a version) all
/// have digit runs and separators but no four-digit tail. A *bare* run of ten
/// digits is likewise ignored — far more likely an order number or id than a
/// phone number. What still slips through is three separated numbers that happen
/// to end in a four-digit group (`100 200 3000`); rare, and strict-tier only.
fn has_phone_number(t: &str) -> bool {
    // Scan maximal runs of "phone characters" rather than whitespace tokens: a
    // real number is written *with* spaces inside it.
    t.split(|c: char| !matches!(c, '0'..='9' | '+' | '-' | '.' | '(' | ')' | ' '))
        .any(is_phone_run)
}

fn is_phone_run(run: &str) -> bool {
    let run = run.trim();
    let digits = run.chars().filter(char::is_ascii_digit).count();
    if run.starts_with('+') {
        return (8..=15).contains(&digits);
    }
    if !(10..=11).contains(&digits) {
        return false;
    }
    // Split into digit groups on the separators; require the classic tail.
    let groups: Vec<usize> = run
        .split(|c: char| !c.is_ascii_digit())
        .filter(|g| !g.is_empty())
        .map(str::len)
        .collect();
    // Three or more groups ending in four digits: `555 123 4567` (NANP 3-3-4),
    // `020 7946 0958` (UK 3-4-4), `(02) 9876 5432` (AU 2-4-4).
    groups.len() >= 3 && groups[groups.len() - 1] == 4
}

/// Street-address-shaped: a line that starts with a house number and names a
/// street type within the next few words — `10 Downing Street`,
/// `1600 Amphitheatre Parkway`, `221B Baker St`.
///
/// Addresses have no checksum and no fixed grammar, so this is the loosest
/// detector here and is fenced accordingly: the house number must be bare digits
/// (with at most a unit letter, so a numbered list item `1.` cannot match), and
/// the street word must be at least two words along — which is what stops
/// ordinary sentences like `3 Way merge` or `2 Lane closure` from matching. It
/// only recognises English-language street types, and it will miss most real
/// addresses (no city/postcode/PO-box logic). That is the intended bias.
fn has_street_address(t: &str) -> bool {
    const STREET_WORDS: [&str; 24] = [
        "street",
        "st",
        "avenue",
        "ave",
        "road",
        "rd",
        "boulevard",
        "blvd",
        "lane",
        "ln",
        "drive",
        "dr",
        "court",
        "ct",
        "place",
        "pl",
        "terrace",
        "way",
        "highway",
        "hwy",
        "parkway",
        "pkwy",
        "square",
        "circle",
    ];
    t.lines().any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 3 {
            return false;
        }
        if !is_house_number(words[0]) {
            return false;
        }
        words
            .iter()
            .take(6)
            .skip(2)
            .map(|w| {
                w.trim_matches(|c: char| !c.is_ascii_alphanumeric())
                    .to_ascii_lowercase()
            })
            .any(|w| STREET_WORDS.contains(&w.as_str()))
    })
}

/// `12`, `221B`, `1600` — digits, optionally one trailing unit letter. Notably
/// NOT `1.` (a list marker) or `12,` (prose).
fn is_house_number(w: &str) -> bool {
    let digits = w.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 6 {
        return false;
    }
    match w.len() - digits {
        0 => true,
        1 => w.chars().last().is_some_and(|c| c.is_ascii_alphabetic()),
        _ => false,
    }
}

fn is_single_token_email(t: &str) -> bool {
    // Keep in lockstep with `detect::is_email` (incl. the leading-dot guard) so an
    // address is never tagged PII here but typed PlainText there, or vice-versa.
    !t.contains(char::is_whitespace)
        && t.split_once('@').is_some_and(|(l, d)| {
            !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
        })
}

/// Card-shaped: a run of 13–19 digits (allowing embedded spaces/dashes) that
/// passes the Luhn check, appearing anywhere in the text — so a label like
/// `card 4242 4242 4242 4242` or `CC: 4242-4242-4242-4242` still classifies.
fn is_credit_card(t: &str) -> bool {
    // Split on anything that isn't a digit/space/dash, then Luhn-check each run's
    // digits. This keeps a trailing `exp 12/26` from being glued onto the number.
    t.split(|c: char| !(c.is_ascii_digit() || c == ' ' || c == '-'))
        .any(|chunk| {
            let digits: Vec<u8> = chunk
                .bytes()
                .filter(u8::is_ascii_digit)
                .map(|b| b - b'0')
                .collect();
            (13..=19).contains(&digits.len()) && luhn_valid(&digits)
        })
}

/// Standard Luhn checksum over already-parsed digits.
fn luhn_valid(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        let mut v = d as u32;
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_classified() {
        assert_eq!(classify("AKIAIOSFODNN7EXAMPLE"), Sensitivity::Secret);
        assert_eq!(classify("ghp_0123456789abcdefghij"), Sensitivity::Secret);
        assert_eq!(
            classify("-----BEGIN RSA PRIVATE KEY-----\nMII...\n-----END RSA PRIVATE KEY-----"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc"),
            Sensitivity::Secret
        );
    }

    #[test]
    fn modern_provider_prefixes_classified() {
        // Fine-grained GitHub PAT, GitHub server token, Stripe live secret key,
        // GitLab PAT, Slack app token, Google API key + OAuth access token.
        assert_eq!(
            classify("github_pat_11ABCDEFG0abcdefghijklmnop"),
            Sensitivity::Secret
        );
        assert_eq!(classify("ghs_0123456789abcdefghij"), Sensitivity::Secret);
        assert_eq!(
            classify("sk_live_0123456789abcdefghij"),
            Sensitivity::Secret
        );
        assert_eq!(classify("glpat-0123456789abcdefghij"), Sensitivity::Secret);
        assert_eq!(classify("xapp-1-0123456789abcdefghij"), Sensitivity::Secret);
        assert_eq!(
            classify("AIzaSyD-1234567890abcdefghijklmnop"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("ya29.a0ARrdaM-1234567890abcdefghij"),
            Sensitivity::Secret
        );
    }

    #[test]
    fn inline_url_credentials_classified() {
        // A Postgres DSN and an HTTPS basic-auth URL both carry a password.
        assert_eq!(
            classify("postgresql://admin:S3cr3tPass@db:5432/prod"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("https://admin:hunter2pass@example.com/db"),
            Sensitivity::Secret
        );
        // A colon in the PATH (not the authority) is not a credential.
        assert_eq!(classify("https://host.com/a:b"), Sensitivity::Normal);
        // A bare `user@host` (no password) is not a *credential* (never Secret);
        // it may still be Pii on other grounds (the userinfo looks email-shaped).
        assert_ne!(classify("ssh://git@example.com/repo"), Sensitivity::Secret);
    }

    #[test]
    fn labeled_secret_values_classified() {
        // A long value behind a secret-ish key, with no recognisable prefix.
        assert_eq!(
            classify("aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("DB_PASSWORD=sup3rs3cr3t-p4ssw0rd-value"),
            Sensitivity::Secret
        );
    }

    #[test]
    fn pii_classified() {
        assert_eq!(classify("dean@example.com"), Sensitivity::Pii);
        // 4242 4242 4242 4242 is a Luhn-valid test card.
        assert_eq!(classify("4242 4242 4242 4242"), Sensitivity::Pii);
    }

    #[test]
    fn phone_numbers_classified() {
        for s in [
            "+44 20 7946 0958",
            "+15551234567",
            "(555) 123-4567",
            "555-123-4567",
            "555.123.4567",
            "call me on 020 7946 0958 tomorrow",
        ] {
            assert_eq!(classify(s), Sensitivity::Pii, "{s}");
        }
    }

    #[test]
    fn phone_lookalikes_stay_normal() {
        // Dates, IPs, versions, ids and dimensions all have digits + separators.
        for s in [
            "2026-07-26",
            "released 2026-07-26 14",
            "192.168.1.100",
            "1.2.3.4",
            "v1.20.3",
            "order 5551234567",
            "1920 1080 3840 2160",
        ] {
            assert_eq!(classify(s), Sensitivity::Normal, "{s}");
        }
    }

    #[test]
    fn street_addresses_classified() {
        for s in [
            "10 Downing Street",
            "1600 Amphitheatre Parkway, Mountain View",
            "221B Baker St, London",
            "742 Evergreen Terrace",
        ] {
            assert_eq!(classify(s), Sensitivity::Pii, "{s}");
        }
    }

    #[test]
    fn address_lookalikes_stay_normal() {
        // A numbered list item, a two-word fragment, and prose that merely uses a
        // street word must not read as an address.
        for s in [
            "1. Drive to the store",
            "3 Way merge",
            "2 Lane closure ahead",
            "12 Main",
            "take the drive way home",
        ] {
            assert_eq!(classify(s), Sensitivity::Normal, "{s}");
        }
    }

    #[test]
    fn opaque_high_entropy_blob_is_secret() {
        // A raw key with no provider prefix — the case prefix lists never catch.
        assert_eq!(
            classify("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("Zm9vYmFyMTIzNDU2Nzg5MEFCQ0RFRkdI9x2Q"),
            Sensitivity::Secret
        );
    }

    #[test]
    fn entropy_heuristic_ignores_hashes_ids_and_code() {
        // The families the mixed-case rule deliberately excludes: hex digests and
        // UUIDs (no uppercase), plus paths/identifiers/prose (no digits, or too
        // structured), plus anything embedded rather than copied alone.
        for s in [
            // 40-char git sha
            "9f8e7d6c5b4a39281706e5d4c3b2a1908f7e6d5c",
            // sha256 digest
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "550e8400-e29b-41d4-a716-446655440000",
            "/home/user/copycat/crates/copycat-core/src",
            "getUserAccountByIdentifierAndTenant",
            "MAX_REQUEST_BYTES_PER_CONNECTION_LIMIT",
            "let decoded = base64::decode(payload)?;",
            // A blob inside prose is data, not a copied credential.
            "the checksum was Zm9vYmFyMTIzNDU2Nzg5MEFCQ0RFRkdI9x2Q ok",
            // Repetitive: long and mixed-class, but far too structured.
            "AbC123AbC123AbC123AbC123AbC123",
        ] {
            assert_eq!(classify(s), Sensitivity::Normal, "{s}");
        }
    }

    #[test]
    fn normal_text() {
        assert_eq!(classify("just a normal sentence"), Sensitivity::Normal);
        // Card-length but Luhn-invalid -> not classified as a card.
        assert_eq!(classify("1234 5678 9012 3456"), Sensitivity::Normal);
        // Leading-dot domain is not an email here either (matches detect::is_email).
        assert_eq!(classify("user@.com"), Sensitivity::Normal);
        // An ordinary `name: value` line must not trip the labeled-secret path.
        assert_eq!(classify("name: value"), Sensitivity::Normal);
        // A short value behind a secret-ish key stays Normal (too short to be one).
        assert_eq!(classify("password: hunter2"), Sensitivity::Normal);
    }

    #[test]
    fn luhn_works() {
        assert!(luhn_valid(&[
            4, 2, 4, 2, 4, 2, 4, 2, 4, 2, 4, 2, 4, 2, 4, 2
        ]));
        assert!(!luhn_valid(&[1, 2, 3, 4]));
    }

    #[test]
    fn secrets_in_common_shapes() {
        // Labeled / assignment forms must be caught, not just a bare token.
        assert_eq!(
            classify("export GITHUB_TOKEN=ghp_0123456789abcdef"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("GITHUB_TOKEN=ghp_0123456789abcdef"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("password: sk-proj-0123456789abcdef"),
            Sensitivity::Secret
        );
        assert_eq!(
            classify("Authorization: Bearer sk-0123456789abcdef"),
            Sensitivity::Secret
        );
    }

    #[test]
    fn cards_with_labels() {
        assert_eq!(classify("card 4242 4242 4242 4242"), Sensitivity::Pii);
        assert_eq!(classify("CC: 4242-4242-4242-4242"), Sensitivity::Pii);
        assert_eq!(
            classify("my visa is 4242424242424242 exp 12/26"),
            Sensitivity::Pii
        );
        // A labeled but Luhn-invalid number stays Normal.
        assert_eq!(classify("card 1234 5678 9012 3456"), Sensitivity::Normal);
    }
}
