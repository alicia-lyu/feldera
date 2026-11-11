//! Query-specific partner reads for the Q3 view circuit.
//!
//! The view is `V = L ⋈ (O ⋈ C)`. The caller maintains the secondary index,
//! publishes the transaction's `Old` (the snapshot before it) and `Dlt` (its
//! signed index changes), and [`Q3State::collect`] runs one scan session over
//! them. The session buffers each touched customer's group once; the groups
//! stay in [`Q3State`] until [`Q3State::finish`].
//!
//! The four partner reads of the delta rules are logical: each is a
//! description (before or after the transaction, a path or a range, and its
//! streams) that reads through a [`VirtualBuffer`], a borrowing view of one
//! group's buffers at one node. A before read sees `Old`; an after read sees
//! `Old` followed by `Dlt`, unconsolidated (`prop:overlay`). The readers never
//! touch the index, and a request outside the touched groups breaks the
//! contract that every driving-delta key lies in a group that `Dlt` touches.
//!
//! The values that cross the DBSP boundary are copied twice: the driving
//! deltas (the buffers' `Dlt` halves) into the circuit's input handles, and
//! each reader's partner vector into the join. The scan session of roadmap
//! step 5 removes both copies.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::AddAssign,
    sync::{Arc, Mutex, OnceLock},
};

use dbsp::{
    circuit::{splitter_output_chunk_size, RootCircuit},
    dynamic::DowncastTrait,
    storage::write_attribution::WriteOwner,
    trace::{
        merged_index::{
            Entry, GroupBuffers, MergedIndexBatch, MergedIndexDefinition, Pass, ScanSession,
            SessionCounts,
        },
        with_read_owner, BatchReader as DynamicBatchReader, Cursor, RunReads,
        SpineSnapshot as DynamicSnapshot,
    },
    typed_batch::{BatchReader, SpineSnapshot, TypedBatch},
    utils::{Tup1, Tup2, Tup3, Tup4, Tup6},
    DBData, OrdIndexedZSet, OrdZSet, Stream, ZSetHandle,
};
use feldera_sqllib::{Date, SqlDecimal, SqlString};

use super::{
    prefix_custkey, prefix_custkey_orderkey_then_linenumber, CustomerByCustkeyKey,
    CustomerOrdersLineitemDefinition, Key, LineitemByCustkeyOrderkeyLinenumberKey,
    OrdersByCustkeyOrderkeyKey, Payload, INDEX_ID_CUSTOMER_BY_CUSTKEY,
    INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER, INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
};

// A *vector* is DBSP's unit of pipelining: a weighted, key-sorted set of rows
// that flows between operators. It is the counterpart of an X100 vector, not
// of a Volcano row.

/// The customers, indexed by `custkey`.
pub type CustomerByCustkeyVector = OrdIndexedZSet<Tup1<i64>, Tup1<SqlString>>;
/// The orders, indexed by their customer: `(orderkey, orderdate, shippriority)`.
pub type OrdersByCustkeyVector = OrdIndexedZSet<Tup1<i64>, Tup3<i64, Date, i32>>;
/// The joined orders `J`, indexed by `(custkey, orderkey)`, the node at which
/// `V` reads them: `(mktsegment, orderdate, shippriority)`.
pub type JoinedByCustkeyOrderkeyVector = OrdIndexedZSet<Tup2<i64, i64>, Tup3<SqlString, Date, i32>>;
/// The line items, indexed by `(custkey, orderkey)`.
pub type LineitemByCustkeyOrderkeyVector =
    OrdIndexedZSet<Tup2<i64, i64>, Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>;

/// A row of the view circuit's customer input: `(custkey, mktsegment)`.
pub type CustomerRow = Tup2<i64, SqlString>;
/// A row of the view circuit's order input: `(custkey, orderkey, orderdate, shippriority)`.
pub type OrderRow = Tup4<i64, i64, Date, i32>;
/// A row of the view circuit's line-item input: `(custkey, orderkey, linenumber, extendedprice,
/// discount, shipdate)`.
pub type LineitemRow = Tup6<i64, i64, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

pub type SourceSnapshot = DynamicSnapshot<MergedIndexBatch>;

