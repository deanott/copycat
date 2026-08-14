//! Temporal clipboard store: a bounded, time-ordered in-memory ring
//! (`MemoryStore`) behind the `ClipboardStore` trait, with pin-exempt eviction
//! and consecutive-duplicate dedup.

use std::collections::VecDeque;

use crate::entry::Entry;

/// Query over the temporal axis.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Recall {
    /// Return at most this many, most-recent first. 0 means "no limit".
    pub limit: usize,
    /// Only entries at/after this unix-millis, if set.
    pub since: Option<i64>,
    /// Only entries at/before this unix-millis, if set.
    pub until: Option<i64>,
    /// Which lane to recall from (`None` = the default lane). Omitted on the
    /// wire when unset so the pre-channels form is byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// The store abstraction; [`MemoryStore`] is the only implementation.
pub trait ClipboardStore {
    /// Record a freshly-copied entry; it becomes the current clipboard.
    fn push(&mut self, entry: Entry) -> anyhow::Result<()>;

    /// The current (most-recent) entry, if any. Must be hot/instant.
    fn current(&self) -> Option<Entry>;

    /// The entry `index` positions back from the most recent (0 = current).
    /// `None` if out of range. Must be hot/instant.
    fn get(&self, index: usize) -> Option<Entry>;

    /// Recall history along the temporal axis, most-recent first.
    fn recall(&self, query: &Recall) -> anyhow::Result<Vec<Entry>>;

    /// Pin/unpin to exempt from eviction & expiry.
    fn set_pinned(&mut self, id: &str, pinned: bool) -> anyhow::Result<()>;

    /// Make an existing entry current again. The entry **moves** to the newest
    /// position (no duplicate) and is re-stamped with `at`, keeping the ring
    /// time-ordered; its **id is preserved**, so pins and held ids stay valid.
    /// Returns the promoted entry.
    fn promote(&mut self, id: &str, at: i64) -> anyhow::Result<Entry>;
}

/// A bounded, time-ordered in-memory ring. Newest entry is at the back; the
/// oldest *unpinned* entry is evicted over capacity, and a consecutive
/// duplicate text copy refreshes the timestamp instead of adding an entry.
pub struct MemoryStore {
    entries: VecDeque<Entry>,
    capacity: usize,
}

impl MemoryStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn evict_if_needed(&mut self) {
        while self.unpinned_len() > self.capacity {
            if let Some(pos) = self.entries.iter().position(|e| !e.pinned) {
                self.entries.remove(pos);
            } else {
                break; // everything is pinned
            }
        }
    }

    fn unpinned_len(&self) -> usize {
        self.entries.iter().filter(|e| !e.pinned).count()
    }

    /// Number of entries currently held (test/inspection helper).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The lexically-maximum entry id in the ring — the *most-recently-created*
    /// entry (ids are monotonic and lexically sortable). Not necessarily the
    /// back entry: `promote` moves an older, smaller id to the back, so a
    /// `Wait` cursor must be derived from this, never from `current().id`.
    pub fn max_id(&self) -> Option<&str> {
        self.entries.iter().map(|e| e.id.as_str()).max()
    }

    /// Hand-off primitive for `Wait`: the first entry in ring order (front =
    /// oldest) whose id is strictly greater than `after` (`None` = the front).
    /// The ring is not id-ascending after a `promote`, so cursors come from
    /// [`MemoryStore::max_id`]; the front-most scan stays correct regardless.
    /// `consume` removes the entry (at-most-once TAKE), else it is cloned (PEEK).
    /// `VecDeque::remove` moves the owned `Entry` out — fine despite
    /// `Entry: Drop`, because it isn't a by-value field move.
    pub fn take_after(&mut self, after: Option<&str>, consume: bool) -> Option<Entry> {
        let pos = self
            .entries
            .iter()
            .position(|e| after.is_none_or(|a| e.id.as_str() > a))?;
        if consume {
            self.entries.remove(pos)
        } else {
            self.entries.get(pos).cloned()
        }
    }

    /// Remove a single entry by recency **index** (0 = newest/back), returning
    /// it, or `None` if out of range. Explicit removal overrides a pin (unlike
    /// eviction). Surviving entries keep their ids and relative order.
    pub fn remove_by_index(&mut self, index: usize) -> Option<Entry> {
        let len = self.entries.len();
        if index >= len {
            return None;
        }
        self.entries.remove(len - 1 - index)
    }

    /// Remove a single entry by stable **id**, returning the removed entry or
    /// `None` if no entry has that id. Like [`MemoryStore::remove_by_index`], this
    /// overrides a pin and preserves surviving ids/order.
    pub fn remove_by_id(&mut self, id: &str) -> Option<Entry> {
        let pos = self
            .entries
            .iter()
            .position(|e| crate::entry::id_matches(&e.id, id))?;
        self.entries.remove(pos)
    }

    /// Bulk-remove entries, returning how many were removed. By default only
    /// *unpinned* entries go (pins survive, matching eviction); `include_pinned`
    /// empties the ring outright. Surviving entries keep their ids and order.
    pub fn clear(&mut self, include_pinned: bool) -> usize {
        let before = self.entries.len();
        if include_pinned {
            self.entries.clear();
        } else {
            self.entries.retain(|e| e.pinned);
        }
        before - self.entries.len()
    }
}

