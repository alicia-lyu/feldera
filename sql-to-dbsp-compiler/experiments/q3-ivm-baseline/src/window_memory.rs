//! Memory evidence for the maintenance window, the interval from the first refresh's start to
//! the last refresh's end.
//!
//! Everything here is Linux-only evidence: `/proc/self/status`, `/proc/self/clear_refs`, the
//! process's own cgroup v2 files, and `posix_fadvise`. On other platforms each reading is
//! `null` and nothing is changed. Missing or unreadable files make their fields `null` (with
//! the reason recorded when it is not simply absence); they never abort the run.
//!
//! The parsers are pure functions over file contents so that they are tested on every
//! platform. They are unused off Linux, hence the `dead_code` allowance.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The cgroup v2 mount point.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The `memory.stat` keys that are reported. Absent keys are omitted, so the set adapts to the
/// kernel version (for example, `workingset_refault` was split into `_anon` and `_file`).
const STAT_KEYS: &[&str] = &[
    "anon",
    "file",
    "pgfault",
    "pgmajfault",
    "pgscan",
    "pgsteal",
    "workingset_refault",
    "workingset_refault_anon",
    "workingset_refault_file",
];

// ---------------------------------------------------------------------------------------------
// Pure parsers.
// ---------------------------------------------------------------------------------------------

/// `VmHWM` and `VmRSS` of `/proc/self/status`, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessMemory {
    /// The resident-set high-water mark since process start or the last `clear_refs` reset.
    pub peak_rss_bytes: u64,
    pub rss_bytes: u64,
}

/// Parses `/proc/self/status`. Returns `None` unless both `VmHWM` and `VmRSS` are present and
/// well formed (lines look like `VmHWM:\t  123456 kB`).
pub fn parse_proc_status(contents: &str) -> Option<ProcessMemory> {
    let kibibytes = |key: &str| -> Option<u64> {
        let line = contents.lines().find_map(|line| line.strip_prefix(key))?;
        let mut fields = line.strip_prefix(':')?.split_whitespace();
        let value: u64 = fields.next()?.parse().ok()?;
        (fields.next()? == "kB").then(|| value.checked_mul(1024))?
    };
    Some(ProcessMemory {
        peak_rss_bytes: kibibytes("VmHWM")?,
        rss_bytes: kibibytes("VmRSS")?,
    })
}

/// Parses a flat-keyed cgroup file (`memory.stat`, `memory.events`): one `name value` pair per
/// line. Lines that are not such a pair are returned in the second element.
pub fn parse_flat_keyed(contents: &str) -> (BTreeMap<String, u64>, Vec<String>) {
    let mut values = BTreeMap::new();
    let mut malformed = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        match (
            fields.next(),
            fields.next().and_then(|v| v.parse::<u64>().ok()),
            fields.next(),
        ) {
            (Some(name), Some(value), None) => {
                values.insert(name.to_owned(), value);
            }
            _ => malformed.push(line.to_owned()),
        }
    }
    (values, malformed)
}

/// One line of `memory.pressure`: the stall averages in percent and the total stall time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PressureLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_microseconds: u64,
}

/// Parses `memory.pressure` (`some avg10=0.00 avg60=0.00 avg300=0.00 total=0`, then `full ...`),
/// keyed by the first word. Returns `None` if no line parses or any line is malformed.
pub fn parse_pressure(contents: &str) -> Option<BTreeMap<String, PressureLine>> {
    let mut lines = BTreeMap::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let kind = fields.next()?;
        let mut parsed = BTreeMap::new();
        for field in fields {
            let (name, value) = field.split_once('=')?;
            parsed.insert(name, value);
        }
        let average = |name: &str| parsed.get(name)?.parse::<f64>().ok();
        lines.insert(
            kind.to_owned(),
            PressureLine {
                avg10: average("avg10")?,
                avg60: average("avg60")?,
                avg300: average("avg300")?,
                total_microseconds: parsed.get("total")?.parse().ok()?,
            },
        );
    }
    (!lines.is_empty()).then_some(lines)
}

/// The path of the cgroup v2 hierarchy that `/proc/self/cgroup` names (the `0::<path>` line),
/// or `None` on a cgroup v1 system.
pub fn parse_own_cgroup(contents: &str) -> Option<&str> {
    let path = contents.lines().find_map(|line| line.strip_prefix("0::"))?;
    // A removed cgroup is shown with a " (deleted)" suffix.
    Some(path.strip_suffix(" (deleted)").unwrap_or(path))
}

