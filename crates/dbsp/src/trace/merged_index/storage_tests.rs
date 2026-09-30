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

use super::{
    CustomerOrdersLineitemIndex, CustomerPayload, LineitemPayload, OrdersPayload, SourceKey,
    SourcePayload,
};
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
const CUSTOMERS_PER_BATCH: usize = 10_000;
type SourceRow = (SourceKey, SourcePayload, ZWeight);

fn customer_segment(id: i32) -> String {
    let segment = format!("{id:08}-{:0<247}", "segment");
    assert_eq!(segment.len(), 256);
    segment
}

fn customer_group(c: i32) -> Vec<SourceRow> {
    let mut rows = vec![(
        SourceKey::Customer(c),
        SourcePayload::Customer(CustomerPayload {
            segment: customer_segment(c),
        }),
        1,
    )];
    let order_day = c % 365 - 180;
    for j in 0..2 {
        let order_id = 1_000_000 + 2 * c + j;
        rows.push((
            SourceKey::Orders(c, order_id),
            SourcePayload::Orders(OrdersPayload {
                order_day,
                ship_priority: j,
            }),
            1,
        ));
        for line_id in 1..=2 {
            rows.push((
                SourceKey::Lineitem(c, order_id, line_id),
                SourcePayload::Lineitem(LineitemPayload {
                    ship_day: order_day + line_id,
                    extended_price_cents: 10_000 + i64::from(order_id + line_id),
                    discount_hundredths: i64::from(c % 101),
                }),
                1,
            ));
        }
    }
    rows
}

// The signed oracle encodes source rows independently of batch construction.
fn encoded_row(index: &CustomerOrdersLineitemIndex, row: &SourceRow) -> Row {
    (
        index.fold(&row.0).unwrap().as_bytes().to_vec(),
        index.encode_payload(&row.1).unwrap().as_bytes().to_vec(),
        row.2,
    )
}

