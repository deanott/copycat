//! `copycat` — the terminal client. Copy/paste/recall/pin from a shell; talks
//! to `copycatd` over the shared `copycat_core::proto` protocol.

mod osc52;
mod render;

use std::io::{IsTerminal, Read};

use anyhow::bail;
use clap::{Parser, Subcommand};

use copycat_core::proto::{socket_path, Request, Response, Selector};
use copycat_core::transform::Transform;

#[derive(Parser)]
#[command(
    name = "copycat",
    version,
    about = "Intelligent temporal clipboard — terminal client"
)]
struct Cli {
    /// Override the daemon socket path (default: $COPYCAT_SOCK, else
    /// $XDG_RUNTIME_DIR/copycat/copycatd.sock, else the temp dir — on macOS
    /// $TMPDIR/copycat/copycatd.sock, since XDG_RUNTIME_DIR is Linux-only).
    #[arg(long, global = true)]
    socket: Option<std::path::PathBuf>,

    /// Emit machine-readable JSON instead of human text.
    #[arg(long, global = true)]
    json: bool,

    /// Show Secret/PII values in cleartext (debug only). Requires a build with
    /// `--features debug-unsafe`; a hard error otherwise.
    #[arg(long, global = true)]
    insecure: bool,

    /// Scope copy/paste/recall/wait to a named channel (an agent hand-off lane).
    /// Omit for the default lane — the user's normal, OS-backed clipboard.
    #[arg(long, global = true)]
    channel: Option<String>,

