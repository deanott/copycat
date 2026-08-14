//! OS-clipboard backend + watch thread.
//!
//! The arboard handle is created inside, and solely owned by, one dedicated OS
//! thread, woken only by [`ClipCmd`]s on a single mpsc channel — that is what
//! keeps writes and change-processing atomic with respect to each other. When
//! no display is reachable, arboard fails to initialise and the daemon runs
//! **headless**: the store is authoritative and purely socket-driven.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use copycat_core::proto::ClipboardBackendInfo;

use crate::state::State;

/// Change-detection poll interval — `arboard` has no change signal. Copies
/// closer together than this collapse into one; the second wins.
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// Everything that can wake the clipboard thread.
#[derive(Debug)]
pub enum ClipCmd {
    /// A front-end asked to put this text on the real OS clipboard.
    Write(String),
    /// The OS clipboard changed and should be re-read (no payload — change
    /// notifications say *that* it changed, never what to). Constructed only by
    /// tests, as the deterministic driver for the `watch_loop` suite.
    #[allow(dead_code)]
    OsChanged,
}

/// The subset of a clipboard this loop needs, so tests can drive it without a
/// display (CI has none, and `arboard::Clipboard::new()` fails there).
pub trait TextClipboard {
    fn get_text(&mut self) -> anyhow::Result<String>;
    fn set_text(&mut self, text: String) -> anyhow::Result<()>;
}

impl TextClipboard for arboard::Clipboard {
    fn get_text(&mut self) -> anyhow::Result<String> {
        Ok(arboard::Clipboard::get_text(self)?)
    }
    fn set_text(&mut self, text: String) -> anyhow::Result<()> {
        arboard::Clipboard::set_text(self, text)?;
        Ok(())
    }
}

/// Where observed clipboard text goes. Exists so the loop can be tested against
/// a recording fake instead of a whole [`State`].
pub trait Ingest {
    fn ingest_from_os(&self, text: &str);
}

impl Ingest for State {
    fn ingest_from_os(&self, text: &str) {
        State::ingest_from_os(self, text);
    }
}

/// The daemon's link to the OS clipboard: a channel to drive the clipboard
/// thread, plus the backend info reported by `status`.
pub struct OsLink {
    pub sender: Sender<ClipCmd>,
    pub info: ClipboardBackendInfo,
    /// Consumed by [`spawn_watch`]; `Some` only when an OS clipboard attached.
    rx: Option<Receiver<ClipCmd>>,
}

impl OsLink {
    /// The headless link: no OS clipboard, nothing to write back to.
    pub fn headless() -> Self {
        // A dangling sender no one reads from — `record_copy` still works, the
        // send is simply best-effort and dropped.
        let (sender, rx) = std::sync::mpsc::channel();
        drop(rx);
        OsLink {
            sender,
            info: ClipboardBackendInfo::Headless,
            rx: None,
        }
    }
}

/// Probe for an OS clipboard and, if present, prepare the link. The watch thread
/// itself is started by [`spawn_watch`] once `State` exists.
pub fn attach() -> OsLink {
    match arboard::Clipboard::new() {
        Ok(_probe) => {
            // Probe succeeded (dropped immediately); the thread makes its own.
            let (sender, rx) = std::sync::mpsc::channel();
            tracing::info!("attached OS clipboard via arboard");
            OsLink {
                sender,
                info: ClipboardBackendInfo::Os {
                    name: "arboard".into(),
                },
                rx: Some(rx),
            }
        }
        Err(e) => {
            tracing::info!("no OS clipboard ({e}); running headless (socket-only)");
            OsLink::headless()
        }
    }
}

/// Start the watch/write thread if an OS clipboard attached. No-op when headless.
pub fn spawn_watch(link: &mut OsLink, state: Arc<State>) {
    let Some(rx) = link.rx.take() else { return };
    std::thread::Builder::new()
        .name("copycat-clipboard".into())
        .spawn(move || match arboard::Clipboard::new() {
            Ok(mut clipboard) => watch_loop(&*state, &mut clipboard, &rx, POLL_INTERVAL),
            Err(e) => tracing::error!("clipboard vanished after probe: {e}"),
        })
        .expect("spawn clipboard thread");
}

