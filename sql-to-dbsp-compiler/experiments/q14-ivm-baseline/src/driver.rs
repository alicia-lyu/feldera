//! The driver: loads dbgen's base files, then runs a list of refreshes, in the
//! mode the build selects, and times each refresh's phases (`Timings`).
//! Loading is setup and is not measured. With verification on, `V` is checked
//! against `Reference` after loading and after each refresh, outside the
//! timers; a measured run passes `--no-verify`.
//!
//! As in the Q3 driver, the work can be split into two programs that meet at
//! an image, a checkpoint of the loaded state:
//!
//! * The load program (`--write-image DIR`) loads, waits for the background
//!   merges, and writes the image into the new directory `DIR`: its storage in
//!   `DIR/storage` and its manifest in `DIR/image.json`.
//! * The refresh program (`--from-image DIR`) reopens the image in place, so
//!   `DIR` must be the run's own copy, and runs only the refreshes. The window
//!   starts with an empty buffer cache.
//!
//! In every program the refreshes evict the base tables' blocks from the
//! buffer cache after each base transaction (`Pipeline::set_evict_base`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dbsp::circuit::{
    CircuitConfig, CircuitStorageConfig, StorageCacheConfig, StorageConfig, StorageOptions,
};
use dbsp::storage::attribution::{read_snapshot, Component, ReadSnapshot, ReadThread};
use dbsp::trace::merged_index::{Pass, SessionReads};
use feldera_types::checkpoint::CheckpointMetadata;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::data::{difference, Change, Reference};
use crate::pipeline::{Pipeline, Timings};
use crate::workload::{self, BaseFiles, Refresh};

const USAGE: &str = "usage: q14-ivm-baseline \
    [--part FILE --partsupp FILE --lineitem FILE | --from-image DIR] \
    [--refresh-list FILE | --write-image DIR] [--batch ROWS] [--storage-path DIR] \
    [--cache-bytes BYTES] [--memory-target-bytes BYTES] [--session-pass seek|step] \
    [--front-code-byte-keys] [--elide-value-headers] [--report FILE] [--no-verify]";

#[derive(Clone, Debug)]
pub struct Config {
    /// The base files to load; the refresh program takes none.
    pub base: Option<BaseFiles>,
    /// The refreshes to run; the load program takes none.
    pub refresh_list: Option<PathBuf>,
    /// The most rows in one loading transaction.
    pub batch: usize,
    /// A new directory for file-backed state; without it, state is in memory.
    /// The two image programs use the image's directory instead.
    pub storage_path: Option<PathBuf>,
    /// The buffer cache's size, a whole number of MiB.
    pub cache_bytes: Option<usize>,
    /// The memory target, from which DBSP derives its memory pressure.
    pub memory_target_bytes: Option<u64>,
    /// How the scan session passes untouched groups; native mode ignores it.
    pub session_pass: Pass,
    /// Prefix truncation of the storage files' byte keys.
    pub front_code_byte_keys: bool,
    /// Elision of the storage files' value headers.
    pub elide_value_headers: bool,
    /// Load, write the loaded state as an image to this new directory, and
    /// stop: the load program.
    pub write_image: Option<PathBuf>,
    /// Reopen the image in this directory and run only the refreshes: the
    /// refresh program.
    pub from_image: Option<PathBuf>,
    /// Whether to check `V` against `Reference`, which holds every base row.
    pub verify: bool,
    /// Where to write the JSON report.
    pub report: Option<PathBuf>,
}

impl Config {
    pub fn from_args(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let (mut part, mut partsupp, mut lineitem) = (None, None, None);
        let mut config = Self {
            base: None,
            refresh_list: None,
            batch: 10_000,
            storage_path: None,
            cache_bytes: None,
            memory_target_bytes: None,
            session_pass: Pass::Seek,
            front_code_byte_keys: false,
            elide_value_headers: false,
            write_image: None,
            from_image: None,
            verify: true,
            report: None,
        };
        let mut arguments = arguments.into_iter();
        while let Some(flag) = arguments.next() {
            match flag.as_str() {
                "--no-verify" => config.verify = false,
                "--front-code-byte-keys" => config.front_code_byte_keys = true,
                "--elide-value-headers" => config.elide_value_headers = true,
                _ => {
                    let value = arguments
                        .next()
                        .with_context(|| format!("{flag} needs a value\n{USAGE}"))?;
                    config.set(&flag, value, [&mut part, &mut partsupp, &mut lineitem])?;
                }
            }
        }
        config.base = match (part, partsupp, lineitem) {
            (Some(part), Some(partsupp), Some(lineitem)) => Some(BaseFiles {
                part,
                partsupp,
                lineitem,
            }),
            (None, None, None) => None,
            _ => bail!("--part, --partsupp and --lineitem go together\n{USAGE}"),
        };
        config.check()?;
        Ok(config)
    }

