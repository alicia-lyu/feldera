//! Prefix Bloom filters: a filter over the distinct key prefixes of a file's
//! column 0.
//!
//! A whole-key filter answers an exact probe (`seek_key_exact`). A group read
//! asks a different question: does this run hold *any* key with this prefix?
//! The merged index's group reads ask it for every touched customer, and a
//! native scan of a composite key (`(l_orderkey, l_linenumber)` by its first
//! field) asks it too. A prefix filter answers it, so a run that holds no key
//! with the prefix need not be sought (RocksDB's prefix Bloom filter).
//!
//! # The spec
//!
//! A [`PrefixSpec`] names the prefix (its [`PrefixKind`]) and carries two
//! functions monomorphized where the key type is known: the hash of a key's
//! prefix, and the comparison of two keys' prefixes. The writer and the probe
//! use the same spec, from the batch factories ([`FilterSpec`]); the file
//! records the kind, and a reader refuses a file whose kind differs.
//!
//! # Storage
//!
//! Keys arrive sorted, so a prefix's keys are contiguous and the writer keeps
//! one hash per distinct prefix ([`PrefixCollector`]). A file of at most
//! [`INLINE_PREFIXES`] prefixes stores their hashes inline in the filter's
//! header block and keeps them in memory: most runs of a small-transaction
//! workload are that small, and a partition would cost a 4 KiB write each.
//! A larger file stores a partitioned Bloom filter sized by the exact prefix
//! count, probed one partition at a time through the buffer cache, as the
//! storage-resident whole-key filters are
//! ([`StoredPartitionedBloom`]).

use std::{
    cmp::Ordering,
    fmt::{Debug, Formatter, Result as FmtResult},
    hash::{Hash, Hasher},
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
};

use xxhash_rust::xxh3::{Xxh3Default, xxh3_64};

use super::{FilterKind, FilterStats, StoredPartitionedBloom};
use crate::{
    DBData,
    dynamic::{DataTrait, DowncastTrait, DynData},
    utils::{Tup1, Tup2, Tup3, Tup4},
};

/// The most prefix hashes a file stores inline, in its filter's header block.
///
/// 120 hashes fill one kibibyte with the header's fields; above that a
/// partitioned filter at the default rate is smaller than the hashes.
pub const INLINE_PREFIXES: usize = 120;

/// Which prefix of a key a prefix filter covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixKind {
    /// The first `n` bytes of a byte-string key (`Vec<u8>`).
    Bytes(u32),
    /// The first `n` fields of a tuple key.
    Fields(u32),
}

impl PrefixKind {
    /// The kind's on-file encoding: a tag and a length.
    pub(crate) fn to_raw(self) -> (u8, u32) {
        match self {
            Self::Bytes(n) => (1, n),
            Self::Fields(n) => (2, n),
        }
    }

    /// The kind encoded as `(tag, length)`, or `None` for an unknown tag.
    pub(crate) fn from_raw(tag: u8, length: u32) -> Option<Self> {
        match tag {
            1 => Some(Self::Bytes(length)),
            2 => Some(Self::Fields(length)),
            _ => None,
        }
    }
}

/// A key prefix that a prefix filter covers; see the [module docs](self).
pub struct PrefixSpec<K: ?Sized> {
    kind: PrefixKind,
    hash: fn(&K) -> u64,
    cmp: fn(&K, &K) -> Ordering,
}

impl<K: ?Sized> Clone for PrefixSpec<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: ?Sized> Copy for PrefixSpec<K> {}

impl<K: ?Sized> Debug for PrefixSpec<K> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_tuple("PrefixSpec").field(&self.kind).finish()
    }
}

impl<K: ?Sized> PrefixSpec<K> {
    /// The prefix this spec covers.
    pub fn kind(&self) -> PrefixKind {
        self.kind
    }

    /// The hash of `key`'s prefix. Keys with equal prefixes hash equally.
    pub fn hash(&self, key: &K) -> u64 {
        (self.hash)(key)
    }

    /// Compares the prefixes of `left` and `right`. Consistent with the key
    /// order: keys in ascending order have non-descending prefixes.
    pub fn cmp(&self, left: &K, right: &K) -> Ordering {
        (self.cmp)(left, right)
    }

