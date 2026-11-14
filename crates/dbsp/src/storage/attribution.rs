//! Attribution of storage reads and writes to the [`Component`] that causes them.
//!
//! The storage backend counts every write in two process-wide metrics: the
//! number of flushes (`WRITE_LATENCY_MICROSECONDS`) and the bytes written
//! (`WRITE_BLOCKS_BYTES`). This module keeps the same two counts per
//! [`Component`], updated at the same two places, so the owners always add up
//! to the totals.
//!
//! # How an owner reaches a file
//!
//! * A thread has a *current owner*, [`Component::Query`] by default.
//!   [`with_write_owner`] changes it for the duration of a closure.
//! * A `Spine` captures the current owner when it is created and keeps it for
//!   life. Its inserts run under that owner, and its background compaction
//!   tasks re-establish it with [`scope_write_owner`], because a tokio task
//!   does not inherit the thread-local of the thread that spawned it.
//! * A file writer captures the current owner when it is created. A later
//!   flush, on any thread and at any time (even after a measurement window has
//!   closed), is counted for that captured owner.
//!
//! Nothing here looks at trace names or file names.
//!
//! # Known limit
//!
//! A batch that an operator builds in storage before it reaches a `Spine`
//! (the spill that a builder performs under memory pressure or when
//! `min_step_storage_bytes` is set) is written under the owner that is current
//! on the thread that runs the operator, which is [`Component::Query`]
//! unless an enclosing scope says otherwise.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use feldera_storage::metrics::{WRITE_BLOCKS_BYTES, WRITE_LATENCY_MICROSECONDS};
use pin_project_lite::pin_project;
use serde::Serialize;

/// The component that a storage read or write is attributed to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    /// Join traces, materialized views, and query execution. The default.
    #[default]
    Query,
    /// Base-relation state (input maps) and the delta accumulators of base
    /// tables.
    BaseRelation,
    /// The merged index that the harness maintains outside the circuit.
    MergedIndex,
    /// The joins' reads of the readers' requested slices of the index: the
    /// batches that the reconstructed view circuit's joins seek for their
    /// partner rows. A read owner only: nothing is written under it, so its
    /// write counts stay zero.
    RequestedSlice,
    /// The joins' reads of their partner traces in native mode, the counterpart
    /// of [`Component::RequestedSlice`]. A read owner only.
    PartnerTrace,
}

impl Component {
    /// Every owner, in the order of [`WriteSnapshot::by_owner`].
    pub const ALL: [Component; 5] = [
        Component::Query,
        Component::BaseRelation,
        Component::MergedIndex,
        Component::RequestedSlice,
        Component::PartnerTrace,
    ];

    /// A stable lower-case name, used as a key in reports.
    pub fn name(self) -> &'static str {
        match self {
            Component::Query => "query",
            Component::BaseRelation => "base_relation",
            Component::MergedIndex => "merged_index",
            Component::RequestedSlice => "requested_slice",
            Component::PartnerTrace => "partner_trace",
        }
    }

    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

thread_local! {
    static CURRENT_OWNER: Cell<Component> = const { Cell::new(Component::Query) };
}

/// The owner that files and spines created on this thread now belong to.
pub fn current_write_owner() -> Component {
    CURRENT_OWNER.get()
}

/// Runs `f` with `owner` as the current owner of this thread, then restores the
/// previous owner, even if `f` panics.
pub fn with_write_owner<R>(owner: Component, f: impl FnOnce() -> R) -> R {
    struct Restore(Component);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_OWNER.set(self.0);
        }
    }
    let _restore = Restore(CURRENT_OWNER.replace(owner));
    f()
}

pin_project! {
    /// A future that runs each of its polls under a fixed [`Component`].
    pub struct WithWriteOwner<F> {
        owner: Component,
        #[pin]
        future: F,
    }
}

impl<F: Future> Future for WithWriteOwner<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        with_write_owner(*this.owner, || this.future.poll(cx))
    }
}

/// Wraps `future` so that every poll runs under `owner`.
///
/// A poll may run on any thread of a tokio runtime, so the owner is set per
/// poll rather than per thread.
pub fn scope_write_owner<F: Future>(owner: Component, future: F) -> WithWriteOwner<F> {
    WithWriteOwner { owner, future }
}

/// Operations and bytes written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WriteCounts {
    /// Backend flushes (the count of `WRITE_LATENCY_MICROSECONDS`).
    pub operations: u64,
    /// Bytes written (the sum of `WRITE_BLOCKS_BYTES`).
    pub bytes: u64,
}

/// A consistent reading of the process-wide write counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WriteSnapshot {
    /// The backend's own totals. These are the numbers that existed before
    /// attribution did.
    pub total: WriteCounts,
    /// The same writes split by owner, indexed like [`Component::ALL`].
    pub by_owner: [WriteCounts; Component::ALL.len()],
}

impl WriteSnapshot {
    /// The counts for `owner`.
    pub fn owner(&self, owner: Component) -> WriteCounts {
        self.by_owner[owner.index()]
    }