/// The streams the session collects: all three.
const STREAMS: [u8; 3] = [
    INDEX_ID_CUSTOMER_BY_CUSTKEY,
    INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
    INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
];

/// What one transaction's view maintenance read from the index: the scan
/// session's moves, and the run reads they cost (all under the merged index's
/// read owner, on the thread that collected).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionReads {
    pub counts: SessionCounts,
    pub run_reads: RunReads,
}

impl AddAssign for SessionReads {
    fn add_assign(&mut self, other: Self) {
        self.counts += other.counts;
        self.run_reads += other.run_reads;
    }
}

/// The touched groups of one transaction, by group prefix (the root domain's
/// tag and value), and what collecting them read.
struct CollectedGroups {
    groups: BTreeMap<Vec<u8>, GroupBuffers<Payload>>,
    reads: SessionReads,
}

impl CollectedGroups {
    /// The buffers of customer `custkey`'s group.
    fn group(&self, custkey: i64) -> Result<&GroupBuffers<Payload>, String> {
        self.groups.get(&prefix_custkey(custkey)).ok_or_else(|| {
            format!("customer {custkey} is outside the touched groups: Dlt has no entry for it")
        })
    }
}

struct ReadViews {
    old: SourceSnapshot,
    dlt: Arc<MergedIndexBatch>,
    /// Set once by [`Q3State::collect`].
    collected: OnceLock<CollectedGroups>,
}

/// One externally prepared transaction's index views and groups, shared by the
/// view circuit's joins.
///
/// Publish `Old`, the index snapshot before the transaction, and `Dlt`, the
/// run of the transaction's signed index changes, after index maintenance and
/// before the view transaction. [`Self::collect`] then buffers the touched
/// groups; the joins' readers and [`Self::push_driving_deltas`] use them. Keep
/// the publication unchanged until `DBSPHandle::transaction` or the equivalent
/// multi-step commit has completed, then call [`Self::finish`]: the groups are
/// held until then, since `J` and `V` are separate operators and `V` flushes
/// only after `J` has passed every customer. The caller must restore the
/// index, base state, and the view circuit at the same completed transaction.
#[derive(Clone, Default)]
pub struct Q3State {
    views: Arc<Mutex<Option<Arc<ReadViews>>>>,
}

impl Q3State {
    pub fn publish(&self, old: SourceSnapshot, dlt: Arc<MergedIndexBatch>) -> Result<(), String> {
        let mut views = self.views.lock().map_err(|_| "Q3 state lock poisoned")?;
        if views.is_some() {
            return Err("finish the previous Q3 transaction before publishing another".into());
        }
        *views = Some(Arc::new(ReadViews {
            old,
            dlt,
            collected: OnceLock::new(),
        }));
        Ok(())
    }

    /// Runs the scan session over the published `Old` and `Dlt` and keeps the
    /// touched groups until [`Self::finish`]. The index is read here, on the
    /// calling thread under the merged index's read owner, and nowhere else;
    /// the caller binds the runtime's buffer cache to this thread.
    pub fn collect(&self, pass: Pass) -> Result<(), String> {
        let views = self.views()?;
        let before = RunReads::current_for(WriteOwner::MergedIndex);
        let mut groups = BTreeMap::new();
        let counts = with_read_owner(WriteOwner::MergedIndex, || {
            let mut session =
                ScanSession::<CustomerOrdersLineitemDefinition>::new(&views.old, &views.dlt, pass);
            session.maintain(&BTreeSet::from(STREAMS), None, |group, buffers| {
                groups.insert(group.to_vec(), buffers);
                Ok(())
            })?;
            Ok::<_, String>(session.counts())
        })?;
        let run_reads = RunReads::current_for(WriteOwner::MergedIndex).since(before);
        let collected = CollectedGroups {
            groups,
            reads: SessionReads { counts, run_reads },
        };
        views
            .collected
            .set(collected)
            .map_err(|_| "the published views were already collected".to_owned())
    }

