use std::{
    collections::BTreeMap,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use feldera_storage::tokio::TOKIO;
use feldera_types::{
    config::{StorageConfig, StorageOptions},
    memory_pressure::MemoryPressure,
};

use super::{CustomerOrdersLineitemIndex, CustomerPayload, SourceKey, SourcePayload};
use crate::{
    DynZWeight, Runtime, ZWeight,
    circuit::{CircuitConfig, CircuitStorageConfig},
    dynamic::{DowncastTrait, DynData, Erase},
    trace::{
        Batch, BatchLocation, BatchReader, BatchReaderFactories, Builder, Cursor, OrdIndexedWSet,
        OrdIndexedWSetFactories, Spine, Trace, TraceRole, WithSnapshot,
        test::run_in_circuit_with_storage_config,
    },
};

type IndexBatch = OrdIndexedWSet<DynData, DynData, DynZWeight>;
type Row = (Vec<u8>, Vec<u8>, ZWeight);

const BATCHES: usize = 16;
const ROWS_PER_BATCH: usize = 10_000;

fn customer_segment(id: i32) -> String {
    let segment = format!("{id:08}-{:0<247}", "segment");
    assert_eq!(segment.len(), 256);
    segment
}

fn customer_row(index: &CustomerOrdersLineitemIndex, id: i32) -> Row {
    let key = index.fold(&SourceKey::Customer(id)).unwrap();
    let payload = index
        .encode_payload(&SourcePayload::Customer(CustomerPayload {
            segment: customer_segment(id),
        }))
        .unwrap();
    (key.as_bytes().to_vec(), payload.as_bytes().to_vec(), 1)
}

fn make_batch(
    factories: &OrdIndexedWSetFactories<DynData, DynData, DynZWeight>,
    rows: &[Row],
) -> IndexBatch {
    let mut rows = rows.to_vec();
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut builder =
        <IndexBatch as Batch>::Builder::with_capacity(factories, rows.len(), rows.len());
    let mut prior_key: Option<&[u8]> = None;
    for (key, value, weight) in &rows {
        if prior_key.is_some_and(|prior| prior != key.as_slice()) {
            builder.push_key(prior_key.unwrap().to_vec().erase());
        }
        builder.push_val_diff(value.erase(), weight.erase());
        prior_key = Some(key);
    }
    if let Some(key) = prior_key {
        builder.push_key(key.to_vec().erase());
    }
    builder.done()
}

fn read_rows(
    batch: &impl BatchReader<Key = DynData, Val = DynData, Time = (), R = DynZWeight>,
) -> Vec<Row> {
    let mut cursor = batch.cursor();
    read_cursor(&mut cursor)
}

fn read_cursor(cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>) -> Vec<Row> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        let key = cursor.key().downcast_checked::<Vec<u8>>().clone();
        while cursor.val_valid() {
            rows.push((
                key.clone(),
                cursor.val().downcast_checked::<Vec<u8>>().clone(),
                *cursor.weight().downcast_checked::<ZWeight>(),
            ));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

fn apply(oracle: &mut BTreeMap<(Vec<u8>, Vec<u8>), ZWeight>, row: &Row) {
    let entry = oracle.entry((row.0.clone(), row.1.clone())).or_default();
    *entry += row.2;
    if *entry == 0 {
        oracle.remove(&(row.0.clone(), row.1.clone()));
    }
}

#[test]
fn native_spine_compacts_folded_customer_runs_to_file_and_preserves_snapshots() {
    let storage_dir = tempfile::tempdir().unwrap();
    let config = CircuitConfig::with_workers(1).with_storage(Some(
        CircuitStorageConfig::for_config(
            StorageConfig {
                path: storage_dir.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            StorageOptions::default(),
        )
        .unwrap(),
    ));
    run_in_circuit_with_storage_config(config, || {
        let index = CustomerOrdersLineitemIndex::default();
        let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
            Vec<u8>,
            Vec<u8>,
            ZWeight,
        >();
        let mut trace = Spine::<IndexBatch>::new(
            &factories,
            Arc::new("merged_index_storage_test".to_owned()),
            TraceRole::Integral,
        );
        let threshold = Runtime::min_merge_storage_bytes().expect("storage enabled");
        assert_eq!(Runtime::memory_pressure(), Some(MemoryPressure::Low));
        assert!(threshold > 0, "the fixture requires low memory pressure");

        let mut first_snapshot = None;
        let mut first_rows = Vec::new();
        let mut first_eight_bytes = 0;
        for batch_number in 0..BATCHES {
            let first_id = (batch_number * ROWS_PER_BATCH) as i32;
            let rows: Vec<_> = (first_id..first_id + ROWS_PER_BATCH as i32)
                .map(|id| customer_row(&index, id))
                .collect();
            let batch = make_batch(&factories, &rows);
            assert_eq!(batch.location(), BatchLocation::Memory);
            if batch_number < 8 {
                first_eight_bytes += batch.approximate_byte_size();
            }
            TOKIO.block_on(trace.insert(batch));
            if batch_number == 0 {
                first_rows = rows;
                first_snapshot = Some(trace.ro_snapshot());
            }
        }
        assert!(
            first_eight_bytes > threshold,
            "eight batches occupy {first_eight_bytes} bytes, merge threshold {threshold} bytes"
        );

        let first_snapshot = first_snapshot.unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let file_batch = loop {
            let snapshot = trace.ro_snapshot();
            if let Some(batch) = snapshot
                .batches()
                .iter()
                .find(|batch| batch.location() == BatchLocation::Storage)
            {
                break Arc::clone(batch);
            }
            if Instant::now() >= deadline {
                let batches: Vec<_> = snapshot
                    .batches()
                    .iter()
                    .map(|batch| (batch.location(), batch.len(), batch.approximate_byte_size()))
                    .collect();
                panic!(
                    "no file batch after 30s: batches={batches:?}, first_eight_bytes={first_eight_bytes}, threshold={threshold}"
                );
            }
            thread::sleep(Duration::from_millis(100));
        };
        eprintln!(
            "merged-index storage: threshold={threshold}, first_eight_bytes={first_eight_bytes}, batches={:?}",
            trace
                .ro_snapshot()
                .batches()
                .iter()
                .map(|batch| (batch.location(), batch.len(), batch.approximate_byte_size()))
                .collect::<Vec<_>>()
        );

        assert_eq!(read_rows(&first_snapshot), first_rows);
        assert_eq!(first_snapshot.batches().len(), 1);
        assert_eq!(
            first_snapshot.batches()[0].location(),
            BatchLocation::Memory
        );

        let reader = file_batch
            .file_reader()
            .expect("file-backed batch has reader");
        let reopened = IndexBatch::from_path(&factories, reader.path()).unwrap();
        assert_eq!(reopened.location(), BatchLocation::Storage);
        assert_eq!(read_rows(&reopened), read_rows(file_batch.as_ref()));

        let before_updates = trace.ro_snapshot();
        let mut oracle = BTreeMap::new();
        for id in 0..(BATCHES * ROWS_PER_BATCH) as i32 {
            apply(&mut oracle, &customer_row(&index, id));
        }
        let expected_before: Vec<_> = oracle
            .iter()
            .map(|((key, value), weight)| (key.clone(), value.clone(), *weight))
            .collect();
        assert_eq!(read_rows(&before_updates), expected_before);
        assert_eq!(expected_before.len(), 160_000);
        for (key, value, _) in read_rows(&reopened) {
            assert_eq!(
                index.fold(&index.unfold(&key).unwrap()).unwrap().as_bytes(),
                key
            );
            assert_eq!(
                index
                    .encode_payload(&index.decode_payload(&key, &value).unwrap())
                    .unwrap()
                    .as_bytes(),
                value
            );
        }

        let updates = vec![
            {
                let mut row = customer_row(&index, 10);
                row.2 = -1;
                row
            },
            {
                let mut row = customer_row(&index, 20);
                row.2 = -1;
                row
            },
            {
                let key = index.fold(&SourceKey::Customer(20)).unwrap();
                let value = index
                    .encode_payload(&SourcePayload::Customer(CustomerPayload {
                        segment: "replacement".into(),
                    }))
                    .unwrap();
                (key.as_bytes().to_vec(), value.as_bytes().to_vec(), 1)
            },
            {
                let mut row = customer_row(&index, 30);
                row.2 = -1;
                row
            },
            {
                let key = index.fold(&SourceKey::Customer(160_000)).unwrap();
                let value = index
                    .encode_payload(&SourcePayload::Customer(CustomerPayload {
                        segment: customer_segment(30),
                    }))
                    .unwrap();
                (key.as_bytes().to_vec(), value.as_bytes().to_vec(), 1)
            },
        ];
        // Keys are already in folded order, including the moved row's new key.
        TOKIO.block_on(trace.insert(make_batch(&factories, &updates)));
        let mixed = trace.ro_snapshot();
        assert!(
            mixed
                .batches()
                .iter()
                .any(|batch| batch.location() == BatchLocation::Memory)
        );
        assert!(
            mixed
                .batches()
                .iter()
                .any(|batch| batch.location() == BatchLocation::Storage)
        );
        for row in &updates {
            apply(&mut oracle, row);
        }
        let expected: Vec<_> = oracle
            .into_iter()
            .map(|((key, value), weight)| (key, value, weight))
            .collect();
        assert_eq!(read_rows(&mixed), expected);
        assert_eq!(read_rows(&before_updates), expected_before);
        assert_eq!(read_rows(&first_snapshot), first_rows);

        let mut cursor = mixed.cursor();
        let key_20 = index.fold(&SourceKey::Customer(20)).unwrap();
        assert!(cursor.seek_key_exact(key_20.as_bytes().to_vec().erase(), None));
        assert_eq!(
            cursor.key().downcast_checked::<Vec<u8>>(),
            key_20.as_bytes()
        );
        let key_10 = index.fold(&SourceKey::Customer(10)).unwrap();
        cursor.rewind_keys();
        assert!(!cursor.seek_key_exact(key_10.as_bytes().to_vec().erase(), None));
        let missing = index.fold(&SourceKey::Customer(200_000)).unwrap();
        cursor.rewind_keys();
        assert!(!cursor.seek_key_exact(missing.as_bytes().to_vec().erase(), None));
        cursor.rewind_keys();
        assert_eq!(read_cursor(&mut cursor), expected);
        cursor.rewind_keys();
        cursor.seek_key(index.customer_prefix(20).erase());
        let prefix = index.customer_prefix(20);
        let mut count = 0;
        while cursor.key_valid()
            && cursor
                .key()
                .downcast_checked::<Vec<u8>>()
                .starts_with(&prefix)
        {
            count += 1;
            cursor.step_key();
        }
        assert_eq!(count, 1);
        assert!(cursor.key_valid());
        assert!(
            !cursor
                .key()
                .downcast_checked::<Vec<u8>>()
                .starts_with(&prefix)
        );
        cursor.rewind_keys();
        assert_eq!(cursor.key().downcast_checked::<Vec<u8>>(), &expected[0].0);
    });
}
