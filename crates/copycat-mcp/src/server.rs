//! The copycat MCP server: the tool surface plus the intent model that tells
//! the model *when* to reach for the clipboard. Every tool is a thin proxy onto
//! `copycatd` (see `proxy`), so the model reads/writes the same clipboard the
//! user's Ctrl+C / Ctrl+V touches.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;

use copycat_core::entry::{Content as EntryContent, Entry, Sensitivity, AUTHOR_MCP};
use copycat_core::proto::{Request, Response, Selector};
use copycat_core::store::Recall;
use copycat_core::transform::Transform;
use copycat_core::view::{self, Audience};

use crate::proxy::Daemon;

/// Server-level guidance the host shows the model: when to WRITE and READ the
/// clipboard, plus the safety rules.
pub const INSTRUCTIONS: &str = "\
copycat is the user's shared, always-on clipboard. You and the user read and \
write the SAME OS clipboard through it, so anything you copy here the user can \
immediately paste with Ctrl/Cmd+V (and vice-versa).

WHEN TO COPY (call `clipboard_copy`): whenever you produce a concrete value the \
user is about to paste somewhere outside this chat — a generated username, \
password suggestion, URL/link, command to run, code snippet, an email or message \
you drafted, a grammar-/tone-fixed version of their text, an ID, an OTP, or \
synthetic/test form data. Do it proactively the moment such a value appears; \
don't wait to be asked. If you rewrite or reformat text the user gave you (\"fix \
this grammar\", \"make this JSON\", \"turn this into a polite email\"), copy the \
RESULT back so it replaces what's on their clipboard, ready to paste. Each \
`clipboard_copy` handles ONE value: when you produce several at once (say a \
username and an email), copy EACH separately so all of them exist in history — a \
single call surfaces only one, and the others are simply lost.

WHEN TO READ (call `clipboard_read_current` / `clipboard_history`): when the user \
refers to something they copied (\"the link I just copied\", \"what I grabbed a \
few minutes ago\", \"use my email from the clipboard\"), or when filling a form \
and a needed value is likely already on the clipboard. `clipboard_history` is \
time-ordered, newest first — use it for \"earlier\" / \"the one before that\".

MAKING SOMETHING PASTEABLE (call `clipboard_promote`): the OS clipboard holds \
exactly ONE value at a time. `clipboard_read_current` only tells YOU what an entry \
says — it does not change what the user's Ctrl/Cmd+V produces. To hand the user an \
entry from history, `clipboard_promote` it: that makes it the current clipboard. \
\"Paste the thing I copied 4 ago\" = promote index 4, then they press Ctrl/Cmd+V. \
Promote by the entry's short handle (`c0`, `c7`, `c1a` — shown as `id=` in \
`clipboard_history`) rather than by `index` whenever the user is working at human \
pace: indices shift the moment anything new is copied, including the user's own \
Ctrl+C elsewhere, while a handle keeps naming the same entry. Handles are short \
on purpose — they cost you almost nothing to repeat, which is the whole point of \
referring to a value instead of retyping it.

FORM-FILLING PATTERN (the common case): when the user is about to fill a form on \
a site, help assemble the fields (email, name, date of birth, address, text \
answers). `clipboard_copy` each value once so it exists in history, and NOTE ITS \
HANDLE. Then walk the form with them: as they reach each field, \
`clipboard_promote` that field's entry by handle and they fill it with a single \
Ctrl/Cmd+V. Do not re-`copy` the \
same value to re-surface it — that appends a duplicate and shifts every index; \
promote moves the existing entry instead. If you're a browser/computer agent \
driving the page yourself, you can skip the clipboard round-trip and just read the \
values and type them in.

SAFETY (non-negotiable): the daemon classifies entries. `Secret` values \
(passwords, API keys, private keys, tokens) are MASKED when you read them and \
must never be echoed; you can still `clipboard_copy` a secret you just generated \
so the user can paste it, but never reproduce a secret you read back. `PII` \
(emails, cards) is shown only when the user is clearly asking for it. Previews in \
`clipboard_history` are masked for anything sensitive.

EDIT-ON-PASTE (deterministic transforms): when reading a value, you can pass a \
`transform` to `clipboard_read_current` to reformat it on the way out \
(json_pretty/json_minify, base64_encode/decode, url_encode/decode, upper, lower, \
trim) — pick one when the destination wants a specific format. It never mutates \
the stored entry, and Secret/PII values stay masked regardless. Grammar/tone/\
translation rewrites are NOT these — do those yourself and `clipboard_copy` the \
result.

AGENT HAND-OFF (channels): the default clipboard (no `channel`) is the user's real \
OS clipboard. Passing a `channel` name to `clipboard_copy` posts to a private \
named lane instead — an agent-to-agent mailbox that does NOT touch the user's \
paste buffer. Another agent receives it by calling `clipboard_wait` on the same \
channel, which BLOCKS until an entry is posted (or the timeout elapses). Use \
`take: true` to consume it exactly once (a work hand-off), or the default peek to \
let every waiter see it (a broadcast). Secret masking is unchanged on channels: \
hand off a real secret by reference/id, never by echoing its value.";

/// Arguments for `clipboard_copy`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CopyArgs {
    /// The exact text to place on the user's OS clipboard, ready to paste.
    pub text: String,
    /// Optional agent hand-off lane. Omit for the user's real OS clipboard;
    /// set a name to post privately to that channel (NOT the OS clipboard) for
    /// another agent to receive via `clipboard_wait`.
    #[serde(default)]
    pub channel: Option<String>,
    /// Optional author provenance (e.g. "agent:claude-3-5", "agent:mcp").
    #[serde(default)]
    pub author: Option<String>,
    /// Optional session identifier (e.g. "session_73b15e1a").
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Arguments for `clipboard_copy_many`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CopyManyArgs {
    /// The values to copy, in order. Each becomes its own history entry; the
    /// LAST one ends up as the current clipboard. Pass a real list of strings —
    /// do NOT jam several values into one string, since values legitimately
    /// contain commas/newlines (URLs, code, addresses).
    pub texts: Vec<String>,
    /// Optional agent hand-off lane, applied to every value. Omit for the user's
    /// real OS clipboard; set a name to post privately to that channel.
    #[serde(default)]
    pub channel: Option<String>,
    /// Optional author provenance (e.g. "agent:claude-3-5", "agent:mcp").
    #[serde(default)]
    pub author: Option<String>,
    /// Optional session identifier (e.g. "session_73b15e1a").
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Arguments for `clipboard_history`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HistoryArgs {
    /// How many recent entries to return, newest first. Defaults to 10.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Arguments for `clipboard_read_current`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// How many entries back from the most recent: 0 = current (default),
    /// 1 = the one before, etc. Use `clipboard_history` to see indices.
    #[serde(default)]
    pub index: Option<usize>,
    /// Optional channel to read from. Omit for the user's OS clipboard; set a
    /// name to peek that agent hand-off lane instead.
    #[serde(default)]
    pub channel: Option<String>,
    /// Optional deterministic EDIT-ON-PASTE transform applied to the returned
    /// value (the stored entry is unchanged). One of: `json_pretty`,
    /// `json_minify`, `base64_encode`, `base64_decode`, `upper`, `lower`, `trim`,
    /// `url_encode`, `url_decode`. Secret/PII values stay masked regardless.
    #[serde(default)]
    pub transform: Option<String>,
}

/// Arguments for `clipboard_wait`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitArgs {
    /// The agent hand-off channel to block on. Omit to wait on the default
    /// (OS) clipboard lane.
    #[serde(default)]
    pub channel: Option<String>,
    /// Give up after this many milliseconds (capped daemon-side at 5 minutes).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Consume the entry (at-most-once TAKE) so no other waiter gets it. Default
    /// false = peek (leave it for other waiters / fan-out).
    #[serde(default)]
    pub take: Option<bool>,
}

/// Arguments for `clipboard_promote`. Provide `index` (from `clipboard_history`)
/// or `id`; `index` wins if both are given.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteArgs {
    /// Recency index of the entry to make current (0 = already current).
    /// From `clipboard_history`.
    #[serde(default)]
    pub index: Option<usize>,
    /// The entry id (alternative to `index`). Prefer this when the user is moving
    /// through a form at human pace — ids are stable, indices shift whenever
    /// anything new is copied.
    #[serde(default)]
    pub id: Option<String>,
}

