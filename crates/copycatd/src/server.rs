//! Local IPC server: newline-delimited JSON [`Request`]/[`Response`] over the
//! local channel. One line in, one line out; a connection may carry many requests.
//!
//! This is the single door into the daemon — `copycat-cli`, `copycat-mcp`, and
//! future companions all speak this. The dispatch is deliberately tiny: parse,
//! call a `State` method, serialise the reply.
//!
//! *What* the channel is (unix socket vs Windows named pipe) and how it is locked
//! to the current user lives in [`crate::ipc`]. Everything below is transport-
//! agnostic: `handle_conn` is generic over any `AsyncRead + AsyncWrite`.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use copycat_core::entry::{Content, Entry};
use copycat_core::proto::{DaemonStatus, Request, Response};
use copycat_core::transform::Transform;

use crate::ipc;
use crate::state::State;

/// Hard cap on a single request line. The clipboard genuinely holds large blobs,
/// but a multi-megabyte *control* line is either a bug or a local
/// memory-exhaustion attempt — reject it rather than buffering unboundedly.
const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

/// Hard cap on how long a single blocking `Wait` may park (5 minutes). A caller
/// asking for longer — or for no timeout — is clamped to this, so a long-poll can
/// never pin a connection/task/FD indefinitely.
const MAX_WAIT_MS: u64 = 300_000;

/// Bind the local channel and serve until cancelled. Binding is where access
/// control is established — see [`crate::ipc`], which locks the channel to the
/// current user on both platforms (the channel carries the clipboard in
/// plaintext, so the ability to open it IS the trust boundary).
pub async fn serve(path: PathBuf, state: Arc<State>) -> anyhow::Result<()> {
    let mut listener = ipc::Listener::bind(&path).await?;
    tracing::info!("listening on {}", listener.describe());

    loop {
        let stream = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, state).await {
                tracing::debug!("connection ended: {e}");
            }
        });
    }
}

/// Serve one connection. Generic over the stream so the unix socket and the
/// Windows named pipe share this — and so the tests can drive it over anything.
async fn handle_conn<S>(stream: S, state: Arc<State>) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Read one line, but never buffer more than the cap. `take` bounds the
        // bytes this read can pull, so an endless line can't exhaust memory.
        let n = {
            let mut limited = (&mut reader).take(MAX_REQUEST_BYTES + 1);
            limited.read_until(b'\n', &mut buf).await?
        };
        if n == 0 {
            break; // clean EOF
        }
        // Hit the cap without a terminating newline: the line is oversized. We
        // can't reliably resync mid-stream, so report and drop the connection.
        if buf.len() as u64 > MAX_REQUEST_BYTES {
            let resp = Response::Error {
                message: format!("request line exceeds {MAX_REQUEST_BYTES}-byte limit"),
            };
            write_response(&mut write_half, &resp).await?;
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(line) {
            Ok(req) => {
                let op = op_name(&req);
                let t = std::time::Instant::now();
                // `req` is consumed below; clone it for the log only when
                // something is actually recording.
                let (log, level) = state.usage();
                let req_for_log = level.records().then(|| req.clone());
                // `Wait` is the one op that blocks; everything else stays on the
                // synchronous, total `dispatch`.
                let resp = match req {
                    Request::Wait {
                        channel,
                        timeout_ms,
                        after,
                        consume,
                    } => handle_wait(&state, channel, timeout_ms, after, consume).await,
                    other => dispatch(other, &state),
                };
                let us = t.elapsed().as_micros() as u64;
                tracing::debug!(op, elapsed_us = us, "handled");
                // `record` never blocks — the request path must not wait on a disk.
                if let Some(req) = &req_for_log {
                    let now = copycat_core::usage::rfc3339_utc(State::now_millis());
                    if let Some(rec) = crate::usage::record_for(op, us, now, req, &resp, level) {
                        log.record(rec);
                    }
                }
                resp
            }
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        write_response(&mut write_half, &response).await?;
    }
    Ok(())
}

