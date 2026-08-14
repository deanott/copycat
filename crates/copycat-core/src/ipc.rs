//! The local IPC transport, and the one place that knows it differs per OS.
//!
//! Every front-end (`copycat-cli`, `copycat-ui`, `copycat-mcp`) and the daemon
//! speak newline-delimited JSON over a *local, single-user* channel. What that
//! channel IS differs:
//!
//! | | unix | windows |
//! |---|---|---|
//! | transport | `AF_UNIX` stream socket | named pipe (`\\.\pipe\…`) |
//! | address | a filesystem path | a pipe name (not a real path) |
//! | access control | `0600` file + `0700` parent dir | DACL on the pipe, set at create |
//! | stale address | a leftover socket file must be unlinked | impossible — pipes are kernel objects |
//!
//! The security property is the same on both and it is the whole trust boundary:
//! **anything that can open this channel can read the plaintext clipboard.** Only
//! the current user may connect. On unix that is filesystem modes; on Windows it
//! is a discretionary ACL, applied by the daemon in `copycatd::ipc`. Named pipes
//! because neither `std` nor `tokio` exposes async `AF_UNIX` on Windows, and the
//! daemon is async.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::proto::{Request, Response};
use crate::wire;

/// A connected client stream. Both variants implement `Read + Write + try_clone`,
/// which is the entire API the blocking front-ends use — so `client.rs` in the
/// CLI and the UI is genuinely identical on both platforms.
///
/// On Windows a named-pipe client handle *is* a file handle, so `File` is not a
/// stand-in: it is the real thing, opened with read+write access.
#[cfg(unix)]
pub type SyncStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type SyncStream = std::fs::File;

/// How long a blocking client will keep retrying a busy pipe before giving up.
/// All pipe instances busy is a transient condition (the daemon creates the next
/// instance immediately after accepting), so a short bounded retry turns a
/// spurious failure into a barely-noticeable pause. Unix has no equivalent state:
/// `connect(2)` on a listening socket queues in the backlog.
#[cfg(windows)]
const BUSY_RETRY_TOTAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Opt out of [`check_trust`] and connect to a socket anyway. An escape hatch for
/// the deliberate case (a shared socket inside a throwaway container), never for
/// a real clipboard — hence an explicit env var rather than a flag.
pub const ALLOW_INSECURE_SOCKET: &str = "COPYCAT_ALLOW_INSECURE_SOCKET";

/// Why an address was judged unsafe to connect to. Both variants mean the same
/// thing in practice: somebody other than us can reach the plaintext clipboard
/// through this socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Untrusted {
    /// Group/other bits are set, so any local user can `connect(2)` to it.
    OpenMode { mode: u32 },
    /// Owned by a different uid — not the socket our daemon bound.
    ForeignOwner { uid: u32, ours: u32 },
}

impl Untrusted {
    /// The message a front-end shows. Names the fix, because the realistic cause
    /// is an `ssh -R` forward rather than anything the user did wrong.
    pub fn explain(&self, addr: &Path) -> String {
        let addr = addr.display();
        let common = format!(
            "anything that can open it reads the plaintext clipboard. If this is a \
             forwarded socket, `chmod 600 {addr}` on this box, or set \
             {ALLOW_INSECURE_SOCKET}=1 to connect anyway."
        );
        match self {
            Untrusted::OpenMode { mode } => format!(
                "refusing to connect to {addr}: mode {mode:04o} lets other local \
                 users connect — {common}"
            ),
            Untrusted::ForeignOwner { uid, ours } => format!(
                "refusing to connect to {addr}: it is owned by uid {uid}, not you \
                 (uid {ours}) — {common}"
            ),
        }
    }
}

