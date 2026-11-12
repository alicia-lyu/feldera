//! Query-specific partner reads for the Q3 view circuit.
//!
//! The view is `V = L ⋈ (O ⋈ C)`. The caller maintains the secondary index,
//! publishes the transaction's `Old` (the snapshot before it) and `Dlt` (its
//! signed index changes), and [`Q3State::collect`] runs one scan session over
//! them. The session routes each entry of a touched customer's group to the
//! [`Reader`]s of its stream, which project it once into the joins' typed
//! rows; the groups' raw buffers are dropped as they are routed.
//!
//! The four partner reads of the delta rules are the readers' batches: `C⁺`
//! (customers, after), `O⁻` (orders by customer, before), and `L⁻` (line items,
//! before) hand their batches to the joins as they are. `J⁺` is `J`'s join
//! re-evaluated on the path of each requested `(custkey, orderkey)`, from the
//! customers' and the orders-by-order readers' after reads; its rows are the
//! one copy left at the DBSP boundary. The driving deltas are the readers'
//! `Dlt` batches ([`Q3State::customer_deltas`] and its siblings). A request outside the touched
//! groups breaks the contract that every driving-delta key lies in a group
//! that `Dlt` touches, and is an error.

use std::{
    ops::AddAssign,
    sync::{Arc, Mutex, OnceLock},
};

use dbsp::{
    circuit::{splitter_output_chunk_size, RootCircuit},
    dynamic::{DowncastTrait, Erase},
    storage::write_attribution::WriteOwner,
    trace::{
        merged_index::{
            reader::collect, FilledReader, MergedIndexBatch, Pass, Reader, ReaderRuns, ScanSession,
            SessionCounts, TouchedGroups,
        },
        with_read_owner, BatchReader as DynamicBatchReader, Cursor, RunReads,
        SpineSnapshot as DynamicSnapshot,
    },
    typed_batch::{BatchReader, SpineSnapshot, TypedBatch},
    utils::{Tup1, Tup2, Tup3, Tup4},
    DBData, OrdIndexedZSet, OrdZSet, Stream, ZWeight,
};
use feldera_sqllib::{Date, SqlDecimal, SqlString};

use super::{
    prefix_custkey, CustomerOrdersLineitemDefinition, Key, Payload, INDEX_ID_CUSTOMER_BY_CUSTKEY,
    INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER, INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
};

// A *vector* is DBSP's unit of pipelining: a weighted, key-sorted set of rows
// that flows between operators. It is the counterpart of an X100 vector, not
// of a Volcano row.

/// The customers, indexed by `custkey`.
pub type CustomerByCustkeyVector = OrdIndexedZSet<Tup1<i64>, Tup1<SqlString>>;
/// The orders, indexed by their customer: `(orderkey, orderdate, shippriority)`.
pub type OrdersByCustkeyVector = OrdIndexedZSet<Tup1<i64>, Tup3<i64, Date, i32>>;
/// The orders, indexed by `(custkey, orderkey)`: `(orderdate, shippriority)`.
pub type OrdersByCustkeyOrderkeyVector = OrdIndexedZSet<Tup2<i64, i64>, Tup2<Date, i32>>;
/// The joined orders `J`, indexed by `(custkey, orderkey)`, the node at which
/// `V` reads them: `(mktsegment, orderdate, shippriority)`.
pub type JoinedByCustkeyOrderkeyVector = OrdIndexedZSet<Tup2<i64, i64>, Tup3<SqlString, Date, i32>>;
/// The line items, indexed by `(custkey, orderkey)`.
pub type LineitemByCustkeyOrderkeyVector =
    OrdIndexedZSet<Tup2<i64, i64>, Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>;

pub type SourceSnapshot = DynamicSnapshot<MergedIndexBatch>;

type Definition = CustomerOrdersLineitemDefinition;

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

/// The sealed readers of one transaction, and what collecting them read.
struct CollectedReaders {
    customers: ReaderRuns<Tup1<i64>, Tup1<SqlString>>,
    orders_by_customer: ReaderRuns<Tup1<i64>, Tup3<i64, Date, i32>>,
    orders_by_order: ReaderRuns<Tup2<i64, i64>, Tup2<Date, i32>>,
    lines: ReaderRuns<Tup2<i64, i64>, Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>,
    touched: TouchedGroups,
    reads: SessionReads,
}

impl CollectedReaders {
    /// Checks that customer `custkey`'s group was collected.
    fn require_touched(&self, custkey: i64) -> Result<(), String> {
        if self.touched.contains(&prefix_custkey(custkey)) {
            Ok(())
        } else {
            Err(format!(
                "customer {custkey} is outside the touched groups: Dlt has no entry for it"
            ))
        }
    }
}

