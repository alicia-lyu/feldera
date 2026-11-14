//! Manual merged-index maintenance: the index's owner thread, and the conversion of a
//! transaction's net delta into the complete `Dlt` that the view circuit's inputs need.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use anyhow::{bail, ensure, Context, Result};
use crossbeam::sync::{Parker, Unparker};
use dbsp::dynamic::{DowncastTrait, DynData, Erase};
use dbsp::storage::attribution::{with_write_owner, Component};
#[cfg(test)]
use dbsp::storage::buffer_cache::BufferCache;
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
/// One row of `Dlt`: a key, its payload, and a signed weight.
pub(crate) type SourceRow = (Key, Payload, ZWeight);

enum Request {
    BackgroundIdle(mpsc::SyncSender<bool>),
    StorageEvidence(mpsc::SyncSender<serde_json::Value>),
    Snapshot(mpsc::SyncSender<Snapshot>),
    /// Builds the rows' run, `Dlt`, inserts it into the spine, and replies with it.
    Append(
        Vec<SourceRow>,
        mpsc::SyncSender<Result<Arc<IndexBatch>, String>>,
    ),
    Save(StoragePath, mpsc::SyncSender<Result<(), String>>),
    Restore(StoragePath, mpsc::SyncSender<Result<(), String>>),
    /// The buffer cache bound to the index's thread. Tests compare it with the view worker's.
    #[cfg(test)]
    BufferCache(mpsc::SyncSender<Arc<BufferCache>>),
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
    ///
    /// Fails if the thread turns out not to be on the runtime's buffer cache: everything in the
    /// harness uses that one cache, so the index must not run on another.
    pub(crate) fn new(runtime: &Runtime) -> Result<Self> {
        Self::spawn(runtime, Some(runtime.clone()))
    }

    /// Like `new`, but leaves the thread's buffer cache unbound, so that it falls back to the
    /// process-wide auxiliary cache. Only the test of the one-cache check uses it, to show that
    /// `new` refuses such a thread: production has no unbound path.
    #[cfg(test)]
    pub(crate) fn without_cache_binding(runtime: &Runtime) -> Result<Self> {
        Self::spawn(runtime, None)
    }