/// Arguments for `clipboard_promote_many`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PromoteManyArgs {
    /// Selectors (indices like "0", "1" or stable IDs) of entries to promote in order.
    pub selectors: Vec<String>,
}

/// Arguments for `clipboard_pin`. Provide `index` (from `clipboard_history`) or
/// `id`; `index` wins if both are given.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PinArgs {
    /// Recency index of the entry to pin (0 = current). From `clipboard_history`.
    #[serde(default)]
    pub index: Option<usize>,
    /// The entry's short handle (`c7`) or full id, from `clipboard_history`
    /// (alternative to `index`). Prefer the handle: it is stable like the id but
    /// a fraction of the tokens.
    #[serde(default)]
    pub id: Option<String>,
    /// Pin (true, default) to exempt from eviction, or unpin (false).
    #[serde(default)]
    pub pinned: Option<bool>,
}

/// Arguments for `clipboard_delete`. Provide `index` (from `clipboard_history`) or
/// `id`; `index` wins if both are given.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeleteArgs {
    /// Recency index of the entry to delete (0 = current). From `clipboard_history`.
    #[serde(default)]
    pub index: Option<usize>,
    /// The entry's short handle (`c7`) or full id, from `clipboard_history`
    /// (alternative to `index`). Prefer the handle: it is stable like the id but
    /// a fraction of the tokens.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional channel/lane to delete from.
    #[serde(default)]
    pub channel: Option<String>,
}

/// Arguments for `clipboard_clear`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearArgs {
    /// Set true to also remove pinned entries (default false keeps pinned entries).
    #[serde(default)]
    pub include_pinned: Option<bool>,
    /// Set true to clear entries across ALL channels/lanes.
    #[serde(default)]
    pub all_channels: Option<bool>,
    /// Optional channel/lane to clear. Omit for the default OS clipboard lane.
    #[serde(default)]
    pub channel: Option<String>,
}

/// Environment override for [`Copycat::session_id`], for a host that knows the
/// agent's real session identity.
pub const SESSION_ENV: &str = "COPYCAT_SESSION";

