//! The memory budget shared by the memtables of all of a runtime's integral spines.
//!
//! A memtable is a spine's in-memory write buffer (see `docs/design/merged-index/memtable.md`).
//! The runtime owns one [`MemtableBudget`] when `storage.memtable_mib` is set. Each spine
//! registers a [`MemtableHandle`] with it and reports the bytes its memtable gains and loses
//! through [`MemtableBudget::add_bytes`]. Whenever the total stands above the budget, the budget
//! asks the largest memtable that is not already flushing to seal itself, whichever spine
//! reported the change. The seal itself belongs to the handle, which runs it on the merger
//! runtime.
//!
//! The budget is a soft cap. Memtable bytes count in RSS like any other memory, so memory pressure
//! is unchanged (decision of 2026-11-19 in `docs/design/merged-index/decisions.md`).

use std::sync::{
    Mutex, PoisonError, Weak,
    atomic::{AtomicUsize, Ordering},
};

/// One spine's memtable, as the budget sees it.
///
/// The budget calls these methods from any thread, possibly while a spine is inserting, so an
/// implementation must answer from atomics and must not block.
pub trait MemtableHandle: Send + Sync {
    /// All live leaf bytes of this memtable: the active tree, any tree being flushed, and the
    /// copies of leaves that a frozen memtable (a snapshot) still holds. Leaf bytes are counted
    /// when allocated and released when dropped.
    fn live_bytes(&self) -> usize;

    /// Whether a seal is pending or in progress. A handle must report `true` from the moment
    /// [`request_seal`](Self::request_seal) is called until the seal has finished, so the budget
    /// moves on to the next-largest memtable instead of asking this one again.
    fn is_flushing(&self) -> bool;

    /// Asks the handle to seal its memtable, that is, copy it into a run, in the background.
    ///
    /// This must return quickly (schedule the work, do not do it) and must be idempotent. It is
    /// called without any budget lock held, so it may call [`MemtableBudget::add_bytes`].
    fn request_seal(&self);
}

/// The budget of the runtime's memtables: the registered handles and the live total.
///
/// The total can overshoot the budget by at most one batch: a seal is requested only after an
/// insert has already added its bytes, and a batch above a quarter of the budget bypasses the
/// memtable.
pub struct MemtableBudget {
    budget_bytes: usize,

    /// The sum of the bytes the spines have reported through [`Self::add_bytes`].
    live_bytes: AtomicUsize,

    /// Every spine's memtable. Dead entries are pruned when a handle registers and when the
    /// budget looks for a memtable to seal.
    handles: Mutex<Vec<Weak<dyn MemtableHandle>>>,
}