fn typed_row(index: &CustomerOrdersLineitemIndex, row: &Row) -> SourceRow {
    (
        index.unfold(&row.0).unwrap(),
        index.decode_payload(&row.0, &row.1).unwrap(),
        row.2,
    )
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
fn native_spine_compacts_full_col_runs_to_file_and_preserves_snapshots() {
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
            let first_id = (batch_number * CUSTOMERS_PER_BATCH) as i32;
            let source_rows: Vec<_> = (first_id..first_id + CUSTOMERS_PER_BATCH as i32)
                .flat_map(customer_group)
                .collect();
            let rows: Vec<_> = source_rows
                .iter()
                .map(|row| encoded_row(&index, row))
                .collect();
            let batch = index.build_batch(source_rows).unwrap();
            assert_eq!(batch.location(), BatchLocation::Memory);
            assert_eq!(batch.len(), 7 * CUSTOMERS_PER_BATCH);
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

        let before_updates = trace.ro_snapshot();
        let mut oracle = BTreeMap::new();
        for id in 0..(BATCHES * CUSTOMERS_PER_BATCH) as i32 {
            for row in customer_group(id) {
                apply(&mut oracle, &encoded_row(&index, &row));
            }
        }
        let expected_before: Vec<_> = oracle
            .iter()
            .map(|((key, value), weight)| (key.clone(), value.clone(), *weight))
            .collect();
        assert_eq!(read_rows(&before_updates), expected_before);
        assert_eq!(expected_before.len(), 1_120_000);
        let file_rows = read_rows(file_batch.as_ref());
        let reopened_rows = read_rows(&reopened);
        assert_eq!(reopened_rows, file_rows);
        let typed_file: Vec<_> = file_rows.iter().map(|row| typed_row(&index, row)).collect();
        assert!(
            typed_file
                .iter()
                .any(|row| matches!(row.0, SourceKey::Customer(_)))
        );
        assert!(
            typed_file
                .iter()
                .any(|row| matches!(row.0, SourceKey::Orders(_, _)))
        );
        assert!(
            typed_file
                .iter()
                .any(|row| matches!(row.0, SourceKey::Lineitem(_, _, _)))
        );
        assert_eq!(
            reopened_rows
                .iter()
                .map(|row| typed_row(&index, row))
                .collect::<Vec<_>>(),
            typed_file
        );
        for row in &file_rows {
            assert_eq!(oracle.get(&(row.0.clone(), row.1.clone())), Some(&row.2));
        }

        drop(typed_file);
        drop(reopened_rows);
        drop(file_rows);

        let mut updates = Vec::new();
        for c in [10, 20, 30] {
            updates.extend(
                customer_group(c)
                    .into_iter()
                    .map(|(key, payload, _)| (key, payload, -1)),
            );
        }
        updates.extend(
            customer_group(20)
                .into_iter()
                .map(|(key, mut payload, weight)| {
                    match &mut payload {
                        SourcePayload::Customer(value) => value.segment = "replacement".into(),
                        SourcePayload::Orders(value) => {
                            value.order_day += 1;
                            value.ship_priority += 2;
                        }
                        SourcePayload::Lineitem(value) => {
                            value.ship_day += 1;
                            value.extended_price_cents += 100;
                            value.discount_hundredths = (value.discount_hundredths + 1) % 101;
                        }
                    }
                    (key, payload, weight)
                }),
        );
        // Move the complete group, preserving payloads while assigning new order IDs.
        updates.extend(
            customer_group(30)
                .into_iter()
                .map(|(key, payload, weight)| {
                    let key = match key {
                        SourceKey::Customer(_) => SourceKey::Customer(160_000),
                        SourceKey::Orders(_, order) => {
                            SourceKey::Orders(160_000, order + 2 * (160_000 - 30))
                        }
                        SourceKey::Lineitem(_, order, line) => {
                            SourceKey::Lineitem(160_000, order + 2 * (160_000 - 30), line)
                        }
                    };
                    (key, payload, weight)
                }),
        );
        TOKIO.block_on(trace.insert(index.build_batch(updates.clone()).unwrap()));
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
            apply(&mut oracle, &encoded_row(&index, row));
        }
        let expected: Vec<_> = oracle
            .into_iter()
            .map(|((key, value), weight)| (key, value, weight))
            .collect();
        assert_eq!(expected.len(), 1_120_000 - 7);
        assert_eq!(read_rows(&mixed), expected);
        assert_eq!(read_rows(&before_updates), expected_before);
        assert_eq!(read_rows(&first_snapshot), first_rows);

        let mut cursor = mixed.cursor();
        // Check exact and absent keys for every source, including the old moved keys.
        for c in [20, 160_000, 10, 30, 200_000] {
            for (key, _, _) in customer_group(c) {
                let folded = index.fold(&key).unwrap().as_bytes().to_vec();
                cursor.rewind_keys();
                assert_eq!(
                    cursor.seek_key_exact(folded.erase(), None),
                    c == 20 || c == 160_000
                );
                if cursor.key_valid() && (c == 20 || c == 160_000) {
                    assert_eq!(cursor.key().downcast_checked::<Vec<u8>>(), &folded);
                }
            }
        }
        cursor.rewind_keys();
        assert_eq!(read_cursor(&mut cursor), expected);
        for c in [20, 40, 160_000] {
            let order = 1_000_000 + 2 * c;
            for (prefix, count) in [
                (index.customer_prefix(c), 7),
                (index.order_prefix(c, order), 3),
                (index.line_prefix(c, order), 2),
            ] {
                cursor.rewind_keys();
                cursor.seek_key(prefix.erase());
                let mut observed = Vec::new();
                while cursor.key_valid()
                    && cursor
                        .key()
                        .downcast_checked::<Vec<u8>>()
                        .starts_with(&prefix)
                {
                    let key = cursor.key().downcast_checked::<Vec<u8>>().clone();
                    while cursor.val_valid() {
                        observed.push((
                            key.clone(),
                            cursor.val().downcast_checked::<Vec<u8>>().clone(),
                            *cursor.weight().downcast_checked::<ZWeight>(),
                        ));
                        cursor.step_val();
                    }
                    cursor.step_key();
                }
                let wanted: Vec<_> = expected
                    .iter()
                    .filter(|row| row.0.starts_with(&prefix))
                    .cloned()
                    .collect();
                assert_eq!(observed.len(), count);
                assert_eq!(observed, wanted);
            }
        }
        cursor.rewind_keys();
        assert_eq!(cursor.key().downcast_checked::<Vec<u8>>(), &expected[0].0);
    });
}

#[test]
fn initial_col_run_stays_in_memory_when_runtime_selects_files() {
    let storage_dir = tempfile::tempdir().unwrap();
    let config = CircuitConfig::with_workers(1).with_storage(Some(
        CircuitStorageConfig::for_config(
            StorageConfig {
                path: storage_dir.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                min_step_storage_bytes: Some(0),
                ..Default::default()
            },
        )
        .unwrap(),
    ));
    run_in_circuit_with_storage_config(config, || {
        assert_eq!(Runtime::min_step_storage_bytes(), Some(0));
        let index = CustomerOrdersLineitemIndex::default();
        for rows in [Vec::new(), customer_group(1)] {
            let batch = index.build_batch(rows.clone()).unwrap();
            assert_eq!(batch.location(), BatchLocation::Memory);
            let expected: Vec<_> = rows.iter().map(|row| encoded_row(&index, row)).collect();
            assert_eq!(read_rows(&batch), expected);
        }
        // Confirm this runtime would place a generic nonempty builder on disk.
        let factories = OrdIndexedWSetFactories::new::<Vec<u8>, Vec<u8>, ZWeight>();
        let mut generic = <IndexBatch as Batch>::Builder::with_capacity(&factories, 1, 1);
        generic.push_val_diff(vec![1_u8].erase(), 1_i64.erase());
        generic.push_key(vec![1_u8].erase());
        assert_eq!(generic.done().location(), BatchLocation::Storage);
    });
}
