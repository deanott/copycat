//! OSC 52: mirror a remote copy onto the local terminal's clipboard, riding the
//! SSH pipe that already exists. One-way (remote → local) because terminals
//! disable the read query as an exfiltration risk. Opt-in (`--osc52` /
//! `COPYCAT_OSC52=1`) because unsolicited escape bytes corrupt logs and pipes.

use base64::Engine;

use copycat_core::proto::{Request, Response};

/// Env var that turns mirroring on for a whole session.
const ENV: &str = "COPYCAT_OSC52";

/// xterm caps a whole OSC sequence near 100 KB and silently drops over-limit
/// sequences, so refuse loudly instead.
pub const MAX_BASE64: usize = 74_994;

/// Why a value could not be routed to the local clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooBig {
    /// Encoded length, and the cap it exceeded.
    Encoded { len: usize, max: usize },
}

impl TooBig {
    pub fn explain(&self) -> String {
        match self {
            TooBig::Encoded { len, max } => format!(
                "value is too large for OSC 52 ({len} base64 bytes, terminals cap \
                 around {max}); it IS on the remote daemon — `copycat paste` there, \
                 or forward the daemon's socket over SSH instead"
            ),
        }
    }
}

/// Build the escape sequence that puts `text` on the local clipboard. `c` is the
/// CLIPBOARD selection (not `p`, primary) — the one Ctrl/Cmd+V pastes from.
pub fn sequence(text: &str) -> Result<String, TooBig> {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    if payload.len() > MAX_BASE64 {
        return Err(TooBig::Encoded {
            len: payload.len(),
            max: MAX_BASE64,
        });
    }
    // BEL (\x07) rather than ST (\x1b\\) as the terminator: both are legal, but
    // BEL is the form with the broadest terminal support, including older xterm
    // and the macOS Terminal.
    Ok(format!("\x1b]52;c;{payload}\x07"))
}

/// Should a copy be mirrored to the local clipboard? `--osc52` or `COPYCAT_OSC52`
/// set to anything but `0`/`false`/empty.
pub fn wanted(flag: bool, env: Option<&str>) -> bool {
    if flag {
        return true;
    }
    match env {
        Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false"),
        None => false,
    }
}

/// The text a completed request should mirror, if any.
///
/// Two conditions beyond the opt-in. Only a `Copy` on the **default lane** is
/// mirrored: a named channel is an agent hand-off lane and must never hijack the
/// terminal's clipboard. And only once the daemon has accepted, so a failed copy
/// never lands a value the store lacks.
fn should_mirror<'a>(
    flag: bool,
    env: Option<&str>,
    req: &'a Request,
    res: &Response,
) -> Option<&'a str> {
    match (req, res) {
        (
            Request::Copy {
                text,
                channel: None,
                ..
            },
            Response::Copied { .. },
        ) if wanted(flag, env) => Some(text),
        _ => None,
    }
}

/// Mirror a just-accepted copy onto the terminal's clipboard when asked to.
/// Best-effort: a terminal that cannot take the escape is a warning, not a
/// failed copy, because the value is on the daemon either way.
pub fn mirror_if_wanted(flag: bool, req: &Request, res: &Response) {
    let env = std::env::var(ENV).ok();
    let Some(text) = should_mirror(flag, env.as_deref(), req, res) else {
        return;
    };
    if let Err(e) = write_to_tty(text) {
        eprintln!("copycat: {e}");
    }
}

/// Write the sequence to `/dev/tty`, never stdout: stdout is routinely piped and
/// the escape bytes would corrupt the stream.
#[cfg(unix)]
fn write_to_tty(text: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let seq = sequence(text).map_err(|e| anyhow::anyhow!("{}", e.explain()))?;
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|e| {
            anyhow::anyhow!("--osc52 needs a controlling terminal to write to (/dev/tty: {e})")
        })?;
    tty.write_all(seq.as_bytes())?;
    tty.flush()?;
    Ok(())
}

