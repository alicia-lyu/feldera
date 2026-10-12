//! A spine's owner reaches its background compaction, and the compaction's
//! files are counted for that owner.
//!
//! The counters and logs are process-wide, so this must be the only test in
//! this file.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dbsp::algebra::OrdZSet;
use dbsp::circuit::{CircuitConfig, CircuitStorageConfig};
use dbsp::dynamic::DynData;
use dbsp::storage::diagnostics::{self, RunWriteKind};
use dbsp::storage::write_attribution::{WriteOwner, with_write_owner, write_snapshot};
use dbsp::trace::{BatchReaderFactories, Spine, Trace, TraceRole};
use dbsp::utils::Tup2;
use dbsp::{OrdZSet as TypedOrdZSet, Runtime, ZWeight};
use feldera_storage::tokio::TOKIO;
use feldera_types::config::{StorageConfig, StorageOptions};

/// Number of one-record runs inserted into each spine: enough to reach the
/// level-0 threshold that starts a background compaction.
const RUNS: i32 = 12;

/// Inserts `RUNS` runs into a new spine created under `owner`, waits for the
/// compaction that this triggers, and returns the spine's name.
fn fill_and_compact(owner: WriteOwner, name: &str) {
    let factories = BatchReaderFactories::new::<i32, (), ZWeight>();
    let mut spine = with_write_owner(owner, || {
        Spine::<OrdZSet<DynData>>::new(&factories, Arc::new(name.to_owned()), TraceRole::Integral)
    });
    // The owner must come from the spine, not from the thread that inserts:
    // insert from outside every scope.
    for key in 0..RUNS {
        let batch = TypedOrdZSet::<i32>::from_tuples((), vec![Tup2(Tup2(key, ()), 1)]);
        TOKIO.block_on(spine.insert(batch.into_inner()));
    }
    spine.initiate_compaction();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !spine.is_compaction_complete() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        spine.is_compaction_complete(),
        "{name}: compaction did not finish"
    );
}

#[test]
fn compaction_inherits_the_spines_owner() {
    diagnostics::enable();
    let directory = tempfile::tempdir().unwrap();
    let config = CircuitConfig::with_workers(1).with_storage(Some(
        CircuitStorageConfig::for_config(
            StorageConfig {
                path: directory.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                ..StorageOptions::default()
            },
        )
        .unwrap(),
    ));
    let finished = Arc::new(Mutex::new(false));
    let report = finished.clone();
    let (handle, ()) = Runtime::init_circuit(config, move |_circuit| {
        // Two spines with different owners run at once, so the test fails if
        // an owner leaks from one spine's tasks to the other's.
        fill_and_compact(WriteOwner::MergedIndex, "merged_index_spine");
        fill_and_compact(WriteOwner::BaseRelation, "base_relation_spine");
        fill_and_compact(WriteOwner::Query, "query_spine");
        *report.lock().unwrap() = true;
        Ok(())
    })
    .unwrap();
    assert!(*finished.lock().unwrap());
    handle.kill().unwrap();

    let compactions = diagnostics::compaction_log();
    let flushes = diagnostics::flush_log();
    for (spine, owner) in [
        ("merged_index_spine", WriteOwner::MergedIndex),
        ("base_relation_spine", WriteOwner::BaseRelation),
        ("query_spine", WriteOwner::Query),
    ] {
        let records: Vec<_> = compactions
            .iter()
            .filter(|record| record.spine == spine)
            .collect();
        assert!(!records.is_empty(), "{spine} logged no compaction");
        assert!(
            records.iter().all(|record| record.owner == owner),
            "{spine}: {records:?}"
        );
        // Every compaction's output file was flushed under the same owner.
        let mut files = 0;
        for record in &records {
            if let Some(file) = &record.output_file {
                files += 1;
                let writes: Vec<_> = flushes.iter().filter(|flush| &flush.file == file).collect();
                assert!(!writes.is_empty(), "{file} was never flushed");
                assert!(
                    writes.iter().all(|flush| flush.owner == owner),
                    "{spine}: {writes:?}"
                );
            }
        }
        assert!(files > 0, "{spine} wrote no compaction output to storage");
    }
    assert!(
        compactions
            .iter()
            .any(|record| record.kind == RunWriteKind::RequestedCompaction),
        "the requested full compaction is logged apart from background compactions"
    );

    // Counted per owner, and the owners add up to the totals.
    let snapshot = write_snapshot();
    assert_eq!(snapshot.owners_sum(), snapshot.total);
    for owner in WriteOwner::ALL {
        assert!(snapshot.owner(owner).bytes > 0, "{owner:?} wrote nothing");
    }
    let logged: u64 = flushes.iter().map(|flush| flush.bytes).sum();
    assert_eq!(logged, snapshot.total.bytes);
    assert_eq!(flushes.len() as u64, snapshot.total.operations);
}
