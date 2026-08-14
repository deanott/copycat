//! The listening side of the local IPC channel, per platform.
//!
//! `copycat_core::ipc` owns the *client* half and the address format; this owns
//! the *server* half, because binding is where access control is established and
//! that is the security-critical part. Anything that can open this channel reads
//! the plaintext clipboard, so "only this user may connect" is the whole boundary.
//!
//! **unix** — bind an `AF_UNIX` socket, `chmod 0600`, inside a `0700` parent dir.
//!
//! **windows** — create a named pipe whose DACL grants access to exactly one SID:
//! the user this process is running as. There is no filesystem object and no
//! `chmod`; the ACL is applied atomically at create time, which is strictly better
//! than the unix bind-then-chmod sequence (that has a window, however small).
//!
//! Two Windows-specific protections worth naming:
//!
//! * `first_pipe_instance(true)` on the first instance. Without it, any process
//!   may create *another instance of the same pipe name* and race the real daemon
//!   for incoming connections — the Windows equivalent of socket squatting, and
//!   the classic named-pipe hijack. With it, a second creator gets
//!   `ERROR_ACCESS_DENIED`, which we report as "a daemon is already running".
//! * An explicit DACL. The default security descriptor for a named pipe grants
//!   more than the creating user; we replace it rather than inherit it.

use std::path::Path;

