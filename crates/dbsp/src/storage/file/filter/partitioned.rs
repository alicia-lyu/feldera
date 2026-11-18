//! Storage-resident Bloom filters, split into partitions read through the
//! buffer cache.
//!
//! A memory-resident filter is read whole when its file is opened and stays in
//! memory until the batch is dropped, whether or not anything ever probes it.
//! A partitioned filter stays on storage instead. Its file holds a small header
//! block and `n_partitions` partition blocks of [`PARTITION_BYTES`] each, laid
//! out back to back. A key's hash picks one partition, and a probe reads only
//! that partition, through the thread's buffer cache, where it competes with
//! data blocks under the cache's eviction policy. A filter that nothing probes
//! is never read, and a cold partition is evicted like any other block.
//!
//! Within a partition the filter is an ordinary Bloom filter: `hashes` bit
//! positions per key, derived by double hashing from the key's hash. This is a
//! blocked Bloom filter whose block is one storage block. Hashing a key to a
//! partition, rather than splitting the key range, needs no partition index
//! and no key comparison, so the resident part of a filter is a few scalars.
//! It gives no locality between neighboring keys, which a key range split
//! would only give to workloads whose probes cluster by key.
//!
//! Bits per key follow the same false positive rates as the modular filters
//! (see [`super::bloom`]): 4.8 bits per key per decade of the rate.
//!
//! A filter that fits in one partition is not stored this way
//! ([`super::BatchKeyFilter::new_bloom`]): it stays memory-resident, since
//! storing it would cost a partition's write per small run.

use super::bloom::{modules_for_rate, rate_for_modules};
use crate::storage::{
    backend::{BlockLocation, FileReader, StorageError},
    buffer_cache::{BufferCache, FBuf},
    file::{FilterStats, format::PARTITIONED_BLOOM_PARTITION_MAGIC},
};
use crc32c::crc32c;
use feldera_buffer_cache::CacheEntry;
use std::{
    fmt::{Debug, Formatter, Result as FmtResult},
    sync::{
        Arc, Once,
        atomic::{AtomicUsize, Ordering},
    },
};
use tracing::warn;

/// Size of one partition block on storage, including its block header.
pub const PARTITION_BYTES: usize = 4096;

/// Bytes of the block header (checksum and magic) at the start of a partition.
const PARTITION_HEADER_BYTES: usize = 8;

/// `u64` words of filter bits in one partition.
pub(crate) const PARTITION_WORDS: usize = (PARTITION_BYTES - PARTITION_HEADER_BYTES) / 8;

/// Filter bits in one partition.
const PARTITION_BITS: u64 = PARTITION_WORDS as u64 * 64;

/// The number of partitions and of hashes per key of a partitioned filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionedBloomShape {
    /// Number of partitions, at least 1.
    pub n_partitions: u32,
    /// Bit positions set per key, at least 1.
    pub hashes: u32,
}

impl PartitionedBloomShape {
    /// The shape for `estimated_keys` keys at the false positive rate `rate`.
    ///
    /// # Returns
    ///
    /// `None` when `rate` asks for no filter (it is at or outside `(0, 1)`) or
    /// when the partition count does not fit a `u32`.
    pub fn for_keys(estimated_keys: u64, rate: f64) -> Option<Self> {
        let modules = modules_for_rate(rate);
        if modules == 0 {
            return None;
        }
        // The optimal bits per key of a Bloom filter at rate p is
        // -ln(p) / ln(2)^2, and the optimal hash count is that times ln(2).
        let rate = rate_for_modules(modules);
        let ln2 = std::f64::consts::LN_2;
        let bits_per_key = -rate.ln() / (ln2 * ln2);
        let hashes = ((bits_per_key * ln2).round() as u32).max(1);
        let bits = (estimated_keys as f64 * bits_per_key).ceil() as u64;
        let n_partitions = u32::try_from(bits.div_ceil(PARTITION_BITS).max(1)).ok()?;
        Some(Self {
            n_partitions,
            hashes,
        })
    }

