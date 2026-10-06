//! Manual merged-index maintenance for the restricted RF1/RF2 experiment.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use anyhow::{bail, Context, Result};
use crossbeam::sync::{Parker, Unparker};
use dbsp::dynamic::DynData;
use dbsp::trace::{
    BatchReaderFactories, OrdIndexedWSetFactories, Spine, SpineSnapshot, Trace, TraceRole,
    WithSnapshot,
};
use dbsp::{DynZWeight, Runtime, ZWeight};
use feldera_sqllib::{Date, SqlDecimal};
use feldera_storage::StoragePath;
use merged_index_col::{
    CustomerOrdersLineitemIndex, CustomerPayload, LineitemPayload, OrdersPayload, SourceKey,
    SourcePayload,
};

use crate::data::{Customer, Line, Order};
use crate::{Change, Reference};

type IndexBatch = dbsp::trace::merged_index::MergedIndexBatch;
pub(crate) type Snapshot = SpineSnapshot<IndexBatch>;
type SourceRow = (SourceKey, SourcePayload, ZWeight);
type OrderLocations = BTreeMap<i64, BTreeSet<i64>>;

enum Request {
    Snapshot(mpsc::SyncSender<Snapshot>),
    Append(Vec<SourceRow>, mpsc::SyncSender<Result<Snapshot, String>>),
    Save(StoragePath, mpsc::SyncSender<Result<(), String>>),
    Restore(StoragePath, mpsc::SyncSender<Result<(), String>>),
}

pub(crate) struct ManualIndex {
    requests: mpsc::Sender<Request>,
    unparker: Unparker,
}

impl ManualIndex {
    pub(crate) fn new(runtime: &Runtime) -> Self {
        let (requests, incoming) = mpsc::channel();
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        runtime.spawn_aux_thread("q3-manual-merged-index", parker, move |parker| {
            let index = CustomerOrdersLineitemIndex::default();
            let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
                Vec<u8>,
                Vec<u8>,
                ZWeight,
            >();
            let mut trace = Spine::<IndexBatch>::new(
                &factories,
                Arc::new("q3_manual_merged_index".to_owned()),
                TraceRole::Integral,
            );
            while !Runtime::kill_in_progress() {
                match incoming.try_recv() {
                    Ok(Request::Snapshot(reply)) => {
                        let _ = reply.send(trace.ro_snapshot());
                    }
                    Ok(Request::Append(rows, reply)) => {
                        let result = index.build_batch(rows).map(|batch| {
                            feldera_storage::tokio::TOKIO.block_on(trace.insert(batch));
                            trace.ro_snapshot()
                        });
                        let _ = reply.send(result);
                    }
                    Ok(Request::Save(path, reply)) => {
                        let result = (|| {
                            let mut files = Vec::new();
                            trace
                                .save(&path, "q3_manual_merged_index", &mut files)
                                .map_err(|error| error.to_string())?;
                            Runtime::storage_backend()
                                .map_err(|error| error.to_string())?
                                .commit_all(&files)
                                .map_err(|error| error.to_string())
                        })();
                        let _ = reply.send(result);
                    }
                    Ok(Request::Restore(path, reply)) => {
                        let result = trace
                            .restore(&path, "q3_manual_merged_index")
                            .map_err(|error| error.to_string());
                        let _ = reply.send(result);
                    }
                    Err(mpsc::TryRecvError::Empty) => parker.park(),
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        });
        Self { requests, unparker }
    }

    fn request<T>(&self, make: impl FnOnce(mpsc::SyncSender<T>) -> Request) -> Result<T> {
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .send(make(reply))
            .context("merged-index owner exited")?;
        self.unparker.unpark();
        result.recv().context("merged-index owner exited")
    }

    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        self.request(Request::Snapshot)
    }

    pub(crate) fn append(&self, rows: Vec<SourceRow>) -> Result<(Snapshot, Snapshot)> {
        let before = self.snapshot()?;
        let after = self.request(|reply| Request::Append(rows, reply))?;
        Ok((before, after.map_err(anyhow::Error::msg)?))
    }

    #[allow(dead_code)]
    pub(crate) fn save(&self, path: StoragePath) -> Result<()> {
        self.request(|reply| Request::Save(path, reply))?
            .map_err(anyhow::Error::msg)
    }

    #[allow(dead_code)]
    pub(crate) fn restore(&self, path: StoragePath) -> Result<()> {
        self.request(|reply| Request::Restore(path, reply))?
            .map_err(anyhow::Error::msg)
    }
}

fn sql_date(text: &str) -> Result<Date> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .context("invalid source date")
}

fn sql_decimal<const P: usize, const S: usize>(hundredths: i64) -> Result<SqlDecimal<P, S>> {
    SqlDecimal::<P, S>::new(i128::from(hundredths), 2)
        .context("source decimal outside SQL precision")
}

fn customer_row(row: &Customer, insert: bool) -> SourceRow {
    (
        SourceKey::Customer(row.key),
        SourcePayload::Customer(CustomerPayload {
            segment: row.segment.clone().into(),
        }),
        if insert { 1 } else { -1 },
    )
}

