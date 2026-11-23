//! Reservations of buffer-cache memory.
//!
//! A merge or a memtable seal holds buffers that the cache does not see: one
//! data block per input run, the output writer's open blocks, and its filter.
//! `compaction_memory` `charge` makes the job reserve those bytes in the cache's
//! budget for its whole length, as RocksDB's `CacheReservationManager` does.
//!
//! A reservation is a set of pinned dummy entries, which the cache never
//! evicts: an entry the cache could evict would end the charge while the
//! job's buffers stay allocated. The cache evicts unpinned blocks to make room
//! for them. The [`ReservationLedger`] bounds the pinned bytes with a cap; a
//! request over the cap waits until another reservation is released, except
//! that a request is admitted when nothing is reserved, so no request waits
//! forever on a cap smaller than itself.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use feldera_buffer_cache::{CacheEntry, SharedBufferCache};
use tokio::sync::Notify;

use super::CacheKey;
use crate::storage::backend::FileId;

/// The default cap on pinned bytes is this fraction of the cache's capacity.
///
/// Proposed, not decided (2026-11-23): see `docs/design/merged-index/memory-model.md`.
pub const RESERVATION_CAP_DIVISOR: usize = 4;

/// A reservation is split into pinned entries of at most `shard capacity /
/// CHUNKS_PER_SHARD`, so that the hash spreads it over the shards of the cache.
/// One entry as large as a shard would leave the shard no room for blocks.
const CHUNKS_PER_SHARD: usize = 16;

/// The smallest chunk, so that a small cache does not hold a reservation as
/// thousands of tiny entries.
const MIN_CHUNK_BYTES: usize = 4096;

/// A dummy cache entry that holds `cost` bytes of the budget and no data.
struct ReservedBytes {
    cost: usize,
}

impl CacheEntry for ReservedBytes {
    fn cost(&self) -> usize {
        self.cost
    }

    fn is_pinned(&self) -> bool {
        true
    }
}

/// What the reservations of one cache backend have cost so far.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReservationStats {
    /// Reservations that did not fit and waited.
    pub waits: u64,

    /// Total time those reservations waited.
    pub wait: Duration,

    /// The most bytes reserved at once, since the last
    /// [`reset_peak_pinned_bytes`](super::BufferCache::reset_peak_pinned_bytes).
    pub peak_pinned_bytes: usize,
}

impl std::ops::Add for ReservationStats {
    type Output = Self;

    /// Adds the stats of two distinct cache backends. The peaks add, which
    /// bounds the peak of the two together.
    fn add(self, other: Self) -> Self {
        Self {
            waits: self.waits + other.waits,
            wait: self.wait + other.wait,
            peak_pinned_bytes: self.peak_pinned_bytes + other.peak_pinned_bytes,
        }
    }
}

/// The reservation accounting of one cache backend, shared by every
/// [`BufferCache`](super::BufferCache) handle on that backend.
pub(super) struct ReservationLedger {
    /// Pinned bytes above which a reservation waits, unless it is the only one.
    cap_bytes: usize,

    /// Bytes reserved now.
    pinned_bytes: Mutex<usize>,

    /// Woken, for all waiters, whenever a reservation is released.
    released: Notify,

    waits: AtomicU64,
    wait_nanos: AtomicU64,
    peak_pinned_bytes: AtomicUsize,
}

impl ReservationLedger {
    pub(super) fn new(cap_bytes: usize) -> Self {
        Self {
            cap_bytes,
            pinned_bytes: Mutex::new(0),
            released: Notify::new(),
            waits: AtomicU64::new(0),
            wait_nanos: AtomicU64::new(0),
            peak_pinned_bytes: AtomicUsize::new(0),
        }
    }

    /// Admits `bytes` if they fit under the cap or nothing else is reserved.
    fn try_admit(&self, bytes: usize) -> bool {
        let mut pinned = self.pinned_bytes.lock().unwrap();
        if *pinned > 0 && pinned.saturating_add(bytes) > self.cap_bytes {
            return false;
        }
        *pinned += bytes;
        self.peak_pinned_bytes.fetch_max(*pinned, Ordering::Relaxed);
        true
    }

    /// Takes `bytes` back and wakes the waiting reservations.
    fn release(&self, bytes: usize) {
        *self.pinned_bytes.lock().unwrap() -= bytes;
        self.released.notify_waiters();
    }

    pub(super) fn pinned_bytes(&self) -> usize {
        *self.pinned_bytes.lock().unwrap()
    }

    pub(super) fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    pub(super) fn stats(&self) -> ReservationStats {
        ReservationStats {
            waits: self.waits.load(Ordering::Relaxed),
            wait: Duration::from_nanos(self.wait_nanos.load(Ordering::Relaxed)),
            peak_pinned_bytes: self.peak_pinned_bytes.load(Ordering::Relaxed),
        }
    }