    /// Bytes the partitions occupy on storage.
    pub fn stored_bytes(&self) -> usize {
        self.n_partitions as usize * PARTITION_BYTES
    }

    /// The partition that holds `hash`.
    ///
    /// Uses the high bits of `hash` (a multiply-shift reduction), so that the
    /// bit positions, which come from a remix of the whole hash, are
    /// independent of the choice of partition.
    fn partition_of(&self, hash: u64) -> usize {
        ((u128::from(hash) * u128::from(self.n_partitions)) >> 64) as usize
    }

    /// The bit positions of `hash` within its partition.
    fn bit_positions(&self, hash: u64) -> impl Iterator<Item = u64> {
        let mixed = remix(hash);
        let first = mixed & 0xffff_ffff;
        let step = (mixed >> 32) | 1;
        (0..u64::from(self.hashes)).map(move |i| (first + i * step) % PARTITION_BITS)
    }
}

/// SplitMix64's finalizer: decorrelates the bit positions from the high bits
/// that chose the partition.
fn remix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn contains(words: &[u64], shape: &PartitionedBloomShape, hash: u64) -> bool {
    shape
        .bit_positions(hash)
        .all(|bit| words[(bit / 64) as usize] & (1 << (bit % 64)) != 0)
}

/// A partitioned filter being built by a file writer, held in memory until
/// the writer stores it.
pub struct PartitionedBloomBuilder {
    shape: PartitionedBloomShape,
    /// All partitions' words, partition after partition.
    words: Vec<u64>,
}

impl Debug for PartitionedBloomBuilder {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("PartitionedBloomBuilder")
            .field("shape", &self.shape)
            .finish()
    }
}

impl PartitionedBloomBuilder {
    /// Starts an empty filter of `shape`.
    pub fn new(shape: PartitionedBloomShape) -> Self {
        Self {
            shape,
            words: vec![0; shape.n_partitions as usize * PARTITION_WORDS],
        }
    }

    /// The shape the filter is built with.
    pub fn shape(&self) -> &PartitionedBloomShape {
        &self.shape
    }