/// Serialise a response as one newline-delimited JSON line and flush it.
async fn write_response(
    write_half: &mut (impl AsyncWriteExt + Unpin),
    response: &Response,
) -> anyhow::Result<()> {
    let mut out = serde_json::to_string(response)?;
    out.push('\n');
    write_half.write_all(out.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Stable short name for a request, for latency logs.
fn op_name(req: &Request) -> &'static str {
    match req {
        Request::Ping => "ping",
        Request::Copy { .. } => "copy",
        Request::Current => "current",
        Request::Read { .. } => "read",
        Request::Recall { .. } => "recall",
        Request::Wait { .. } => "wait",
        Request::Pin { .. } => "pin",
        Request::Promote { .. } => "promote",
        Request::Delete { .. } => "delete",
        Request::Clear { .. } => "clear",
        Request::SetPrivacy { .. } => "set_privacy",
        Request::Status => "status",
    }
}

/// Blocking hand-off: park until `channel` has an entry strictly after the
/// cursor, then return it as `Handoff`, or `WaitTimeout` at the deadline.
/// Register-before-check closes the lost-wakeup race; no lock is held across an
/// `.await`; two `consume` takers race on the store `Mutex`, so exactly one wins.
async fn handle_wait(
    state: &Arc<State>,
    channel: Option<String>,
    timeout_ms: Option<u64>,
    after: Option<String>,
    consume: bool,
) -> Response {
    let chan = channel.as_deref();
    // Cursor selection differs by lane (all test-enforced): a named lane
    // drains-or-blocks on the oldest pending entry, so copy-then-wait works in
    // either order; the default lane is seeded from history, so waiting there
    // means "block for the user's NEXT copy" (drain-or-block only when it is
    // genuinely empty). The cursor is `max_id`, not `current().id`, because
    // `promote` puts a smaller id at the back, which would re-deliver.
    let is_default = matches!(chan, None | Some(""));
    let cursor = match after {
        Some(a) => Some(a),
        None if is_default => state.max_id(chan),
        None => None,
    };
    let notify = state.notify_for(chan);
    let dur = std::time::Duration::from_millis(timeout_ms.unwrap_or(MAX_WAIT_MS).min(MAX_WAIT_MS));
    let sleep = tokio::time::sleep(dur);
    tokio::pin!(sleep);
    loop {
        // Register this waiter BEFORE checking the lane (lost-wakeup guard).
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(entry) = state.take_after(chan, cursor.as_deref(), consume) {
            return Response::Handoff { entry };
        }
        tokio::select! {
            _ = &mut notified => continue,
            _ = &mut sleep => return Response::WaitTimeout,
        }
    }
}

fn error(e: anyhow::Error) -> Response {
    Response::Error {
        message: e.to_string(),
    }
}

/// Map one request onto the store. Kept synchronous and total — every arm
/// returns a `Response`, never panics on user input.
fn dispatch(req: Request, state: &State) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Copy {
            text,
            channel,
            author,
            session_id,
        } => Response::Copied {
            id: state.record_copy_with_provenance(
                &text,
                channel.as_deref(),
                author.as_deref(),
                session_id.as_deref(),
            ),
        },
        Request::Current => Response::Current {
            entry: state.current(None),
        },
        Request::Read {
            index,
            channel,
            transform,
        } => apply_transform(state.get(index, channel.as_deref()), transform)
            .map(|entry| Response::Current { entry })
            .unwrap_or_else(error),
        Request::Recall { query } => state
            .recall(&query)
            .map(|entries| Response::Entries { entries })
            .unwrap_or_else(error),
        // `Wait` is handled in the async connection loop, never here.
        Request::Wait { .. } => Response::Error {
            message: "internal: Wait must be handled asynchronously".into(),
        },
        Request::Pin { selector, pinned } => state
            .pin(&selector, pinned)
            .map(|id| Response::Pinned { id, pinned })
            .unwrap_or_else(error),
        Request::Promote { selector } => state
            .promote(&selector)
            .map(|entry| Response::Promoted { entry })
            .unwrap_or_else(error),
        Request::Delete { selector, channel } => state
            .delete(&selector, channel.as_deref())
            .map(|id| Response::Deleted { id })
            .unwrap_or_else(error),
        Request::Clear {
            include_pinned,
            all_channels,
            channel,
        } => Response::Cleared {
            removed: state.clear(include_pinned, all_channels, channel.as_deref()),
        },
        Request::SetPrivacy { mode } => Response::Privacy {
            mode: state.set_privacy(mode),
        },
        Request::Status => Response::Status(DaemonStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            clipboard: state.backend().clone(),
            entries: state.len(),
            device: state.device().to_string(),
            insecure: state.insecure(),
            privacy: state.privacy(),
        }),
    }
}

