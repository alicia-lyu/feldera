//! Readers: the partner reads of a circuit's joins, as typed buffers that one
//! scan session fills.
//!
//! A circuit keeps DBSP's own operators. Only the partner reads of its joins
//! are taken from the merged index, through the [`ScanSession`]. Each read is a
//! [`Reader`]: a declaration (one stream of the index and a projection of its
//! entries into the join's typed rows) together with its buffer. While the
//! session collects the touched groups, [`collect`] routes each buffered entry
//! to the readers of its stream; each reader projects it once, so a key is
//! unfolded once per entry and a payload decoded once, by the session. A
//! group's [`GroupBuffers`] are dropped as soon as its entries are routed.
//!
//! A sealed reader, [`ReaderRuns`], holds two batches of the join's type:
//!
//! * `Old`'s rows, the read before the transaction ([`ReaderRuns::before`]);
//! * `Dlt`'s rows, the transaction's driving delta ([`ReaderRuns::delta`]).
//!
//! The read after the transaction is both batches in one snapshot, kept apart
//! and unconsolidated, since a join is bilinear (`prop:overlay`).
//!
//! Whether a read is a path or a range read follows from the reader's key:
//! the join seeks its driving delta's keys in the reader's batches, which
//! hold every row of the touched groups. Each reader has its own buffer, so an
//! entry that two readers take is stored twice, once in each reader's shape;
//! the write-up's buffers are per stream (`def:session`).

use std::{collections::BTreeSet, sync::Arc};

use crate::{
    DBData, ZWeight,
    trace::SpineSnapshot as DynamicSnapshot,
    typed_batch::{BatchReader, OrdIndexedZSet, SpineSnapshot, TypedBatch},
    utils::Tup2,
};

use super::{GroupBuffers, MergedIndexDefinition, ScanSession};

/// Which part of the session's input an entry came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Half {
    /// `Old`, the index before the transaction.
    Old,
    /// `Dlt`, the transaction's changes to the index.
    Dlt,
}

/// Projects one entry of a stream, its unfolded key and decoded payload, into
/// a row of the join's typed input. It errors on an entry of another stream.
pub type Projection<D, K, V> = fn(
    &<D as MergedIndexDefinition>::Key,
    &<D as MergedIndexDefinition>::Payload,
) -> Result<(K, V), String>;

/// A reader as [`collect`] fills it, whatever its row type.
pub trait FilledReader<D: MergedIndexDefinition> {
    /// The id of the stream whose entries this reader takes.
    fn stream(&self) -> u8;

    /// Takes one entry of the reader's stream.
    fn push(
        &mut self,
        half: Half,
        key: &D::Key,
        payload: &D::Payload,
        weight: ZWeight,
    ) -> Result<(), String>;
}

/// One partner read: a stream of the index, a projection into the join's rows
/// `(K, V)`, and the rows taken so far.
pub struct Reader<D: MergedIndexDefinition, K: DBData, V: DBData> {
    stream: u8,
    project: Projection<D, K, V>,
    old: Vec<Tup2<Tup2<K, V>, ZWeight>>,
    dlt: Vec<Tup2<Tup2<K, V>, ZWeight>>,
}

impl<D: MergedIndexDefinition, K: DBData, V: DBData> Reader<D, K, V> {
    /// An empty reader of stream `stream`.
    pub fn new(stream: u8, project: Projection<D, K, V>) -> Self {
        Self {
            stream,
            project,
            old: Vec::new(),
            dlt: Vec::new(),
        }
    }

    /// Builds the reader's two batches. Each is consolidated within itself,
    /// never with the other.
    pub fn seal(self) -> ReaderRuns<K, V> {
        ReaderRuns {
            old: Arc::new(OrdIndexedZSet::from_tuples((), self.old).into_inner()),
            dlt: Arc::new(OrdIndexedZSet::from_tuples((), self.dlt).into_inner()),
        }
    }
}

