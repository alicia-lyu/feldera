//! The compaction-memory charge: what a merge or a seal reserves in the buffer
//! cache under `storage.compaction_memory` `charge` (see
//! `docs/design/merged-index/memory-model.md`).

use crate::{
    Runtime,
    storage::{buffer_cache::Reservation, file::writer::Parameters},
};

/// Columns of a layer file that holds an indexed Z-set: keys and values. A
/// Z-set of keys alone has one, so this over-estimates it.
const FILE_COLUMNS: usize = 2;

/// Open index blocks per column of a file being written, one per level of the
/// column's tree. Three levels hold millions of entries at the default block
/// size. An assumption, not a measurement.
const OPEN_INDEX_LEVELS: usize = 3;

/// The writer's buffers beside its open blocks, in data blocks: the block it
/// compresses into, and the bounce buffer it compresses through.
const WRITER_BUFFER_BLOCKS: usize = 2;

/// Estimates the bytes of buffers that a merge or a seal of `input_runs` runs
/// with `input_keys` keys holds from before its first read until its output is
/// closed:
///
/// - one data block per column of each input run's cursor, at the writer's
///   `min_data_block` (the runs' blocks are at least that big);
/// - the output builder's open data block and open index blocks, per column;
/// - the writer's buffers, [`WRITER_BUFFER_BLOCKS`] data blocks;
/// - the key filter under construction, sized for every input key at the
///   configured false positive rate, since the merge cannot know how many keys
///   survive consolidation.
///
/// It leaves out the input runs' index blocks along a cursor's path, which the
/// cache holds as ordinary blocks, and the transient allocations of one key
/// and its values.
pub(super) fn compaction_buffer_bytes(
    parameters: &Parameters,
    input_runs: usize,
    input_keys: usize,
    bloom_false_positive_rate: f64,
) -> usize {
    let input_cursors = input_runs * FILE_COLUMNS * parameters.min_data_block;
    let output_open_blocks =
        FILE_COLUMNS * (parameters.min_data_block + OPEN_INDEX_LEVELS * parameters.min_index_block);
    let writer_buffers = WRITER_BUFFER_BLOCKS * parameters.min_data_block;
    input_cursors
        + output_open_blocks
        + writer_buffers
        + bloom_filter_bytes(input_keys, bloom_false_positive_rate)
}

/// Bytes of a Bloom filter for `keys` keys at false positive rate `rate`, or 0
/// if `rate` asks for no filter (it is outside `(0, 1)`). The optimal bits per
/// key is `-ln(rate) / ln(2)^2`, as in `PartitionedBloomShape::for_keys`.
fn bloom_filter_bytes(keys: usize, rate: f64) -> usize {
    if !(rate > 0.0 && rate < 1.0) {
        return 0;
    }
    let ln2 = std::f64::consts::LN_2;
    let bits_per_key = -rate.ln() / (ln2 * ln2);
    (keys as f64 * bits_per_key / 8.0).ceil() as usize
}

/// Returns the bytes that a job with these inputs reserves, or `None` when the
/// charge is off, so the job allocates as it always did.
pub(super) fn compaction_charge_bytes(input_runs: usize, input_keys: usize) -> Option<usize> {
    Runtime::charge_compaction_memory().then(|| {
        compaction_buffer_bytes(
            &Runtime::file_writer_parameters(),
            input_runs,
            input_keys,
            Runtime::bloom_false_positive_rate(),
        )
    })
}

/// Reserves `bytes` in the merger's buffer cache, waiting for room.
///
/// Called on a merger task, where [`Runtime::buffer_cache`] is the cache the
/// task's writer inserts into.
pub(super) async fn reserve_in_merger_cache(bytes: usize) -> Reservation {
    Runtime::buffer_cache()
        .expect("a merger task has a buffer cache")
        .reserve(bytes)
        .await
}

#[cfg(test)]
mod tests {
    use super::{bloom_filter_bytes, compaction_buffer_bytes};
    use crate::storage::file::writer::Parameters;

    #[test]
    fn the_estimate_adds_inputs_output_writer_and_filter() {
        let parameters = Parameters::default();
        let block = 8192;
        assert_eq!(parameters.min_data_block, block);
        assert_eq!(parameters.min_index_block, block);

        // No filter: 3 inputs x 2 columns, then 2 columns x (1 data + 3 index blocks), then 2 writer blocks.
        assert_eq!(
            compaction_buffer_bytes(&parameters, 3, 1_000_000, 0.0),
            (3 * 2 + 2 * (1 + 3) + 2) * block
        );

        // A filter adds its bytes, and an input run adds two data blocks.
        let with_filter = compaction_buffer_bytes(&parameters, 3, 1_000_000, 0.0001);
        let without_filter = compaction_buffer_bytes(&parameters, 3, 1_000_000, 0.0);
        assert_eq!(
            with_filter - without_filter,
            bloom_filter_bytes(1_000_000, 0.0001)
        );
        assert_eq!(
            compaction_buffer_bytes(&parameters, 4, 1_000_000, 0.0) - without_filter,
            2 * block
        );
    }

    #[test]
    fn a_bloom_filter_costs_bits_per_key_by_rate() {
        // 1% needs 9.585 bits per key: 1,198,133 bytes for a million keys.
        assert_eq!(bloom_filter_bytes(1_000_000, 0.01), 1_198_133);
        assert_eq!(bloom_filter_bytes(0, 0.01), 0);
        for no_filter in [0.0, 1.0, -0.5, 2.0, f64::NAN] {
            assert_eq!(bloom_filter_bytes(1_000_000, no_filter), 0, "{no_filter}");
        }
    }
}