/// What we believe is on the OS clipboard, and *why* we believe it.
///
/// The distinction matters: text we put there ourselves comes back to us on the
/// next read, and re-ingesting it would duplicate the entry the front-end has
/// already recorded. Text the user copied is genuinely new.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// Observed on the OS clipboard (or seeded at startup) and already ingested.
    Os(String),
    /// We just wrote this. Exactly one read is expected to echo it back; that
    /// one is swallowed, after which this decays to [`Seen::Os`].
    PendingSelfWrite(String),
}

/// The decision half of the loop, kept free of I/O so it can be tested directly.
#[derive(Debug, Default)]
struct Tracker {
    seen: Option<Seen>,
}

impl Tracker {
    /// Record that `text` was **successfully** written to the OS clipboard by us.
    ///
    /// Only call this on a successful write. Arming on a failed write is a real
    /// bug: the clipboard still holds the old value, so the next read looks like
    /// a change and re-ingests it as a duplicate — and a later genuine user copy
    /// of `text` gets swallowed as "already seen".
    fn armed_self_write(&mut self, text: String) {
        self.seen = Some(Seen::PendingSelfWrite(text));
    }

    /// Feed the clipboard's current contents. Returns `true` when this is new
    /// content that should be ingested.
    fn observe(&mut self, cur: &str) -> bool {
        match &self.seen {
            // The echo of our own write. Consume the suppression exactly once,
            // then treat the value as ordinary observed content.
            Some(Seen::PendingSelfWrite(t)) if t == cur => {
                self.seen = Some(Seen::Os(cur.to_string()));
                false
            }
            // Unchanged since the last read. Note this cannot distinguish "still
            // the same value" from "the user re-copied the identical string" —
            // no platform change notification carries the value, so the only fix
            // is owner identity, which arboard does not expose.
            Some(Seen::Os(t)) if t == cur => false,
            _ => {
                self.seen = Some(Seen::Os(cur.to_string()));
                true
            }
        }
    }
}

/// Read the clipboard and ingest it if it is genuinely new.
fn refresh<C: TextClipboard, I: Ingest>(ingest: &I, cb: &mut C, tracker: &mut Tracker) {
    let Ok(cur) = cb.get_text() else { return };
    if cur.is_empty() {
        return;
    }
    if tracker.observe(&cur) {
        ingest.ingest_from_os(&cur);
    }
}