    /// Release the groups and published handles after all transaction
    /// consumers finish, and return what collecting them read from the index
    /// (nothing, if the transaction never collected).
    pub fn finish(&self) -> Result<SessionReads, String> {
        let mut views = self.views.lock().map_err(|_| "Q3 state lock poisoned")?;
        let views = views
            .take()
            .ok_or("no Q3 transaction views were published")?;
        Ok(views
            .collected
            .get()
            .map_or_else(SessionReads::default, |collected| collected.reads))
    }

    /// The published `Old` snapshot.
    pub fn old(&self) -> Result<SourceSnapshot, String> {
        Ok(self.views()?.old.clone())
    }

    /// The published `Dlt` run.
    pub fn dlt(&self) -> Result<Arc<MergedIndexBatch>, String> {
        Ok(self.views()?.dlt.clone())
    }

    fn views(&self) -> Result<Arc<ReadViews>, String> {
        self.views
            .lock()
            .map_err(|_| "Q3 state lock poisoned")?
            .clone()
            .ok_or_else(|| "publish merged-index views before Q3 maintenance".into())
    }

    /// Pushes the buffers' `Dlt` halves into the view circuit's input handles:
    /// the driving deltas. Every row carries its trie keys. This is the first
    /// of the two copies at the DBSP boundary.
    pub fn push_driving_deltas(
        &self,
        customers: &ZSetHandle<CustomerRow>,
        orders: &ZSetHandle<OrderRow>,
        lines: &ZSetHandle<LineitemRow>,
    ) -> Result<(), String> {
        self.push_customer_deltas(customers)?;
        self.push_order_deltas(orders)?;
        self.push_line_deltas(lines)
    }

    /// Pushes the `Dlt` entries of the Customer stream; see
    /// [`Self::push_driving_deltas`].
    pub fn push_customer_deltas(&self, customers: &ZSetHandle<CustomerRow>) -> Result<(), String> {
        let views = self.views()?;
        for buffers in views.collected_groups()?.groups.values() {
            for entry in buffers.dlt(INDEX_ID_CUSTOMER_BY_CUSTKEY) {
                let key = customer_key(entry)?;
                let segment = customer_segment(entry)?;
                customers.push(Tup2(key.custkey, segment.clone()), entry.weight);
            }
        }
        Ok(())
    }

    /// Pushes the `Dlt` entries of the Orders stream; see
    /// [`Self::push_driving_deltas`].
    pub fn push_order_deltas(&self, orders: &ZSetHandle<OrderRow>) -> Result<(), String> {
        let views = self.views()?;
        for buffers in views.collected_groups()?.groups.values() {
            for entry in buffers.dlt(INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY) {
                let key = order_key(entry)?;
                let (date, priority) = order_payload(entry)?;
                orders.push(
                    Tup4(key.custkey, key.orderkey, date, priority),
                    entry.weight,
                );
            }
        }
        Ok(())
    }

    /// Pushes the `Dlt` entries of the Lineitem stream; see
    /// [`Self::push_driving_deltas`].
    pub fn push_line_deltas(&self, lines: &ZSetHandle<LineitemRow>) -> Result<(), String> {
        let views = self.views()?;
        for buffers in views.collected_groups()?.groups.values() {
            for entry in buffers.dlt(INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER) {
                let key = line_key(entry)?;
                let (price, discount, ship_date) = line_payload(entry)?;
                lines.push(
                    Tup6(
                        key.custkey,
                        key.orderkey,
                        key.linenumber,
                        price,
                        discount,
                        ship_date,
                    ),
                    entry.weight,
                );
            }
        }
        Ok(())
    }