fn order_row(row: &Order, insert: bool) -> Result<SourceRow> {
    Ok((
        SourceKey::Orders(row.customer_key, row.key),
        SourcePayload::Orders(OrdersPayload {
            order_date: sql_date(&row.date)?,
            ship_priority: row.ship_priority,
        }),
        if insert { 1 } else { -1 },
    ))
}

fn line_row(row: &Line, customer: i64, insert: bool) -> Result<SourceRow> {
    Ok((
        SourceKey::Lineitem(customer, row.order_key, row.number),
        SourcePayload::Lineitem(LineitemPayload {
            ship_date: sql_date(&row.ship_date)?,
            extended_price: sql_decimal(row.price_cents)?,
            discount: sql_decimal(row.discount_hundredths)?,
        }),
        if insert { 1 } else { -1 },
    ))
}

/// Validate the entire transaction before either owner is mutated. Only complete
/// insert/delete order groups, plus initial customer inserts, are supported.
pub(crate) fn prepare(
    change: &Change,
    state: &Reference,
) -> Result<(OrderLocations, Vec<SourceRow>)> {
    let mut placements = OrderLocations::new();
    let mut seen_customers = BTreeSet::new();
    let mut seen_orders = BTreeSet::new();
    let mut seen_lines = BTreeSet::new();
    let mut rows = Vec::with_capacity(change.len());
    for (customer, insert) in &change.customers {
        if !insert
            || !state.orders.is_empty()
            || state.customers.contains_key(&customer.key)
            || !seen_customers.insert(customer.key)
        {
            bail!("only initial Customer insertion is supported");
        }
        rows.push(customer_row(customer, *insert));
    }
    for (order, insert) in &change.orders {
        if !seen_orders.insert(order.key) {
            bail!("duplicate or replacement Orders delta for {}", order.key);
        }
        if *insert {
            if state.orders.contains_key(&order.key)
                || !(state.customers.contains_key(&order.customer_key)
                    || change
                        .customers
                        .iter()
                        .any(|(c, plus)| *plus && c.key == order.customer_key))
            {
                bail!(
                    "RF1 Order {} has missing Customer or already exists",
                    order.key
                );
            }
        } else if state.orders.get(&order.key) != Some(order) {
            bail!("RF2 Order {} is missing or has changed payload", order.key);
        }
        placements
            .entry(order.key)
            .or_default()
            .insert(order.customer_key);
        rows.push(order_row(order, *insert)?);
    }
    for (line, insert) in &change.lines {
        let key = (line.order_key, line.number);
        if !seen_lines.insert(key) {
            bail!("duplicate or replacement Lineitem delta for {key:?}");
        }
        let order = change
            .orders
            .iter()
            .find(|(order, _)| order.key == line.order_key)
            .with_context(|| {
                format!("Lineitem {key:?} has no Orders record in this transaction")
            })?;
        if *insert != order.1 {
            bail!("Lineitem {key:?} direction differs from its Orders delta");
        }
        if *insert && state.lines.contains_key(&key) {
            bail!("RF1 Lineitem {key:?} already exists");
        }
        if !insert && state.lines.get(&key) != Some(line) {
            bail!("RF2 Lineitem {key:?} is missing or has changed payload");
        }
        rows.push(line_row(line, order.0.customer_key, *insert)?);
    }
    for (order, insert) in &change.orders {
        if !insert
            && state.lines.values().any(|line| {
                line.order_key == order.key && !seen_lines.contains(&(line.order_key, line.number))
            })
        {
            bail!("RF2 Order {} does not delete all its Lineitems", order.key);
        }
    }
    Ok((placements, rows))
}

/// Group each order and all of its lines so loading never exposes an orphan.
pub(crate) fn grouped_load(load: Change, target: usize) -> Result<Vec<Change>> {
    let mut batches = Vec::new();
    let mut customers = Change::default();
    for row in load.customers {
        customers.customers.push(row);
        if customers.len() >= target {
            batches.push(std::mem::take(&mut customers));
        }
    }
    if customers.len() > 0 {
        batches.push(customers);
    }
    let mut lines_by_order: BTreeMap<i64, Vec<(Line, bool)>> = BTreeMap::new();
    for row in load.lines {
        lines_by_order.entry(row.0.order_key).or_default().push(row);
    }
    let mut batch = Change::default();
    for order in load.orders {
        let group_lines = lines_by_order.remove(&order.0.key).unwrap_or_default();
        let group_size = 1 + group_lines.len();
        if batch.len() > 0 && batch.len() + group_size > target {
            batches.push(std::mem::take(&mut batch));
        }
        batch.orders.push(order);
        batch.lines.extend(group_lines);
    }
    if !lines_by_order.is_empty() {
        bail!("initial Lineitem load contains Orders not in this load");
    }
    if batch.len() > 0 {
        batches.push(batch);
    }
    Ok(batches)
}
