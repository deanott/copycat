//! Async client to `copycatd` over its local channel (unix socket, or a named
//! pipe on Windows — see `copycat_core::ipc`).
//!
//! The MCP server is a thin façade: every tool call becomes one
//! `copycat_core::proto` request to the daemon, which is the single owner of the
//! temporal store and the OS clipboard. Keeping all logic in the daemon means
//! Claude Code, claude.ai, and the CLI all see the exact same clipboard.

use std::path::PathBuf;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use copycat_core::proto::{Request, Response};
use copycat_core::wire;

/// Open one connection to the daemon.
///
/// Windows: a named-pipe client handle. `ERROR_PIPE_BUSY` means every instance is
/// momentarily serving someone else — the daemon creates a replacement instance
/// as soon as it accepts, so a short retry turns a spurious failure into a pause.
/// Unix has no equivalent state; `connect(2)` queues in the listen backlog.
///
/// The trust check runs here too, not just in the blocking client: a remote
/// Claude Code reaching a laptop daemon over an `ssh -R` forward is precisely
/// the case where the socket was created by sshd rather than by copycatd, so
/// none of the daemon's bind-time `0600` enforcement applies to it. See
/// [`copycat_core::ipc::check_trust`].
#[cfg(unix)]
async fn connect(path: &std::path::Path) -> std::io::Result<tokio::net::UnixStream> {
    copycat_core::ipc::enforce_trust(path)?;
    tokio::net::UnixStream::connect(path).await
}

#[cfg(windows)]
async fn connect(
    path: &std::path::Path,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = path.as_os_str();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match ClientOptions::new().open(name) {
            Ok(c) => return Ok(c),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A handle to the daemon socket. One short-lived connection per request.
#[derive(Clone)]
pub struct Daemon {
    path: PathBuf,
}

impl Daemon {
    pub fn new(path: PathBuf) -> Self {
        Daemon { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Send one request and await the single-line response.
    pub async fn call(&self, req: &Request) -> anyhow::Result<Response> {
        let stream = connect(&self.path)
            .await
            .map_err(|e| wire::connect_failure(&self.path, e))?;
        let (read_half, mut write_half) = tokio::io::split(stream);

        write_half
            .write_all(wire::encode(req)?.as_bytes())
            .await
            .context("send request")?;
        write_half.flush().await.ok();

        let mut reader = BufReader::new(read_half);
        let mut resp = String::new();
        let n = reader.read_line(&mut resp).await.context("read response")?;
        if n == 0 {
            anyhow::bail!(wire::NO_REPLY);
        }
        wire::decode(&resp).context("parse response")
    }
}

// These drive the proxy against a fake daemon built on a `UnixListener`, so they
// are unix-only. The Windows client path (`ClientOptions::open` above) is thin,
// and the named-pipe transport it talks to IS covered end-to-end on Windows by
// `copycatd::server`'s suite, which runs the full client→pipe→dispatch round trip
// on both platforms. What is NOT covered on Windows is this file's busy-retry
// loop; if the pipe transport ever grows teeth, port `fake_daemon` to
// `ServerOptions` rather than leaving that gap silent.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::net::UnixListener;

    static N: AtomicU32 = AtomicU32::new(0);

    /// Bind a fake daemon the way the real one does — `0600`.
    ///
    /// Not incidental tidiness: `connect` now runs `ipc::check_trust`, so a fake
    /// left at the default umask is refused exactly like an exposed `ssh -R`
    /// socket would be. Binding at the daemon's own mode keeps these tests
    /// testing the proxy rather than the trust check (which has its own tests in
    /// `copycat_core::ipc`).
    fn bind_private(path: &std::path::Path) -> UnixListener {
        use std::os::unix::fs::PermissionsExt;
        let l = UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        l
    }

    fn temp_sock() -> PathBuf {
        // Unique per test without a randomness source (unavailable here): pid + a
        // process-local counter. A plain temp-dir path is fine for the fake; the
        // mode that matters is set by `bind_private`.
        let mut p = std::env::temp_dir();
        p.push(format!(
            "copycat-proxy-test-{}-{}.sock",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// One-shot fake daemon: accept a connection, read the single request line
    /// (returned to the caller), then send `reply` (or close silently if `None`).
    async fn fake_daemon(listener: UnixListener, reply: Option<String>) -> String {
        let (stream, _) = listener.accept().await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut got = String::new();
        reader.read_line(&mut got).await.unwrap();
        if let Some(r) = reply {
            write_half.write_all(r.as_bytes()).await.unwrap();
            write_half.write_all(b"\n").await.unwrap();
            write_half.flush().await.ok();
        }
        got
    }

    #[tokio::test]
    async fn call_frames_request_and_parses_response() {
        let path = temp_sock();
        let listener = bind_private(&path);
        let reply = serde_json::to_string(&Response::Pong).unwrap();
        let server = tokio::spawn(fake_daemon(listener, Some(reply)));

        let resp = Daemon::new(path.clone())
            .call(&Request::Ping)
            .await
            .unwrap();
        assert!(matches!(resp, Response::Pong));

        // The request went out as exactly one newline-terminated JSON line.
        let got = server.await.unwrap();
        assert!(
            got.ends_with('\n'),
            "request must be newline-framed: {got:?}"
        );
        assert!(matches!(
            serde_json::from_str::<Request>(got.trim_end()).unwrap(),
            Request::Ping
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn call_errors_when_daemon_closes_without_replying() {
        let path = temp_sock();
        let listener = bind_private(&path);
        let server = tokio::spawn(fake_daemon(listener, None));

        let err = Daemon::new(path.clone())
            .call(&Request::Ping)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("without replying"), "{err}");
        let _ = server.await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn call_errors_when_daemon_absent() {
        // Nothing is bound at this path.
        let err = Daemon::new(temp_sock())
            .call(&Request::Ping)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("could not reach copycatd"),
            "{err}"
        );
    }
}
