//! Per-refresh counters for the compaction study (measurement version 15): the runs every spine
//! holds after each transaction, the merges between them, and the device reads by thread and
//! owner.
//!
//! The samples are taken after each transaction's timers stop, and the driver subtracts the time
//! they take from the refresh's foreground time, so they change no reported latency.

use std::time::{Duration, Instant};

use dbsp::storage::attribution::{read_snapshot, Component, ReadSnapshot, ReadThread};
use dbsp::storage::diagnostics::{spine_run_counts, SpineRunCounts};
use serde_json::{json, Map, Value};

/// The run counts of one refresh: one sample per transaction, plus the spines' state at the
/// refresh's start and end.
pub(crate) struct RunCountSampler {
    start: Vec<SpineRunCounts>,
    /// `runs[spine][transaction]`: the spine's runs after the transaction.
    runs: Vec<Vec<usize>>,
    /// The spines' total runs after each transaction.
    total_runs: Vec<usize>,
    reads_before: ReadSnapshot,
    /// A transaction after which the spines were not the ones at the start: their counts are not
    /// comparable from then on, and the report says so.
    changed_after: Option<usize>,
    /// Time spent sampling, which the caller removes from the refresh's foreground time.
    pub(crate) sampling: Duration,
}

impl RunCountSampler {
    /// Takes the refresh's starting state.
    pub(crate) fn start(transactions: usize) -> Self {
        let start = spine_run_counts();
        Self {
            runs: start
                .iter()
                .map(|_| Vec::with_capacity(transactions))
                .collect(),
            start,
            total_runs: Vec::with_capacity(transactions),
            reads_before: read_snapshot(),
            changed_after: None,
            sampling: Duration::ZERO,
        }
    }

    /// Samples every spine after a transaction.
    pub(crate) fn sample(&mut self) {
        let started = Instant::now();
        let now = spine_run_counts();
        if self.changed_after.is_none() && !same_spines(&self.start, &now) {
            self.changed_after = Some(self.total_runs.len());
        }
        if self.changed_after.is_none() {
            for (samples, spine) in self.runs.iter_mut().zip(&now) {
                samples.push(spine.runs());
            }
        }
        self.total_runs
            .push(now.iter().map(SpineRunCounts::runs).sum());
        self.sampling += started.elapsed();
    }

    /// The refresh's report: per spine, the runs after each transaction (minimum, median,
    /// maximum), the levels at the end, and the merges during the refresh; the total runs; and
    /// the device reads.
    pub(crate) fn json(self) -> Value {
        let end = spine_run_counts();
        let spines: Vec<Value> = if same_spines(&self.start, &end) {
            self.start
                .iter()
                .zip(&end)
                .zip(&self.runs)
                .map(|((start, end), runs)| {
                    json!({
                        "owner": end.owner.name(), "name": end.name,
                        "runs_after_transaction": spread(runs),
                        "runs_at_start": start.runs(), "runs_at_end": end.runs(),
                        "storage_runs_at_end": end.storage_runs,
                        "loose_by_level_at_end": end.loose_by_level,
                        "merging_by_level_at_end": end.merging_by_level,
                        "completed_merges": end.completed_merges - start.completed_merges,
                        "merge_real_ms": (end.merge_real_micros - start.merge_real_micros) as f64 / 1000.0,
                        "merge_cpu_ms": (end.merge_cpu_micros - start.merge_cpu_micros) as f64 / 1000.0,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        json!({
            "note": "Runs per spine after each transaction (loose and merging; minimum, median, maximum), sampled after the transaction's timers; merges are the ones completed during the refresh, with their real and CPU time on the merger threads. Device reads are every storage read of the process during the refresh, split by the reading thread (foreground worker, background merger, other) and once by its read owner and once by its write owner.",
            "spines": spines,
            "spines_changed_after_transaction": self.changed_after,
            "total_runs_after_transaction": spread(&self.total_runs),
            "device_reads": reads_json(&read_snapshot().since(&self.reads_before)),
        })
    }
}

/// Whether two samples list the same spines in the same order.
fn same_spines(a: &[SpineRunCounts], b: &[SpineRunCounts]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.owner == b.owner && a.name == b.name)
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
    let split = |by: &[[dbsp::storage::attribution::ReadCounts; 5]; 3]| {
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