    /// Sets the value of `flag`; `base` holds the three base files' slots.
    fn set(&mut self, flag: &str, value: String, base: [&mut Option<PathBuf>; 3]) -> Result<()> {
        let [part, partsupp, lineitem] = base;
        match flag {
            "--part" => *part = Some(value.into()),
            "--partsupp" => *partsupp = Some(value.into()),
            "--lineitem" => *lineitem = Some(value.into()),
            "--refresh-list" => self.refresh_list = Some(value.into()),
            "--batch" => self.batch = value.parse().context("--batch")?,
            "--storage-path" => self.storage_path = Some(value.into()),
            "--write-image" => self.write_image = Some(value.into()),
            "--from-image" => self.from_image = Some(value.into()),
            "--report" => self.report = Some(value.into()),
            "--cache-bytes" => self.cache_bytes = Some(value.parse().context("--cache-bytes")?),
            "--memory-target-bytes" => {
                self.memory_target_bytes = Some(value.parse().context("--memory-target-bytes")?)
            }
            "--session-pass" => {
                self.session_pass = match value.as_str() {
                    "seek" => Pass::Seek,
                    "step" => Pass::Step,
                    _ => bail!("--session-pass is seek or step, not {value}"),
                }
            }
            _ => bail!("unknown flag {flag}\n{USAGE}"),
        }
        Ok(())
    }

    fn check(&self) -> Result<()> {
        if self.batch == 0 {
            bail!("--batch must be positive");
        }
        if self
            .cache_bytes
            .is_some_and(|bytes| bytes == 0 || bytes % MIB != 0)
        {
            bail!("--cache-bytes must be a positive whole number of MiB");
        }
        if self.memory_target_bytes == Some(0) {
            bail!("--memory-target-bytes must be positive");
        }
        let file_backed =
            self.storage_path.is_some() || self.write_image.is_some() || self.from_image.is_some();
        if self.cache_bytes.is_some() && !file_backed {
            bail!("--cache-bytes needs file-backed state");
        }
        match (&self.write_image, &self.from_image) {
            (Some(_), Some(_)) => {
                bail!(
                    "--write-image and --from-image are the load and the refresh program; pass one"
                )
            }
            (Some(_), None) => {
                if self.base.is_none() || self.refresh_list.is_some() || self.storage_path.is_some()
                {
                    bail!(
                        "--write-image loads only: it takes the base files, no --refresh-list, \
                         and no --storage-path (the image holds the storage)"
                    );
                }
            }
            (None, Some(_)) => {
                if self.base.is_some()
                    || self.refresh_list.is_none()
                    || self.storage_path.is_some()
                    || self.verify
                {
                    bail!(
                        "--from-image refreshes only: it takes --refresh-list, no base files, \
                         no --storage-path (the image holds the state), and --no-verify \
                         (verification needs the loaded rows)"
                    );
                }
            }
            (None, None) => {
                if self.base.is_none() || self.refresh_list.is_none() {
                    bail!("the base files and --refresh-list are required\n{USAGE}");
                }
            }
        }
        Ok(())
    }

