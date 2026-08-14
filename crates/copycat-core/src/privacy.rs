//! Privacy mode — the single dial for how much copycat masks.
//!
//! | [`Mode`] | detects | effect |
//! |---|---|---|
//! | [`Mode::Off`] | nothing | no tags, no masking. |
//! | [`Mode::Guarded`] (default) | credentials only | secrets masked on every surface and never sent to a model; personal data left alone. |
//! | [`Mode::Strict`] | credentials **and** personal data | additionally masks emails/phones/cards/addresses for humans and withholds them from the model. |
//!
//! The mode is **daemon-authoritative**: seeded from [`Mode::from_env`] at boot,
//! applied to every entry on the way out ([`project`]), published in
//! [`DaemonStatus::privacy`](crate::proto::DaemonStatus), and settable at runtime
//! via [`Request::SetPrivacy`](crate::proto::Request). A front-end never keeps
//! its own copy. Detection never drops or rewrites an entry — masking happens at
//! display time. [`Mode::Off`] does not lift the model-facing credential floor
//! ([`crate::view::model_sensitivity`]).

use serde::{Deserialize, Serialize};

use crate::entry::Sensitivity;

/// Environment variable that seeds the daemon's privacy mode at boot:
/// `off` | `guarded` | `strict` (case-insensitive). Unset ⇒ [`Mode::Guarded`].
pub const ENV_PRIVACY: &str = "COPYCAT_PRIVACY";

/// How much copycat protects — the user-facing privacy posture.
///
/// Ordered least→most protective, which is also least→most friction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No scanning: nothing is tagged, nothing is masked for humans. Does
    /// **not** lift the model-facing credential floor — see the module docs.
    Off,
    /// **Default.** Credentials only: API keys, tokens, private keys, passwords,
    /// connection strings and opaque high-entropy blobs are masked and withheld
    /// from models. Ordinary personal data is left alone — it is the user's own
    /// data, usually copied precisely in order to use it.
    #[default]
    Guarded,
    /// Credentials **and** personal data: emails, phones, card-shaped numbers
    /// and street addresses are masked for humans and withheld from models. For
    /// a shared screen, a hosted model, or a regulated environment.
    Strict,
}

impl Mode {
    /// Every mode, least→most protective. The order the UI lists them in.
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Guarded, Mode::Strict];

    /// Stable machine key — the wire/env/CLI spelling. Round-trips with
    /// [`Mode::from_key`].
    pub fn as_key(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Guarded => "guarded",
            Mode::Strict => "strict",
        }
    }

    /// Parse a key (case-insensitive, surrounding whitespace ignored).
    /// Unrecognised input ⇒ `None`, so a caller can decide between "fall back to
    /// the default" and "tell the user they typo'd".
    pub fn from_key(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" => Some(Mode::Off),
            // `secrets` is the descriptive alias people reach for.
            "guarded" | "secrets" | "default" | "on" | "1" => Some(Mode::Guarded),
            // `lockdown` is the legacy name for this posture.
            "strict" | "lockdown" | "hard" | "2" => Some(Mode::Strict),
            _ => None,
        }
    }

    /// Short label for a menu/combo box.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "Off",
            Mode::Guarded => "Guarded (secrets only)",
            Mode::Strict => "Strict (secrets + personal data)",
        }
    }

    /// One-sentence explanation of the trade-off, for the settings UI.
    pub fn describe(self) -> &'static str {
        match self {
            Mode::Off => {
                "Nothing is hidden. Passwords and API keys in your history are readable on \
                 screen. (Models are still never handed a detected credential.)"
            }
            Mode::Guarded => {
                "Hides credentials only — API keys, tokens, private keys, passwords, \
                 connection strings. Emails, phone numbers and addresses stay visible."
            }
            Mode::Strict => {
                "Also hides personal data: emails, phone numbers, card numbers, street \
                 addresses. Safest, and the most obstructive — expect masked rows."
            }
        }
    }

    /// Whether this mode acts on `sensitivity` at all.
    pub fn covers(self, sensitivity: Sensitivity) -> bool {
        match (self, sensitivity) {
            (_, Sensitivity::Normal) | (Mode::Off, _) => false,
            (Mode::Guarded, Sensitivity::Pii) => false,
            (Mode::Guarded, Sensitivity::Secret) | (Mode::Strict, _) => true,
        }
    }

    /// The mode this process's environment asks for: [`ENV_PRIVACY`], else the
    /// legacy `COPYCAT_LOCKDOWN` flag (⇒ [`Mode::Strict`]), else the default.
    /// An unparseable `COPYCAT_PRIVACY` falls back to the default rather than
    /// failing the daemon's boot — a typo must not leave you *less* protected
    /// than `Guarded`.
    ///
    /// Read once, by the **daemon**, at startup. A front-end in another process
    /// must honour [`DaemonStatus::privacy`](crate::proto::DaemonStatus) instead.
    pub fn from_env() -> Mode {
        match std::env::var(ENV_PRIVACY) {
            Ok(v) => Mode::from_key(&v).unwrap_or_default(),
            // COPYCAT_LOCKDOWN is the legacy alias for COPYCAT_PRIVACY=strict.
            Err(_) if std::env::var_os("COPYCAT_LOCKDOWN").is_some() => Mode::Strict,
            Err(_) => Mode::default(),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_key())
    }
}

