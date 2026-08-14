//! Background daemon polling: a dedicated thread round-trips `Recall` + `Status`
//! about once a second (blocking the egui loop on socket I/O would stutter it),
//! pushes an [`Update`] down an mpsc channel, and requests a repaint so the UI
//! drains it promptly.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use copycat_core::ipc;
use copycat_core::proto::{DaemonStatus, Request, Response};
use copycat_core::store::Recall;

use crate::rows::{rows_from_entries, Row};

/// How many recent entries the window shows. 0 would mean "all"; we cap it so a
/// huge history doesn't build an unbounded row list every second.
pub const RECALL_LIMIT: usize = 200;

/// Poll cadence.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A snapshot delivered from the poll thread to the UI.
pub enum Update {
    /// A fresh snapshot of history + status.
    Data {
        rows: Vec<Row>,
        status: Option<DaemonStatus>,
    },
    /// The daemon was unreachable or replied with an error, with a message to
    /// surface in the status bar. The UI keeps showing the last good data.
    Error(String),
}

/// Fetch a snapshot with configurable recall limit and channel lane.
pub fn fetch_with_options(socket: &Path, limit: usize, channel: Option<&str>) -> Update {
    let recall = Request::Recall {
        query: Recall {
            limit,
            channel: channel.map(|s| s.to_string()),
            ..Default::default()
        },
    };
    let rows = match ipc::roundtrip(socket, &recall) {
        Ok(Response::Entries { entries }) => rows_from_entries(&entries),
        Ok(Response::Error { message }) => return Update::Error(message),
        Ok(other) => return Update::Error(format!("unexpected reply to recall: {other:?}")),
        Err(e) => return Update::Error(format!("{e}")),
    };
    let status = match ipc::roundtrip(socket, &Request::Status) {
        Ok(Response::Status(s)) => Some(s),
        // A missing/failed status shouldn't blank the history we just got.
        _ => None,
    };
    Update::Data { rows, status }
}

/// Spawn the polling thread. It stops on its own once the [`Receiver`] is dropped
/// (i.e. the UI has quit), since the send then errors.
pub fn spawn(socket: PathBuf, ctx: egui::Context) -> Receiver<Update> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("copycat-ui-poll".into())
        .spawn(move || run(socket, ctx, tx))
        .expect("spawn poll thread");
    rx
}

fn run(socket: PathBuf, ctx: egui::Context, tx: Sender<Update>) {
    loop {
        let update = fetch_with_options(&socket, RECALL_LIMIT, None);
        if tx.send(update).is_err() {
            // UI gone — nothing left to notify.
            return;
        }
        // Wake the UI so it drains the channel now rather than at the next frame.
        ctx.request_repaint();
        std::thread::sleep(POLL_INTERVAL);
    }
}