    /// Whether `key` carries the prefix of `probe`.
    pub fn has_prefix(&self, key: &K, probe: &K) -> bool {
        self.cmp(key, probe) == Ordering::Equal
    }
}

fn byte_prefix<const N: usize>(key: &DynData) -> &[u8] {
    let bytes = key.downcast_checked::<Vec<u8>>();
    assert!(
        bytes.len() >= N,
        "a {}-byte key is shorter than its {N}-byte filter prefix",
        bytes.len()
    );
    &bytes[..N]
}

impl PrefixSpec<DynData> {
    /// The first `N` bytes of a `Vec<u8>` key. Every key must be at least `N`
    /// bytes long; a shorter key panics in the writer and in a probe.
    ///
    /// The bytes are hashed directly: `Vec<u8>`'s `Hash` would also hash the
    /// key's length, which differs between a key and its prefix.
    pub fn bytes<const N: usize>() -> Self {
        Self {
            kind: PrefixKind::Bytes(N as u32),
            hash: |key| xxh3_64(byte_prefix::<N>(key)),
            cmp: |left, right| byte_prefix::<N>(left).cmp(byte_prefix::<N>(right)),
        }
    }

    /// The first `N` bytes of a `Vec<u8>` key, for a length known only at run
    /// time. `n` must be one of the lengths [`bytes`](Self::bytes) is
    /// instantiated for here (1 to 64).
    ///
    /// # Panics
    ///
    /// If `n` is 0 or above 64.
    pub fn bytes_of_length(n: usize) -> Self {
        macro_rules! dispatch {
            ($($len:literal)*) => {
                match n {
                    $($len => Self::bytes::<$len>(),)*
                    _ => panic!("prefix filters cover 1 to 64 bytes, not {n}"),
                }
            };
        }
        dispatch!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28
                  29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53
                  54 55 56 57 58 59 60 61 62 63 64)
    }

    /// The first `N` fields of a tuple key of type `T`.
    pub fn fields<T, const N: usize>() -> Self
    where
        T: TuplePrefix + DBData,
    {
        assert!(
            (1..=T::FIELDS).contains(&N),
            "a prefix of {N} fields of a {}-field tuple",
            T::FIELDS
        );
        Self {
            kind: PrefixKind::Fields(N as u32),
            hash: |key| {
                let mut hasher = Xxh3Default::new();
                key.downcast_checked::<T>().hash_prefix(N, &mut hasher);
                hasher.finish()
            },
            cmp: |left, right| {
                left.downcast_checked::<T>()
                    .cmp_prefix(right.downcast_checked::<T>(), N)
            },
        }
    }
}

/// Tuples whose leading fields can be hashed and compared on their own.
pub trait TuplePrefix {
    /// The number of fields.
    const FIELDS: usize;

    /// Feeds the first `n` fields to `hasher`, in order.
    fn hash_prefix<H: Hasher>(&self, n: usize, hasher: &mut H);

    /// Compares the first `n` fields with `other`'s, lexicographically.
    fn cmp_prefix(&self, other: &Self, n: usize) -> Ordering;
}

macro_rules! tuple_prefix {
    ($tuple:ident, $count:literal, $($field:tt $type:ident),+) => {
        impl<$($type: Hash + Ord),+> TuplePrefix for $tuple<$($type),+> {
            const FIELDS: usize = $count;

            fn hash_prefix<H: Hasher>(&self, n: usize, hasher: &mut H) {
                $(if $field < n { self.$field.hash(hasher); })+
            }

            fn cmp_prefix(&self, other: &Self, n: usize) -> Ordering {
                $(
                    if $field < n {
                        match self.$field.cmp(&other.$field) {
                            Ordering::Equal => {}
                            unequal => return unequal,
                        }
                    }
                )+
                Ordering::Equal
            }
        }
    };
}

tuple_prefix!(Tup1, 1, 0 A);
tuple_prefix!(Tup2, 2, 0 A, 1 B);
tuple_prefix!(Tup3, 3, 0 A, 1 B, 2 C);
tuple_prefix!(Tup4, 4, 0 A, 1 B, 2 C, 3 D);

