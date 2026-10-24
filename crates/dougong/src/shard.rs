// Rust guideline compliant 2026-10-24
//! Fixed-stripe striped maps (performance track P1, improve requirement 一.1).
//!
//! A [`ShardMap`] partitions one `HashMap` into a fixed number of
//! independently locked stripes. Keys map to stripes deterministically, so
//! operations on disjoint keys proceed concurrently while same-key
//! operations keep their natural serialization. Two rules keep the lock
//! topology deadlock-free:
//!
//! - a stripe lock is never held while acquiring another stripe lock or any
//!   other root-state structure lock (`for_each_shard` scans stripes
//!   sequentially, one lock at a time);
//! - a callback handed a locked stripe only reads or mutates that stripe's
//!   map — it never calls back into root state.

use parking_lot::Mutex;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Stripes per sharded structure. Sixteen spreads the hot maps across
/// typical worker counts (4–16 threads) while a full scan (invariant deep
/// checks, per-provider name collection) stays a handful of extra
/// uncontended lock acquisitions. The count is fixed — not host-derived — so
/// behavior is identical across machines and reproducible in tests.
const SHARD_COUNT: usize = 16;

/// A `HashMap` striped into [`SHARD_COUNT`] independently locked shards.
pub struct ShardMap<K, V> {
    shards: Box<[Mutex<HashMap<K, V>>]>,
}

impl<K: Hash + Eq, V> ShardMap<K, V> {
    /// Creates an empty striped map.
    pub(crate) fn new() -> Self {
        Self {
            shards: (0..SHARD_COUNT)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
        }
    }

    /// The stripe holding `key`. The stripe index uses the low bits of the
    /// hash (the count is a power of two); `DefaultHasher` is the same
    /// `SipHash` family the standard maps already use.
    fn stripe<Q>(&self, key: &Q) -> &Mutex<HashMap<K, V>>
    where
        Q: Hash + ?Sized,
    {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "only the low bits are used (masked by the stripe count); truncation is the point"
        )]
        let index = hasher.finish() as usize & (SHARD_COUNT - 1);
        &self.shards[index]
    }

    /// Runs `f` with the stripe holding `key` locked, passing the looked-up
    /// entry. The lock is released when `f` returns.
    pub(crate) fn read<Q, R>(&self, key: &Q, f: impl FnOnce(Option<&V>) -> R) -> R
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        f(self.stripe(key).lock().get(key))
    }

    /// Runs `f` with the stripe holding `key` locked, passing the whole
    /// stripe map so entry-API patterns stay available. `key` only selects
    /// the stripe; `f` must not touch data keyed outside it.
    pub(crate) fn write<Q, R>(&self, key: &Q, f: impl FnOnce(&mut HashMap<K, V>) -> R) -> R
    where
        Q: Hash + ?Sized,
    {
        f(&mut self.stripe(key).lock())
    }

    /// Visits every stripe in index order, holding one stripe lock at a
    /// time. The scan is not atomic across stripes; callers (invariant deep
    /// checks, diagnostics) tolerate that by construction.
    pub(crate) fn for_each_shard(&self, mut f: impl FnMut(&HashMap<K, V>)) {
        for shard in &self.shards {
            f(&shard.lock());
        }
    }

    /// Total entries across all stripes (diagnostics only).
    pub(crate) fn total_entries(&self) -> usize {
        let mut total = 0;
        self.for_each_shard(|map| total += map.len());
        total
    }
}

impl<K: Hash + Eq, V> Default for ShardMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Hash + Eq, V> std::fmt::Debug for ShardMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Contents stay behind their locks; the entry count is the useful
        // diagnostic and keeps this impl usable for every `K`/`V`.
        f.debug_struct("ShardMap")
            .field("entries", &self.total_entries())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn disjoint_keys_land_independent_stripes() {
        let map: ShardMap<u64, u64> = ShardMap::new();
        for key in 0..1000 {
            map.write(&key, |stripe| {
                stripe.insert(key, key * 2);
            });
        }
        assert_eq!(map.total_entries(), 1000);
        for key in 0..1000 {
            let value = map.read(&key, |entry: Option<&u64>| entry.copied());
            assert_eq!(value, Some(key * 2));
        }
    }

    #[test]
    fn same_key_serializes_writers() {
        let map = std::sync::Arc::new(ShardMap::new());
        let in_section = std::sync::Arc::new(AtomicUsize::new(0));
        let max_seen = std::sync::Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let map = std::sync::Arc::clone(&map);
            let in_section = std::sync::Arc::clone(&in_section);
            let max_seen = std::sync::Arc::clone(&max_seen);
            threads.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    map.write(&"hot", |stripe| {
                        let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        std::thread::yield_now();
                        let entry = stripe.entry("hot").or_insert(0_u64);
                        *entry += 1;
                        in_section.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }));
        }
        for thread in threads {
            thread.join().expect("writer thread panicked");
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
        assert_eq!(
            map.read(&"hot", |entry: Option<&u64>| entry.copied()),
            Some(400)
        );
    }

    #[test]
    fn str_key_reads_through_string_borrow() {
        let map: ShardMap<String, u64> = ShardMap::new();
        map.write("svc", |stripe| {
            stripe.insert("svc".to_string(), 7);
        });
        assert_eq!(
            map.read("svc", |entry: Option<&u64>| entry.copied()),
            Some(7)
        );
        assert_eq!(
            map.read("missing", |entry: Option<&u64>| entry.copied()),
            None
        );
    }
}