// ---------------------------------------------------------------------------------------------
// Cgroup snapshots.
// ---------------------------------------------------------------------------------------------

/// The process's own cgroup v2 directory.
#[derive(Clone, Debug)]
pub struct Cgroup {
    directory: PathBuf,
}

impl Cgroup {
    /// The cgroup this process runs in, or `None` off Linux, on cgroup v1, or if the directory
    /// is not visible.
    pub fn own() -> Option<Self> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let path = parse_own_cgroup(&membership)?;
        Self::at(&Path::new(CGROUP_ROOT).join(path.trim_start_matches('/')))
    }

    fn at(directory: &Path) -> Option<Self> {
        directory.is_dir().then(|| Self {
            directory: directory.to_path_buf(),
        })
    }

    fn file(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    /// `memory.current` in bytes, or `None` if unreadable.
    pub fn memory_current(&self) -> Option<u64> {
        std::fs::read_to_string(self.file("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub fn snapshot(&self) -> CgroupSnapshot {
        let mut problems = Vec::new();
        let mut read = |name: &str| match std::fs::read_to_string(self.file(name)) {
            Ok(contents) => Some(contents),
            // Absence is expected: PSI may be off, and old kernels lack some files.
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                problems.push(format!("{name}: {error}"));
                None
            }
        };
        let current = read("memory.current");
        let events = read("memory.events");
        let stat = read("memory.stat");
        let pressure = read("memory.pressure");
        CgroupSnapshot::from_contents(
            current.as_deref(),
            events.as_deref(),
            stat.as_deref(),
            pressure.as_deref(),
            problems,
        )
    }
}

/// One reading of the cgroup files that bear on memory pressure. A field is `None` when its
/// file was absent or unparseable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CgroupSnapshot {
    pub memory_current_bytes: Option<u64>,
    /// All of `memory.events` (`low`, `high`, `max`, `oom`, `oom_kill`, ...).
    pub events: Option<BTreeMap<String, u64>>,
    /// The `STAT_KEYS` subset of `memory.stat` that the kernel provides.
    pub stat: Option<BTreeMap<String, u64>>,
    pub pressure: Option<BTreeMap<String, PressureLine>>,
    /// Read or parse failures other than a file being absent.
    pub problems: Vec<String>,
}

impl CgroupSnapshot {
    /// Builds a snapshot from file contents (`None` for an absent file).
    pub fn from_contents(
        current: Option<&str>,
        events: Option<&str>,
        stat: Option<&str>,
        pressure: Option<&str>,
        mut problems: Vec<String>,
    ) -> Self {
        let memory_current_bytes = current.and_then(|text| match text.trim().parse() {
            Ok(value) => Some(value),
            Err(_) => {
                problems.push(format!("memory.current: not a number: {:?}", text.trim()));
                None
            }
        });
        let mut keyed = |name: &str, text: Option<&str>| {
            text.map(|text| {
                let (values, malformed) = parse_flat_keyed(text);
                problems.extend(
                    malformed
                        .into_iter()
                        .map(|line| format!("{name}: {line:?}")),
                );
                values
            })
        };
        let events = keyed("memory.events", events);
        let stat = keyed("memory.stat", stat).map(|all| {
            all.into_iter()
                .filter(|(name, _)| STAT_KEYS.contains(&name.as_str()))
                .collect()
        });
        let pressure = pressure.and_then(|text| {
            let parsed = parse_pressure(text);
            if parsed.is_none() {
                problems.push(format!("memory.pressure: unparseable: {:?}", text.trim()));
            }
            parsed
        });
        Self {
            memory_current_bytes,
            events,
            stat,
            pressure,
            problems,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "memory_current_bytes": self.memory_current_bytes,
            "memory_events": self.events,
            "memory_stat": self.stat,
            "memory_pressure": self.pressure.as_ref().map(|lines| {
                lines.iter().map(|(kind, line)| (kind.clone(), json!({
                    "avg10": line.avg10, "avg60": line.avg60, "avg300": line.avg300,
                    "total_microseconds": line.total_microseconds,
                }))).collect::<Map<String, Value>>()
            }),
            "problems": self.problems,
        })
    }

    /// The change from `self` to `after`. Counters and gauges alike are reported as signed
    /// differences (`after - before`); a field is `null` when either side lacks it.
    pub fn delta(&self, after: &Self) -> Value {
        json!({
            "memory_current_bytes": signed_difference(self.memory_current_bytes, after.memory_current_bytes),
            "memory_events": map_difference(&self.events, &after.events),
            "memory_stat": map_difference(&self.stat, &after.stat),
            "memory_pressure_total_microseconds": self.pressure.as_ref().zip(after.pressure.as_ref()).map(|(before, after)| {
                after.iter().filter_map(|(kind, line)| {
                    let earlier = before.get(kind)?;
                    Some((kind.clone(), json!(signed_difference(
                        Some(earlier.total_microseconds), Some(line.total_microseconds)))))
                }).collect::<Map<String, Value>>()
            }),
        })
    }
}