/// Windows has no `/dev/tty` to write the escape to; refuse plainly.
#[cfg(windows)]
fn write_to_tty(_text: &str) -> anyhow::Result<()> {
    anyhow::bail!("--osc52 is not supported on Windows (no /dev/tty to write the escape to)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_clipboard_selection() {
        // "hi" -> aGk=
        assert_eq!(sequence("hi").unwrap(), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn round_trips_through_base64() {
        let seq = sequence("hello world").unwrap();
        let payload = seq
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "hello world");
    }

    /// Non-ASCII must survive: base64 is over the UTF-8 bytes, not chars.
    #[test]
    fn handles_multibyte_text() {
        let seq = sequence("café ☕").unwrap();
        let payload = seq
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "café ☕");
    }

    /// Over the cap we refuse loudly rather than emitting a sequence the terminal
    /// will silently discard.
    #[test]
    fn refuses_oversized_values() {
        // 3 raw bytes -> 4 base64 bytes, so this comfortably clears the cap.
        let big = "x".repeat(MAX_BASE64);
        let err = sequence(&big).unwrap_err();
        match err {
            TooBig::Encoded { len, max } => {
                assert!(len > max);
                assert_eq!(max, MAX_BASE64);
            }
        }
        assert!(err.explain().contains("forward"), "{}", err.explain());
    }

    #[test]
    fn a_value_at_the_cap_is_still_emitted() {
        // 3 raw bytes encode to exactly 4 base64 bytes, no padding.
        let raw = "x".repeat(MAX_BASE64 / 4 * 3);
        assert!(sequence(&raw).is_ok());
    }

    #[test]
    fn opt_in_requires_flag_or_env() {
        assert!(!wanted(false, None));
        assert!(wanted(true, None));
        assert!(wanted(false, Some("1")));
        assert!(wanted(false, Some("yes")));
        // Explicitly-off env values must not enable it.
        for off in ["0", "false", "FALSE", "", "  "] {
            assert!(!wanted(false, Some(off)), "{off:?} must not enable");
        }
        // The flag still wins over an off env value.
        assert!(wanted(true, Some("0")));
    }

    fn copy_on(channel: Option<&str>) -> Request {
        Request::Copy {
            text: "hi".into(),
            channel: channel.map(str::to_string),
            author: None,
            session_id: None,
        }
    }

    #[test]
    fn mirrors_an_accepted_copy_on_the_default_lane() {
        let res = Response::Copied { id: "1".into() };
        assert_eq!(should_mirror(true, None, &copy_on(None), &res), Some("hi"));
    }

    /// A named channel is an agent hand-off lane: mirroring it would hijack the
    /// terminal's clipboard with a value the user never copied.
    #[test]
    fn never_mirrors_a_named_channel() {
        let res = Response::Copied { id: "1".into() };
        assert_eq!(
            should_mirror(true, None, &copy_on(Some("build")), &res),
            None
        );
    }

    /// Without the opt-in, nothing is emitted however the copy went.
    #[test]
    fn never_mirrors_without_the_opt_in() {
        let res = Response::Copied { id: "1".into() };
        assert_eq!(should_mirror(false, None, &copy_on(None), &res), None);
        assert_eq!(should_mirror(false, Some("0"), &copy_on(None), &res), None);
    }

    /// A rejected copy must not land on the local clipboard: the store lacks it.
    #[test]
    fn never_mirrors_a_failed_copy() {
        let err = Response::Error {
            message: "nope".into(),
        };
        assert_eq!(should_mirror(true, None, &copy_on(None), &err), None);
    }

    /// Only `Copy` mirrors — reading an entry must not re-emit it.
    #[test]
    fn never_mirrors_a_non_copy_request() {
        let res = Response::Copied { id: "1".into() };
        assert_eq!(should_mirror(true, None, &Request::Current, &res), None);
    }

    /// Clearing the clipboard is a legitimate thing to mirror, and the empty
    /// payload must still be a well-formed sequence rather than a bare prefix.
    #[test]
    fn an_empty_value_is_still_a_valid_sequence() {
        assert_eq!(sequence("").unwrap(), "\x1b]52;c;\x07");
    }

    /// The cap is on the ENCODED length, which is 4/3 of the raw bytes. A value
    /// that looks comfortably under the limit in characters can still exceed it,
    /// so the boundary is worth pinning from both sides.
    #[test]
    fn the_cap_is_on_encoded_length_not_raw_length() {
        let just_over = "x".repeat(MAX_BASE64 / 4 * 3 + 1);
        assert!(just_over.len() < MAX_BASE64, "raw length is under the cap");
        assert!(
            sequence(&just_over).is_err(),
            "but its base64 is over, so it must be refused"
        );
    }

    /// Multibyte text encodes 4 base64 bytes per 3 UTF-8 bytes, not per char, so
    /// the cap bites sooner than a char count suggests.
    #[test]
    fn the_cap_counts_utf8_bytes_not_chars() {
        // Each '☕' is 3 UTF-8 bytes, so this is exactly at the raw-byte cap.
        let raw = "☕".repeat(MAX_BASE64 / 4);
        assert!(raw.chars().count() < MAX_BASE64);
        assert!(sequence(&raw).is_ok());
        assert!(sequence(&format!("{raw}☕")).is_err());
    }

    /// The payload carries no BEL of its own, so a value cannot smuggle a
    /// terminator and leave the terminal parsing clipboard bytes as commands.
    #[test]
    fn a_control_byte_in_the_value_cannot_terminate_the_sequence() {
        let seq = sequence("a\x07b\x1b]52;c;evil\x07").unwrap();
        assert_eq!(seq.matches('\x07').count(), 1, "exactly one terminator");
        assert!(seq.ends_with('\x07'), "and it is the last byte");
        // Everything between the prefix and the terminator is base64, which has
        // no control characters by construction.
        let payload = seq
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert!(payload.bytes().all(|b| b.is_ascii_graphic()));
    }

    /// The env var is read case-insensitively and trimmed, so a value pasted
    /// with stray whitespace behaves the way it looks.
    #[test]
    fn env_values_are_trimmed_and_case_folded() {
        for on in [" 1 ", "TRUE", "True", "\tyes\n"] {
            assert!(wanted(false, Some(on)), "{on:?} must enable");
        }
        for off in [" 0 ", "False", "\tFALSE\n"] {
            assert!(!wanted(false, Some(off)), "{off:?} must not enable");
        }
    }

    /// A copy the daemon answered with something other than `Copied` has not
    /// landed in the store, whatever the response is.
    #[test]
    fn never_mirrors_when_the_daemon_answered_with_anything_but_copied() {
        for res in [Response::Pong, Response::Cleared { removed: 0 }] {
            assert_eq!(
                should_mirror(true, None, &copy_on(None), &res),
                None,
                "{res:?} is not an accepted copy"
            );
        }
    }
}
