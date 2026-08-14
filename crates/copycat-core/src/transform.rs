//! Deterministic edit-on-paste egress transforms, applied on the way *out*
//! without mutating the stored entry. Pure and total: malformed input is an
//! `Err`, never a panic. Non-deterministic rewrites (translate, summarise)
//! need an LLM and stay client-side, so the daemon never ships plaintext to a
//! model. Transforms do not move the trust boundary: Secret/PII masking is
//! applied by front-ends AFTER (and independently of) any transform.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};

/// A deterministic egress transform applied to a clipboard value on paste.
///
/// Serialised in `snake_case` on the wire (`json_pretty`, `base64_encode`, …);
/// [`FromStr`] accepts the same names, so the CLI `--as base64_encode` and the
/// MCP `transform` arg share one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transform {
    /// Parse as JSON, re-emit pretty-printed (2-space indent). Err on invalid JSON.
    JsonPretty,
    /// Parse as JSON, re-emit compact (no insignificant whitespace). Err on invalid JSON.
    JsonMinify,
    /// Standard base64-encode the UTF-8 bytes.
    Base64Encode,
    /// Standard base64-decode, then interpret the bytes as UTF-8. Err on invalid
    /// base64 or non-UTF-8 output.
    Base64Decode,
    /// Uppercase (Unicode-aware).
    Upper,
    /// Lowercase (Unicode-aware).
    Lower,
    /// Strip leading/trailing whitespace.
    Trim,
    /// Percent-encode every non-alphanumeric byte (aggressive, URL-safe).
    UrlEncode,
    /// Percent-decode, then interpret the bytes as UTF-8. Err on non-UTF-8 output.
    UrlDecode,
}

impl Transform {
    /// The one variant↔name table; `name()`, [`Transform::NAMES`] and
    /// [`FromStr`] all derive from it.
    const ALL: [(Transform, &'static str); 9] = [
        (Transform::JsonPretty, "json_pretty"),
        (Transform::JsonMinify, "json_minify"),
        (Transform::Base64Encode, "base64_encode"),
        (Transform::Base64Decode, "base64_decode"),
        (Transform::Upper, "upper"),
        (Transform::Lower, "lower"),
        (Transform::Trim, "trim"),
        (Transform::UrlEncode, "url_encode"),
        (Transform::UrlDecode, "url_decode"),
    ];

    /// The canonical `snake_case` name (matches the wire form and [`FromStr`]).
    pub fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(t, _)| *t == self)
            .expect("every variant is in ALL")
            .1
    }

    /// Every transform name, for help text / error messages.
    pub const NAMES: [&'static str; 9] = {
        let mut names = [""; 9];
        let mut i = 0;
        while i < names.len() {
            names[i] = Self::ALL[i].1;
            i += 1;
        }
        names
    };

    /// Apply the transform to `text`, returning the rewritten string. Pure and
    /// total: a malformed input yields `Err`, never a panic.
    pub fn apply(&self, text: &str) -> anyhow::Result<String> {
        use anyhow::Context as _;
        Ok(match self {
            Transform::JsonPretty => {
                let v: serde_json::Value =
                    serde_json::from_str(text).context("input is not valid JSON")?;
                serde_json::to_string_pretty(&v)?
            }
            Transform::JsonMinify => {
                let v: serde_json::Value =
                    serde_json::from_str(text).context("input is not valid JSON")?;
                serde_json::to_string(&v)?
            }
            Transform::Base64Encode => base64::engine::general_purpose::STANDARD.encode(text),
            Transform::Base64Decode => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(text.trim())
                    .context("input is not valid base64")?;
                String::from_utf8(bytes).context("base64 decoded to non-UTF-8 bytes")?
            }
            Transform::Upper => text.to_uppercase(),
            Transform::Lower => text.to_lowercase(),
            Transform::Trim => text.trim().to_string(),
            Transform::UrlEncode => utf8_percent_encode(text, NON_ALPHANUMERIC).to_string(),
            Transform::UrlDecode => percent_decode_str(text)
                .decode_utf8()
                .context("URL-decoded to non-UTF-8 bytes")?
                .into_owned(),
        })
    }
}

