//! Wire protocol between the front-ends (`copycat-cli`, `copycat-mcp`, future
//! companions) and the daemon (`copycatd`).
//!
//! Deliberately transport-agnostic and dependency-light: it's just serde types.
//! The daemon frames these as **newline-delimited JSON** over a local Unix
//! socket (one `Request` per line, one `Response` per line). Keeping the codec
//! this simple means any language / a `socat` one-liner can drive the daemon,
//! and the same messages could later be tunnelled to a relay for cross-device
//! sync without changing the shape.

use serde::{Deserialize, Serialize};

use crate::entry::Entry;
use crate::privacy::Mode as PrivacyMode;
use crate::store::Recall;
use crate::transform::Transform;

/// Default socket filename; callers should prefer [`socket_path`].
pub const SOCKET_NAME: &str = "copycatd.sock";

/// The default (unnamed) clipboard lane. `""` reproduces the original flat
/// clipboard: it is the lane bound to the OS clipboard and the only lane an
/// unscoped `Read`/`Recall`/`Current` sees. Named lanes are agent mailboxes used
/// for inter-agent hand-off.
pub const DEFAULT_CHANNEL: &str = "";

/// Resolve the daemon socket path. Honours `COPYCAT_SOCK` (tests / isolated
/// daemons), else `$XDG_RUNTIME_DIR/copycat/copycatd.sock`, falling back to the
/// OS temp dir. The nested `copycat/` subdirectory exists so the daemon can lock
/// it `0700` and keep other local users off the socket (and the plaintext
/// clipboard), which matters most in the shared-`/tmp` fallback. On Windows the
/// address is a named pipe — see [`crate::ipc::default_addr`].
pub fn socket_path() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("COPYCAT_SOCK") {
        return std::path::PathBuf::from(explicit);
    }
    crate::ipc::default_addr()
}

/// How a caller points at a specific entry: by recency **index** (0 = current,
/// 1 = the one before, …) or by stable **id**. Serialized untagged, so a JSON
/// number is an index and a JSON string is an id — which also makes the CLI
/// `copycat pin 2` (index) vs `copycat pin 1783…-000000` (id) unambiguous.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Selector {
    Index(usize),
    Id(String),
}

