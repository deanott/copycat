//! The insecure debug-mode gate — the ONE place the two safety gates combine.
//!
//! copycat masks secrets/PII on every human- and model-facing surface. For
//! debugging you sometimes need to see the raw values; [`reveal_secrets`] is the
//! single switch that lifts the mask, and it is deliberately hard to turn on:
//!
//!   1. **Build gate** — the code only exists when the crate is built with the
//!      `debug-unsafe` cargo feature. In a normal (release) build this function
//!      is a `const false`, so the compiler eliminates every reveal branch: the
//!      secret-printing code isn't just unreachable, it's gone from the binary.
//!   2. **Runtime gate** — even in a `debug-unsafe` build it stays off until the
//!      `COPYCAT_INSECURE` environment variable is set.
//!
//! Neither gate alone is enough. Front-ends must route every "should I show the
//! raw value?" decision through here rather than checking the env var directly,
//! so the build gate can never be bypassed.

/// Whether Secret/PII values may be shown in cleartext.
///
/// `false` unless the crate was built with `debug-unsafe` **and**
/// `COPYCAT_INSECURE` is set in the environment. See the module docs.
#[inline]
pub fn reveal_secrets() -> bool {
    #[cfg(feature = "debug-unsafe")]
    {
        std::env::var_os("COPYCAT_INSECURE").is_some()
    }
    #[cfg(not(feature = "debug-unsafe"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "debug-unsafe"))]
    fn release_build_never_reveals() {
        // Without the build feature the reveal path does not exist, regardless
        // of the environment. This is the default (no-feature) test run.
        std::env::set_var("COPYCAT_INSECURE", "1");
        assert!(
            !reveal_secrets(),
            "release builds must never reveal secrets"
        );
        std::env::remove_var("COPYCAT_INSECURE");
    }

    #[test]
    #[cfg(feature = "debug-unsafe")]
    fn debug_build_reveals_only_with_env() {
        std::env::remove_var("COPYCAT_INSECURE");
        assert!(!reveal_secrets(), "off until the runtime opt-in is set");
        std::env::set_var("COPYCAT_INSECURE", "1");
        assert!(reveal_secrets(), "on with feature + env");
        std::env::remove_var("COPYCAT_INSECURE");
    }
}