/// One stdio process = one agent session. FNV over pid + start time so the
/// crate needs no RNG dependency; [`SESSION_ENV`] overrides.
fn derive_session_id() -> String {
    if let Ok(explicit) = std::env::var(SESSION_ENV) {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in u64::from(std::process::id())
        .to_le_bytes()
        .into_iter()
        .chain(millis.to_le_bytes())
    {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("sess_{:08x}", h as u32)
}

/// The MCP server: a daemon handle, the session identity stamped onto its
/// writes, and the tool router.
#[derive(Clone)]
pub struct Copycat {
    daemon: Daemon,
    session_id: String,
    tool_router: rmcp::handler::server::tool::ToolRouter<Copycat>,
}

#[tool_router]
impl Copycat {
    pub fn new(daemon: Daemon) -> Self {
        Copycat {
            daemon,
            session_id: derive_session_id(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Put text on the user's REAL OS clipboard so they (or a \
computer/browser agent) can paste it with Ctrl/Cmd+V. Call this proactively \
whenever you generate a value meant to be pasted elsewhere: a username, link, \
command, code snippet, drafted email/message, a grammar- or tone-fixed rewrite, \
an ID/OTP, or synthetic form data. If you transformed the user's text, copy the \
result back so it replaces the clipboard. Copies EXACTLY ONE value — if you \
produced several pasteable values (e.g. a username AND an email), call this once \
PER value so each lands as its own history entry the user can later promote; a \
single call only ever surfaces one of them."
    )]
    async fn clipboard_copy(
        &self,
        Parameters(a): Parameters<CopyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let channel = a.channel.clone();
        let author = a.author.or_else(|| Some(AUTHOR_MCP.into()));
        let session_id = a.session_id.or_else(|| Some(self.session_id.clone()));
        match self
            .call(&Request::Copy {
                text: a.text,
                channel: a.channel,
                author,
                session_id,
            })
            .await
        {
            Ok(Response::Copied { id }) => {
                let id = addr_of(&id);
                let msg = match channel {
                    Some(ch) => format!(
                        "Posted to hand-off channel '{ch}' (id {id}). It is private to that lane \
(NOT the OS clipboard); another agent can receive it with clipboard_wait."
                    ),
                    None => format!(
                        "Copied to the OS clipboard (id {id}). The user can now paste it with Ctrl/Cmd+V."
                    ),
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Copy SEVERAL values at once, each as its own history entry \
— the batch form of `clipboard_copy`. Use this the moment you generate more than \
one pasteable value in a single step (e.g. a username AND an email, or every field \
of a signup form): pass them as a list and each becomes independently \
promotable/pasteable later. The values are copied in order and the LAST one ends \
up as the current clipboard. Pass a real list of strings, never one \
comma-joined string (values contain commas/newlines). Returns each value's id in \
order so you can `clipboard_promote` the right one per form field."
    )]
    async fn clipboard_copy_many(
        &self,
        Parameters(a): Parameters<CopyManyArgs>,
    ) -> Result<CallToolResult, McpError> {
        if a.texts.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "`texts` is empty — pass at least one value to copy.",
            )]));
        }
        let channel = a.channel.clone();
        let author = a.author.or_else(|| Some(AUTHOR_MCP.into()));
        let session_id = a.session_id.or_else(|| Some(self.session_id.clone()));
        let mut ids = Vec::with_capacity(a.texts.len());
        for (i, text) in a.texts.into_iter().enumerate() {
            match self
                .daemon
                .call(&Request::Copy {
                    text,
                    channel: channel.clone(),
                    author: author.clone(),
                    session_id: session_id.clone(),
                })
                .await
            {
                Ok(Response::Copied { id }) => ids.push(addr_of(&id)),
                // Fail loud but partial: report how many made it in so the model
                // knows the clipboard state rather than assuming all-or-nothing.
                Ok(Response::Error { message }) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Copied {} of {} value(s), then the daemon reported an error: {message}",
                        ids.len(),
                        i + 1
                    ))]))
                }
                Ok(other) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Copied {} of {} value(s), then got an unexpected daemon response: {other:?}",
                    ids.len(),
                    i + 1
                ))]))
                }
                Err(e) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Copied {} value(s), then the daemon failed on value #{}: {e}",
                        ids.len(),
                        i
                    ))]))
                }
            }
        }
        let msg = match channel {
            Some(ch) => format!(
                "Posted {} value(s) to hand-off channel '{ch}' (ids in order: {}). Private to that \
lane (NOT the OS clipboard); another agent can receive them with clipboard_wait.",
                ids.len(),
                ids.join(", ")
            ),
            None => format!(
                "Copied {} value(s) to the clipboard (ids in order: {}). Each is its own history \
entry; the LAST (id {}) is the current clipboard the user pastes with Ctrl/Cmd+V. Promote any \
other by its id to make it pasteable.",
                ids.len(),
                ids.join(", "),
                ids.last().map(String::as_str).unwrap_or("?")
            ),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    #[tool(
        description = "Read a clipboard entry by recency: `index` 0 = the current \
clipboard (default), 1 = the one before, etc. Use for \"the thing I just copied\" \
(index 0) or \"the one before that\" (index 1), or when a form field likely needs \
a recent clipboard value. Get indices from `clipboard_history`. Secret values are \
returned masked and must not be echoed. Optionally pass `transform` to reformat \
the value on the way out (see EDIT-ON-PASTE in the instructions): json_pretty, \
json_minify, base64_encode, base64_decode, upper, lower, trim, url_encode, \
url_decode."
    )]
    async fn clipboard_read_current(
        &self,
        Parameters(a): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let index = a.index.unwrap_or(0);
        let transform = match a.transform.as_deref() {
            Some(name) => match name.parse::<Transform>() {
                Ok(t) => Some(t),
                Err(e) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        e.to_string(),
                    )]))
                }
            },
            None => None,
        };
        match self
            .call(&Request::Read {
                index,
                channel: a.channel,
                transform,
            })
            .await
        {
            Ok(Response::Current { entry: Some(e) }) => {
                let audience = self.model_audience().await;
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    read_view(&e, audience),
                )]))
            }
            Ok(Response::Current { entry: None }) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "No clipboard entry at index {index}."
                ))]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Block until an entry is handed off on an agent channel, \
