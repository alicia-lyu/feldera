//! The driver: loads dbgen's base files, then runs a list of refreshes, in the
//! mode the build selects. Loading is setup; the refreshes are what a later
//! measurement times. With verification on, `V` is checked against
//! `Reference` after loading and after each refresh.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use dbsp::circuit::{
    CircuitConfig, CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions,
};
use dbsp::trace::merged_index::Pass;

use crate::data::{difference, Change, Reference};
use crate::pipeline::Pipeline;
use crate::workload::{self, BaseFiles, Refresh};

const USAGE: &str = "usage: q14-ivm-baseline --part FILE --partsupp FILE --lineitem FILE \
    --refresh-list FILE [--batch ROWS] [--storage-path DIR] [--cache-bytes BYTES] \
    [--memory-target-bytes BYTES] [--session-pass seek|step] [--no-verify]";

#[derive(Clone, Debug)]
pub struct Config {
    pub base: BaseFiles,
    pub refresh_list: PathBuf,
    /// The most rows in one loading transaction.
    pub batch: usize,
    /// A new directory for file-backed state; without it, state is in memory.
    pub storage_path: Option<PathBuf>,
    /// The buffer cache's size, a whole number of MiB.
    pub cache_bytes: Option<usize>,
    /// The memory target, from which DBSP derives its memory pressure.
    pub memory_target_bytes: Option<u64>,
    /// How the scan session passes untouched groups; native mode ignores it.
    pub session_pass: Pass,
    /// Whether to check `V` against `Reference`, which holds every base row.
    pub verify: bool,
}

impl Config {
    pub fn from_args(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let (mut part, mut partsupp, mut lineitem, mut refresh_list) = (None, None, None, None);
        let mut config = Self {
            base: BaseFiles {
                part: PathBuf::new(),
                partsupp: PathBuf::new(),
                lineitem: PathBuf::new(),
            },
            refresh_list: PathBuf::new(),
            batch: 10_000,
            storage_path: None,
            cache_bytes: None,
            memory_target_bytes: None,
            session_pass: Pass::Seek,
            verify: true,
        };
        let mut arguments = arguments.into_iter();
        while let Some(flag) = arguments.next() {
            if flag == "--no-verify" {
                config.verify = false;
                continue;
            }
            let value = arguments
                .next()
                .with_context(|| format!("{flag} needs a value\n{USAGE}"))?;
            match flag.as_str() {
                "--part" => part = Some(value.into()),
                "--partsupp" => partsupp = Some(value.into()),
                "--lineitem" => lineitem = Some(value.into()),
                "--refresh-list" => refresh_list = Some(value.into()),
                "--batch" => config.batch = value.parse().context("--batch")?,
                "--storage-path" => config.storage_path = Some(value.into()),
                "--cache-bytes" => {
                    config.cache_bytes = Some(value.parse().context("--cache-bytes")?)
                }
                "--memory-target-bytes" => {
                    config.memory_target_bytes =
                        Some(value.parse().context("--memory-target-bytes")?)
                }
                "--session-pass" => {
                    config.session_pass = match value.as_str() {
                        "seek" => Pass::Seek,
                        "step" => Pass::Step,
                        _ => bail!("--session-pass is seek or step, not {value}"),
                    }
                }
                _ => bail!("unknown flag {flag}\n{USAGE}"),
            }
        }
        let required = |path: Option<PathBuf>, flag: &str| {
            path.with_context(|| format!("{flag} is required\n{USAGE}"))
        };
        config.base = BaseFiles {
            part: required(part, "--part")?,
            partsupp: required(partsupp, "--partsupp")?,
            lineitem: required(lineitem, "--lineitem")?,
        };
        config.refresh_list = required(refresh_list, "--refresh-list")?;
        if config.batch == 0 {
            bail!("--batch must be positive");
        }
        if config
            .cache_bytes
            .is_some_and(|bytes| bytes == 0 || bytes % MIB != 0)
        {
            bail!("--cache-bytes must be a positive whole number of MiB");
        }
        if config.memory_target_bytes == Some(0) {
            bail!("--memory-target-bytes must be positive");
        }
        if config.cache_bytes.is_some() && config.storage_path.is_none() {
            bail!("--cache-bytes needs --storage-path");
        }
        Ok(config)
    }