/// A command sent to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness check.
    Ping,
    /// Record freshly-copied text; it becomes the current clipboard and, if the
    /// daemon owns an OS clipboard, is written there too. An optional `channel`
    /// scopes the write to a named lane (an agent mailbox); a scoped copy is
    /// private and does NOT touch the OS clipboard. Absent/`None` = the default
    /// lane (today's behaviour).
    Copy {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// The current (most-recent) entry. Sugar for `Read { index: 0 }`.
    Current,
    /// Fast recall of the entry `index` positions back from the most recent
    /// (0 = current). The hot-path "give me the thing I copied N ago". An
    /// optional `channel` scopes the read to a named lane (`None` = default lane).
    Read {
        index: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// Deterministic edit-on-paste transform applied to the returned text on
        /// the way out (the "edit" in `address → edit → destination`). `None` =
        /// return the value unchanged. Non-text entries ignore it. The daemon
        /// applies it to the plaintext it already returns; front-end Secret/PII
        /// masking still wins over any transform. See `crate::transform`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transform: Option<Transform>,
    },
    /// Recall history along the temporal axis, most-recent first.
    Recall { query: Recall },
    /// Pin/unpin an entry by index or id (exempt from eviction/expiry).
    Pin { selector: Selector, pinned: bool },
    /// Make an existing entry the **current** clipboard — i.e. what the next
    /// Ctrl/Cmd+V yields. This is what `Read` is not: `Read` only *tells* the
    /// caller what an entry says, leaving the OS clipboard untouched. `Promote`
    /// moves the entry to newest and writes it back to the OS clipboard, so the
    /// human can paste history entry N without copying it by hand.
    ///
    /// Default-lane only, by construction: the OS clipboard *is* the default
    /// lane, so a named-lane entry has nowhere to be promoted into.
    Promote { selector: Selector },
    /// Remove a single entry by index or id. Explicit delete OVERRIDES a pin
    /// (unlike eviction). channel scopes to a named lane (None = default lane).
    Delete {
        selector: Selector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    /// Bulk remove. By default removes only UNPINNED entries in the scoped lane.
    /// include_pinned=true also removes pinned entries. all_channels=true applies
    /// to EVERY lane (channel is then ignored). channel scopes to one named lane
    /// (None = default lane).
    Clear {
        #[serde(default)]
        include_pinned: bool,
        #[serde(default)]
        all_channels: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    /// Block until an entry is available on `channel`, then return it. The
    /// inter-agent hand-off primitive: A `Copy`s to a named lane, B `Wait`s on it.
    Wait {
        /// Lane to wait on (`None` = default lane).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// Give up after this many millis (capped daemon-side). `None` = the cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        /// Cursor: return the first entry with an id strictly greater than this.
        /// `None` = the lane head at wait-start (i.e. block for the *next* post).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<String>,
        /// `true` = TAKE (consume the entry, at-most-once hand-off); `false` =
        /// PEEK (leave it in the lane, fan-out to every waiter).
        #[serde(default)]
        consume: bool,
    },
    /// Change the daemon's [privacy posture](crate::privacy::Mode) — the switch
    /// behind the tray UI's privacy setting. Daemon-wide and immediate: it
    /// re-scopes entries already in history, not just future copies, so every
    /// surface agrees the moment it lands. Replies with the mode now in force.
    ///
    /// Deliberately not authenticated beyond the socket itself: anything that
    /// can open the socket already reads the plaintext clipboard, so the ability
    /// to loosen masking adds no new exposure.
    SetPrivacy { mode: PrivacyMode },
    /// Daemon status / introspection.
    Status,
}

/// The daemon's reply. Exactly one per request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum Response {
    Pong,
    /// Result of a `Copy` — the id assigned to the (new or refreshed) entry.
    Copied {
        id: String,
    },
    /// Result of `Current` — `None` when the clipboard has never been written.
    Current {
        entry: Option<Entry>,
    },
    /// Result of `Recall`, most-recent first.
    Entries {
        entries: Vec<Entry>,
    },
    /// Result of `Pin`.
    Pinned {
        id: String,
        pinned: bool,
    },
    /// Result of `Promote` — the entry that is now the current clipboard.
    Promoted {
        entry: Entry,
    },
    /// Result of `Delete` — the id of the entry that was removed.
    Deleted {
        id: String,
    },
    /// Result of `Clear` — how many entries were removed.
    Cleared {
        removed: usize,
    },
    /// Result of a fulfilled `Wait` — the entry handed off on the lane.
    Handoff {
        entry: Entry,
    },
    /// Result of a `Wait` whose deadline passed with nothing to hand off
    /// (distinct from an empty read, so a caller can retry vs. give up).
    WaitTimeout,
    /// Result of `SetPrivacy` — the posture now in force daemon-wide.
    Privacy {
        mode: PrivacyMode,
    },
    /// Result of `Status`.
    Status(DaemonStatus),
    /// Any command can fail with a human-readable message.
    Error {
        message: String,
    },
}

/// Snapshot of what the daemon is doing — surfaced by `copycat status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Daemon version (crate version).
    pub version: String,
    /// How the daemon relates to the OS clipboard on this host.
    pub clipboard: ClipboardBackendInfo,
    /// Entries currently held in the temporal store.
    pub entries: usize,
    /// The device label stamped on entries this daemon records.
    pub device: String,
    /// Whether the daemon is running in insecure debug mode (Secret/PII shown in
    /// cleartext). Always `false` in release builds; surfaced in `status` so an
    /// accidentally-left-on daemon announces itself. Defaults to `false` for
    /// wire back-compat with older clients that don't send it.
    #[serde(default)]
    pub insecure: bool,
    /// The daemon's active [privacy posture](crate::privacy::Mode) — the single
    /// dial for how much is masked, on every surface. Defaults to
    /// [`PrivacyMode::Guarded`] when absent, so a reply from an older daemon
    /// decodes to the posture that daemon actually had.
    #[serde(default)]
    pub privacy: PrivacyMode,
}