    /// On `copy`, also emit an OSC 52 escape so the value lands on the clipboard
    /// of the terminal you are sitting at — the zero-setup way to get a copy off
    /// a headless/SSH box. One-way (remote → local) and size-capped.
    /// `COPYCAT_OSC52=1` makes it the default for a session.
    #[arg(long, global = true)]
    osc52: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Put text on the clipboard. With no TEXT (or "-"), read from stdin.
    Copy {
        text: Option<String>,
        /// Optional author provenance (e.g. "human:user", "agent:claude-3-5")
        #[arg(long)]
        author: Option<String>,
        /// Optional session identifier (e.g. "session_73b15e1a")
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Print a clipboard entry by recency index (0 = current, 1 = previous, …).
    Paste {
        /// How many entries back from the most recent (0 = current).
        #[arg(default_value_t = 0)]
        index: usize,
        /// Apply a deterministic edit-on-paste transform to the value on the way
        /// out (the stored entry is unchanged). One of: json_pretty, json_minify,
        /// base64_encode, base64_decode, upper, lower, trim, url_encode, url_decode.
        #[arg(long = "as", value_name = "TRANSFORM")]
        transform: Option<Transform>,
    },
    /// List recent entries, newest first (each shown with its recency index).
    Recall {
        /// How many to show (0 = all).
        #[arg(short = 'n', long = "last", default_value_t = 10)]
        last: usize,
    },
    /// Make an entry the current clipboard so the next Ctrl/Cmd+V yields it.
    /// Select by index (e.g. `4`) or id. Unlike `paste`, which only prints an
    /// entry, this actually puts it back on the OS clipboard — it moves the entry
    /// to newest rather than adding a duplicate, and keeps its id.
    #[command(alias = "promote")]
    Use { selector: String },
    /// Pin an entry (exempt from eviction/expiry) by index (e.g. `2`) or id.
    Pin { selector: String },
    /// Unpin a previously pinned entry by index or id.
    Unpin { selector: String },
    /// Block until an entry is handed off on a channel (agent hand-off). Use
    /// `--channel` to pick the lane; without `--take` it peeks (leaves the entry
    /// for other waiters), with `--take` it consumes it exactly once.
    Wait {
        /// Give up after this many milliseconds (capped daemon-side at 5min).
        #[arg(long = "timeout-ms")]
        timeout_ms: Option<u64>,
        /// Consume the entry (at-most-once TAKE) instead of peeking it.
        #[arg(long)]
        take: bool,
    },
    /// Delete an entry by index (e.g. `0`) or id. Explicit delete overrides a pin.
    #[command(alias = "rm", alias = "remove")]
    Delete { selector: String },
    /// Bulk clear clipboard history entries. By default removes unpinned entries in the channel.
    Clear {
        /// Also remove pinned entries.
        #[arg(long = "all", alias = "include-pinned")]
        include_pinned: bool,
        /// Apply clear across all channels/lanes.
        #[arg(long = "all-channels")]
        all_channels: bool,
    },
    /// Show daemon status (backend, entry count, device).
    Status,
    /// Check the daemon is alive.
    Ping,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.insecure {
        if cfg!(feature = "debug-unsafe") {
            std::env::set_var("COPYCAT_INSECURE", "1");
        } else {
            bail!("--insecure requires a build with `--features debug-unsafe`");
        }
    }
    let path = cli.socket.unwrap_or_else(socket_path);
    let channel = cli.channel.clone();

    let request = to_request(cli.command, channel, &Caller::detect())?;

    let response = copycat_core::ipc::roundtrip(&path, &request)?;

    osc52::mirror_if_wanted(cli.osc52, &request, &response);

    if cli.json {
        println!("{}", serde_json::to_string(&response)?);
    } else if let Some(text) = render::human(&response) {
        // No trailing-newline munging: `paste`/`wait` payloads must pipe cleanly.
        print!("{text}");
    }

    // Daemon-side errors and expired waits exit non-zero for scripts.
    if let Response::Error { message } = &response {
        bail!("{message}");
    }
    if let Response::WaitTimeout = &response {
        bail!("wait timed out: no hand-off on the channel within the timeout");
    }
    Ok(())
}

/// Who is running this process, resolved once in `main` so [`to_request`] stays
/// pure. Only `--author`/`--session-id` outrank it.
struct Caller {
    author: String,
    session_id: Option<String>,
}

impl Caller {
    /// Detect an agent (`COPYCAT_AGENT`, or `CLAUDECODE` = claude) or a person.
    /// The tty probe is on stderr: stdin is a pipe in ordinary
    /// `echo x | copycat copy -` usage, so it means nothing.
    fn detect() -> Self {
        let agent = std::env::var("COPYCAT_AGENT").ok().or_else(|| {
            std::env::var("CLAUDECODE")
                .ok()
                .map(|_| "claude".to_string())
        });
        Self {
            author: copycat_core::entry::cli_author(
                std::io::stderr().is_terminal(),
                agent.as_deref(),
            ),
            session_id: std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
        }
    }
}

/// Translate a parsed subcommand into the wire `Request`. Pure (bar stdin for
/// `Copy -`), so the mapping is unit-testable without a daemon.
fn to_request(
    command: Command,
    channel: Option<String>,
    caller: &Caller,
) -> anyhow::Result<Request> {
    Ok(match command {
        Command::Copy {
            text,
            author,
            session_id,
        } => Request::Copy {
            text: resolve_copy_text(text)?,
            channel,
            author: author.or_else(|| Some(caller.author.clone())),
            session_id: session_id.or_else(|| caller.session_id.clone()),
        },
        Command::Paste { index, transform } => Request::Read {
            index,
            channel,
            transform,
        },
        Command::Recall { last } => Request::Recall {
            query: copycat_core::store::Recall {
                limit: last,
                channel,
                ..Default::default()
            },
        },
        Command::Pin { selector } => Request::Pin {
            selector: parse_selector(selector),
            pinned: true,
        },
        Command::Unpin { selector } => Request::Pin {
            selector: parse_selector(selector),
            pinned: false,
        },
        Command::Wait { timeout_ms, take } => Request::Wait {
            channel,
            timeout_ms,
            after: None,
            consume: take,
        },
        Command::Use { selector } => Request::Promote {
            selector: parse_selector(selector),
        },
        Command::Delete { selector } => Request::Delete {
            selector: parse_selector(selector),
            channel,
        },
        Command::Clear {
            include_pinned,
            all_channels,
        } => Request::Clear {
            include_pinned,
            all_channels,
            channel,
        },
        Command::Status => Request::Status,
        Command::Ping => Request::Ping,
    })
}

/// A bare non-negative integer is a recency index; anything else is an id.
/// (Ids contain a `-`, so they never parse as a `usize`.)
fn parse_selector(s: String) -> Selector {
    match s.parse::<usize>() {
        Ok(index) => Selector::Index(index),
        Err(_) => Selector::Id(s),
    }
}

/// Resolve the text to copy: an explicit arg, or stdin when omitted or "-".
fn resolve_copy_text(arg: Option<String>) -> anyhow::Result<String> {
    match arg.as_deref() {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
        Some(_) => Ok(arg.unwrap()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed stand-in for a person at a terminal: `Caller::detect` reads the
    /// real environment and would flip under CI, which has no tty.
    fn test_caller() -> Caller {
        Caller {
            author: copycat_core::entry::AUTHOR_CLI.to_string(),
            session_id: None,
        }
    }

    #[test]
    fn parse_selector_index_vs_id() {
        assert_eq!(parse_selector("0".into()), Selector::Index(0));
        assert_eq!(parse_selector("12".into()), Selector::Index(12));
        // Ids contain a '-' and never parse as usize.
        assert_eq!(
            parse_selector("1699999999999-000001".into()),
            Selector::Id("1699999999999-000001".into())
        );
        assert_eq!(parse_selector("abc".into()), Selector::Id("abc".into()));
    }

    #[test]
    fn pin_sets_true_unpin_sets_false() {
        match to_request(
            Command::Pin {
                selector: "2".into(),
            },
            None,
            &test_caller(),
        )
        .unwrap()
        {
            Request::Pin { selector, pinned } => {
                assert_eq!(selector, Selector::Index(2));
                assert!(pinned, "pin must set pinned:true");
            }
            other => panic!("expected Pin, got {other:?}"),
        }
        match to_request(
            Command::Unpin {
                selector: "x-1".into(),
            },
            None,
            &test_caller(),
        )
        .unwrap()
        {
            Request::Pin { selector, pinned } => {
                assert_eq!(selector, Selector::Id("x-1".into()));
                assert!(!pinned, "unpin must set pinned:false");
            }
            other => panic!("expected Pin, got {other:?}"),
        }
    }

    #[test]
    fn wait_take_maps_to_consume_and_carries_channel() {
        match to_request(
            Command::Wait {
                timeout_ms: Some(500),
                take: true,
            },
            Some("build".into()),
            &test_caller(),
        )
        .unwrap()
        {
            Request::Wait {
                channel,
                timeout_ms,
                after,
                consume,
            } => {
                assert_eq!(channel.as_deref(), Some("build"));
                assert_eq!(timeout_ms, Some(500));
                assert_eq!(after, None);
                assert!(consume, "--take must map to consume:true");
            }
            other => panic!("expected Wait, got {other:?}"),
        }
        match to_request(
            Command::Wait {
                timeout_ms: None,
                take: false,
            },
            None,
            &test_caller(),
        )
        .unwrap()
        {
            Request::Wait { consume, .. } => assert!(!consume, "no --take = peek"),
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn use_maps_to_promote() {
        match to_request(
            Command::Use {
                selector: "5".into(),
            },
            None,
            &test_caller(),
        )
        .unwrap()
        {
            Request::Promote { selector } => assert_eq!(selector, Selector::Index(5)),
            other => panic!("expected Promote, got {other:?}"),
        }
    }

    #[test]
    fn paste_and_recall_carry_channel() {
        match to_request(
            Command::Paste {
                index: 3,
                transform: None,
            },
            Some("L".into()),
            &test_caller(),
        )
        .unwrap()
        {
            Request::Read { index, channel, .. } => {
                assert_eq!(index, 3);
                assert_eq!(channel.as_deref(), Some("L"));
            }
            other => panic!("expected Read, got {other:?}"),
        }
        match to_request(
            Command::Recall { last: 7 },
            Some("L".into()),
            &test_caller(),
        )
        .unwrap()
        {
            Request::Recall { query } => {
                assert_eq!(query.limit, 7);
                assert_eq!(query.channel.as_deref(), Some("L"));
            }
            other => panic!("expected Recall, got {other:?}"),
        }
    }

    #[test]
    fn copy_with_inline_text_does_not_read_stdin() {
        match to_request(
            Command::Copy {
                text: Some("hello".into()),
                author: None,
                session_id: None,
            },
            None,
            &test_caller(),
        )
        .unwrap()
        {
            Request::Copy {
                text,
                channel,
                author,
                session_id,
            } => {
                assert_eq!(text, "hello");
                assert!(channel.is_none());
                // Unflagged: attributed to the human at the terminal.
                assert_eq!(author.as_deref(), Some("human:cli"));
                assert!(session_id.is_none());
            }
            other => panic!("expected Copy, got {other:?}"),
        }
    }

    #[test]
    fn copy_without_flags_takes_the_callers_identity() {
        use copycat_core::entry::Origin;
        let req = to_request(
            Command::Copy {
                text: Some("x".into()),
                author: None,
                session_id: None,
            },
            None,
            &test_caller(),
        )
        .unwrap();
        let Request::Copy { author, .. } = req else {
            panic!("expected Copy")
        };
        assert_eq!(Origin::of(author.as_deref()), Origin::Human);
    }

    #[test]
    fn an_agent_caller_is_not_laundered_into_the_user() {
        use copycat_core::entry::Origin;
        let agent = Caller {
            author: "agent:claude".to_string(),
            session_id: Some("sess_9".to_string()),
        };
        let req = to_request(
            Command::Copy {
                text: Some("x".into()),
                author: None,
                session_id: None,
            },
            None,
            &agent,
        )
        .unwrap();
        let Request::Copy {
            author, session_id, ..
        } = req
        else {
            panic!("expected Copy")
        };
        assert_eq!(author.as_deref(), Some("agent:claude"));
        assert_eq!(Origin::of(author.as_deref()), Origin::Agent);
        // The detected session rides along so the badge can name the run.
        assert_eq!(session_id.as_deref(), Some("sess_9"));
    }

    #[test]
    fn explicit_flags_outrank_a_detected_caller() {
        let agent = Caller {
            author: "agent:claude".to_string(),
            session_id: Some("detected".to_string()),
        };
        let req = to_request(
            Command::Copy {
                text: Some("x".into()),
                author: Some("human:user".into()),
                session_id: Some("explicit".into()),
            },
            None,
            &agent,
        )
        .unwrap();
        let Request::Copy {
            author, session_id, ..
        } = req
        else {
            panic!("expected Copy")
        };
        assert_eq!(author.as_deref(), Some("human:user"));
        assert_eq!(session_id.as_deref(), Some("explicit"));
    }

    #[test]
    fn copy_author_flag_overrides_the_human_default() {
        use copycat_core::entry::Origin;
        // An agent shelling out to the CLI must keep its own identity.
        let req = to_request(
            Command::Copy {
                text: Some("x".into()),
                author: Some("agent:claude-3-5".into()),
                session_id: Some("sess_9".into()),
            },
            None,
            &test_caller(),
        )
        .unwrap();
        let Request::Copy {
            author, session_id, ..
        } = req
        else {
            panic!("expected Copy")
        };
        assert_eq!(author.as_deref(), Some("agent:claude-3-5"));
        assert_eq!(session_id.as_deref(), Some("sess_9"));
        assert_eq!(Origin::of(author.as_deref()), Origin::Agent);
    }

    #[test]
    fn delete_and_clear_map_to_requests() {
        match to_request(
            Command::Delete {
                selector: "3".into(),
            },
            Some("lane".into()),
            &test_caller(),
        )
        .unwrap()
        {
            Request::Delete { selector, channel } => {
                assert_eq!(selector, Selector::Index(3));
                assert_eq!(channel.as_deref(), Some("lane"));
            }
            other => panic!("expected Delete, got {other:?}"),
        }

        match to_request(
            Command::Clear {
                include_pinned: true,
                all_channels: true,
            },
            None,
            &test_caller(),
        )
        .unwrap()
        {
            Request::Clear {
                include_pinned,
                all_channels,
                channel,
            } => {
                assert!(include_pinned);
                assert!(all_channels);
                assert!(channel.is_none());
            }
            other => panic!("expected Clear, got {other:?}"),
        }
    }
}
