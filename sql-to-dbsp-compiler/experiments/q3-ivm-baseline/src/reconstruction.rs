//! Manual merged-index maintenance for the restricted RF1/RF2 experiment.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use anyhow::{bail, Context, Result};
use crossbeam::sync::{Parker, Unparker};
use dbsp::dynamic::{DowncastTrait, DynData, Erase};
use dbsp::storage::write_attribution::{with_write_owner, WriteOwner};
use dbsp::trace::merged_index::MergedIndexDefinition;
use dbsp::trace::{
    BatchLocation, BatchReader, BatchReaderFactories, Cursor, OrdIndexedWSetFactories, Spine,
    SpineSnapshot, Trace, TraceRole, WithSnapshot,
};
use dbsp::{DynZWeight, Runtime, ZWeight};
use feldera_sqllib::{Date, SqlDecimal};
use feldera_storage::StoragePath;
use merged_index_col::{
    build_batch, decode_payload, prefix_custkey_orderkey_then_linenumber, CustomerByCustkeyKey,
    CustomerByCustkeyPayload, CustomerOrdersLineitemDefinition, Key,
    LineitemByCustkeyOrderkeyLinenumberKey, LineitemByCustkeyOrderkeyLinenumberPayload,
    OrdersByCustkeyOrderkeyKey, OrdersByCustkeyOrderkeyPayload, Payload,
};

use crate::data::{Customer, Line, Order};
use crate::Change;

type IndexBatch = dbsp::trace::merged_index::MergedIndexBatch;
pub(crate) type Snapshot = SpineSnapshot<IndexBatch>;
type SourceRow = (Key, Payload, ZWeight);
type OrderLocations = BTreeMap<i64, BTreeSet<i64>>;

enum Request {
    BackgroundIdle(mpsc::SyncSender<bool>),
    StorageEvidence(mpsc::SyncSender<serde_json::Value>),
    Snapshot(mpsc::SyncSender<Snapshot>),
    Append(Vec<SourceRow>, mpsc::SyncSender<Result<Snapshot, String>>),
    Save(StoragePath, mpsc::SyncSender<Result<(), String>>),
    Restore(StoragePath, mpsc::SyncSender<Result<(), String>>),
    /// Request full compaction of the index's spine.
    StartCompaction(mpsc::SyncSender<()>),
    CompactionComplete(mpsc::SyncSender<bool>),
}

pub(crate) struct ManualIndex {
    requests: mpsc::Sender<Request>,
    unparker: Unparker,
}