fn signed_difference(before: Option<u64>, after: Option<u64>) -> Option<i64> {
    let (before, after) = (before?, after?);
    Some(i64::try_from(i128::from(after) - i128::from(before)).unwrap_or(i64::MAX))
}

/// Per-key differences over the keys present on both sides.
fn map_difference(
    before: &Option<BTreeMap<String, u64>>,
    after: &Option<BTreeMap<String, u64>>,
) -> Option<BTreeMap<String, i64>> {
    let (before, after) = (before.as_ref()?, after.as_ref()?);
    Some(
        after
            .iter()
            .filter_map(|(name, value)| {
                Some((
                    name.clone(),
                    signed_difference(Some(*before.get(name)?), Some(*value))?,
                ))
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------------
// Process memory.
// ---------------------------------------------------------------------------------------------

/// Reads `VmHWM` and `VmRSS`; `None` off Linux or if `/proc/self/status` is unreadable.
pub fn process_memory() -> Option<ProcessMemory> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    parse_proc_status(&std::fs::read_to_string("/proc/self/status").ok()?)
}

/// The current resident set size, or `None` off Linux.
pub fn current_rss_bytes() -> Option<u64> {
    process_memory().map(|memory| memory.rss_bytes)
}

/// Resets `VmHWM` to the current resident set size (`echo 5 > /proc/self/clear_refs`, Linux
/// 4.0 and later).
fn reset_peak_rss() -> io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/clear_refs")?
        .write_all(b"5")
}

// ---------------------------------------------------------------------------------------------
// Page-cache eviction.
// ---------------------------------------------------------------------------------------------

/// Asks the kernel to drop the cached pages of `path` (`POSIX_FADV_DONTNEED`). Dirty pages are
/// not dropped; the driver only reads these files, so they are clean.
#[cfg(target_os = "linux")]
fn evict_file(path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = File::open(path)?;
    // SAFETY: the descriptor is open for the duration of the call; offset and length zero mean
    // the whole file. posix_fadvise returns the error number instead of setting errno.
    let status = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status))
    }
}

#[cfg(not(target_os = "linux"))]
fn evict_file(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "posix_fadvise(DONTNEED) is applied on Linux only",
    ))
}

/// Evicts every distinct file in `paths` from the page cache, and records what happened. When
/// the process runs in a cgroup, `memory.current` is read before and after. Failures are
/// recorded, not raised.
pub fn evict_inputs(paths: &[PathBuf]) -> Value {
    let cgroup = Cgroup::own();
    let before = cgroup.as_ref().and_then(Cgroup::memory_current);
    let mut distinct = paths.to_vec();
    distinct.sort();
    distinct.dedup();
    let mut evicted = Vec::new();
    let mut failures = Vec::new();
    for path in &distinct {
        match evict_file(path) {
            Ok(()) => evicted.push(path.display().to_string()),
            Err(error) => failures
                .push(json!({"path": path.display().to_string(), "error": error.to_string()})),
        }
    }
    let after = cgroup.as_ref().and_then(Cgroup::memory_current);
    json!({
        "evicted": cfg!(target_os = "linux") && failures.is_empty() && !evicted.is_empty(),
        "method": "posix_fadvise(POSIX_FADV_DONTNEED), Linux only",
        "files": evicted,
        "failures": failures,
        "cgroup_memory_current_before_bytes": before,
        "cgroup_memory_current_after_bytes": after,
    })
}

// ---------------------------------------------------------------------------------------------
// The window.
// ---------------------------------------------------------------------------------------------

/// `memory.peak`, opened so that it can be reset and read back through one descriptor. The
/// kernel (6.12 and later, per `Documentation/admin-guide/cgroup-v2.rst`) keeps the reset
/// watermark per open file, so the file must stay open from the reset to the final read.
/// UNVERIFIED on this project's Linux host: the first pilot run shows whether it works.
struct WindowPeak {
    file: File,
}