then return it. This is the inter-agent hand-off receive primitive: another agent \
(or you, earlier) posts with `clipboard_copy` using the same `channel`, and this \
call waits until that arrives (or `timeout_ms` elapses). Set `take: true` to \
consume it exactly once (a work hand-off — no other waiter gets it); leave it \
false to peek (fan-out to every waiter). Omit `channel` to wait on the user's \
clipboard. Secret values are still returned masked."
    )]
    async fn clipboard_wait(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let consume = a.take.unwrap_or(false);
        let req = Request::Wait {
            channel: a.channel,
            timeout_ms: a.timeout_ms,
            after: None,
            consume,
        };
        match self.call(&req).await {
            Ok(Response::Handoff { entry }) => {
                // Same masking policy as a normal read: Secret is never echoed.
                let audience = self.model_audience().await;
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    read_view(&entry, audience),
                )]))
            }
            Ok(Response::WaitTimeout) => Ok(CallToolResult::success(vec![ContentBlock::text(
                "No hand-off arrived on the channel within the timeout. Nothing was received.",
            )])),
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "List recent clipboard entries, newest first (the temporal \
history). Each line starts with its recency index (0 = current) — pass that index \
to `clipboard_read_current` to read it or `clipboard_pin` to pin it. Use for \
\"what did I copy earlier / a few minutes ago\" or to pick \"the one before that\". \
Previews of secret/PII entries are masked; read a specific entry's full value only \
when the user clearly wants it."
    )]
    async fn clipboard_history(
        &self,
        Parameters(a): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        // limit==0 is the store's internal "no limit" sentinel; clamp it so a
        // model passing limit:0 doesn't get the entire masked history dumped.
        let limit = match a.limit {
            None | Some(0) => 10,
            Some(n) => n as usize,
        };
        let query = Recall {
            limit,
            ..Default::default()
        };
        match self.call(&Request::Recall { query }).await {
            Ok(Response::Entries { entries }) => {
                let audience = self.model_audience().await;
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    history_view(&entries, audience),
                )]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Make an existing clipboard entry the CURRENT clipboard, so \
the user's very next Ctrl/Cmd+V pastes it. This is the tool for \"paste the thing I \
copied 4 ago\" and for walking a user through a form: as they reach each field, \
promote the entry that belongs in it and they just hit Ctrl/Cmd+V. \
`clipboard_read_current` only TELLS you what an entry says — it does not make it \
pasteable; this does. Identify the entry by `index` (from clipboard_history) or, \
preferably for a human-paced flow, its stable `id` (indices shift when anything new \
is copied). The entry moves to newest rather than being duplicated, and keeps its \
id. Safe for secrets: this puts the value on the clipboard WITHOUT returning it to \
you, so you can hand the user a password to paste without ever reading it."
    )]
    async fn clipboard_promote(
        &self,
        Parameters(a): Parameters<PromoteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let selector = match selector_from(a.index, a.id, "promote") {
            Ok(s) => s,
            Err(res) => return Ok(res),
        };
        match self.call(&Request::Promote { selector }).await {
            // Reply with the MASKED preview, never the raw value: the point of
            // promote is that the value reaches the OS clipboard without passing
            // through the model.
            Ok(Response::Promoted { entry }) => {
                let audience = self.model_audience().await;
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Entry {} is now the current clipboard ({}) — the user can paste it \
with Ctrl/Cmd+V.",
                    addr(&entry),
                    preview(&entry, audience)
                ))]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Promote MULTIPLE existing clipboard entries in sequence — \
the batch form of `clipboard_promote`. Pass a list of selectors (short handles \
like \"c7\", indices like \"0\", or full ids); each is promoted in order, so they \
end up at the top of history in that order and the LAST one becomes the current \
OS clipboard. \
NOT a way to stage several values for a multi-field form: the OS clipboard holds \
exactly ONE value, so every earlier selector is overwritten by the next and only \
the last can be pasted. To fill N fields, promote and paste them one at a time."
    )]
    async fn clipboard_promote_many(
        &self,
        Parameters(a): Parameters<PromoteManyArgs>,
    ) -> Result<CallToolResult, McpError> {
        if a.selectors.is_empty() {
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                "`selectors` is empty — pass at least one selector to promote.",
            )]));
        }
        let mut promoted_ids = Vec::with_capacity(a.selectors.len());
        for (i, sel_str) in a.selectors.into_iter().enumerate() {
            let selector = match sel_str.parse::<usize>() {
                Ok(idx) => Selector::Index(idx),
                Err(_) => Selector::Id(sel_str),
            };
            match self.daemon.call(&Request::Promote { selector }).await {
                Ok(Response::Promoted { entry }) => promoted_ids.push(addr(&entry)),
                Ok(Response::Error { message }) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Promoted {} selector(s), then daemon reported error on selector #{}: {message}",
                        promoted_ids.len(),
                        i + 1
                    ))]))
                }
                Ok(other) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Promoted {} selector(s), then got unexpected response: {other:?}",
                        promoted_ids.len()
                    ))]))
                }
                Err(e) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Promoted {} selector(s), then daemon error: {e}",
                        promoted_ids.len()
                    ))]))
                }
            }
        }
        let msg = format!(
            "Promoted {} entry(ies) in order (ids: {}). The LAST entry (id {}) is now the current OS clipboard.",
            promoted_ids.len(),
            promoted_ids.join(", "),
            promoted_ids.last().map(String::as_str).unwrap_or("?")
        );
        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
    }

    #[tool(description = "Pin (or unpin) a clipboard entry so it is exempt from \
eviction/expiry — for a value the user will reuse across the session (an address, \
a reference code). Identify it by `index` (0 = current; from clipboard_history) or \
`id`.")]
    async fn clipboard_pin(
        &self,
        Parameters(a): Parameters<PinArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pinned = a.pinned.unwrap_or(true);
        let selector = match selector_from(a.index, a.id, "pin") {
            Ok(s) => s,
            Err(res) => return Ok(res),
        };
        match self.call(&Request::Pin { selector, pinned }).await {
            Ok(Response::Pinned { id, pinned }) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "{} entry {id}.",
                    if pinned { "Pinned" } else { "Unpinned" }
                ))]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Delete a specific clipboard entry by `index` (0 = current) or `id`. \
Explicit delete overrides a pin (unlike eviction). Option `channel` scopes to a named lane."
    )]
    async fn clipboard_delete(
        &self,
        Parameters(a): Parameters<DeleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let selector = match selector_from(a.index, a.id, "delete") {
            Ok(s) => s,
            Err(res) => return Ok(res),
        };
        match self
            .call(&Request::Delete {
                selector,
                channel: a.channel,
            })
            .await
        {
            Ok(Response::Deleted { id }) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format!("Deleted entry {id}."),
            )])),
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Bulk clear clipboard history entries. By default removes unpinned \
entries in the default OS lane. Set `include_pinned: true` to also clear pinned entries, and \
`all_channels: true` to clear across all channels/lanes."
    )]
    async fn clipboard_clear(
        &self,
        Parameters(a): Parameters<ClearArgs>,
    ) -> Result<CallToolResult, McpError> {
        let req = Request::Clear {
            include_pinned: a.include_pinned.unwrap_or(false),
            all_channels: a.all_channels.unwrap_or(false),
            channel: a.channel,
        };
        match self.call(&req).await {
            Ok(Response::Cleared { removed }) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Cleared {removed} entry(ies).",
                ))]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }

    #[tool(
        description = "Report clipboard daemon status: backend (real OS clipboard \
vs headless/shared), how many entries are held, and the device label. Useful to \
check the clipboard is connected before relying on it."
    )]
    async fn clipboard_status(&self) -> Result<CallToolResult, McpError> {
        match self.call(&Request::Status).await {
            Ok(Response::Status(s)) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "copycatd v{} on device '{}': backend = {}, {} entries held.",
                    s.version,
                    s.device,
                    s.clipboard.label(),
                    s.entries
                ))]))
            }
            Ok(other) => Ok(unexpected(other)),
            Err(res) => Ok(res),
        }
    }
}