/// A connected client. Both platforms' streams are `AsyncRead + AsyncWrite`, which
/// is all `server::handle_conn` needs — it is generic over this.
#[cfg(unix)]
pub type Stream = tokio::net::UnixStream;
#[cfg(windows)]
pub type Stream = tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(unix)]
pub use unix::Listener;
#[cfg(windows)]
pub use windows::Listener;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use anyhow::Context;

    pub struct Listener {
        inner: tokio::net::UnixListener,
        path: PathBuf,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> anyhow::Result<Self> {
            ensure_private_parent(path)?;
            ensure_free(path).await?;
            let inner = tokio::net::UnixListener::bind(path)
                .with_context(|| format!("binding socket {}", path.display()))?;
            // Tighten the socket to owner-only. `connect(2)` needs write permission
            // on the socket file, so `0600` alone keeps other local users out; the
            // private parent dir is defence-in-depth for the shared-/tmp fallback.
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 0600 {}", path.display()))?;
            Ok(Listener {
                inner,
                path: path.to_path_buf(),
            })
        }

        pub async fn accept(&mut self) -> anyhow::Result<Stream> {
            let (stream, _addr) = self.inner.accept().await.context("accept")?;
            Ok(stream)
        }

        pub fn describe(&self) -> String {
            self.path.display().to_string()
        }
    }

    /// Ensure the socket's parent directory exists, is owned by us, and is only
    /// accessible by us (`0700`). This is what actually keeps other local users
    /// off the socket when it falls back to a shared dir like `/tmp` (see
    /// `proto::socket_path`, which nests the socket in a private `copycat/` dir).
    ///
    /// We only create/lock a directory we own. If the parent already exists we
    /// verify its mode rather than clobbering it — we must never chmod a shared
    /// dir like `/tmp` itself. A pre-existing parent with looser-than-`0700`
    /// perms is a hard error: better to refuse to start than to serve the
    /// clipboard from a directory other users can traverse into.
    fn ensure_private_parent(path: &Path) -> anyhow::Result<()> {
        let Some(dir) = path.parent() else {
            return Ok(());
        };
        // An empty parent (relative bare filename) or the process CWD: nothing to do.
        if dir.as_os_str().is_empty() {
            return Ok(());
        }
        match std::fs::symlink_metadata(dir) {
            Ok(meta) => {
                // A symlinked parent is suspicious (redirection); demand a real dir.
                if !meta.is_dir() {
                    anyhow::bail!("socket parent {} is not a directory", dir.display());
                }
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    anyhow::bail!(
                        "refusing to use socket dir {} with mode {mode:o}: it is accessible \
                         by other users, which would expose the clipboard. Fix with \
                         `chmod 700 {}` or set COPYCAT_SOCK to a private path.",
                        dir.display(),
                        dir.display()
                    );
                }
            }
            Err(_) => {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating socket dir {}", dir.display()))?;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("locking socket dir {} to 0700", dir.display()))?;
            }
        }
        Ok(())
    }

    /// If a socket file already exists, ping it: a live daemon means we must not
    /// steal the address; a dead socket file is stale and gets removed.
    async fn ensure_free(path: &Path) -> anyhow::Result<()> {
        // symlink_metadata so a dangling symlink still counts as "present" and a
        // symlink is never followed when we remove it below.
        if std::fs::symlink_metadata(path).is_err() {
            return Ok(());
        }
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            anyhow::bail!(
                "a copycatd already appears to be listening on {} \
                 (set COPYCAT_SOCK to run a second, isolated daemon)",
                path.display()
            );
        }
        std::fs::remove_file(path)
            .with_context(|| format!("removing stale socket {}", path.display()))?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_world_accessible_socket_dir() {
            let dir = std::env::temp_dir().join(format!("copycat-open-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("copycatd.sock");
            // A group/other-accessible parent is refused (would expose the clipboard).
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                ensure_private_parent(&sock).is_err(),
                "0755 dir must be rejected"
            );
            // A private 0700 parent is accepted.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(
                ensure_private_parent(&sock).is_ok(),
                "0700 dir must be accepted"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::io;

    use anyhow::Context;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// Owns the pipe name plus the *next, not-yet-connected* server instance.
    ///
    /// A named pipe server is not a socket: one instance serves one client. The
    /// standard pattern is to always keep one idle instance listening, hand it to
    /// the connecting client, and immediately create its replacement — otherwise
    /// there is a window in which a client sees "no such pipe" rather than
    /// queueing (unix gets this for free from the listen backlog).
    pub struct Listener {
        name: String,
        next: NamedPipeServer,
        sd: SecurityDescriptor,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> anyhow::Result<Self> {
            let name = path.to_string_lossy().to_string();
            if !copycat_core::ipc::is_pipe_name(path) {
                anyhow::bail!(
                    "COPYCAT_SOCK must be a named pipe on Windows, e.g. \
                     \\\\.\\pipe\\copycatd-me (got {name})"
                );
            }
            let sd = SecurityDescriptor::current_user_only()
                .context("building the pipe's owner-only security descriptor")?;

            // `first_pipe_instance` is the anti-squatting control: if any other
            // process already owns this pipe name we get ACCESS_DENIED here rather
            // than silently sharing the name with it and racing for clients.
            let next = unsafe {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .create_with_security_attributes_raw(&name, sd.as_attributes_ptr())
            }
            .map_err(|e| {
                if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
                    anyhow::anyhow!(
                        "a copycatd already appears to own {name} \
                         (set COPYCAT_SOCK to run a second, isolated daemon)"
                    )
                } else {
                    anyhow::Error::new(e).context(format!("creating named pipe {name}"))
                }
            })?;

            Ok(Listener { name, next, sd })
        }

        pub async fn accept(&mut self) -> anyhow::Result<Stream> {
            // Park until a client opens the idle instance, then swap in a fresh
            // one so the next client never sees the pipe as absent.
            self.next.connect().await.context("pipe connect")?;
            let replacement = self
                .create_instance()
                .context("creating next pipe instance")?;
            Ok(std::mem::replace(&mut self.next, replacement))
        }

        fn create_instance(&self) -> io::Result<NamedPipeServer> {
            // Only the FIRST instance may set first_pipe_instance; later ones must
            // not, or creation fails.
            unsafe {
                ServerOptions::new()
                    .create_with_security_attributes_raw(&self.name, self.sd.as_attributes_ptr())
            }
        }

        pub fn describe(&self) -> String {
            self.name.clone()
        }
    }

    const ERROR_ACCESS_DENIED: i32 = 5;

    /// A SECURITY_ATTRIBUTES whose DACL names exactly one trustee: the SID this
    /// process runs as. Built from SDDL so the intent is legible:
    ///
    /// ```text
    /// D:P(A;;GA;;;<sid>)
    ///  │ │ │  │     └── the current user's SID, looked up from the process token
    ///  │ │ │  └──────── GA = GENERIC_ALL
    ///  │ │ └─────────── A  = allow
    ///  │ └───────────── P  = protected: do NOT inherit any parent ACEs
    ///  └─────────────── D  = this is the DACL
    /// ```
    ///
    /// `P` matters as much as the ACE: without it the object can pick up inherited
    /// permissive entries. Note there is deliberately NO entry for Administrators
    /// or SYSTEM — they can take ownership regardless, so listing them would only
    /// widen the ACL without adding a protection anyone lacks.
    pub struct SecurityDescriptor {
        psd: *mut core::ffi::c_void,
        attrs: Box<SecurityAttributes>,
    }

    // The descriptor is a self-contained heap allocation we own; it is only read
    // by pipe creation, which we do from one thread at a time.
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}

    #[repr(C)]
    struct SecurityAttributes {
        n_length: u32,
        lp_security_descriptor: *mut core::ffi::c_void,
        b_inherit_handle: i32,
    }

    impl SecurityDescriptor {
        pub fn current_user_only() -> anyhow::Result<Self> {
            let sid = current_user_sid_string()?;
            let sddl = format!("D:P(A;;GA;;;{sid})");
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

            let mut psd: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the
            // call; `psd` is a valid out-pointer. On success the callee allocates
            // with LocalAlloc and we free it in Drop.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut psd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error())
                    .context(format!("converting SDDL {sddl:?} to a security descriptor"));
            }

            let attrs = Box::new(SecurityAttributes {
                n_length: std::mem::size_of::<SecurityAttributes>() as u32,
                lp_security_descriptor: psd,
                b_inherit_handle: 0,
            });
            Ok(SecurityDescriptor { psd, attrs })
        }

        fn as_attributes_ptr(&self) -> *mut core::ffi::c_void {
            (&*self.attrs) as *const SecurityAttributes as *mut core::ffi::c_void
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.psd.is_null() {
                // SAFETY: `psd` came from ConvertStringSecurityDescriptorToSecurityDescriptorW,
                // which documents LocalFree as the matching deallocator.
                unsafe { LocalFree(self.psd) };
            }
        }
    }

    /// The current process's user SID, as an SDDL string (`S-1-5-21-…`).
    fn current_user_sid_string() -> anyhow::Result<String> {
        unsafe {
            let mut token: isize = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error()).context("OpenProcessToken");
            }
            let _guard = HandleGuard(token);

            // Two-call idiom: ask for the size, then fill a buffer of that size.
            let mut needed: u32 = 0;
            GetTokenInformation(
                token,
                TOKEN_USER_CLASS,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
            if needed == 0 {
                return Err(io::Error::last_os_error()).context("GetTokenInformation (size)");
            }
            let mut buf = vec![0u8; needed as usize];
            if GetTokenInformation(
                token,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                needed,
                &mut needed,
            ) == 0
            {
                return Err(io::Error::last_os_error()).context("GetTokenInformation");
            }

            // TOKEN_USER begins with a SID_AND_ATTRIBUTES whose first field is the
            // SID pointer.
            let psid = *(buf.as_ptr() as *const *mut core::ffi::c_void);
            let mut pstr: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(psid, &mut pstr) == 0 {
                return Err(io::Error::last_os_error()).context("ConvertSidToStringSidW");
            }
            let mut len = 0usize;
            while *pstr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(pstr, len));
            LocalFree(pstr as *mut core::ffi::c_void);
            Ok(s)
        }
    }

    struct HandleGuard(isize);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    const SDDL_REVISION_1: u32 = 1;
    const TOKEN_QUERY: u32 = 0x0008;
    /// `TokenUser` from the TOKEN_INFORMATION_CLASS enum.
    const TOKEN_USER_CLASS: i32 = 1;

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut core::ffi::c_void,
            security_descriptor_size: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut core::ffi::c_void, string_sid: *mut *mut u16) -> i32;
        fn OpenProcessToken(process: isize, desired_access: u32, token_handle: *mut isize) -> i32;
        fn GetTokenInformation(
            token_handle: isize,
            token_information_class: i32,
            token_information: *mut core::ffi::c_void,
            token_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn CloseHandle(object: isize) -> i32;
        fn LocalFree(mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
}
