//! Writes the usage log: one JSON line per handled request.
//!
//! # Why this cannot be a plain `writeln!` on the request path
//!
//! Every request the daemon handles goes through one dispatch point, and that
//! point is on the latency path for `copycat paste` and for every model tool
//! call. A synchronous append there would put a filesystem round-trip — and, on
//! a slow or full disk, an unbounded stall — between a Ctrl+V and the value
//! arriving. Instrumentation that changes the thing it measures is worse than no
//! instrumentation.
//!
//! So the request path does exactly one thing: `try_send` on a **bounded**
//! channel. If the channel is full it drops the record and increments a counter,
//! and the next record written carries that counter. The log is therefore honest
//! about its own gaps rather than silently incomplete — which matters, because
//! the whole point is to reason about *rates*, and a log that quietly loses
//! records under load would understate exactly the busy periods worth studying.
//!
//! # Rotation
//!
//! One file per UTC day, oldest deleted past the retention window. Nothing else
//! in copycat rotates — `copycatd.log` grows forever — and this is intended to
//! run for weeks, so it is bounded by construction rather than by remembering.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

use serde::Serialize;

use copycat_core::entry::{Entry, Source};
use copycat_core::proto::{Request, Response};
use copycat_core::usage::{self, Level};
use copycat_core::view::{self, Audience};

/// How many records may queue before the request path starts dropping them.
///
/// Sized so a burst (a `copy_many`, an agent filling a form) never blocks, while
/// a genuinely stuck writer costs bounded memory rather than growing until the
/// daemon is killed.
const QUEUE_CAP: usize = 1024;

/// One handled request, as it appears in the log.
///
/// Field names are short because this file is read by `jq` over a week of
/// records, and long enough to be unambiguous. Optional fields are omitted
/// entirely when absent so a line stays readable by eye.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    /// RFC3339 UTC, so lines sort lexically and `jq` can compare them as strings.
    pub ts: String,
    /// Operation name — the same string the daemon already logs (`copy`,
    /// `promote`, `current`, …).
    pub op: &'static str,
    /// How long the daemon spent handling it. The performance question.
    pub us: u64,
    /// Provenance of the entry involved, when there is one: `human:os`,
    /// `agent:claude`, … This is the "where is it being used" axis.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Session that asked, when the caller supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Detected content kind (`PlainText`, `Url`, `OneTimeCode`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Sensitivity as classified on copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sens: Option<String>,
    /// Payload length in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<usize>,
    /// Estimated tokens the payload would cost a model to emit. With `op` and
    /// `author` this is what makes the counterfactual — "what if copycat were
    /// not here" — computable after the fact rather than guessed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_tokens: Option<u32>,
    /// Whether referring to this value cost less than emitting it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub above_break_even: Option<bool>,
    /// Hand-off lane, when not the default clipboard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Short model-facing handle for the entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// Masked preview. Only ever present at `Level::Preview`, and always routed
    /// through `view::preview` — a `Secret` is the mask here, never the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Records dropped since the last line, because the queue was full. Omitted
    /// when zero, so its presence is a signal rather than noise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<u64>,
}

/// Handle held by the daemon. Cloneable, cheap, and safe to call from the
/// request path.
#[derive(Clone)]
pub struct UsageLog {
    tx: Option<SyncSender<Record>>,
    dropped: Arc<AtomicU64>,
}

impl UsageLog {
    /// A log that discards everything — what `Level::Off` gets, and what tests
    /// and headless runs get for free. No file is created, no thread spawned.
    pub fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Start the writer thread, appending daily files under `dir`.
    ///
    /// Failure to create the directory is logged and degrades to
    /// [`UsageLog::disabled`]: a clipboard that stops working because its
    /// telemetry could not open a file would be an absurd trade.
    pub fn spawn(dir: PathBuf, retention_days: u16) -> Self {
        if let Err(e) = fs::create_dir_all(&dir) {
            tracing::warn!("usage log disabled: cannot create {}: {e}", dir.display());
            return Self::disabled();
        }
        let (tx, rx) = sync_channel::<Record>(QUEUE_CAP);
        let d = dir.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("copycat-usage".into())
            .spawn(move || writer_loop(d, retention_days, rx))
        {
            tracing::warn!("usage log disabled: cannot spawn writer: {e}");
            return Self::disabled();
        }
        tracing::info!(dir = %dir.display(), retention_days, "usage log enabled");
        Self {
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Record one handled request. **Never blocks and never fails.**
    ///
    /// Called from the request path, so the only acceptable behaviours are
    /// "enqueue" and "drop and count". Anything that could wait belongs on the
    /// writer thread.
    pub fn record(&self, mut rec: Record) {
        let Some(tx) = &self.tx else { return };
        // Attach and clear the drop count, so the gap is attributed to the line
        // that follows it rather than being lost.
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        rec.dropped = (dropped > 0).then_some(dropped);
        match tx.try_send(rec) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                // Put back what we took, plus this one.
                self.dropped.fetch_add(dropped + 1, Ordering::Relaxed);
            }
        }
    }
}