/// Which membership filters a file writer builds for column 0.
pub struct FilterSpec<K: ?Sized> {
    /// Whether to build the whole-key filter (Bloom or roaring), which exact
    /// probes consult.
    pub whole_key: bool,
    /// The prefix to build a prefix filter over, if any.
    pub prefix: Option<PrefixSpec<K>>,
}

impl<K: ?Sized> Clone for FilterSpec<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: ?Sized> Copy for FilterSpec<K> {}

impl<K: ?Sized> Debug for FilterSpec<K> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("FilterSpec")
            .field("whole_key", &self.whole_key)
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl<K: ?Sized> Default for FilterSpec<K> {
    /// A whole-key filter and no prefix filter: DBSP's behavior before prefix
    /// filters.
    fn default() -> Self {
        Self {
            whole_key: true,
            prefix: None,
        }
    }
}

impl<K: ?Sized> FilterSpec<K> {
    /// A prefix filter only. For a spine that never makes exact probes, such as
    /// a merged index, whose group reads seek by prefix.
    pub fn prefix_only(prefix: PrefixSpec<K>) -> Self {
        Self {
            whole_key: false,
            prefix: Some(prefix),
        }
    }

    /// A whole-key filter and a prefix filter.
    pub fn both(prefix: PrefixSpec<K>) -> Self {
        Self {
            whole_key: true,
            prefix: Some(prefix),
        }
    }
}

/// Collects the hashes of the distinct prefixes of a file's sorted keys.
pub struct PrefixCollector<K: ?Sized> {
    spec: PrefixSpec<K>,
    hashes: Vec<u64>,
}

impl<K: DataTrait + ?Sized> PrefixCollector<K> {
    /// A collector of `spec`'s prefixes.
    pub fn new(spec: PrefixSpec<K>) -> Self {
        Self {
            spec,
            hashes: Vec::new(),
        }
    }

    /// Adds the next key, in sorted order.
    ///
    /// Equal prefixes are contiguous, so a prefix is new exactly when its hash
    /// differs from the last one kept. Two different adjacent prefixes with
    /// equal hashes set the same filter bits, so keeping one loses nothing.
    pub fn push_key(&mut self, key: &K) {
        let hash = self.spec.hash(key);
        if self.hashes.last() != Some(&hash) {
            self.hashes.push(hash);
        }
    }

    /// The spec whose prefixes this collects.
    pub fn spec(&self) -> PrefixSpec<K> {
        self.spec
    }

    /// The hashes collected, one per distinct prefix, in key order.
    pub fn into_hashes(self) -> Vec<u64> {
        self.hashes
    }
}

/// The prefix filter of one file, as a batch probes it.
pub enum PrefixFilter {
    /// The sorted, distinct hashes of a file with few prefixes, in memory.
    Inline {
        /// The prefix hashes, sorted and distinct.
        hashes: Box<[u64]>,
        /// Probes that could not rule the prefix out.
        hits: AtomicUsize,
        /// Probes that ruled the prefix out.
        misses: AtomicUsize,
    },
    /// A partitioned Bloom filter on storage.
    Stored(StoredPartitionedBloom),
}

impl Debug for PrefixFilter {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::Inline { hashes, .. } => f
                .debug_struct("Inline")
                .field("prefixes", &hashes.len())
                .finish(),
            Self::Stored(filter) => f.debug_tuple("Stored").field(filter).finish(),
        }
    }
}

impl PrefixFilter {
    /// An inline filter over `hashes`, which it sorts and deduplicates.
    pub fn inline(mut hashes: Vec<u64>) -> Self {
        hashes.sort_unstable();
        hashes.dedup();
        Self::Inline {
            hashes: hashes.into_boxed_slice(),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        }
    }

    /// Whether a key whose prefix hashes to `hash` might be in the file.
    pub fn contains_hash(&self, hash: u64) -> bool {
        match self {
            Self::Inline {
                hashes,
                hits,
                misses,
            } => {
                let hit = hashes.binary_search(&hash).is_ok();
                if hit { hits } else { misses }.fetch_add(1, AtomicOrdering::Relaxed);
                hit
            }
            Self::Stored(filter) => filter.contains_hash(hash),
        }
    }