impl MemtableBudget {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            live_bytes: AtomicUsize::new(0),
            handles: Mutex::new(Vec::new()),
        }
    }

    /// The configured budget (`storage.memtable_mib`) in bytes.
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// The bytes all memtables hold, as reported to [`Self::add_bytes`].
    pub fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::Relaxed)
    }

    /// Whether the memtables together hold more than the budget.
    pub fn is_over_budget(&self) -> bool {
        self.live_bytes() > self.budget_bytes
    }

    /// Registers a spine's memtable, and drops the entries whose spines are gone.
    pub fn register(&self, handle: Weak<dyn MemtableHandle>) {
        let mut handles = self.lock_handles();
        handles.retain(|registered| registered.strong_count() > 0);
        handles.push(handle);
    }

    /// Adds `delta` bytes to the live total (a negative `delta` when memtable bytes are freed),
    /// then, if the total stands above the budget, asks the largest memtable that is not
    /// flushing to seal.
    ///
    /// The check runs after a decrease too, so that a seal that frees too little hands over to
    /// the next-largest memtable.
    pub fn add_bytes(&self, delta: isize) {
        self.live_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                // A total that would go negative is a bookkeeping bug; keep it at zero rather
                // than wrap, so the budget never sees an enormous memtable total.
                debug_assert!(
                    live.checked_add_signed(delta).is_some(),
                    "memtable bytes went negative: {live} + {delta}"
                );
                Some(live.saturating_add_signed(delta))
            })
            .expect("the update closure always returns Some");

        if self.is_over_budget() {
            self.request_seal_of_largest();
        }
    }

    /// Asks the largest memtable that holds something and is not flushing to seal. The choice
    /// looks at every registered handle, so a spine that has stopped inserting is still chosen.
    fn request_seal_of_largest(&self) {
        let candidate = {
            let mut handles = self.lock_handles();
            let mut live_handles = Vec::with_capacity(handles.len());
            handles.retain(|registered| match registered.upgrade() {
                Some(handle) => {
                    live_handles.push(handle);
                    true
                }
                None => false,
            });
            live_handles
                .into_iter()
                .filter(|handle| !handle.is_flushing())
                .map(|handle| (handle.live_bytes(), handle))
                .filter(|(live_bytes, _)| *live_bytes > 0)
                .max_by_key(|(live_bytes, _)| *live_bytes)
        };

        // The handle is called outside the lock, since it may call back into `add_bytes`.
        if let Some((_, handle)) = candidate {
            handle.request_seal();
        }
    }

    fn lock_handles(&self) -> std::sync::MutexGuard<'_, Vec<Weak<dyn MemtableHandle>>> {
        // The list holds no invariant a panicking holder could break.
        self.handles.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{MemtableBudget, MemtableHandle};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    /// A memtable handle whose size and state the test sets.
    #[derive(Default)]
    pub(crate) struct MockMemtable {
        pub(crate) bytes: AtomicUsize,
        pub(crate) flushing: AtomicBool,
        pub(crate) seal_requests: AtomicUsize,
    }

    impl MockMemtable {
        /// A mock registered with `budget`, holding `bytes` that the budget already counts.
        pub(crate) fn registered(budget: &MemtableBudget, bytes: usize) -> Arc<Self> {
            let mock = Arc::new(Self::default());
            budget.register(Arc::downgrade(&mock) as _);
            mock.grow(budget, bytes);
            mock
        }

        /// Adds `bytes` to the mock and reports them to the budget, like an insert would.
        pub(crate) fn grow(&self, budget: &MemtableBudget, bytes: usize) {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
            budget.add_bytes(bytes as isize);
        }

        pub(crate) fn seal_requests(&self) -> usize {
            self.seal_requests.load(Ordering::Relaxed)
        }
    }

    impl MemtableHandle for MockMemtable {
        fn live_bytes(&self) -> usize {
            self.bytes.load(Ordering::Relaxed)
        }

        fn is_flushing(&self) -> bool {
            self.flushing.load(Ordering::Relaxed)
        }

        fn request_seal(&self) {
            self.seal_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn no_seal_while_the_total_stays_within_the_budget() {
        let budget = MemtableBudget::new(100);
        let first = MockMemtable::registered(&budget, 60);
        let second = MockMemtable::registered(&budget, 40);

        // Exactly the budget is within it.
        assert_eq!(budget.live_bytes(), 100);
        assert!(!budget.is_over_budget());
        assert_eq!(first.seal_requests() + second.seal_requests(), 0);
    }

    #[test]
    fn the_largest_memtable_is_sealed_first() {
        let budget = MemtableBudget::new(100);
        let small = MockMemtable::registered(&budget, 30);
        let large = MockMemtable::registered(&budget, 60);
        let medium = MockMemtable::registered(&budget, 10);

        // The small one inserts and brings the total over; the large one is asked, not the
        // spine that inserted.
        small.grow(&budget, 5);
        assert_eq!(large.seal_requests(), 1);
        assert_eq!(small.seal_requests(), 0);
        assert_eq!(medium.seal_requests(), 0);
    }

    #[test]
    fn a_memtable_that_stopped_inserting_is_still_selected() {
        let budget = MemtableBudget::new(100);
        let dormant = MockMemtable::registered(&budget, 70);
        let active = MockMemtable::registered(&budget, 20);

        // Only `active` inserts from here on; its memtable stays smaller than the dormant one.
        active.grow(&budget, 20);
        assert_eq!(dormant.seal_requests(), 1);
        assert_eq!(active.seal_requests(), 0);
    }

    #[test]
    fn flushing_memtables_are_skipped() {
        let budget = MemtableBudget::new(100);
        let flushing = MockMemtable::registered(&budget, 80);
        let next = MockMemtable::registered(&budget, 15);
        flushing.flushing.store(true, Ordering::Relaxed);

        next.grow(&budget, 10);
        assert_eq!(flushing.seal_requests(), 0);
        assert_eq!(next.seal_requests(), 1);
    }

    #[test]
    fn nothing_is_requested_when_every_memtable_is_flushing_or_empty() {
        let budget = MemtableBudget::new(10);
        // Flushing before its bytes are reported, so registering asks nothing of it.
        let flushing = MockMemtable::registered(&budget, 0);
        flushing.flushing.store(true, Ordering::Relaxed);
        flushing.grow(&budget, 20);
        let empty = MockMemtable::registered(&budget, 0);

        budget.add_bytes(1);
        assert_eq!(flushing.seal_requests() + empty.seal_requests(), 0);
    }

    #[test]
    fn a_seal_that_frees_too_little_hands_over_to_the_next_memtable() {
        let budget = MemtableBudget::new(100);
        let first = MockMemtable::registered(&budget, 70);
        let second = MockMemtable::registered(&budget, 50);
        assert_eq!(first.seal_requests(), 1);

        // `first` seals and frees 10 bytes, so the total, 110, is still over the budget.
        first.flushing.store(true, Ordering::Relaxed);
        first.bytes.fetch_sub(10, Ordering::Relaxed);
        budget.add_bytes(-10);
        assert_eq!(second.seal_requests(), 1);

        // It frees the rest; the total is within the budget, so no further request is made.
        first.bytes.fetch_sub(60, Ordering::Relaxed);
        budget.add_bytes(-60);
        assert_eq!(budget.live_bytes(), 50);
        assert_eq!(first.seal_requests(), 1);
        assert_eq!(second.seal_requests(), 1);
    }

    #[test]
    fn dead_handles_are_pruned() {
        let budget = MemtableBudget::new(10);
        let dropped = MockMemtable::registered(&budget, 0);
        drop(dropped);
        let kept = MockMemtable::registered(&budget, 0);
        assert_eq!(budget.lock_handles().len(), 1);

        kept.grow(&budget, 11);
        assert_eq!(kept.seal_requests(), 1);
    }

    #[test]
    fn a_handle_may_call_back_into_the_budget_when_asked_to_seal() {
        struct Reentrant(Arc<MemtableBudget>);
        impl MemtableHandle for Reentrant {
            fn live_bytes(&self) -> usize {
                100
            }
            fn is_flushing(&self) -> bool {
                false
            }
            fn request_seal(&self) {
                // Must not deadlock on the handle list.
                self.0.add_bytes(-1);
            }
        }

        let budget = Arc::new(MemtableBudget::new(0));
        let handle = Arc::new(Reentrant(budget.clone()));
        budget.register(Arc::downgrade(&handle) as _);
        // The nested call is over budget too (the total is 2, then 1), so this recurses once
        // per byte; it ends when the total reaches zero.
        budget.add_bytes(2);
        assert_eq!(budget.live_bytes(), 0);
    }
}
