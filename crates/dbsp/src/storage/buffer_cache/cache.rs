mod reservation;

use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Display};
use std::ops::Range;
use std::ops::{Add, AddAssign};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use enum_map::{Enum, EnumMap};
use feldera_buffer_cache::{
    BufferCacheBuilder, CacheEntry, LruCache, SharedBufferCache, ThreadType,
};
use feldera_types::config::dev_tweaks::{BufferCacheAllocationStrategy, BufferCacheStrategy};
use serde::{Deserialize, Serialize};
use size_of::SizeOf;

use crate::Runtime;
use crate::circuit::metadata::{
    CACHE_BACKGROUND_HIT_RATE_PERCENT, CACHE_BACKGROUND_HITS, CACHE_BACKGROUND_MISSES,
    CACHE_FOREGROUND_HIT_RATE_PERCENT, CACHE_FOREGROUND_HITS, CACHE_FOREGROUND_MISSES, MetaItem,
    MetricId, MetricReading, OperatorMeta,
};
use crate::circuit::runtime::current_thread_type;
use crate::storage::backend::{BlockLocation, FileId, FileReader};

pub use reservation::{Reservation, ReservationStats};
use reservation::{RESERVATION_CAP_DIVISOR, ReservationLedger, Reserver};

/// A key for the block cache.
///
/// The block size could be part of the key, but we'll never read a given offset
/// with more than one size so it's also not necessary.
///
/// It's important that the sort order is by `fd` first and `offset` second.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CacheKey {
    /// File being cached.
    file_id: FileId,

    /// Offset in file.
    offset: u64,
}

impl CacheKey {
    fn new(file_id: FileId, offset: u64) -> Self {
        Self { file_id, offset }
    }

    /// Returns the key range that covers every cached block for `file_id`.
    fn file_range(file_id: FileId) -> Range<Self> {
        Self { file_id, offset: 0 }..Self {
            file_id: file_id.after(),
            offset: 0,
        }
    }
}

/// A cache on top of a storage [backend](crate::storage::backend).
pub struct BufferCache {
    inner: SharedBufferCache<CacheKey, Arc<dyn CacheEntry>>,

    /// The reservations on `inner`, shared by every handle on the same backend.
    ledger: Arc<ReservationLedger>,
}

impl Debug for BufferCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferCache")
            .field("strategy", &self.strategy())
            .finish()
    }
}

impl BufferCache {
    /// Creates a new cache using the default [`BufferCacheStrategy`].
    ///
    /// `max_cost` limits the size of the cache in terms of [CacheEntry::cost].
    pub fn new(max_cost: usize) -> Self {
        Self::from_inner(
            BufferCacheBuilder::<CacheKey, Arc<dyn CacheEntry>>::new().build_single(max_cost),
        )
    }

    /// Returns the strategy backing this cache.
    pub fn strategy(&self) -> BufferCacheStrategy {
        self.inner.strategy()
    }

    pub fn get(
        &self,
        file: &dyn FileReader,
        location: BlockLocation,
    ) -> Option<Arc<dyn CacheEntry>> {
        self.inner
            .get(CacheKey::new(file.file_id(), location.offset))
    }

    pub fn insert(&self, file_id: FileId, offset: u64, aux: Arc<dyn CacheEntry>) {
        self.inner.insert(CacheKey::new(file_id, offset), aux);
    }

    pub fn evict(&self, file: &dyn FileReader) {
        let eager_evict = Runtime::with_dev_tweaks(|d| d.eager_evict.unwrap_or(false));
        if !eager_evict {
            return;
        }

        let file_id = file.file_id();
        if let Some(lru) = self
            .inner
            .as_any()
            .downcast_ref::<LruCache<CacheKey, Arc<dyn CacheEntry>>>()
        {
            lru.remove_range(CacheKey::file_range(file_id));
        } else {
            let predicate = |key: &CacheKey| key.file_id == file_id;
            self.inner.remove_if(&predicate);
        }
    }

