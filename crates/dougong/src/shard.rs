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
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

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

// ---------------------------------------------------------------------------
// Active-fiber membership mirror (performance track P4, improve 一.4)
// ---------------------------------------------------------------------------

/// Bitmap words: 4096 × 64 = 262,144 dense uids resolved lock-free; the
/// 32 KiB boxed array is a one-time fixed allocation per root state (same
/// fixed-count philosophy as [`SHARD_COUNT`]).
const ACTIVE_WORDS: usize = 4096;
/// Bits per bitmap word: an `AtomicU64` is 64 bits wide by definition.
const WORD_BITS: u64 = 64;
/// uid bound covered by the bitmap; uids at or beyond it use the tail set.
const ACTIVE_BITS: u64 = ACTIVE_WORDS as u64 * WORD_BITS;

/// Lock-free membership mirror of the active-fiber set (P4).
///
/// Fiber uids are dense, monotonic, and never reused (S3), so membership
/// maps onto a fixed bitmap indexed directly by uid; uids at or beyond the
/// bitmap capacity fall back to a mutex-guarded tail set (cold by
/// construction: reachable only after 262,144 fibers).
///
/// Ordering contract: the set is a *mirror* — authoritative liveness lives
/// in each fiber's own state. Strict resolution carries its happens-before
/// through the reflect store's stripe locks (the provider's insert releases
/// the stripe, the reader acquires it), so flips only need per-operation
/// atomicity: `AcqRel` read-modify-writes and `Acquire` reads, never
/// `SeqCst`. A transient stale read (the provider's `mark_active` landing
/// after its store insert) exists identically in the pre-P4 mutex version
/// and self-heals through the post-load republish-before-notify order plus
/// `check_impl` re-resolution.
pub struct ActiveSet {
    words: Box<[AtomicU64]>,
    tail: Mutex<HashSet<u64>>,
}

impl ActiveSet {
    pub fn new() -> Self {
        Self {
            words: (0..ACTIVE_WORDS).map(|_| AtomicU64::new(0)).collect(),
            tail: Mutex::new(HashSet::new()),
        }
    }

    /// Marks `uid` active (idempotent).
    pub fn mark(&self, uid: u64) {
        if uid < ACTIVE_BITS {
            let (word, bit) = Self::locate(uid);
            self.words[word].fetch_or(bit, Ordering::AcqRel);
        } else {
            self.tail.lock().insert(uid);
        }
    }

    /// Marks `uid` inactive (idempotent; a no-op for unknown uids).
    pub fn unmark(&self, uid: u64) {
        if uid < ACTIVE_BITS {
            let (word, bit) = Self::locate(uid);
            self.words[word].fetch_and(!bit, Ordering::AcqRel);
        } else {
            self.tail.lock().remove(&uid);
        }
    }

    /// True when `uid` is currently marked active.
    #[must_use]
    pub fn contains(&self, uid: u64) -> bool {
        if uid < ACTIVE_BITS {
            let (word, bit) = Self::locate(uid);
            self.words[word].load(Ordering::Acquire) & bit != 0
        } else {
            self.tail.lock().contains(&uid)
        }
    }

    /// Splits `uid` into its bitmap word index and bit. Callers pass
    /// `uid < ACTIVE_BITS`, so the word index is below `ACTIVE_WORDS` and
    /// always fits `usize`.
    fn locate(uid: u64) -> (usize, u64) {
        let word = usize::try_from(uid >> 6).unwrap_or_else(|_| {
            unreachable!("word index out of bitmap range") // cannot happen: callers pass uid < ACTIVE_BITS (word index < ACTIVE_WORDS)
        });
        (word, 1_u64 << (uid % WORD_BITS))
    }
}

impl Default for ActiveSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

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

    #[test]
    fn active_set_roundtrip_and_neighbor_bits() {
        let set = ActiveSet::new();
        assert!(!set.contains(5));
        set.mark(5);
        set.mark(5);
        assert!(set.contains(5), "mark is idempotent");
        set.mark(6);
        set.unmark(5);
        assert!(!set.contains(5));
        assert!(
            set.contains(6),
            "unmarking one uid must not clear its neighbor"
        );
        set.unmark(6);
        assert!(!set.contains(6));
        set.unmark(6);
        assert!(!set.contains(6), "unmark is idempotent");
    }

    #[test]
    fn active_set_bitmap_boundary_and_overflow_tail() {
        let set = ActiveSet::new();
        let last_bitmap = ACTIVE_BITS - 1;
        set.mark(last_bitmap);
        assert!(set.contains(last_bitmap), "last bitmap uid stays in words");
        assert!(!set.contains(ACTIVE_BITS), "first tail uid starts empty");
        set.mark(ACTIVE_BITS);
        set.mark(ACTIVE_BITS + 42);
        assert!(set.contains(ACTIVE_BITS));
        assert!(set.contains(ACTIVE_BITS + 42));
        set.unmark(ACTIVE_BITS);
        assert!(!set.contains(ACTIVE_BITS));
        assert!(set.contains(ACTIVE_BITS + 42), "tail removals are per-uid");
        set.unmark(last_bitmap);
        assert!(!set.contains(last_bitmap));
    }
}