    pub(super) fn reset_peak_pinned_bytes(&self) {
        let pinned = *self.pinned_bytes.lock().unwrap();
        self.peak_pinned_bytes.store(pinned, Ordering::Relaxed);
    }
}

/// Counts one wait and adds its length to the total when dropped, so that a
/// waiter that is cancelled still reports the time it waited.
struct WaitRecorder<'a> {
    ledger: &'a ReservationLedger,
    started: Instant,
}

impl<'a> WaitRecorder<'a> {
    fn start(ledger: &'a ReservationLedger) -> Self {
        ledger.waits.fetch_add(1, Ordering::Relaxed);
        Self {
            ledger,
            started: Instant::now(),
        }
    }
}

impl Drop for WaitRecorder<'_> {
    fn drop(&mut self) {
        self.ledger
            .wait_nanos
            .fetch_add(self.started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Bytes of the cache's budget held for one job. Dropping it, at the end of
/// the job or on an error, removes its pinned entries and wakes the waiters.
#[must_use = "a reservation is released when it is dropped"]
pub struct Reservation {
    cache: SharedBufferCache<CacheKey, Arc<dyn CacheEntry>>,
    ledger: Arc<ReservationLedger>,

    /// The fresh file id that keys this reservation's entries: no file uses it.
    file_id: FileId,

    /// The entries are `(file_id, 0)` to `(file_id, chunks - 1)`.
    chunks: u64,

    bytes: usize,
}

impl Reservation {
    /// The number of bytes this reservation holds.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        for chunk in 0..self.chunks {
            self.cache.remove(&CacheKey::new(self.file_id, chunk));
        }
        self.ledger.release(self.bytes);
    }
}

/// The reservation operations of a cache backend.
pub(super) struct Reserver<'a> {
    pub(super) cache: &'a SharedBufferCache<CacheKey, Arc<dyn CacheEntry>>,
    pub(super) ledger: &'a Arc<ReservationLedger>,
}