    /// The sum over all owners. Equal to [`Self::total`] in any snapshot that
    /// [`write_snapshot`] returns.
    pub fn owners_sum(&self) -> WriteCounts {
        WriteCounts {
            operations: self.by_owner.iter().map(|c| c.operations).sum(),
            bytes: self.by_owner.iter().map(|c| c.bytes).sum(),
        }
    }
}

static OPERATIONS_BY_OWNER: [AtomicU64; Component::ALL.len()] =
    [const { AtomicU64::new(0) }; Component::ALL.len()];
static BYTES_BY_OWNER: [AtomicU64; Component::ALL.len()] =
    [const { AtomicU64::new(0) }; Component::ALL.len()];

/// Number of counter updates that have started but not finished.
static UPDATES_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
/// Number of counter updates that have finished.
static UPDATES_COMPLETED: AtomicU64 = AtomicU64::new(0);

/// Runs `update`, which changes a backend total and the matching owner count,
/// so that [`write_snapshot`] never observes it half done.
fn grouped_update(update: impl FnOnce()) {
    UPDATES_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    update();
    UPDATES_COMPLETED.fetch_add(1, Ordering::SeqCst);
    UPDATES_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
}

/// Counts `bytes` written by one vectored write on behalf of `owner`.
pub(crate) fn record_write_bytes(owner: Component, bytes: usize) {
    grouped_update(|| {
        WRITE_BLOCKS_BYTES.record(bytes);
        BYTES_BY_OWNER[owner.index()].fetch_add(bytes as u64, Ordering::Relaxed);
    });
}

/// Counts one backend flush on behalf of `owner`. `started` is when the flush
/// began; its duration goes to the backend's latency histogram, as before.
pub(crate) fn record_write_operation(owner: Component, started: Instant) {
    grouped_update(|| {
        WRITE_LATENCY_MICROSECONDS.record_elapsed(started);
        OPERATIONS_BY_OWNER[owner.index()].fetch_add(1, Ordering::Relaxed);
    });
}

fn histogram_count(histogram: &feldera_storage::histogram::ExponentialHistogram) -> u64 {
    histogram
        .snapshot()
        .iter_buckets()
        .map(|bucket| bucket.count)
        .sum()
}

/// Reads the totals and the per-owner counts at one instant.
///
/// The reading retries while a counter update is in progress, so the owners
/// sum to the totals even when a background compaction writes concurrently.
pub fn write_snapshot() -> WriteSnapshot {
    loop {
        let completed = UPDATES_COMPLETED.load(Ordering::SeqCst);
        if UPDATES_IN_FLIGHT.load(Ordering::SeqCst) != 0 {
            std::thread::yield_now();
            continue;
        }
        let snapshot = WriteSnapshot {
            total: WriteCounts {
                operations: histogram_count(&WRITE_LATENCY_MICROSECONDS),
                bytes: WRITE_BLOCKS_BYTES.sum(),
            },
            by_owner: Component::ALL.map(|owner| WriteCounts {
                operations: OPERATIONS_BY_OWNER[owner.index()].load(Ordering::Relaxed),
                bytes: BYTES_BY_OWNER[owner.index()].load(Ordering::Relaxed),
            }),
        };
        if UPDATES_IN_FLIGHT.load(Ordering::SeqCst) == 0
            && UPDATES_COMPLETED.load(Ordering::SeqCst) == completed
        {
            return snapshot;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_owner_is_query() {
        assert_eq!(current_write_owner(), Component::Query);
    }

    #[test]
    fn scopes_nest_and_restore() {
        with_write_owner(Component::BaseRelation, || {
            assert_eq!(current_write_owner(), Component::BaseRelation);
            with_write_owner(Component::MergedIndex, || {
                assert_eq!(current_write_owner(), Component::MergedIndex);
            });
            assert_eq!(current_write_owner(), Component::BaseRelation);
        });
        assert_eq!(current_write_owner(), Component::Query);
    }

    #[test]
    fn a_panic_restores_the_owner() {
        let result = std::panic::catch_unwind(|| {
            with_write_owner(Component::MergedIndex, || panic!("boom"));
        });
        assert!(result.is_err());
        assert_eq!(current_write_owner(), Component::Query);
    }

    #[test]
    fn a_scoped_future_sets_the_owner_in_every_poll_and_leaves_none_behind() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .unwrap();
        let observed = runtime
            .block_on(async {
                tokio::spawn(scope_write_owner(Component::MergedIndex, async {
                    let first = current_write_owner();
                    // Yielding lets the task resume on another thread.
                    tokio::task::yield_now().await;
                    let second = current_write_owner();
                    (first, second)
                }))
                .await
            })
            .unwrap();
        assert_eq!(observed, (Component::MergedIndex, Component::MergedIndex));
        assert_eq!(current_write_owner(), Component::Query);
    }

    #[test]
    fn owner_names_are_distinct() {
        let names: std::collections::BTreeSet<_> =
            Component::ALL.iter().map(|owner| owner.name()).collect();
        assert_eq!(names.len(), Component::ALL.len());
    }
}