    /// The runtime's configuration: one worker (user decision of 2026-10-12)
    /// and, with `storage`, one storage directory and one buffer cache.
    fn circuit_config(&self, storage: Option<&Path>) -> Result<CircuitConfig> {
        let mut circuit_config =
            CircuitConfig::with_workers(1).with_max_rss_bytes(self.memory_target_bytes);
        // Native joins probe their traces through run cursors, as the
        // reconstructed readers do (Q3 decision of 2026-11-04).
        circuit_config.dev_tweaks.fetch_join = Some(false);
        circuit_config.dev_tweaks.front_code_byte_keys = Some(self.front_code_byte_keys);
        circuit_config.dev_tweaks.elide_value_headers = Some(self.elide_value_headers);
        if let Some(path) = storage {
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

/// What an image's storage was written with. The refresh program runs only
/// with the same settings, so that its writes use the image's format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSettings {
    mode: String,
    front_code_byte_keys: bool,
    elide_value_headers: bool,
}

impl ImageSettings {
    fn of(config: &Config) -> Self {
        Self {
            mode: mode().to_owned(),
            front_code_byte_keys: config.front_code_byte_keys,
            elide_value_headers: config.elide_value_headers,
        }
    }
}

/// `image.json`, beside an image's `storage/` directory: what the load
/// program loaded and the checkpoint that holds it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageManifest {
    /// [`ImageManifest::FORMAT`] when written.
    format: u32,
    settings: ImageSettings,
    /// The published checkpoint of both circuits, and of the merged index in
    /// reconstructed mode.
    checkpoint: CheckpointMetadata,
    inputs: BaseFiles,
    /// The load's record (`LoadRecord`); loading is not measured.
    load: LoadRecord,
}

impl ImageManifest {
    /// The image format. Bump it whenever a change alters what an image
    /// holds: the circuits, the index, the load loop, or the checkpoint.
    const FORMAT: u32 = 1;
    const FILE: &'static str = "image.json";
    const STORAGE: &'static str = "storage";

    fn read(image: &Path) -> Result<Self> {
        let path = image.join(Self::FILE);
        let manifest: Self = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
        )
        .with_context(|| format!("parsing {}", path.display()))?;
        if manifest.format != Self::FORMAT {
            bail!(
                "{}: unsupported image format {}",
                path.display(),
                manifest.format
            );
        }
        Ok(manifest)
    }

    /// Fails unless the image was written with the settings that `config`
    /// refreshes with.
    fn check(&self, config: &Config) -> Result<()> {
        let wanted = ImageSettings::of(config);
        if self.settings != wanted {
            bail!(
                "the image was written with {:?}, but this refresh runs with {wanted:?}",
                self.settings
            );
        }
        Ok(())
    }
}

/// What loading did. It is setup, recorded but not measured.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadRecord {
    rows: usize,
    transactions: usize,
    /// `V`'s rows after loading.
    view_rows: i64,
    elapsed_ms: f64,
    background_merge_wait_ms: f64,
    /// The storage directory after the merges: `{bytes, files}`.
    observed_storage: Value,
}

/// What one refresh changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefreshSummary {
    pub phase: &'static str,
    /// The rows of the refresh's change: line items inserted or deleted.
    pub rows: usize,
    /// The rows that `V` gained and lost.
    pub view_inserted: i64,
    pub view_deleted: i64,
    /// `V`'s rows after the refresh: the load's, plus every refresh's
    /// change. A measured run does not read `V`, which is as large as
    /// Lineitem; a verified run checks `V` itself against the reference.
    pub view_rows: i64,
}

/// One refresh: what it changed, and how long its phases took.
#[derive(Clone, Debug)]
pub struct RefreshReport {
    pub summary: RefreshSummary,
    /// Finding RF2's line items in the base circuit's Lineitem; zero for RF1.
    pub discovery: Duration,
    pub timings: Timings,
    /// The refresh's wall time: discovery and every phase, but no
    /// verification.
    pub total: Duration,
    /// The buffer cache's cost that the base eviction freed.
    pub base_evicted_bytes: usize,
    /// The process's storage reads during the refresh, per reading thread and
    /// read owner (`device_reads_json`).
    pub device_reads: Value,
    /// What the scan session read from the index (reconstructed mode).
    pub session_reads: Option<SessionReads>,
    /// DBSP's memory pressure before and after the refresh.
    pub memory_pressure: Value,
}

