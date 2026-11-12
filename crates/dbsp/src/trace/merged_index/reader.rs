//! Readers: the partner reads of a circuit's joins, as typed batches that the
//! routing of one scan session fills.
//!
//! A circuit keeps DBSP's own operators. Only the partner reads of its joins
//! are taken from the merged index. Each read is a [`Reader`]. It owns:
//!
//! * a *driving delta*, the delta input of the join that the reader serves,
//!   as a typed batch built from `Dlt` ([`DrivingDelta::Stream`]), or, when
//!   the delta is the output of another join, only its *requests*
//!   ([`DrivingDelta::Join`]);
//! * a *partner batch*, the rows of the other input that the driving delta
//!   asks for, which the join seeks. It holds only the requested slice of
//!   the index: the rows of the requested nodes, not of the whole touched
//!   groups.
//!
//! The *requests* of a reader are the prefixes of its driving delta's keys at
//! the reader's request node: a sorted list of folded-key prefixes of a fixed
//! length. The reader keeps an index entry only if the entry belongs to its
//! partner stream and its key starts with a request.
//!
//! # Static and dynamic parts
//!
//! What the circuit fixes lives in the reader's type, a [`ReaderDefinition`]:
//! the partner stream, the depth of the request node, the driving delta
//! ([`DrivingDelta`]), and the two functions that unfold an index entry into
//! the typed rows of the partner batch and of the driving delta. Only the
//! requests come from the data.
//!
//! # A transaction
//!
//! 1. [`capture`] reads `Dlt` once. It lists the touched groups, hands each
//!    delta entry to the readers it drives (which unfold it straight into
//!    their driving-delta builder, one copy, and note its request), and
//!    counts the payloads it decodes.
//! 2. A [`ScanSession`] visits the touched groups of `Old`. For each position,
//!    the routing asks each reader from the folded key alone whether it keeps
//!    the entry ([`StoredReader::keeps`]). [`route`] reads and decodes the
//!    payload once for all readers that keep it.
//! 3. Each reader unfolds a kept entry once, straight into its partner
//!    builder.
//!
//! A [`RowBuilder`] takes rows in ascending key order and errors on a key that
//! goes back. The rows of one key may arrive in any order: the packed payload
//! bytes of the index do not order like the typed values, so the builder sorts
//! and consolidates the values of a key when the key ends.
//!
//! Whether a read is a path or a range read follows from the reader's request
//! node: the join seeks its driving delta's keys in the partner batch.

use std::sync::Arc;

use crate::{
    DBData, ZWeight,
    dynamic::{DowncastTrait, Erase},
    storage::write_attribution::WriteOwner,
    trace::{
        Batch as DynBatch, BatchReader as DynBatchReader, Builder, Cursor,
        SpineSnapshot as DynamicSnapshot, with_read_owner,
    },
    typed_batch::{BatchReader, OrdIndexedZSet, SpineSnapshot, TypedBatch},
};

use super::{MergedIndexBatch, MergedIndexDefinition, ScanSession, session::split_key};

/// The unfolded key type of the index a reader serves.
type IndexKey<R> = <<R as ReaderDefinition>::Index as MergedIndexDefinition>::Key;

/// The decoded payload type of the index a reader serves.
type IndexPayload<R> = <<R as ReaderDefinition>::Index as MergedIndexDefinition>::Payload;

/// The dynamically typed batch of an [`OrdIndexedZSet`].
type Inner<K, V> = <OrdIndexedZSet<K, V> as BatchReader>::Inner;

/// How a reader's driving delta arises.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrivingDelta {
    /// The `Dlt` rows of the index stream with this id, unfolded into a batch
    /// that the reader owns.
    Stream(u8),
    /// The output of a join of other readers' rows, evaluated at routing. The
    /// reader keeps only the requests that its keys make, and no batch.
    Join,
}

/// The static plan of one reader; see the [module docs](self).
pub trait ReaderDefinition {
    /// The index that the reader reads.
    type Index: MergedIndexDefinition;

    /// The key and value of the partner batch's rows.
    type Key: DBData;
    type Val: DBData;