/// Which OS-clipboard backend the daemon attached at startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardBackendInfo {
    /// Bound to the real OS clipboard (e.g. arboard over X11/Wayland). Copies
    /// made elsewhere on the machine are ingested; `Copy` writes back to it.
    Os { name: String },
    /// No display / no OS clipboard reachable. The daemon is authoritative and
    /// purely socket-driven — this is the "shared clipboard between sessions /
    /// over SSH / headless" mode.
    Headless,
}

impl ClipboardBackendInfo {
    /// The human label every surface renders for this backend.
    pub fn label(&self) -> String {
        match self {
            ClipboardBackendInfo::Os { name } => format!("OS clipboard ({name})"),
            ClipboardBackendInfo::Headless => "headless (socket-only)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, Source};

    fn src() -> Source {
        Source::simple("test")
    }

    #[test]
    fn request_roundtrips_as_tagged_json() {
        let reqs = vec![
            Request::Ping,
            Request::Copy {
                text: "hi".into(),
                channel: None,
                author: None,
                session_id: None,
            },
            Request::Copy {
                text: "hi".into(),
                channel: Some("build".into()),
                author: Some("agent:claude".into()),
                session_id: Some("sess_123".into()),
            },
            Request::Current,
            Request::Read {
                index: 2,
                channel: None,
                transform: None,
            },
            Request::Read {
                index: 0,
                channel: Some("build".into()),
                transform: None,
            },
            Request::Read {
                index: 0,
                channel: None,
                transform: Some(crate::transform::Transform::JsonPretty),
            },
            Request::Recall {
                query: Recall {
                    limit: 3,
                    since: Some(1),
                    until: None,
                    channel: None,
                },
            },
            Request::Recall {
                query: Recall {
                    limit: 3,
                    since: None,
                    until: None,
                    channel: Some("build".into()),
                },
            },
            Request::Wait {
                channel: Some("build".into()),
                timeout_ms: Some(500),
                after: None,
                consume: true,
            },
            Request::Wait {
                channel: None,
                timeout_ms: None,
                after: Some("x".into()),
                consume: false,
            },
            Request::Pin {
                selector: Selector::Index(2),
                pinned: true,
            },
            Request::Pin {
                selector: Selector::Id("a".into()),
                pinned: false,
            },
            Request::Promote {
                selector: Selector::Index(4),
            },
            Request::Promote {
                selector: Selector::Id("b".into()),
            },
            Request::Delete {
                selector: Selector::Index(5),
                channel: None,
            },
            Request::Delete {
                selector: Selector::Id("c".into()),
                channel: Some("build".into()),
            },
            Request::Clear {
                include_pinned: false,
                all_channels: false,
                channel: None,
            },
            Request::Clear {
                include_pinned: true,
                all_channels: true,
                channel: Some("build".into()),
            },
            Request::SetPrivacy {
                mode: PrivacyMode::Off,
            },
            Request::SetPrivacy {
                mode: PrivacyMode::Strict,
            },
            Request::Status,
        ];
        for r in reqs {
            let line = serde_json::to_string(&r).unwrap();
            assert!(
                !line.contains('\n'),
                "wire line must be single-line: {line}"
            );
            let back: Request = serde_json::from_str(&line).unwrap();
            // Re-serialise and compare to guard the tag/rename contract.
            assert_eq!(serde_json::to_string(&back).unwrap(), line);
        }
    }

    #[test]
    fn copy_request_uses_op_tag() {
        // `skip_serializing_if` keeps the wire form byte-identical to the
        // pre-channels protocol, so old daemons/clients still interop.
        let line = serde_json::to_string(&Request::Copy {
            text: "x".into(),
            channel: None,
            author: None,
            session_id: None,
        })
        .unwrap();
        assert_eq!(line, r#"{"op":"copy","text":"x"}"#);
    }

    #[test]
    fn old_form_copy_deserializes_with_none_channel() {
        // A pre-channels client sends no `channel` field; it must decode to None.
        let req: Request = serde_json::from_str(r#"{"op":"copy","text":"x"}"#).unwrap();
        match req {
            Request::Copy {
                text,
                channel,
                author,
                session_id,
            } => {
                assert_eq!(text, "x");
                assert_eq!(channel, None);
                assert_eq!(author, None);
                assert_eq!(session_id, None);
            }
            other => panic!("expected Copy, got {other:?}"),
        }
        // Old-form Read too: a pre-transform client sends neither `channel` nor
        // `transform`; both must decode to None (wire back-compat).
        match serde_json::from_str::<Request>(r#"{"op":"read","index":0}"#).unwrap() {
            Request::Read {
                index,
                channel,
                transform,
            } => {
                assert_eq!(index, 0);
                assert_eq!(channel, None);
                assert_eq!(transform, None);
            }
            other => panic!("expected Read, got {other:?}"),
        }
    }

    #[test]
    fn read_without_transform_omits_the_field() {
        // `skip_serializing_if` keeps the wire form byte-identical to the
        // pre-transform protocol when no transform is requested.
        let line = serde_json::to_string(&Request::Read {
            index: 0,
            channel: None,
            transform: None,
        })
        .unwrap();
        assert_eq!(line, r#"{"op":"read","index":0}"#);
    }

    #[test]
    fn read_with_transform_serializes_snake_case_and_roundtrips() {
        let line = serde_json::to_string(&Request::Read {
            index: 1,
            channel: None,
            transform: Some(crate::transform::Transform::Base64Encode),
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"op":"read","index":1,"transform":"base64_encode"}"#
        );
        match serde_json::from_str::<Request>(&line).unwrap() {
            Request::Read {
                index, transform, ..
            } => {
                assert_eq!(index, 1);
                assert_eq!(transform, Some(crate::transform::Transform::Base64Encode));
            }
            other => panic!("expected Read, got {other:?}"),
        }
    }

    #[test]
    fn channel_scoped_copy_serializes_field() {
        let line = serde_json::to_string(&Request::Copy {
            text: "x".into(),
            channel: Some("a".into()),
            author: None,
            session_id: None,
        })
        .unwrap();
        assert_eq!(line, r#"{"op":"copy","text":"x","channel":"a"}"#);
    }

    #[test]
    fn delete_without_channel_omits_the_field() {
        // `skip_serializing_if` on `channel` keeps the default-lane delete compact;
        // an id selector serialises as a bare string, an index as a bare number.
        assert_eq!(
            serde_json::to_string(&Request::Delete {
                selector: Selector::Index(5),
                channel: None,
            })
            .unwrap(),
            r#"{"op":"delete","selector":5}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::Delete {
                selector: Selector::Id("x".into()),
                channel: Some("build".into()),
            })
            .unwrap(),
            r#"{"op":"delete","selector":"x","channel":"build"}"#
        );
    }

    #[test]
    fn clear_defaults_serialize_and_roundtrip() {
        // The bool flags always serialise (they carry the semantics); only
        // `channel` is skipped when None.
        let line = serde_json::to_string(&Request::Clear {
            include_pinned: false,
            all_channels: false,
            channel: None,
        })
        .unwrap();
        assert_eq!(
            line,
            r#"{"op":"clear","include_pinned":false,"all_channels":false}"#
        );
        // A pre-clear client that sends only the op decodes to all-defaults.
        match serde_json::from_str::<Request>(r#"{"op":"clear"}"#).unwrap() {
            Request::Clear {
                include_pinned,
                all_channels,
                channel,
            } => {
                assert!(!include_pinned);
                assert!(!all_channels);
                assert_eq!(channel, None);
            }
            other => panic!("expected Clear, got {other:?}"),
        }
    }

    #[test]
    fn set_privacy_uses_snake_case_mode_keys() {
        assert_eq!(
            serde_json::to_string(&Request::SetPrivacy {
                mode: PrivacyMode::Guarded,
            })
            .unwrap(),
            r#"{"op":"set_privacy","mode":"guarded"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::Privacy {
                mode: PrivacyMode::Strict,
            })
            .unwrap(),
            r#"{"ok":"privacy","mode":"strict"}"#
        );
    }

    #[test]
    fn status_from_an_older_daemon_decodes_to_the_default_posture() {
        // A pre-privacy daemon sends no `privacy`; it must decode to what that
        // daemon actually did — guarded, PII to the model.
        let s: DaemonStatus = serde_json::from_str(
            r#"{"version":"0.1.0","clipboard":"headless","entries":0,"device":"d"}"#,
        )
        .unwrap();
        assert_eq!(s.privacy, PrivacyMode::Guarded);
    }

    #[test]
    fn backend_label_names_the_backend() {
        assert_eq!(
            ClipboardBackendInfo::Os { name: "X11".into() }.label(),
            "OS clipboard (X11)"
        );
        assert_eq!(
            ClipboardBackendInfo::Headless.label(),
            "headless (socket-only)"
        );
    }

    #[test]
    fn selector_is_untagged_number_or_string() {
        // A JSON number deserializes to Index; a JSON string to Id.
        assert_eq!(
            serde_json::from_str::<Selector>("2").unwrap(),
            Selector::Index(2)
        );
        assert_eq!(
            serde_json::from_str::<Selector>(r#""abc""#).unwrap(),
            Selector::Id("abc".into())
        );
        assert_eq!(serde_json::to_string(&Selector::Index(5)).unwrap(), "5");
        assert_eq!(
            serde_json::to_string(&Selector::Id("x".into())).unwrap(),
            r#""x""#
        );
    }

    #[test]
    fn response_roundtrips() {
        let entry = Entry::from_text("1", "hello", 5, src());
        let resps = vec![
            Response::Pong,
            Response::Copied { id: "1".into() },
            Response::Current {
                entry: Some(entry.clone()),
            },
            Response::Current { entry: None },
            Response::Entries {
                entries: vec![entry.clone()],
            },
            Response::Pinned {
                id: "1".into(),
                pinned: false,
            },
            Response::Promoted {
                entry: entry.clone(),
            },
            Response::Deleted { id: "1".into() },
            Response::Cleared { removed: 3 },
            Response::Handoff { entry },
            Response::WaitTimeout,
            Response::Error {
                message: "boom".into(),
            },
        ];
        for r in resps {
            let line = serde_json::to_string(&r).unwrap();
            let back: Response = serde_json::from_str(&line).unwrap();
            assert_eq!(serde_json::to_string(&back).unwrap(), line);
        }
    }

    /// Serializes the two tests that mutate COPYCAT_SOCK / XDG_RUNTIME_DIR, which
    /// are process-global; without this they can race each other. Recovers from
    /// poisoning so one panicking test doesn't cascade.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn socket_path_honours_explicit_override() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // COPYCAT_SOCK wins when set (documented contract for isolated daemons).
        temp_env_var("COPYCAT_SOCK", Some("/tmp/custom.sock"), || {
            assert_eq!(socket_path(), std::path::PathBuf::from("/tmp/custom.sock"));
        });
    }

    /// The `#[cfg(unix)]` gate is a RUNTIME concern, not a compile one: this
    /// test type-checks on Windows but asserts the wrong answer there —
    /// `default_addr` only consults `XDG_RUNTIME_DIR` under `#[cfg(unix)]`; the
    /// sibling test below covers the named-pipe answer.
    #[cfg(unix)]
    #[test]
    fn socket_path_falls_back_to_runtime_dir() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        temp_env_var("COPYCAT_SOCK", None, || {
            temp_env_var("XDG_RUNTIME_DIR", Some("/run/user/42"), || {
                assert_eq!(
                    socket_path(),
                    std::path::PathBuf::from("/run/user/42/copycat/copycatd.sock")
                );
            });
        });
    }

    /// The Windows half: no socket path, a per-user named pipe, and
    /// `XDG_RUNTIME_DIR` ignored entirely. Also asserts the non-alphanumeric
    /// normalisation in `default_addr` — a domain login like `CORP\dean` must not
    /// smuggle a backslash into the pipe name.
    #[cfg(windows)]
    #[test]
    fn socket_path_is_a_named_pipe_ignoring_runtime_dir() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        temp_env_var("COPYCAT_SOCK", None, || {
            temp_env_var("XDG_RUNTIME_DIR", Some("/run/user/42"), || {
                temp_env_var("USERNAME", Some("CORP\\dean"), || {
                    assert_eq!(
                        socket_path(),
                        std::path::PathBuf::from(r"\\.\pipe\copycatd-CORP_dean")
                    );
                });
            });
        });
    }

    /// Minimal scoped env-var setter for these serial tests. Restores the prior
    /// value on drop of the closure. (Tests in this module don't run in parallel
    /// with each other in a way that races these keys.)
    fn temp_env_var(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let prev = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