    /// Removes every cached entry of the files in `files` (data and index
    /// blocks, trailers, and storage-resident filter partitions) in one pass
    /// over the cache, whatever the `eager_evict` dev tweak says.
    ///
    /// Returns the cache cost freed, measured as the occupancy before less the
    /// occupancy after; entries other threads insert or evict meanwhile make it
    /// approximate.
    pub fn evict_files(&self, files: &HashSet<FileId>) -> usize {
        if files.is_empty() {
            return 0;
        }
        let before = self.inner.total_charge();
        let predicate = |key: &CacheKey| files.contains(&key.file_id);
        self.inner.remove_if(&predicate);
        before.saturating_sub(self.inner.total_charge())
    }

    /// Returns `(cur_cost, max_cost)`, reporting the amount of the cache that
    /// is currently used and the maximum value, both denominated in terms of
    /// [CacheEntry::cost] for `CacheEntry`.
    pub fn occupancy(&self) -> (usize, usize) {
        (self.inner.total_charge(), self.inner.total_capacity())
    }

    /// Builds a wrapper around an already-constructed cache backend, with a
    /// reservation ledger of its own.
    fn from_inner(inner: SharedBufferCache<CacheKey, Arc<dyn CacheEntry>>) -> Self {
        let ledger = Arc::new(Self::new_ledger(&inner));
        Self { inner, ledger }
    }

    /// Builds a ledger whose cap is a fraction of `inner`'s capacity.
    fn new_ledger(inner: &SharedBufferCache<CacheKey, Arc<dyn CacheEntry>>) -> ReservationLedger {
        ReservationLedger::new(inner.total_capacity() / RESERVATION_CAP_DIVISOR)
    }

    fn reserver(&self) -> Reserver<'_> {
        Reserver {
            cache: &self.inner,
            ledger: &self.ledger,
        }
    }

    /// Reserves `bytes` of the cache's budget if they fit under the cap on
    /// pinned bytes, or if nothing else is reserved; returns `None` otherwise.
    ///
    /// The reservation is a set of pinned entries that the cache never
    /// evicts; it evicts other blocks to make room for them. Dropping the
    /// [`Reservation`] removes the entries.
    pub fn try_reserve(&self, bytes: usize) -> Option<Reservation> {
        self.reserver().try_reserve(bytes)
    }

    /// Reserves `bytes` as [`try_reserve`](Self::try_reserve) does, but waits
    /// for another reservation to be released if they do not fit.
    ///
    /// The wait is an `await`, so it does not hold an executor thread. It
    /// counts in [`reservation_stats`](Self::reservation_stats).
    pub async fn reserve(&self, bytes: usize) -> Reservation {
        self.reserver().reserve(bytes).await
    }

    /// The most bytes that reservations may pin before a further one waits.
    pub fn reservation_cap_bytes(&self) -> usize {
        self.ledger.cap_bytes()
    }

    /// The bytes reserved now.
    pub fn pinned_bytes(&self) -> usize {
        self.ledger.pinned_bytes()
    }

    /// The waits and the peak of pinned bytes of this cache backend.
    pub fn reservation_stats(&self) -> ReservationStats {
        self.ledger.stats()
    }

    /// Starts a new peak of pinned bytes from the current level, so that the
    /// next [`reservation_stats`](Self::reservation_stats) reports the peak
    /// since this call.
    pub fn reset_peak_pinned_bytes(&self) {
        self.ledger.reset_peak_pinned_bytes()
    }

    /// Returns an identifier for the shared cache backend used by this wrapper.
    pub(crate) fn backend_id(&self) -> usize {
        Arc::as_ptr(&self.inner) as *const () as usize
    }

    /// Returns `true` if `self` and `other` share the same cache backend.
    #[cfg(test)]
    pub(crate) fn shares_backend_with(&self, other: &Self) -> bool {
        self.backend_id() == other.backend_id()
    }
}