impl RefreshReport {
    /// The report's record of refresh `number`. `base_ms` includes
    /// `index_ms`, and `ivm_ms` is `delta_ms + q14_ms`; base and IVM are
    /// always reported side by side.
    fn json(&self, number: usize) -> Value {
        let (summary, timings) = (&self.summary, &self.timings);
        json!({
            "refresh": number,
            "phase": summary.phase,
            "rows": summary.rows,
            "view_inserted": summary.view_inserted,
            "view_deleted": summary.view_deleted,
            "view_rows": summary.view_rows,
            "total_ms": milliseconds(self.total),
            "discovery_ms": milliseconds(self.discovery),
            "preparation_ms": milliseconds(timings.preparation),
            "base_ms": milliseconds(timings.base()),
            "index_ms": milliseconds(timings.index),
            "forward_ms": milliseconds(timings.forward),
            "base_eviction_ms": milliseconds(timings.base_eviction),
            "base_evicted_bytes": self.base_evicted_bytes,
            "ivm_ms": milliseconds(timings.ivm()),
            "delta_ms": milliseconds(timings.delta),
            "q14_ms": milliseconds(timings.view),
            "publication_ms": milliseconds(timings.publication),
            "memory_pressure": self.memory_pressure,
            "device_reads": self.device_reads,
            "session": self.session_reads.map(|reads| json!({
                "counts": reads.counts,
                "index_run_reads": reads.run_reads,
            })),
        })
    }
}

/// Storage reads, the nonzero ones, per reading thread (worker, merger, or
/// other, such as the index's and the driver's threads) and read owner.
fn device_reads_json(reads: &ReadSnapshot) -> Value {
    let threads = ["foreground", "background", "other"];
    debug_assert_eq!(threads.len(), ReadThread::ALL.len());
    let mut by_thread = serde_json::Map::new();
    for (thread, owners) in threads.into_iter().zip(&reads.by_read_owner) {
        let owners: serde_json::Map<_, _> = Component::ALL
            .iter()
            .zip(owners)
            .filter(|(_, counts)| counts.operations > 0)
            .map(|(owner, counts)| {
                (
                    owner.name().to_owned(),
                    json!({"operations": counts.operations, "bytes": counts.bytes}),
                )
            })
            .collect();
        by_thread.insert(thread.to_owned(), Value::Object(owners));
    }
    Value::Object(by_thread)
}

/// All storage reads: operations and bytes.
fn total_reads(reads: &ReadSnapshot) -> (u64, u64) {
    reads
        .by_read_owner
        .iter()
        .flatten()
        .fold((0, 0), |(operations, bytes), counts| {
            (operations + counts.operations, bytes + counts.bytes)
        })
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// How long loading may wait for its background merges, as in the Q3 load
/// program.
const MERGE_WAIT: Duration = Duration::from_secs(3600);

/// Waits until every background merge is idle, so that the refreshes do not
/// compete with loading's compaction. Returns the wait.
fn wait_for_merges(pipeline: &mut Pipeline) -> Result<Duration> {
    let started = Instant::now();
    while !pipeline.is_background_merge_idle()? {
        if started.elapsed() > MERGE_WAIT {
            bail!("background merges still running after {MERGE_WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(started.elapsed())
}

/// The bytes and the files under `root`. DBSP removes its uncheckpointed
/// files when the runtime exits, so the driver observes them before.
fn storage_size(root: &Path) -> Result<Value> {
    let (mut bytes, mut files) = (0u64, 0usize);
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata =
            std::fs::symlink_metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        } else {
            bytes += metadata.len();
            files += 1;
        }
    }
    Ok(json!({"bytes": bytes, "files": files}))
}

fn observed_storage(storage: Option<&Path>) -> Result<Value> {
    storage.map_or(Ok(Value::Null), storage_size)
}

/// The process's peak resident set since the last reset, from `VmHWM` in
/// `/proc/self/status`; `None` where that file does not exist.
fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Resets the peak that `peak_rss_bytes` reads (Linux `clear_refs`).
/// Returns whether it did.
fn reset_peak_rss() -> bool {
    std::fs::write("/proc/self/clear_refs", "5").is_ok()
}

fn mode() -> &'static str {
    if cfg!(feature = "merged-index") {
        "reconstructed"
    } else {
        "native"
    }
}

/// Makes `path` as a new directory; it must not exist.
fn new_directory(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("directory already exists: {}", path.display());
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))
}

