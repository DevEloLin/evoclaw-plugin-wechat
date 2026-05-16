//! Per-conversation serialization for the WeChat plugin.
//!
//! When the plugin enables session persistence (see `evo channel run
//! --session-dir`), each user's history lives in a single jsonl file
//! keyed by `conversation_id`. Two concurrent messages from the same
//! WeChat fan must NOT process in parallel — they'd race on read /
//! load / write of that file, and (worse) the bridge's `pending` map is
//! keyed on conversation_id, so the second reply could be delivered to
//! the first caller.
//!
//! `ConvSerializer` is a `cid → tokio::Mutex` map. Different cids are
//! fully parallel; same cid is strictly serial.
//!
//! ## Design choices
//!
//! - **`std::sync::Mutex` around the outer map** matches the established
//!   `reply_cache: Arc<StdMutex<HashMap>>` precedent in `handler.rs:55`
//!   — no new `dashmap` dep needed for what is a tiny critical section
//!   (look up an entry, clone the Arc, drop the std mutex).
//! - **`tokio::sync::Mutex` for the per-cid inner lock** because the
//!   guarded section spans `bridge.checkout().await + bridge.ask().await`,
//!   which is asynchronous. A `std::sync::Mutex` would block the runtime.
//! - **GC on idle** evicts cid entries with no recent activity to keep
//!   the map's memory bounded. Active locks (`Arc::strong_count > 1`)
//!   are never evicted regardless of `last_seen` age.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

#[derive(Default)]
pub struct ConvSerializer {
    locks: StdMutex<HashMap<String, Entry>>,
}

struct Entry {
    lock: Arc<TokioMutex<()>>,
    last_seen: Instant,
}

impl ConvSerializer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or create) the per-cid mutex. Caller `.lock().await`s on the
    /// returned `Arc<Mutex>` to enter the critical section, then drops
    /// the guard. The `Arc` outlives the inner std-mutex critical section
    /// so we hold the outer lock for only a few instructions.
    pub fn acquire(&self, cid: &str) -> Arc<TokioMutex<()>> {
        let mut map = self.locks.lock().expect("conv_serializer outer lock");
        let entry = map.entry(cid.to_string()).or_insert_with(|| Entry {
            lock: Arc::new(TokioMutex::new(())),
            last_seen: Instant::now(),
        });
        entry.last_seen = Instant::now();
        entry.lock.clone()
    }

    /// Evict cid entries whose inner mutex is currently uncontended
    /// (`strong_count == 1`, i.e. only the map holds an Arc) AND have not
    /// been seen for `idle_threshold`. Returns the number of entries
    /// dropped, for diagnostics.
    ///
    /// Run from a background task — see `main.rs` for the loop body.
    pub fn gc(&self, idle_threshold: Duration) -> usize {
        let mut map = self.locks.lock().expect("conv_serializer outer lock");
        let cutoff = Instant::now() - idle_threshold;
        let before = map.len();
        map.retain(|_, entry| {
            // Keep if either currently in use OR recently active.
            Arc::strong_count(&entry.lock) > 1 || entry.last_seen > cutoff
        });
        before - map.len()
    }

    /// Test-only helper to inspect map size. Production code shouldn't
    /// need this — the GC log line is the operational signal.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.locks.lock().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn distinct_cids_run_in_parallel() {
        let s = Arc::new(ConvSerializer::new());
        let counter = Arc::new(AtomicU32::new(0));
        let max_concurrent = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        for i in 0..8 {
            let s = s.clone();
            let counter = counter.clone();
            let max = max_concurrent.clone();
            tasks.push(tokio::spawn(async move {
                let lock = s.acquire(&format!("cid_{i}"));
                let _g = lock.lock().await;
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            max_concurrent.load(Ordering::SeqCst) > 1,
            "distinct cids must overlap"
        );
    }

    #[tokio::test]
    async fn same_cid_serializes() {
        let s = Arc::new(ConvSerializer::new());
        let counter = Arc::new(AtomicU32::new(0));
        let max_concurrent = Arc::new(AtomicU32::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let s = s.clone();
            let counter = counter.clone();
            let max = max_concurrent.clone();
            tasks.push(tokio::spawn(async move {
                let lock = s.acquire("same_cid");
                let _g = lock.lock().await;
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "same cid must never have concurrent guards"
        );
    }

    #[tokio::test]
    async fn gc_evicts_idle_entries() {
        let s = Arc::new(ConvSerializer::new());
        // Touch 3 cids.
        let _ = s.acquire("a");
        let _ = s.acquire("b");
        let _ = s.acquire("c");
        assert_eq!(s.len(), 3);

        // Wait past the idle threshold.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let evicted = s.gc(Duration::from_millis(10));
        assert_eq!(evicted, 3);
        assert_eq!(s.len(), 0);
    }

    #[tokio::test]
    async fn gc_keeps_active_entries() {
        let s = Arc::new(ConvSerializer::new());
        let held = s.acquire("active");
        // Even past idle, an outstanding Arc must keep the entry.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let evicted = s.gc(Duration::from_millis(1));
        assert_eq!(evicted, 0);
        assert_eq!(s.len(), 1);
        drop(held);
    }
}
