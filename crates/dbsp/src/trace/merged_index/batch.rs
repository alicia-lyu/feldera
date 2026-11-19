//! Folded-key batches and consolidated reads across batches.

use crate::{
    DBData, DynZWeight,
    algebra::ZWeight,
    dynamic::{DynData, Erase},
    storage::file::FilterSpec,
    trace::{
        BatchReader, BatchReaderFactories, Builder,
        cursor::CursorList,
        ord::{OrdIndexedWSet, OrdIndexedWSetFactories},
    },
};

use super::{FoldedKey, KeyTrie};

/// The factories of an index's runs, whose payload type is `P`: files get a
/// prefix filter over the trie's group prefix and no whole-key filter, since a
/// group read seeks by prefix and the index makes no exact probe. Use the same
/// factories for the index's spine and for the batches inserted into it, so
/// that every run, merged or restored, has the filter.
pub fn index_factories<P: DBData>(
    trie: &KeyTrie,
) -> OrdIndexedWSetFactories<DynData, DynData, DynZWeight> {
    OrdIndexedWSetFactories::new::<Vec<u8>, P, ZWeight>()
        .with_filter_spec(FilterSpec::prefix_only(trie.root_prefix_spec()))
}

/// Folded byte keys with typed payloads. The payload type is erased here;
/// [`build_batch`] and [`batch_cursor`] name it.
pub type MergedIndexBatch = OrdIndexedWSet<DynData, DynData, DynZWeight>;
type BatchCursor<'a> = <MergedIndexBatch as BatchReader>::Cursor<'a>;

/// Sorts and consolidates contributions within one immutable initial batch.
/// A zero net weight has no stored row. Files get DBSP's default filters; an
/// index's own batches use [`build_batch_in`] with [`index_factories`].
pub fn build_batch<P: DBData>(
    rows: impl IntoIterator<Item = (FoldedKey, P, ZWeight)>,
) -> Result<MergedIndexBatch, String> {
    build_batch_in(&OrdIndexedWSetFactories::new::<Vec<u8>, P, ZWeight>(), rows)
}

/// [`build_batch`] with `factories`, which must have been made for payload
/// type `P`.
pub fn build_batch_in<P: DBData>(
    factories: &OrdIndexedWSetFactories<DynData, DynData, DynZWeight>,
    rows: impl IntoIterator<Item = (FoldedKey, P, ZWeight)>,
) -> Result<MergedIndexBatch, String> {
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_unstable_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));

    let mut consolidated: Vec<(FoldedKey, P, ZWeight)> = Vec::with_capacity(rows.len());
    for (key, payload, weight) in rows {
        if let Some((last_key, last_payload, last_weight)) = consolidated.last_mut()
            && *last_key == key
            && *last_payload == payload
        {
            *last_weight = last_weight
                .checked_add(weight)
                .ok_or_else(|| "merged-index batch weight overflow".to_string())?;
            continue;
        }
        consolidated.push((key, payload, weight));
    }

    // No fixed location: the batch is placed as an operator's output is, by the runtime's step
    // storage threshold (`min_step_storage_bytes`; zero at Critical memory pressure). Outside a
    // runtime with storage, or without a threshold, it is built in memory.
    let mut builder = <MergedIndexBatch as crate::trace::Batch>::Builder::with_capacity_in_location(
        factories,
        consolidated.len(),
        consolidated.len(),
        None,
    );
    let mut pending_key: Option<FoldedKey> = None;
    for (key, payload, weight) in consolidated {
        if weight == 0 {
            continue;
        }
        if pending_key
            .as_ref()
            .is_some_and(|previous| *previous != key)
        {
            builder.push_key(pending_key.take().unwrap().0.erase());
        }
        builder.push_val_diff(payload.erase(), weight.erase());
        pending_key = Some(key);
    }
    if let Some(key) = pending_key {
        builder.push_key(key.0.erase());
    }
    Ok(builder.done())
}

/// Consolidates matching pairs without changing or copying the underlying
/// batches, whose payloads are of type `P`.
pub fn batch_cursor<P: DBData>(
    batches: &[MergedIndexBatch],
) -> CursorList<DynData, DynData, (), DynZWeight, BatchCursor<'_>> {
    let factories =
        OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, P, ZWeight>();
    CursorList::new(
        factories.weight_factory(),
        batches.iter().map(BatchReader::cursor).collect(),
    )
}
