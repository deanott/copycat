//! Shared daemon state: the temporal store plus the machinery to turn a raw
//! copied string into an [`Entry`] (id + timestamp + detection + classification).
//!
//! Both the IPC server (async) and the clipboard-watch thread (sync) hold an
//! `Arc<State>` and go through these methods, so there is exactly one place that
//! mints entries and one lock guarding the store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

use copycat_core::entry::{Entry, Source, AUTHOR_OS_CLIPBOARD};
use copycat_core::privacy::{self, Mode as PrivacyMode};
use copycat_core::proto::{ClipboardBackendInfo, Selector, DEFAULT_CHANNEL};
use copycat_core::store::{ClipboardStore, MemoryStore, Recall};

use crate::clip::ClipCmd;

/// Everything the daemon shares across the server and the clipboard thread.
///
/// The store is a **map of independent rings**, one lane per channel. The
/// default lane (`DEFAULT_CHANNEL == ""`) is the flat clipboard bound to the OS
/// clipboard; named lanes are private agent mailboxes for inter-agent hand-off.
/// Each lane also owns a [`Notify`] so a blocked `Wait` on that lane wakes the
/// instant something is posted to it (and only it).
pub struct State {
    /// One ring per channel. Lanes are created lazily on first write.
    channels: Mutex<HashMap<String, MemoryStore>>,
    /// One wakeup source per channel, so `Wait` on lane A never wakes for a post
    /// to lane B. Created lazily alongside (or ahead of) the lane.
    notifies: Mutex<HashMap<String, Arc<Notify>>>,
    /// Per-lane ring capacity, applied when a lane is first created.
    capacity: usize,
    /// Monotonic counter making ids unique even within the same millisecond.
    seq: AtomicU64,
    /// Device label stamped onto entries this daemon records.
    device: String,
    /// How this daemon relates to the OS clipboard (for `status`).
    backend: ClipboardBackendInfo,
    /// When bound to an OS clipboard, a **default-lane** `Copy` pushes text here
    /// so the clipboard thread writes it back to the real clipboard. A scoped
    /// copy to a named lane never does this. `None` when headless.
    os_set: Option<Sender<ClipCmd>>,
    /// Writes the usage log. `disabled()` unless the daemon was started with a
    /// level that records, so the request path pays nothing when it is off.
    usage: crate::usage::UsageLog,
    /// What is being recorded. Published in `DaemonStatus` so front-ends read
    /// the daemon's answer rather than their own environment.
    usage_level: copycat_core::usage::Level,
    /// Whether the daemon is running in insecure debug mode (for `status`).
    insecure: bool,
    /// The active privacy posture. Daemon-authoritative: seeded from the
    /// daemon's env, changeable at runtime via `SetPrivacy`, applied to every
    /// entry on the way out ([`State::project`]) and published in `status`, so
    /// no front-end holds its own copy. Not persisted.
    privacy: Mutex<PrivacyMode>,
}

/// Lock recovering from poisoning: a panic mid-mutation must not brick the
/// daemon for every later request — the guarded data stays structurally valid.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn not_found(selector: &Selector) -> anyhow::Error {
    match selector {
        Selector::Id(id) => anyhow::anyhow!("no entry with id {id}"),
        Selector::Index(i) => anyhow::anyhow!("no entry at index {i}"),
    }
}

/// Resolve a selector to an entry id. Callers must hold the store lock across
/// resolution AND the mutation it feeds, or an index races a concurrent copy.
fn resolve_id(store: &MemoryStore, selector: &Selector) -> anyhow::Result<String> {
    match selector {
        Selector::Id(id) => Ok(id.clone()),
        Selector::Index(i) => store
            .get(*i)
            .map(|e| e.id.clone())
            .ok_or_else(|| not_found(selector)),
    }
}

/// Reap an emptied NAMED lane so consumed mailboxes don't pin rings (or their
/// zeroizing payloads) in memory. The default lane is never reaped, and the
/// lane's `Notify` is deliberately left in place — removing it could drop a
/// wakeup for a waiter still parked on this lane.
fn reap_if_empty(map: &mut HashMap<String, MemoryStore>, key: &str) {
    if key != DEFAULT_CHANNEL && map.get(key).is_some_and(|s| s.is_empty()) {
        map.remove(key);
    }
}

