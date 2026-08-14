//! End-to-end smoke test of the MCP tool surface (the Rust port of the former
//! `evals/mcp_smoke.py`).
//!
//! Starts a real `copycatd` (headless, on a private throwaway address) and a real
//! `copycat-mcp --stdio`, then drives the server as an MCP client would: the
//! expected tools are advertised, the intent-model instructions are present, a
//! copy round-trips through the shared daemon, and secrets are masked on read and
//! in history. It tests the *plumbing*; `evals/behavioral/run.py` tests whether a
//! model picks the right tool, which needs an API key and so stays in Python.
//!
//! This lives in `tests/` rather than a `#[cfg(test)]` module because it needs
//! `CARGO_BIN_EXE_*`, which Cargo only sets for integration tests — and spawning
//! the actual built binaries is the whole point. It is the only test covering
//! argument parsing, the stdio JSON-RPC framing and socket-path resolution the
//! way a real Claude Code session hits them; the in-process `copycatd::server`
//! e2e starts one layer below all three.
//!
//! It needs the `copycatd` binary alongside this crate's own. `cargo test
//! --workspace` (and both CI jobs) build both; a bare `cargo test -p copycat-mcp`
//! does not — run `cargo build --workspace` first.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The tools the surface must advertise. A tool going missing costs a Claude
/// session that capability silently — no unit test in the server would notice,
/// because the loss would be in how the surface is *assembled*, not in a handler.
const EXPECTED_TOOLS: &[&str] = &[
    "clipboard_copy",
    "clipboard_copy_many",
    "clipboard_read_current",
    "clipboard_wait",
    "clipboard_history",
    "clipboard_pin",
    "clipboard_promote",
    "clipboard_promote_many",
    "clipboard_delete",
    "clipboard_clear",
    "clipboard_status",
];

/// Resolve a workspace binary sitting next to this crate's own built one.
/// `CARGO_BIN_EXE_copycat-mcp` gives us the target directory; `copycatd` belongs
/// to a different package so Cargo hands us no variable for it, but every
/// workspace binary lands in the same directory.
fn bin(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_copycat-mcp"))
        .parent()
        .expect("a built binary always has a parent dir")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "missing binary {}: this test drives the real binaries, so build the whole \
         workspace first (`cargo test --workspace`, or `cargo build --workspace` \
         before `cargo test -p copycat-mcp`)",
        path.display()
    );
    path
}

/// A private address for this test run, in the shape this platform's transport
/// wants. On unix we deliberately hand the daemon a path that does NOT exist yet:
/// it creates and locks the parent to `0700` itself, and rightly refuses a
/// world-readable dir like bare `/tmp` (which is what `mkdtemp` was doing for the
/// Python version).
fn smoke_addr() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("copycat-smoke-{}", std::process::id()))
            .join("s.sock")
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\copycatd-smoke-{}", std::process::id()))
    }
}

