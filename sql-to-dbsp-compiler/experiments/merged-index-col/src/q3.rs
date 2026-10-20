//! Query-specific accumulated-state reads for the RF1/RF2 Q3 prototype.
//!
//! The caller maintains the secondary index before publishing a transaction's
//! immutable views. Order placement comes exclusively from that transaction's
//! complete Orders delta. These readers never insert into a historical trace.
//!
//! Requested rows are decoded (each stored payload with its key) and copied
//! into temporary native batches so the existing join can consume them. This is suboptimal: a future reconstructed
//! cursor could avoid this materialization and its possible sorting costs.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use dbsp::{
    circuit::{splitter_output_chunk_size, RootCircuit},
    dynamic::{DowncastTrait, Erase},
    trace::{
        merged_index::MergedIndexBatch, BatchReader as DynamicBatchReader, Cursor,
        SpineSnapshot as DynamicSnapshot,
    },
    typed_batch::{BatchReader, SpineSnapshot, TypedBatch},
    utils::{Tup1, Tup2, Tup3, Tup4},
    DBData, OrdIndexedZSet, OrdZSet, Stream,
};
use feldera_sqllib::{Date, SqlDecimal, SqlString};

use super::{decode_payload, CustomerOrdersLineitemIndex, SourceKey, SourcePayload, StoredPayload};

pub type CustomerBatch = OrdIndexedZSet<Tup1<i64>, Tup1<SqlString>>;
pub type OrdersBatch = OrdIndexedZSet<Tup1<i64>, Tup3<i64, Date, i32>>;
pub type JoinedOrdersBatch = OrdIndexedZSet<Tup1<i64>, Tup4<i64, SqlString, Date, i32>>;
pub type LineitemBatch =
    OrdIndexedZSet<Tup1<i64>, Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>>;
pub type SourceSnapshot = DynamicSnapshot<MergedIndexBatch>;

#[derive(Clone)]
struct ReadViews {
    before: SourceSnapshot,
    after: SourceSnapshot,
    order_customers: BTreeMap<i64, BTreeSet<i64>>,
}

/// One externally prepared transaction's read views, shared by the generated joins.
///
/// Publish after complete index maintenance and before starting Q3. Keep the
/// publication unchanged until `DBSPHandle::transaction` or the equivalent
/// multi-step commit has completed, then call [`Self::finish`]. The caller must
/// restore the index, base state, and Q3 at the same completed transaction.
/// Customer sets contain candidate physical locations for each requested order;
/// both temporal readers probe the same locations in their respective snapshots.
/// They deduplicate addresses without storing payloads or signed weights.
#[derive(Clone, Default)]
pub struct Q3State {
    views: Arc<Mutex<Option<Arc<ReadViews>>>>,
}

impl Q3State {
    pub fn publish(
        &self,
        before: SourceSnapshot,
        after: SourceSnapshot,
        order_customers: BTreeMap<i64, BTreeSet<i64>>,
    ) -> Result<(), String> {
        let mut views = self.views.lock().map_err(|_| "Q3 state lock poisoned")?;
        if views.is_some() {
            return Err("finish the previous Q3 transaction before publishing another".into());
        }
        *views = Some(Arc::new(ReadViews {
            before,
            after,
            order_customers,
        }));
        Ok(())
    }

    /// Release published handles after all transaction consumers finish.
    pub fn finish(&self) -> Result<(), String> {
        let mut views = self.views.lock().map_err(|_| "Q3 state lock poisoned")?;
        if views.take().is_none() {
            return Err("no Q3 transaction views were published".into());
        }
        Ok(())
    }

    fn views(&self) -> Result<Arc<ReadViews>, String> {
        self.views
            .lock()
            .map_err(|_| "Q3 state lock poisoned")?
            .clone()
            .ok_or_else(|| "publish merged-index views before Q3 maintenance".into())
    }

    pub fn customers_after(
        &self,
        delta: &SpineSnapshot<OrdersBatch>,
    ) -> Result<SpineSnapshot<CustomerBatch>, String> {
        let views = self.views()?;
        let index = CustomerOrdersLineitemIndex;
        let mut output = RequestedRows::new();
        for customer in requested_keys(delta) {
            let key = index.fold(&SourceKey::Customer(customer));
            scan(
                &index,
                &views.after,
                key.as_bytes(),
                |key, payload, weight| {
                    if let (SourceKey::Customer(id), SourcePayload::Customer(payload)) =
                        (key, payload)
                    {
                        output.push(Tup1(id), Tup1(payload.segment.clone()), weight);
                    }
                    Ok(())
                },
            )?;
        }
        Ok(output.finish())
    }

    pub fn orders_before(
        &self,
        delta: &SpineSnapshot<CustomerBatch>,
    ) -> Result<SpineSnapshot<OrdersBatch>, String> {
        let views = self.views()?;
        let index = CustomerOrdersLineitemIndex;
        let mut output = RequestedRows::new();
        for customer in requested_keys(delta) {
            scan(
                &index,
                &views.before,
                &index.prefix_custkey(customer),
                |key, payload, weight| {
                    if let (SourceKey::Orders(c, order), SourcePayload::Orders(payload)) =
                        (key, payload)
                    {
                        output.push(
                            Tup1(c),
                            Tup3(order, payload.order_date, payload.ship_priority),
                            weight,
                        );
                    }
                    Ok(())
                },
            )?;
        }
        Ok(output.finish())
    }