impl WindowPeak {
    /// Opens `memory.peak` and resets it. The error says why the reset is unsupported.
    fn reset(cgroup: &Cgroup) -> Result<Self, String> {
        let path = cgroup.file("memory.peak");
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("open {} read-write: {error}", path.display()))?;
        file.write_all(b"reset")
            .map_err(|error| format!("write to {}: {error}", path.display()))?;
        Ok(Self { file })
    }

    fn read(&mut self) -> Result<u64, String> {
        let mut contents = String::new();
        self.file
            .rewind()
            .and_then(|()| self.file.read_to_string(&mut contents))
            .map_err(|error| format!("read memory.peak: {error}"))?;
        contents
            .trim()
            .parse()
            .map_err(|_| format!("memory.peak: not a number: {:?}", contents.trim()))
    }
}

/// Collects memory evidence from the start of the window to its end.
pub struct WindowMemory {
    /// The outcome of the `clear_refs` write; `Err` carries the reason (reported as
    /// `window_peak_rss_error`) and nulls the window peak.
    peak_rss_reset: Result<(), String>,
    cgroup: Option<CgroupState>,
}

struct CgroupState {
    handle: Cgroup,
    before: CgroupSnapshot,
    window_peak: Result<WindowPeak, String>,
    /// The maximum `memory.current` over the sampled boundaries.
    max_sampled_current_bytes: Option<u64>,
}

impl CgroupState {
    fn sample(&mut self) {
        let current = self.handle.memory_current();
        self.max_sampled_current_bytes = self.max_sampled_current_bytes.max(current);
    }
}

impl WindowMemory {
    /// Opens the window: resets the peak counters and takes the "before" readings. Off Linux
    /// this does nothing and every later reading is `null`.
    pub fn start() -> Self {
        let peak_rss_reset = if cfg!(target_os = "linux") {
            reset_peak_rss().map_err(|error| format!("write 5 to /proc/self/clear_refs: {error}"))
        } else {
            Err("not Linux".to_owned())
        };
        let cgroup = Cgroup::own().map(|handle| {
            let window_peak = WindowPeak::reset(&handle);
            let mut state = CgroupState {
                before: handle.snapshot(),
                handle,
                window_peak,
                max_sampled_current_bytes: None,
            };
            state.sample();
            state
        });
        Self {
            peak_rss_reset,
            cgroup,
        }
    }

    /// Records `memory.current` at a refresh boundary, for the sampled maximum.
    pub fn sample_boundary(&mut self) {
        if let Some(cgroup) = &mut self.cgroup {
            cgroup.sample();
        }
    }

    /// Closes the window. Returns the `sequence` fields `window_peak_rss_bytes`,
    /// `window_end_rss_bytes`, `window_peak_rss_error` and `cgroup`.
    pub fn finish(mut self) -> Map<String, Value> {
        let memory = process_memory();
        let peak_rss_bytes = match (&self.peak_rss_reset, memory) {
            (Ok(()), Some(memory)) => Some(memory.peak_rss_bytes),
            _ => None,
        };
        let mut fields = Map::new();
        fields.insert("window_peak_rss_bytes".into(), json!(peak_rss_bytes));
        fields.insert(
            "window_end_rss_bytes".into(),
            json!(memory.map(|memory| memory.rss_bytes)),
        );
        fields.insert(
            "window_peak_rss_error".into(),
            json!(self.peak_rss_reset.as_ref().err()),
        );
        let cgroup = self.cgroup.as_mut().map(|state| {
            state.sample();
            let after = state.handle.snapshot();
            let (window_peak, peak_error) = match &mut state.window_peak {
                Ok(peak) => match peak.read() {
                    Ok(bytes) => (Some(bytes), None),
                    Err(error) => (None, Some(error)),
                },
                Err(error) => (None, Some(error.clone())),
            };
            let lifetime_peak = std::fs::read_to_string(state.handle.file("memory.peak"))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok());
            json!({
                "path": state.handle.directory.display().to_string(),
                "before": state.before.to_json(),
                "after": after.to_json(),
                "delta": state.before.delta(&after),
                "peak": {
                    "window_peak_supported": window_peak.is_some(),
                    "window_peak_error": peak_error,
                    "window_peak_bytes": window_peak,
                    "lifetime_peak_at_end_bytes": lifetime_peak,
                    "max_sampled_memory_current_bytes": state.max_sampled_current_bytes,
                    "note": "memory.peak is reset by writing to it (kernel 6.12 and later) and read through the same open file; window_peak_bytes is null if the reset failed. lifetime_peak_at_end_bytes is a fresh read and covers the cgroup's life, including the input copy and loading. The sampled maximum covers only the window start, each refresh end and the window end.",
                },
            })
        });
        fields.insert("cgroup".into(), json!(cgroup));
        fields
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tq3-ivm-baseline\nVmPeak:\t  900000 kB\nVmSize:\t  800000 kB\nVmHWM:\t  123456 kB\nVmRSS:\t   65432 kB\nRssAnon:\t   60000 kB\nThreads:\t12\n";