    /// The key and value of the driving delta's rows.
    type DeltaKey: DBData;
    type DeltaVal: DBData;

    /// The stream whose entries the partner batch keeps, or `None` if the
    /// routing builds the partner rows itself ([`Reader::push_row`]).
    const PARTNER_STREAM: Option<u8>;

    /// How many leading fields of a key name the request node: the node at
    /// which the join matches its driving delta's keys. `1` is the root
    /// domain, the group.
    const REQUEST_DEPTH: usize;

    const DRIVING_DELTA: DrivingDelta;

    /// Unfolds a partner entry, its unfolded key and decoded payload, into a
    /// row of the partner batch. Only called for a stored partner.
    fn unfold(
        _key: &<Self::Index as MergedIndexDefinition>::Key,
        _payload: &<Self::Index as MergedIndexDefinition>::Payload,
    ) -> Result<(Self::Key, Self::Val), String> {
        Err("this reader has no stored partner rows".into())
    }

    /// Unfolds an entry of the driving stream into a row of the driving delta.
    /// Only called for a [`DrivingDelta::Stream`].
    fn unfold_delta(
        _key: &<Self::Index as MergedIndexDefinition>::Key,
        _payload: &<Self::Index as MergedIndexDefinition>::Payload,
    ) -> Result<(Self::DeltaKey, Self::DeltaVal), String> {
        Err("this reader's driving delta is not a stream of the index".into())
    }
}

/// The folded-key prefixes that a driving delta requests, sorted, with a
/// pointer that only moves forward.
///
/// The questions the routing asks arrive in folded-key order, so
/// [`Self::has`] and [`Self::has_below`] advance the pointer over the
/// requests below the question and never search. They give wrong answers to
/// a question that goes back. [`Self::contains`] searches and has no such
/// restriction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Requests {
    prefixes: Vec<Vec<u8>>,
    next: usize,
}

impl Requests {
    /// Adds a request. Requests must arrive in ascending order; a repeat of
    /// the last one is ignored.
    pub fn push(&mut self, prefix: &[u8]) -> Result<(), String> {
        match self.prefixes.last().map(|last| prefix.cmp(last.as_slice())) {
            Some(std::cmp::Ordering::Equal) => {}
            Some(std::cmp::Ordering::Less) => {
                return Err(format!(
                    "request {prefix:?} goes back from {:?}",
                    self.prefixes.last()
                ));
            }
            _ => self.prefixes.push(prefix.to_vec()),
        }
        Ok(())
    }

    /// Whether `prefix` is a request. Moves forward.
    pub fn has(&mut self, prefix: &[u8]) -> bool {
        self.advance_to(prefix);
        self.prefixes
            .get(self.next)
            .is_some_and(|next| next == prefix)
    }

    /// Whether some request starts with `prefix`, such as a group prefix for
    /// requests at a deeper node. Moves forward.
    pub fn has_below(&mut self, prefix: &[u8]) -> bool {
        self.advance_to(prefix);
        self.prefixes
            .get(self.next)
            .is_some_and(|next| next.starts_with(prefix))
    }

    /// Whether `prefix` is a request, by search.
    pub fn contains(&self, prefix: &[u8]) -> bool {
        self.prefixes
            .binary_search_by(|request| request.as_slice().cmp(prefix))
            .is_ok()
    }

    /// The number of requests.
    pub fn len(&self) -> usize {
        self.prefixes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// The requests, ascending.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.prefixes.iter().map(Vec::as_slice)
    }

    fn advance_to(&mut self, prefix: &[u8]) {
        while self
            .prefixes
            .get(self.next)
            .is_some_and(|request| request.as_slice() < prefix)
        {
            self.next += 1;
        }
    }
}

/// Builds an indexed Z-set one row at a time, straight into DBSP's builder.
///
/// Keys must be nondecreasing: a key that goes back is an error. The rows of
/// one key may come in any order. They are held in a scratch vector until the
/// key ends, then sorted, consolidated (weights of equal values summed, zero
/// sums dropped), and pushed. A key whose values all cancel is dropped.
pub struct RowBuilder<K: DBData, V: DBData> {
    builder: <Inner<K, V> as DynBatch>::Builder,
    key: Option<K>,
    values: Vec<(V, ZWeight)>,
}