/// Owns the file handle and does every blocking thing.
fn writer_loop(dir: PathBuf, retention_days: u16, rx: Receiver<Record>) {
    let mut open: Option<(String, BufWriter<File>)> = None;
    prune(&dir, retention_days);

    // `recv` blocks until a record arrives or every sender is gone, so an idle
    // daemon costs nothing — no timer, no polling. Each wake then drains
    // whatever else has queued and flushes once, so a burst costs one syscall
    // rather than one per line.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        for rec in batch {
            let day = rec.ts.get(..10).unwrap_or("unknown").to_string();
            // Day changed (or first record): roll the file and prune the old ones.
            if open.as_ref().is_none_or(|(d, _)| *d != day) {
                if let Some((_, mut w)) = open.take() {
                    let _ = w.flush();
                }
                match OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(format!("usage-{day}.jsonl")))
                {
                    Ok(f) => open = Some((day, BufWriter::new(f))),
                    Err(e) => {
                        tracing::warn!("usage log: cannot open today's file: {e}");
                        continue;
                    }
                }
                prune(&dir, retention_days);
            }
            let Some((_, w)) = open.as_mut() else {
                continue;
            };
            match serde_json::to_string(&rec) {
                Ok(line) => {
                    let _ = writeln!(w, "{line}");
                }
                // A record that will not serialise must not kill the writer.
                Err(e) => tracing::warn!("usage log: cannot serialise record: {e}"),
            }
        }
        // One flush per drained batch. The daemon is long-lived and usually
        // killed rather than shut down, so a buffer that only flushed at exit
        // would lose data; flushing per batch keeps the log durable within
        // microseconds of going idle without a syscall per line.
        if let Some((_, w)) = open.as_mut() {
            let _ = w.flush();
        }
    }
    if let Some((_, mut w)) = open.take() {
        let _ = w.flush();
    }
}

/// Delete `usage-YYYY-MM-DD.jsonl` files older than the retention window.
///
/// Compares the date encoded in the filename rather than the file's mtime:
/// copying or restoring a directory rewrites mtimes, and the day a record
/// describes is the thing retention is actually about.
fn prune(dir: &Path, retention_days: u16) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut days: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let day = name.strip_prefix("usage-")?.strip_suffix(".jsonl")?;
            (day.len() == 10).then(|| day.to_string())
        })
        .collect();
    if days.len() <= retention_days as usize {
        return;
    }
    // ISO dates sort lexically, so the oldest are simply the first.
    days.sort();
    let cutoff = days.len() - retention_days as usize;
    for day in &days[..cutoff] {
        let path = dir.join(format!("usage-{day}.jsonl"));
        if let Err(e) = fs::remove_file(&path) {
            tracing::warn!("usage log: cannot prune {}: {e}", path.display());
        }
    }
}

