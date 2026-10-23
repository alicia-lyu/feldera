//! Process and storage measurements for the R5 maintenance experiment.
//!
//! Storage histograms are process-global. Each measured executable runs in a
//! fresh process so their deltas belong to one comparison mode and run.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dbsp::storage::write_attribution::{write_snapshot, WriteOwner, WriteSnapshot};
use feldera_storage::histogram::ExponentialHistogram;
use feldera_storage::metrics::{READ_BLOCKS_BYTES, READ_LATENCY_MICROSECONDS};
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug)]
pub struct ResourceSnapshot {
    backend_read_operations: u64,
    backend_write_operations: u64,
    backend_read_bytes: u64,
    backend_write_bytes: u64,
    /// The write totals above, split by owner. Read together with them so the
    /// owners always sum to the totals.
    writes: WriteSnapshot,
    cpu_microseconds: u64,
    peak_rss_bytes: u64,
    os_physical_read_bytes: Option<u64>,
    os_physical_write_bytes: Option<u64>,
}

impl ResourceSnapshot {
    pub fn capture() -> Result<Self> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the complete structure on success.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error()).context("getrusage");
        }
        // SAFETY: The successful call above initialized `usage`.
        let usage = unsafe { usage.assume_init() };
        let cpu_microseconds =
            timeval_micros(usage.ru_utime).saturating_add(timeval_micros(usage.ru_stime));
        #[cfg(target_os = "macos")]
        let peak_rss_bytes = usage.ru_maxrss.max(0) as u64;
        #[cfg(not(target_os = "macos"))]
        let peak_rss_bytes = (usage.ru_maxrss.max(0) as u64).saturating_mul(1024);

        #[cfg(target_os = "linux")]
        let (os_physical_read_bytes, os_physical_write_bytes) = linux_physical_io()?;
        #[cfg(not(target_os = "linux"))]
        let (os_physical_read_bytes, os_physical_write_bytes) = (None, None);

        let writes = write_snapshot();
        Ok(Self {
            backend_read_operations: histogram_count(&READ_LATENCY_MICROSECONDS),
            backend_write_operations: writes.total.operations,
            backend_read_bytes: READ_BLOCKS_BYTES.sum(),
            backend_write_bytes: writes.total.bytes,
            writes,
            cpu_microseconds,
            peak_rss_bytes,
            os_physical_read_bytes,
            os_physical_write_bytes,
        })
    }

    /// Returns counter increments. RSS is the process high-water value at the
    /// end of the interval, not an additive interval counter.
    pub fn delta(&self, after: &Self) -> Value {
        json!({
            "backend_write_by_owner": self.write_delta_by_owner(after),
            "backend_read_operations": after.backend_read_operations.saturating_sub(self.backend_read_operations),
            "backend_write_operations": after.backend_write_operations.saturating_sub(self.backend_write_operations),
            "backend_read_bytes": after.backend_read_bytes.saturating_sub(self.backend_read_bytes),
            "backend_write_bytes": after.backend_write_bytes.saturating_sub(self.backend_write_bytes),
            "cpu_microseconds": after.cpu_microseconds.saturating_sub(self.cpu_microseconds),
            "peak_rss_bytes": after.peak_rss_bytes,
            "os_physical_read_bytes": optional_delta(self.os_physical_read_bytes, after.os_physical_read_bytes),
            "os_physical_write_bytes": optional_delta(self.os_physical_write_bytes, after.os_physical_write_bytes),
            "notes": {
                "backend_write_by_owner": "The write operations and bytes above, split by the owner (query, base_relation, merged_index) that each file writer captured when its file was created; the owners sum to the totals.",
                "backend_operations": "Counts from storage read/write latency histograms; process-global.",
                "backend_bytes": "Exact sums of storage read/write block-size histograms; process-global.",
                "os_physical_bytes": "Linux /proc/self/io read_bytes/write_bytes; unavailable on other platforms. These differ from rchar/wchar syscall bytes.",
                "peak_rss_bytes": "getrusage process-lifetime high-water mark since process start: it includes loading, setup compaction, the refresh pre-read, the driver and the runtime. It is not a maintenance-window figure; see sequence.window_peak_rss_bytes (Linux only)."
            }
        })
    }
}

