//! Per-refresh counters for the compaction study (measurement version 15): the runs every spine
//! holds after each transaction, the merges between them, and the device reads by thread and
//! owner.
//!
//! The samples are taken after each transaction's timers stop, and the driver subtracts the time
//! they take from the refresh's foreground time, so they change no reported latency.
//!
//! Spines are followed by their registration id. The spines alive at the refresh's start are the
//! persistent state (base tables, join traces, `V`, the merged index); spines that appear later
//! and are gone by the next sample are transient (created and dropped within a transaction or
//! between two), and are reported apart, as counts only.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use dbsp::storage::attribution::{read_snapshot, Component, ReadCounts, ReadSnapshot, ReadThread};
use dbsp::storage::diagnostics::{spine_run_counts, SpineRunCounts};
use serde_json::{json, Map, Value};

/// The run counts of one refresh: one sample per transaction, plus the spines' state at the
/// refresh's start and end.
pub(crate) struct RunCountSampler {
    /// The spines alive at the start, by id.
    start: BTreeMap<u64, SpineRunCounts>,
    /// `runs[id]`: a starting spine's runs after each transaction, while it lives.
    runs: BTreeMap<u64, Vec<usize>>,
    /// The starting spines' total runs after each transaction.
    total_runs: Vec<usize>,
    /// Spines that were not alive at the start, seen in some sample.
    transient_ids: BTreeSet<u64>,
    /// The transient spines' runs after each transaction.
    transient_runs: Vec<usize>,
    reads_before: ReadSnapshot,
    /// Time spent sampling, which the caller removes from the refresh's foreground time.
    pub(crate) sampling: Duration,
}

impl RunCountSampler {
    /// Takes the refresh's starting state.
    pub(crate) fn start(transactions: usize) -> Self {
        let start: BTreeMap<u64, SpineRunCounts> = spine_run_counts()
            .into_iter()
            .map(|spine| (spine.id, spine))
            .collect();
        Self {
            runs: start
                .keys()
                .map(|id| (*id, Vec::with_capacity(transactions)))
                .collect(),
            start,
            total_runs: Vec::with_capacity(transactions),
            transient_ids: BTreeSet::new(),
            transient_runs: Vec::with_capacity(transactions),
            reads_before: read_snapshot(),
            sampling: Duration::ZERO,
        }
    }

    /// Samples every spine after a transaction.
    pub(crate) fn sample(&mut self) {
        let started = Instant::now();
        let (mut total, mut transient) = (0, 0);
        for spine in spine_run_counts() {
            match self.runs.get_mut(&spine.id) {
                Some(samples) => {
                    samples.push(spine.runs());
                    total += spine.runs();
                }
                None => {
                    self.transient_ids.insert(spine.id);
                    transient += spine.runs();
                }
            }
        }
        self.total_runs.push(total);
        self.transient_runs.push(transient);
        self.sampling += started.elapsed();
    }

    /// The refresh's report: per starting spine, the runs after each transaction (minimum,
    /// median, maximum), the levels at the end, and the merges during the refresh; the total
    /// runs; the transient spines; and the device reads.
    pub(crate) fn json(self) -> Value {
        let end: BTreeMap<u64, SpineRunCounts> = spine_run_counts()
            .into_iter()
            .map(|spine| (spine.id, spine))
            .collect();
        let spines: Vec<Value> = self
            .start
            .iter()
            .map(|(id, start)| {
                let runs = &self.runs[id];
                match end.get(id) {
                    Some(end) => json!({
                        "owner": end.owner.name(), "name": end.name, "alive_at_end": true,
                        "runs_after_transaction": spread(runs),
                        "runs_at_start": start.runs(), "runs_at_end": end.runs(),
                        "storage_runs_at_end": end.storage_runs,
                        "loose_by_level_at_end": end.loose_by_level,
                        "merging_by_level_at_end": end.merging_by_level,
                        "completed_merges": end.completed_merges - start.completed_merges,
                        "merge_real_ms": (end.merge_real_micros - start.merge_real_micros) as f64 / 1000.0,
                        "merge_cpu_ms": (end.merge_cpu_micros - start.merge_cpu_micros) as f64 / 1000.0,
                        // The memtable is not a run: its tuples and bytes are reported apart.
                        "memtable_entries_at_end": end.memtable_entries,
                        "memtable_bytes_at_end": end.memtable_bytes,
                        "memtable_flushes": end.memtable_flushes - start.memtable_flushes,
                        "memtable_node_copies": end.memtable_node_copies - start.memtable_node_copies,
                        "memtable_copy_ms": (end.memtable_copy_nanos - start.memtable_copy_nanos) as f64 / 1e6,
                    }),
                    None => json!({
                        "owner": start.owner.name(), "name": start.name, "alive_at_end": false,
                        "runs_after_transaction": spread(runs), "runs_at_start": start.runs(),
                    }),
                }
            })
            .collect();
        json!({
            "note": "Spines alive at the refresh's start, followed by registration id: runs after each transaction (loose and merging; minimum, median, maximum), sampled after the transaction's timers, and the merges completed during the refresh with their real and CPU time on the merger threads. Transient spines are those created during the refresh; only their number and their runs are counted. Device reads are every storage read of the process during the refresh, split by the reading thread (foreground worker, background merger, other) and once by its read owner and once by its write owner.",
            "spines": spines,
            "total_runs_after_transaction": spread(&self.total_runs),
            "transient_spines": self.transient_ids.len(),
            "transient_runs_after_transaction": spread(&self.transient_runs),
            "device_reads": reads_json(&read_snapshot().since(&self.reads_before)),
        })
    }
}

/// The minimum, median (nearest rank), and maximum of `values`; nulls when there are none.
fn spread(values: &[usize]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    match (sorted.first(), sorted.last()) {
        (Some(min), Some(max)) => {
            let median = sorted[sorted.len().div_ceil(2) - 1];
            json!({"min": min, "median": median, "max": max, "samples": sorted.len()})
        }
        _ => json!({"min": null, "median": null, "max": null, "samples": 0}),
    }
}

fn reads_json(reads: &ReadSnapshot) -> Value {
    let split = |by: &[[ReadCounts; 5]; 3]| {
        let mut threads = Map::new();
        for (thread, owners) in ReadThread::ALL.iter().zip(by) {
            let mut row = Map::new();
            for (owner, counts) in Component::ALL.iter().zip(owners) {
                if counts.operations > 0 {
                    row.insert(
                        owner.name().to_owned(),
                        json!({"operations": counts.operations, "bytes": counts.bytes}),
                    );
                }
            }
            threads.insert(thread_name(*thread).to_owned(), Value::Object(row));
        }
        Value::Object(threads)
    };
    json!({
        "by_read_owner": split(&reads.by_read_owner),
        "by_write_owner": split(&reads.by_write_owner),
    })
}

fn thread_name(thread: ReadThread) -> &'static str {
    match thread {
        ReadThread::Foreground => "foreground",
        ReadThread::Background => "background",
        ReadThread::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::spread;
    use serde_json::json;

    #[test]
    fn spread_reports_nearest_rank_median() {
        assert_eq!(
            spread(&[5, 1, 3, 2]),
            json!({"min": 1, "median": 2, "max": 5, "samples": 4})
        );
        assert_eq!(spread(&[7])["median"], 7);
        assert_eq!(spread(&[])["samples"], 0);
    }
}