/// Creates the runtime cache layout for DBSP worker pairs.
pub(crate) fn build_buffer_caches(
    worker_pairs: usize,
    total_capacity_bytes: usize,
    strategy: BufferCacheStrategy,
    max_buckets: Option<usize>,
    allocation_strategy: BufferCacheAllocationStrategy,
) -> Vec<EnumMap<ThreadType, Arc<BufferCache>>> {
    // Slots that share a backend share its reservation ledger, so that the
    // cap bounds the pinned bytes of the backend, not of each handle.
    let mut ledgers: HashMap<usize, Arc<ReservationLedger>> = HashMap::new();
    BufferCacheBuilder::<CacheKey, Arc<dyn CacheEntry>>::new()
        .with_buffer_cache_strategy(strategy)
        .with_buffer_max_buckets(max_buckets)
        .with_buffer_cache_allocation_strategy(allocation_strategy)
        .build(worker_pairs, total_capacity_bytes)
        .into_iter()
        .map(|caches| {
            EnumMap::from_fn(|thread_type| {
                let inner = caches[thread_type].clone();
                let backend_id = Arc::as_ptr(&inner) as *const () as usize;
                let ledger = ledgers
                    .entry(backend_id)
                    .or_insert_with(|| Arc::new(BufferCache::new_ledger(&inner)))
                    .clone();
                Arc::new(BufferCache { inner, ledger })
            })
        })
        .collect()
}

/// Cache statistics that can be accessed atomically for multithread updates.
#[derive(Debug, Default, SizeOf)]
#[size_of(skip_all)]
pub struct AtomicCacheStats(EnumMap<ThreadType, EnumMap<CacheAccess, AtomicCacheCounts>>);

impl AtomicCacheStats {
    /// Records that `location` was access in the cache with effect `access`.
    pub fn record(&self, access: CacheAccess, duration: Duration, location: BlockLocation) {
        let Some(thread_type) = current_thread_type() else {
            // TODO: record stats for aux threads.
            return;
        };
        self.0[thread_type][access].record(duration, location);
    }

    /// Fast path for cache hits: skips the timestamp call. Hits are nearly
    /// instantaneous (the latency that matters lives on the miss path), so we
    /// record only count and bytes.
    pub fn record_hit(&self, location: BlockLocation) {
        let Some(thread_type) = current_thread_type() else {
            return;
        };
        self.0[thread_type][CacheAccess::Hit].record_untimed(location);
    }

    /// Records a cache miss with its elapsed time (covers the actual I/O).
    pub fn record_miss(&self, duration: Duration, location: BlockLocation) {
        let Some(thread_type) = current_thread_type() else {
            return;
        };
        self.0[thread_type][CacheAccess::Miss].record(duration, location);
    }

    /// Reads out the statistics for processing.
    pub fn read(&self) -> CacheStats {
        CacheStats(EnumMap::from_fn(|thread_type| {
            EnumMap::from_fn(|access| self.0[thread_type][access].read())
        }))
    }
}

/// Whether a cache access was a hit or a miss.
#[derive(Copy, Clone, Debug, Enum)]
pub enum CacheAccess {
    /// Cache hit.
    Hit,

    /// Cache miss.
    Miss,
}

fn cache_metric(thread_type: ThreadType, access: CacheAccess) -> MetricId {
    match (thread_type, access) {
        (ThreadType::Foreground, CacheAccess::Hit) => CACHE_FOREGROUND_HITS,
        (ThreadType::Foreground, CacheAccess::Miss) => CACHE_FOREGROUND_MISSES,
        (ThreadType::Background, CacheAccess::Hit) => CACHE_BACKGROUND_HITS,
        (ThreadType::Background, CacheAccess::Miss) => CACHE_BACKGROUND_MISSES,
    }
}

fn cache_hit_rate_metric(thread_type: ThreadType) -> MetricId {
    match thread_type {
        ThreadType::Foreground => CACHE_FOREGROUND_HIT_RATE_PERCENT,
        ThreadType::Background => CACHE_BACKGROUND_HIT_RATE_PERCENT,
    }
}

impl Display for CacheAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hit => write!(f, "hits"),
            Self::Miss => write!(f, "misses"),
        }
    }
}

/// Cache counts that can be accessed atomically for multithread updates.
#[derive(Debug, Default)]
pub struct AtomicCacheCounts {
    /// Number of accessed blocks.
    count: AtomicU64,

    /// Total bytes across all the accesses.
    bytes: AtomicU64,

    /// Elapsed time, in nanoseconds.
    elapsed_ns: AtomicU64,
}