/// Is this address safe for *us* to connect to?
///
/// `copycatd` locks every socket it binds to `0600` inside a `0700` dir, so for a
/// locally-started daemon this check always passes. It exists for the socket
/// copycatd did **not** create: `ssh -R` has *sshd* create the socket on the
/// remote box, under sshd's umask, and none of the daemon's bind-time
/// enforcement applies to it. That socket is still the full trust boundary —
/// whoever opens it reads the clipboard of the machine at the other end of the
/// tunnel — so the check has to live on the client side too.
///
/// A missing address is *not* an error here: that is "no daemon", which
/// [`connect_sync`] reports with its own message.
///
/// Windows has no equivalent: a named pipe is a kernel object with a DACL set at
/// creation, it cannot be left lying around, and it is not what `ssh -R`
/// forwards. Always `Ok`.
pub fn check_trust(addr: &Path) -> Result<(), Untrusted> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let Ok(meta) = std::fs::symlink_metadata(addr) else {
            return Ok(());
        };
        let ours = unsafe { libc::geteuid() };
        if meta.uid() != ours {
            return Err(Untrusted::ForeignOwner {
                uid: meta.uid(),
                ours,
            });
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(Untrusted::OpenMode { mode });
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = addr;
        Ok(())
    }
}

/// [`check_trust`], but honouring [`ALLOW_INSECURE_SOCKET`] and shaped as the
/// `io::Error` the connect paths return. Every client transport funnels through
/// this so no front-end can forget it.
pub fn enforce_trust(addr: &Path) -> io::Result<()> {
    if let Err(bad) = check_trust(addr) {
        if std::env::var_os(ALLOW_INSECURE_SOCKET).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                bad.explain(addr),
            ));
        }
    }
    Ok(())
}