/// Non-tool helpers, kept out of the `#[tool_router]` block so nothing here can
/// be mistaken for part of the tool surface.
impl Copycat {
    /// One daemon round-trip, with transport failures and daemon-reported errors
    /// mapped to ready-made error results so every tool surfaces them uniformly.
    async fn call(&self, req: &Request) -> Result<Response, CallToolResult> {
        match self.daemon.call(req).await {
            Ok(Response::Error { message }) => {
                Err(CallToolResult::error(vec![ContentBlock::text(message)]))
            }
            Ok(resp) => Ok(resp),
            Err(e) => Err(daemon_error(e)),
        }
    }

    /// Privacy mode is daemon-authoritative (the host env may differ from the
    /// daemon's); the env fallback applies only when the daemon is unreachable
    /// and never errs looser than the default.
    async fn model_audience(&self) -> Audience {
        let mode = match self.daemon.call(&Request::Status).await {
            Ok(Response::Status(s)) => s.privacy,
            _ => copycat_core::privacy::Mode::from_env(),
        };
        Audience::Model { mode }
    }
}

// rmcp's macro default rebuilds the router per call; point it at the instance
// built in `new()`.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Copycat {
    fn get_info(&self) -> ServerInfo {
        // rmcp marks these `#[non_exhaustive]`, so constructors, not literals.
        let mut implementation = Implementation::new("copycat", env!("CARGO_PKG_VERSION"));
        implementation.title = Some("copycat — shared clipboard".to_string());

        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS);
        info.server_info = implementation;
        info
    }
}

/// Full-value view for `clipboard_read_current`. Sensitivity is re-derived via
/// [`view::model_sensitivity`] rather than read off the entry: a privacy-off
/// daemon reports `Secret` entries as `Normal`, but a credential must never
/// reach the model in any posture.
fn read_view(e: &Entry, audience: Audience) -> String {
    let kind = format!("{:?}", e.kind);
    let sensitivity = view::model_sensitivity(e);
    if view::should_mask(sensitivity, audience) {
        let (what, hint) = match sensitivity {
            Sensitivity::Secret => (
                "SECRET",
                "The user can paste it themselves; do not ask me to reveal it.",
            ),
            // Only reachable under the strict posture; Normal is always revealed.
            _ => (
                "PII — strict privacy mode",
                "Personal data is withheld from the model here; the user can paste it themselves.",
            ),
        };
        return format!(
            "[{kind}, {what} — value withheld] A sensitive value is on the clipboard (id {}). {hint}",
            addr(e)
        );
    }
    match &e.content {
        EntryContent::Text(t) => format!("[{kind}] {t}"),
        EntryContent::Image { mime, .. } => format!("[image {mime}] (binary, not shown)"),
        EntryContent::Files(fs) => format!("[files] {}", fs.join(", ")),
    }
}

/// The address to hand the model for an entry. Prefer the short handle: it is
/// the one string the model has to retype.
fn addr_of(id: &str) -> String {
    copycat_core::entry::handle_of(id).unwrap_or_else(|| id.to_string())
}

/// [`addr_of`] for an entry in hand.
fn addr(e: &Entry) -> String {
    addr_of(&e.id)
}

/// Resolve the shared `index`/`id` argument pair, erroring when neither is set.
fn selector_from(
    index: Option<usize>,
    id: Option<String>,
    verb: &str,
) -> Result<Selector, CallToolResult> {
    match (index, id) {
        (Some(index), _) => Ok(Selector::Index(index)),
        (None, Some(id)) => Ok(Selector::Id(id)),
        (None, None) => Err(CallToolResult::error(vec![ContentBlock::text(format!(
            "provide either `index` or `id` to {verb}"
        ))])),
    }
}

/// Compact, safety-masked listing for `clipboard_history`.
fn history_view(entries: &[Entry], audience: Audience) -> String {
    if entries.is_empty() {
        return "Clipboard history is empty.".into();
    }
    let mut out = String::from("Recent clipboard entries (index 0 = current, newest first):\n");
    for (i, e) in entries.iter().enumerate() {
        let tag = with_leading_space(view::sensitivity_tag(view::model_sensitivity(e)));
        let author_tag = with_leading_space(&view::author_badge(
            e.source.author.as_deref(),
            e.source.session_id.as_deref(),
        ));
        let pin = if e.pinned { " 📌" } else { "" };
        out.push_str(&format!(
            "{i}. id={} {:?}{tag}{author_tag}{pin}: {}\n",
            addr(e),
            e.kind,
            preview(e, audience),
        ));
    }
    out
}

/// One-line preview for the model audience: `Secret` masked, `Pii` masked only
/// under `strict`.
fn preview(e: &Entry, audience: Audience) -> String {
    if view::should_mask(view::model_sensitivity(e), audience) {
        return view::MASK.to_string();
    }
    view::preview_text(&e.content)
}

fn with_leading_space(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        format!(" {s}")
    }
}

fn daemon_error(e: anyhow::Error) -> CallToolResult {
    // A refused socket is not an absent daemon; "start copycatd" advice would
    // displace the message naming the actual fix.
    if copycat_core::wire::is_connect_refusal(&e) {
        return CallToolResult::error(vec![ContentBlock::text(format!(
            "copycat refused to use its socket: {e}"
        ))]);
    }
    CallToolResult::error(vec![ContentBlock::text(format!(
        "copycat daemon unavailable: {e}. Start it with `copycatd`."
    ))])
}