impl AtomicCacheCounts {
    pub fn record(&self, duration: Duration, location: BlockLocation) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(location.size as u64, Ordering::Relaxed);
        self.elapsed_ns
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Records an access without timing. Used on the cache-hit fast path,
    /// where `Instant::now()` + `elapsed()` would dominate the work; hits
    /// are nanoseconds anyway so the timing has no useful meaning.
    pub fn record_untimed(&self, location: BlockLocation) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(location.size as u64, Ordering::Relaxed);
    }

    /// Reads out the counts for processing.
    fn read(&self) -> CacheCounts {
        CacheCounts {
            count: self.count.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            elapsed: Duration::from_nanos(self.elapsed_ns.load(Ordering::Relaxed)),
        }
    }
}

/// Cache access statistics.
#[derive(Copy, Clone, Debug, Default)]
pub struct CacheStats(pub EnumMap<ThreadType, EnumMap<CacheAccess, CacheCounts>>);

impl CacheStats {
    /// Adds metadata for these cache statistics to `meta`, if they are nonzero.
    pub fn metadata(&self, meta: &mut OperatorMeta) {
        for (thread_type, accesses) in &self.0 {
            if !accesses.values().all(CacheCounts::is_empty) {
                meta.extend(accesses.iter().map(|(access, counts)| {
                    MetricReading::new(
                        cache_metric(thread_type, access),
                        Vec::new(),
                        counts.meta_item(),
                    )
                }));

                let hits = accesses[CacheAccess::Hit].count;
                let misses = accesses[CacheAccess::Miss].count;
                meta.extend([MetricReading::new(
                    cache_hit_rate_metric(thread_type),
                    Vec::new(),
                    MetaItem::Percent {
                        numerator: hits,
                        denominator: hits + misses,
                    },
                )]);
            }
        }
    }
}

impl Add for CacheStats {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self::Output {
        self.add_assign(rhs);
        self
    }
}

impl AddAssign for CacheStats {
    fn add_assign(&mut self, rhs: Self) {
        for (thread_type, accesses) in &mut self.0 {
            for (access, counts) in accesses {
                *counts += rhs.0[thread_type][access];
            }
        }
    }
}

/// Cache counts.
#[derive(Copy, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheCounts {
    /// Number of accessed blocks.
    pub count: u64,

    /// Total bytes across all the accesses.
    pub bytes: u64,

    /// Elapsed time.
    pub elapsed: Duration,
}

impl CacheCounts {
    /// Are the counts all zero?
    pub fn is_empty(&self) -> bool {
        self.count == 0 && self.bytes == 0
    }

    /// Returns a [MetaItem] for these cache counts.
    pub fn meta_item(&self) -> MetaItem {
        MetaItem::CacheCounts(*self)
    }
}

impl Add for CacheCounts {
    type Output = Self;

    fn add(mut self, rhs: Self) -> Self::Output {
        self.add_assign(rhs);
        self
    }
}

impl AddAssign for CacheCounts {
    fn add_assign(&mut self, rhs: Self) {
        self.count += rhs.count;
        self.bytes += rhs.bytes;
        self.elapsed += rhs.elapsed;
    }
}

#[cfg(test)]
mod evict_files_tests {
    use super::{BufferCache, CacheKey};
    use crate::storage::backend::FileId;
    use feldera_buffer_cache::CacheEntry;
    use std::collections::HashSet;
    use std::sync::Arc;

    struct Block(usize);

    impl CacheEntry for Block {
        fn cost(&self) -> usize {
            self.0
        }
    }

    /// `evict_files` removes the named files' entries, every offset of them, and only those.
    #[test]
    fn evict_files_removes_exactly_the_named_files() {
        let cache = BufferCache::new(64 * 1024 * 1024);
        let files: Vec<FileId> = (0..3).map(|_| FileId::new()).collect();
        for file in &files {
            for offset in [0, 4096, 8192] {
                cache.insert(*file, offset, Arc::new(Block(1000)));
            }
        }
        let freed = cache.evict_files(&HashSet::from([files[0], files[2]]));
        assert_eq!(freed, 6000);
        for (index, file) in files.iter().enumerate() {
            for offset in [0, 4096, 8192] {
                let cached = cache.inner.get(CacheKey::new(*file, offset)).is_some();
                assert_eq!(cached, index == 1, "file {index} offset {offset}");
            }
        }
        assert_eq!(cache.evict_files(&HashSet::new()), 0);
    }
}