    /// `C⁺`: the customers of the requested customers after the transaction.
    /// A path read of the Customer stream, driven by `ΔO`.
    pub fn customers_after(
        &self,
        delta: &SpineSnapshot<OrdersByCustkeyVector>,
    ) -> Result<SpineSnapshot<CustomerByCustkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_groups()?;
        let mut output = RequestedRows::new();
        for Tup1(customer) in requested_keys(delta) {
            let buffer = VirtualBuffer::new(collected.group(customer)?, Version::After);
            let node = fold_customer(customer);
            for entry in buffer.entries(INDEX_ID_CUSTOMER_BY_CUSTKEY, Node::Path(&node)) {
                output.push(
                    Tup1(customer),
                    Tup1(customer_segment(entry)?.clone()),
                    entry.weight,
                );
            }
        }
        Ok(output.finish())
    }

    /// `O⁻`: the orders of the requested customers before the transaction. A
    /// range read of the Orders stream below the customer, driven by `ΔC`.
    pub fn orders_before(
        &self,
        delta: &SpineSnapshot<CustomerByCustkeyVector>,
    ) -> Result<SpineSnapshot<OrdersByCustkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_groups()?;
        let mut output = RequestedRows::new();
        for Tup1(customer) in requested_keys(delta) {
            let buffer = VirtualBuffer::new(collected.group(customer)?, Version::Before);
            let below_customer = prefix_custkey(customer);
            for entry in buffer.entries(
                INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
                Node::Range(&below_customer),
            ) {
                let key = order_key(entry)?;
                let (date, priority) = order_payload(entry)?;
                output.push(
                    Tup1(customer),
                    Tup3(key.orderkey, date, priority),
                    entry.weight,
                );
            }
        }
        Ok(output.finish())
    }

    /// `J⁺`: the joined orders at the requested `(custkey, orderkey)` nodes
    /// after the transaction. A path read of the Customer and Orders streams,
    /// driven by `ΔL`. A joined row's weight is the product of its two
    /// entries' weights.
    pub fn joined_orders_after(
        &self,
        delta: &SpineSnapshot<LineitemByCustkeyOrderkeyVector>,
    ) -> Result<SpineSnapshot<JoinedByCustkeyOrderkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_groups()?;
        let mut output = RequestedRows::new();
        for Tup2(customer, order) in requested_keys(delta) {
            let buffer = VirtualBuffer::new(collected.group(customer)?, Version::After);
            let customer_node = fold_customer(customer);
            let order_node = fold_order(customer, order);
            for customer_entry in
                buffer.entries(INDEX_ID_CUSTOMER_BY_CUSTKEY, Node::Path(&customer_node))
            {
                let segment = customer_segment(customer_entry)?;
                for order_entry in
                    buffer.entries(INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY, Node::Path(&order_node))
                {
                    let (date, priority) = order_payload(order_entry)?;
                    let weight = order_entry
                        .weight
                        .checked_mul(customer_entry.weight)
                        .ok_or("Q3 reconstructed join weight overflow")?;
                    output.push(
                        Tup2(customer, order),
                        Tup3(segment.clone(), date, priority),
                        weight,
                    );
                }
            }
        }
        Ok(output.finish())
    }

    /// `L⁻`: the line items of the requested `(custkey, orderkey)` nodes
    /// before the transaction. A range read of the Lineitem stream below the
    /// order, driven by `ΔJ`.
    pub fn lineitems_before(
        &self,
        delta: &SpineSnapshot<JoinedByCustkeyOrderkeyVector>,
    ) -> Result<SpineSnapshot<LineitemByCustkeyOrderkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_groups()?;
        let mut output = RequestedRows::new();
        for Tup2(customer, order) in requested_keys(delta) {
            let buffer = VirtualBuffer::new(collected.group(customer)?, Version::Before);
            let below_order = prefix_custkey_orderkey_then_linenumber(customer, order);
            for entry in buffer.entries(
                INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
                Node::Range(&below_order),
            ) {
                let key = line_key(entry)?;
                let (price, discount, ship_date) = line_payload(entry)?;
                output.push(
                    Tup2(customer, order),
                    Tup4(key.linenumber, price, discount, ship_date),
                    entry.weight,
                );
            }
        }
        Ok(output.finish())
    }
}

impl ReadViews {
    fn collected_groups(&self) -> Result<&CollectedGroups, String> {
        self.collected
            .get()
            .ok_or_else(|| "collect the touched groups before reading them".to_owned())
    }
}

/// Which state of the index a read sees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Version {
    /// `Old`, the index before the transaction.
    Before,
    /// `Old ⊎ Dlt`, the index after it, unconsolidated.
    After,
}