/// Connect to the daemon (blocking). `addr` comes from [`crate::proto::socket_path`].
pub fn connect_sync(addr: &Path) -> io::Result<SyncStream> {
    enforce_trust(addr)?;
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(addr)
    }
    #[cfg(windows)]
    {
        use std::time::{Duration, Instant};
        // ERROR_PIPE_BUSY (231): every instance is serving someone else right now.
        const ERROR_PIPE_BUSY: i32 = 231;
        let deadline = Instant::now() + BUSY_RETRY_TOTAL;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(addr)
            {
                Ok(f) => return Ok(f),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    if Instant::now() >= deadline {
                        return Err(e);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// One blocking round-trip with the daemon at `addr`: connect, write one wire
/// line, read one back. The shared client path for every blocking front-end.
///
/// Uses [`wire::connect_failure`] so a `PermissionDenied` from [`check_trust`]
/// keeps its own explanation — a trust refusal, not an absent daemon.
pub fn roundtrip(addr: &Path, req: &Request) -> anyhow::Result<Response> {
    let stream = connect_sync(addr).map_err(|e| wire::connect_failure(addr, e))?;

    let mut writer = stream.try_clone().context("clone socket")?;
    writer
        .write_all(wire::encode(req)?.as_bytes())
        .context("send request")?;
    writer.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).context("read response")?;
    if n == 0 {
        anyhow::bail!(wire::NO_REPLY);
    }
    wire::decode(&resp_line).context("parse response")
}

/// The default address for this user's daemon.
///
/// Unix: `$XDG_RUNTIME_DIR/copycat/copycatd.sock`, else a `copycat/` dir in the
/// OS temp dir. The nested private dir is what the daemon locks to `0700`.
///
/// Windows: `\\.\pipe\copycatd-<user>`. The username is part of the NAME because
/// the pipe namespace is machine-global and flat — two users on one machine (a
/// terminal server, a shared workstation) must not collide on the same object.
/// The name is not a secret and is not what protects the pipe; the DACL is. It
/// only prevents an accidental collision from turning into a hard failure.
pub fn default_addr() -> PathBuf {
    #[cfg(unix)]
    {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        dir.join("copycat").join(crate::proto::SOCKET_NAME)
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
        // Keep the name to the pipe-name charset: anything but backslash is legal,
        // but a domain login can contain characters that make the path confusing to
        // read in logs, so normalise conservatively.
        let user: String = user
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        PathBuf::from(format!(r"\\.\pipe\copycatd-{user}"))
    }
}

/// Is `addr` a Windows named pipe name rather than a filesystem path? Used by the
/// daemon to skip the unix-only staleness/permission dance, which has no meaning
/// for a kernel object that cannot be left behind.
pub fn is_pipe_name(addr: &Path) -> bool {
    addr.to_string_lossy().starts_with(r"\\.\pipe\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_addr_is_platform_shaped() {
        let a = default_addr();
        if cfg!(windows) {
            assert!(is_pipe_name(&a), "windows addr must be a pipe name: {a:?}");
        } else {
            assert!(!is_pipe_name(&a));
            assert!(a.is_absolute(), "unix addr must be absolute: {a:?}");
            assert_eq!(a.file_name().unwrap(), crate::proto::SOCKET_NAME);
        }
    }

    /// A missing address is "no daemon", not "unsafe" — `connect_sync` owns that
    /// error, and reporting it as a permission problem would be actively
    /// misleading on a box where the daemon simply isn't running.
    #[test]
    fn missing_address_is_not_a_trust_failure() {
        assert_eq!(
            check_trust(Path::new("/nonexistent/copycat/definitely-not-here.sock")),
            Ok(())
        );
    }

    #[cfg(unix)]
    #[test]
    fn mode_decides_trust() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("copycat-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        // A plain file stands in for the socket: `check_trust` only stats the
        // address, so the inode type is irrelevant and this keeps the test from
        // needing a listener.
        std::fs::write(&sock, b"").unwrap();

        // What copycatd itself binds.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(check_trust(&sock), Ok(()));

        // What an `ssh -R` forward under a default umask can leave behind.
        for mode in [0o644, 0o666, 0o755, 0o660, 0o601] {
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                check_trust(&sock),
                Err(Untrusted::OpenMode { mode }),
                "mode {mode:o} must be refused"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The refusal has to tell the user how to fix it — the cause is almost never
    /// something they did on this machine.
    #[cfg(unix)]
    #[test]
    fn refusal_names_the_remedy() {
        let msg = Untrusted::OpenMode { mode: 0o755 }.explain(Path::new("/run/x.sock"));
        assert!(msg.contains("0755"), "{msg}");
        assert!(msg.contains("chmod 600 /run/x.sock"), "{msg}");
        assert!(msg.contains(ALLOW_INSECURE_SOCKET), "{msg}");
    }

    /// The escape hatch is opt-in and env-gated, so an ordinary run cannot
    /// silently fall through to an exposed socket.
    #[cfg(unix)]
    #[test]
    fn enforce_refuses_by_default_and_yields_to_the_override() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("copycat-enforce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        std::fs::write(&sock, b"").unwrap();
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).unwrap();

        let err = enforce_trust(&sock).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        std::env::set_var(ALLOW_INSECURE_SOCKET, "1");
        assert!(enforce_trust(&sock).is_ok());
        std::env::remove_var(ALLOW_INSECURE_SOCKET);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn roundtrip_sends_one_line_and_reads_one_back() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("copycat-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sock = dir.join("t.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        // Match what copycatd binds, so check_trust passes.
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).unwrap();

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let req: Request = serde_json::from_str(line.trim_end()).unwrap();
            assert!(matches!(req, Request::Ping));
            let mut reply = serde_json::to_string(&Response::Pong).unwrap();
            reply.push('\n');
            (&stream).write_all(reply.as_bytes()).unwrap();
        });

        let resp = roundtrip(&sock, &Request::Ping).unwrap();
        assert!(matches!(resp, Response::Pong));
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_to_an_absent_daemon_reports_unreachable() {
        let e = roundtrip(
            Path::new("/nonexistent/copycat/definitely-not-here.sock"),
            &Request::Ping,
        )
        .unwrap_err();
        assert!(e.to_string().contains("could not reach copycatd"), "{e}");
    }

    #[test]
    fn pipe_names_are_recognised() {
        assert!(is_pipe_name(Path::new(r"\\.\pipe\copycatd-dean")));
        assert!(!is_pipe_name(Path::new(
            "/run/user/1000/copycat/copycatd.sock"
        )));
        assert!(!is_pipe_name(Path::new("relative.sock")));
    }
}