    /// The runtime's configuration: one worker (user decision of 2026-10-12)
    /// and, with a storage path, one storage directory and one buffer cache.
    fn circuit_config(&self) -> Result<CircuitConfig> {
        let mut circuit_config =
            CircuitConfig::with_workers(1).with_max_rss_bytes(self.memory_target_bytes);
        // Native joins probe their traces through run cursors, as the
        // reconstructed readers do (Q3 decision of 2026-11-04).
        circuit_config.dev_tweaks.fetch_join = Some(false);
        if let Some(path) = &self.storage_path {
            if path.exists() {
                bail!("storage directory already exists: {}", path.display());
            }
            std::fs::create_dir_all(path)?;
            // DBSP's default run placement (user decision of 2026-11-09):
            // `min_storage_bytes` and `min_step_storage_bytes` stay unset.
            let options = StorageOptions {
                cache_mib: self.cache_bytes.map(|bytes| bytes / MIB),
                compression: feldera_types::config::StorageCompression::None,
                ..StorageOptions::default()
            };
            let storage = StorageConfig {
                path: path.to_string_lossy().into_owned(),
                cache: StorageCacheConfig::FelderaCache,
            };
            circuit_config = circuit_config
                .with_storage(Some(CircuitStorageConfig::for_config(storage, options)?));
        }
        Ok(circuit_config)
    }
}

const MIB: usize = 1024 * 1024;

/// What one refresh did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshSummary {
    pub phase: &'static str,
    /// The rows of the refresh's change: line items inserted or deleted.
    pub rows: usize,
    /// The rows that `V` gained and lost.
    pub view_inserted: i64,
    pub view_deleted: i64,
    /// `V`'s rows after the refresh.
    pub view_rows: i64,
}