/// Runs the program that `config` selects: load and refresh, the load
/// program, or the refresh program. Prints one line for loading and one per
/// refresh, writes the report if asked, and returns the refreshes' records
/// (none for the load program).
pub fn run(config: &Config) -> Result<Vec<RefreshReport>> {
    let mut reference = config.verify.then(Reference::default);
    let (mut pipeline, storage, load, image) = match &config.from_image {
        Some(directory) => {
            let (pipeline, manifest, restore) = reopen(config, directory)?;
            let image = json!({
                "directory": directory,
                "checkpoint": manifest.checkpoint.uuid.to_string(),
                "restore_ms": milliseconds(restore),
            });
            let storage = directory.join(ImageManifest::STORAGE);
            (pipeline, Some(storage), manifest.load, image)
        }
        None => {
            let storage = match &config.write_image {
                Some(directory) => Some(directory.join(ImageManifest::STORAGE)),
                None => config.storage_path.clone(),
            };
            if let Some(path) = &storage {
                new_directory(path)?;
            }
            let mut pipeline = Pipeline::new(config.circuit_config(storage.as_deref())?)?;
            let load = load(
                config,
                &mut pipeline,
                reference.as_mut(),
                storage.as_deref(),
            )?;
            (pipeline, storage, load, Value::Null)
        }
    };
    if let Some(directory) = &config.write_image {
        let checkpoint = pipeline.write_checkpoint()?;
        let manifest = ImageManifest {
            format: ImageManifest::FORMAT,
            settings: ImageSettings::of(config),
            checkpoint,
            inputs: config
                .base
                .clone()
                .context("the load program loads base files")?,
            load,
        };
        std::fs::write(
            directory.join(ImageManifest::FILE),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        println!(
            "mode={} phase=image directory={} checkpoint={}",
            mode(),
            directory.display(),
            manifest.checkpoint.uuid
        );
        return Ok(Vec::new());
    }

    let refresh_list = config
        .refresh_list
        .as_deref()
        .context("the refreshes need --refresh-list")?;
    // Every refresh file is read before the first refresh runs.
    let refreshes = workload::read_refreshes(refresh_list)?;
    pipeline.set_evict_base(true);
    let peak_reset = reset_peak_rss();
    let reports = run_refreshes(&mut pipeline, refreshes, reference.as_mut(), load.view_rows)?;
    let window_peak_rss = peak_reset.then(peak_rss_bytes).flatten();

    let end_storage = observed_storage(storage.as_deref())?;
    println!("mode={} phase=end storage={end_storage}", mode());
    if let Some(path) = &config.report {
        let report = json!({
            "mode": mode(),
            "configuration": {
                "workers": 1,
                "batch": config.batch,
                "file_backed": storage.is_some(),
                "cache_bytes": config.cache_bytes,
                "memory_target_bytes": config.memory_target_bytes,
                "session_pass": cfg!(feature = "merged-index")
                    .then(|| format!("{:?}", config.session_pass).to_lowercase()),
                "front_code_byte_keys": config.front_code_byte_keys,
                "elide_value_headers": config.elide_value_headers,
                "evict_base_after_transaction": true,
                "verified": config.verify,
            },
            "image": image,
            "load": load,
            "window_peak_rss_bytes": window_peak_rss,
            "observed_storage_after_refreshes": end_storage,
            "refreshes": reports
                .iter()
                .enumerate()
                .map(|(number, report)| report.json(number + 1))
                .collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(reports)
}

/// Loads the base files into `pipeline`, and `reference` if given, then waits
/// for the background merges.
fn load(
    config: &Config,
    pipeline: &mut Pipeline,
    mut reference: Option<&mut Reference>,
    storage: Option<&Path>,
) -> Result<LoadRecord> {
    #[cfg(feature = "merged-index")]
    pipeline.set_session_pass(config.session_pass);
    if config.verify {
        pipeline.enable_contract_check();
    }
    let base = config
        .base
        .as_ref()
        .context("loading needs the base files")?;
    let started = Instant::now();
    let mut transactions = 0;
    let rows = workload::load(base, config.batch, |change| {
        if let Some(reference) = reference.as_deref_mut() {
            reference.apply(&change);
        }
        pipeline.transact(change, false)?;
        transactions += 1;
        Ok(())
    })?;
    let merge_wait = wait_for_merges(pipeline)?;
    let elapsed = started.elapsed();
    let (_, view) = pipeline.view_output()?;
    if let Some(reference) = &reference {
        if view != reference.evaluate() {
            bail!("after loading, V differs from the reference");
        }
    }
    let record = LoadRecord {
        rows,
        transactions,
        view_rows: view.values().sum(),
        elapsed_ms: milliseconds(elapsed),
        background_merge_wait_ms: milliseconds(merge_wait),
        observed_storage: observed_storage(storage)?,
    };
    println!(
        "mode={} phase=load rows={rows} transactions={transactions} view_rows={} \
         merge_wait_ms={:.3} storage={} verified={}",
        mode(),
        record.view_rows,
        record.background_merge_wait_ms,
        record.observed_storage,
        config.verify
    );
    Ok(record)
}

/// Reopens the image in `directory` and republishes its state with one empty
/// transaction, which must leave `V` unchanged. Returns the pipeline, the
/// image's manifest, and the time both took.
fn reopen(config: &Config, directory: &Path) -> Result<(Pipeline, ImageManifest, Duration)> {
    let manifest = ImageManifest::read(directory)?;
    manifest.check(config)?;
    let storage = directory.join(ImageManifest::STORAGE);
    if !storage.is_dir() {
        bail!("image has no storage directory: {}", storage.display());
    }
    let started = Instant::now();
    let mut pipeline =
        Pipeline::reopen(config.circuit_config(Some(&storage))?, &manifest.checkpoint)?;
    #[cfg(feature = "merged-index")]
    pipeline.set_session_pass(config.session_pass);
    // A reopened pipeline's outputs are empty until a transaction runs.
    pipeline.transact(Change::default(), false)?;
    if !pipeline.view_delta()?.is_empty() {
        bail!("an empty transaction changed the reopened image's V");
    }
    Ok((pipeline, manifest, started.elapsed()))
}

/// Runs the refreshes in order, from a `V` of `view_rows` rows, checking
/// each against `reference` if given.
fn run_refreshes(
    pipeline: &mut Pipeline,
    refreshes: Vec<Refresh>,
    mut reference: Option<&mut Reference>,
    mut view_rows: i64,
) -> Result<Vec<RefreshReport>> {
    let mut reports = Vec::with_capacity(refreshes.len());
    for (number, refresh) in refreshes.into_iter().enumerate() {
        let phase = refresh.phase();
        let pressure_before = pipeline.memory_pressure();
        let reads_before = read_snapshot();
        // RF2's discovery is the lookup of its line items; RF1 has none.
        let (lines, insert, discovery) = match refresh {
            Refresh::Rf1(lines) => (lines, true, Duration::ZERO),
            Refresh::Rf2(order_keys) => {
                let started = Instant::now();
                let lines = pipeline.order_lines(&order_keys)?;
                (lines, false, started.elapsed())
            }
        };
        let change = Change {
            lines: lines.into_iter().map(|line| (line, insert)).collect(),
            ..Change::default()
        };
        let rows = change.len();
        // The reference's upkeep is verification, outside the timers.
        let verifying = reference
            .as_deref()
            .map(|reference| (reference.evaluate(), change.clone()));
        let started = Instant::now();
        let committed = pipeline.transact(change, false)?;
        let total = discovery + started.elapsed();
        let reads = read_snapshot().since(&reads_before);
        let timings = committed.timings;

        let delta = match (reference.as_deref_mut(), verifying) {
            (Some(reference), Some((before, change))) => {
                let (delta, view) = pipeline.view_output()?;
                reference.apply(&change);
                let after = reference.evaluate();
                if delta != difference(&before, &after) || view != after {
                    bail!("refresh {}: V differs from the reference", number + 1);
                }
                delta
            }
            _ => pipeline.view_delta()?,
        };
        view_rows += delta.values().sum::<i64>();
        let report = RefreshReport {
            summary: RefreshSummary {
                phase,
                rows,
                view_inserted: delta.values().filter(|w| **w > 0).sum(),
                view_deleted: -delta.values().filter(|w| **w < 0).sum::<i64>(),
                view_rows,
            },
            discovery,
            timings,
            total,
            base_evicted_bytes: committed.base_evicted_bytes,
            device_reads: device_reads_json(&reads),
            session_reads: committed.session_reads,
            memory_pressure: json!({
                "before": pressure_before,
                "after": pipeline.memory_pressure(),
            }),
        };
        let (read_operations, read_bytes) = total_reads(&reads);
        println!(
            "mode={} refresh={} phase={phase} rows={rows} view_rows={} total_ms={:.3} \
             base_ms={:.3} index_ms={:.3} ivm_ms={:.3} delta_ms={:.3} q14_ms={:.3} \
             reads={read_operations} read_bytes={read_bytes}",
            mode(),
            number + 1,
            report.summary.view_rows,
            milliseconds(total),
            milliseconds(timings.base()),
            milliseconds(timings.index),
            milliseconds(timings.ivm()),
            milliseconds(timings.delta),
            milliseconds(timings.view),
        );
        reports.push(report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn arguments(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    const BASE: &str = "--part p --partsupp ps --lineitem l";
    const REQUIRED: &str = "--part p --partsupp ps --lineitem l --refresh-list r";

    #[test]
    fn the_command_line_takes_the_files_and_checks_the_numbers() -> Result<()> {
        let config = Config::from_args(arguments(&format!(
            "{REQUIRED} --batch 5 --storage-path s --cache-bytes 2097152 \
             --memory-target-bytes 7 --session-pass step --report out.json --no-verify \
             --front-code-byte-keys --elide-value-headers"
        )))?;
        assert_eq!(
            config.base.as_ref().map(|base| base.lineitem.as_path()),
            Some(Path::new("l"))
        );
        assert_eq!(config.batch, 5);
        assert_eq!(config.cache_bytes, Some(2 * MIB));
        assert_eq!(config.memory_target_bytes, Some(7));
        assert_eq!(config.session_pass, Pass::Step);
        assert_eq!(config.report.as_deref(), Some(Path::new("out.json")));
        assert!(config.front_code_byte_keys && config.elide_value_headers);
        assert!(!config.verify);
        assert!(Config::from_args(arguments(REQUIRED))?.verify);

        for bad in [
            BASE,
            "--part p --lineitem l --refresh-list r",
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

    /// The load program takes the base files only; the refresh program takes
    /// the refreshes only, unverified; neither takes a storage path.
    #[test]
    fn the_image_programs_take_their_own_inputs() -> Result<()> {
        let load = Config::from_args(arguments(&format!(
            "{BASE} --write-image i --cache-bytes 2097152"
        )))?;
        assert_eq!(load.write_image.as_deref(), Some(Path::new("i")));
        let refresh = Config::from_args(arguments(
            "--from-image i --refresh-list r --no-verify --cache-bytes 2097152",
        ))?;
        assert_eq!(refresh.from_image.as_deref(), Some(Path::new("i")));

        for bad in [
            format!("{REQUIRED} --write-image i"),
            format!("{BASE} --write-image i --storage-path s"),
            "--write-image i".to_owned(),
            format!("{BASE} --from-image i --refresh-list r --no-verify"),
            "--from-image i --refresh-list r".to_owned(),
            "--from-image i --no-verify".to_owned(),
            "--from-image i --refresh-list r --no-verify --storage-path s".to_owned(),
            format!("{BASE} --write-image i --from-image j --no-verify"),
        ] {
            assert!(Config::from_args(arguments(&bad)).is_err(), "{bad}");
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
    /// absent order 7. Loading takes three transactions. The run is verified,
    /// file-backed, and writes its report into `directory`.
    fn small_run(directory: &Path) -> Config {
        let lines: String = [(1, 1, 1), (1, 2, 2), (2, 1, 1), (3, 1, 2), (4, 1, 1)]
            .into_iter()
            .map(|(order, number, part)| line(order, number, part))
            .collect();
        write(
            directory,
            "lineitem.tbl.u1",
            &(line(5, 1, 2) + &line(5, 2, 9)),
        );
        write(directory, "delete.1", "1|\n5|\n7|\n");
        Config {
            base: Some(BaseFiles {
                part: write(
                    directory,
                    "part.tbl",
                    "1|n|m|b|A|1|c|1.00|x|\n2|n|m|b|B|1|c|1.00|x|\n3|n|m|b|C|1|c|1.00|x|\n",
                ),
                partsupp: write(
                    directory,
                    "partsupp.tbl",
                    "1|10|5|1.00|x|\n2|10|5|1.00|x|\n",
                ),
                lineitem: write(directory, "lineitem.tbl", &lines),
            }),
            refresh_list: Some(write(
                directory,
                "list.json",
                r#"[{"rf1_lines": "lineitem.tbl.u1"}, {"rf2": "delete.1"}]"#,
            )),
            batch: 4,
            storage_path: Some(directory.join("storage")),
            cache_bytes: Some(4 * MIB),
            memory_target_bytes: None,
            session_pass: Pass::Seek,
            front_code_byte_keys: true,
            elide_value_headers: true,
            write_image: None,
            from_image: None,
            verify: true,
            report: Some(directory.join("report.json")),
        }
    }

    fn summaries(reports: &[RefreshReport]) -> Vec<RefreshSummary> {
        reports
            .iter()
            .map(|report| report.summary.clone())
            .collect()
    }

    #[test]
    fn a_run_loads_and_refreshes_in_agreement_with_the_reference() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        let config = small_run(path);
        let reports = run(&config)?;
        assert_eq!(
            summaries(&reports),
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
        // RF1 discovers nothing, every phase fits in the refresh's total, and
        // only reconstructed mode keeps an index.
        assert_eq!(reports[0].discovery, Duration::ZERO);
        for report in &reports {
            let timings = report.timings;
            let phases = report.discovery
                + timings.preparation
                + timings.base()
                + timings.forward
                + timings.base_eviction
                + timings.ivm()
                + timings.publication;
            assert!(phases <= report.total, "{report:?}");
            assert_eq!(cfg!(feature = "merged-index"), !timings.index.is_zero());
        }
        let written: Value = serde_json::from_slice(&std::fs::read(path.join("report.json"))?)?;
        assert_eq!(written["mode"], mode());
        assert_eq!(written["load"]["view_rows"], 5);
        assert!(written["image"].is_null());
        assert_eq!(written["refreshes"][1]["phase"], "rf2");
        assert_eq!(written["refreshes"][1]["view_deleted"], 3);
        assert!(written["refreshes"][1]["memory_pressure"]["after"].is_string());
        assert!(written["observed_storage_after_refreshes"]["files"].is_u64());
        assert!(written["refreshes"][0]["device_reads"].is_object());
        assert_eq!(
            written["refreshes"][0]["session"].is_object(),
            cfg!(feature = "merged-index")
        );
        // A second run may not reuse the storage directory.
        assert!(run(&config).is_err());
        Ok(())
    }

    /// The load program's image, reopened by the refresh program, gives the
    /// refreshes exactly what a single verified run gives them. The refresh
    /// program refuses an image written with other storage settings.
    #[test]
    fn an_image_reopens_where_its_loader_stopped() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path();
        let single = small_run(path);
        let expected = summaries(&run(&single)?);

        let image = path.join("image");
        let loader = Config {
            refresh_list: None,
            storage_path: None,
            write_image: Some(image.clone()),
            report: None,
            ..single.clone()
        };
        assert!(run(&loader)?.is_empty());
        let manifest = ImageManifest::read(&image)?;
        assert_eq!(manifest.load.view_rows, 5);
        assert_eq!(manifest.settings, ImageSettings::of(&single));
        // The image directory must be new.
        assert!(run(&loader).is_err());

        let refresher = Config {
            base: None,
            storage_path: None,
            from_image: Some(image.clone()),
            verify: false,
            report: Some(path.join("from-image.json")),
            ..single.clone()
        };
        let other_format = Config {
            front_code_byte_keys: false,
            ..refresher.clone()
        };
        assert!(run(&other_format).is_err());
        assert_eq!(summaries(&run(&refresher)?), expected);
        let written: Value = serde_json::from_slice(&std::fs::read(path.join("from-image.json"))?)?;
        assert_eq!(written["load"]["view_rows"], 5);
        assert!(written["image"]["restore_ms"].is_f64());
        Ok(())
    }
}