impl fmt::Display for Transform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error from [`Transform::from_str`] on an unknown name. Lists the valid names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTransformError(String);

impl fmt::Display for ParseTransformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown transform '{}' (expected one of: {})",
            self.0,
            Transform::NAMES.join(", ")
        )
    }
}

impl std::error::Error for ParseTransformError {}

impl FromStr for Transform {
    type Err = ParseTransformError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Transform::ALL
            .iter()
            .find(|(_, n)| *n == s)
            .map(|(t, _)| *t)
            .ok_or_else(|| ParseTransformError(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_pretty_reformats() {
        let out = Transform::JsonPretty.apply(r#"{"b":1,"a":[2,3]}"#).unwrap();
        assert!(out.contains("\n"), "pretty output is multi-line: {out}");
        assert!(out.contains("  "), "pretty output is indented: {out}");
        // Round-trips back to the same value.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v, serde_json::json!({"b":1,"a":[2,3]}));
    }

    #[test]
    fn json_minify_strips_whitespace() {
        let out = Transform::JsonMinify
            .apply("{\n  \"a\": 1,\n  \"b\": 2\n}")
            .unwrap();
        assert_eq!(out, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn json_transforms_error_on_invalid() {
        assert!(Transform::JsonPretty.apply("not json").is_err());
        assert!(Transform::JsonMinify.apply("{oops").is_err());
    }

    #[test]
    fn base64_roundtrips() {
        let enc = Transform::Base64Encode.apply("hello world").unwrap();
        assert_eq!(enc, "aGVsbG8gd29ybGQ=");
        let dec = Transform::Base64Decode.apply(&enc).unwrap();
        assert_eq!(dec, "hello world");
    }

    #[test]
    fn base64_decode_errors_on_non_base64() {
        // '!' is not in the base64 alphabet.
        assert!(Transform::Base64Decode.apply("not!base64!").is_err());
    }

    #[test]
    fn base64_decode_errors_on_non_utf8() {
        // 0xFF 0xFE is valid base64 but not valid UTF-8.
        let b64 = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFE]);
        assert!(Transform::Base64Decode.apply(&b64).is_err());
    }

    #[test]
    fn case_transforms() {
        assert_eq!(Transform::Upper.apply("Hello").unwrap(), "HELLO");
        assert_eq!(Transform::Lower.apply("Hello").unwrap(), "hello");
    }

    #[test]
    fn trim_strips_surrounding_whitespace() {
        assert_eq!(Transform::Trim.apply("  hi \n").unwrap(), "hi");
    }

    #[test]
    fn url_roundtrips() {
        let enc = Transform::UrlEncode.apply("a b/c?=&").unwrap();
        assert_eq!(enc, "a%20b%2Fc%3F%3D%26");
        let dec = Transform::UrlDecode.apply(&enc).unwrap();
        assert_eq!(dec, "a b/c?=&");
    }

    #[test]
    fn url_decode_errors_on_non_utf8() {
        // %FF is a valid percent-escape but decodes to a non-UTF-8 byte.
        assert!(Transform::UrlDecode.apply("%FF").is_err());
    }

    #[test]
    fn from_str_accepts_snake_case_names() {
        assert_eq!(
            "json_pretty".parse::<Transform>().unwrap(),
            Transform::JsonPretty
        );
        assert_eq!(
            "base64_encode".parse::<Transform>().unwrap(),
            Transform::Base64Encode
        );
        assert_eq!("trim".parse::<Transform>().unwrap(), Transform::Trim);
    }

    #[test]
    fn from_str_rejects_unknown_with_helpful_error() {
        let err = "bogus".parse::<Transform>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "{msg}");
        assert!(msg.contains("json_pretty"), "lists valid names: {msg}");
    }

    #[test]
    fn name_matches_serde_wire_form() {
        for (t, name) in Transform::ALL {
            // serde's snake_case rename must equal the table's name.
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(t.name(), name);
            // …and FromStr round-trips it.
            assert_eq!(name.parse::<Transform>().unwrap(), t);
        }
    }
}