/// Loads the base files, then runs the refreshes in order. Prints one line
/// for loading and one per refresh, and returns the refreshes' summaries.
pub fn run(config: &Config) -> Result<Vec<RefreshSummary>> {
    let mut pipeline = Pipeline::new(config.circuit_config()?)?;
    #[cfg(feature = "merged-index")]
    pipeline.set_session_pass(config.session_pass);
    let mut reference = config.verify.then(Reference::default);
    if config.verify {
        pipeline.enable_contract_check();
    }

    let mut transactions = 0;
    let rows = workload::load(&config.base, config.batch, |change| {
        if let Some(reference) = &mut reference {
            reference.apply(&change);
        }
        pipeline.transact(change, false)?;
        transactions += 1;
        Ok(())
    })?;
    let (_, view) = pipeline.view_output()?;
    if let Some(reference) = &reference {
        if view != reference.evaluate() {
            bail!("after loading, V differs from the reference");
        }
    }
    println!(
        "phase=load rows={rows} transactions={transactions} view_rows={} verified={}",
        view.values().sum::<i64>(),
        config.verify
    );

    // Every refresh file is read before the first refresh runs.
    let refreshes = workload::read_refreshes(&config.refresh_list)?;
    let mut summaries = Vec::with_capacity(refreshes.len());
    for (number, refresh) in refreshes.into_iter().enumerate() {
        let phase = refresh.phase();
        let change = match refresh {
            Refresh::Rf1(lines) => Change {
                lines: lines.into_iter().map(|line| (line, true)).collect(),
                ..Change::default()
            },
            Refresh::Rf2(order_keys) => Change {
                lines: pipeline
                    .order_lines(&order_keys)?
                    .into_iter()
                    .map(|line| (line, false))
                    .collect(),
                ..Change::default()
            },
        };
        let rows = change.len();
        let before = reference.as_ref().map(Reference::evaluate);
        if let Some(reference) = &mut reference {
            reference.apply(&change);
        }
        pipeline.transact(change, false)?;
        let (delta, view) = pipeline.view_output()?;
        if let (Some(reference), Some(before)) = (&reference, &before) {
            let after = reference.evaluate();
            if delta != difference(before, &after) || view != after {
                bail!("refresh {}: V differs from the reference", number + 1);
            }
        }
        let summary = RefreshSummary {
            phase,
            rows,
            view_inserted: delta.values().filter(|w| **w > 0).sum(),
            view_deleted: -delta.values().filter(|w| **w < 0).sum::<i64>(),
            view_rows: view.values().sum(),
        };
        println!(
            "refresh={} phase={phase} rows={rows} view_inserted={} view_deleted={} view_rows={}",
            number + 1,
            summary.view_inserted,
            summary.view_deleted,
            summary.view_rows
        );
        summaries.push(summary);
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn arguments(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    const REQUIRED: &str = "--part p --partsupp ps --lineitem l --refresh-list r";

    #[test]
    fn the_command_line_takes_the_files_and_checks_the_numbers() -> Result<()> {
        let config = Config::from_args(arguments(&format!(
            "{REQUIRED} --batch 5 --storage-path s --cache-bytes 2097152 \
             --memory-target-bytes 7 --session-pass step --no-verify"
        )))?;
        assert_eq!(config.base.lineitem, Path::new("l"));
        assert_eq!(config.batch, 5);
        assert_eq!(config.cache_bytes, Some(2 * MIB));
        assert_eq!(config.memory_target_bytes, Some(7));
        assert_eq!(config.session_pass, Pass::Step);
        assert!(!config.verify);
        assert!(Config::from_args(arguments(REQUIRED))?.verify);

        for bad in [
            "--part p --partsupp ps --lineitem l",
            &format!("{REQUIRED} --batch 0"),
            &format!("{REQUIRED} --batch"),
            &format!("{REQUIRED} --storage-path s --cache-bytes 1000"),
            &format!("{REQUIRED} --cache-bytes 2097152"),
            &format!("{REQUIRED} --memory-target-bytes 0"),
            &format!("{REQUIRED} --session-pass skip"),
            &format!("{REQUIRED} --workers 2"),
        ] {
            assert!(Config::from_args(arguments(bad)).is_err(), "{bad}");
        }
        Ok(())
    }

    fn line(order: i64, number: i32, part: i64) -> String {
        format!("{order}|{part}|10|{number}|1|10.00|0.05|0.01|N|O|1996-01-02|d|d|i|m|c|\n")
    }

    fn write(directory: &Path, name: &str, text: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Three parts (part 3 has no line items) and four orders; RF1 adds order
    /// 5 and a line item of absent part 9; RF2 deletes orders 1 and 5 and
    /// absent order 7. Loading takes three transactions.
    #[test]
    fn a_run_loads_and_refreshes_in_agreement_with_the_reference() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        let lines: String = [(1, 1, 1), (1, 2, 2), (2, 1, 1), (3, 1, 2), (4, 1, 1)]
            .into_iter()
            .map(|(order, number, part)| line(order, number, part))
            .collect();
        write(path, "lineitem.tbl.u1", &(line(5, 1, 2) + &line(5, 2, 9)));
        write(path, "delete.1", "1|\n5|\n7|\n");
        let config = Config {
            base: BaseFiles {
                part: write(
                    path,
                    "part.tbl",
                    "1|n|m|b|A|1|c|1.00|x|\n2|n|m|b|B|1|c|1.00|x|\n3|n|m|b|C|1|c|1.00|x|\n",
                ),
                partsupp: write(path, "partsupp.tbl", "1|10|5|1.00|x|\n2|10|5|1.00|x|\n"),
                lineitem: write(path, "lineitem.tbl", &lines),
            },
            refresh_list: write(
                path,
                "list.json",
                r#"[{"rf1_lines": "lineitem.tbl.u1"}, {"rf2": "delete.1"}]"#,
            ),
            batch: 4,
            storage_path: Some(path.join("storage")),
            cache_bytes: Some(4 * MIB),
            memory_target_bytes: None,
            session_pass: Pass::Seek,
            verify: true,
        };
        let summaries = run(&config)?;
        assert_eq!(
            summaries,
            [
                RefreshSummary {
                    phase: "rf1",
                    rows: 2,
                    view_inserted: 1,
                    view_deleted: 0,
                    view_rows: 6,
                },
                RefreshSummary {
                    phase: "rf2",
                    rows: 4,
                    view_inserted: 0,
                    view_deleted: 3,
                    view_rows: 3,
                },
            ]
        );
        // A second run may not reuse the storage directory.
        assert!(run(&config).is_err());
        Ok(())
    }
}
