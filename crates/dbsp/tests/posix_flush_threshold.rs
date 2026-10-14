//! The POSIX writer buffers blocks and flushes once it holds 1 MiB. After a
//! flush it must start counting from zero again; otherwise every later block
//! is flushed on its own.
//!
//! The flush log is process-wide, so this must be the only test in this file.

use std::sync::Arc;

use dbsp::storage::diagnostics;
use feldera_storage::{StorageBackend, fbuf::FBuf};
use feldera_types::config::{FileBackendConfig, StorageCacheConfig};

const BLOCK_BYTES: usize = 4096;
const FLUSH_THRESHOLD_BYTES: usize = 1024 * 1024;

#[test]
fn writer_flushes_once_per_threshold() {
    diagnostics::enable();
    let directory = tempfile::tempdir().unwrap();
    let backend: Arc<dyn StorageBackend> =
        Arc::new(dbsp::storage::backend::posixio_impl::PosixBackend::new(
            directory.path(),
            StorageCacheConfig::default(),
            &FileBackendConfig::default(),
        ));

    // 2.5 thresholds' worth of blocks: two full flushes, then the remainder
    // at completion.
    let blocks = FLUSH_THRESHOLD_BYTES * 5 / 2 / BLOCK_BYTES;
    let mut writer = backend.create().unwrap();
    for _ in 0..blocks {
        writer
            .write_block(FBuf::from_slice(&[0xa5; BLOCK_BYTES]))
            .unwrap();
    }
    let file = writer.complete().unwrap().path().to_string();

    let flush_bytes: Vec<u64> = diagnostics::flush_log()
        .into_iter()
        .filter(|record| record.file == file)
        .map(|record| record.bytes)
        .collect();
    assert_eq!(
        flush_bytes.iter().sum::<u64>(),
        (blocks * BLOCK_BYTES) as u64,
        "every buffered byte is written exactly once"
    );
    let total_bytes = blocks * BLOCK_BYTES;
    assert_eq!(
        flush_bytes.len(),
        total_bytes.div_ceil(FLUSH_THRESHOLD_BYTES),
        "expected one flush per full threshold plus one at completion, got {flush_bytes:?}"
    );
    let (last, full) = flush_bytes.split_last().unwrap();
    assert!(
        full.iter()
            .all(|&bytes| bytes == FLUSH_THRESHOLD_BYTES as u64),
        "each flush before completion holds exactly one threshold: {flush_bytes:?}"
    );
    assert!(*last > 0);
}
