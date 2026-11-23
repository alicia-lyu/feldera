//! The pool that limits the merges open at once, under `storage.compaction_memory` `pool`.
//!
//! A merge is open from the moment it starts, holding its input cursors and its output
//! builder, until its output is closed; it spans many steps at the higher spine levels, and
//! every spine level runs its own merger task, so nothing else bounds how many are open.
//! The runtime owns one [`CompactionJobs`]. Under `budgets` with `compaction_memory` `pool`
//! it holds a semaphore of `compaction_jobs` permits, and a merge takes a permit before it
//! starts and keeps it until its output is closed, as RocksDB's `max_background_jobs` limits
//! its compactions. Seals of memtables take no permit, as RocksDB's flushes have their own
//! pool. Tokio's semaphore serves waiters first-come, first-served, so no merge starves.
//!
//! Every model counts the open merges and their peak, so a run without the pool reports the
//! concurrency it had. See `docs/design/merged-index/memory-model.md`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The limit of `storage.compaction_jobs` when it is unset, as RocksDB's
/// `max_background_jobs`.
pub const DEFAULT_COMPACTION_JOBS: usize = 2;

/// What the compaction jobs of a runtime have cost so far.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactionJobStats {
    /// Merges that found the pool full and waited for a permit.
    pub waits: u64,

    /// Total time those merges waited.
    pub wait: Duration,

    /// The most merges open at once, since the last
    /// [`CompactionJobs::reset_peak_open_merges`].
    pub peak_open_merges: usize,
}

/// The runtime's compaction job pool and its counters.
#[derive(Debug)]
pub struct CompactionJobs {
    /// The permits and their number, or `None` when the runtime does not limit the merges.
    pool: Option<(Arc<Semaphore>, usize)>,

    waits: AtomicU64,
    wait_nanos: AtomicU64,

    /// Merges open now.
    open_merges: AtomicUsize,
    peak_open_merges: AtomicUsize,
}

impl CompactionJobs {
    /// A pool of `limit` permits, or no pool for `None`.
    ///
    /// # Panics
    ///
    /// If `limit` is zero: no merge could ever start. The runtime validates the option first.
    pub fn new(limit: Option<usize>) -> Self {
        assert_ne!(limit, Some(0), "a pool of zero permits starts no merge");
        Self {
            pool: limit.map(|limit| (Arc::new(Semaphore::new(limit)), limit)),
            waits: AtomicU64::new(0),
            wait_nanos: AtomicU64::new(0),
            open_merges: AtomicUsize::new(0),
            peak_open_merges: AtomicUsize::new(0),
        }
    }

    /// The pool's limit, or `None` when the merges are not limited.
    pub fn limit(&self) -> Option<usize> {
        self.pool.as_ref().map(|(_, limit)| *limit)
    }

    /// The permits not held now, or `None` when the merges are not limited.
    pub fn available_permits(&self) -> Option<usize> {
        self.pool.as_ref().map(|(pool, _)| pool.available_permits())
    }

    /// Takes a permit from the pool, waiting while it is full. Returns `None` at once when the
    /// merges are not limited. The permit returns to the pool when it is dropped.
    ///
    /// Called on a merger task, so the wait is an `await` and holds no thread.
    pub async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        let (pool, _) = self.pool.as_ref()?;
        if let Ok(permit) = pool.clone().try_acquire_owned() {
            return Some(permit);
        }
        self.waits.fetch_add(1, Ordering::Relaxed);
        let waiting_since = Instant::now();
        // The pool is never closed, so the acquire cannot fail.
        let permit = pool
            .clone()
            .acquire_owned()
            .await
            .expect("the compaction pool is never closed");
        self.wait_nanos.fetch_add(
            u64::try_from(waiting_since.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        Some(permit)
    }

    /// Counts a merge as open until the returned guard drops, under every memory model.
    pub fn open_merge(self: &Arc<Self>) -> OpenMerge {
        let open = self.open_merges.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_open_merges.fetch_max(open, Ordering::Relaxed);
        OpenMerge(self.clone())
    }

    pub fn stats(&self) -> CompactionJobStats {
        CompactionJobStats {
            waits: self.waits.load(Ordering::Relaxed),
            wait: Duration::from_nanos(self.wait_nanos.load(Ordering::Relaxed)),
            peak_open_merges: self.peak_open_merges.load(Ordering::Relaxed),
        }
    }

    /// Starts a new peak at the merges open now, so that [`Self::stats`] reports the peak since
    /// this call.
    pub fn reset_peak_open_merges(&self) {
        self.peak_open_merges
            .store(self.open_merges.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// A merge counted as open; see [`CompactionJobs::open_merge`].
#[derive(Debug)]
pub struct OpenMerge(Arc<CompactionJobs>);

impl Drop for OpenMerge {
    fn drop(&mut self) {
        self.0.open_merges.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::CompactionJobs;

    #[test]
    #[should_panic(expected = "zero permits")]
    fn a_pool_of_zero_permits_is_refused() {
        CompactionJobs::new(Some(0));
    }

    #[tokio::test]
    async fn without_a_pool_acquire_returns_no_permit_and_counts_nothing() {
        let jobs = CompactionJobs::new(None);
        assert_eq!(jobs.limit(), None);
        assert!(jobs.acquire().await.is_none());
        assert_eq!(jobs.stats().waits, 0);
    }

    #[tokio::test]
    async fn a_full_pool_makes_the_next_merge_wait_for_a_release() {
        let jobs = Arc::new(CompactionJobs::new(Some(1)));
        assert_eq!(jobs.limit(), Some(1));
        let first = jobs.acquire().await.unwrap();
        assert_eq!(jobs.stats().waits, 0);

        let waiter = tokio::spawn({
            let jobs = jobs.clone();
            async move { jobs.acquire().await.unwrap() }
        });
        while jobs.stats().waits == 0 {
            tokio::task::yield_now().await;
        }
        assert!(!waiter.is_finished());

        std::thread::sleep(Duration::from_millis(5));
        drop(first);
        let second = waiter.await.unwrap();
        assert!(jobs.stats().wait >= Duration::from_millis(5));
        drop(second);
    }

    #[test]
    fn the_peak_counts_open_merges_and_resets_to_those_still_open() {
        let jobs = Arc::new(CompactionJobs::new(None));
        let first = jobs.open_merge();
        let second = jobs.open_merge();
        assert_eq!(jobs.stats().peak_open_merges, 2);

        drop(second);
        assert_eq!(jobs.stats().peak_open_merges, 2);
        jobs.reset_peak_open_merges();
        assert_eq!(jobs.stats().peak_open_merges, 1);

        drop(first);
        jobs.reset_peak_open_merges();
        assert_eq!(jobs.stats().peak_open_merges, 0);
    }
}