    const MEMORY_STAT: &str = "anon 1000\nfile 2000\nkernel 300\npgfault 40\npgmajfault 5\npgscan 600\npgsteal 70\nworkingset_refault_anon 8\nworkingset_refault_file 9\nworkingset_activate_anon 1\n";

    const MEMORY_EVENTS: &str = "low 0\nhigh 0\nmax 12\noom 1\noom_kill 0\n";

    const MEMORY_PRESSURE: &str = "some avg10=1.50 avg60=0.25 avg300=0.05 total=123456\nfull avg10=0.00 avg60=0.10 avg300=0.00 total=7890\n";

    #[test]
    fn proc_status_gives_hwm_and_rss_in_bytes() {
        assert_eq!(
            parse_proc_status(STATUS),
            Some(ProcessMemory {
                peak_rss_bytes: 123_456 * 1024,
                rss_bytes: 65_432 * 1024,
            })
        );
    }

    #[test]
    fn proc_status_needs_both_fields_in_kibibytes() {
        assert_eq!(parse_proc_status("VmHWM:\t 1 kB\n"), None);
        assert_eq!(parse_proc_status("VmHWM:\t 1 kB\nVmRSS:\t 2 MB\n"), None);
        assert_eq!(parse_proc_status("VmHWM:\t x kB\nVmRSS:\t 2 kB\n"), None);
        assert_eq!(parse_proc_status(""), None);
    }

    #[test]
    fn flat_keyed_files_parse_and_report_malformed_lines() {
        let (events, malformed) = parse_flat_keyed(MEMORY_EVENTS);
        assert_eq!(events["max"], 12);
        assert_eq!(events["oom"], 1);
        assert_eq!(events.len(), 5);
        assert!(malformed.is_empty());
        let (values, malformed) = parse_flat_keyed("a 1\nb two\nc 3 4\n\nd 5\n");
        assert_eq!(values.keys().collect::<Vec<_>>(), ["a", "d"]);
        assert_eq!(malformed, ["b two", "c 3 4"]);
    }

    #[test]
    fn pressure_parses_some_and_full() {
        let lines = parse_pressure(MEMORY_PRESSURE).unwrap();
        assert_eq!(
            lines["some"],
            PressureLine {
                avg10: 1.5,
                avg60: 0.25,
                avg300: 0.05,
                total_microseconds: 123_456,
            }
        );
        assert_eq!(lines["full"].total_microseconds, 7_890);
        assert_eq!(parse_pressure(""), None);
        assert_eq!(parse_pressure("some avg10=1 total=2\n"), None);
        assert_eq!(
            parse_pressure("some avg10=x avg60=0 avg300=0 total=2\n"),
            None
        );
    }

    #[test]
    fn own_cgroup_is_the_v2_line() {
        assert_eq!(
            parse_own_cgroup("0::/user.slice/run-1\n"),
            Some("/user.slice/run-1")
        );
        assert_eq!(
            parse_own_cgroup("12:memory:/old\n0::/a/b (deleted)\n"),
            Some("/a/b")
        );
        assert_eq!(parse_own_cgroup("0::/\n"), Some("/"));
        assert_eq!(parse_own_cgroup("4:memory:/old\n3:cpu:/old\n"), None);
    }

    fn snapshot(current: &str, events: &str, stat: &str, pressure: &str) -> CgroupSnapshot {
        CgroupSnapshot::from_contents(
            Some(current),
            Some(events),
            Some(stat),
            Some(pressure),
            Vec::new(),
        )
    }

    #[test]
    fn snapshot_keeps_the_named_stat_subset() {
        let snapshot = snapshot("4096\n", MEMORY_EVENTS, MEMORY_STAT, MEMORY_PRESSURE);
        assert_eq!(snapshot.memory_current_bytes, Some(4096));
        let stat = snapshot.stat.as_ref().unwrap();
        assert_eq!(stat["pgscan"], 600);
        assert_eq!(stat["workingset_refault_file"], 9);
        assert!(!stat.contains_key("kernel"));
        assert!(!stat.contains_key("workingset_activate_anon"));
        assert!(snapshot.problems.is_empty());
        assert_eq!(
            snapshot.to_json()["memory_pressure"]["some"]["total_microseconds"],
            123_456
        );
    }