struct ReadViews {
    old: SourceSnapshot,
    dlt: Arc<MergedIndexBatch>,
    /// Set once by [`Q3State::collect`].
    collected: OnceLock<CollectedReaders>,
}

/// One externally prepared transaction's index views and readers, shared by
/// the view circuit's source and joins.
///
/// Publish `Old`, the index snapshot before the transaction, and `Dlt`, the
/// run of the transaction's signed index changes, after index maintenance and
/// before the view transaction. [`Self::collect`] then fills the readers; the
/// circuit's source takes the driving deltas and the joins the partner reads.
/// Keep the publication unchanged until `DBSPHandle::transaction` or the
/// equivalent multi-step commit has completed, then call [`Self::finish`]: the
/// readers are held until then, since `J` and `V` are separate operators and
/// `V` flushes only after `J` has passed every customer. The caller must
/// restore the index, base state, and the view circuit at the same completed
/// transaction.
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

    /// Runs the scan session over the published `Old` and `Dlt` and fills the
    /// readers, which are kept until [`Self::finish`]. The index is read here,
    /// on the calling thread under the merged index's read owner, and nowhere
    /// else; the caller binds the runtime's buffer cache to this thread.
    pub fn collect(&self, pass: Pass) -> Result<(), String> {
        let views = self.views()?;
        let before = RunReads::current_for(WriteOwner::MergedIndex);
        let mut customers = Reader::new(INDEX_ID_CUSTOMER_BY_CUSTKEY, customer_by_custkey);
        let mut orders_by_customer =
            Reader::new(INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY, order_by_custkey);
        let mut orders_by_order = Reader::new(
            INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY,
            order_by_custkey_orderkey,
        );
        let mut lines = Reader::new(
            INDEX_ID_LINEITEM_BY_CUSTKEY_ORDERKEY_LINENUMBER,
            line_by_custkey_orderkey,
        );
        let (touched, counts) = with_read_owner(WriteOwner::MergedIndex, || {
            let mut session = ScanSession::<Definition>::new(&views.old, &views.dlt, pass);
            let readers: &mut [&mut dyn FilledReader<Definition>] = &mut [
                &mut customers,
                &mut orders_by_customer,
                &mut orders_by_order,
                &mut lines,
            ];
            let touched = collect(&mut session, readers)?;
            Ok::<_, String>((touched, session.counts()))
        })?;
        let run_reads = RunReads::current_for(WriteOwner::MergedIndex).since(before);
        let collected = CollectedReaders {
            customers: customers.seal(),
            orders_by_customer: orders_by_customer.seal(),
            orders_by_order: orders_by_order.seal(),
            lines: lines.seal(),
            touched,
            reads: SessionReads { counts, run_reads },
        };
        views
            .collected
            .set(collected)
            .map_err(|_| "the published views were already collected".to_owned())
    }

    /// Release the readers and published handles after all transaction
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

    // The transaction's driving deltas, `ΔC`, `ΔO`, and `ΔL`, are the readers'
    // `Dlt` batches, indexed by the joins' keys. Every row carries its trie keys.

    /// `ΔC`, by `custkey`.
    pub fn customer_deltas(&self) -> Result<CustomerByCustkeyVector, String> {
        Ok(self.views()?.collected_readers()?.customers.delta())
    }

    /// `ΔO`, by `custkey`.
    pub fn order_deltas(&self) -> Result<OrdersByCustkeyVector, String> {
        Ok(self
            .views()?
            .collected_readers()?
            .orders_by_customer
            .delta())
    }

    /// `ΔL`, by `(custkey, orderkey)`.
    pub fn line_deltas(&self) -> Result<LineitemByCustkeyOrderkeyVector, String> {
        Ok(self.views()?.collected_readers()?.lines.delta())
    }

    /// `C⁺`: the customers after the transaction, driven by `ΔO`. A path read
    /// of the Customer stream, so each key of the delta takes its customer.
    pub fn customers_after(
        &self,
        delta: &SpineSnapshot<OrdersByCustkeyVector>,
    ) -> Result<SpineSnapshot<CustomerByCustkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        for Tup1(customer) in requested_keys(delta) {
            collected.require_touched(customer)?;
        }
        Ok(collected.customers.after())
    }

    /// `O⁻`: the orders before the transaction, driven by `ΔC`. A range read
    /// of the Orders stream below the customer.
    pub fn orders_before(
        &self,
        delta: &SpineSnapshot<CustomerByCustkeyVector>,
    ) -> Result<SpineSnapshot<OrdersByCustkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        for Tup1(customer) in requested_keys(delta) {
            collected.require_touched(customer)?;
        }
        Ok(collected.orders_by_customer.before())
    }

    /// `J⁺`: the joined orders at the requested `(custkey, orderkey)` nodes
    /// after the transaction, driven by `ΔL`. `J` is not stored, so this is
    /// `J`'s join re-evaluated on each requested path: the customer of the
    /// customers' after read times the order of the orders-by-order after
    /// read, the weights multiplied.
    pub fn joined_orders_after(
        &self,
        delta: &SpineSnapshot<LineitemByCustkeyOrderkeyVector>,
    ) -> Result<SpineSnapshot<JoinedByCustkeyOrderkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        let customers = collected.customers.after();
        let orders = collected.orders_by_order.after();
        let mut output = RequestedRows::new();
        for Tup2(customer, order) in requested_keys(delta) {
            collected.require_touched(customer)?;
            let segments = values_at(&customers, &Tup1(customer));
            for (Tup2(date, priority), order_weight) in values_at(&orders, &Tup2(customer, order)) {
                for (Tup1(segment), customer_weight) in &segments {
                    let weight = order_weight
                        .checked_mul(*customer_weight)
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

    /// `L⁻`: the line items before the transaction, driven by `ΔJ`. A range
    /// read of the Lineitem stream below the order.
    pub fn lineitems_before(
        &self,
        delta: &SpineSnapshot<JoinedByCustkeyOrderkeyVector>,
    ) -> Result<SpineSnapshot<LineitemByCustkeyOrderkeyVector>, String> {
        let views = self.views()?;
        let collected = views.collected_readers()?;
        for Tup2(customer, _) in requested_keys(delta) {
            collected.require_touched(customer)?;
        }
        Ok(collected.lines.before())
    }
}

impl ReadViews {
    fn collected_readers(&self) -> Result<&CollectedReaders, String> {
        self.collected
            .get()
            .ok_or_else(|| "collect the touched groups before reading them".to_owned())
    }
}

// The readers' projections: each takes one entry of its stream, with its key
// unfolded and its payload decoded, and gives the join's typed row.

/// A row of vector `B`, or why the entry is not one.
type Projected<B> = Result<(<B as BatchReader>::Key, <B as BatchReader>::Val), String>;

fn customer_by_custkey(
    key: &Key,
    payload: &Payload,
) -> Result<(Tup1<i64>, Tup1<SqlString>), String> {
    match (key, payload) {
        (Key::CustomerByCustkey(key), Payload::CustomerByCustkey(payload)) => {
            Ok((Tup1(key.custkey), Tup1(payload.c_mktsegment.clone())))
        }
        other => Err(format!("expected a Customer entry, found {other:?}")),
    }
}

fn order_by_custkey(key: &Key, payload: &Payload) -> Projected<OrdersByCustkeyVector> {
    match (key, payload) {
        (Key::OrdersByCustkeyOrderkey(key), Payload::OrdersByCustkeyOrderkey(payload)) => Ok((
            Tup1(key.custkey),
            Tup3(key.orderkey, payload.o_orderdate, payload.o_shippriority),
        )),
        other => Err(format!("expected an Orders entry, found {other:?}")),
    }
}

fn order_by_custkey_orderkey(
    key: &Key,
    payload: &Payload,
) -> Projected<OrdersByCustkeyOrderkeyVector> {
    match (key, payload) {
        (Key::OrdersByCustkeyOrderkey(key), Payload::OrdersByCustkeyOrderkey(payload)) => Ok((
            Tup2(key.custkey, key.orderkey),
            Tup2(payload.o_orderdate, payload.o_shippriority),
        )),
        other => Err(format!("expected an Orders entry, found {other:?}")),
    }
}

#[allow(clippy::type_complexity)]
fn line_by_custkey_orderkey(
    key: &Key,
    payload: &Payload,
) -> Result<
    (
        Tup2<i64, i64>,
        Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>,
    ),
    String,
> {
    match (key, payload) {
        (
            Key::LineitemByCustkeyOrderkeyLinenumber(key),
            Payload::LineitemByCustkeyOrderkeyLinenumber(payload),
        ) => Ok((
            Tup2(key.custkey, key.orderkey),
            Tup4(
                key.linenumber,
                payload.l_extendedprice,
                payload.l_discount,
                payload.l_shipdate,
            ),
        )),
        other => Err(format!("expected a Lineitem entry, found {other:?}")),
    }
}

/// The values under `key` in `snapshot`, with their weights summed across its
/// batches.
fn values_at<K: DBData, V: DBData>(
    snapshot: &SpineSnapshot<OrdIndexedZSet<K, V>>,
    key: &K,
) -> Vec<(V, ZWeight)> {
    let mut cursor = snapshot.inner().cursor();
    let mut values = Vec::new();
    if cursor.seek_key_exact(key.erase(), None) {
        while cursor.val_valid() {
            values.push((
                cursor.val().downcast_checked::<V>().clone(),
                *cursor.weight().downcast_checked::<ZWeight>(),
            ));
            cursor.step_val();
        }
    }
    values
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
/// collected readers. `J`'s rows are indexed by `(custkey, orderkey)`, the node
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
