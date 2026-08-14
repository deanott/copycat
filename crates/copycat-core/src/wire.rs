//! The line framing every daemon client speaks: one newline-terminated JSON
//! [`Request`] out, one [`Response`] back, then the connection closes. Shared so
//! the contract and its failure messages can't drift between front-ends.
//! Transport stays per-crate (std for the CLI/GUI, tokio for the MCP proxy).

use std::path::Path;

use crate::proto::{Request, Response};

/// Shown when the socket connection can't be established. Built with the
/// offending path so every surface phrases the failure identically.
pub fn unreachable_msg(path: &Path) -> String {
    format!(
        "could not reach copycatd at {} — is the daemon running? Start it with `copycatd`.",
        path.display()
    )
}

/// Turn a failed attempt to open the socket into the error a front-end reports.
///
/// Not every failure is "the daemon isn't running". A refusal from
/// [`crate::ipc::check_trust`] means the opposite — something *is* listening and
/// we declined to talk to it — so wrapping it in "is the daemon running? Start it
/// with `copycatd`" would send the user to restart a healthy daemon and bury the
/// one line naming the actual fix.
///
/// The `io::Error` is kept as the chain's root either way, which is what
/// [`is_connect_refusal`] inspects.
pub fn connect_failure(path: &Path, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        // Already a full explanation; adding context would print it twice.
        return anyhow::Error::new(e);
    }
    let msg = format!("{} ({e})", unreachable_msg(path));
    anyhow::Error::new(e).context(msg)
}

/// Does this error chain bottom out in a refusal to open the socket? Front-ends
/// use it to suppress "start the daemon" advice that would be actively
/// misleading. Typed rather than string-matched so the wording stays free to
/// change.
pub fn is_connect_refusal(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// Shown when the daemon accepts the connection but closes it without a reply.
pub const NO_REPLY: &str = "daemon closed the connection without replying";

/// Serialize a request to its single wire line (JSON + trailing newline).
pub fn encode(req: &Request) -> serde_json::Result<String> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    Ok(line)
}

/// Parse a response from a single wire line.
pub fn decode(line: &str) -> serde_json::Result<Response> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn encode_is_one_newline_terminated_line() {
        let line = encode(&Request::Ping).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.trim_end().contains('\n'));
    }

    #[test]
    fn encode_then_decode_roundtrips() {
        let line = encode(&Request::Ping).unwrap();
        let resp_line = serde_json::to_string(&Response::Pong).unwrap();
        assert!(matches!(
            serde_json::from_str::<Request>(line.trim_end()).unwrap(),
            Request::Ping
        ));
        assert!(matches!(decode(&resp_line).unwrap(), Response::Pong));
    }

    /// "Start the daemon" is the right advice for a missing socket…
    #[test]
    fn an_absent_daemon_is_reported_as_unreachable() {
        let e = connect_failure(
            &PathBuf::from("/run/copycatd.sock"),
            std::io::Error::new(std::io::ErrorKind::NotFound, "No such file"),
        );
        assert!(e.to_string().contains("could not reach copycatd"), "{e}");
        assert!(!is_connect_refusal(&e), "not a refusal: {e}");
    }

    /// …and exactly the wrong advice for a socket we declined to open.
    #[test]
    fn a_refusal_keeps_its_own_explanation() {
        let e = connect_failure(
            &PathBuf::from("/run/copycatd.sock"),
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "mode 0666 lets …"),
        );
        assert!(e.to_string().contains("mode 0666"), "{e}");
        assert!(
            !e.to_string().contains("is the daemon running"),
            "must not tell the user to restart a healthy daemon: {e}"
        );
        assert!(is_connect_refusal(&e), "{e}");
    }

    /// The message must appear once. Wrapping an error in context equal to its
    /// own text prints the whole sentence twice in anyhow's report.
    #[test]
    fn a_refusal_is_not_reported_twice() {
        let e = connect_failure(
            &PathBuf::from("/run/copycatd.sock"),
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "UNIQUEMARKER"),
        );
        let report = format!("{e:?}");
        assert_eq!(
            report.matches("UNIQUEMARKER").count(),
            1,
            "explanation duplicated in: {report}"
        );
    }

    #[test]
    fn unreachable_msg_names_the_path_and_the_fix() {
        let m = unreachable_msg(&PathBuf::from("/run/user/1000/copycatd.sock"));
        assert!(m.contains("could not reach copycatd"), "{m}");
        assert!(m.contains("/run/user/1000/copycatd.sock"), "{m}");
        assert!(m.contains("copycatd"), "{m}");
    }
}
