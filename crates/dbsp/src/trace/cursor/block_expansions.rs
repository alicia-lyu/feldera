//! Counts of the data blocks that a reader expands from their disk encoding.
//!
//! A file run can store a data block in a disk-only encoding (see
//! `storage::file::front_code`): *prefix truncation* of byte-string keys, or
//! *elision* of the value headers of byte-string values. The reader rebuilds
//! the exact bytes of an ordinary block when it loads the block into the
//! buffer cache. This is a *block expansion*: a storage-level step on a cache
//! miss, not an item decode. A cursor decodes an item when it reads a key,
//! value, or weight, and [`RunReads`](super::RunReads) counts those; the
//! expansions are counted here, apart from them.
//!
//! Per thread, [`BlockExpansions`] keeps, for each encoding, the blocks
//! expanded, the bytes of those blocks as loaded (`*_encoded_bytes`: the
//! block after decompression, with the zero padding of an uncompressed block),
//! and the bytes after expansion (`*_expanded_bytes`). A cache hit expands
//! nothing and counts nothing; a block written in no encoding counts nothing.
//!
//! # Attribution
//!
//! An expansion is charged to the thread's current read owner, the one
//! [`RunReads`](super::RunReads) uses ([`with_read_owner`](super::with_read_owner)),
//! and the owners add up to [`BlockExpansions::current`].
//!
//! The expansion runs in `decompress`, on the thread that consumes the block.
//! The asynchronous multi-key fetches (`fetch_zset`, `fetch_indexed_zset`) and
//! `bulk_rows` hand the raw bytes to a callback that only sends them to a
//! channel; the consuming thread, which polls the channel, decompresses. So
//! the expansion lands on that thread's owner. The limit is the same as the
//! run reads': a future moved to another thread between polls, such as a tokio
//! task, has its own owner and totals.

use std::cell::Cell;
use std::ops::AddAssign;

use serde::Serialize;

use super::current_read_owner;
use crate::storage::attribution::Component;

/// The encodings whose blocks are expanded on a cache miss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpandedEncoding {
    /// Keys stored as a shared-prefix length and a suffix (front coding).
    PrefixTruncated,
    /// Values stored without their 16-byte `ArchivedVec` roots.
    Elided,
}

/// The block expansions on one thread; see the [module docs](self).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BlockExpansions {
    pub prefix_truncated_blocks: u64,
    pub prefix_truncated_encoded_bytes: u64,
    pub prefix_truncated_expanded_bytes: u64,
    pub elided_blocks: u64,
    pub elided_encoded_bytes: u64,
    pub elided_expanded_bytes: u64,
}

/// One running total per owner, indexed like [`Component::ALL`].
type PerOwner = [Cell<BlockExpansions>; Component::ALL.len()];

thread_local! {
    static EXPANSIONS: PerOwner =
        const { [const { Cell::new(BlockExpansions::ZERO) }; Component::ALL.len()] };
}

impl BlockExpansions {
    const ZERO: Self = Self {
        prefix_truncated_blocks: 0,
        prefix_truncated_encoded_bytes: 0,
        prefix_truncated_expanded_bytes: 0,
        elided_blocks: 0,
        elided_encoded_bytes: 0,
        elided_expanded_bytes: 0,
    };

    /// This thread's running totals, over all owners.
    pub fn current() -> Self {
        Component::ALL.into_iter().map(Self::current_for).fold(
            Self::default(),
            |mut sum, owner_total| {
                sum += owner_total;
                sum
            },
        )
    }

    /// This thread's running totals for the expansions made on behalf of
    /// `owner`.
    pub fn current_for(owner: Component) -> Self {
        EXPANSIONS.with(|totals| totals[owner.index()].get())
    }

    /// The expansions between `earlier` and `self`, both taken on this thread
    /// with [`BlockExpansions::current`] (or both with
    /// [`BlockExpansions::current_for`] and the same owner), `earlier` first.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            prefix_truncated_blocks: self.prefix_truncated_blocks - earlier.prefix_truncated_blocks,
            prefix_truncated_encoded_bytes: self.prefix_truncated_encoded_bytes
                - earlier.prefix_truncated_encoded_bytes,
            prefix_truncated_expanded_bytes: self.prefix_truncated_expanded_bytes
                - earlier.prefix_truncated_expanded_bytes,
            elided_blocks: self.elided_blocks - earlier.elided_blocks,
            elided_encoded_bytes: self.elided_encoded_bytes - earlier.elided_encoded_bytes,
            elided_expanded_bytes: self.elided_expanded_bytes - earlier.elided_expanded_bytes,
        }
    }
}

impl AddAssign for BlockExpansions {
    fn add_assign(&mut self, other: Self) {
        self.prefix_truncated_blocks += other.prefix_truncated_blocks;
        self.prefix_truncated_encoded_bytes += other.prefix_truncated_encoded_bytes;
        self.prefix_truncated_expanded_bytes += other.prefix_truncated_expanded_bytes;
        self.elided_blocks += other.elided_blocks;
        self.elided_encoded_bytes += other.elided_encoded_bytes;
        self.elided_expanded_bytes += other.elided_expanded_bytes;
    }
}