/// The environment both children share: the private address, and silence — MCP
/// stdio is a protocol channel, so anything chatty is a hazard, not just noise.
fn child_env(addr: &Path) -> [(&'static str, std::ffi::OsString); 4] {
    [
        ("COPYCAT_SOCK", addr.as_os_str().to_os_string()),
        ("COPYCAT_DEVICE", "eval".into()),
        ("COPYCATD_LOG", "off".into()),
        ("COPYCAT_MCP_LOG", "off".into()),
    ]
}

/// A real `copycatd`, torn down (with its socket dir) when the test ends.
struct Daemon {
    child: Child,
    addr: PathBuf,
}

impl Daemon {
    fn start(addr: &Path) -> Daemon {
        #[cfg(unix)]
        if let Some(dir) = addr.parent() {
            // A leftover dir from a crashed run would be ours and 0700, so the
            // daemon would accept it — but a stale socket in it is one more thing
            // to reason about. Start from nothing.
            let _ = std::fs::remove_dir_all(dir);
        }
        let child = Command::new(bin("copycatd"))
            .envs(child_env(addr))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn copycatd");
        let mut daemon = Daemon {
            child,
            addr: addr.to_path_buf(),
        };
        daemon.await_listening();
        daemon
    }

    /// Wait until the daemon accepts connections. Polling for the socket *file*
    /// is unix-only; connecting is what a client does anyway and works on both
    /// transports.
    fn await_listening(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if copycat_core::ipc::connect_sync(&self.addr).is_ok() {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "copycatd exited before listening on {} ({status})",
                    self.addr.display()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("copycatd never listened on {}", self.addr.display());
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        if let Some(dir) = self.addr.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Minimal MCP stdio client: newline-delimited JSON-RPC, one line per message.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(addr: &Path) -> Mcp {
        let mut child = Command::new(bin("copycat-mcp"))
            .arg("--stdio")
            .envs(child_env(addr))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn copycat-mcp");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Mcp {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").expect("write to copycat-mcp");
        self.stdin.flush().expect("flush copycat-mcp");
    }

    fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .expect("read from copycat-mcp");
        assert!(n > 0, "copycat-mcp closed stdout without replying");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("not a JSON-RPC line ({e}): {line:?}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.recv()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Call a tool and return the text of its first content block — exactly the
    /// bytes the model would read, which is what every safety assertion here is
    /// really about.
    fn call_tool(&mut self, name: &str, arguments: Value) -> String {
        let resp = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name} returned no text content: {resp}"))
            .to_string()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One sequential story, because the interesting assertions are about accumulated
/// history (index recall, masking of an *earlier* entry, promote). Splitting it
/// into separate `#[test]`s would mean a daemon per test and re-staging the same
/// state in each.
#[test]
fn mcp_tool_surface_end_to_end() {
    let addr = smoke_addr();
    // Declaration order matters: `mcp` drops (and dies) before the daemon it
    // proxies to.
    let _daemon = Daemon::start(&addr);
    let mut mcp = Mcp::start(&addr);

    // 1. Handshake: server identity, plus the intent-model instructions that are
    //    the only thing telling the model *when* to reach for the clipboard.
    let init = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"},
        }),
    );
    mcp.notify("notifications/initialized", json!({}));

    assert_eq!(
        init["result"]["serverInfo"]["name"], "copycat",
        "server identity: {init}"
    );
    let instructions = init["result"]["instructions"].as_str().unwrap_or_default();
    for key in ["WHEN TO COPY", "WHEN TO READ", "SAFETY"] {
        assert!(
            instructions.contains(key),
            "intent-model instructions missing {key:?}"
        );
    }

    // 2. The expected tool surface is advertised.
    let listed = mcp.request("tools/list", json!({}));
    let advertised: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list returned no tool array: {listed}"))
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for want in EXPECTED_TOOLS {
        assert!(
            advertised.contains(want),
            "tool {want} not advertised; got {advertised:?}"
        );
    }

    // 3. Copy → read round-trips through the shared daemon.
    mcp.call_tool("clipboard_copy", json!({"text": "generated-username-42"}));
    let read = mcp.call_tool("clipboard_read_current", json!({}));
    assert!(
        read.contains("generated-username-42"),
        "copy did not round-trip to read: {read}"
    );

    // 3b. Index recall: after a second copy, index 1 is the prior entry.
    mcp.call_tool("clipboard_copy", json!({"text": "second-value"}));
    let read1 = mcp.call_tool("clipboard_read_current", json!({"index": 1}));
    assert!(
        read1.contains("generated-username-42"),
        "index 1 is not the prior entry: {read1}"
    );

    // 4. A secret is withheld on read and never echoed raw.
    mcp.call_tool(
        "clipboard_copy",
        json!({"text": "ghp_ABCDEFGHIJKLMNOP0123"}),
    );
    let secret_read = mcp.call_tool("clipboard_read_current", json!({}));
    assert!(
        secret_read.contains("withheld") && !secret_read.contains("ghp_"),
        "secret not withheld on read: {secret_read}"
    );

    // 5. History masks the secret's preview but still shows normal entries.
    let history = mcp.call_tool("clipboard_history", json!({"limit": 5}));
    assert!(
        history.contains("[SECRET]") && !history.contains("ghp_"),
        "secret not masked in history: {history}"
    );
    assert!(
        history.contains("generated-username-42"),
        "normal entry missing from history: {history}"
    );

    // 5b. `limit: 0` must clamp, not read as "unlimited" — the boundary where a
    //     careless default dumps the entire clipboard, secrets included, at the model.
    let history0 = mcp.call_tool("clipboard_history", json!({"limit": 0}));
    assert!(
        !history0.contains("ghp_") && history0.matches("id=").count() <= 10,
        "history limit:0 was not clamped: {history0}"
    );

    // 6. Promote makes an older entry current.
    mcp.call_tool("clipboard_copy", json!({"text": "promote-me-A"}));
    mcp.call_tool("clipboard_copy", json!({"text": "promote-me-B"})); // A is now index 1
    mcp.call_tool("clipboard_promote", json!({"index": 1}));
    let current = mcp.call_tool("clipboard_read_current", json!({}));
    assert!(
        current.contains("promote-me-A"),
        "promoted entry did not become current: {current}"
    );

    // 6a. The address the model is told to reuse must actually work when it does.
    //     This is the whole loop for short handles — mint, display, model retypes
    //     it, daemon resolves it — and it is only real end-to-end: the unit tests
    //     can prove `handle_of` and `id_matches` separately but not that the
    //     string a live daemon prints is one a live daemon accepts back.
    mcp.call_tool("clipboard_copy", json!({"text": "handle-round-trip-A"}));
    mcp.call_tool("clipboard_copy", json!({"text": "handle-round-trip-B"}));
    let listing = mcp.call_tool("clipboard_history", json!({"limit": 2}));
    // Pull the id the model would copy out of the listing, exactly as it sees it.
    let handle = listing
        .split("id=")
        .nth(2)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("no second id= in history: {listing}"))
        .to_string();
    assert!(
        handle.starts_with('c') && handle.len() <= 4,
        "history should advertise a short handle, got {handle:?}: {listing}"
    );
    assert!(
        handle.parse::<usize>().is_err(),
        "a handle that parses as an integer would be read as a recency index: {handle:?}"
    );
    let promoted = mcp.call_tool("clipboard_promote", json!({"id": handle}));
    assert!(
        !promoted.contains("error") && !promoted.contains("no entry"),
        "daemon rejected the handle it just advertised ({handle}): {promoted}"
    );
    let current = mcp.call_tool("clipboard_read_current", json!({}));
    assert!(
        current.contains("handle-round-trip-A"),
        "handle {handle} resolved to the wrong entry: {current}"
    );

    // 6b. Promoting a SECRET confirms without echoing it: the whole point is that
    //     the value reaches the OS clipboard without passing through the model.
    mcp.call_tool("clipboard_copy", json!({"text": "ghp_SECRETPROMOTE0123"})); // index 0
    let secret_promote = mcp.call_tool("clipboard_promote", json!({"index": 0}));
    assert!(
        !secret_promote.contains("ghp_"),
        "promote echoed a secret: {secret_promote}"
    );

    // 7. Channel hand-off: post to a private lane, then wait-consume it. The post
    //    lands first, so drain-or-block returns immediately (no second thread
    //    needed) and `take` consumes it exactly once.
    mcp.call_tool(
        "clipboard_copy",
        json!({"text": "handoff-payload", "channel": "smoke"}),
    );
    let waited = mcp.call_tool(
        "clipboard_wait",
        json!({"channel": "smoke", "timeout_ms": 2000, "take": true}),
    );
    assert!(
        waited.contains("handoff-payload"),
        "clipboard_wait did not deliver the channel post: {waited}"
    );
    // A named-lane post must not leak into the user's default clipboard.
    let default_lane = mcp.call_tool("clipboard_read_current", json!({}));
    assert!(
        !default_lane.contains("handoff-payload"),
        "channel post leaked onto the default lane: {default_lane}"
    );

    // 8. Batch copy and promote.
    mcp.call_tool(
        "clipboard_copy_many",
        json!({"texts": ["batch1", "batch2"]}),
    );
    let promoted_many = mcp.call_tool("clipboard_promote_many", json!({"selectors": ["1", "0"]}));
    assert!(
        promoted_many.contains("Promoted 2"),
        "clipboard_promote_many: {promoted_many}"
    );

    // 9. Delete and clear.
    let deleted = mcp.call_tool("clipboard_delete", json!({"index": 0}));
    assert!(
        deleted.contains("Deleted entry"),
        "clipboard_delete: {deleted}"
    );
    let cleared = mcp.call_tool("clipboard_clear", json!({}));
    assert!(cleared.contains("Cleared"), "clipboard_clear: {cleared}");
}