/// The clipboard thread's body: own the handle, serve writes, ingest changes.
/// `idle` bounds how long to wait on the channel before re-reading unprompted.
fn watch_loop<C: TextClipboard, I: Ingest>(
    ingest: &I,
    cb: &mut C,
    rx: &Receiver<ClipCmd>,
    idle: Duration,
) {
    let mut tracker = Tracker::default();

    // Seed from whatever is already on the clipboard so we don't ingest
    // pre-existing content as a "new" copy on every startup — but do record it
    // once, so the current clipboard is populated from the moment we start.
    refresh(ingest, cb, &mut tracker);

    loop {
        match rx.recv_timeout(idle) {
            Ok(ClipCmd::Write(text)) => match cb.set_text(text.clone()) {
                // Suppress the echo only if the write actually landed.
                Ok(()) => tracker.armed_self_write(text),
                Err(e) => tracing::warn!("failed to set OS clipboard: {e}"),
            },
            Ok(ClipCmd::OsChanged) | Err(RecvTimeoutError::Timeout) => {
                refresh(ingest, cb, &mut tracker)
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::debug!("clipboard channel closed; watch thread exiting");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::mpsc::channel;
    use std::sync::Mutex;

    /// A clipboard that lives in a `String`, with switches to make it fail the
    /// way arboard does when another client holds the selection.
    ///
    /// Shared behind an `Arc` so a test can act as a *second* application and
    /// change the clipboard underneath a running `watch_loop`.
    #[derive(Clone, Default)]
    struct FakeClipboard {
        inner: Arc<Mutex<FakeInner>>,
    }

    #[derive(Default)]
    struct FakeInner {
        text: String,
        fail_writes: bool,
        get_err: bool,
        /// How many times the loop has read us so far.
        reads: usize,
        /// "Another application copied X just before read N." Keyed by read
        /// number so a test can place a change at an exact point in the loop
        /// without sleeping — read 1 is always the startup seed.
        changes: HashMap<usize, String>,
    }

    impl FakeClipboard {
        fn holding(text: &str) -> Self {
            let cb = Self::default();
            cb.inner.lock().unwrap().text = text.to_string();
            cb
        }
        fn failing_writes(text: &str) -> Self {
            let cb = Self::holding(text);
            cb.inner.lock().unwrap().fail_writes = true;
            cb
        }
        fn unreadable() -> Self {
            let cb = Self::default();
            cb.inner.lock().unwrap().get_err = true;
            cb
        }
        /// Someone else (the "user") copies `text`, taking effect just before
        /// the loop's `nth` read. `n == 1` is the startup seed.
        fn user_copies_before_read(self, n: usize, text: &str) -> Self {
            self.inner
                .lock()
                .unwrap()
                .changes
                .insert(n, text.to_string());
            self
        }
        fn contents(&self) -> String {
            self.inner.lock().unwrap().text.clone()
        }
    }

    impl TextClipboard for FakeClipboard {
        fn get_text(&mut self) -> anyhow::Result<String> {
            let mut g = self.inner.lock().unwrap();
            if g.get_err {
                anyhow::bail!("no clipboard");
            }
            g.reads += 1;
            let n = g.reads;
            if let Some(t) = g.changes.remove(&n) {
                g.text = t;
            }
            Ok(g.text.clone())
        }
        fn set_text(&mut self, text: String) -> anyhow::Result<()> {
            let mut g = self.inner.lock().unwrap();
            if g.fail_writes {
                anyhow::bail!("clipboard occupied");
            }
            g.text = text;
            Ok(())
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Ingest for Recorder {
        fn ingest_from_os(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_string());
        }
    }

    impl Recorder {
        fn ingested(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Drive `watch_loop` to completion over a fixed script of commands. The
    /// idle timeout is set far longer than the test can take, so the only
    /// wakeups are the scripted ones and the run is deterministic.
    fn run(cb: &mut FakeClipboard, cmds: Vec<ClipCmd>) -> Recorder {
        let rec = Recorder::default();
        let (tx, rx) = channel();
        for c in cmds {
            tx.send(c).unwrap();
        }
        drop(tx); // loop exits on Disconnected once the script is drained
        watch_loop(&rec, cb, &rx, Duration::from_secs(3600));
        rec
    }

    #[test]
    fn first_sighting_is_ingested_then_repeats_are_not() {
        let mut t = Tracker::default();
        assert!(t.observe("a"));
        assert!(!t.observe("a"));
        assert!(!t.observe("a"));
    }

    #[test]
    fn self_write_echo_is_swallowed_exactly_once() {
        let mut t = Tracker::default();
        t.armed_self_write("b".into());
        // The echo of our own write: not new content.
        assert!(!t.observe("b"));
        // Suppression is spent — the tracker now just holds "b" as observed.
        assert_eq!(t.seen, Some(Seen::Os("b".into())));
    }

    #[test]
    fn user_copy_after_our_write_is_recorded() {
        let mut t = Tracker::default();
        t.armed_self_write("b".into());
        assert!(!t.observe("b")); // our echo
        assert!(t.observe("c")); // the user copied something else
    }

    #[test]
    fn a_user_copy_racing_our_write_is_not_swallowed() {
        let mut t = Tracker::default();
        t.armed_self_write("b".into());
        // The user copied "c" before we ever read "b" back. The pending
        // suppression must not hide it.
        assert!(t.observe("c"));
    }

    #[test]
    fn startup_seeds_from_existing_clipboard_without_double_recording() {
        let mut cb = FakeClipboard::holding("already here");
        let rec = run(&mut cb, vec![ClipCmd::OsChanged, ClipCmd::OsChanged]);
        // Seeded once at startup; the two prompted re-reads see no change.
        assert_eq!(rec.ingested(), vec!["already here".to_string()]);
    }

    #[test]
    fn write_reaches_the_clipboard_and_is_not_ingested_back() {
        let mut cb = FakeClipboard::default();
        let rec = run(
            &mut cb,
            vec![ClipCmd::Write("from an agent".into()), ClipCmd::OsChanged],
        );
        assert_eq!(cb.contents(), "from an agent");
        // The front-end already recorded it; re-ingesting would duplicate.
        assert!(rec.ingested().is_empty());
    }

    #[test]
    fn os_change_is_ingested() {
        // Empty at startup (seed reads nothing), then the user copies and the
        // watcher-equivalent wakes us.
        let mut cb = FakeClipboard::default().user_copies_before_read(2, "user copied this");
        let rec = run(&mut cb, vec![ClipCmd::OsChanged]);
        assert_eq!(rec.ingested(), vec!["user copied this".to_string()]);
    }

    /// A failed write must not arm echo suppression: the unchanged clipboard
    /// would look like a change on the next read and be recorded twice.
    #[test]
    fn failed_write_does_not_duplicate_the_existing_entry() {
        let mut cb = FakeClipboard::failing_writes("original");
        let rec = run(
            &mut cb,
            vec![ClipCmd::Write("never lands".into()), ClipCmd::OsChanged],
        );
        assert_eq!(cb.contents(), "original", "the write must not have landed");
        // "original" is ingested once by the startup seed and never again.
        assert_eq!(rec.ingested(), vec!["original".to_string()]);
    }

    /// The other half of the same bug: a failed write must not poison the
    /// tracker such that the user genuinely copying that text is ignored.
    #[test]
    fn failed_write_does_not_swallow_a_later_real_copy_of_that_text() {
        // The user copies the very string our write failed to set.
        let mut cb = FakeClipboard::failing_writes("original").user_copies_before_read(2, "wanted");
        let rec = run(
            &mut cb,
            vec![ClipCmd::Write("wanted".into()), ClipCmd::OsChanged],
        );
        assert_eq!(
            rec.ingested(),
            vec!["original".to_string(), "wanted".to_string()]
        );
    }

    #[test]
    fn empty_clipboard_is_never_ingested() {
        let mut cb = FakeClipboard::default();
        let rec = run(&mut cb, vec![ClipCmd::OsChanged]);
        assert!(rec.ingested().is_empty());
    }

    #[test]
    fn unreadable_clipboard_is_survived() {
        let mut cb = FakeClipboard::unreadable();
        let rec = run(&mut cb, vec![ClipCmd::OsChanged, ClipCmd::OsChanged]);
        assert!(rec.ingested().is_empty());
    }

    #[test]
    fn writing_the_same_text_twice_is_still_not_ingested() {
        // arboard re-asserts selection ownership on every write (ICCCM 2.6.1.3),
        // so a repeated write produces a second change notification.
        let mut cb = FakeClipboard::default();
        let rec = run(
            &mut cb,
            vec![
                ClipCmd::Write("b".into()),
                ClipCmd::OsChanged,
                ClipCmd::Write("b".into()),
                ClipCmd::OsChanged,
            ],
        );
        assert!(rec.ingested().is_empty());
    }

    /// No commands are ever sent: the change happens *after* the loop has
    /// started and seeded, so only the timeout arm can possibly catch it.
    #[test]
    fn idle_timeout_alone_detects_a_change_with_no_events() {
        // The change lands on read 3, which only a timeout can reach: read 1 is
        // the seed and nothing is ever sent on the channel.
        let mut cb = FakeClipboard::holding("seed").user_copies_before_read(3, "copied elsewhere");
        let rec = Arc::new(Recorder::default());
        let (tx, rx) = channel::<ClipCmd>();

        let ticker = {
            let rec = Arc::clone(&rec);
            std::thread::spawn(move || {
                // End on the condition, not a fixed sleep; the cap only bounds a
                // genuine hang, and on failure the assertion reports the state.
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while rec.ingested().len() < 2 && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(1));
                }
                drop(tx);
            })
        };

        watch_loop(&*rec, &mut cb, &rx, Duration::from_millis(5));
        ticker.join().unwrap();

        assert_eq!(
            rec.ingested(),
            vec!["seed".to_string(), "copied elsewhere".to_string()]
        );
    }
}
