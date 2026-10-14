//! Storage writes are counted per owner at the same place as the backend's
//! totals, and a file keeps the owner it was created under.
//!
//! The counters are process-wide, so this must be the only test in this file:
//! a second test would run concurrently and write into the same counters.

use std::sync::Arc;

use dbsp::storage::diagnostics::{self, FlushRecord};
use dbsp::storage::write_attribution::{WriteCounts, WriteOwner, with_write_owner, write_snapshot};
use feldera_storage::{StorageBackend, fbuf::FBuf};
use feldera_types::config::{FileBackendConfig, StorageCacheConfig};

const BLOCK_BYTES: usize = 4096;

/// Writes `blocks` blocks of `BLOCK_BYTES` to a new file and finishes it.
///
/// The file is created under `owner` but written by whichever thread calls
/// this function.
fn write_file(backend: &Arc<dyn StorageBackend>, owner: WriteOwner, blocks: usize) -> String {
    let mut writer = with_write_owner(owner, || backend.create().unwrap());
    for _ in 0..blocks {
        writer
            .write_block(FBuf::from_slice(&[0xa5; BLOCK_BYTES]))
            .unwrap();
    }
    let reader = writer.complete().unwrap();
    reader.path().to_string()
}

fn records_for(log: &[FlushRecord], file: &str) -> Vec<FlushRecord> {
    log.iter()
        .filter(|record| record.file == file)
        .cloned()
        .collect()
}

#[test]
fn owners_sum_to_the_totals_and_a_file_keeps_its_owner() {
    diagnostics::enable();
    let directory = tempfile::tempdir().unwrap();
    let backend: Arc<dyn StorageBackend> =
        Arc::new(dbsp::storage::backend::posixio_impl::PosixBackend::new(
            directory.path(),
            StorageCacheConfig::default(),
            &FileBackendConfig::default(),
        ));
    let before = write_snapshot();
    assert_eq!(
        before.total,
        WriteCounts::default(),
        "the test must own the process to read global counters"
    );

    // Ordinary writes under each owner. 300 blocks of 4 KiB cross the writer's
    // 1 MiB flush threshold, so one file takes several flushes.
    let query_file = write_file(&backend, WriteOwner::Query, 300);
    let base_file = write_file(&backend, WriteOwner::BaseRelation, 2);

    // A late flush: the owner is set only while the file is created, and the
    // file is written and completed afterwards on another thread, outside any
    // scope. The writes still belong to the owner the file was created under.
    let late_file = {
        let writer = with_write_owner(WriteOwner::MergedIndex, || backend.create().unwrap());
        std::thread::spawn(move || {
            let mut writer = writer;
            for _ in 0..3 {
                writer
                    .write_block(FBuf::from_slice(&[0x5a; BLOCK_BYTES]))
                    .unwrap();
            }
            writer.complete().unwrap().path().to_string()
        })
        .join()
        .unwrap()
    };

    let after = write_snapshot();
    assert_eq!(
        after.owners_sum(),
        after.total,
        "the owners must add up to the backend's totals"
    );

    // Native-style workloads never touch the merged-index owner: only the file
    // created under that owner counts for it.
    let merged_index = after.owner(WriteOwner::MergedIndex);
    assert_eq!(merged_index.bytes, 3 * BLOCK_BYTES as u64);
    assert_eq!(
        merged_index.operations, 1,
        "three blocks flush once, at completion"
    );
    assert_eq!(
        after.owner(WriteOwner::BaseRelation).bytes,
        2 * BLOCK_BYTES as u64
    );
    assert_eq!(
        after.owner(WriteOwner::Query).bytes,
        300 * BLOCK_BYTES as u64
    );

    // The flush log sums to the counters, and every record carries the owner
    // of its file.
    let log = diagnostics::flush_log();
    let logged_bytes: u64 = log.iter().map(|record| record.bytes).sum();
    assert_eq!(logged_bytes, after.total.bytes);
    assert_eq!(log.len() as u64, after.total.operations);
    for (file, owner, bytes) in [
        (&query_file, WriteOwner::Query, 300 * BLOCK_BYTES as u64),
        (&base_file, WriteOwner::BaseRelation, 2 * BLOCK_BYTES as u64),
        (&late_file, WriteOwner::MergedIndex, 3 * BLOCK_BYTES as u64),
    ] {
        let records = records_for(&log, file);
        assert!(!records.is_empty(), "{file} was never flushed");
        assert!(records.iter().all(|record| record.owner == owner), "{file}");
        assert_eq!(
            records.iter().map(|record| record.bytes).sum::<u64>(),
            bytes
        );
        assert!(records.iter().all(|record| record.succeeded));
    }
    assert!(
        records_for(&log, &query_file).len() > 1,
        "a file past the flush threshold flushes more than once"
    );
}