impl ManualIndex {
    /// Starts the index's owner thread in `runtime`, the runtime that hosts the base and the view
    /// circuit: the index is secondary storage of the base relations, so its thread, buffer cache
    /// and storage directory are the runtime's. The view circuit's index readers use the same
    /// buffer cache.
    pub(crate) fn new(runtime: &Runtime) -> Self {
        let (requests, incoming) = mpsc::channel();
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let cache_runtime = runtime.clone();
        runtime.spawn_aux_thread("q3-manual-merged-index", parker, move |parker| {
            // Every file this thread creates, and every spine it owns and that
            // spine's background compaction, belongs to the merged index.
            with_write_owner(WriteOwner::MergedIndex, || {
                cache_runtime.with_buffer_cache(0, || {
                    // Folded byte keys; packed payload bytes.
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
                            Ok(Request::BackgroundIdle(reply)) => {
                                let _ = reply.send(trace.is_background_merge_idle());
                            }
                            Ok(Request::StartCompaction(reply)) => {
                                trace.initiate_compaction();
                                let _ = reply.send(());
                            }
                            Ok(Request::CompactionComplete(reply)) => {
                                let _ = reply.send(trace.is_compaction_complete());
                            }
                            Ok(Request::StorageEvidence(reply)) => {
                                let batches = trace.get_batches();
                                let memory_batches = batches
                                    .iter()
                                    .filter(|batch| {
                                        !batch.is_empty()
                                            && batch.location() != BatchLocation::Storage
                                    })
                                    .count();
                                let storage_batches = batches
                                    .iter()
                                    .filter(|batch| {
                                        !batch.is_empty()
                                            && batch.location() == BatchLocation::Storage
                                    })
                                    .count();
                                let _ = reply.send(serde_json::json!({
                                    "snapshot_batch_count": batches.len(),
                                    "memory_batch_count": memory_batches,
                                    "storage_batch_count": storage_batches,
                                    "file_backed": memory_batches == 0,
                                }));
                            }
                            Ok(Request::Snapshot(reply)) => {
                                let _ = reply.send(trace.ro_snapshot());
                            }
                            Ok(Request::Append(rows, reply)) => {
                                let result = build_batch(rows).map(|batch| {
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
            });
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

    pub(crate) fn is_background_merge_idle(&self) -> Result<bool> {
        self.request(Request::BackgroundIdle)
    }

    /// Requests full compaction; it runs in the background.
    pub(crate) fn start_compaction(&self) -> Result<()> {
        self.request(Request::StartCompaction)
    }

    /// Whether the spine has compacted to at most one run with no merge pending.
    pub(crate) fn is_compaction_complete(&self) -> Result<bool> {
        self.request(Request::CompactionComplete)
    }

    pub(crate) fn storage_evidence(&self) -> Result<serde_json::Value> {
        self.request(Request::StorageEvidence)
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

/// Where a snapshot's nonempty runs are: how many, and their approximate
/// bytes, in files and in memory. Only file runs count key, value, and weight
/// reads, so this qualifies every index read count.
pub(crate) fn snapshot_layout(snapshot: &Snapshot) -> serde_json::Value {
    let mut layout = BTreeMap::from([("file", (0_usize, 0_usize)), ("memory", (0, 0))]);
    for batch in snapshot.batches().iter().filter(|batch| !batch.is_empty()) {
        let location = match batch.location() {
            BatchLocation::Storage => "file",
            BatchLocation::Memory => "memory",
        };
        let (runs, bytes) = layout
            .get_mut(location)
            .expect("both locations are present");
        *runs += 1;
        *bytes += batch.approximate_byte_size();
    }
    layout
        .into_iter()
        .map(|(location, (runs, bytes))| {
            (
                location.to_owned(),
                serde_json::json!({"runs": runs, "bytes": bytes}),
            )
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
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
        Key::CustomerByCustkey(CustomerByCustkeyKey { custkey: row.key }),
        Payload::CustomerByCustkey(CustomerByCustkeyPayload {
            c_mktsegment: row.segment.clone().into(),
        }),
        if insert { 1 } else { -1 },
    )
}

fn order_row(row: &Order, insert: bool) -> Result<SourceRow> {
    Ok((
        Key::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyKey {
            custkey: row.customer_key,
            orderkey: row.key,
        }),
        Payload::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyPayload {
            o_orderdate: sql_date(&row.date)?,
            o_shippriority: row.ship_priority,
        }),
        if insert { 1 } else { -1 },
    ))
}

fn line_row(row: &Line, customer: i64, insert: bool) -> Result<SourceRow> {
    Ok((
        Key::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberKey {
            custkey: customer,
            orderkey: row.order_key,
            linenumber: row.number,
        }),
        Payload::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberPayload {
            l_shipdate: sql_date(&row.ship_date)?,
            l_extendedprice: sql_decimal(row.price_cents)?,
            l_discount: sql_decimal(row.discount_hundredths)?,
        }),
        if insert { 1 } else { -1 },
    ))
}

/// Convert the base tables' net delta (`workload::net_delta`); physical folding/encoding stays
/// in batch construction. Every Lineitem needs its Orders row in the same delta, for the
/// customer that places it, and deletions carry complete rows.
pub(crate) fn prepare(change: &Change) -> Result<(OrderLocations, Vec<SourceRow>)> {
    let mut placements = OrderLocations::new();
    let orders: BTreeMap<_, _> = change
        .orders
        .iter()
        .map(|(row, insert)| (row.key, (row, insert)))
        .collect();
    let mut rows = Vec::with_capacity(change.len());
    for (customer, insert) in &change.customers {
        rows.push(customer_row(customer, *insert));
    }
    for (order, insert) in &change.orders {
        placements
            .entry(order.key)
            .or_default()
            .insert(order.customer_key);
        rows.push(order_row(order, *insert)?);
    }
    for (line, insert) in &change.lines {
        let (order, direction) = orders.get(&line.order_key).with_context(|| {
            format!(
                "Lineitem ({}, {}) has no Orders record in this transaction",
                line.order_key, line.number
            )
        })?;
        if insert != *direction {
            bail!("Lineitem direction differs from Orders delta");
        }
        rows.push(line_row(line, order.customer_key, *insert)?);
    }
    Ok((placements, rows))
}

/// Seek one current complete Lineitem range, decoding only matching rows.
pub(crate) fn lines(snapshot: &Snapshot, order: &Order) -> Result<Vec<Line>> {
    let prefix = prefix_custkey_orderkey_then_linenumber(order.customer_key, order.key);
    let mut cursor = snapshot.cursor();
    cursor.seek_key(prefix.erase());
    let mut rows = Vec::new();
    while cursor.key_valid() {
        let key = cursor.key().downcast_checked::<Vec<u8>>().clone();
        if !key.starts_with(&prefix) {
            break;
        }
        let line_key =
            CustomerOrdersLineitemDefinition::unfold(&key).map_err(anyhow::Error::msg)?;
        let Key::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberKey {
            custkey: customer,
            orderkey: order_key,
            linenumber: number,
        }) = line_key
        else {
            bail!("non-Lineitem row inside Lineitem range");
        };
        if customer != order.customer_key || order_key != order.key {
            bail!("Lineitem range has unexpected identity");
        }
        while cursor.val_valid() {
            if **cursor.weight() != 1 {
                bail!("current Lineitem does not have unit multiplicity");
            }
            // The stored bytes carry no tag: the key names the fields.
            let stored = cursor.val().downcast_checked::<Vec<u8>>();
            let Payload::LineitemByCustkeyOrderkeyLinenumber(payload) =
                decode_payload(&line_key, stored).map_err(anyhow::Error::msg)?
            else {
                bail!("Lineitem payload has unexpected type");
            };
            rows.push(Line {
                order_key,
                number,
                price_cents: crate::data::decimal_hundredths(
                    &payload.l_extendedprice.to_string(),
                    false,
                )?,
                discount_hundredths: crate::data::decimal_hundredths(
                    &payload.l_discount.to_string(),
                    false,
                )?,
                ship_date: payload.l_shipdate.to_string(),
            });
            cursor.step_val();
        }
        cursor.step_key();
    }
    Ok(rows)
}

/// Group each order and all of its lines so loading never exposes an orphan.
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_layout_counts_nonempty_runs_by_location() {
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            merged_index_col::StoredPayload,
            ZWeight,
        >();
        let run = Arc::new(
            build_batch(vec![(
                Key::CustomerByCustkey(CustomerByCustkeyKey { custkey: 1 }),
                Payload::CustomerByCustkey(CustomerByCustkeyPayload {
                    c_mktsegment: "BUILDING".into(),
                }),
                1,
            )])
            .unwrap(),
        );
        let empty = Arc::new(build_batch(Vec::new()).unwrap());
        let bytes = run.approximate_byte_size();
        let layout = snapshot_layout(&Snapshot::with_batches(&factories, vec![run, empty]));
        assert_eq!(
            layout,
            serde_json::json!({
                "file": {"runs": 0, "bytes": 0},
                "memory": {"runs": 1, "bytes": bytes},
            })
        );
    }
}