fn unexpected(r: Response) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "unexpected daemon response: {r:?}"
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use copycat_core::entry::{ContentKind, Source};

    fn entry(text: &str) -> Entry {
        Entry::from_text("id1", text, 0, Source::simple("t"))
    }

    /// Bind at `0600`: `proxy::connect` runs `ipc::check_trust`, so a fake
    /// socket left at the default umask would be refused.
    #[cfg(unix)]
    fn bind_private(path: &std::path::Path) -> tokio::net::UnixListener {
        use std::os::unix::fs::PermissionsExt;
        let l = tokio::net::UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        l
    }

    #[cfg(unix)]
    fn temp_sock(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("copycat-mcp-{tag}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    /// One-shot fake daemon: accepts one connection per reply, reads the request
    /// line, answers in order, and returns the raw request lines for assertion.
    #[cfg(unix)]
    fn fake_daemon(
        path: &std::path::Path,
        replies: Vec<Response>,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = bind_private(path);
        tokio::spawn(async move {
            let mut lines = Vec::with_capacity(replies.len());
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let (rh, mut wh) = stream.into_split();
                let mut got = String::new();
                BufReader::new(rh).read_line(&mut got).await.unwrap();
                lines.push(got);
                let json = serde_json::to_string(&reply).unwrap();
                wh.write_all(json.as_bytes()).await.unwrap();
                wh.write_all(b"\n").await.unwrap();
                wh.flush().await.ok();
            }
            lines
        })
    }

    /// Serializes tests touching the process-global COPYCAT_INSECURE env that
    /// the view functions consult. Recovers from poisoning.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The default posture, which is what a daemon reports unless the user
    /// changed it. Tests that care about `strict` build their own audience.
    fn aud() -> Audience {
        Audience::Model {
            mode: copycat_core::privacy::Mode::Guarded,
        }
    }

    #[test]
    fn read_view_masks_secret_value() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let e = entry("ghp_0123456789abcdefghij");
        assert_eq!(e.sensitivity, Sensitivity::Secret);
        let v = read_view(&e, aud());
        assert!(v.contains("withheld"), "{v}");
        assert!(!v.contains("ghp_"), "secret must not appear: {v}");
    }

    #[test]
    #[cfg(feature = "debug-unsafe")]
    fn insecure_mode_reveals_secret_to_model() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("COPYCAT_INSECURE", "1");
        let v = read_view(&entry("ghp_0123456789abcdefghij"), aud());
        let p = history_view(&[entry("ghp_0123456789abcdefghij")], aud());
        std::env::remove_var("COPYCAT_INSECURE");
        assert!(
            v.contains("ghp_0123456789abcdefghij"),
            "read_view reveal: {v}"
        );
        assert!(
            p.contains("ghp_0123456789abcdefghij"),
            "history reveal: {p}"
        );
    }

    #[test]
    fn read_view_reveals_normal_text() {
        let v = read_view(&entry("just a normal note"), aud());
        assert!(v.contains("just a normal note"), "{v}");
    }

    #[test]
    fn read_view_reveals_pii_when_asked() {
        // PII (email) is shown on an explicit current-clipboard read.
        let e = entry("dean@example.com");
        assert_eq!(e.sensitivity, Sensitivity::Pii);
        assert!(read_view(&e, aud()).contains("dean@example.com"));
    }

    #[test]
    fn history_view_masks_sensitive_previews() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let out = history_view(&[entry("ghp_0123456789abcdefghij"), entry("hello")], aud());
        assert!(out.contains("[SECRET]"));
        assert!(out.contains("••••"));
        assert!(!out.contains("ghp_"), "raw secret leaked: {out}");
        assert!(out.contains("hello"), "normal entry shown: {out}");
    }

    #[test]
    fn history_view_empty_is_friendly() {
        assert_eq!(history_view(&[], aud()), "Clipboard history is empty.");
    }

    #[test]
    fn preview_truncates_long_text() {
        let e = entry(&"a".repeat(200));
        assert!(preview(&e, aud()).chars().count() <= 72);
        assert!(preview(&e, aud()).ends_with('…'));
    }

    #[test]
    fn instructions_cover_copy_and_read_intents() {
        assert!(INSTRUCTIONS.contains("WHEN TO COPY"));
        assert!(INSTRUCTIONS.contains("WHEN TO READ"));
        assert!(INSTRUCTIONS.contains("FORM-FILLING"));
        assert!(INSTRUCTIONS.contains("SAFETY"));
        assert!(INSTRUCTIONS.contains("AGENT HAND-OFF"));
    }

    #[test]
    fn wait_read_view_masks_secret_handoff() {
        // A secret handed off on a channel must still be masked to the model:
        // channels do NOT relax the Secret-never-to-LLM policy.
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let e = entry("ghp_0123456789abcdefghij");
        assert_eq!(e.sensitivity, Sensitivity::Secret);
        let v = read_view(&e, aud());
        assert!(!v.contains("ghp_"), "handoff must not echo a secret: {v}");
    }

    /// The model must be told the thing that isn't obvious: reading an entry does
    /// NOT make it pasteable — only promoting does. Without this the form-filling
    /// flow silently doesn't work for the user.
    #[test]
    fn instructions_teach_promote_as_the_pasteable_primitive() {
        assert!(INSTRUCTIONS.contains("clipboard_promote"));
        assert!(
            INSTRUCTIONS.contains("ONE value at a time"),
            "must explain why promote exists"
        );
        assert!(
            INSTRUCTIONS.contains("short handle"),
            "must steer to handles for human-paced flows, since indices shift"
        );
        assert!(
            INSTRUCTIONS.contains("indices shift"),
            "must say *why* a stable address beats an index"
        );
    }

    /// A promoted secret is confirmed to the model as a masked preview — the value
    /// reaches the OS clipboard without ever passing through the model.
    #[test]
    fn promote_confirmation_never_carries_the_raw_value() {
        let secret = entry("ghp_0123456789abcdefghij");
        assert_eq!(secret.sensitivity, Sensitivity::Secret);
        let shown = preview(&secret, aud());
        assert_eq!(shown, "••••••••");
        assert!(!shown.contains("ghp_"));
    }

    #[test]
    fn transform_shows_for_normal_but_secret_stays_masked() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Normal entry, transformed the way the daemon would (base64_encode):
        // read_view surfaces the transformed value.
        let mut normal = entry("hello world");
        assert_eq!(normal.sensitivity, Sensitivity::Normal);
        if let EntryContent::Text(t) = &normal.content {
            let encoded = Transform::Base64Encode.apply(t).unwrap();
            normal.content = EntryContent::Text(encoded);
        }
        let v = read_view(&normal, aud());
        assert!(
            v.contains("aGVsbG8gd29ybGQ="),
            "transformed value shown: {v}"
        );

        // Secret entry: even if the daemon applied a transform to its bytes,
        // read_view masks FIRST — neither the raw nor the transformed value leaks.
        let mut secret = entry("ghp_0123456789abcdefghij");
        assert_eq!(secret.sensitivity, Sensitivity::Secret);
        let encoded = Transform::Base64Encode
            .apply("ghp_0123456789abcdefghij")
            .unwrap();
        secret.content = EntryContent::Text(encoded.clone());
        let v = read_view(&secret, aud());
        assert!(v.contains("withheld"), "secret masked: {v}");
        assert!(!v.contains("ghp_"), "raw secret must not leak: {v}");
        assert!(
            !v.contains(&encoded),
            "transformed secret must not leak: {v}"
        );
    }

    fn result_json(r: &CallToolResult) -> String {
        serde_json::to_string(r).unwrap()
    }

    // The fake-daemon tests are unix-only; the named-pipe transport is covered
    // on Windows by copycatd's own suite.
    #[cfg(unix)]
    #[tokio::test]
    async fn copy_many_copies_each_value_in_order_and_reports_ids() {
        let path = temp_sock("copymany");
        let replies = (0..3)
            .map(|i| Response::Copied {
                id: format!("id{i}"),
            })
            .collect();
        let server = fake_daemon(&path, replies);

        let cat = Copycat::new(Daemon::new(path.clone()));
        let args = CopyManyArgs {
            texts: vec![
                "tim.mercer88".into(),
                "tim.mercer88@example.com".into(),
                "S3cret-pw".into(),
            ],
            channel: None,
            author: None,
            session_id: None,
        };
        let res = cat.clipboard_copy_many(Parameters(args)).await.unwrap();
        let json = result_json(&res);
        assert!(
            json.contains("id0, id1, id2"),
            "ids listed in order: {json}"
        );
        assert!(
            json.contains("id id2"),
            "last value is the current clipboard: {json}"
        );

        let texts: Vec<String> = server
            .await
            .unwrap()
            .iter()
            .map(|l| match serde_json::from_str(l.trim_end()).unwrap() {
                Request::Copy { text, .. } => text,
                other => panic!("expected Copy, got {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            vec!["tim.mercer88", "tim.mercer88@example.com", "S3cret-pw"],
            "each value copied once, in order"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn copy_many_rejects_empty_texts_without_touching_daemon() {
        // No daemon is bound at this path; the empty-input guard must short-circuit
        // before any connection is attempted.
        let cat = Copycat::new(Daemon::new(std::env::temp_dir().join("copycat-nope.sock")));
        let res = cat
            .clipboard_copy_many(Parameters(CopyManyArgs {
                texts: vec![],
                channel: None,
                author: None,
                session_id: None,
            }))
            .await
            .unwrap();
        let json = result_json(&res);
        assert!(
            json.contains("empty"),
            "empty texts is a user error: {json}"
        );
        assert_eq!(res.is_error, Some(true), "reported as an error: {json}");
    }

    #[test]
    fn session_id_is_stable_for_the_life_of_the_server() {
        // Every entry a session writes must carry the SAME id, or history can't
        // be grouped by conversation.
        let cat = Copycat::new(Daemon::new(std::env::temp_dir().join("copycat-nope.sock")));
        let first = cat.session_id.clone();
        assert_eq!(cat.session_id, first);
        assert!(!first.is_empty(), "a session must always have an id");
    }

    #[test]
    fn derived_session_ids_are_shaped_for_display() {
        // Only meaningful without the env override in play.
        if std::env::var(SESSION_ENV).is_ok() {
            return;
        }
        let id = derive_session_id();
        assert!(id.starts_with("sess_"), "recognisable prefix: {id}");
        // `view::session_label` strips the prefix; what's left must be non-empty
        // and short enough to sit in a row without truncation.
        let label = view::session_label(&id);
        assert!(!label.is_empty(), "renders as something: {id}");
        assert!(!label.contains('…'), "fits the chip uncut: {label}");
    }

    #[test]
    fn mcp_default_author_reads_as_an_agent() {
        use copycat_core::entry::Origin;
        assert_eq!(Origin::of(Some(AUTHOR_MCP)), Origin::Agent);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_stamps_agent_author_and_session_when_the_model_gives_none() {
        let path = temp_sock("prov");
        let server = fake_daemon(&path, vec![Response::Copied { id: "i1".into() }]);

        let cat = Copycat::new(Daemon::new(path.clone()));
        let expected_session = cat.session_id.clone();
        cat.clipboard_copy(Parameters(CopyArgs {
            text: "generated value".into(),
            channel: None,
            author: None,
            session_id: None,
        }))
        .await
        .unwrap();

        let lines = server.await.unwrap();
        let Request::Copy {
            author, session_id, ..
        } = serde_json::from_str(lines[0].trim_end()).unwrap()
        else {
            panic!("expected Copy")
        };
        assert_eq!(author.as_deref(), Some(AUTHOR_MCP));
        assert_eq!(session_id.as_deref(), Some(expected_session.as_str()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copy_keeps_the_identity_the_model_declared() {
        let path = temp_sock("prov2");
        let server = fake_daemon(&path, vec![Response::Copied { id: "i1".into() }]);

        let cat = Copycat::new(Daemon::new(path.clone()));
        cat.clipboard_copy(Parameters(CopyArgs {
            text: "v".into(),
            channel: None,
            author: Some("agent:claude-3-5".into()),
            session_id: Some("sess_explicit".into()),
        }))
        .await
        .unwrap();

        let lines = server.await.unwrap();
        let Request::Copy {
            author, session_id, ..
        } = serde_json::from_str(lines[0].trim_end()).unwrap()
        else {
            panic!("expected Copy")
        };
        assert_eq!(author.as_deref(), Some("agent:claude-3-5"));
        assert_eq!(session_id.as_deref(), Some("sess_explicit"));
    }

    #[test]
    fn files_entry_preview_counts() {
        let e = Entry {
            id: "f".into(),
            content: EntryContent::Files(vec!["/a".into(), "/b".into()]),
            kind: ContentKind::Files,
            copied_at: 0,
            source: Source::simple("t"),
            sensitivity: Sensitivity::Normal,
            pinned: false,
        };
        assert!(preview(&e, aud()).contains("2 file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_and_clear_proxy_to_daemon() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let mut path = std::env::temp_dir();
        path.push(format!("copycat-mcp-delclear-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = bind_private(&path);

        let server = tokio::spawn(async move {
            let mut ops = Vec::new();
            // First call: delete
            let (stream, _) = listener.accept().await.unwrap();
            let (rh, mut wh) = stream.into_split();
            let mut reader = BufReader::new(rh);
            let mut got = String::new();
            reader.read_line(&mut got).await.unwrap();
            ops.push(got.clone());
            let reply = serde_json::to_string(&Response::Deleted {
                id: "item-1".into(),
            })
            .unwrap();
            wh.write_all(reply.as_bytes()).await.unwrap();
            wh.write_all(b"\n").await.unwrap();

            // Second call: clear
            let (stream, _) = listener.accept().await.unwrap();
            let (rh, mut wh) = stream.into_split();
            let mut reader = BufReader::new(rh);
            let mut got = String::new();
            reader.read_line(&mut got).await.unwrap();
            ops.push(got.clone());
            let reply = serde_json::to_string(&Response::Cleared { removed: 42 }).unwrap();
            wh.write_all(reply.as_bytes()).await.unwrap();
            wh.write_all(b"\n").await.unwrap();

            ops
        });

        let cat = Copycat::new(Daemon::new(path.clone()));
        let res_del = cat
            .clipboard_delete(Parameters(DeleteArgs {
                index: Some(2),
                id: None,
                channel: None,
            }))
            .await
            .unwrap();
        assert!(result_json(&res_del).contains("Deleted entry item-1"));

        let res_clr = cat
            .clipboard_clear(Parameters(ClearArgs {
                include_pinned: Some(true),
                all_channels: Some(false),
                channel: None,
            }))
            .await
            .unwrap();
        assert!(result_json(&res_clr).contains("Cleared 42 entry(ies)"));

        let ops = server.await.unwrap();
        assert!(ops[0].contains("\"op\":\"delete\""));
        assert!(ops[1].contains("\"op\":\"clear\""));
        assert!(ops[1].contains("\"include_pinned\":true"));

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn promote_many_promotes_each_selector_in_order() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let mut path = std::env::temp_dir();
        path.push(format!(
            "copycat-mcp-promotemany-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = bind_private(&path);

        let server = tokio::spawn(async move {
            let mut selectors = Vec::new();
            for i in 0..2u32 {
                let (stream, _) = listener.accept().await.unwrap();
                let (rh, mut wh) = stream.into_split();
                let mut reader = BufReader::new(rh);
                let mut got = String::new();
                reader.read_line(&mut got).await.unwrap();
                if let Request::Promote { selector } = serde_json::from_str(got.trim_end()).unwrap()
                {
                    selectors.push(selector);
                }
                let dummy_entry =
                    Entry::from_text(format!("id-{i}"), "val", 0, Source::simple("t"));
                let reply =
                    serde_json::to_string(&Response::Promoted { entry: dummy_entry }).unwrap();
                wh.write_all(reply.as_bytes()).await.unwrap();
                wh.write_all(b"\n").await.unwrap();
            }
            selectors
        });

        let cat = Copycat::new(Daemon::new(path.clone()));
        let res = cat
            .clipboard_promote_many(Parameters(PromoteManyArgs {
                selectors: vec!["0".into(), "id-0".into()],
            }))
            .await
            .unwrap();
        let json = result_json(&res);
        assert!(json.contains("Promoted 2 entry(ies) in order"), "{json}");
        assert!(json.contains("id-1"), "last entry id reported: {json}");

        let selectors = server.await.unwrap();
        assert_eq!(selectors.len(), 2);
        assert_eq!(selectors[0], Selector::Index(0));
        assert_eq!(selectors[1], Selector::Id("id-0".into()));

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn promote_places_on_clipboard_without_echoing_raw_value() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let mut path = std::env::temp_dir();
        path.push(format!("copycat-mcp-noecho-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = bind_private(&path);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rh, mut wh) = stream.into_split();
            let mut reader = BufReader::new(rh);
            let mut got = String::new();
            reader.read_line(&mut got).await.unwrap();

            // Return a secret entry that was promoted daemon-side
            let secret_entry = Entry::from_text(
                "secret-id-99",
                "ghp_VERY_SECRET_TOKEN_DO_NOT_ECHO_TO_LLM",
                0,
                Source::simple("t"),
            );
            let reply = serde_json::to_string(&Response::Promoted {
                entry: secret_entry,
            })
            .unwrap();
            wh.write_all(reply.as_bytes()).await.unwrap();
            wh.write_all(b"\n").await.unwrap();
        });

        let cat = Copycat::new(Daemon::new(path.clone()));
        let res = cat
            .clipboard_promote(Parameters(PromoteArgs {
                index: Some(0),
                id: None,
            }))
            .await
            .unwrap();
        let json = result_json(&res);

        // Prove 1: The response confirms current clipboard status and directs Ctrl/Cmd+V
        assert!(
            json.contains("is now the current clipboard"),
            "must confirm clipboard status: {json}"
        );
        assert!(
            json.contains("paste it with Ctrl/Cmd+V"),
            "must direct Ctrl+V paste: {json}"
        );

        // Prove 2: The raw secret value is NEVER returned in the MCP response payload
        assert!(
            !json.contains("VERY_SECRET_TOKEN"),
            "raw secret must NOT leak to LLM: {json}"
        );
        assert!(json.contains("••••••••"), "secret must be masked: {json}");

        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