/// Apply an optional edit-on-paste [`Transform`] to a fetched entry on the way
/// out. The transform operates on the plaintext the daemon already returns; it
/// does NOT move the trust boundary — front-ends still mask Secret/PII AFTER this
/// (see `copycat-mcp`'s `read_view`). Only `Text` content is transformable;
/// non-text entries (and `transform == None`) pass through unchanged. A failed
/// transform (e.g. base64-decoding non-base64) surfaces as a daemon `Error`.
fn apply_transform(
    entry: Option<Entry>,
    transform: Option<Transform>,
) -> anyhow::Result<Option<Entry>> {
    // No transform requested (or no entry to transform): pass through unchanged.
    let Some(t) = transform else {
        return Ok(entry);
    };
    let Some(mut e) = entry else {
        return Ok(None);
    };
    // Compute the rewritten text first, so the immutable borrow of the content
    // ends before we reassign it (Entry can't be moved out of — it impls Drop).
    let new_text = match &e.content {
        Content::Text(text) => Some(t.apply(text)?),
        _ => None, // non-text entries are left untouched
    };
    if let Some(out) = new_text {
        e.content = Content::Text(out);
    }
    Ok(Some(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copycat_core::proto::ClipboardBackendInfo;

    fn state() -> Arc<State> {
        Arc::new(State::new(
            16,
            "test".into(),
            ClipboardBackendInfo::Headless,
            None,
        ))
    }

    /// A private address for one test, in the shape this platform's transport
    /// wants: a socket under a fresh temp dir on unix, a unique pipe name on
    /// Windows. `tag` keeps concurrently-running tests off each other.
    ///
    /// These e2e tests are deliberately NOT unix-gated: the Windows named-pipe
    /// listener is the least-exercised code in the daemon, and running the same
    /// end-to-end suite against it in CI is the only thing that keeps it honest.
    fn test_addr(tag: &str) -> std::path::PathBuf {
        #[cfg(unix)]
        {
            std::env::temp_dir()
                .join(format!("copycat-{tag}-{}", std::process::id()))
                .join("copycatd.sock")
        }
        #[cfg(windows)]
        {
            std::path::PathBuf::from(format!(
                r"\\.\pipe\copycatd-test-{tag}-{}",
                std::process::id()
            ))
        }
    }

    /// Connect to a listening test daemon, whatever the transport is. On Windows
    /// this MUST retry `ERROR_PIPE_BUSY` (231) like every production client — a
    /// pipe server has a real all-instances-busy window. Duplicated here because
    /// core has no tokio; keep 231 / ~2s / 20ms in step with
    /// `copycat_core::ipc::connect_sync` and copycat-mcp's `proxy::connect`.
    async fn connect_stream(
        path: &std::path::Path,
    ) -> std::io::Result<impl tokio::io::AsyncRead + tokio::io::AsyncWrite> {
        #[cfg(unix)]
        {
            tokio::net::UnixStream::connect(path).await
        }
        #[cfg(windows)]
        {
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
    }

    /// Wait for `serve` to be accepting. Polling for the socket FILE only works on
    /// unix; trying to connect works everywhere and is what a client does anyway.
    async fn await_listening(path: &std::path::Path) {
        for _ in 0..200 {
            if connect_stream(path).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("server did not start listening on {}", path.display());
    }

    /// One request over a fresh connection to a real listening daemon; returns
    /// the parsed response. Exercises the full path: transport -> framed JSON ->
    /// serve/handle_conn/dispatch -> State -> back.
    async fn call(path: &std::path::Path, req: &Request) -> Response {
        let stream = connect_stream(path).await.expect("connect");
        let (r, mut w) = tokio::io::split(stream);
        let mut line = serde_json::to_string(req).unwrap();
        line.push('\n');
        w.write_all(line.as_bytes()).await.unwrap();
        w.flush().await.unwrap();
        let mut lines = BufReader::new(r).lines();
        let resp = lines.next_line().await.unwrap().expect("response line");
        serde_json::from_str(&resp).unwrap()
    }

    /// True end-to-end over the real Unix socket: bind the actual server, then
    /// drive copy / read-by-index / recall / pin-by-index / status / bad-input
    /// as a client — nothing mocked but the (headless) clipboard backend.
    #[tokio::test]
    async fn socket_ipc_end_to_end() {
        use copycat_core::proto::Selector;
        use copycat_core::store::Recall as RecallQuery;

        // Bind inside a private per-test dir the server will create+lock 0700
        // (real usage nests the socket the same way — see proto::socket_path).
        let path = test_addr("e2e");
        #[cfg(unix)]
        let dir = path.parent().unwrap().to_path_buf();
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(&dir);
        let st = state();
        let server = tokio::spawn(serve(path.clone(), Arc::clone(&st)));

        await_listening(&path).await;

        // The socket is owner-only and its parent dir is 0700 — this is what
        // keeps other local users off the (plaintext) clipboard. The Windows
        // equivalent is the pipe's DACL, which is set at create time and cannot
        // be read back this way; `ipc::SecurityDescriptor` is where it lives.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                sock_mode, 0o600,
                "socket must be chmod 0600, was {sock_mode:o}"
            );
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700, "socket dir must be 0700, was {dir_mode:o}");
        }

        assert!(matches!(call(&path, &Request::Ping).await, Response::Pong));

        // copy two values through the real socket
        let id1 = match call(
            &path,
            &Request::Copy {
                text: "e2e one".into(),
                channel: None,
                author: None,
                session_id: None,
            },
        )
        .await
        {
            Response::Copied { id } => id,
            other => panic!("expected Copied, got {other:?}"),
        };
        let _ = call(
            &path,
            &Request::Copy {
                text: "e2e two".into(),
                channel: None,
                author: None,
                session_id: None,
            },
        )
        .await;

        // read by recency index over the socket
        match call(
            &path,
            &Request::Read {
                index: 0,
                channel: None,
                transform: None,
            },
        )
        .await
        {
            Response::Current { entry: Some(e) } => assert_eq!(e.as_text(), Some("e2e two")),
            other => panic!("index 0 -> {other:?}"),
        }
        match call(
            &path,
            &Request::Read {
                index: 1,
                channel: None,
                transform: None,
            },
        )
        .await
        {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.as_text(), Some("e2e one"));
                assert_eq!(e.id, id1);
            }
            other => panic!("index 1 -> {other:?}"),
        }

        // recall newest-first
        // channel-scoped copy stays out of the default lane, and a wait on that
        // lane over the real socket returns it (consume).
        let _ = call(
            &path,
            &Request::Copy {
                text: "for build".into(),
                channel: Some("build".into()),
                author: None,
                session_id: None,
            },
        )
        .await;
        match call(
            &path,
            &Request::Wait {
                channel: Some("build".into()),
                timeout_ms: Some(1000),
                after: None,
                consume: true,
            },
        )
        .await
        {
            Response::Handoff { entry } => assert_eq!(entry.as_text(), Some("for build")),
            other => panic!("wait -> {other:?}"),
        }
        // default lane recall is unaffected by the scoped copy.
        match call(
            &path,
            &Request::Recall {
                query: RecallQuery {
                    limit: 5,
                    ..Default::default()
                },
            },
        )
        .await
        {
            Response::Entries { entries } => {
                let texts: Vec<_> = entries.iter().filter_map(|e| e.as_text()).collect();
                assert_eq!(texts, ["e2e two", "e2e one"]);
            }
            other => panic!("recall -> {other:?}"),
        }

        // pin by index 1 resolves to id1 under the daemon's lock
        match call(
            &path,
            &Request::Pin {
                selector: Selector::Index(1),
                pinned: true,
            },
        )
        .await
        {
            Response::Pinned { id, pinned } => {
                assert_eq!(id, id1);
                assert!(pinned);
            }
            other => panic!("pin -> {other:?}"),
        }

        // promote index 1 ("e2e one") over the socket: it becomes the current entry
        match call(
            &path,
            &Request::Promote {
                selector: Selector::Index(1),
            },
        )
        .await
        {
            Response::Promoted { entry } => {
                assert_eq!(entry.id, id1);
                assert_eq!(entry.as_text(), Some("e2e one"));
            }
            other => panic!("promote -> {other:?}"),
        }
        // ...and a subsequent read of index 0 now yields it, with no duplicate added
        match call(
            &path,
            &Request::Read {
                index: 0,
                channel: None,
                transform: None,
            },
        )
        .await
        {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.as_text(), Some("e2e one"));
                assert_eq!(e.id, id1, "promote preserves the id");
            }
            other => panic!("index 0 after promote -> {other:?}"),
        }
        // promoting an unknown id is an error response, not a crash
        match call(
            &path,
            &Request::Promote {
                selector: Selector::Id("nope".into()),
            },
        )
        .await
        {
            Response::Error { .. } => {}
            other => panic!("promote unknown -> {other:?}"),
        }

        // status reflects the two entries (promote moved one, it did not add one)
        match call(&path, &Request::Status).await {
            Response::Status(s) => assert_eq!(s.entries, 2),
            other => panic!("status -> {other:?}"),
        }

        // malformed input over the channel yields an Error response, not a crash
        {
            let stream = connect_stream(&path).await.unwrap();
            let (r, mut w) = tokio::io::split(stream);
            w.write_all(b"not json\n").await.unwrap();
            w.flush().await.unwrap();
            let mut lines = BufReader::new(r).lines();
            let resp: Response =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert!(matches!(resp, Response::Error { .. }));
        }

        server.abort();
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single line larger than the cap is rejected with an Error, not buffered
    /// unboundedly (local memory-exhaustion guard).
    #[tokio::test]
    async fn oversized_request_is_rejected() {
        let path = test_addr("big");
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let server = tokio::spawn(serve(path.clone(), state()));
        await_listening(&path).await;

        let stream = connect_stream(&path).await.unwrap();
        let (r, mut w) = tokio::io::split(stream);
        // Oversized payload with no newline: the server must stop at the cap.
        let blob = vec![b'x'; (MAX_REQUEST_BYTES + 16) as usize];
        w.write_all(&blob).await.unwrap();
        w.flush().await.unwrap();
        let mut lines = BufReader::new(r).lines();
        let resp: Response =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        match resp {
            Response::Error { message } => assert!(message.contains("limit"), "{message}"),
            other => panic!("expected size-limit Error, got {other:?}"),
        }

        server.abort();
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn dispatch_copy_then_current_roundtrips() {
        let s = state();
        let id = match dispatch(
            Request::Copy {
                text: "hi".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        ) {
            Response::Copied { id } => id,
            other => panic!("expected Copied, got {other:?}"),
        };
        match dispatch(Request::Current, &s) {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.id, id);
                assert_eq!(e.as_text(), Some("hi"));
            }
            other => panic!("expected Current(Some), got {other:?}"),
        }
    }

    #[test]
    fn dispatch_ping_is_pong() {
        assert!(matches!(dispatch(Request::Ping, &state()), Response::Pong));
    }

    #[test]
    fn dispatch_pin_unknown_id_is_error_not_panic() {
        let sel = copycat_core::proto::Selector::Id("nope".into());
        match dispatch(
            Request::Pin {
                selector: sel,
                pinned: true,
            },
            &state(),
        ) {
            Response::Error { .. } => {}
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_read_by_index() {
        let s = state();
        dispatch(
            Request::Copy {
                text: "older".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        dispatch(
            Request::Copy {
                text: "newer".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        match dispatch(
            Request::Read {
                index: 1,
                channel: None,
                transform: None,
            },
            &s,
        ) {
            Response::Current { entry: Some(e) } => assert_eq!(e.as_text(), Some("older")),
            other => panic!("expected older at index 1, got {other:?}"),
        }
        // out-of-range index yields no entry, not an error
        assert!(matches!(
            dispatch(
                Request::Read {
                    index: 9,
                    channel: None,
                    transform: None
                },
                &s
            ),
            Response::Current { entry: None }
        ));
    }

    #[test]
    fn dispatch_read_applies_transform_to_text() {
        let s = state();
        dispatch(
            Request::Copy {
                text: r#"{"b":1,"a":2}"#.into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        // json_pretty reformats the current entry on the way out.
        match dispatch(
            Request::Read {
                index: 0,
                channel: None,
                transform: Some(Transform::JsonPretty),
            },
            &s,
        ) {
            Response::Current { entry: Some(e) } => {
                let t = e.as_text().unwrap();
                assert!(t.contains('\n'), "pretty output is multi-line: {t}");
            }
            other => panic!("expected transformed entry, got {other:?}"),
        }
        // The STORED entry is untouched — a plain read still returns the original.
        match dispatch(
            Request::Read {
                index: 0,
                channel: None,
                transform: None,
            },
            &s,
        ) {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.as_text(), Some(r#"{"b":1,"a":2}"#));
            }
            other => panic!("expected untouched entry, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_read_base64_encodes() {
        let s = state();
        dispatch(
            Request::Copy {
                text: "hello world".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        match dispatch(
            Request::Read {
                index: 0,
                channel: None,
                transform: Some(Transform::Base64Encode),
            },
            &s,
        ) {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.as_text(), Some("aGVsbG8gd29ybGQ="));
            }
            other => panic!("expected base64 entry, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_read_transform_failure_is_error_not_panic() {
        let s = state();
        dispatch(
            Request::Copy {
                text: "not base64 !!!".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        match dispatch(
            Request::Read {
                index: 0,
                channel: None,
                transform: Some(Transform::Base64Decode),
            },
            &s,
        ) {
            Response::Error { .. } => {}
            other => panic!("expected Error on bad transform input, got {other:?}"),
        }
    }

    #[test]
    fn apply_transform_passthrough_for_none_and_missing() {
        // No transform requested: entry returned verbatim.
        let e = Entry::from_text("1", "raw", 0, copycat_core::entry::Source::simple("t"));
        let out = apply_transform(Some(e), None).unwrap().unwrap();
        assert_eq!(out.as_text(), Some("raw"));
        // No entry: still Ok(None) even with a transform.
        assert!(apply_transform(None, Some(Transform::Upper))
            .unwrap()
            .is_none());
    }

    // The wait tests below run on a PAUSED clock: the runtime only advances
    // when every task is parked, so waiter registration is deterministic and
    // timeouts cost microseconds. Do NOT extend `start_paused` to the e2e
    // socket tests above — it would burn their connect-retry budget.

    #[tokio::test(start_paused = true)]
    async fn wait_wakes_on_post() {
        let s = state();
        let waiter = {
            let s = Arc::clone(&s);
            tokio::spawn(
                async move { handle_wait(&s, Some("a".into()), Some(2000), None, false).await },
            )
        };
        // Give the waiter a moment to register, then post to lane `a`.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dispatch(
            Request::Copy {
                text: "hi A".into(),
                channel: Some("a".into()),
                author: None,
                session_id: None,
            },
            &s,
        );
        match waiter.await.unwrap() {
            Response::Handoff { entry } => assert_eq!(entry.as_text(), Some("hi A")),
            other => panic!("expected Handoff, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out_when_empty() {
        let s = state();
        let r = handle_wait(&s, Some("idle".into()), Some(50), None, false).await;
        assert!(matches!(r, Response::WaitTimeout), "got {r:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn default_lane_wait_blocks_for_next_copy_not_history() {
        // The default (OS) lane is seeded with history, so an unscoped wait must
        // block for the user's NEXT copy — not hand back an existing entry.
        let s = state();
        dispatch(
            Request::Copy {
                text: "old value".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        let waiter = {
            let s = Arc::clone(&s);
            tokio::spawn(async move { handle_wait(&s, None, Some(2000), None, false).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dispatch(
            Request::Copy {
                text: "new value".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        match waiter.await.unwrap() {
            Response::Handoff { entry } => assert_eq!(
                entry.as_text(),
                Some("new value"),
                "must deliver the next copy, not the seeded history entry"
            ),
            other => panic!("expected the next copy, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn default_lane_wait_on_empty_returns_first_post() {
        // When the default lane is genuinely empty, fall through to drain-or-block
        // so the first arriving copy is delivered (no head to block past).
        let s = state();
        let waiter = {
            let s = Arc::clone(&s);
            tokio::spawn(async move { handle_wait(&s, None, Some(2000), None, false).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dispatch(
            Request::Copy {
                text: "first".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        match waiter.await.unwrap() {
            Response::Handoff { entry } => assert_eq!(entry.as_text(), Some("first")),
            other => panic!("expected first post, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_consume_delivers_to_exactly_one_of_two() {
        let s = state();
        let mk = || {
            let s = Arc::clone(&s);
            tokio::spawn(
                async move { handle_wait(&s, Some("a".into()), Some(2000), None, true).await },
            )
        };
        let w1 = mk();
        let w2 = mk();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dispatch(
            Request::Copy {
                text: "once".into(),
                channel: Some("a".into()),
                author: None,
                session_id: None,
            },
            &s,
        );
        let r1 = w1.await.unwrap();
        let r2 = w2.await.unwrap();
        let handoffs = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Response::Handoff { .. }))
            .count();
        let timeouts = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Response::WaitTimeout))
            .count();
        assert_eq!(
            handoffs, 1,
            "consume must hand off to exactly one: {r1:?} / {r2:?}"
        );
        assert_eq!(timeouts, 1, "the loser times out: {r1:?} / {r2:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_peek_fans_out_to_both() {
        let s = state();
        let mk = || {
            let s = Arc::clone(&s);
            tokio::spawn(
                async move { handle_wait(&s, Some("a".into()), Some(2000), None, false).await },
            )
        };
        let w1 = mk();
        let w2 = mk();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dispatch(
            Request::Copy {
                text: "broadcast".into(),
                channel: Some("a".into()),
                author: None,
                session_id: None,
            },
            &s,
        );
        for r in [w1.await.unwrap(), w2.await.unwrap()] {
            match r {
                Response::Handoff { entry } => assert_eq!(entry.as_text(), Some("broadcast")),
                other => panic!("peek must fan out to both, got {other:?}"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_after_existing_id_returns_immediately() {
        let s = state();
        // Two posts on lane `a`; wait with cursor = first id returns the second
        // without blocking.
        let id1 = match dispatch(
            Request::Copy {
                text: "a1".into(),
                channel: Some("a".into()),
                author: None,
                session_id: None,
            },
            &s,
        ) {
            Response::Copied { id } => id,
            other => panic!("{other:?}"),
        };
        dispatch(
            Request::Copy {
                text: "a2".into(),
                channel: Some("a".into()),
                author: None,
                session_id: None,
            },
            &s,
        );
        let r = handle_wait(&s, Some("a".into()), Some(50), Some(id1), false).await;
        match r {
            Response::Handoff { entry } => assert_eq!(entry.as_text(), Some("a2")),
            other => panic!("expected immediate Handoff, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cross_lane_post_does_not_wake_waiter() {
        let s = state();
        let waiter = {
            let s = Arc::clone(&s);
            tokio::spawn(
                async move { handle_wait(&s, Some("a".into()), Some(120), None, false).await },
            )
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Post to a DIFFERENT lane — must not satisfy the waiter on `a`.
        dispatch(
            Request::Copy {
                text: "for B".into(),
                channel: Some("b".into()),
                author: None,
                session_id: None,
            },
            &s,
        );
        match waiter.await.unwrap() {
            Response::WaitTimeout => {}
            other => panic!("cross-lane post must not wake; got {other:?}"),
        }
    }

    #[test]
    fn dispatch_status_reports_backend_and_count() {
        let s = state();
        dispatch(
            Request::Copy {
                text: "a".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        match dispatch(Request::Status, &s) {
            Response::Status(st) => {
                assert_eq!(st.clipboard, ClipboardBackendInfo::Headless);
                assert_eq!(st.entries, 1);
                assert_eq!(st.device, "test");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn set_privacy_takes_effect_and_is_reported_by_status() {
        use copycat_core::entry::Sensitivity;
        use copycat_core::privacy::Mode as PrivacyMode;

        let s = state();
        dispatch(
            Request::Copy {
                text: "dean@example.com".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            &s,
        );
        // The UI's toggle: one request, and the whole daemon changes posture.
        match dispatch(
            Request::SetPrivacy {
                mode: PrivacyMode::Strict,
            },
            &s,
        ) {
            Response::Privacy { mode } => assert_eq!(mode, PrivacyMode::Strict),
            other => panic!("expected Privacy, got {other:?}"),
        }
        match dispatch(Request::Current, &s) {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.sensitivity, Sensitivity::Pii)
            }
            other => panic!("expected Current, got {other:?}"),
        }
        match dispatch(Request::Status, &s) {
            Response::Status(st) => assert_eq!(st.privacy, PrivacyMode::Strict),
            other => panic!("expected Status, got {other:?}"),
        }

        dispatch(
            Request::SetPrivacy {
                mode: PrivacyMode::Off,
            },
            &s,
        );
        match dispatch(Request::Current, &s) {
            Response::Current { entry: Some(e) } => {
                assert_eq!(e.sensitivity, Sensitivity::Normal);
                // Never dropped, never rewritten — only the marking changed.
                assert_eq!(e.as_text(), Some("dean@example.com"));
            }
            other => panic!("expected Current, got {other:?}"),
        }
    }
}