    /// Bytes in memory, bytes on storage, probe outcomes, and partition reads.
    pub fn stats(&self) -> FilterStats {
        match self {
            Self::Inline {
                hashes,
                hits,
                misses,
            } => FilterStats {
                size_byte: size_of::<Self>() + hashes.len() * size_of::<u64>(),
                hits: hits.load(AtomicOrdering::Relaxed),
                misses: misses.load(AtomicOrdering::Relaxed),
                ..FilterStats::default()
            },
            Self::Stored(filter) => filter.stats(),
        }
    }

    /// The filter kind, for statistics.
    pub fn kind(&self) -> FilterKind {
        FilterKind::Prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic::Erase;
    use proptest::prelude::*;

    fn bytes_key(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    #[test]
    fn byte_prefixes_hash_and_compare_by_their_leading_bytes() {
        let spec = PrefixSpec::bytes::<3>();
        let (a, b, c) = (bytes_key(b"abcX"), bytes_key(b"abcYZ"), bytes_key(b"abd"));
        assert_eq!(spec.hash(a.erase()), spec.hash(b.erase()));
        assert_ne!(spec.hash(a.erase()), spec.hash(c.erase()));
        assert_eq!(spec.cmp(a.erase(), b.erase()), Ordering::Equal);
        assert_eq!(spec.cmp(a.erase(), c.erase()), Ordering::Less);
        assert!(spec.has_prefix(b.erase(), a.erase()));
        assert_eq!(spec.kind(), PrefixKind::Bytes(3));
        assert_eq!(
            PrefixSpec::bytes_of_length(3).hash(a.erase()),
            spec.hash(a.erase())
        );
    }

    #[test]
    #[should_panic(expected = "shorter than its 3-byte filter prefix")]
    fn a_key_shorter_than_its_byte_prefix_panics() {
        PrefixSpec::bytes::<3>().hash(bytes_key(b"ab").erase());
    }

    #[test]
    fn tuple_prefixes_cover_their_leading_fields() {
        let spec = PrefixSpec::fields::<Tup2<i64, i32>, 1>();
        let key = |order: i64, line: i32| Tup2(order, line);
        assert_eq!(spec.hash(key(7, 1).erase()), spec.hash(key(7, 4).erase()));
        assert_ne!(spec.hash(key(7, 1).erase()), spec.hash(key(8, 1).erase()));
        assert_eq!(
            spec.cmp(key(7, 9).erase(), key(8, 0).erase()),
            Ordering::Less
        );
        assert_eq!(spec.kind(), PrefixKind::Fields(1));
        let whole = PrefixSpec::fields::<Tup2<i64, i32>, 2>();
        assert_ne!(whole.hash(key(7, 1).erase()), whole.hash(key(7, 4).erase()));
    }

    #[test]
    fn the_collector_keeps_one_hash_per_distinct_prefix() {
        let mut collector = PrefixCollector::new(PrefixSpec::bytes::<1>());
        for key in [b"a1", b"a2", b"b1", b"c1", b"c2"] {
            collector.push_key(bytes_key(key).erase());
        }
        assert_eq!(collector.into_hashes().len(), 3);
    }

    #[test]
    fn kinds_round_trip_through_their_encoding() {
        for kind in [PrefixKind::Bytes(9), PrefixKind::Fields(2)] {
            let (tag, length) = kind.to_raw();
            assert_eq!(PrefixKind::from_raw(tag, length), Some(kind));
        }
        assert_eq!(PrefixKind::from_raw(0, 1), None);
    }

    proptest! {
        #[test]
        fn an_inline_filter_has_no_false_negatives(
            keys in proptest::collection::btree_set(proptest::collection::vec(any::<u8>(), 2..6), 1..200),
        ) {
            let spec = PrefixSpec::bytes::<2>();
            let mut collector = PrefixCollector::new(spec);
            for key in &keys {
                collector.push_key(bytes_key(key).erase());
            }
            let filter = PrefixFilter::inline(collector.into_hashes());
            for key in &keys {
                prop_assert!(filter.contains_hash(spec.hash(bytes_key(key).erase())));
            }
        }
    }
}