    #[test]
    fn delta_subtracts_counters_and_tolerates_a_decrease() {
        let before = snapshot("4096", MEMORY_EVENTS, MEMORY_STAT, MEMORY_PRESSURE);
        let after = snapshot(
            "1024",
            "low 0\nhigh 3\nmax 15\noom 1\noom_kill 0\n",
            "anon 900\nfile 5000\npgscan 650\npgsteal 90\nworkingset_refault_anon 18\n",
            "some avg10=0.00 avg60=0.00 avg300=0.00 total=123556\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=7890\n",
        );
        let delta = before.delta(&after);
        assert_eq!(delta["memory_current_bytes"], -3072);
        assert_eq!(delta["memory_events"]["max"], 3);
        assert_eq!(delta["memory_events"]["high"], 3);
        assert_eq!(delta["memory_stat"]["pgscan"], 50);
        assert_eq!(delta["memory_stat"]["pgsteal"], 20);
        assert_eq!(delta["memory_stat"]["anon"], -100);
        assert_eq!(delta["memory_stat"]["workingset_refault_anon"], 10);
        // A key present on one side only is left out rather than guessed.
        assert!(delta["memory_stat"]
            .get("workingset_refault_file")
            .is_none());
        assert_eq!(delta["memory_pressure_total_microseconds"]["some"], 100);
        assert_eq!(delta["memory_pressure_total_microseconds"]["full"], 0);
    }

    #[test]
    fn missing_files_are_null_and_malformed_ones_are_recorded() {
        let none = CgroupSnapshot::from_contents(None, None, None, None, Vec::new());
        assert_eq!(none, CgroupSnapshot::default());
        let json = none.delta(&none);
        assert!(json["memory_current_bytes"].is_null());
        assert!(json["memory_events"].is_null());
        assert!(json["memory_pressure_total_microseconds"].is_null());
        let bad = CgroupSnapshot::from_contents(
            Some("lots"),
            Some("max 1\nbroken\n"),
            None,
            Some("garbage"),
            Vec::new(),
        );
        assert_eq!(bad.memory_current_bytes, None);
        assert_eq!(bad.events.as_ref().unwrap()["max"], 1);
        assert_eq!(bad.problems.len(), 3, "{:?}", bad.problems);
        // A one-sided snapshot has no delta.
        assert!(bad.delta(&none)["memory_events"].is_null());
    }

    #[test]
    fn window_peak_resets_and_reads_through_one_file() {
        // A regular file stands in for memory.peak: writing succeeds and reading returns
        // whatever the file then holds.
        let directory = std::env::temp_dir().join(format!("window-peak-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("memory.peak"), "").unwrap();
        let cgroup = Cgroup::at(&directory).unwrap();
        let mut peak = WindowPeak::reset(&cgroup).unwrap();
        assert!(peak.read().unwrap_err().contains("not a number"));
        std::fs::write(directory.join("memory.peak"), "8192\n").unwrap();
        assert_eq!(peak.read().unwrap(), 8192);
        // Without the file the reset is unsupported, and says why.
        std::fs::remove_file(directory.join("memory.peak")).unwrap();
        assert!(WindowPeak::reset(&cgroup).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn eviction_reports_failures_and_deduplicates() {
        let missing = PathBuf::from("/nonexistent/window-memory-input");
        let report = evict_inputs(&[missing.clone(), missing]);
        assert_eq!(report["evicted"], false);
        assert_eq!(report["failures"].as_array().unwrap().len(), 1);
        assert_eq!(report["files"].as_array().unwrap().len(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eviction_of_a_real_file_succeeds_on_linux() {
        let path = std::env::temp_dir().join(format!("evict-{}", std::process::id()));
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let report = evict_inputs(&[path.clone()]);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(report["evicted"], true, "{report}");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn window_readings_are_null_off_linux() {
        assert!(process_memory().is_none());
        assert!(Cgroup::own().is_none());
        let fields = WindowMemory::start().finish();
        assert!(fields["window_peak_rss_bytes"].is_null());
        assert!(fields["window_end_rss_bytes"].is_null());
        assert!(fields["cgroup"].is_null());
        assert!(fields["window_peak_rss_error"].is_string());
    }
}
