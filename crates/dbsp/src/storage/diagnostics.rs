//! Opt-in storage diagnostics: a flush log, a compaction log, and an
//! inventory of the runs that live spines hold.
//!
//! Everything here is off until [`enable`] is called, and costs one relaxed
//! atomic load per event while off. The logs grow without bound while on, so
//! they are meant for short experiments, not for production.
//!
//! * The **flush log** has one [`FlushRecord`] per backend flush, written at
//!   the place where the backend's write counters are updated. Over any period
//!   in which diagnostics were on and no flush was in flight at either end,
//!   the record count equals the counted operations and the record bytes sum
//!   to the counted bytes.
//! * The **compaction log** has one [`CompactionRecord`] for each run that a
//!   spine writes outside the circuit's own steps: a background compaction, a
//!   requested full compaction, or the spill of an inserted in-memory run.
//! * The **inventory** lists, per live spine, its runs and its consolidated
//!   row count. Taking it reads data through the buffer cache but writes
//!   nothing.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::write_attribution::WriteOwner;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns the diagnostics on for the rest of the process.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// True once [`enable`] has been called.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Microseconds since the Unix epoch.
///
/// Every timestamp in these logs uses this clock so that a harness can place
/// events inside or outside its own measurement window.
pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64)
}

/// One backend flush of a file writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FlushRecord {
    /// Storage path of the file, without the `.mut` suffix of an unfinished
    /// file.
    pub file: String,
    /// Bytes this flush wrote.
    pub bytes: u64,
    /// Number of buffered blocks the flush wrote.
    pub blocks: usize,
    /// When the flush began, in [`now_micros`].
    pub started_micros: u64,
    /// When the flush ended, in [`now_micros`].
    pub finished_micros: u64,
    /// The owner that the file writer captured when it was created.
    pub owner: WriteOwner,
    /// False if the flush failed after writing `bytes`.
    pub succeeded: bool,
}

/// Why a spine wrote a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunWriteKind {
    /// A compaction that the spine started on its own.
    BackgroundCompaction,
    /// A compaction in the sweep that a full-compaction request started.
    RequestedCompaction,
    /// A run inserted in memory that the spine wrote to storage at once.
    InsertSpill,
}

/// One run that a spine wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompactionRecord {
    /// Why the run was written.
    pub kind: RunWriteKind,
    /// The owner of the spine.
    pub owner: WriteOwner,
    /// The spine's name at the time.
    pub spine: String,
    /// The spine level that was compacted (0 for a spill).
    pub level: usize,
    /// When the work began, in [`now_micros`].
    pub started_micros: u64,
    /// When the work ended, in [`now_micros`].
    pub finished_micros: u64,
    /// Number of runs consumed.
    pub input_runs: usize,
    /// Records in the consumed runs, before consolidation.
    pub input_records: u64,
    /// Records in the output run.
    pub output_records: u64,
    /// `"memory"` or `"storage"`.
    pub output_location: &'static str,
    /// Storage path of the output file, if the output is on storage.
    pub output_file: Option<String>,
    /// Size of the output file, if the output is on storage.
    pub output_file_bytes: Option<u64>,
}

static FLUSH_LOG: Mutex<Vec<FlushRecord>> = Mutex::new(Vec::new());
static COMPACTION_LOG: Mutex<Vec<CompactionRecord>> = Mutex::new(Vec::new());

/// Appends a flush record if diagnostics are on.
pub(crate) fn log_flush(record: impl FnOnce() -> FlushRecord) {
    if enabled() {
        FLUSH_LOG.lock().unwrap().push(record());
    }
}

/// Appends a compaction record if diagnostics are on.
pub(crate) fn log_compaction(record: impl FnOnce() -> CompactionRecord) {
    if enabled() {
        COMPACTION_LOG.lock().unwrap().push(record());
    }
}

/// A copy of the flush log, in the order the flushes finished.
pub fn flush_log() -> Vec<FlushRecord> {
    FLUSH_LOG.lock().unwrap().clone()
}

/// A copy of the compaction log, in the order the work finished.
pub fn compaction_log() -> Vec<CompactionRecord> {
    COMPACTION_LOG.lock().unwrap().clone()
}

/// One run (an immutable sorted batch) of a spine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunContents {
    /// `"memory"` or `"storage"`.
    pub location: &'static str,
    /// Records in the run, counting every (key, value) pair once.
    pub records: u64,
    /// Records with a negative weight, when the run tracks them.
    pub negative_weight_records: Option<u64>,
    /// Storage path of the run's file, if it is on storage.
    pub file: Option<String>,
    /// Size of that file in bytes.
    pub file_bytes: Option<u64>,
}

/// The runs of one spine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpineContents {
    /// The owner the spine captured when it was created.
    pub owner: WriteOwner,
    /// The spine's name.
    pub name: String,
    /// `"accumulator"` or `"integral"`.
    pub role: &'static str,
    /// The spine's runs, loose and merging, oldest first within each level.
    pub runs: Vec<RunContents>,
    /// Number of (key, value) pairs whose weights sum to nonzero over all
    /// runs, or `None` for batches with timestamps.
    pub consolidated_rows: Option<u64>,
}

type Inspector = Box<dyn Fn() -> Option<SpineContents> + Send + Sync>;

static SPINES: Mutex<Vec<Inspector>> = Mutex::new(Vec::new());

/// Registers a spine so that [`spine_contents`] reports it. `inspect` returns
/// `None` once the spine has been dropped.
pub(crate) fn register_spine(inspect: Inspector) {
    SPINES.lock().unwrap().push(inspect);
}

/// Lists the runs of every spine created while diagnostics were on and still
/// alive. Spines that have been dropped are forgotten.
pub fn spine_contents() -> Vec<SpineContents> {
    // Inspecting a spine reads storage, so do it outside the registry lock.
    let inspectors = std::mem::take(&mut *SPINES.lock().unwrap());
    let mut contents = Vec::new();
    let mut alive = Vec::new();
    for inspect in inspectors {
        if let Some(spine) = inspect() {
            contents.push(spine);
            alive.push(inspect);
        }
    }
    // Spines registered while the lock was released are kept after the old ones.
    let mut registry = SPINES.lock().unwrap();
    alive.append(&mut registry);
    *registry = alive;
    contents
}