impl<D: MergedIndexDefinition, K: DBData, V: DBData> FilledReader<D> for Reader<D, K, V> {
    fn stream(&self) -> u8 {
        self.stream
    }

    fn push(
        &mut self,
        half: Half,
        key: &D::Key,
        payload: &D::Payload,
        weight: ZWeight,
    ) -> Result<(), String> {
        let (key, value) = (self.project)(key, payload)?;
        let rows = match half {
            Half::Old => &mut self.old,
            Half::Dlt => &mut self.dlt,
        };
        rows.push(Tup2(Tup2(key, value), weight));
        Ok(())
    }
}

/// The dynamically typed batch of an [`OrdIndexedZSet`].
type Inner<K, V> = <OrdIndexedZSet<K, V> as BatchReader>::Inner;

/// A sealed reader: its rows of `Old` and of `Dlt`, as two batches.
pub struct ReaderRuns<K: DBData, V: DBData> {
    old: Arc<Inner<K, V>>,
    dlt: Arc<Inner<K, V>>,
}

impl<K: DBData, V: DBData> ReaderRuns<K, V> {
    /// The read before the transaction: `Old`'s rows.
    pub fn before(&self) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
        Self::snapshot([self.old.clone()])
    }

    /// The read after the transaction: `Old`'s rows, then `Dlt`'s, as two
    /// batches of one snapshot, unconsolidated.
    pub fn after(&self) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
        Self::snapshot([self.old.clone(), self.dlt.clone()])
    }

    /// The driving delta: `Dlt`'s rows. The batch is cloned, since the
    /// after read may share it; nothing is decoded or projected again.
    pub fn delta(&self) -> OrdIndexedZSet<K, V> {
        TypedBatch::new((*self.dlt).clone())
    }

    fn snapshot(
        batches: impl IntoIterator<Item = Arc<Inner<K, V>>>,
    ) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
        let mut snapshot =
            TypedBatch::new(DynamicSnapshot::new(OrdIndexedZSet::<K, V>::factories()));
        snapshot.inner_mut().extend_with_batches(batches);
        snapshot
    }
}

/// The groups that one session touched, by group prefix: the root domain's
/// tag and value.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TouchedGroups {
    prefixes: BTreeSet<Vec<u8>>,
}

impl TouchedGroups {
    /// Whether the session collected the group with prefix `prefix`.
    pub fn contains(&self, prefix: &[u8]) -> bool {
        self.prefixes.contains(prefix)
    }

    /// The number of touched groups.
    pub fn len(&self) -> usize {
        self.prefixes.len()
    }

    /// Whether no group was touched.
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }
}

/// Runs `session` over every touched group and routes each buffered entry to
/// the readers of its stream. The session collects only the readers' streams.
/// Returns the touched groups.
pub fn collect<D: MergedIndexDefinition>(
    session: &mut ScanSession<'_, D>,
    readers: &mut [&mut dyn FilledReader<D>],
) -> Result<TouchedGroups, String> {
    let inputs: BTreeSet<u8> = readers.iter().map(|reader| reader.stream()).collect();
    let mut touched = TouchedGroups::default();
    session.maintain(&inputs, None, |group, buffers| {
        touched.prefixes.insert(group.to_vec());
        route(&buffers, readers)
    })?;
    Ok(touched)
}

/// Routes one group's entries; the caller drops the buffers afterwards.
fn route<D: MergedIndexDefinition>(
    buffers: &GroupBuffers<D::Payload>,
    readers: &mut [&mut dyn FilledReader<D>],
) -> Result<(), String> {
    for (stream, buffer) in buffers.streams() {
        let halves = [(Half::Old, &buffer.old), (Half::Dlt, &buffer.dlt)];
        for (half, entries) in halves {
            for entry in entries {
                let key = D::unfold(&entry.key)?;
                for reader in readers
                    .iter_mut()
                    .filter(|reader| reader.stream() == stream)
                {
                    reader.push(half, &key, &entry.payload, entry.weight)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