impl<K: DBData, V: DBData> Default for RowBuilder<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: DBData, V: DBData> RowBuilder<K, V> {
    pub fn new() -> Self {
        Self {
            builder: Builder::new_builder(&OrdIndexedZSet::<K, V>::factories()),
            key: None,
            values: Vec::new(),
        }
    }

    /// Adds one row.
    pub fn push(&mut self, key: K, value: V, weight: ZWeight) -> Result<(), String> {
        match &self.key {
            Some(last) if *last == key => {}
            Some(last) if *last > key => {
                return Err(format!(
                    "rows must arrive in key order: {key:?} comes after {last:?}"
                ));
            }
            _ => {
                self.end_key()?;
                self.key = Some(key);
            }
        }
        self.values.push((value, weight));
        Ok(())
    }

    /// Completes the batch.
    pub fn finish(mut self) -> Result<OrdIndexedZSet<K, V>, String> {
        self.end_key()?;
        Ok(TypedBatch::new(self.builder.done()))
    }

    /// Pushes the values of the key that has just ended.
    fn end_key(&mut self) -> Result<(), String> {
        let Some(mut key) = self.key.take() else {
            return Ok(());
        };
        self.values.sort_by(|left, right| left.0.cmp(&right.0));

        let mut pushed_any = false;
        let mut values = std::mem::take(&mut self.values).into_iter().peekable();
        while let Some((mut value, mut weight)) = values.next() {
            while let Some((_, next_weight)) = values.next_if(|(next, _)| *next == value) {
                weight = weight
                    .checked_add(next_weight)
                    .ok_or("reader row weight overflow")?;
            }
            if weight != 0 {
                self.builder
                    .push_val_diff_mut(value.erase_mut(), weight.erase_mut());
                pushed_any = true;
            }
        }
        if pushed_any {
            self.builder.push_key_mut(key.erase_mut());
        }
        // Reuse the scratch allocation for the next key.
        self.values.clear();
        Ok(())
    }
}

/// The delta side of a reader as [`capture`] fills it, whatever its row types.
pub trait DeltaReader<D: MergedIndexDefinition> {
    /// The id of the stream whose `Dlt` entries drive this reader, if any.
    fn driving_stream(&self) -> Option<u8>;

    /// Takes one `Dlt` entry of the driving stream: unfolds it into the
    /// driving delta and notes its request. `folded` is the entry's folded
    /// key, `key` its unfolded key.
    fn capture(
        &mut self,
        folded: &[u8],
        key: &D::Key,
        payload: &D::Payload,
        weight: ZWeight,
    ) -> Result<(), String>;

    /// Ends the capture: seals the driving delta.
    fn seal_delta(&mut self) -> Result<(), String>;
}

/// The routing side of a reader that stores entries of an index stream.
pub trait StoredReader<D: MergedIndexDefinition> {
    /// Whether the reader keeps the entry with this folded key, from the key
    /// alone. Moves forward: ask in folded-key order.
    fn keeps(&mut self, folded: &[u8]) -> bool;

    /// Unfolds a kept entry into the partner batch.
    fn push(&mut self, key: &D::Key, payload: &D::Payload, weight: ZWeight) -> Result<(), String>;
}

/// A partner batch under construction, then sealed and shared with the join.
enum PartnerRows<K: DBData, V: DBData> {
    Building(Box<RowBuilder<K, V>>),
    Sealed(Arc<Inner<K, V>>),
}

/// A driving delta under construction, then sealed and owned until the
/// driver takes it for the circuit's input handle.
enum DeltaRows<K: DBData, V: DBData> {
    Building(Box<RowBuilder<K, V>>),
    Sealed(Box<OrdIndexedZSet<K, V>>),
    Taken,
}