    pub fn joined_orders_after(
        &self,
        delta: &SpineSnapshot<LineitemBatch>,
    ) -> Result<SpineSnapshot<JoinedOrdersBatch>, String> {
        let views = self.views()?;
        let index = CustomerOrdersLineitemIndex;
        let mut output = RequestedRows::new();
        for order in requested_keys(delta) {
            for &customer in placements(&views, order)? {
                let mut customers = Vec::new();
                let customer_key = index.fold(&SourceKey::Customer(customer));
                scan(
                    &index,
                    &views.after,
                    customer_key.as_bytes(),
                    |key, payload, weight| {
                        if let (SourceKey::Customer(_), SourcePayload::Customer(payload)) =
                            (key, payload)
                        {
                            customers.push((payload.segment.clone(), weight));
                        }
                        Ok(())
                    },
                )?;
                let order_key = index.fold(&SourceKey::Orders(customer, order));
                scan(
                    &index,
                    &views.after,
                    order_key.as_bytes(),
                    |key, payload, order_weight| {
                        if let (SourceKey::Orders(_, _), SourcePayload::Orders(payload)) =
                            (key, payload)
                        {
                            for (segment, customer_weight) in &customers {
                                let weight = order_weight
                                    .checked_mul(*customer_weight)
                                    .ok_or("Q3 reconstructed join weight overflow")?;
                                output.push(
                                    Tup1(order),
                                    Tup4(
                                        customer,
                                        segment.clone(),
                                        payload.order_date,
                                        payload.ship_priority,
                                    ),
                                    weight,
                                );
                            }
                        }
                        Ok(())
                    },
                )?;
            }
        }
        Ok(output.finish())
    }

    pub fn lineitems_before(
        &self,
        delta: &SpineSnapshot<JoinedOrdersBatch>,
    ) -> Result<SpineSnapshot<LineitemBatch>, String> {
        let views = self.views()?;
        let index = CustomerOrdersLineitemIndex;
        let mut output = RequestedRows::new();
        for order in requested_keys(delta) {
            for &customer in placements(&views, order)? {
                scan(
                    &index,
                    &views.before,
                    &index.prefix_custkey_orderkey_then_linenumber(customer, order),
                    |key, payload, weight| {
                        if let (SourceKey::Lineitem(_, o, line), SourcePayload::Lineitem(payload)) =
                            (key, payload)
                        {
                            output.push(
                                Tup1(o),
                                Tup4(
                                    line,
                                    payload.extended_price,
                                    payload.discount,
                                    payload.ship_date,
                                ),
                                weight,
                            );
                        }
                        Ok(())
                    },
                )?;
            }
        }
        Ok(output.finish())
    }
}

fn placements(views: &ReadViews, order: i64) -> Result<&BTreeSet<i64>, String> {
    views
        .order_customers
        .get(&order)
        .filter(|customers| !customers.is_empty())
        .ok_or_else(|| format!("order {order} is missing from the current Orders delta"))
}

fn requested_keys<V: DBData>(delta: &SpineSnapshot<OrdIndexedZSet<Tup1<i64>, V>>) -> Vec<i64> {
    let mut cursor = delta.inner().cursor();
    let mut keys = Vec::new();
    while cursor.key_valid() {
        keys.push(cursor.key().downcast_checked::<Tup1<i64>>().0);
        cursor.step_key();
    }
    keys
}

fn scan(
    index: &CustomerOrdersLineitemIndex,
    snapshot: &SourceSnapshot,
    prefix: &[u8],
    mut visit: impl FnMut(SourceKey, SourcePayload, i64) -> Result<(), String>,
) -> Result<(), String> {
    let mut cursor = snapshot.cursor();
    let seek = prefix.to_vec();
    cursor.seek_key(seek.erase());
    while cursor.key_valid() {
        let bytes = cursor.key().downcast_checked::<Vec<u8>>();
        if !bytes.starts_with(prefix) {
            break;
        }
        let key = index.unfold(bytes)?;
        while cursor.val_valid() {
            let weight = **cursor.weight();
            // The stored bytes carry no tag: the key names the fields.
            let stored = cursor.val().downcast_checked::<StoredPayload>();
            let payload = decode_payload(&key, stored)
                .map_err(|error| format!("stored payload of key {key:?}: {error}"))?;
            visit(key.clone(), payload, weight)?;
            cursor.step_val();
        }
        cursor.step_key();
    }
    Ok(())
}

/// Bounded sort chunks, combined through the native consolidated snapshot cursor.
/// The snapshot still owns all result chunks; this is not a hard memory bound.
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

/// The generated Orders/Customer join, with both historical operands reconstructed.
pub fn join_orders_customers<F, It>(
    orders: &Stream<RootCircuit, OrdersBatch>,
    customers: &Stream<RootCircuit, CustomerBatch>,
    state: Q3State,
    join: F,
) -> Stream<RootCircuit, JoinedOrdersBatch>
where
    F: Fn(&Tup1<i64>, &Tup3<i64, Date, i32>, &Tup1<SqlString>) -> It + Clone + 'static,
    It: IntoIterator<Item = (Tup1<i64>, Tup4<i64, SqlString, Date, i32>)> + 'static,
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

/// The generated Lineitem/joined-orders join, preserving the native delta formula.
pub fn join_lineitems_orders<F, OV>(
    lines: &Stream<RootCircuit, LineitemBatch>,
    orders: &Stream<RootCircuit, JoinedOrdersBatch>,
    state: Q3State,
    join: F,
) -> Stream<RootCircuit, OrdZSet<OV>>
where
    OV: DBData,
    F: Fn(
            &Tup1<i64>,
            &Tup4<i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>,
            &Tup4<i64, SqlString, Date, i32>,
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
                .expect("Q3 B+ reconstruction failed")
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
