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

#[cfg(test)]
mod cursor_test;
#[cfg(test)]
mod test;