/// The values and weights under `key` of a sealed driving delta. These are
/// the routing's own reads of the reader's in-memory batch, so they are not
/// counted for the merged index, whose reads the session makes.
fn rows_at<K: DBData, V: DBData>(batch: &OrdIndexedZSet<K, V>, key: &K) -> Vec<(V, ZWeight)> {
    with_read_owner(WriteOwner::Query, || {
        let mut cursor = batch.inner().cursor();
        let mut rows = Vec::new();
        if cursor.seek_key_exact(key.erase(), None) {
            while cursor.val_valid() {
                rows.push((
                    cursor.val().downcast_checked::<V>().clone(),
                    *cursor.weight().downcast_checked::<ZWeight>(),
                ));
                cursor.step_val();
            }
        }
        rows
    })
}

/// One partner read: its requests, its driving delta, and its partner batch.
pub struct Reader<R: ReaderDefinition> {
    /// Byte length of a request prefix: the first `REQUEST_DEPTH` fields of a
    /// key, each a tag and a value.
    request_width: usize,
    /// The same, measured on a key of the driving stream.
    delta_width: usize,

    requests: Requests,
    partner: PartnerRows<R::Key, R::Val>,
    /// `None` for a driving delta that is a join.
    delta: Option<DeltaRows<R::DeltaKey, R::DeltaVal>>,
}

impl<R: ReaderDefinition> Reader<R> {
    /// An empty reader. Checks the definition against the index's trie: the
    /// streams exist, the request node is on their paths, and a stored
    /// partner and a driving stream share the fields down to the request node.
    pub fn new() -> Result<Self, String> {
        let trie = &<R::Index as MergedIndexDefinition>::TRIE;
        let partner_width = R::PARTNER_STREAM
            .map(|stream| trie.prefix_width(stream, R::REQUEST_DEPTH))
            .transpose()?;
        let delta_width = match R::DRIVING_DELTA {
            DrivingDelta::Stream(stream) => Some(trie.prefix_width(stream, R::REQUEST_DEPTH)?),
            DrivingDelta::Join => None,
        };
        if let (Some(partner_stream), DrivingDelta::Stream(driving_stream)) =
            (R::PARTNER_STREAM, R::DRIVING_DELTA)
        {
            let fields = |stream| {
                trie.stream_path(stream)
                    .map(|path| path[..R::REQUEST_DEPTH].to_vec())
            };
            if fields(partner_stream) != fields(driving_stream) {
                return Err(format!(
                    "streams {partner_stream} and {driving_stream} differ above the request node"
                ));
            }
        }
        let request_width = partner_width.or(delta_width).ok_or(
            "a reader needs a partner stream or a driving stream to know its request width",
        )?;
        Ok(Self {
            request_width,
            delta_width: delta_width.unwrap_or(request_width),
            requests: Requests::default(),
            partner: PartnerRows::Building(Box::default()),
            delta: matches!(R::DRIVING_DELTA, DrivingDelta::Stream(_))
                .then(|| DeltaRows::Building(Box::default())),
        })
    }

    /// The requests of the driving delta.
    pub fn requests(&self) -> &Requests {
        &self.requests
    }

    /// The requests, for the routing's forward questions
    /// ([`Requests::has`], [`Requests::has_below`]).
    pub fn requests_mut(&mut self) -> &mut Requests {
        &mut self.requests
    }

    /// Adds a request that the routing computed, for a [`DrivingDelta::Join`].
    pub fn request(&mut self, prefix: &[u8]) -> Result<(), String> {
        self.requests.push(prefix)
    }

    /// The length of a request prefix, in bytes.
    pub fn request_width(&self) -> usize {
        self.request_width
    }

    /// Adds a partner row that the routing built itself: for a reader with no
    /// stored partner stream, or an extra row for a stored one.
    pub fn push_row(&mut self, key: R::Key, value: R::Val, weight: ZWeight) -> Result<(), String> {
        match &mut self.partner {
            PartnerRows::Building(builder) => builder.push(key, value, weight),
            PartnerRows::Sealed(_) => Err("the partner batch is already sealed".into()),
        }
    }

    /// Seals the partner batch, once the routing is done.
    pub fn seal_partner(&mut self) -> Result<(), String> {
        if let PartnerRows::Building(builder) = &mut self.partner {
            let builder = std::mem::take(builder);
            self.partner = PartnerRows::Sealed(Arc::new((*builder).finish()?.into_inner()));
        }
        Ok(())
    }

