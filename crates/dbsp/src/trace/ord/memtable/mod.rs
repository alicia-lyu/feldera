//! A memtable: an in-memory B+-tree of sorted `(key, value, weight)` items
//! with copy-on-write nodes, for the spine to absorb the deltas of many
//! transactions before it writes them to a file.
//!
//! See `docs/design/merged-index/memtable.md`. The tree is in [`tree`], and the
//! leaves that hold the items are in [`leaf`].

pub mod cursor;
pub mod leaf;
pub mod tree;

pub use cursor::MemtableCursor;
pub use leaf::{Bound, IndexedLeaf, MemLeaf, Slot, WSetLeaf};
pub use tree::{FrozenMemtable, Memtable, Totals, TreeShape};

use std::sync::{Arc, atomic::AtomicUsize};

/// A memtable as a spine of batch type `B` uses it: `B`'s deltas go in, and
/// frozen memtables come out as batches of type `B`.
///
/// [`crate::trace::Batch::new_memtable`] makes one; only the untimed fallback
/// W-set and indexed W-set have one.
pub trait BatchMemtable<B>: Send + Sync {
    /// Inserts every tuple of `batch`, between the tuples already held.
    fn insert_batch(&mut self, batch: &B);

    /// A batch that reads the memtable as it is now, or `None` if it is empty.
    /// It shares the tree's nodes, so later inserts copy what it holds.
    fn freeze(&self) -> Option<B>;

    /// Takes the memtable's contents as a batch, leaving the memtable empty,
    /// with a fresh leaf-memory counter. Returns the batch and the counter of
    /// its leaves, which falls to zero once the batch and every snapshot of it
    /// are dropped. Returns `None` if the memtable is empty.
    fn take(&mut self) -> Option<(B, Arc<AtomicUsize>)>;

    /// The bytes of the memtable's live leaves, including copies that a
    /// snapshot still holds.
    fn live_leaf_bytes(&self) -> usize;

    /// The number of `(key, value)` tuples held.
    fn tuples(&self) -> usize;

    /// Nodes copied on write since the memtable was made, across takes.
    fn node_copies(&self) -> u64;

    /// The time spent on those copies, in nanoseconds.
    fn copy_nanos(&self) -> u64;
}

#[cfg(test)]
mod cursor_test;
#[cfg(test)]
mod test;

/// A batch type that can read a frozen memtable of leaves `L` in place.
pub trait FromFrozenMemtable<L: MemLeaf>: crate::trace::BatchReader {
    fn from_frozen_memtable(factories: &Self::Factories, frozen: FrozenMemtable<L>) -> Self;
}

/// The [`BatchMemtable`] of a batch type `B` whose frozen memtables have
/// leaves `L`.
pub struct LeafBatchMemtable<L: MemLeaf, B: crate::trace::BatchReader> {
    batch_factories: B::Factories,
    leaf_factories: L::Factories,
    table: Memtable<L>,
    /// The copy counts and times of the tables already taken.
    earlier_node_copies: u64,
    earlier_copy_nanos: u64,
}

impl<L, B> LeafBatchMemtable<L, B>
where
    L: MemLeaf,
    B: crate::trace::BatchReader,
{
    pub fn new(batch_factories: &B::Factories, leaf_factories: &L::Factories) -> Self {
        Self {
            batch_factories: batch_factories.clone(),
            leaf_factories: leaf_factories.clone(),
            table: Memtable::new(leaf_factories),
            earlier_node_copies: 0,
            earlier_copy_nanos: 0,
        }
    }
}

impl<L, B> BatchMemtable<B> for LeafBatchMemtable<L, B>
where
    L: MemLeaf,
    B: FromFrozenMemtable<L, Key = L::Key, Val = L::Val, Time = (), R = L::Weight>,
    B::Factories: Send + Sync,
{
    fn insert_batch(&mut self, batch: &B) {
        self.table.insert_batch(&mut batch.cursor());
    }

    fn freeze(&self) -> Option<B> {
        (self.table.totals().tuples > 0)
            .then(|| B::from_frozen_memtable(&self.batch_factories, self.table.freeze()))
    }

    fn take(&mut self) -> Option<(B, Arc<AtomicUsize>)> {
        if self.table.totals().tuples == 0 {
            return None;
        }
        let taken = std::mem::replace(&mut self.table, Memtable::new(&self.leaf_factories));
        self.earlier_node_copies += taken.node_copies();
        self.earlier_copy_nanos += taken.copy_nanos();
        let leaf_memory = taken.leaf_memory().clone();
        let batch = B::from_frozen_memtable(&self.batch_factories, taken.freeze());
        Some((batch, leaf_memory))
    }

    fn live_leaf_bytes(&self) -> usize {
        self.table.live_leaf_bytes()
    }

    fn tuples(&self) -> usize {
        self.table.totals().tuples
    }

    fn node_copies(&self) -> u64 {
        self.earlier_node_copies + self.table.node_copies()
    }

    fn copy_nanos(&self) -> u64 {
        self.earlier_copy_nanos + self.table.copy_nanos()
    }
}