impl Reserver<'_> {
    /// Reserves `bytes` if they fit under the cap, or if nothing else is
    /// reserved; returns `None` otherwise.
    pub(super) fn try_reserve(&self, bytes: usize) -> Option<Reservation> {
        if !self.ledger.try_admit(bytes) {
            return None;
        }
        let file_id = FileId::new();
        let chunk_bytes = self.chunk_bytes();
        let chunks = bytes.div_ceil(chunk_bytes) as u64;
        for chunk in 0..chunks {
            let cost = chunk_bytes.min(bytes - chunk as usize * chunk_bytes);
            self.cache.insert(
                CacheKey::new(file_id, chunk),
                Arc::new(ReservedBytes { cost }),
            );
        }
        Some(Reservation {
            cache: self.cache.clone(),
            ledger: self.ledger.clone(),
            file_id,
            chunks,
            bytes,
        })
    }

    /// Reserves `bytes`, waiting without blocking the thread until another
    /// reservation is released if they do not fit.
    ///
    /// The merger threads run merges and seals as tokio tasks, so the wait is
    /// an `await`: the task yields its worker thread to the other merges
    /// meanwhile. A blocking wait would hold a worker thread, and with it a
    /// runnable merge, for as long as the reservations ahead take.
    pub(super) async fn reserve(&self, bytes: usize) -> Reservation {
        let mut wait = None;
        loop {
            // Register for the wake-up before trying, so that a release between the
            // failed attempt and the await is not missed.
            let released = self.ledger.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if let Some(reservation) = self.try_reserve(bytes) {
                return reservation;
            }
            wait.get_or_insert_with(|| WaitRecorder::start(self.ledger));
            released.await;
        }
    }

    /// The size of one pinned entry: a sixteenth of a shard.
    fn chunk_bytes(&self) -> usize {
        let shard_bytes = self.cache.total_capacity() / self.cache.shard_count();
        (shard_bytes / CHUNKS_PER_SHARD).max(MIN_CHUNK_BYTES)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use feldera_buffer_cache::{BufferCacheBuilder, CacheEntry};
    use feldera_types::config::dev_tweaks::{BufferCacheAllocationStrategy, BufferCacheStrategy};

    use super::{CacheKey, FileId};
    use crate::storage::buffer_cache::{BufferCache, cache::build_buffer_caches};

    const MIB: usize = 1 << 20;

    struct Block(usize);

    impl CacheEntry for Block {
        fn cost(&self) -> usize {
            self.0
        }
    }

    /// An 8 MiB cache on `strategy`, so a reservation cap of 2 MiB.
    fn cache(strategy: BufferCacheStrategy) -> BufferCache {
        BufferCache::from_inner(
            BufferCacheBuilder::<CacheKey, Arc<dyn CacheEntry>>::new()
                .with_buffer_cache_strategy(strategy)
                .build_single(8 * MIB),
        )
    }

    fn insert_blocks(cache: &BufferCache, count: usize) {
        let file = FileId::new();
        for block in 0..count {
            cache.insert(file, block as u64 * 4096, Arc::new(Block(4096)));
        }
    }

    const STRATEGIES: [BufferCacheStrategy; 2] =
        [BufferCacheStrategy::Lru, BufferCacheStrategy::S3Fifo];

    #[test]
    fn reservation_survives_eviction_pressure_and_frees_its_bytes_on_drop() {
        for strategy in STRATEGIES {
            let cache = cache(strategy);
            let reservation = cache.try_reserve(MIB).unwrap();
            assert_eq!(reservation.bytes(), MIB);
            assert_eq!(cache.pinned_bytes(), MIB);

            // Twice the cache's capacity in blocks.
            insert_blocks(&cache, 4 * 1024);
            for chunk in 0..reservation.chunks {
                assert!(
                    cache
                        .inner
                        .contains_key(&CacheKey::new(reservation.file_id, chunk)),
                    "{strategy:?}: chunk {chunk} was evicted"
                );
            }
            let (occupied, capacity) = cache.occupancy();
            assert!(
                occupied >= MIB,
                "{strategy:?}: the pin is part of the charge"
            );
            assert!(
                occupied <= capacity,
                "{strategy:?}: blocks were not evicted"
            );

            drop(reservation);
            assert_eq!(cache.pinned_bytes(), 0);
            assert_eq!(cache.occupancy().0, occupied - MIB, "{strategy:?}");
        }
    }

    #[test]
    fn a_reservation_over_the_cap_is_refused_unless_it_is_alone() {
        for strategy in STRATEGIES {
            let cache = cache(strategy);
            let cap = cache.reservation_cap_bytes();
            assert_eq!(cap, 2 * MIB);

            let first = cache.try_reserve(cap / 2).unwrap();
            let second = cache.try_reserve(cap / 2).unwrap();
            assert!(cache.try_reserve(1).is_none(), "{strategy:?}: over the cap");
            drop(first);
            assert!(cache.try_reserve(cap / 2 + 1).is_none());
            let third = cache.try_reserve(cap / 2).unwrap();
            drop((second, third));

            // Alone, a job larger than the cap is admitted, and then excludes the rest.
            let large = cache.try_reserve(cap + MIB).unwrap();
            assert!(cache.try_reserve(1).is_none(), "{strategy:?}");
            drop(large);
            assert_eq!(cache.pinned_bytes(), 0);
            assert!(cache.try_reserve(0).is_some());
        }
    }

    #[test]
    fn a_reservation_that_does_not_fit_waits_for_a_release() {
        let cache = Arc::new(cache(BufferCacheStrategy::S3Fifo));
        let cap = cache.reservation_cap_bytes();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let held = cache.try_reserve(cap).unwrap();
            let waiter = {
                let cache = cache.clone();
                tokio::spawn(async move { cache.reserve(MIB).await })
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!waiter.is_finished(), "the job did not wait");
            assert_eq!(cache.reservation_stats().waits, 1);

            drop(held);
            let granted = waiter.await.unwrap();
            assert_eq!(granted.bytes(), MIB);
            let stats = cache.reservation_stats();
            assert_eq!(stats.waits, 1);
            assert!(stats.wait >= Duration::from_millis(40), "{stats:?}");
            assert_eq!(stats.peak_pinned_bytes, cap);
            drop(granted);

            // A reservation that fits does not wait.
            drop(cache.reserve(MIB).await);
            assert_eq!(cache.reservation_stats().waits, 1);
        });
    }

    #[test]
    fn the_peak_restarts_from_the_current_level() {
        let cache = cache(BufferCacheStrategy::Lru);
        let held = cache.try_reserve(MIB).unwrap();
        drop(cache.try_reserve(MIB / 2).unwrap());
        assert_eq!(cache.reservation_stats().peak_pinned_bytes, MIB + MIB / 2);
        cache.reset_peak_pinned_bytes();
        assert_eq!(cache.reservation_stats().peak_pinned_bytes, MIB);
        drop(held);
    }

    #[test]
    fn handles_on_one_backend_share_one_ledger() {
        let caches = build_buffer_caches(
            2,
            32 * MIB,
            BufferCacheStrategy::S3Fifo,
            None,
            BufferCacheAllocationStrategy::Global,
        );
        let backends: HashSet<usize> = caches
            .iter()
            .flat_map(|slots| slots.values())
            .map(|cache| cache.backend_id())
            .collect();
        assert_eq!(backends.len(), 1);
        let first = &caches[0].values().next().unwrap();
        let second = &caches[1].values().next().unwrap();
        let held = first.try_reserve(first.reservation_cap_bytes()).unwrap();
        assert_eq!(second.pinned_bytes(), first.reservation_cap_bytes());
        assert!(second.try_reserve(1).is_none());
        drop(held);
        assert!(second.try_reserve(1).is_some());
    }
}