/// Build the record for a handled request, or `None` at [`Level::Off`].
///
/// Pure: everything it needs is passed in, so what gets written — and, more
/// importantly, what does **not** — is testable without a daemon, a socket or a
/// disk. The masking question in particular deserves a test rather than a
/// reviewer's confidence.
///
/// The entry it reads is the one already on its way back to the caller, so it
/// has been through `privacy::project` and carries whatever masking the daemon's
/// posture dictates. Previews go through `view::preview` on that projected
/// entry, never the raw store value.
pub fn record_for(
    op: &'static str,
    us: u64,
    now_rfc3339: String,
    req: &Request,
    resp: &Response,
    level: Level,
) -> Option<Record> {
    if !level.records() {
        return None;
    }
    let mut rec = Record {
        ts: now_rfc3339,
        op,
        us,
        author: None,
        session: None,
        kind: None,
        sens: None,
        bytes: None,
        est_tokens: None,
        above_break_even: None,
        channel: None,
        handle: None,
        preview: None,
        dropped: None,
    };

    // Request side: the lane, and who claimed to be asking.
    match req {
        Request::Copy {
            channel,
            author,
            session_id,
            ..
        } => {
            rec.channel.clone_from(channel);
            rec.author.clone_from(author);
            rec.session.clone_from(session_id);
        }
        Request::Recall { query } => rec.channel.clone_from(&query.channel),
        Request::Wait { channel, .. } => rec.channel.clone_from(channel),
        _ => {}
    }

    // Response side: the entry actually involved, when there is one.
    let entry = match resp {
        Response::Promoted { entry } | Response::Handoff { entry } => Some(entry),
        Response::Current { entry } => entry.as_ref(),
        // A recall's cost is the whole page, not one entry; the row count is the
        // interesting figure and `bytes` below covers the volume.
        Response::Entries { entries } => entries.first(),
        _ => None,
    };
    if let Some(e) = entry {
        rec.kind = Some(format!("{:?}", e.kind));
        rec.sens = Some(format!("{:?}", e.sensitivity));
        rec.handle = e.handle();
        // Provenance from the entry wins over anything the request claimed: it
        // is what the daemon actually stamped.
        if e.source.author.is_some() {
            rec.author.clone_from(&e.source.author);
        }
        if e.source.session_id.is_some() {
            rec.session.clone_from(&e.source.session_id);
        }
        if let Some(text) = e.as_text() {
            rec.bytes = Some(text.len());
            rec.est_tokens = Some(usage::estimate_tokens(text));
            rec.above_break_even = Some(usage::above_break_even(text));
            if level.writes_previews() {
                rec.preview = Some(view::preview(e, Audience::Human));
            }
        }
    } else if let Request::Copy { text, .. } = req {
        // A copy's payload never comes back in the response — `Copied` carries
        // only an id — so it is measured on the way in. Classifying here costs a
        // scan, but without it a copy record could say how MUCH was written and
        // never what KIND, which is half the question the log exists to answer.
        // Reuses `Entry::from_text` rather than calling the classifier directly
        // so the log's idea of "kind" and "sensitivity" cannot drift from the
        // store's.
        let classified = Entry::from_text("", text.as_str(), 0, Source::simple(""));
        rec.kind = Some(format!("{:?}", classified.kind));
        rec.sens = Some(format!("{:?}", classified.sensitivity));
        rec.bytes = Some(text.len());
        rec.est_tokens = Some(usage::estimate_tokens(text));
        rec.above_break_even = Some(usage::above_break_even(text));
        if level.writes_previews() {
            rec.preview = Some(view::preview(&classified, Audience::Human));
        }
    }
    Some(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(text: &str) -> Entry {
        Entry::from_text("1785315779365-000022", text, 0, Source::simple("test"))
    }

    /// The rule this whole feature lives or dies by. A durable file has none of
    /// the protections the socket has, so a credential must never reach it —
    /// at any level, including the one whose entire purpose is writing values.
    #[test]
    fn a_secret_is_never_written_even_at_preview_level() {
        let secret = "ghp_0123456789abcdefghij";
        let e = entry_with(secret);
        assert_eq!(
            e.sensitivity,
            copycat_core::entry::Sensitivity::Secret,
            "fixture must actually classify as Secret or this test proves nothing"
        );
        let resp = Response::Promoted { entry: e };
        let rec = record_for(
            "promote",
            10,
            "2026-07-30T10:00:00Z".into(),
            &Request::Current,
            &resp,
            Level::Preview,
        )
        .unwrap();
        let line = serde_json::to_string(&rec).unwrap();
        assert!(!line.contains(secret), "secret leaked into the log: {line}");
        assert!(
            !line.contains("ghp_"),
            "even the prefix must not appear: {line}"
        );
        // The metadata still lands — that is the point of recording at all.
        assert_eq!(rec.sens.as_deref(), Some("Secret"));
        assert!(rec.est_tokens.unwrap() > 0);
    }

    #[test]
    fn meta_level_writes_no_preview_for_ordinary_text_either() {
        let resp = Response::Promoted {
            entry: entry_with("perfectly ordinary text"),
        };
        let rec = record_for(
            "promote",
            10,
            "2026-07-30T10:00:00Z".into(),
            &Request::Current,
            &resp,
            Level::Meta,
        )
        .unwrap();
        assert_eq!(rec.preview, None, "meta must stay content-free");
        let line = serde_json::to_string(&rec).unwrap();
        assert!(
            !line.contains("ordinary"),
            "value leaked at meta level: {line}"
        );
    }

    #[test]
    fn preview_level_does_write_ordinary_values() {
        // Otherwise the level would be pointless.
        let resp = Response::Promoted {
            entry: entry_with("perfectly ordinary text"),
        };
        let rec = record_for(
            "promote",
            10,
            "2026-07-30T10:00:00Z".into(),
            &Request::Current,
            &resp,
            Level::Preview,
        )
        .unwrap();
        assert!(rec.preview.unwrap().contains("ordinary"));
    }

    #[test]
    fn off_records_nothing_at_all() {
        let resp = Response::Promoted {
            entry: entry_with("x"),
        };
        assert!(record_for(
            "promote",
            10,
            "2026-07-30T10:00:00Z".into(),
            &Request::Current,
            &resp,
            Level::Off,
        )
        .is_none());
    }

    #[test]
    fn a_copy_is_measured_on_the_way_in() {
        // Copy's payload never comes back in the response, so it has to be read
        // from the request or the volume written is invisible.
        let text = "2yW4Acq9GFz6Y1t9EwL56nGisiWgNZq6ITZM5jtgUe52RvEJgwBuNO6n9JEC3Hqd";
        let req = Request::Copy {
            text: text.into(),
            channel: None,
            author: Some("agent:claude".into()),
            session_id: Some("s1".into()),
        };
        let resp = Response::Copied {
            id: "1785315779365-000022".into(),
        };
        let rec = record_for(
            "copy",
            5,
            "2026-07-30T10:00:00Z".into(),
            &req,
            &resp,
            Level::Meta,
        )
        .unwrap();
        assert_eq!(rec.bytes, Some(text.len()));
        assert_eq!(rec.author.as_deref(), Some("agent:claude"));
        assert_eq!(rec.session.as_deref(), Some("s1"));
        assert_eq!(
            rec.above_break_even,
            Some(true),
            "a 64-char opaque token pays"
        );
    }

    #[test]
    fn entry_provenance_beats_what_the_request_claimed() {
        // The daemon stamps `human:os` for a real Ctrl+C; a client must not be
        // able to make the log say otherwise.
        let mut e = entry_with("hello");
        e.source.author = Some("human:os".into());
        let req = Request::Copy {
            text: "hello".into(),
            channel: None,
            author: Some("agent:liar".into()),
            session_id: None,
        };
        let resp = Response::Promoted { entry: e };
        let rec = record_for(
            "promote",
            1,
            "2026-07-30T10:00:00Z".into(),
            &req,
            &resp,
            Level::Meta,
        )
        .unwrap();
        assert_eq!(rec.author.as_deref(), Some("human:os"));
    }

    fn rec(ts: &str) -> Record {
        Record {
            ts: ts.to_string(),
            op: "copy",
            us: 12,
            author: Some("agent:claude".into()),
            session: None,
            kind: Some("PlainText".into()),
            sens: Some("Normal".into()),
            bytes: Some(4),
            est_tokens: Some(2),
            above_break_even: Some(false),
            channel: None,
            handle: Some("c1".into()),
            preview: None,
            dropped: None,
        }
    }

    #[test]
    fn a_disabled_log_never_touches_the_disk() {
        let log = UsageLog::disabled();
        // Would panic or block if it tried to do anything real.
        for _ in 0..10_000 {
            log.record(rec("2026-07-30T10:00:00Z"));
        }
    }

    #[test]
    fn dropping_is_counted_and_reported_rather_than_hidden() {
        // A queue of 1 with nothing draining it: everything after the first is
        // dropped, and the count must survive to be attached to a later line.
        let (tx, rx) = sync_channel::<Record>(1);
        let log = UsageLog {
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
        };
        for _ in 0..5 {
            log.record(rec("2026-07-30T10:00:00Z"));
        }
        assert_eq!(
            log.dropped.load(Ordering::Relaxed),
            4,
            "4 of 5 should have been dropped and counted"
        );
        // Draining lets the next record through, carrying the gap with it.
        let _ = rx.recv().unwrap();
        log.record(rec("2026-07-30T10:00:01Z"));
        let carried = rx.recv().unwrap();
        assert_eq!(carried.dropped, Some(4), "the gap must be reported");
        assert_eq!(log.dropped.load(Ordering::Relaxed), 0, "and then cleared");
    }

    #[test]
    fn prune_keeps_the_newest_and_deletes_by_filename_date() {
        let dir = std::env::temp_dir().join(format!("cc-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for day in ["2026-07-01", "2026-07-02", "2026-07-03", "2026-07-04"] {
            File::create(dir.join(format!("usage-{day}.jsonl"))).unwrap();
        }
        // An unrelated file must survive: this prunes its own logs, not the dir.
        File::create(dir.join("notes.txt")).unwrap();

        prune(&dir, 2);

        let mut left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "notes.txt".to_string(),
                "usage-2026-07-03.jsonl".to_string(),
                "usage-2026-07-04.jsonl".to_string(),
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_is_a_noop_below_the_retention_window() {
        let dir = std::env::temp_dir().join(format!("cc-usage-noop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        File::create(dir.join("usage-2026-07-01.jsonl")).unwrap();
        prune(&dir, 30);
        assert!(dir.join("usage-2026-07-01.jsonl").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_serialises_without_its_empty_fields() {
        let line = serde_json::to_string(&rec("2026-07-30T10:00:00Z")).unwrap();
        assert!(line.contains(r#""op":"copy""#));
        assert!(line.contains(r#""us":12"#));
        // Absent fields are omitted entirely, not written as null — a week of
        // these is read by eye as often as by jq.
        assert!(!line.contains("null"), "no nulls in the log: {line}");
        assert!(!line.contains("preview"), "meta records carry no preview");
        assert!(
            !line.contains("dropped"),
            "no drop key when nothing dropped"
        );
    }
}