impl ResourceSnapshot {
    /// Per-owner increments of the write counters, keyed by owner name.
    fn write_delta_by_owner(&self, after: &Self) -> Value {
        let mut owners = Map::new();
        for owner in WriteOwner::ALL {
            let before = self.writes.owner(owner);
            let after = after.writes.owner(owner);
            owners.insert(
                owner.name().to_owned(),
                json!({
                    "operations": after.operations.saturating_sub(before.operations),
                    "bytes": after.bytes.saturating_sub(before.bytes),
                }),
            );
        }
        Value::Object(owners)
    }
}

fn histogram_count(histogram: &ExponentialHistogram) -> u64 {
    histogram
        .snapshot()
        .iter_buckets()
        .map(|bucket| bucket.count)
        .sum()
}

fn optional_delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    before
        .zip(after)
        .map(|(before, after)| after.saturating_sub(before))
}

fn timeval_micros(time: libc::timeval) -> u64 {
    (time.tv_sec.max(0) as u64)
        .saturating_mul(1_000_000)
        .saturating_add(time.tv_usec.max(0) as u64)
}

#[cfg(target_os = "linux")]
fn linux_physical_io() -> Result<(Option<u64>, Option<u64>)> {
    let contents = fs::read_to_string("/proc/self/io").context("read /proc/self/io")?;
    let mut read_bytes = None;
    let mut write_bytes = None;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("read_bytes:") {
            read_bytes = Some(value.trim().parse().context("parse read_bytes")?);
        } else if let Some(value) = line.strip_prefix("write_bytes:") {
            write_bytes = Some(value.trim().parse().context("parse write_bytes")?);
        }
    }
    if read_bytes.is_none() || write_bytes.is_none() {
        bail!("/proc/self/io lacks read_bytes or write_bytes");
    }
    Ok((read_bytes, write_bytes))
}

/// Recursively sums regular-file lengths, tolerating files removed during
/// compaction. A missing root or other filesystem error fails the measurement.
pub fn storage_size_bytes(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if path != root && error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
        };
        if metadata.is_dir() {
            let entries = match fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(error) if path != root && error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("read directory {}", path.display()));
                }
            };
            for entry in entries {
                let entry = entry.with_context(|| format!("read directory {}", path.display()))?;
                pending.push(entry.path());
            }
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// Samples storage during a run. `stop` joins the sampler and propagates scan
/// errors, including errors from samples taken before stop was requested.
pub struct StorageSampler {
    stop_sender: Sender<()>,
    worker: JoinHandle<Result<u64>>,
}

impl StorageSampler {
    pub fn start(root: PathBuf) -> Result<Self> {
        let initial_size = storage_size_bytes(&root)?;
        let (stop_sender, stop_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("r5-storage-sampler".to_owned())
            .spawn(move || {
                let mut peak = initial_size;
                loop {
                    match stop_receiver.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            peak = peak.max(storage_size_bytes(&root)?);
                        }
                    }
                }
                Ok(peak.max(storage_size_bytes(&root)?))
            })
            .context("spawn storage sampler")?;
        Ok(Self {
            stop_sender,
            worker,
        })
    }

    pub fn stop(self) -> Result<u64> {
        let _ = self.stop_sender.send(());
        self.worker
            .join()
            .map_err(|_| anyhow::anyhow!("storage sampler panicked"))?
    }
}