impl ClipboardStore for MemoryStore {
    fn push(&mut self, entry: Entry) -> anyhow::Result<()> {
        // Consecutive identical text copy: just bump the timestamp.
        if let (Some(last), crate::entry::Content::Text(new_text)) =
            (self.entries.back(), &entry.content)
        {
            if let crate::entry::Content::Text(last_text) = &last.content {
                if last_text == new_text {
                    if let Some(back) = self.entries.back_mut() {
                        back.copied_at = entry.copied_at;
                    }
                    return Ok(());
                }
            }
        }
        self.entries.push_back(entry);
        self.evict_if_needed();
        Ok(())
    }

    fn current(&self) -> Option<Entry> {
        self.entries.back().cloned()
    }

    fn get(&self, index: usize) -> Option<Entry> {
        // Newest at the back; O(1) — the hot recall path.
        let len = self.entries.len();
        if index >= len {
            return None;
        }
        self.entries.get(len - 1 - index).cloned()
    }

    fn recall(&self, query: &Recall) -> anyhow::Result<Vec<Entry>> {
        // Clone only up to `limit`, never the whole ring.
        let matched = self
            .entries
            .iter()
            .rev() // most-recent first
            .filter(|e| query.since.is_none_or(|s| e.copied_at >= s))
            .filter(|e| query.until.is_none_or(|u| e.copied_at <= u));
        let out: Vec<Entry> = if query.limit == 0 {
            matched.cloned().collect()
        } else {
            matched.take(query.limit).cloned().collect()
        };
        Ok(out)
    }

    fn set_pinned(&mut self, id: &str, pinned: bool) -> anyhow::Result<()> {
        for e in self.entries.iter_mut() {
            if crate::entry::id_matches(&e.id, id) {
                e.pinned = pinned;
                return Ok(());
            }
        }
        anyhow::bail!("no entry with id {id}")
    }

    fn promote(&mut self, id: &str, at: i64) -> anyhow::Result<Entry> {
        let pos = self
            .entries
            .iter()
            .position(|e| crate::entry::id_matches(&e.id, id))
            .ok_or_else(|| anyhow::anyhow!("no entry with id {id}"))?;
        // Remove-then-push-back keeps the count unchanged, so nothing evicts.
        let mut entry = self.entries.remove(pos).expect("position was just found");
        entry.copied_at = at;
        self.entries.push_back(entry.clone());
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Content, ContentKind, Sensitivity, Source};

    fn entry(id: &str, text: &str, copied_at: i64) -> Entry {
        Entry {
            id: id.into(),
            content: Content::Text(text.into()),
            kind: ContentKind::PlainText,
            copied_at,
            source: Source::simple("test"),
            sensitivity: Sensitivity::Normal,
            pinned: false,
        }
    }