/// The entries of a stream that a read takes at a node of the trie.
#[derive(Clone, Copy, Debug)]
enum Node<'k> {
    /// The entries whose folded key is exactly this one.
    Path(&'k [u8]),
    /// The entries whose folded key starts with this prefix: the subtree.
    Range(&'k [u8]),
}

impl Node<'_> {
    /// The sub-slice of `entries`, which are in key order, that this node takes.
    fn select<P>(self, entries: &[Entry<P>]) -> &[Entry<P>] {
        let (Node::Path(bytes) | Node::Range(bytes)) = self;
        let start = entries.partition_point(|entry| entry.key.as_slice() < bytes);
        let taken = entries[start..]
            .iter()
            .take_while(|entry| match self {
                Node::Path(key) => entry.key == key,
                Node::Range(prefix) => entry.key.starts_with(prefix),
            })
            .count();
        &entries[start..start + taken]
    }
}

/// A borrowing view of one group's buffers: what a read of a given version
/// sees. Nothing is copied and nothing is consolidated; a join is bilinear,
/// so it can take `Old`'s entries and `Dlt`'s as they are (`prop:overlay`).
struct VirtualBuffer<'g> {
    group: &'g GroupBuffers<Payload>,
    version: Version,
}

impl<'g> VirtualBuffer<'g> {
    fn new(group: &'g GroupBuffers<Payload>, version: Version) -> Self {
        Self { group, version }
    }

    /// The entries of `stream` at `node`: `Old`'s, then, for an after read,
    /// `Dlt`'s.
    fn entries(&self, stream: u8, node: Node<'_>) -> impl Iterator<Item = &'g Entry<Payload>> + 'g {
        let old = node.select(self.group.old(stream));
        let dlt = match self.version {
            Version::Before => &[][..],
            Version::After => node.select(self.group.dlt(stream)),
        };
        old.iter().chain(dlt)
    }
}

fn fold_customer(custkey: i64) -> Vec<u8> {
    CustomerOrdersLineitemDefinition::fold(&Key::CustomerByCustkey(CustomerByCustkeyKey {
        custkey,
    }))
    .as_bytes()
    .to_vec()
}

fn fold_order(custkey: i64, orderkey: i64) -> Vec<u8> {
    CustomerOrdersLineitemDefinition::fold(&Key::OrdersByCustkeyOrderkey(
        OrdersByCustkeyOrderkeyKey { custkey, orderkey },
    ))
    .as_bytes()
    .to_vec()
}

fn customer_key(entry: &Entry<Payload>) -> Result<CustomerByCustkeyKey, String> {
    match CustomerOrdersLineitemDefinition::unfold(&entry.key)? {
        Key::CustomerByCustkey(key) => Ok(key),
        other => Err(format!("expected a Customer key, found {other:?}")),
    }
}

fn order_key(entry: &Entry<Payload>) -> Result<OrdersByCustkeyOrderkeyKey, String> {
    match CustomerOrdersLineitemDefinition::unfold(&entry.key)? {
        Key::OrdersByCustkeyOrderkey(key) => Ok(key),
        other => Err(format!("expected an Orders key, found {other:?}")),
    }
}

fn line_key(entry: &Entry<Payload>) -> Result<LineitemByCustkeyOrderkeyLinenumberKey, String> {
    match CustomerOrdersLineitemDefinition::unfold(&entry.key)? {
        Key::LineitemByCustkeyOrderkeyLinenumber(key) => Ok(key),
        other => Err(format!("expected a Lineitem key, found {other:?}")),
    }
}

fn customer_segment(entry: &Entry<Payload>) -> Result<&SqlString, String> {
    match &entry.payload {
        Payload::CustomerByCustkey(payload) => Ok(&payload.c_mktsegment),
        other => Err(format!("expected a Customer payload, found {other:?}")),
    }
}

fn order_payload(entry: &Entry<Payload>) -> Result<(Date, i32), String> {
    match &entry.payload {
        Payload::OrdersByCustkeyOrderkey(payload) => {
            Ok((payload.o_orderdate, payload.o_shippriority))
        }
        other => Err(format!("expected an Orders payload, found {other:?}")),
    }
}