/// Waits until all caller-defined compaction work is complete. The caller
/// should request circuit and manual-index compaction before invoking this.
pub fn drain_until(
    timeout: Duration,
    mut complete: impl FnMut() -> Result<bool>,
) -> Result<Duration> {
    let started = Instant::now();
    loop {
        if complete()? {
            return Ok(started.elapsed());
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            bail!("compaction drainage timed out after {timeout:?}");
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbsp::storage::write_attribution::WriteCounts;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "r5-measurement-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    /// A write snapshot whose owners, in `WriteOwner::ALL` order, wrote the
    /// given (operations, bytes), and whose totals are their sums.
    fn snapshot_of(by_owner: [(u64, u64); 3]) -> WriteSnapshot {
        let by_owner = by_owner.map(|(operations, bytes)| WriteCounts { operations, bytes });
        WriteSnapshot {
            total: WriteCounts {
                operations: by_owner.iter().map(|c| c.operations).sum(),
                bytes: by_owner.iter().map(|c| c.bytes).sum(),
            },
            by_owner,
        }
    }

    #[test]
    fn owner_deltas_sum_to_the_total_deltas() {
        let before = ResourceSnapshot {
            backend_write_operations: 3,
            backend_write_bytes: 30,
            writes: snapshot_of([(1, 10), (1, 10), (1, 10)]),
            ..zero_snapshot()
        };
        let after = ResourceSnapshot {
            backend_write_operations: 7,
            backend_write_bytes: 300,
            writes: snapshot_of([(2, 100), (2, 100), (3, 100)]),
            ..zero_snapshot()
        };
        let delta = before.delta(&after);
        let owners = &delta["backend_write_by_owner"];
        assert_eq!(owners["query"], json!({"operations": 1, "bytes": 90}));
        assert_eq!(
            owners["base_relation"],
            json!({"operations": 1, "bytes": 90})
        );
        assert_eq!(
            owners["merged_index"],
            json!({"operations": 2, "bytes": 90})
        );
        for (field, total) in [
            ("operations", "backend_write_operations"),
            ("bytes", "backend_write_bytes"),
        ] {
            let by_owner: u64 = WriteOwner::ALL
                .iter()
                .map(|owner| owners[owner.name()][field].as_u64().unwrap())
                .sum();
            assert_eq!(Value::from(by_owner), delta[total]);
        }
    }

    fn zero_snapshot() -> ResourceSnapshot {
        ResourceSnapshot {
            backend_read_operations: 0,
            backend_write_operations: 0,
            backend_read_bytes: 0,
            backend_write_bytes: 0,
            writes: WriteSnapshot::default(),
            cpu_microseconds: 0,
            peak_rss_bytes: 0,
            os_physical_read_bytes: None,
            os_physical_write_bytes: None,
        }
    }

    #[test]
    fn counters_are_interval_deltas_with_explicit_unavailable_io() {
        let before = ResourceSnapshot {
            backend_read_operations: 2,
            backend_write_operations: 3,
            backend_read_bytes: 20,
            backend_write_bytes: 30,
            writes: snapshot_of([(1, 10), (1, 10), (1, 10)]),
            cpu_microseconds: 100,
            peak_rss_bytes: 1_000,
            os_physical_read_bytes: None,
            os_physical_write_bytes: None,
        };
        let after = ResourceSnapshot {
            backend_read_operations: 5,
            backend_write_operations: 7,
            backend_read_bytes: 200,
            backend_write_bytes: 300,
            writes: snapshot_of([(2, 100), (2, 100), (3, 100)]),
            cpu_microseconds: 150,
            peak_rss_bytes: 2_000,
            os_physical_read_bytes: None,
            os_physical_write_bytes: None,
        };
        let delta = before.delta(&after);
        assert_eq!(delta["backend_read_operations"], 3);
        assert_eq!(delta["backend_write_operations"], 4);
        assert_eq!(delta["backend_read_bytes"], 180);
        assert_eq!(delta["backend_write_bytes"], 270);
        assert_eq!(delta["cpu_microseconds"], 50);
        assert_eq!(delta["peak_rss_bytes"], 2_000);
        assert!(delta["os_physical_read_bytes"].is_null());
    }

    #[test]
    fn sampler_tracks_files_and_final_size() {
        let root = test_directory();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/file"), vec![0; 128]).unwrap();
        let sampler = StorageSampler::start(root.clone()).unwrap();
        fs::write(root.join("new-file"), vec![0; 64]).unwrap();
        assert_eq!(storage_size_bytes(&root).unwrap(), 192);
        assert!(sampler.stop().unwrap() >= 192);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drainage_succeeds_and_times_out() {
        let mut polls = 0;
        let elapsed = drain_until(Duration::from_secs(1), || {
            polls += 1;
            Ok(polls == 2)
        })
        .unwrap();
        assert!(elapsed < Duration::from_secs(1));
        assert!(drain_until(Duration::ZERO, || Ok(false))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }
}
