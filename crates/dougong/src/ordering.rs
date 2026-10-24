// Rust guideline compliant 2026-10-24
//! Deterministic ordering for dependency updates (S6, design decision D21).
//!
//! Every mutation of a reflect-store slot runs together with its dependent
//! notification (candidate recompute + fiber refresh + `internal/service`
//! emission) as one serial section keyed by `(label, name)`. Two updates to
//! the *same* slot can therefore never interleave: the withdrawal event of a
//! replacement can no longer be lost, and the event-generation order always
//! matches the commit order. Updates to *different* slots proceed
//! concurrently — serialization is scoped to the group, never global
//! (improve requirement 二.6).
//!
//! The sections are await-free, so a plain keyed mutex is the honest
//! mechanism: for truly concurrent submitters the relative order is defined
//! by the scheduler either way, and a task queue would add a worker hop with
//! no semantic gain (a coalescing queue remains a possible P3 seam behind
//! the same semantics). Event *observation* order on the listener side stays
//! governed by the existing C5/C7 contracts (dispatched on spawned tasks).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Identity of one service group: the reflect-store slot `(label, name)`.
///
/// The label is the provider-scope `primary_label`, so every call site
/// touching the same slot derives the same key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey {
    label: u64,
    name: String,
}

impl GroupKey {
    /// Keys the slot `name` lands in under `label`.
    pub fn new(label: u64, name: &str) -> Self {
        Self {
            label,
            name: name.to_string(),
        }
    }
}

/// Per-service-group serial sections for dependency updates.
///
/// Keys live for the runtime's lifetime, mirroring how the reflect store
/// and the event store retain per-service entries. Lock ordering: a group
/// lock is always the *outermost* lock (taken before reflect/index/fiber
/// locks inside the section), and no path holding one of those locks ever
/// acquires a group lock, so the order is acyclic.
#[derive(Debug, Default)]
pub struct UpdateSerializers {
    locks: Mutex<HashMap<GroupKey, Arc<Mutex<()>>>>,
}

impl UpdateSerializers {
    /// Runs `f` inside the serial section of `key`'s service group.
    ///
    /// Concurrent sections with the same key execute in some total order
    /// (never torn); sections with different keys never block each other.
    pub fn run<T>(&self, key: &GroupKey, f: impl FnOnce() -> T) -> T {
        let lock = {
            let mut locks = self.locks.lock();
            Arc::clone(locks.entry(key.clone()).or_default())
        };
        let _guard = lock.lock();
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn same_key_sections_never_overlap() {
        let serializers = Arc::new(UpdateSerializers::default());
        let in_section = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let key = GroupKey::new(0, "svc");
        let mut threads = Vec::new();
        for _ in 0..8 {
            let serializers = Arc::clone(&serializers);
            let in_section = Arc::clone(&in_section);
            let max_seen = Arc::clone(&max_seen);
            let key = key.clone();
            threads.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    serializers.run(&key, || {
                        let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        std::thread::yield_now();
                        in_section.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            }));
        }
        for thread in threads {
            thread.join().expect("worker thread panicked");
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn different_keys_do_not_block_each_other() {
        let serializers = Arc::new(UpdateSerializers::default());
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let serializers_a = Arc::clone(&serializers);
        let entered_a = Arc::clone(&entered);
        let release_a = Arc::clone(&release);
        let holder = std::thread::spawn(move || {
            serializers_a.run(&GroupKey::new(0, "svc-a"), || {
                // Signal "inside A's section", then stay until the main
                // thread proved B's section completed underneath.
                entered_a.wait();
                release_a.wait();
            });
        });
        entered.wait();
        let done = std::thread::spawn({
            let serializers = Arc::clone(&serializers);
            move || serializers.run(&GroupKey::new(0, "svc-b"), || 42)
        });
        let value = done.join().expect("probe thread panicked");
        assert_eq!(value, 42, "a different key must not wait on a held key");
        release.wait();
        holder.join().expect("holder thread panicked");
    }

    #[test]
    fn same_key_reuses_one_lock() {
        let serializers = UpdateSerializers::default();
        serializers.run(&GroupKey::new(7, "svc"), || {});
        serializers.run(&GroupKey::new(7, "svc"), || {});
        serializers.run(&GroupKey::new(8, "svc"), || {});
        serializers.run(&GroupKey::new(7, "other"), || {});
        assert_eq!(serializers.locks.lock().len(), 3);
    }
}
