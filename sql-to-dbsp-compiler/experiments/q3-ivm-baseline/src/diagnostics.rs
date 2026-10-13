//! Opt-in storage diagnostics for R5 runs (`--diagnostics <dir>`).
//!
//! The driver turns on DBSP's flush log, compaction log and spine inventory
//! before it builds the circuit, and writes four files into the directory:
//!
//! * `flush-log.jsonl`: one line per backend flush (file, bytes, owner, times).
//! * `compaction-log.jsonl`: one line per run that a spine wrote outside the
//!   circuit's steps (background compaction, requested compaction, or the
//!   spill of an inserted run).
//! * `run-contents.json`: for every live spine at the end of the measured
//!   window, its runs (location, records, negative-weight records, file size,
//!   bytes per block type) and its consolidated row count.
//! * `summary.json`: the window, how the flush log splits around the window,
//!   and whether the flush log matches the backend's counters.
//!
//! Every figure is labelled with the run's mode. Nothing here runs inside the
//! timed window; the end-of-window scan reads storage but writes none, and it
//! happens after the report's counters and disk usage have been read.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use dbsp::storage::diagnostics::{self as dbsp_diagnostics, FlushRecord};
use dbsp::storage::file::block_stats::layer_file_block_bytes;
use dbsp::storage::write_attribution::{write_snapshot, WriteOwner};
use serde::Serialize;
use serde_json::{json, Value};

/// The measured window, in microseconds since the Unix epoch.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub started_micros: u64,
    pub finished_micros: u64,
}

pub struct Diagnostics {
    directory: PathBuf,
    mode: &'static str,
}

impl Diagnostics {
    /// Creates `directory`, which must not exist, and turns the DBSP logs on.
    /// Call before the circuit is built, so that every spine is registered.
    pub fn start(directory: &Path, mode: &'static str) -> Result<Self> {
        if directory.exists() {
            bail!(
                "diagnostics directory already exists: {}",
                directory.display()
            );
        }
        fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
        dbsp_diagnostics::enable();
        Ok(Self {
            directory: directory.to_owned(),
            mode,
        })
    }

    /// Writes `run-contents.json`: the runs of every live spine, with the
    /// bytes each file spends on each block type. `storage_root` is the
    /// backend's directory.
    pub fn write_run_contents(&self, storage_root: &Path) -> Result<()> {
        let mut spines = Vec::new();
        for spine in dbsp_diagnostics::spine_contents() {
            let mut runs = Vec::new();
            for run in &spine.runs {
                let mut value = serde_json::to_value(run)?;
                if let Some(file) = &run.file {
                    let blocks = layer_file_block_bytes(&storage_root.join(file))?;
                    ensure!(
                        Some(blocks.total()) == run.file_bytes,
                        "{file}: block bytes sum to {} but the file has {:?} bytes",
                        blocks.total(),
                        run.file_bytes
                    );
                    value["blocks"] = serde_json::to_value(blocks)?;
                }
                runs.push(value);
            }
            let in_memory = spine.runs.iter().filter(|r| r.location == "memory").count();
            let sum = |field: fn(&dbsp_diagnostics::RunContents) -> u64| -> u64 {
                spine.runs.iter().map(field).sum()
            };
            spines.push(json!({
                "owner": spine.owner,
                "name": spine.name,
                "role": spine.role,
                "consolidated_rows": spine.consolidated_rows,
                "runs_in_memory": in_memory,
                "runs_on_storage": spine.runs.len() - in_memory,
                "records_in_runs": sum(|r| r.records),
                "negative_weight_records_in_runs":
                    spine.runs.iter().filter_map(|r| r.negative_weight_records).sum::<u64>(),
                "file_bytes": sum(|r| r.file_bytes.unwrap_or(0)),
                "runs": runs,
            }));
        }
        // Empty accumulators are the bulk of the registry; keep only spines
        // that hold something at the end of the window.
        spines.retain(|spine| {
            spine["runs"]
                .as_array()
                .is_some_and(|runs| !runs.is_empty())
        });
        self.write_json(
            "run-contents.json",
            &json!({"mode": self.mode, "spines": spines}),
        )
    }