    /// Starts the owner thread in `runtime`. The thread binds the foreground cache of
    /// `cache_owner`, when there is one, and then checks that it is on the runtime's own cache.
    fn spawn(runtime: &Runtime, cache_owner: Option<Runtime>) -> Result<Self> {
        let (requests, incoming) = mpsc::channel();
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let (started, startup) = mpsc::sync_channel(1);
        let expected_cache_owner = runtime.clone();
        runtime.spawn_aux_thread("q3-manual-merged-index", parker, move |parker| {
            // Every file this thread creates, and every spine it owns and that
            // spine's background compaction, belongs to the merged index.
            with_write_owner(Component::MergedIndex, || {
                bound_to_cache(cache_owner.as_ref(), || {
                    // Before the thread builds anything, so that nothing is written elsewhere.
                    let on_runtime_cache = check_on_runtime_cache(&expected_cache_owner);
                    let refused = on_runtime_cache.is_err();
                    let _ = started.send(on_runtime_cache);
                    if refused {
                        return;
                    }
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
                            #[cfg(test)]
                            Ok(Request::BufferCache(reply)) => {
                                let _ = reply.send(
                                    Runtime::buffer_cache().expect("the index thread has a cache"),
                                );
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
                                    let dlt = Arc::new(batch);
                                    feldera_storage::tokio::TOKIO
                                        .block_on(trace.insert(dlt.clone()));
                                    dlt
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
        startup
            .recv()
            .context("merged-index owner exited at start")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self { requests, unparker })
    }

    fn request<T>(&self, make: impl FnOnce(mpsc::SyncSender<T>) -> Request) -> Result<T> {
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .send(make(reply))
            .context("merged-index owner exited")?;
        self.unparker.unpark();
        result.recv().context("merged-index owner exited")
    }

    /// The buffer cache that the index's thread writes its files through.
    #[cfg(test)]
    pub(crate) fn buffer_cache(&self) -> Result<Arc<BufferCache>> {
        self.request(Request::BufferCache)
    }

    pub(crate) fn snapshot(&self) -> Result<Snapshot> {
        self.request(Request::Snapshot)
    }

    /// Appends `rows` and returns `(Old, Dlt)`: the snapshot before the append, and the
    /// appended run. The index after the append is `Old + Dlt`.
    pub(crate) fn append(&self, rows: Vec<SourceRow>) -> Result<(Snapshot, Arc<IndexBatch>)> {
        let old = self.snapshot()?;
        let dlt = self.request(|reply| Request::Append(rows, reply))?;
        Ok((old, dlt.map_err(anyhow::Error::msg)?))
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

/// Runs `f` with `cache_owner`'s buffer cache bound to the calling thread, or with the thread's
/// default binding when there is no owner.
fn bound_to_cache<R>(cache_owner: Option<&Runtime>, f: impl FnOnce() -> R) -> R {
    match cache_owner {
        Some(runtime) => runtime.with_buffer_cache(0, f),
        None => f(),
    }
}

/// Checks, on the calling thread, that its buffer cache is `runtime`'s foreground cache of
/// worker 0, the cache that the view circuit's worker uses.
fn check_on_runtime_cache(runtime: &Runtime) -> Result<(), String> {
    let thread_cache = Runtime::buffer_cache().ok_or("merged-index thread has no buffer cache")?;
    if Arc::ptr_eq(&thread_cache, &runtime.foreground_buffer_cache(0)) {
        Ok(())
    } else {
        Err("merged-index thread is not on the runtime's buffer cache".to_owned())
    }
}

/// A `Dlt` with no entries, for a view transaction that changes no base table.
pub(crate) fn empty_run() -> Result<Arc<IndexBatch>> {
    Ok(Arc::new(
        build_batch(Vec::new()).map_err(anyhow::Error::msg)?,
    ))
}

/// Where nonempty runs are: how many, and their approximate bytes, in files
/// and in memory. File runs count decodes and in-memory runs count `memory_*`
/// reads, so this qualifies every index read count.
pub(crate) fn runs_layout<'a>(
    runs: impl IntoIterator<Item = &'a Arc<IndexBatch>>,
) -> serde_json::Value {
    let mut layout = BTreeMap::from([("file", (0_usize, 0_usize)), ("memory", (0, 0))]);
    for batch in runs.into_iter().filter(|batch| !batch.is_empty()) {
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

/// Line items whose order is absent, by order key and then line number: the side store of
/// orphan lines, beside the index. The index folds a line item under its order's customer, so
/// a line item with no order has no folded key; it waits here until its order arrives, and
/// native Feldera accepts it too (it never joins). The store is part of base maintenance.
#[derive(Debug, Default)]
pub(crate) struct OrphanLines(BTreeMap<i64, BTreeMap<i32, Line>>);

impl OrphanLines {
    /// Removes and returns the orphan lines of `order_key`.
    fn take(&mut self, order_key: i64) -> BTreeMap<i32, Line> {
        self.0.remove(&order_key).unwrap_or_default()
    }

    /// Stores the orphan lines of `order_key`, replacing any; no lines clear the entry.
    fn put(&mut self, order_key: i64, lines: BTreeMap<i32, Line>) {
        if lines.is_empty() {
            self.0.remove(&order_key);
        } else {
            self.0.insert(order_key, lines);
        }
    }

    /// The orphan lines of `order_key`, by line number.
    #[cfg(test)]
    pub(crate) fn lines(&self, order_key: i64) -> Vec<&Line> {
        self.0
            .get(&order_key)
            .map_or_else(Vec::new, |lines| lines.values().collect())
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Converts the base tables' net delta (`workload::net_delta`) into the rows of `Dlt`, the
/// transaction's complete change of the index; physical folding and encoding stay in batch
/// construction. Deletions carry complete rows.
///
/// A line item folds under its order's customer, so every changed order `o` has a *before
/// owner* and an *after owner*, each a customer or none. They come from the net delta's
/// deletion and insertion of `o`, or else, when the delta does not mention `o`, from
/// `base_owner`, the base `Orders` state, which the transaction left unchanged. The order's
/// lines before are `L⁻`, and the lines after `L⁺ = L⁻ + ΔL(o)`:
///
/// | Case | `Dlt` line rows | Side store |
/// | --- | --- | --- |
/// | same owner `c` | `ΔL(o)` at `c` | unchanged |
/// | the order moves | `−L⁻` at the old owner, `+L⁺` at the new | unchanged |
/// | the order is deleted, lines kept | `−L⁻` at the old owner | `o ↦ L⁺` |
/// | the order arrives for orphan lines | `+L⁺` at the new owner | remove `o` |
/// | orphan lines only | none | `o ↦ L⁺` |
///
/// `L⁻` is read from `old` at `(owner, o)`, or, for an order with no owner before, from
/// `orphans`. Customers and orders fold from the net delta alone; an order folds at its own
/// `o_custkey`, so customers need no side store.
pub(crate) fn prepare(
    change: &Change,
    old: &Snapshot,
    orphans: &mut OrphanLines,
    base_owner: &mut dyn FnMut(i64) -> Result<Option<i64>>,
) -> Result<Vec<SourceRow>> {
    let mut rows = Vec::with_capacity(change.len());
    for (customer, insert) in &change.customers {
        rows.push(customer_row(customer, *insert));
    }

    // What the delta says about each order: its deletion and its insertion.
    let mut order_changes: BTreeMap<i64, (Option<&Order>, Option<&Order>)> = BTreeMap::new();
    for (order, insert) in &change.orders {
        rows.push(order_row(order, *insert)?);
        let (deleted, inserted) = order_changes.entry(order.key).or_default();
        let slot = if *insert { inserted } else { deleted };
        ensure!(
            slot.replace(order).is_none(),
            "duplicate Orders {} of order {} in the net delta",
            if *insert { "insertion" } else { "deletion" },
            order.key
        );
    }

    // ΔL(o) for each order that has any.
    let mut line_changes: BTreeMap<i64, Vec<(&Line, bool)>> = BTreeMap::new();
    for (line, insert) in &change.lines {
        line_changes
            .entry(line.order_key)
            .or_default()
            .push((line, *insert));
    }

    let changed_orders: BTreeSet<i64> = order_changes
        .keys()
        .chain(line_changes.keys())
        .copied()
        .collect();
    for order_key in changed_orders {
        let (before_owner, after_owner) = match order_changes.get(&order_key) {
            Some((deleted, inserted)) => (
                deleted.map(|order| order.customer_key),
                inserted.map(|order| order.customer_key),
            ),
            None => {
                let owner = base_owner(order_key)?;
                (owner, owner)
            }
        };
        let order_lines = line_changes.get(&order_key).map_or(&[][..], Vec::as_slice);
        match (before_owner, after_owner) {
            // The lines stay where they are: only ΔL(o) changes the index.
            (Some(before), Some(after)) if before == after => {
                for (line, insert) in order_lines {
                    rows.push(line_row(line, after, *insert)?);
                }
            }
            _ => move_lines(
                &mut rows,
                old,
                orphans,
                order_key,
                (before_owner, after_owner),
                order_lines,
            )?,
        }
    }
    Ok(rows)
}

/// The `Dlt` rows of an order whose lines change owner (a customer, or none): `−L⁻` at the
/// before owner and `+L⁺` at the after owner, with the side store holding the lines of an
/// order that has no owner. See [`prepare`].
fn move_lines(
    rows: &mut Vec<SourceRow>,
    old: &Snapshot,
    orphans: &mut OrphanLines,
    order_key: i64,
    (before_owner, after_owner): (Option<i64>, Option<i64>),
    delta: &[(&Line, bool)],
) -> Result<()> {
    let lines_before: BTreeMap<i32, Line> = match before_owner {
        Some(customer) => lines_below(old, customer, order_key)?
            .into_iter()
            .map(|line| (line.number, line))
            .collect(),
        None => orphans.take(order_key),
    };

    // Deletions first, so that an upsert's insertion of a line replaces its deletion.
    let mut lines_after = lines_before.clone();
    for (line, _) in delta.iter().filter(|(_, insert)| !insert) {
        ensure!(
            lines_after.remove(&line.number).is_some(),
            "net delta deletes Lineitem ({}, {}), which the order does not hold",
            order_key,
            line.number
        );
    }
    for (line, _) in delta.iter().filter(|(_, insert)| *insert) {
        lines_after.insert(line.number, (*line).clone());
    }

    if let Some(customer) = before_owner {
        for line in lines_before.values() {
            rows.push(line_row(line, customer, false)?);
        }
    }
    match after_owner {
        Some(customer) => {
            for line in lines_after.values() {
                rows.push(line_row(line, customer, true)?);
            }
        }
        None => orphans.put(order_key, lines_after),
    }
    Ok(())
}

/// The current lines of `order`, read from `snapshot`. The scan decodes only matching rows.
pub(crate) fn lines(snapshot: &Snapshot, order: &Order) -> Result<Vec<Line>> {
    lines_below(snapshot, order.customer_key, order.key)
}

/// Seeks the range below `(customer_key, order_key)` and decodes the line items in it.
fn lines_below(snapshot: &Snapshot, customer_key: i64, order_key: i64) -> Result<Vec<Line>> {
    let prefix = prefix_custkey_orderkey_then_linenumber(customer_key, order_key);
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
            orderkey: found_order,
            linenumber: number,
        }) = line_key
        else {
            bail!("non-Lineitem row inside Lineitem range");
        };
        if customer != customer_key || found_order != order_key {
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
    fn runs_layout_counts_nonempty_runs_by_location() {
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
        let layout = runs_layout(Snapshot::with_batches(&factories, vec![run, empty]).batches());
        assert_eq!(
            layout,
            serde_json::json!({
                "file": {"runs": 0, "bytes": 0},
                "memory": {"runs": 1, "bytes": bytes},
            })
        );
    }

    // ---- `prepare`: one test for each row of its table ----

    fn customer(key: i64) -> Customer {
        Customer {
            key,
            segment: "BUILDING".into(),
        }
    }

    fn order(key: i64, customer_key: i64) -> Order {
        Order {
            key,
            customer_key,
            date: "1996-01-02".into(),
            ship_priority: 0,
        }
    }

    fn line(order_key: i64, number: i32) -> Line {
        Line {
            order_key,
            number,
            price_cents: 100,
            discount_hundredths: 4,
            ship_date: "1996-03-13".into(),
        }
    }

    /// `Old`: one run holding `rows`.
    fn old_index(rows: Vec<SourceRow>) -> Snapshot {
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            merged_index_col::StoredPayload,
            ZWeight,
        >();
        Snapshot::with_batches(&factories, vec![Arc::new(build_batch(rows).unwrap())])
    }

    /// The lines of the given orders, at their customers, as `Old` holds them.
    fn old_lines(lines: &[(Line, i64)]) -> Snapshot {
        old_index(
            lines
                .iter()
                .map(|(line, customer)| line_row(line, *customer, true).unwrap())
                .collect(),
        )
    }

    /// The sorted rows of `prepare`, with the base state naming `owners` (order, customer).
    fn prepared(
        change: &Change,
        old: &Snapshot,
        orphans: &mut OrphanLines,
        owners: &[(i64, i64)],
    ) -> Result<Vec<SourceRow>> {
        let mut base_owner = |order: i64| {
            Ok(owners
                .iter()
                .find(|(key, _)| *key == order)
                .map(|(_, c)| *c))
        };
        let mut rows = prepare(change, old, orphans, &mut base_owner)?;
        rows.sort();
        Ok(rows)
    }

    fn sorted(mut rows: Vec<SourceRow>) -> Vec<SourceRow> {
        rows.sort();
        rows
    }

    /// Same owner: the delta of the lines folds at the owner, whether the base state or the
    /// order's replacement names it, and `Old` is not read.
    #[test]
    fn lines_of_an_order_with_one_owner_fold_from_the_delta_alone() {
        let unread = old_index(Vec::new());
        let change = Change {
            lines: vec![(line(20, 2), true), (line(20, 1), false)],
            ..Change::default()
        };
        assert_eq!(
            prepared(&change, &unread, &mut OrphanLines::default(), &[(20, 3)]).unwrap(),
            sorted(vec![
                line_row(&line(20, 2), 3, true).unwrap(),
                line_row(&line(20, 1), 3, false).unwrap(),
            ])
        );

        let replaced = Change {
            customers: vec![(customer(2), true)],
            orders: vec![(order(20, 2), false), (order(20, 2), true)],
            lines: vec![(line(20, 3), true)],
        };
        assert_eq!(
            prepared(&replaced, &unread, &mut OrphanLines::default(), &[]).unwrap(),
            sorted(vec![
                customer_row(&customer(2), true),
                order_row(&order(20, 2), false).unwrap(),
                order_row(&order(20, 2), true).unwrap(),
                line_row(&line(20, 3), 2, true).unwrap(),
            ])
        );
    }

    /// The order moves: every line leaves the old customer and arrives at the new one, those
    /// of `Old` and those the delta adds, and a deleted line does not arrive.
    #[test]
    fn a_moved_order_takes_its_old_and_new_lines_with_it() {
        let old = old_lines(&[(line(20, 1), 2), (line(20, 2), 2), (line(21, 1), 2)]);
        let change = Change {
            orders: vec![(order(20, 2), false), (order(20, 3), true)],
            lines: vec![(line(20, 2), false), (line(20, 3), true)],
            ..Change::default()
        };
        let mut orphans = OrphanLines::default();
        assert_eq!(
            prepared(&change, &old, &mut orphans, &[]).unwrap(),
            sorted(vec![
                order_row(&order(20, 2), false).unwrap(),
                order_row(&order(20, 3), true).unwrap(),
                line_row(&line(20, 1), 2, false).unwrap(),
                line_row(&line(20, 2), 2, false).unwrap(),
                line_row(&line(20, 1), 3, true).unwrap(),
                line_row(&line(20, 3), 3, true).unwrap(),
            ])
        );
        assert!(orphans.is_empty());
    }

    /// The order is deleted and its lines are kept: they leave the customer and wait in the
    /// side store.
    #[test]
    fn a_deleted_order_leaves_its_kept_lines_in_the_side_store() {
        let old = old_lines(&[(line(20, 1), 2), (line(20, 2), 2)]);
        let change = Change {
            orders: vec![(order(20, 2), false)],
            lines: vec![(line(20, 2), false)],
            ..Change::default()
        };
        let mut orphans = OrphanLines::default();
        assert_eq!(
            prepared(&change, &old, &mut orphans, &[]).unwrap(),
            sorted(vec![
                order_row(&order(20, 2), false).unwrap(),
                line_row(&line(20, 1), 2, false).unwrap(),
                line_row(&line(20, 2), 2, false).unwrap(),
            ])
        );
        assert_eq!(orphans.lines(20), vec![&line(20, 1)]);
    }

    /// Orphan lines only: nothing reaches the index. The order then arrives, and the side
    /// store's lines and the delta's fold at its customer.
    #[test]
    fn an_arriving_order_takes_the_orphan_lines() {
        let old = old_index(Vec::new());
        let mut orphans = OrphanLines::default();
        let orphan_lines = Change {
            lines: vec![(line(20, 1), true), (line(20, 2), true)],
            ..Change::default()
        };
        assert_eq!(
            prepared(&orphan_lines, &old, &mut orphans, &[]).unwrap(),
            Vec::new()
        );
        assert_eq!(orphans.lines(20), vec![&line(20, 1), &line(20, 2)]);

        let arrival = Change {
            orders: vec![(order(20, 2), true)],
            lines: vec![(line(20, 3), true), (line(20, 1), false)],
            ..Change::default()
        };
        assert_eq!(
            prepared(&arrival, &old, &mut orphans, &[]).unwrap(),
            sorted(vec![
                order_row(&order(20, 2), true).unwrap(),
                line_row(&line(20, 2), 2, true).unwrap(),
                line_row(&line(20, 3), 2, true).unwrap(),
            ])
        );
        assert!(orphans.is_empty());
    }

    #[test]
    fn a_net_delta_that_contradicts_the_state_is_rejected() {
        let old = old_lines(&[(line(20, 1), 2)]);
        let twice = Change {
            orders: vec![(order(20, 2), false), (order(20, 2), false)],
            ..Change::default()
        };
        let error = prepared(&twice, &old, &mut OrphanLines::default(), &[]).unwrap_err();
        assert!(error.to_string().contains("duplicate Orders deletion"));

        let missing = Change {
            orders: vec![(order(20, 2), false), (order(20, 3), true)],
            lines: vec![(line(20, 9), false)],
            ..Change::default()
        };
        let error = prepared(&missing, &old, &mut OrphanLines::default(), &[]).unwrap_err();
        assert!(error.to_string().contains("does not hold"));
    }
}