/// Counts one expansion of `encoding` for this thread's current read owner.
pub(crate) fn count_block_expansion(
    encoding: ExpandedEncoding,
    encoded_bytes: usize,
    expanded_bytes: usize,
) {
    let (encoded_bytes, expanded_bytes) = (encoded_bytes as u64, expanded_bytes as u64);
    let one = match encoding {
        ExpandedEncoding::PrefixTruncated => BlockExpansions {
            prefix_truncated_blocks: 1,
            prefix_truncated_encoded_bytes: encoded_bytes,
            prefix_truncated_expanded_bytes: expanded_bytes,
            ..BlockExpansions::ZERO
        },
        ExpandedEncoding::Elided => BlockExpansions {
            elided_blocks: 1,
            elided_encoded_bytes: encoded_bytes,
            elided_expanded_bytes: expanded_bytes,
            ..BlockExpansions::ZERO
        },
    };
    let owner = current_read_owner();
    EXPANSIONS.with(|totals| {
        let total = &totals[owner.index()];
        let mut sum = total.get();
        sum += one;
        total.set(sum);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::with_read_owner;

    /// The expansions that `f` makes, in total and for each owner.
    fn expansions_by_owner(f: impl FnOnce()) -> (BlockExpansions, [BlockExpansions; 5]) {
        let earlier_total = BlockExpansions::current();
        let earlier = Component::ALL.map(BlockExpansions::current_for);
        f();
        let later = Component::ALL.map(BlockExpansions::current_for);
        let by_owner = std::array::from_fn(|i| later[i].since(earlier[i]));
        (BlockExpansions::current().since(earlier_total), by_owner)
    }

    const ONE_PREFIX_TRUNCATED: BlockExpansions = BlockExpansions {
        prefix_truncated_blocks: 1,
        prefix_truncated_encoded_bytes: 512,
        prefix_truncated_expanded_bytes: 2048,
        ..BlockExpansions::ZERO
    };

    const ONE_ELIDED: BlockExpansions = BlockExpansions {
        elided_blocks: 1,
        elided_encoded_bytes: 1024,
        elided_expanded_bytes: 4096,
        ..BlockExpansions::ZERO
    };

    #[test]
    fn each_encoding_counts_its_own_fields() {
        let (total, _) = expansions_by_owner(|| {
            count_block_expansion(ExpandedEncoding::PrefixTruncated, 512, 2048);
            count_block_expansion(ExpandedEncoding::Elided, 1024, 4096);
        });
        let mut both = ONE_PREFIX_TRUNCATED;
        both += ONE_ELIDED;
        assert_eq!(total, both);
    }

    #[test]
    fn expansions_count_for_the_query_by_default() {
        let (total, by_owner) = expansions_by_owner(|| {
            count_block_expansion(ExpandedEncoding::PrefixTruncated, 512, 2048)
        });
        assert_eq!(total, ONE_PREFIX_TRUNCATED);
        assert_eq!(by_owner[Component::Query.index()], ONE_PREFIX_TRUNCATED);
        for owner in Component::ALL {
            if owner != Component::Query {
                assert_eq!(by_owner[owner.index()], BlockExpansions::default());
            }
        }
    }

    #[test]
    fn owners_are_kept_apart() {
        let (total, by_owner) = expansions_by_owner(|| {
            with_read_owner(Component::MergedIndex, || {
                count_block_expansion(ExpandedEncoding::Elided, 1024, 4096)
            });
            with_read_owner(Component::RequestedSlice, || {
                count_block_expansion(ExpandedEncoding::PrefixTruncated, 512, 2048);
                count_block_expansion(ExpandedEncoding::PrefixTruncated, 512, 2048);
            });
            count_block_expansion(ExpandedEncoding::PrefixTruncated, 512, 2048);
        });
        assert_eq!(total.prefix_truncated_blocks, 3);
        assert_eq!(total.elided_blocks, 1);
        assert_eq!(by_owner[Component::MergedIndex.index()], ONE_ELIDED);
        assert_eq!(by_owner[Component::Query.index()], ONE_PREFIX_TRUNCATED);
        let slice = by_owner[Component::RequestedSlice.index()];
        assert_eq!(slice.prefix_truncated_blocks, 2);
        assert_eq!(slice.prefix_truncated_expanded_bytes, 4096);
        assert_eq!(slice.elided_blocks, 0);
        assert_eq!(
            by_owner[Component::PartnerTrace.index()],
            BlockExpansions::default()
        );
    }

    #[test]
    fn counts_stay_on_their_thread() {
        let earlier = BlockExpansions::current();
        std::thread::spawn(|| count_block_expansion(ExpandedEncoding::Elided, 1, 2))
            .join()
            .unwrap();
        assert_eq!(
            BlockExpansions::current().since(earlier),
            BlockExpansions::default()
        );
    }
}