    /// The partner batch, as the one-batch snapshot that a join reads.
    pub fn partner(&self) -> Result<SpineSnapshot<OrdIndexedZSet<R::Key, R::Val>>, String> {
        let PartnerRows::Sealed(batch) = &self.partner else {
            return Err("the partner batch is not sealed yet".into());
        };
        let mut snapshot = TypedBatch::new(DynamicSnapshot::new(
            OrdIndexedZSet::<R::Key, R::Val>::factories(),
        ));
        snapshot
            .inner_mut()
            .extend_with_batches([Arc::clone(batch)]);
        Ok(snapshot)
    }

    /// The rows of the driving delta under `key`: values with their weights.
    /// For the routing, which needs the delta's rows at the node it passes.
    pub fn delta_rows_at(&self, key: &R::DeltaKey) -> Result<Vec<(R::DeltaVal, ZWeight)>, String> {
        match &self.delta {
            Some(DeltaRows::Sealed(batch)) => Ok(rows_at(batch, key)),
            Some(DeltaRows::Building(_)) => Err("the driving delta is not sealed yet".into()),
            Some(DeltaRows::Taken) => Err("the driving delta was already taken".into()),
            None => Err("this reader's driving delta is a join, which holds no batch".into()),
        }
    }

    /// Moves the driving delta out for the circuit's input handle. Nothing is
    /// cloned: no other holder shares the batch.
    pub fn take_delta(&mut self) -> Result<OrdIndexedZSet<R::DeltaKey, R::DeltaVal>, String> {
        match self.delta.replace(DeltaRows::Taken) {
            Some(DeltaRows::Sealed(batch)) => Ok(*batch),
            Some(DeltaRows::Building(_)) => Err("the driving delta is not sealed yet".into()),
            Some(DeltaRows::Taken) => Err("the driving delta was already taken".into()),
            None => {
                self.delta = None;
                Err("this reader's driving delta is a join, which holds no batch".into())
            }
        }
    }
}

impl<R: ReaderDefinition> DeltaReader<R::Index> for Reader<R> {
    fn driving_stream(&self) -> Option<u8> {
        match R::DRIVING_DELTA {
            DrivingDelta::Stream(stream) => Some(stream),
            DrivingDelta::Join => None,
        }
    }

    fn capture(
        &mut self,
        folded: &[u8],
        key: &IndexKey<R>,
        payload: &IndexPayload<R>,
        weight: ZWeight,
    ) -> Result<(), String> {
        let prefix = folded
            .get(..self.delta_width)
            .ok_or_else(|| format!("delta key {folded:?} is shorter than its request prefix"))?;
        self.requests.push(prefix)?;
        let (delta_key, delta_value) = R::unfold_delta(key, payload)?;
        match &mut self.delta {
            Some(DeltaRows::Building(builder)) => builder.push(delta_key, delta_value, weight),
            _ => Err("the driving delta is not being built".into()),
        }
    }

    fn seal_delta(&mut self) -> Result<(), String> {
        if let Some(DeltaRows::Building(builder)) = &mut self.delta {
            let builder = std::mem::take(builder);
            self.delta = Some(DeltaRows::Sealed(Box::new((*builder).finish()?)));
        }
        Ok(())
    }
}

impl<R: ReaderDefinition> StoredReader<R::Index> for Reader<R> {
    fn keeps(&mut self, folded: &[u8]) -> bool {
        let Some(stream) = R::PARTNER_STREAM else {
            return false;
        };
        folded.last() == Some(&stream)
            && folded
                .get(..self.request_width)
                .is_some_and(|prefix| self.requests.has(prefix))
    }

    fn push(
        &mut self,
        key: &IndexKey<R>,
        payload: &IndexPayload<R>,
        weight: ZWeight,
    ) -> Result<(), String> {
        let (row_key, row_value) = R::unfold(key, payload)?;
        self.push_row(row_key, row_value, weight)
    }
}