    /// Adds a key's hash.
    pub fn insert_hash(&mut self, hash: u64) {
        let partition = self.shape.partition_of(hash);
        let words = &mut self.words[partition * PARTITION_WORDS..][..PARTITION_WORDS];
        for bit in self.shape.bit_positions(hash) {
            words[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    /// Returns whether `hash` might have been inserted. For tests: a reader
    /// probes the stored filter.
    #[cfg(test)]
    pub fn contains_hash(&self, hash: u64) -> bool {
        let partition = self.shape.partition_of(hash);
        contains(
            &self.words[partition * PARTITION_WORDS..][..PARTITION_WORDS],
            &self.shape,
            hash,
        )
    }

    /// Memory the builder holds.
    pub fn size_byte(&self) -> usize {
        size_of::<Self>() + self.words.len() * 8
    }

    /// The partition blocks to write, in order, each [`PARTITION_BYTES`] long
    /// with its magic in place and its checksum left for the block writer.
    pub fn partition_blocks(&self) -> impl Iterator<Item = FBuf> + '_ {
        self.words.chunks(PARTITION_WORDS).map(|words| {
            let mut block = FBuf::with_capacity(PARTITION_BYTES);
            block.extend_from_slice(&[0; 4]);
            block.extend_from_slice(&PARTITIONED_BLOOM_PARTITION_MAGIC);
            for word in words {
                block.extend_from_slice(&word.to_le_bytes());
            }
            debug_assert_eq!(block.len(), PARTITION_BYTES);
            block
        })
    }
}

/// One partition's bits, as the buffer cache holds them.
pub(crate) struct FilterPartition {
    words: Box<[u64]>,
}

impl CacheEntry for FilterPartition {
    fn cost(&self) -> usize {
        size_of::<Self>() + self.words.len() * 8
    }
}

impl FilterPartition {
    /// Parses a partition block read from storage, checking its checksum and
    /// magic.
    fn from_block(block: &FBuf) -> Result<Self, String> {
        if block.len() != PARTITION_BYTES {
            return Err(format!(
                "partition block holds {} bytes, expected {PARTITION_BYTES}",
                block.len()
            ));
        }
        let checksum = u32::from_le_bytes(block[..4].try_into().unwrap());
        let computed = crc32c(&block[4..]);
        if checksum != computed {
            return Err(format!(
                "partition checksum {checksum:#x}, computed {computed:#x}"
            ));
        }
        if block[4..8] != PARTITIONED_BLOOM_PARTITION_MAGIC {
            return Err(format!("partition magic {:?}", &block[4..8]));
        }
        let words = block[PARTITION_HEADER_BYTES..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect();
        Ok(Self { words })
    }
}

/// A partitioned filter on storage, probed one partition at a time through
/// the buffer cache.
pub struct StoredPartitionedBloom {
    shape: PartitionedBloomShape,
    /// Offset of partition 0; partition `i` follows at `i * PARTITION_BYTES`.
    first_partition_offset: u64,
    file: Arc<dyn FileReader>,
    cache: fn() -> Option<Arc<BufferCache>>,
    /// Probes that could not rule the key out.
    hits: AtomicUsize,
    /// Probes that ruled the key out.
    misses: AtomicUsize,
    /// Partitions read from storage (buffer-cache misses).
    partition_loads: AtomicUsize,
    /// Partitions found in the buffer cache.
    partition_cache_hits: AtomicUsize,
}

impl Debug for StoredPartitionedBloom {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("StoredPartitionedBloom")
            .field("shape", &self.shape)
            .field("first_partition_offset", &self.first_partition_offset)
            .field("file", &self.file)
            .finish()
    }
}

impl StoredPartitionedBloom {
    /// A handle on the filter whose partitions start at
    /// `first_partition_offset` in `file`.
    pub fn new(
        shape: PartitionedBloomShape,
        first_partition_offset: u64,
        file: Arc<dyn FileReader>,
        cache: fn() -> Option<Arc<BufferCache>>,
    ) -> Self {
        Self {
            shape,
            first_partition_offset,
            file,
            cache,
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
            partition_loads: AtomicUsize::new(0),
            partition_cache_hits: AtomicUsize::new(0),
        }
    }

    /// The filter's shape.
    pub fn shape(&self) -> &PartitionedBloomShape {
        &self.shape
    }

    fn location(&self, partition: usize) -> BlockLocation {
        BlockLocation::new(
            self.first_partition_offset + (partition * PARTITION_BYTES) as u64,
            PARTITION_BYTES,
        )
        .expect("partitions are 512-aligned by construction")
    }

    /// The partition `partition`, from the buffer cache or else from storage,
    /// in which case it is put in the cache.
    fn partition(&self, partition: usize) -> Result<Arc<FilterPartition>, String> {
        let location = self.location(partition);
        let cache = (self.cache)();
        if let Some(cache) = &cache
            && let Some(entry) = cache.get(&*self.file, location)
        {
            self.partition_cache_hits.fetch_add(1, Ordering::Relaxed);
            return entry
                .downcast()
                .ok_or_else(|| format!("cache entry at {location} is not a filter partition"));
        }
        let block = self
            .file
            .read_block(location)
            .map_err(|error: StorageError| error.to_string())?;
        let entry = Arc::new(FilterPartition::from_block(&block)?);
        self.partition_loads.fetch_add(1, Ordering::Relaxed);
        if let Some(cache) = cache {
            cache.insert(self.file.file_id(), location.offset, entry.clone());
        }
        Ok(entry)
    }

    /// Returns whether `hash` might have been inserted, counting the outcome.
    ///
    /// A partition that cannot be read answers "might be present": the filter
    /// may only ever save a lookup, never decide one, so the caller's exact
    /// lookup then reports the real state or the real error.
    pub fn contains_hash(&self, hash: u64) -> bool {
        let is_hit = match self.partition(self.shape.partition_of(hash)) {
            Ok(partition) => contains(&partition.words, &self.shape, hash),
            Err(error) => {
                static WARN: Once = Once::new();
                WARN.call_once(|| {
                    warn!(
                        "{}: filter partition unreadable, treating keys as present: {error}",
                        self.file.path()
                    )
                });
                true
            }
        };
        if is_hit {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        is_hit
    }

    /// Statistics: memory-resident bytes (this handle), stored bytes, probe
    /// outcomes, and partition reads.
    pub fn stats(&self) -> FilterStats {
        FilterStats {
            size_byte: size_of::<Self>(),
            stored_byte: self.shape.stored_bytes(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            partition_loads: self.partition_loads.load(Ordering::Relaxed),
            partition_cache_hits: self.partition_cache_hits.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PARTITION_BITS, PARTITION_BYTES, PartitionedBloomBuilder, PartitionedBloomShape, remix,
    };
    use proptest::prelude::*;

    fn hash(i: u64) -> u64 {
        remix(i.wrapping_add(0x1234_5678))
    }

    /// The shape matches the modular filters' bits per key at each rate.
    #[test]
    fn shape_follows_the_rate() {
        assert_eq!(PartitionedBloomShape::for_keys(1000, 0.0), None);
        assert_eq!(PartitionedBloomShape::for_keys(1000, 1.0), None);
        let shape = PartitionedBloomShape::for_keys(1_000_000, 1e-4).unwrap();
        assert_eq!(shape.hashes, 13);
        let bits_per_key = shape.n_partitions as f64 * PARTITION_BITS as f64 / 1e6;
        assert!((19.1..19.3).contains(&bits_per_key), "{bits_per_key}");
        let coarse = PartitionedBloomShape::for_keys(1_000_000, 0.1).unwrap();
        assert!(coarse.n_partitions * 3 < shape.n_partitions);
        assert_eq!(
            PartitionedBloomShape::for_keys(0, 1e-4)
                .unwrap()
                .n_partitions,
            1
        );
    }

    /// The false positive rate stays near the target.
    #[test]
    fn false_positive_rate_near_target() {
        let keys = 200_000u64;
        let shape = PartitionedBloomShape::for_keys(keys, 1e-2).unwrap();
        let mut builder = PartitionedBloomBuilder::new(shape);
        (0..keys).for_each(|i| builder.insert_hash(hash(i)));
        let probes = 200_000u64;
        let false_positives = (keys..keys + probes)
            .filter(|i| builder.contains_hash(hash(*i)))
            .count();
        let rate = false_positives as f64 / probes as f64;
        assert!(rate < 0.015, "false positive rate {rate}");
    }

    #[test]
    fn partition_blocks_have_the_partition_size() {
        let shape = PartitionedBloomShape::for_keys(10_000, 1e-4).unwrap();
        let builder = PartitionedBloomBuilder::new(shape);
        let blocks: Vec<_> = builder.partition_blocks().collect();
        assert_eq!(blocks.len(), shape.n_partitions as usize);
        assert!(blocks.iter().all(|block| block.len() == PARTITION_BYTES));
    }

    proptest! {
        /// A partitioned filter has no false negatives, whatever the keys and
        /// the number of partitions.
        #[test]
        fn no_false_negatives(hashes in prop::collection::vec(any::<u64>(), 0..2000),
                              estimated in 0u64..5000) {
            let shape = PartitionedBloomShape::for_keys(estimated, 1e-4).unwrap();
            let mut builder = PartitionedBloomBuilder::new(shape);
            for hash in &hashes {
                builder.insert_hash(*hash);
            }
            for hash in &hashes {
                prop_assert!(builder.contains_hash(*hash));
            }
        }
    }
}