/// Filter a full-fidelity [`Sensitivity`] through the active mode: what the rest
/// of the system should treat this entry as.
///
/// The daemon applies this to every entry on the way out of the store, which is
/// what makes the mode take effect everywhere at once — including on entries
/// copied before the mode was changed — without any front-end knowing the
/// setting exists.
pub fn project(sensitivity: Sensitivity, mode: Mode) -> Sensitivity {
    if mode.covers(sensitivity) {
        sensitivity
    } else {
        Sensitivity::Normal
    }
}

/// THE model-egress policy: may a value with this (already
/// [projected](project)) sensitivity be shown to an LLM?
///
/// - `Secret` — never, in any mode.
/// - `Pii` — withheld under [`Mode::Strict`], sent otherwise. Feeding the
///   clipboard to agents is copycat's job, and personal data is usually the
///   payload the user wants used.
/// - `Normal` — always.
pub fn reveal_to_model(sensitivity: Sensitivity, mode: Mode) -> bool {
    match sensitivity {
        Sensitivity::Secret => false,
        Sensitivity::Pii => mode != Mode::Strict,
        Sensitivity::Normal => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative payload of each class, reused across the tier tests.
    const SECRET: &str = "ghp_0123456789abcdefghij";
    const EMAIL: &str = "dean@example.com";
    const PROSE: &str = "just a normal sentence";

    /// What the daemon's classify-then-project pipeline yields for `s` at `mode`.
    fn tier(s: &str, mode: Mode) -> Sensitivity {
        project(crate::pii::classify(s), mode)
    }

    #[test]
    fn default_is_guarded() {
        assert_eq!(Mode::default(), Mode::Guarded);
    }

    #[test]
    fn keys_round_trip_and_accept_aliases() {
        for m in Mode::ALL {
            assert_eq!(Mode::from_key(m.as_key()), Some(m));
            assert_eq!(m.to_string(), m.as_key());
        }
        assert_eq!(Mode::from_key(" STRICT "), Some(Mode::Strict));
        assert_eq!(Mode::from_key("lockdown"), Some(Mode::Strict));
        assert_eq!(Mode::from_key("secrets"), Some(Mode::Guarded));
        assert_eq!(Mode::from_key("nonsense"), None);
    }

    /// The only test that touches COPYCAT_PRIVACY / COPYCAT_LOCKDOWN, so no
    /// cross-test env guard is needed.
    #[test]
    fn from_env_honours_the_legacy_lockdown_alias() {
        let prev_privacy = std::env::var_os(ENV_PRIVACY);
        let prev_lockdown = std::env::var_os("COPYCAT_LOCKDOWN");

        std::env::remove_var(ENV_PRIVACY);
        std::env::remove_var("COPYCAT_LOCKDOWN");
        assert_eq!(Mode::from_env(), Mode::Guarded);

        std::env::set_var("COPYCAT_LOCKDOWN", "1");
        assert_eq!(Mode::from_env(), Mode::Strict);

        // An explicit COPYCAT_PRIVACY wins over the legacy flag.
        std::env::set_var(ENV_PRIVACY, "off");
        assert_eq!(Mode::from_env(), Mode::Off);

        match prev_privacy {
            Some(v) => std::env::set_var(ENV_PRIVACY, v),
            None => std::env::remove_var(ENV_PRIVACY),
        }
        match prev_lockdown {
            Some(v) => std::env::set_var("COPYCAT_LOCKDOWN", v),
            None => std::env::remove_var("COPYCAT_LOCKDOWN"),
        }
    }

    #[test]
    fn serde_uses_snake_case_keys() {
        assert_eq!(
            serde_json::to_string(&Mode::Guarded).unwrap(),
            r#""guarded""#
        );
        assert_eq!(
            serde_json::from_str::<Mode>(r#""strict""#).unwrap(),
            Mode::Strict
        );
    }

    #[test]
    fn guarded_tier_catches_credentials() {
        for s in [
            SECRET,
            "AKIAIOSFODNN7EXAMPLE",
            "postgresql://admin:S3cr3tPass@db:5432/prod",
            "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "-----BEGIN RSA PRIVATE KEY-----\nMII...\n-----END RSA PRIVATE KEY-----",
        ] {
            assert_eq!(tier(s, Mode::Guarded), Sensitivity::Secret, "{s}");
            assert_eq!(tier(s, Mode::Strict), Sensitivity::Secret, "{s}");
        }
    }

    #[test]
    fn strict_tier_adds_personal_data() {
        for s in [EMAIL, "+44 20 7946 0958", "4242 4242 4242 4242"] {
            assert_eq!(tier(s, Mode::Strict), Sensitivity::Pii, "{s}");
        }
    }

    #[test]
    fn guarded_tier_ignores_personal_data() {
        // The headline promise of the default mode: no nagging about an email.
        for s in [
            EMAIL,
            "+44 20 7946 0958",
            "4242 4242 4242 4242",
            "10 Downing Street",
        ] {
            assert_eq!(tier(s, Mode::Guarded), Sensitivity::Normal, "{s}");
        }
    }

    #[test]
    fn no_tier_flags_ordinary_text() {
        for s in [
            PROSE,
            "let x = compute(y).unwrap_or_default();",
            "TODO: fix the flaky test",
        ] {
            for mode in Mode::ALL {
                assert_eq!(tier(s, mode), Sensitivity::Normal, "{s} @ {mode}");
            }
        }
    }

    #[test]
    fn off_detects_nothing() {
        for s in [SECRET, EMAIL, PROSE] {
            assert_eq!(tier(s, Mode::Off), Sensitivity::Normal, "{s}");
        }
    }

    #[test]
    fn covers_is_the_masking_scope() {
        assert!(!Mode::Off.covers(Sensitivity::Secret));
        assert!(Mode::Guarded.covers(Sensitivity::Secret));
        assert!(!Mode::Guarded.covers(Sensitivity::Pii));
        assert!(Mode::Strict.covers(Sensitivity::Secret));
        assert!(Mode::Strict.covers(Sensitivity::Pii));
        for m in Mode::ALL {
            assert!(!m.covers(Sensitivity::Normal), "{m}");
        }
    }

    #[test]
    fn secret_never_reaches_the_model_in_any_mode() {
        for m in Mode::ALL {
            assert!(!reveal_to_model(Sensitivity::Secret, m), "{m}");
        }
    }

    #[test]
    fn strict_is_the_only_mode_that_withholds_pii() {
        assert!(reveal_to_model(Sensitivity::Pii, Mode::Off));
        assert!(reveal_to_model(Sensitivity::Pii, Mode::Guarded));
        assert!(!reveal_to_model(Sensitivity::Pii, Mode::Strict));
        for m in Mode::ALL {
            assert!(reveal_to_model(Sensitivity::Normal, m), "{m}");
        }
    }
}