    #[test]
    fn current_is_most_recent() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "first", 1)).unwrap();
        s.push(entry("b", "second", 2)).unwrap();
        assert_eq!(s.current().unwrap().id, "b");
    }

    #[test]
    fn get_indexes_from_most_recent() {
        let mut s = MemoryStore::new(10);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            s.push(entry(t, t, i as i64)).unwrap();
        }
        assert_eq!(s.get(0).unwrap().id, "c"); // current
        assert_eq!(s.get(1).unwrap().id, "b");
        assert_eq!(s.get(2).unwrap().id, "a");
        assert!(s.get(3).is_none()); // out of range
    }

    #[test]
    fn get_zero_matches_current() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        assert_eq!(
            s.get(0).map(|e| e.id.clone()),
            s.current().map(|e| e.id.clone())
        );
    }

    #[test]
    fn recall_is_newest_first_and_respects_limit() {
        let mut s = MemoryStore::new(10);
        for (i, t) in ["a", "b", "c", "d"].iter().enumerate() {
            s.push(entry(t, t, i as i64)).unwrap();
        }
        let got = s
            .recall(&Recall {
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<_> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["d", "c"]);
    }

    #[test]
    fn recall_filters_by_time_window() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 100)).unwrap();
        s.push(entry("b", "b", 200)).unwrap();
        s.push(entry("c", "c", 300)).unwrap();
        let got = s
            .recall(&Recall {
                limit: 0,
                since: Some(150),
                until: Some(250),
                channel: None,
            })
            .unwrap();
        let ids: Vec<_> = got.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["b"]);
    }

    #[test]
    fn consecutive_duplicate_text_bumps_timestamp_not_count() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "same", 1)).unwrap();
        s.push(entry("b", "same", 5)).unwrap(); // duplicate text
        assert_eq!(s.len(), 1);
        assert_eq!(s.current().unwrap().copied_at, 5);
    }

    #[test]
    fn evicts_oldest_unpinned_over_capacity() {
        let mut s = MemoryStore::new(2);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.push(entry("c", "c", 3)).unwrap(); // over capacity -> evict "a"
        let ids: Vec<_> = s
            .recall(&Recall::default())
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(ids, ["c", "b"]);
    }

    #[test]
    fn pinned_entries_are_not_evicted() {
        let mut s = MemoryStore::new(2);
        s.push(entry("a", "a", 1)).unwrap();
        s.set_pinned("a", true).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.push(entry("c", "c", 3)).unwrap();
        s.push(entry("d", "d", 4)).unwrap();
        // "a" is pinned so it survives; unpinned capacity is 2 -> b evicted, c,d kept
        let ids: Vec<_> = s
            .recall(&Recall::default())
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"d".to_string()));
        assert!(ids.contains(&"c".to_string()));
        assert!(!ids.contains(&"b".to_string()));
    }

    #[test]
    fn set_pinned_unknown_id_errors() {
        let mut s = MemoryStore::new(2);
        assert!(s.set_pinned("nope", true).is_err());
    }

    #[test]
    fn promote_makes_an_old_entry_current() {
        let mut s = MemoryStore::new(10);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            s.push(entry(t, t, i as i64)).unwrap();
        }
        // "a" is the oldest (index 2); promoting it makes it the current clipboard.
        let promoted = s.promote("a", 99).unwrap();
        assert_eq!(promoted.id, "a");
        assert_eq!(s.current().unwrap().id, "a");
        assert_eq!(s.get(0).unwrap().id, "a");
    }

    #[test]
    fn promote_moves_rather_than_duplicates() {
        let mut s = MemoryStore::new(10);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            s.push(entry(t, t, i as i64)).unwrap();
        }
        s.promote("a", 99).unwrap();
        assert_eq!(s.len(), 3, "promote must not add an entry");
        let ids: Vec<_> = s
            .recall(&Recall::default())
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(
            ids,
            ["a", "c", "b"],
            "a moved to newest; b,c keep their order"
        );
    }

    #[test]
    fn promote_preserves_id_and_pin_but_restamps_time() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.set_pinned("a", true).unwrap();
        let promoted = s.promote("a", 500).unwrap();
        assert_eq!(promoted.id, "a", "id is stable across a promote");
        assert!(promoted.pinned, "pin survives a promote");
        assert_eq!(promoted.copied_at, 500, "re-stamped as freshly copied");
    }

    #[test]
    fn promote_keeps_the_ring_time_ordered() {
        // The ring's front->back order must stay ascending in copied_at, otherwise
        // recall's since/until windows would return non-monotonic results.
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 100)).unwrap();
        s.push(entry("b", "b", 200)).unwrap();
        s.push(entry("c", "c", 300)).unwrap();
        s.promote("a", 400).unwrap();
        let newest_first = s.recall(&Recall::default()).unwrap();
        let times: Vec<_> = newest_first.iter().map(|e| e.copied_at).collect();
        assert_eq!(times, [400, 300, 200], "newest-first stays sorted");
        // And a time window still selects correctly after the move.
        let got = s
            .recall(&Recall {
                limit: 0,
                since: Some(350),
                until: None,
                channel: None,
            })
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a");
    }

    #[test]
    fn promote_current_entry_is_a_noop_refresh() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.promote("b", 50).unwrap(); // "b" is already current
        assert_eq!(s.len(), 2);
        assert_eq!(s.current().unwrap().id, "b");
        assert_eq!(s.current().unwrap().copied_at, 50);
    }

    #[test]
    fn promote_unknown_id_errors() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        assert!(s.promote("nope", 5).is_err());
    }

    #[test]
    fn take_after_none_returns_oldest() {
        let mut s = MemoryStore::new(10);
        s.push(entry("1", "a", 1)).unwrap();
        s.push(entry("2", "b", 2)).unwrap();
        // `after = None` == the oldest entry in the lane (front, FIFO).
        assert_eq!(s.take_after(None, false).unwrap().id, "1");
    }

    #[test]
    fn take_after_cursor_returns_next_strictly_after() {
        let mut s = MemoryStore::new(10);
        s.push(entry("1", "a", 1)).unwrap();
        s.push(entry("2", "b", 2)).unwrap();
        s.push(entry("3", "c", 3)).unwrap();
        assert_eq!(s.take_after(Some("1"), false).unwrap().id, "2");
        // A cursor at/after the head yields nothing.
        assert!(s.take_after(Some("3"), false).is_none());
    }

    #[test]
    fn take_after_consume_removes_entry_once() {
        let mut s = MemoryStore::new(10);
        s.push(entry("1", "a", 1)).unwrap();
        assert_eq!(s.take_after(None, true).unwrap().id, "1");
        // Consumed: gone from the lane, so a second take finds nothing.
        assert!(s.take_after(None, true).is_none());
        assert!(s.is_empty());
    }

    #[test]
    fn take_after_peek_leaves_entry() {
        let mut s = MemoryStore::new(10);
        s.push(entry("1", "a", 1)).unwrap();
        assert_eq!(s.take_after(None, false).unwrap().id, "1");
        // Peek (consume=false) leaves it for the next reader.
        assert_eq!(s.take_after(None, false).unwrap().id, "1");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn remove_by_index_removes_the_recency_indexed_entry() {
        let mut s = MemoryStore::new(10);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            s.push(entry(t, t, i as i64)).unwrap();
        }
        // index 1 == "b" (one back from current "c").
        let removed = s.remove_by_index(1).unwrap();
        assert_eq!(removed.id, "b");
        let ids: Vec<_> = s
            .recall(&Recall::default())
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(ids, ["c", "a"], "surviving order/ids intact");
        // Out of range yields None.
        assert!(s.remove_by_index(9).is_none());
    }

    #[test]
    fn remove_by_id_removes_the_matching_entry() {
        let mut s = MemoryStore::new(10);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            s.push(entry(t, t, i as i64)).unwrap();
        }
        assert_eq!(s.remove_by_id("a").unwrap().id, "a");
        assert_eq!(s.len(), 2);
        // Unknown id yields None.
        assert!(s.remove_by_id("nope").is_none());
    }

    /// The point of short handles: one must select an entry anywhere the full id
    /// does, across every id-taking operation, without the caller knowing which
    /// form it was handed.
    #[test]
    fn a_short_handle_selects_wherever_a_full_id_does() {
        use crate::entry::handle_of;
        let ids = [
            "1785315779365-000000",
            "1785315779365-000001",
            "1785315779365-000002",
        ];
        let mut s = MemoryStore::new(10);
        // Distinct text per entry: consecutive duplicates collapse into one.
        for (i, id) in ids.iter().enumerate() {
            s.push(entry(id, &format!("v{i}"), i as i64)).unwrap();
        }
        assert_eq!(s.len(), 3, "all three must be distinct entries");
        assert_eq!(handle_of(ids[2]).unwrap(), "c2");

        // pin by handle
        s.set_pinned("c2", true).unwrap();
        assert!(s.recall(&Recall::default()).unwrap()[0].pinned);

        // promote by handle moves that entry to newest
        let promoted = s.promote("c0", 99).unwrap();
        assert_eq!(promoted.id, ids[0], "handle c0 must resolve to seq 000000");
        assert_eq!(s.current().unwrap().id, ids[0]);

        // delete by handle
        assert_eq!(s.remove_by_id("c1").unwrap().id, ids[1]);
        assert_eq!(s.len(), 2);

        // the full id still works, unchanged
        assert_eq!(s.remove_by_id(ids[2]).unwrap().id, ids[2]);

        // a handle naming no entry resolves to nothing rather than to something
        assert!(s.remove_by_id("c9").is_none());
    }

    #[test]
    fn remove_a_pinned_entry_succeeds() {
        // Explicit removal overrides a pin (unlike eviction).
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.set_pinned("a", true).unwrap();
        assert_eq!(s.remove_by_id("a").unwrap().id, "a");
        assert!(!s
            .recall(&Recall::default())
            .unwrap()
            .iter()
            .any(|e| e.id == "a"));
        // Also via index.
        s.push(entry("c", "c", 3)).unwrap();
        s.set_pinned("c", true).unwrap();
        assert_eq!(s.remove_by_index(0).unwrap().id, "c");
    }

    #[test]
    fn clear_keeps_pinned_by_default() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.push(entry("c", "c", 3)).unwrap();
        s.set_pinned("b", true).unwrap();
        let removed = s.clear(false);
        assert_eq!(removed, 2, "the two unpinned entries were removed");
        let ids: Vec<_> = s
            .recall(&Recall::default())
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(ids, ["b"], "only the pinned entry survives");
    }

    #[test]
    fn clear_include_pinned_empties_and_counts() {
        let mut s = MemoryStore::new(10);
        s.push(entry("a", "a", 1)).unwrap();
        s.push(entry("b", "b", 2)).unwrap();
        s.set_pinned("a", true).unwrap();
        let removed = s.clear(true);
        assert_eq!(removed, 2, "every entry removed, pins included");
        assert!(s.is_empty());
    }

    #[test]
    fn max_id_survives_a_promote_of_an_older_entry() {
        // After promoting an older (smaller-id) entry to current, `current()` is
        // that smaller id, but `max_id()` must still report the true maximum id
        // (the id-ascending ring invariant no longer holds — see take_after).
        let mut s = MemoryStore::new(10);
        s.push(entry("1", "a", 1)).unwrap();
        s.push(entry("2", "b", 2)).unwrap();
        s.push(entry("3", "c", 3)).unwrap();
        s.promote("1", 99).unwrap(); // "1" is now current, but "3" is the max id
        assert_eq!(s.current().unwrap().id, "1");
        assert_eq!(s.max_id(), Some("3"));
    }
}
