//! In-memory folded-key batches and ordered reads of their raw contributions.

use crate::{
    DynZWeight,
    algebra::ZWeight,
    dynamic::{DowncastTrait, DynData, Erase},
    trace::{
        BatchReader, BatchReaderFactories, Builder, Cursor,
        ord::vec::{VecIndexedWSet, VecIndexedWSetFactories},
    },
};

use super::{FoldedKey, PayloadBytes};

pub(crate) type MergedIndexBatch = VecIndexedWSet<DynData, DynData, DynZWeight>;
type BatchCursor<'a> = <MergedIndexBatch as BatchReader>::Cursor<'a>;

/// Sorts and consolidates contributions within one immutable in-memory batch.
/// A zero net weight has no stored row.
pub(crate) fn build_batch(
    rows: impl IntoIterator<Item = (FoldedKey, PayloadBytes, ZWeight)>,
) -> Result<MergedIndexBatch, String> {
    let mut rows: Vec<_> = rows.into_iter().collect();
    rows.sort_unstable_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));

    let mut consolidated: Vec<(FoldedKey, PayloadBytes, ZWeight)> = Vec::with_capacity(rows.len());
    for (key, payload, weight) in rows {
        if let Some((last_key, last_payload, last_weight)) = consolidated.last_mut() {
            if *last_key == key && *last_payload == payload {
                *last_weight = last_weight
                    .checked_add(weight)
                    .ok_or_else(|| "merged-index batch weight overflow".to_string())?;
                continue;
            }
        }
        consolidated.push((key, payload, weight));
    }

    let factories = VecIndexedWSetFactories::new::<Vec<u8>, Vec<u8>, ZWeight>();
    let mut builder = <MergedIndexBatch as crate::trace::Batch>::Builder::with_capacity(
        &factories,
        consolidated.len(),
        consolidated.len(),
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

/// Reads stored rows across batches without merging equal rows between batches.
pub(crate) struct RawBatchCursor<'a> {
    cursors: Vec<BatchCursor<'a>>,
    current: Option<usize>,
}

impl<'a> RawBatchCursor<'a> {
    pub fn new(batches: &'a [MergedIndexBatch]) -> Self {
        let mut result = Self {
            cursors: batches.iter().map(BatchReader::cursor).collect(),
            current: None,
        };
        result.select_current();
        result
    }

    /// Positions each batch at its first key greater than or equal to `key`.
    pub fn seek_ge(&mut self, key: &[u8]) {
        let key = key.to_vec();
        for cursor in &mut self.cursors {
            cursor.rewind_keys();
            cursor.seek_key(key.erase());
            if cursor.key_valid() {
                cursor.rewind_vals();
            }
        }
        self.select_current();
    }

    /// The returned slices and weight are borrowed until this cursor advances.
    pub fn row(&mut self) -> Option<(&[u8], &[u8], ZWeight)> {
        let cursor = self.cursors.get_mut(self.current?)?;
        let weight = *cursor.weight().downcast_checked::<ZWeight>();
        let key = cursor.key().downcast_checked::<Vec<u8>>();
        let val = cursor.val().downcast_checked::<Vec<u8>>();
        Some((key, val, weight))
    }

    pub fn next(&mut self) {
        if let Some(index) = self.current {
            let cursor = &mut self.cursors[index];
            cursor.step_val();
            if !cursor.val_valid() {
                cursor.step_key();
                if cursor.key_valid() {
                    cursor.rewind_vals();
                }
            }
            self.select_current();
        }
    }

    fn select_current(&mut self) {
        self.current = self
            .cursors
            .iter()
            .enumerate()
            .filter(|(_, cursor)| cursor.key_valid() && cursor.val_valid())
            .min_by(|(left_index, left), (right_index, right)| {
                (left.key(), left.val(), left_index).cmp(&(right.key(), right.val(), right_index))
            })
            .map(|(index, _)| index);
    }
}