impl State {
    pub fn new(
        capacity: usize,
        device: String,
        backend: ClipboardBackendInfo,
        os_set: Option<Sender<ClipCmd>>,
    ) -> Self {
        State {
            channels: Mutex::new(HashMap::new()),
            notifies: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            seq: AtomicU64::new(0),
            usage: crate::usage::UsageLog::disabled(),
            usage_level: copycat_core::usage::Level::Off,
            device,
            backend,
            os_set,
            insecure: copycat_core::insecure::reveal_secrets(),
            privacy: Mutex::new(PrivacyMode::from_env()),
        }
    }

    fn channels(&self) -> std::sync::MutexGuard<'_, HashMap<String, MemoryStore>> {
        lock(&self.channels)
    }

    /// Normalise an optional channel to a lane key. `None` and `Some("")` both
    /// map to the default lane.
    fn lane_key(channel: Option<&str>) -> &str {
        channel.unwrap_or(DEFAULT_CHANNEL)
    }

    /// Apply the active [privacy posture](PrivacyMode) to an entry **on its way
    /// out of the store**. Entries are stored full-fidelity; projection rewrites
    /// only the outgoing sensitivity, never the payload — which is what lets a
    /// mode change cover history that already exists, not just the next copy.
    fn project(&self, mut entry: Entry) -> Entry {
        entry.sensitivity = privacy::project(entry.sensitivity, self.privacy());
        entry
    }

    /// The wakeup source for a lane, created on demand. Cloned out (an `Arc`) so
    /// a waiter can `.await` on it without holding any lock.
    pub fn notify_for(&self, channel: Option<&str>) -> Arc<Notify> {
        let key = Self::lane_key(channel);
        let mut m = lock(&self.notifies);
        m.entry(key.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// Hand-off take/peek on a lane (see [`MemoryStore::take_after`]). Two
    /// consuming waiters are serialised by this `Mutex`, so exactly one wins.
    pub fn take_after(
        &self,
        channel: Option<&str>,
        after: Option<&str>,
        consume: bool,
    ) -> Option<Entry> {
        let key = Self::lane_key(channel);
        let mut map = self.channels();
        let taken = map
            .get_mut(key)
            .and_then(|s| s.take_after(after, consume))
            .map(|e| self.project(e));
        if consume {
            reap_if_empty(&mut map, key);
        }
        taken
    }

    /// Attach a usage log. Builder rather than a `new` parameter so the many
    /// `State::new` call sites keep getting a log that writes nothing.
    pub fn with_usage(
        mut self,
        log: crate::usage::UsageLog,
        level: copycat_core::usage::Level,
    ) -> Self {
        self.usage = log;
        self.usage_level = level;
        self
    }

    /// The log and the level in force, for the request path.
    pub fn usage(&self) -> (&crate::usage::UsageLog, copycat_core::usage::Level) {
        (&self.usage, self.usage_level)
    }

    /// Number of live lanes, for the named-lane GC tests.
    #[cfg(test)]
    pub fn lane_count(&self) -> usize {
        self.channels().len()
    }

    pub(crate) fn now_millis() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// A per-daemon-unique id: `<millis>-<seq>`. No random dependency needed.
    fn next_id(&self, millis: i64) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{millis:013}-{n:06}")
    }

    fn source_with(&self, author: Option<&str>, session_id: Option<&str>) -> Source {
        Source {
            device: self.device.clone(),
            app: None,
            author: author.map(String::from),
            session_id: session_id.map(String::from),
        }
    }

    /// Provenance-free shorthand for [`Self::record_copy_with_provenance`];
    /// every production copy arrives over IPC with provenance attached.
    #[cfg(test)]
    pub fn record_copy(&self, text: &str, channel: Option<&str>) -> String {
        self.record_copy_with_provenance(text, channel, None, None)
    }

    pub fn record_copy_with_provenance(
        &self,
        text: &str,
        channel: Option<&str>,
        author: Option<&str>,
        session_id: Option<&str>,
    ) -> String {
        let id = self.push_text_with_provenance(text, "front-end", channel, author, session_id);
        // OS-clipboard write-back is default-lane-only.
        if Self::lane_key(channel) == DEFAULT_CHANNEL {
            if let Some(tx) = &self.os_set {
                // Best-effort: if the watch thread is gone the daemon is shutting down.
                let _ = tx.send(ClipCmd::Write(text.to_string()));
            }
        }
        // `notify_waiters` only wakes already-registered waiters; the wait loop
        // registers before checking, so a post in that window is still caught.
        self.notify_for(channel).notify_waiters();
        id
    }

    /// Ingest text observed on the OS clipboard by the watch thread: a
    /// default-lane copy with no write-back (it is already the live clipboard).
    /// A change no client asked for means a person pressed Ctrl/Cmd+C, so the
    /// daemon — not a forgeable client claim — stamps [`AUTHOR_OS_CLIPBOARD`].
    pub fn ingest_from_os(&self, text: &str) -> String {
        let id = self.push_text_with_provenance(
            text,
            "os-clipboard",
            None,
            Some(AUTHOR_OS_CLIPBOARD),
            None,
        );
        self.notify_for(None).notify_waiters();
        id
    }

    fn push_text_with_provenance(
        &self,
        text: &str,
        origin: &str,
        channel: Option<&str>,
        author: Option<&str>,
        session_id: Option<&str>,
    ) -> String {
        let key = Self::lane_key(channel);
        let millis = Self::now_millis();
        let id = self.next_id(millis);
        let entry = Entry::from_text(
            id.clone(),
            text,
            millis,
            self.source_with(author, session_id),
        );
        // Audit trail for the clipboard-hijack surface: record that a write
        // happened, its classification and size — never the value itself.
        tracing::info!(
            id = %id,
            origin,
            channel = key,
            sensitivity = ?entry.sensitivity,
            kind = ?entry.kind,
            bytes = text.len(),
            "clipboard write"
        );
        let mut map = self.channels();
        let store = map
            .entry(key.to_string())
            .or_insert_with(|| MemoryStore::new(self.capacity));
        // Report the id that actually ends up current: on a consecutive-duplicate
        // copy the store refreshes the existing entry, so surface that one's id.
        let _ = store.push(entry);
        // `Entry` impls Drop (payload zeroizing), so clone the id rather than
        // moving it out of the returned entry.
        store.current().map(|e| e.id.clone()).unwrap_or(id)
    }

    pub fn current(&self, channel: Option<&str>) -> Option<Entry> {
        let key = Self::lane_key(channel);
        self.channels()
            .get(key)
            .and_then(|s| s.current())
            .map(|e| self.project(e))
    }

    /// Fast recall of the entry `index` positions back (0 = current) on a lane.
    pub fn get(&self, index: usize, channel: Option<&str>) -> Option<Entry> {
        let key = Self::lane_key(channel);
        self.channels()
            .get(key)
            .and_then(|s| s.get(index))
            .map(|e| self.project(e))
    }

    /// Pin/unpin by index or id on the **default lane**, resolving the selector
    /// to an id under a single lock so an index can't race a concurrent copy.
    /// Returns the resolved id.
    pub fn pin(&self, selector: &Selector, pinned: bool) -> anyhow::Result<String> {
        let mut map = self.channels();
        let store = map
            .get_mut(DEFAULT_CHANNEL)
            .ok_or_else(|| anyhow::anyhow!("clipboard is empty"))?;
        let id = resolve_id(store, selector)?;
        store.set_pinned(&id, pinned)?;
        Ok(id)
    }

    /// Make an existing entry the current clipboard: move it to newest in the
    /// store **and** write it back to the OS clipboard, so the user's next
    /// Ctrl/Cmd+V yields it. Default lane only — the OS clipboard *is* the
    /// default lane. The text never passes through the caller, so an agent can
    /// promote a `Secret` entry without ever reading it.
    pub fn promote(&self, selector: &Selector) -> anyhow::Result<Entry> {
        let millis = Self::now_millis();
        // Resolve and move under one lock; drop the guard before touching the
        // OS clipboard channel.
        let entry = {
            let mut map = self.channels();
            let store = map
                .get_mut(DEFAULT_CHANNEL)
                .ok_or_else(|| anyhow::anyhow!("clipboard is empty"))?;
            let id = resolve_id(store, selector)?;
            store.promote(&id, millis)?
        };
        if let (Some(tx), Some(text)) = (&self.os_set, entry.as_text()) {
            // Best-effort: if the watch thread is gone the daemon is shutting down.
            let _ = tx.send(ClipCmd::Write(text.to_string()));
        }
        Ok(self.project(entry))
    }

    /// Remove one entry by index or id from `channel` (`None` = default lane),
    /// resolving under a single lock so an index can't race a concurrent copy.
    /// Explicit delete **overrides a pin**, unlike eviction. The OS clipboard is
    /// deliberately left alone — only `promote` writes back.
    pub fn delete(&self, selector: &Selector, channel: Option<&str>) -> anyhow::Result<String> {
        let key = Self::lane_key(channel);
        let mut map = self.channels();
        let store = map
            .get_mut(key)
            .ok_or_else(|| anyhow::anyhow!("clipboard is empty"))?;
        let removed = match selector {
            Selector::Id(id) => store.remove_by_id(id),
            Selector::Index(i) => store.remove_by_index(*i),
        };
        let entry = removed.ok_or_else(|| not_found(selector))?;
        reap_if_empty(&mut map, key);
        // Clone rather than move: `Entry: Drop`, and it must still drop
        // (zeroizing its payload) here.
        Ok(entry.id.clone())
    }

    /// Bulk-remove entries, returning how many went. Scope widens outward:
    /// `all_channels` clears **every** lane (and `channel` is ignored), otherwise
    /// just the named lane (`None` = default). `include_pinned` also takes pinned
    /// entries; by default pins survive, matching eviction.
    pub fn clear(&self, include_pinned: bool, all_channels: bool, channel: Option<&str>) -> usize {
        let mut map = self.channels();
        if all_channels {
            let removed: usize = map.values_mut().map(|s| s.clear(include_pinned)).sum();
            // Reap emptied named lanes; the default lane stays even when empty.
            map.retain(|k, s| k == DEFAULT_CHANNEL || !s.is_empty());
            return removed;
        }
        let key = Self::lane_key(channel);
        let Some(store) = map.get_mut(key) else {
            return 0;
        };
        let removed = store.clear(include_pinned);
        reap_if_empty(&mut map, key);
        removed
    }

    /// The high-water entry id on a lane — the cursor a `Wait` should start from.
    ///
    /// Deliberately **not** `current().id`: `promote` moves an older (smaller-id)
    /// entry to the back, so the back entry's id can be lower than one still
    /// mid-ring. Seeding a cursor from the back would then re-deliver that
    /// mid-ring entry as if it were new. See [`MemoryStore::max_id`].
    pub fn max_id(&self, channel: Option<&str>) -> Option<String> {
        let key = Self::lane_key(channel);
        self.channels()
            .get(key)
            .and_then(|s| s.max_id().map(str::to_string))
    }

    /// Recall history along the temporal axis for the lane named in `query`.
    pub fn recall(&self, query: &Recall) -> anyhow::Result<Vec<Entry>> {
        let key = Self::lane_key(query.channel.as_deref());
        let found = match self.channels().get(key) {
            Some(s) => s.recall(query)?,
            None => Vec::new(),
        };
        Ok(found.into_iter().map(|e| self.project(e)).collect())
    }

    /// Total entries held across every lane (for `status`).
    pub fn len(&self) -> usize {
        self.channels().values().map(|s| s.len()).sum()
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn backend(&self) -> &ClipboardBackendInfo {
        &self.backend
    }

    /// Whether the daemon is in insecure debug mode (surfaced by `status`).
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    /// The active privacy posture (surfaced by `status`, and honoured by
    /// front-ends in other processes).
    pub fn privacy(&self) -> PrivacyMode {
        *lock(&self.privacy)
    }

    /// Change the posture daemon-wide, returning the mode now in force. Takes
    /// effect on the next read of *any* entry, including ones already in
    /// history: the mode is applied on the way out, never baked in at copy time.
    /// Logged as a security-relevant change — never with clipboard content.
    pub fn set_privacy(&self, mode: PrivacyMode) -> PrivacyMode {
        let mut guard = lock(&self.privacy);
        let previous = *guard;
        *guard = mode;
        if previous != mode {
            tracing::info!(from = %previous, to = %mode, "privacy mode changed");
        }
        mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless_state() -> State {
        State::new(
            16,
            "test-device".into(),
            ClipboardBackendInfo::Headless,
            None,
        )
    }

    #[test]
    fn record_copy_becomes_current() {
        let s = headless_state();
        let id = s.record_copy("hello world", None);
        assert_eq!(s.current(None).unwrap().id, id);
        assert_eq!(s.current(None).unwrap().as_text(), Some("hello world"));
        assert_eq!(s.len(), 1);
    }

    use copycat_core::entry::Sensitivity;

    /// The sensitivity a front-end would see for `text` under `mode`.
    fn served_sensitivity(text: &str, mode: PrivacyMode) -> Sensitivity {
        let s = headless_state();
        s.set_privacy(mode);
        s.record_copy(text, None);
        s.current(None).expect("entry").sensitivity
    }

    #[test]
    fn guarded_serves_secrets_marked_and_personal_data_plain() {
        assert_eq!(
            served_sensitivity("ghp_0123456789abcdefghij", PrivacyMode::Guarded),
            Sensitivity::Secret
        );
        // The headline of the default posture: an email is not flagged at all.
        assert_eq!(
            served_sensitivity("dean@example.com", PrivacyMode::Guarded),
            Sensitivity::Normal
        );
    }

    #[test]
    fn strict_serves_personal_data_marked_too() {
        assert_eq!(
            served_sensitivity("dean@example.com", PrivacyMode::Strict),
            Sensitivity::Pii
        );
        assert_eq!(
            served_sensitivity("ghp_0123456789abcdefghij", PrivacyMode::Strict),
            Sensitivity::Secret
        );
    }

    #[test]
    fn off_serves_everything_unmarked() {
        for text in ["ghp_0123456789abcdefghij", "dean@example.com"] {
            assert_eq!(
                served_sensitivity(text, PrivacyMode::Off),
                Sensitivity::Normal,
                "{text}"
            );
        }
    }

    #[test]
    fn mode_change_re_scopes_entries_already_in_history() {
        // The reason the projection lives on the read path: a user who tightens
        // the setting expects it to cover what they copied five minutes ago.
        let s = headless_state();
        s.set_privacy(PrivacyMode::Guarded);
        s.record_copy("dean@example.com", None);
        assert_eq!(s.current(None).unwrap().sensitivity, Sensitivity::Normal);

        s.set_privacy(PrivacyMode::Strict);
        assert_eq!(s.current(None).unwrap().sensitivity, Sensitivity::Pii);
        assert_eq!(
            s.recall(&Recall::default()).unwrap()[0].sensitivity,
            Sensitivity::Pii
        );
        assert_eq!(s.get(0, None).unwrap().sensitivity, Sensitivity::Pii);

        s.set_privacy(PrivacyMode::Off);
        assert_eq!(s.current(None).unwrap().sensitivity, Sensitivity::Normal);
    }

    #[test]
    fn projection_never_touches_the_stored_payload() {
        // Masking is display policy; the value itself must survive intact in
        // every posture, or "re-copy from history" would hand back a lie.
        for mode in PrivacyMode::ALL {
            let s = headless_state();
            s.set_privacy(mode);
            s.record_copy("ghp_0123456789abcdefghij", None);
            assert_eq!(
                s.current(None).unwrap().as_text(),
                Some("ghp_0123456789abcdefghij"),
                "{mode}"
            );
        }
    }

    #[test]
    fn set_privacy_changes_the_mode_in_force() {
        let s = headless_state();
        s.set_privacy(PrivacyMode::Guarded);
        assert_eq!(s.privacy(), PrivacyMode::Guarded);
        s.set_privacy(PrivacyMode::Strict);
        assert_eq!(s.privacy(), PrivacyMode::Strict);
        assert_eq!(s.set_privacy(PrivacyMode::Off), PrivacyMode::Off);
        assert_eq!(s.privacy(), PrivacyMode::Off);
    }

    #[test]
    fn ids_are_unique_within_a_millisecond() {
        let s = headless_state();
        let a = s.record_copy("a", None);
        let b = s.record_copy("b", None);
        assert_ne!(a, b, "seq counter must disambiguate same-millis ids");
    }

    #[test]
    fn recall_returns_newest_first() {
        let s = headless_state();
        s.record_copy("one", None);
        s.record_copy("two", None);
        s.record_copy("three", None);
        let got = s
            .recall(&Recall {
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        let texts: Vec<_> = got.iter().filter_map(|e| e.as_text()).collect();
        assert_eq!(texts, ["three", "two"]);
    }

    #[test]
    fn duplicate_copy_refreshes_instead_of_growing() {
        let s = headless_state();
        s.record_copy("same", None);
        let id2 = s.record_copy("same", None);
        assert_eq!(s.len(), 1, "consecutive duplicate must not add a new entry");
        assert_eq!(s.current(None).unwrap().id, id2);
    }

    #[test]
    fn delete_removes_by_index_and_by_id() {
        let s = headless_state();
        s.record_copy("one", None);
        let two = s.record_copy("two", None);
        s.record_copy("three", None);
        // Index 1 == "two" (one back from current "three").
        assert_eq!(s.delete(&Selector::Index(1), None).unwrap(), two);
        assert_eq!(s.len(), 2);
        let id = s.current(None).unwrap().id.clone();
        assert_eq!(s.delete(&Selector::Id(id.clone()), None).unwrap(), id);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn delete_of_a_missing_entry_is_an_error_not_a_panic() {
        let s = headless_state();
        s.record_copy("only", None);
        assert!(s.delete(&Selector::Index(9), None).is_err());
        assert!(s.delete(&Selector::Id("nope".into()), None).is_err());
        // An untouched lane still errors rather than unwrapping a missing store.
        assert!(s.delete(&Selector::Index(0), Some("ghost")).is_err());
        assert_eq!(s.len(), 1, "failed deletes must not remove anything");
    }

    #[test]
    fn delete_overrides_a_pin() {
        let s = headless_state();
        let id = s.record_copy("pinned", None);
        s.pin(&Selector::Id(id.clone()), true).unwrap();
        assert_eq!(s.delete(&Selector::Id(id.clone()), None).unwrap(), id);
        assert_eq!(s.len(), 0, "explicit delete beats a pin, unlike eviction");
    }

    #[test]
    fn delete_is_lane_scoped_and_reaps_an_emptied_named_lane() {
        let s = headless_state();
        s.record_copy("default", None);
        s.record_copy("laned", Some("mailbox"));
        assert_eq!(s.lane_count(), 2);
        s.delete(&Selector::Index(0), Some("mailbox")).unwrap();
        assert_eq!(s.lane_count(), 1, "emptied named lane is reaped");
        assert_eq!(s.current(None).unwrap().as_text(), Some("default"));
    }

    #[test]
    fn clear_keeps_pins_by_default_and_scopes_to_a_lane() {
        let s = headless_state();
        let keep = s.record_copy("keep", None);
        s.record_copy("drop", None);
        s.pin(&Selector::Id(keep.clone()), true).unwrap();
        s.record_copy("laned", Some("mailbox"));

        assert_eq!(s.clear(false, false, None), 1, "only the unpinned entry");
        assert_eq!(s.current(None).unwrap().id, keep);
        // The named lane was out of scope and survives.
        assert_eq!(s.current(Some("mailbox")).unwrap().as_text(), Some("laned"));

        assert_eq!(
            s.clear(true, false, None),
            1,
            "include_pinned takes the pin"
        );
        assert_eq!(s.len(), 1, "only the named lane's entry remains");
    }

    #[test]
    fn clear_all_channels_empties_every_lane() {
        let s = headless_state();
        s.record_copy("a", None);
        s.record_copy("b", Some("one"));
        s.record_copy("c", Some("two"));
        assert_eq!(s.clear(false, true, None), 3);
        assert_eq!(s.len(), 0);
        assert_eq!(
            s.lane_count(),
            1,
            "named lanes reaped, default lane persists"
        );
    }

    #[test]
    fn clear_of_an_unknown_lane_removes_nothing() {
        let s = headless_state();
        s.record_copy("a", None);
        assert_eq!(s.clear(false, false, Some("ghost")), 0);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn max_id_is_the_high_water_mark_not_the_promoted_back_entry() {
        // The reason `Wait` seeds its cursor from `max_id` and not `current`:
        // promoting an older entry puts a SMALLER id at the back.
        let s = headless_state();
        let first = s.record_copy("first", None);
        let newest = s.record_copy("newest", None);
        s.promote(&Selector::Id(first.clone())).unwrap();
        assert_eq!(s.current(None).unwrap().id, first, "promoted to back");
        assert!(first < newest, "ids are monotonic");
        assert_eq!(
            s.max_id(None).as_deref(),
            Some(newest.as_str()),
            "cursor must be the high-water id, not the back entry's"
        );
    }

    #[test]
    fn pin_marks_entry() {
        let s = headless_state();
        let id = s.record_copy("keep me", None);
        s.pin(&Selector::Id(id.clone()), true).unwrap();
        assert!(s.current(None).unwrap().pinned);
        assert!(s.pin(&Selector::Id("nope".into()), true).is_err());
    }

    #[test]
    fn get_recalls_by_index() {
        let s = headless_state();
        s.record_copy("oldest", None);
        s.record_copy("middle", None);
        s.record_copy("newest", None);
        assert_eq!(s.get(0, None).unwrap().as_text(), Some("newest"));
        assert_eq!(s.get(2, None).unwrap().as_text(), Some("oldest"));
        assert!(s.get(5, None).is_none());
    }

    #[test]
    fn pin_by_index_resolves_to_id() {
        let s = headless_state();
        s.record_copy("a", None);
        let id_b = s.record_copy("b", None); // index 0
        let resolved = s.pin(&Selector::Index(0), true).unwrap();
        assert_eq!(resolved, id_b);
        assert!(s.get(0, None).unwrap().pinned);
        // out-of-range index errors, not panics
        assert!(s.pin(&Selector::Index(9), true).is_err());
        // and by id still works
        assert_eq!(s.pin(&Selector::Id(id_b.clone()), false).unwrap(), id_b);
    }

    #[test]
    fn promote_makes_an_older_entry_current() {
        let s = headless_state();
        let id_a = s.record_copy("oldest", None);
        s.record_copy("middle", None);
        s.record_copy("newest", None);
        let promoted = s.promote(&Selector::Index(2)).unwrap(); // "oldest"
        assert_eq!(promoted.id, id_a);
        assert_eq!(s.current(None).unwrap().as_text(), Some("oldest"));
        assert_eq!(s.len(), 3, "promote moves, never duplicates");
        // by id works too, and out-of-range/unknown selectors error rather than panic
        assert_eq!(s.promote(&Selector::Id(id_a.clone())).unwrap().id, id_a);
        assert!(s.promote(&Selector::Index(9)).is_err());
        assert!(s.promote(&Selector::Id("nope".into())).is_err());
    }

    /// The whole point of promote: it must write the entry back to the OS
    /// clipboard, or the user's next Ctrl+V still yields the old value.
    #[test]
    fn promote_writes_the_entry_back_to_the_os_clipboard() {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = State::new(
            16,
            "test".into(),
            ClipboardBackendInfo::Os {
                name: "fake".into(),
            },
            Some(tx),
        );
        s.record_copy("first", None);
        s.record_copy("second", None);
        let _ = rx.recv().unwrap(); // "first"  (from record_copy)
        let _ = rx.recv().unwrap(); // "second" (from record_copy)

        s.promote(&Selector::Index(1)).unwrap(); // promote "first"
        assert!(
            matches!(rx.recv().unwrap(), ClipCmd::Write(t) if t == "first"),
            "promoted entry must be pushed to the real clipboard"
        );
    }

    #[test]
    fn ingest_classifies_secret() {
        let s = headless_state();
        s.ingest_from_os("ghp_0123456789abcdefghij");
        assert_eq!(
            s.current(None).unwrap().sensitivity,
            copycat_core::entry::Sensitivity::Secret
        );
    }

    #[test]
    fn os_clipboard_ingest_is_attributed_to_the_human() {
        use copycat_core::entry::Origin;
        let s = headless_state();
        s.ingest_from_os("copied out of a browser");
        let e = s.current(None).unwrap();
        assert_eq!(e.source.author.as_deref(), Some(AUTHOR_OS_CLIPBOARD));
        assert_eq!(
            Origin::of(e.source.author.as_deref()),
            Origin::Human,
            "a clipboard change nobody asked for is a person pressing Ctrl+C"
        );
        // The watcher has no session to attribute it to.
        assert_eq!(e.source.session_id, None);
    }

    #[test]
    fn front_end_copy_keeps_the_authorship_the_client_declared() {
        use copycat_core::entry::Origin;
        let s = headless_state();
        s.record_copy_with_provenance(
            "written by a model",
            None,
            Some("agent:claude-3-5"),
            Some("sess_abc"),
        );
        let e = s.current(None).unwrap();
        assert_eq!(e.source.author.as_deref(), Some("agent:claude-3-5"));
        assert_eq!(e.source.session_id.as_deref(), Some("sess_abc"));
        assert_eq!(Origin::of(e.source.author.as_deref()), Origin::Agent);
    }

    #[test]
    fn front_end_copy_without_an_author_stays_unattributed() {
        use copycat_core::entry::Origin;
        // The daemon must not invent an author for a client that declined to
        // give one — a guessed attribution is worse than an absent one.
        let s = headless_state();
        s.record_copy("from some raw socket client", None);
        let e = s.current(None).unwrap();
        assert_eq!(e.source.author, None);
        assert_eq!(Origin::of(e.source.author.as_deref()), Origin::Unknown);
    }

    #[test]
    fn scoped_copy_does_not_touch_os_clipboard() {
        // A named-lane copy must NOT be sent to the OS write-back channel.
        let (tx, rx) = std::sync::mpsc::channel::<ClipCmd>();
        let s = State::new(
            16,
            "test-device".into(),
            ClipboardBackendInfo::Os {
                name: "test".into(),
            },
            Some(tx),
        );
        // Default-lane copy DOES write back.
        s.record_copy("to os", None);
        assert!(
            matches!(rx.try_recv(), Ok(ClipCmd::Write(t)) if t == "to os"),
            "default-lane copy must reach the clipboard thread as a Write"
        );
        // Named-lane copy stays private: nothing on the OS channel.
        s.record_copy("private", Some("build"));
        assert!(rx.try_recv().is_err(), "scoped copy must not hit os_set");
    }

    #[test]
    fn lane_isolation_default_unaffected_by_named_post() {
        let s = headless_state();
        s.record_copy("user thing", None);
        s.record_copy("agent thing", Some("a"));
        // Default lane still sees only its own entry.
        assert_eq!(s.current(None).unwrap().as_text(), Some("user thing"));
        assert_eq!(s.len(), 2, "both lanes hold one entry each");
        // Unscoped recall never unions the named lane.
        let def = s
            .recall(&Recall {
                limit: 0,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(def.len(), 1);
        assert_eq!(def[0].as_text(), Some("user thing"));
    }

    #[test]
    fn consuming_take_reaps_the_emptied_named_lane() {
        let s = headless_state();
        s.record_copy("mailbox item", Some("job"));
        assert_eq!(s.lane_count(), 1);
        // Consume it; the now-empty named lane is reaped so it can't linger forever.
        let got = s.take_after(Some("job"), None, true);
        assert_eq!(got.unwrap().as_text(), Some("mailbox item"));
        assert_eq!(s.lane_count(), 0, "emptied named lane must be reaped");
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn default_lane_is_never_reaped_when_emptied() {
        let s = headless_state();
        s.record_copy("on the os clipboard", None);
        // A consuming take that empties the DEFAULT lane must NOT drop it.
        let _ = s.take_after(None, None, true);
        assert_eq!(s.lane_count(), 1, "default lane persists even when empty");
    }

    #[test]
    fn peek_does_not_reap() {
        let s = headless_state();
        s.record_copy("keep", Some("job"));
        // A non-consuming peek leaves the entry, so the lane stays.
        let _ = s.take_after(Some("job"), None, false);
        assert_eq!(s.lane_count(), 1);
        assert_eq!(s.current(Some("job")).unwrap().as_text(), Some("keep"));
    }

    #[test]
    fn named_lane_index_zero_is_its_own_newest() {
        let s = headless_state();
        s.record_copy("a1", Some("a"));
        s.record_copy("a2", Some("a"));
        // index 0 on lane `a` is that lane's newest, not the default lane's.
        assert_eq!(s.get(0, Some("a")).unwrap().as_text(), Some("a2"));
        assert_eq!(s.get(1, Some("a")).unwrap().as_text(), Some("a1"));
        // default lane is empty / independent.
        assert!(s.current(None).is_none());
    }
}
