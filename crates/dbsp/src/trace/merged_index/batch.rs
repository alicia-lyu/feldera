//! Folded-key batches and consolidated reads across batches.

use crate::{
    DynZWeight,
    algebra::ZWeight,
    dynamic::{DynData, Erase},
    trace::{
        BatchLocation, BatchReader, BatchReaderFactories, Builder,
        cursor::CursorList,
        ord::{OrdIndexedWSet, OrdIndexedWSetFactories},
    },
};

use super::{FoldedKey, PayloadBytes};

pub(crate) type MergedIndexBatch = OrdIndexedWSet<DynData, DynData, DynZWeight>;
type BatchCursor<'a> = <MergedIndexBatch as BatchReader>::Cursor<'a>;

/// Sorts and consolidates contributions within one immutable initial batch.
/// A zero net weight has no stored row.
pub(crate) fn build_batch(
    rows: impl IntoIterator<Item = (FoldedKey, PayloadBytes, ZWeight)>,
) -> Result<MergedIndexBatch, String> {
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_unstable_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));

    let mut consolidated: Vec<(FoldedKey, PayloadBytes, ZWeight)> = Vec::with_capacity(rows.len());
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

    let factories = OrdIndexedWSetFactories::new::<Vec<u8>, Vec<u8>, ZWeight>();
    // Generic builders may select file storage during construction. L0 runs start in memory.
    let mut builder = <MergedIndexBatch as crate::trace::Batch>::Builder::with_capacity_in_location(
        &factories,
        consolidated.len(),
        consolidated.len(),
        Some(BatchLocation::Memory),
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
        builder.push_val_diff(payload.0.erase(), weight.erase());
        pending_key = Some(key);
    }
    if let Some(key) = pending_key {
        builder.push_key(key.0.erase());
    }
    Ok(builder.done())
}

/// Consolidates matching pairs without changing or copying the underlying batches.
pub(crate) fn batch_cursor(
    batches: &[MergedIndexBatch],
) -> CursorList<DynData, DynData, (), DynZWeight, BatchCursor<'_>> {
    let factories =
        OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, Vec<u8>, ZWeight>();
    CursorList::new(
        factories.weight_factory(),
        batches.iter().map(BatchReader::cursor).collect(),
    )
}