fn line_payload(
    entry: &Entry<Payload>,
) -> Result<(SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date), String> {
    match &entry.payload {
        Payload::LineitemByCustkeyOrderkeyLinenumber(payload) => Ok((
            payload.l_extendedprice,
            payload.l_discount,
            payload.l_shipdate,
        )),
        other => Err(format!("expected a Lineitem payload, found {other:?}")),
    }
}

/// The keys of a driving delta, in order.
fn requested_keys<K: DBData, V: DBData>(delta: &SpineSnapshot<OrdIndexedZSet<K, V>>) -> Vec<K> {
    let mut cursor = delta.inner().cursor();
    let mut keys = Vec::new();
    while cursor.key_valid() {
        keys.push(cursor.key().downcast_checked::<K>().clone());
        cursor.step_key();
    }
    keys
}

/// Bounded sort chunks, combined through the native consolidated snapshot cursor.
/// The snapshot still owns all result chunks; this is not a hard memory bound.
/// This is the second of the two copies at the DBSP boundary: the reader's
/// partner vector.
struct RequestedRows<K: DBData, V: DBData> {
    pending: Vec<Tup2<Tup2<K, V>, i64>>,
    snapshot: SpineSnapshot<OrdIndexedZSet<K, V>>,
}

impl<K: DBData, V: DBData> RequestedRows<K, V> {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            snapshot: TypedBatch::new(DynamicSnapshot::new(OrdIndexedZSet::<K, V>::factories())),
        }
    }

    fn push(&mut self, key: K, value: V, weight: i64) {
        if weight != 0 {
            self.pending.push(Tup2(Tup2(key, value), weight));
            if self.pending.len() >= splitter_output_chunk_size() {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let batch = OrdIndexedZSet::<K, V>::from_tuples((), std::mem::take(&mut self.pending));
            self.snapshot
                .inner_mut()
                .extend_with_batches([Arc::new(batch.into_inner())]);
        }
    }

    fn finish(mut self) -> SpineSnapshot<OrdIndexedZSet<K, V>> {
        self.flush();
        self.snapshot
    }
}

/// `J = O ⋈ C` on `custkey`, with both historical operands read from the
/// collected groups. `J`'s rows are indexed by `(custkey, orderkey)`, the node
/// at which `V` reads them.
pub fn join_orders_customers<F, It>(
    orders: &Stream<RootCircuit, OrdersByCustkeyVector>,
    customers: &Stream<RootCircuit, CustomerByCustkeyVector>,
    state: Q3State,
    join: F,
) -> Stream<RootCircuit, JoinedByCustkeyOrderkeyVector>
where
    F: Fn(&Tup1<i64>, &Tup3<i64, Date, i32>, &Tup1<SqlString>) -> It + Clone + 'static,
    It: IntoIterator<Item = (Tup2<i64, i64>, Tup3<SqlString, Date, i32>)> + 'static,
{
    let before = state.clone();
    orders.join_index_with_snapshot_readers(
        customers,
        move |delta| {
            state
                .customers_after(delta)
                .expect("Q3 C+ reconstruction failed")
        },
        move |delta| {
            before
                .orders_before(delta)
                .expect("Q3 O- reconstruction failed")
        },
        join,
    )
}

/// `V = L ⋈ J` on `(custkey, orderkey)`, preserving the native delta formula.
pub fn join_lineitems_orders<F, OV>(
    lines: &Stream<RootCircuit, LineitemByCustkeyOrderkeyVector>,
    orders: &Stream<RootCircuit, JoinedByCustkeyOrderkeyVector>,
    state: Q3State,
    join: F,
) -> Stream<RootCircuit, OrdZSet<OV>>
where
    OV: DBData,
    F: Fn(
            &Tup2<i64, i64>,
            &Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>,
            &Tup3<SqlString, Date, i32>,
        ) -> OV
        + Clone
        + 'static,
{
    let before = state.clone();
    lines.join_with_snapshot_readers(
        orders,
        move |delta| {
            state
                .joined_orders_after(delta)
                .expect("Q3 J+ reconstruction failed")
        },
        move |delta| {
            before
                .lineitems_before(delta)
                .expect("Q3 L- reconstruction failed")
        },
        join,
    )
}

#[cfg(test)]
mod tests;
