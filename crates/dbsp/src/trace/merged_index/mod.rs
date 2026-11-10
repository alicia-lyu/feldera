//! Schema-independent folded keys, byte-backed batches, and weighted layer-file rows.
//! Concrete index definitions and payload codecs belong to client crates.

use crate::{
    DBData,
    algebra::ZWeight,
    dynamic::{DynData, DynUnit, DynWeight, Erase},
    storage::{backend::StorageError, file::writer::Writer2},
};

pub mod batch;
pub mod ordered;
pub mod packed;
pub mod run_merge;
pub mod session;
pub mod testing;
pub mod trie;

pub use batch::MergedIndexBatch;
pub use ordered::OrderedKey;
pub use packed::{PackedField, PayloadDecoder, PayloadEncoder};
pub use run_merge::RunMerge;
pub use session::{Entry, GroupBuffers, Pass, ScanSession, SessionCounts, StreamBuffer};
pub use trie::{ColumnRef, Domain, KeyTrie, Node, Payload, PlacedIndex, Placement, Via};

/// Terminates the key fields of a folded key. No domain may use it as a tag.
const INDEX_TAG: u8 = 0;

/// The smallest byte string greater than every string that starts with
/// `prefix`, so that `[prefix, successor)` holds exactly the strings with that
/// prefix. `None` when no such string exists: the prefix is empty or all
/// `0xFF`, and its range runs to the end of the index.
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let kept = prefix.iter().rposition(|&byte| byte != u8::MAX)? + 1;
    let mut successor = prefix[..kept].to_vec();
    // The kept bytes end in a byte below 0xFF, so the increment cannot overflow.
    successor[kept - 1] += 1;
    Some(successor)
}

/// A static merged-index definition: a key trie plus typed folding.
///
/// A folded key is `[tag value]* 0 id`. It has one tag and one value per
/// domain on the path to its placement, then the terminator and the placement
/// id. Tags and ids are derived from `TRIE` (see [`KeyTrie`]). The functions
/// below hard-code them as constants, and tests check them against the trie.
pub trait MergedIndexDefinition {
    type Key: Clone + Eq + std::fmt::Debug;

    /// The decoded payload of any stream of the index.
    type Payload;

    const TRIE: KeyTrie;

    /// Folds a key. Typed fields make this infallible.
    fn fold(key: &Self::Key) -> FoldedKey;

    /// The inverse of [`fold`](Self::fold). Rejects bytes that `fold` could not
    /// have produced.
    fn unfold(bytes: &[u8]) -> Result<Self::Key, String>;

    /// Decodes the stored payload `bytes` of the stream with id `stream_id`:
    /// the last byte of a folded key. The id alone selects the payload type, so
    /// the key need not be unfolded. Rejects an unknown id and bytes that the
    /// stream's encoder could not have produced.
    fn decode_payload(stream_id: u8, bytes: &[u8]) -> Result<Self::Payload, String>;
}

/// A complete folded key. Its byte ordering is its query ordering.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FoldedKey(Vec<u8>);

impl FoldedKey {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Writes the `[tag value]*` part of a folded key. A definition lists its
/// `(tag, value)` pairs in path order and picks one of the three endings.
#[derive(Debug, Default)]
pub struct KeyEncoder {
    bytes: Vec<u8>,
}

impl KeyEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the tag of a domain, then the ordered encoding of the value.
    pub fn field<T: OrderedKey>(mut self, tag: u8, value: &T) -> Self {
        self.bytes.push(tag);
        value.encode_ordered(&mut self.bytes);
        self
    }

    /// Ends a complete key with the terminator and the placement id.
    pub fn finish(mut self, placement_id: u8) -> FoldedKey {
        self.bytes.push(INDEX_TAG);
        self.bytes.push(placement_id);
        FoldedKey(self.bytes)
    }

    /// The bytes shared by every key below the fields written so far.
    pub fn prefix(self) -> Vec<u8> {
        self.bytes
    }

    /// A prefix that ends after the tag of the next field, before its value.
    pub fn then_tag(mut self, tag: u8) -> Vec<u8> {
        self.bytes.push(tag);
        self.bytes
    }
}

/// Reads a folded key written by [`KeyEncoder`].
#[derive(Debug)]
pub struct KeyDecoder<'a> {
    fields: &'a [u8],
}

impl<'a> KeyDecoder<'a> {
    /// Checks the terminator and splits off the placement id. The decoder then
    /// reads the fields that precede the terminator.
    pub fn new(bytes: &'a [u8]) -> Result<(u8, Self), String> {
        let (&placement_id, body) = bytes
            .split_last()
            .ok_or_else(|| "empty folded key".to_string())?;
        let (&terminator, fields) = body
            .split_last()
            .ok_or_else(|| "folded key has no terminator".to_string())?;
        if terminator != INDEX_TAG {
            return Err("folded key has an invalid terminator".into());
        }
        Ok((placement_id, Self { fields }))
    }

    /// Reads the next field, which must carry `tag`.
    pub fn field<T: OrderedKey>(&mut self, tag: u8) -> Result<T, String> {
        let (&found_tag, after_tag) = self
            .fields
            .split_first()
            .ok_or_else(|| "folded key ends before the expected field".to_string())?;
        if found_tag != tag {
            return Err(format!(
                "folded key has domain tag {found_tag}, expected {tag}"
            ));
        }
        if after_tag.len() < T::WIDTH {
            return Err("folded key has a truncated field".into());
        }
        let (value_bytes, rest) = after_tag.split_at(T::WIDTH);
        let value = T::decode_ordered(value_bytes)
            .ok_or_else(|| "folded key has a non-canonical field".to_string())?;
        self.fields = rest;
        Ok(value)
    }

    /// Checks that every field byte was consumed.
    pub fn finish(self) -> Result<(), String> {
        if self.fields.is_empty() {
            Ok(())
        } else {
            Err("folded key has trailing bytes".into())
        }
    }
}

/// Adds all children for one parent, then writes the parent exactly once.
/// The caller supplies a strictly increasing parent key and strictly
/// increasing payloads.
pub fn write_parent<P: DBData>(
    writer: &mut Writer2<DynData, DynUnit, DynData, DynWeight>,
    key: &FoldedKey,
    children: &[(P, ZWeight)],
) -> Result<(), StorageError> {
    assert!(
        !children.is_empty(),
        "a layer-file parent requires children"
    );
    for (payload, weight) in children {
        writer.write1((payload.erase(), weight.erase()))?;
    }
    writer.write0((key.0.erase(), ().erase()))
}

#[cfg(test)]
mod tests;
