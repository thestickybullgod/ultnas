//! VerifiedCache — LRU-bounded in-memory cache of BLAKE3-verified sealed records.
//!
//! ## Mutex choice
//! Shared as [`SharedCache`] (`Arc<std::sync::Mutex<VerifiedCache>>`). Every
//! critical section is a hashmap lookup or insert — never file I/O — and the
//! only callers run inside `spawn_blocking`. A std `MutexGuard` is `!Send`,
//! so the compiler rejects any spawned future that holds it across `.await`.
//!
//! ## Admission
//! `insert` re-hashes the bytes and rejects anything that doesn't match its
//! ContentId, so a tampered object can never enter the cache and later be
//! "restored" as if it were good.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tracing::{debug, warn};
use ultnas_core::{hash_bytes, ContentId, UltnasCoreError, Vault};

/// LRU-bounded, BLAKE3-verified in-memory record cache.
///
/// Values are `Arc<[u8]>` so `get` is a refcount bump, not a copy.
pub struct VerifiedCache {
    slots: HashMap<ContentId, Arc<[u8]>>,
    /// Least recently used at the front.
    order: VecDeque<ContentId>,
    used_bytes: u64,
    max_bytes: u64,
}

impl VerifiedCache {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            slots: HashMap::new(),
            order: VecDeque::new(),
            used_bytes: 0,
            max_bytes,
        }
    }

    /// Insert verified bytes, evicting least-recently-used entries to make room.
    ///
    /// Returns `false` (and caches nothing) if `data` doesn't hash to `id` or
    /// is larger than the whole budget.
    pub fn insert(&mut self, id: ContentId, data: Vec<u8>) -> bool {
        if hash_bytes(&data) != id {
            warn!(
                "VerifiedCache: rejecting {} — content does not match its ContentId",
                id
            );
            return false;
        }
        if self.slots.contains_key(&id) {
            self.touch(&id);
            return true;
        }
        let size = data.len() as u64;
        if size > self.max_bytes {
            debug!(
                "VerifiedCache: {} ({} bytes) exceeds the cache budget",
                id, size
            );
            return false;
        }
        while self.used_bytes + size > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.slots.remove(&oldest) {
                self.used_bytes = self.used_bytes.saturating_sub(evicted.len() as u64);
            }
        }
        self.used_bytes += size;
        self.order.push_back(id);
        self.slots.insert(id, data.into());
        true
    }

    /// Retrieve a cached record by its ContentId and mark it most recently used.
    pub fn get(&mut self, id: &ContentId) -> Option<Arc<[u8]>> {
        let data = self.slots.get(id)?.clone();
        self.touch(id);
        Some(data)
    }

    /// Move `id` to the most-recently-used end. O(n), fine at cache sizes
    /// of a few thousand entries; swap for an intrusive list if that grows.
    fn touch(&mut self, id: &ContentId) {
        if let Some(pos) = self.order.iter().position(|x| x == id) {
            self.order.remove(pos);
        }
        self.order.push_back(*id);
    }

    /// Warm the cache from the vault's sealed records.
    ///
    /// Designed to be called from `tokio::task::spawn_blocking` at daemon startup.
    /// Returns the count of records successfully loaded. Objects that fail
    /// verification are skipped (the watcher will report them).
    pub fn warm_from_vault(&mut self, vault: &Vault) -> Result<usize, UltnasCoreError> {
        let mut loaded = 0usize;
        for record in vault.all_records()?.into_iter().filter(|r| r.is_sealed()) {
            match vault.read_content(&record.id) {
                Ok(data) => {
                    if self.insert(record.id, data) {
                        debug!("VerifiedCache: warmed {}", record.id);
                        loaded += 1;
                    }
                }
                Err(e) => {
                    warn!("VerifiedCache: skipping {} — {}", record.id, e);
                }
            }
        }
        Ok(loaded)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.len()
    }
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
    }
}

/// The cache as shared between the warm-up task and `IntegrityGuard`.
pub type SharedCache = Arc<Mutex<VerifiedCache>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(data: &[u8]) -> ContentId {
        hash_bytes(data)
    }

    #[test]
    fn rejects_bytes_that_do_not_match_id() {
        let mut cache = VerifiedCache::new(1024);
        assert!(!cache.insert(id_of(b"good"), b"evil".to_vec()));
        assert!(cache.is_empty());
    }

    #[test]
    fn rejects_entries_larger_than_budget() {
        let mut cache = VerifiedCache::new(4);
        assert!(cache.insert(id_of(b"abcd"), b"abcd".to_vec()));
        assert!(!cache.insert(id_of(b"abcdef"), b"abcdef".to_vec()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn get_refreshes_recency() {
        let mut cache = VerifiedCache::new(8);
        let (a, b, c) = (id_of(b"aaaa"), id_of(b"bbbb"), id_of(b"cccc"));
        cache.insert(a, b"aaaa".to_vec());
        cache.insert(b, b"bbbb".to_vec());
        assert!(cache.get(&a).is_some()); // a is now most recent
        cache.insert(c, b"cccc".to_vec()); // must evict b, not a
        assert!(cache.get(&a).is_some());
        assert!(cache.get(&b).is_none());
        assert!(cache.get(&c).is_some());
        assert_eq!(cache.used_bytes(), 8);
    }
}