/// What [`capture`] learned from `Dlt`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Captured {
    /// The touched groups: the group prefix of each `Dlt` key, ascending.
    pub touched: Vec<Vec<u8>>,
    /// The payloads of `Dlt` decoded: one per delta entry of a driving stream,
    /// however many readers it drives.
    pub payload_decodes_dlt: u64,
}

/// Phase A: reads `Dlt` once, in folded-key order. Lists the touched groups,
/// and hands each entry of a stream that some reader's driving delta takes to
/// those readers. The entry is unfolded and decoded once, however many
/// readers take it. Seals the readers' driving deltas.
pub fn capture<D: MergedIndexDefinition>(
    dlt: &MergedIndexBatch,
    readers: &mut [&mut dyn DeltaReader<D>],
) -> Result<Captured, String> {
    let group_width = 1 + D::TRIE.root_width();
    let mut captured = Captured::default();
    let mut cursor = dlt.cursor();
    while cursor.key_valid() {
        let folded = cursor.key().downcast_checked::<Vec<u8>>();
        let group = folded
            .get(..group_width)
            .ok_or_else(|| format!("delta key {folded:?} is shorter than a group prefix"))?;
        if captured.touched.last().map(Vec::as_slice) != Some(group) {
            captured.touched.push(group.to_vec());
        }
        let (_, stream) = split_key(folded)?;
        if readers
            .iter()
            .any(|reader| reader.driving_stream() == Some(stream))
        {
            let folded = folded.clone();
            let key = D::unfold(&folded)?;
            while cursor.val_valid() {
                let weight = *cursor.weight().downcast_checked::<ZWeight>();
                let payload =
                    D::decode_payload(stream, cursor.val().downcast_checked::<Vec<u8>>())?;
                captured.payload_decodes_dlt += 1;
                for reader in readers
                    .iter_mut()
                    .filter(|reader| reader.driving_stream() == Some(stream))
                {
                    reader.capture(&folded, &key, &payload, weight)?;
                }
                cursor.step_val();
            }
        }
        cursor.step_key();
    }
    for reader in readers {
        reader.seal_delta()?;
    }
    Ok(captured)
}

/// One position of `Old`, whose payloads are read and decoded at most once,
/// however many parties ask for them.
pub struct Position<'s, 'a, D: MergedIndexDefinition> {
    session: &'s mut ScanSession<'a, D>,
    decoded: Option<Vec<(D::Payload, ZWeight)>>,
}

impl<'s, 'a, D: MergedIndexDefinition> Position<'s, 'a, D> {
    /// The session's current position.
    pub fn at(session: &'s mut ScanSession<'a, D>) -> Self {
        Self {
            session,
            decoded: None,
        }
    }

    /// The folded key at the position, while it lies in the group.
    pub fn key(&self) -> Option<&[u8]> {
        self.session.key()
    }

    /// The position's decoded payloads with their summed weights. The first
    /// call reads them from the index (and counts the decode); later calls
    /// return the same.
    pub fn decoded(&mut self) -> Result<&[(D::Payload, ZWeight)], String> {
        if self.decoded.is_none() {
            self.decoded = Some(self.session.take_and_decode()?);
        }
        Ok(self.decoded.as_deref().unwrap_or_default())
    }
}

/// Phase B for stored partners: offers the position to each reader, by its
/// folded key alone. If some keep it, reads and decodes the payload once,
/// unfolds the key once, and gives each keeper the entry.
pub fn route<D: MergedIndexDefinition>(
    position: &mut Position<'_, '_, D>,
    readers: &mut [&mut dyn StoredReader<D>],
) -> Result<(), String> {
    assert!(readers.len() <= 64, "at most 64 readers are routed at once");
    let folded = position.key().ok_or("route outside a group's range")?;
    let mut keepers = 0_u64;
    for (index, reader) in readers.iter_mut().enumerate() {
        if reader.keeps(folded) {
            keepers |= 1 << index;
        }
    }
    if keepers == 0 {
        return Ok(());
    }
    let key = D::unfold(folded)?;
    let decoded = position.decoded()?;
    for (index, reader) in readers.iter_mut().enumerate() {
        if keepers & (1 << index) != 0 {
            for (payload, weight) in decoded {
                reader.push(&key, payload, *weight)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
