//! The MI_P merged index beside the circuits: its owner thread, and the
//! folding of a net delta into the index's `Dlt`.
//!
//! The thread follows the Q3 harness's `ManualIndex`, reduced to what the
//! correctness harness needs (a snapshot and an append): no checkpoint, no
//! compaction control, no prefix-filter arm.
//!
//! Every MI_P path is made of its own row's columns (a line item's part and
//! supplier are its `l_partkey` and `l_suppkey`), so the net delta folds row by
//! row. Unlike COL, no row needs an owner from another table, and no row waits
//! in a side store.

use std::sync::{mpsc, Arc};

use anyhow::{Context, Result};
use crossbeam::sync::{Parker, Unparker};
use dbsp::{
    dynamic::DynData,
    storage::attribution::{with_write_owner, Component},
    trace::{
        merged_index::MergedIndexBatch, BatchReaderFactories, OrdIndexedWSetFactories, Spine,
        SpineSnapshot, Trace, TraceRole, WithSnapshot,
    },
    DynZWeight, Runtime, ZWeight,
};
use feldera_sqllib::{Date, SqlDecimal};
use merged_index_p::{
    build_batch_in, Key, LineitemByPartkeySuppkeyOrderkeyLinenumberKey,
    LineitemByPartkeySuppkeyOrderkeyLinenumberPayload, PartByPartkeyKey, PartByPartkeyPayload,
    PartsuppByPartkeySuppkeyKey, PartsuppByPartkeySuppkeyPayload, Payload, StoredPayload,
};

use crate::data::{Change, Line, Part, Partsupp};

pub type Snapshot = SpineSnapshot<MergedIndexBatch>;

/// One row of `Dlt`: a key, its payload, and a signed weight.
pub type IndexRow = (Key, Payload, ZWeight);

enum Request {
    Snapshot(mpsc::SyncSender<Snapshot>),
    /// Builds the rows' run, `Dlt`, inserts it into the spine, and replies with it.
    Append(
        Vec<IndexRow>,
        mpsc::SyncSender<Result<Arc<MergedIndexBatch>, String>>,
    ),
}

/// The index's owner thread, on the runtime's buffer cache.
pub struct ManualIndex {
    requests: mpsc::Sender<Request>,
    unparker: Unparker,
}

impl ManualIndex {
    /// Starts the owner thread in `runtime`, the runtime of both circuits.
    /// Fails if the thread is not on the runtime's buffer cache, which the
    /// view circuit's reads of the index use too.
    pub fn new(runtime: &Runtime) -> Result<Self> {
        let (requests, incoming) = mpsc::channel();
        let parker = Parker::new();
        let unparker = parker.unparker().clone();
        let (started, startup) = mpsc::sync_channel(1);
        let cache_owner = runtime.clone();
        runtime.spawn_aux_thread("q14-manual-merged-index", parker, move |parker| {
            // Every file this thread writes belongs to the merged index.
            with_write_owner(Component::MergedIndex, || {
                cache_owner.with_buffer_cache(0, || {
                    let on_runtime_cache = check_on_runtime_cache(&cache_owner);
                    let refused = on_runtime_cache.is_err();
                    let _ = started.send(on_runtime_cache);
                    if refused {
                        return;
                    }
                    let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
                        Vec<u8>,
                        StoredPayload,
                        ZWeight,
                    >();
                    let mut trace = Spine::<MergedIndexBatch>::new(
                        &factories,
                        Arc::new("q14_manual_merged_index".to_owned()),
                        TraceRole::Integral,
                    );
                    while !Runtime::kill_in_progress() {
                        match incoming.try_recv() {
                            Ok(Request::Snapshot(reply)) => {
                                let _ = reply.send(trace.ro_snapshot());
                            }
                            Ok(Request::Append(rows, reply)) => {
                                let result = build_batch_in(&factories, rows).map(|batch| {
                                    let dlt = Arc::new(batch);
                                    feldera_storage::tokio::TOKIO
                                        .block_on(trace.insert(dlt.clone()));
                                    dlt
                                });
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

    /// Appends `rows` and returns `(Old, Dlt)`: the snapshot before the
    /// append, and the appended run.
    pub fn append(&self, rows: Vec<IndexRow>) -> Result<(Snapshot, Arc<MergedIndexBatch>)> {
        let old = self.request(Request::Snapshot)?;
        let dlt = self.request(|reply| Request::Append(rows, reply))?;
        Ok((old, dlt.map_err(anyhow::Error::msg)?))
    }
}

/// Checks, on the calling thread, that its buffer cache is `runtime`'s
/// foreground cache of worker 0, the view circuit's worker.
fn check_on_runtime_cache(runtime: &Runtime) -> Result<(), String> {
    let thread_cache = Runtime::buffer_cache().ok_or("merged-index thread has no buffer cache")?;
    if Arc::ptr_eq(&thread_cache, &runtime.foreground_buffer_cache(0)) {
        Ok(())
    } else {
        Err("merged-index thread is not on the runtime's buffer cache".to_owned())
    }
}

/// Folds a net delta into the rows of the index's `Dlt`, one per changed row.
pub fn index_rows(net_delta: &Change) -> Result<Vec<IndexRow>> {
    let weight = |insert: bool| if insert { 1 } else { -1 };
    let mut rows = Vec::with_capacity(net_delta.len());
    for (part, insert) in &net_delta.parts {
        rows.push(part_row(part, weight(*insert)));
    }
    for (partsupp, insert) in &net_delta.partsupps {
        rows.push(partsupp_row(partsupp, weight(*insert)));
    }
    for (line, insert) in &net_delta.lines {
        rows.push(line_row(line, weight(*insert))?);
    }
    Ok(rows)
}

fn part_row(part: &Part, weight: ZWeight) -> IndexRow {
    (
        Key::PartByPartkey(PartByPartkeyKey { partkey: part.key }),
        Payload::PartByPartkey(PartByPartkeyPayload {
            p_type: part.part_type.as_str().into(),
        }),
        weight,
    )
}

fn partsupp_row(partsupp: &Partsupp, weight: ZWeight) -> IndexRow {
    (
        Key::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyKey {
            partkey: partsupp.part_key,
            suppkey: partsupp.supplier_key,
        }),
        Payload::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyPayload {
            ps_availqty: partsupp.available,
        }),
        weight,
    )
}

fn line_row(line: &Line, weight: ZWeight) -> Result<IndexRow> {
    Ok((
        Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(
            LineitemByPartkeySuppkeyOrderkeyLinenumberKey {
                partkey: line.part_key,
                suppkey: line.supplier_key,
                orderkey: line.order_key,
                linenumber: line.number,
            },
        ),
        Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(
            LineitemByPartkeySuppkeyOrderkeyLinenumberPayload {
                l_shipdate: serde_json::from_value::<Date>(line.ship_date.as_str().into())
                    .with_context(|| format!("invalid date {}", line.ship_date))?,
                l_extendedprice: decimal(line.price_cents)?,
                l_discount: decimal(line.discount_hundredths)?,
            },
        ),
        weight,
    ))
}

fn decimal<const P: usize, const S: usize>(hundredths: i64) -> Result<SqlDecimal<P, S>> {
    SqlDecimal::<P, S>::new(i128::from(hundredths), 2)
        .context("a decimal outside its SQL precision")
}