    /// Writes the flush log, the compaction log, and the summary. Call last,
    /// after the run's report: logs include writes that happen after the
    /// window closed.
    pub fn finish(&self, window: Window) -> Result<()> {
        let flushes = dbsp_diagnostics::flush_log();
        let compactions = dbsp_diagnostics::compaction_log();
        self.write_jsonl("flush-log.jsonl", &flushes)?;
        self.write_jsonl("compaction-log.jsonl", &compactions)?;

        let counters = write_snapshot();
        let logged_bytes: u64 = flushes.iter().map(|flush| flush.bytes).sum();
        let mut by_kind = BTreeMap::<String, u64>::new();
        for record in &compactions {
            *by_kind
                .entry(
                    serde_json::to_value(record.kind)?
                        .as_str()
                        .unwrap_or("?")
                        .to_owned(),
                )
                .or_default() += 1;
        }
        self.write_json(
            "summary.json",
            &json!({
                "mode": self.mode,
                "window": {
                    "started_micros": window.started_micros,
                    "finished_micros": window.finished_micros,
                },
                "flush_log": {
                    "records": flushes.len(),
                    "bytes": logged_bytes,
                    "matches_counters": flushes.len() as u64 == counters.total.operations
                        && logged_bytes == counters.total.bytes,
                    "before_window": flush_split(&flushes, |f| f.finished_micros < window.started_micros),
                    "in_window": flush_split(&flushes, |f| {
                        f.finished_micros >= window.started_micros
                            && f.finished_micros <= window.finished_micros
                    }),
                    "after_window": flush_split(&flushes, |f| f.finished_micros > window.finished_micros),
                },
                "compaction_log": {"records": compactions.len(), "by_kind": by_kind},
                "counters_at_exit": counters,
            }),
        )
    }

    fn write_json(&self, name: &str, value: &Value) -> Result<()> {
        let path = self.directory.join(name);
        fs::write(&path, serde_json::to_vec_pretty(value)?)
            .with_context(|| format!("write {}", path.display()))
    }

    fn write_jsonl<T: Serialize>(&self, name: &str, records: &[T]) -> Result<()> {
        let path = self.directory.join(name);
        let mut out = BufWriter::new(
            File::create(&path).with_context(|| format!("create {}", path.display()))?,
        );
        for record in records {
            serde_json::to_writer(&mut out, record)?;
            out.write_all(b"\n")?;
        }
        out.flush()
            .with_context(|| format!("write {}", path.display()))
    }
}

/// Operations and bytes of the flushes that satisfy `keep`, per owner and in
/// total.
fn flush_split(flushes: &[FlushRecord], keep: impl Fn(&FlushRecord) -> bool) -> Value {
    let mut by_owner = BTreeMap::<&'static str, (u64, u64)>::new();
    for owner in WriteOwner::ALL {
        by_owner.insert(owner.name(), (0, 0));
    }
    for flush in flushes.iter().filter(|flush| keep(flush)) {
        let entry = by_owner
            .get_mut(flush.owner.name())
            .expect("every owner is present");
        entry.0 += 1;
        entry.1 += flush.bytes;
    }
    let operations: u64 = by_owner.values().map(|(operations, _)| operations).sum();
    let bytes: u64 = by_owner.values().map(|(_, bytes)| bytes).sum();
    let owners: BTreeMap<_, _> = by_owner
        .into_iter()
        .map(|(name, (operations, bytes))| {
            (name, json!({"operations": operations, "bytes": bytes}))
        })
        .collect();
    json!({"operations": operations, "bytes": bytes, "by_owner": owners})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flush(owner: WriteOwner, bytes: u64, finished_micros: u64) -> FlushRecord {
        FlushRecord {
            file: "file".to_owned(),
            bytes,
            blocks: 1,
            started_micros: finished_micros,
            finished_micros,
            owner,
            succeeded: true,
        }
    }

    #[test]
    fn a_flush_split_counts_each_owner_and_sums_to_its_total() {
        let flushes = [
            flush(WriteOwner::Query, 10, 1),
            flush(WriteOwner::MergedIndex, 20, 2),
            flush(WriteOwner::MergedIndex, 40, 3),
            flush(WriteOwner::BaseRelation, 80, 4),
        ];
        let split = flush_split(&flushes, |f| f.finished_micros >= 2);
        assert_eq!(split["operations"], 3);
        assert_eq!(split["bytes"], 140);
        assert_eq!(
            split["by_owner"]["merged_index"],
            json!({"operations": 2, "bytes": 60})
        );
        assert_eq!(
            split["by_owner"]["query"],
            json!({"operations": 0, "bytes": 0})
        );
    }

    #[test]
    fn an_existing_directory_is_refused() {
        let directory = std::env::temp_dir().join(format!("q3-diagnostics-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let error = Diagnostics::start(&directory, "native").err().unwrap();
        assert!(error.to_string().contains("already exists"));
        fs::remove_dir_all(&directory).unwrap();
    }
}
