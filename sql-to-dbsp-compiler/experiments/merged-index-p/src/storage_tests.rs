//! MI_P runs in files: compaction, snapshots, reopened files, the part-group
//! prefix filter, and the runtime's placement of a built batch.
//!
//! Every runtime here sets both storage thresholds to zero, so each built
//! batch and each compaction's output is a file run. The schema-independent
//! storage behavior is tested at scale in merged-index-col; these tests keep
//! what is particular to MI_P: 34-byte keys, part groups, and lines that move
//! between parts.

use std::{
    collections::BTreeMap,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use feldera_storage::tokio::TOKIO;
use feldera_types::config::{StorageConfig, StorageOptions};

use super::{
    build_batch_in, encode_payload, prefix_partkey, prefix_partkey_suppkey, IndexFactories, Key,
    LineitemByPartkeySuppkeyOrderkeyLinenumberKey,
    LineitemByPartkeySuppkeyOrderkeyLinenumberPayload, PartByPartkeyKey, PartByPartkeyPayload,
    PartPartsuppLineitemDefinition, PartsuppByPartkeySuppkeyKey, PartsuppByPartkeySuppkeyPayload,
    Payload, StoredPayload, TRIE,
};
use dbsp::{
    circuit::{CircuitConfig, CircuitStorageConfig},
    dynamic::{DowncastTrait, DynData, Erase},
    trace::{
        merged_index::{batch::index_factories, MergedIndexBatch, MergedIndexDefinition, RunMerge},
        Batch, BatchLocation, BatchReader, Cursor, RunReads, Spine, SpineSnapshot, Trace,
        TraceRole, WithSnapshot,
    },
    DynZWeight, Runtime, ZWeight,
};
use feldera_sqllib::{Date, SqlDecimal};

/// A source row: a typed key, its payload, and a weight.
type SourceRow = (Key, Payload, ZWeight);
/// A stored row: a folded key, its packed payload, and a weight.
type Row = (Vec<u8>, StoredPayload, ZWeight);
/// Net weights by folded key and packed payload; no zero weights.
type Oracle = BTreeMap<(Vec<u8>, StoredPayload), ZWeight>;

/// Runs inserted into the spine. Only even parts exist; run `b` holds the
/// parts `2i` with `i % RUNS == b`. Every run, and every compaction of runs,
/// spans the whole key range, so only its prefix filter can tell that a part
/// is absent: an odd part always, an even part of another run sometimes.
const RUNS: i64 = 8;
/// Parts per run: more than `INLINE_PREFIXES` (120), so each run's prefix
/// filter is the partitioned Bloom filter.
const PARTS_PER_RUN: i64 = 300;
/// One more than the largest part inserted before the updates.
const PARTS: i64 = 2 * RUNS * PARTS_PER_RUN;
/// Rows of one part group: the part, two supplies, and two lines per supply.
const ROWS_PER_PART: usize = 7;
/// The suppliers of every part.
const SUPPLIERS: [i64; 2] = [100, 200];
/// How long the compaction test waits for a background compaction.
const COMPACTION_DEADLINE: Duration = Duration::from_secs(120);

/// A runtime with storage under `storage_dir` and the given step threshold.
/// The merge threshold is zero, so every compaction writes a file.
fn storage_config(
    storage_dir: &tempfile::TempDir,
    min_step_storage_bytes: Option<usize>,
) -> CircuitConfig {
    CircuitConfig::with_workers(1).with_storage(Some(
        CircuitStorageConfig::for_config(
            StorageConfig {
                path: storage_dir.path().to_string_lossy().into_owned(),
                cache: Default::default(),
            },
            StorageOptions {
                min_storage_bytes: Some(0),
                min_step_storage_bytes,
                ..Default::default()
            },
        )
        .unwrap(),
    ))
}

fn run_in_runtime<F>(config: CircuitConfig, f: F)
where
    F: FnOnce() + Clone + Send + 'static,
{
    let _handle = Runtime::init_circuit(config, move |_| {
        f();
        Ok(())
    })
    .unwrap();
}

/// The order of the line that part `p` gets from the supplier at `supply`.
fn order_of(p: i64, supply: usize) -> i64 {
    1_000_000 + 2 * p + supply as i64
}

fn line_key(partkey: i64, suppkey: i64, orderkey: i64, linenumber: i32) -> Key {
    Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(LineitemByPartkeySuppkeyOrderkeyLinenumberKey {
        partkey,
        suppkey,
        orderkey,
        linenumber,
    })
}

fn line_payload(orderkey: i64, linenumber: i32) -> Payload {
    Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(
        LineitemByPartkeySuppkeyOrderkeyLinenumberPayload {
            l_shipdate: Date::from_days((orderkey % 2_000) as i32 + linenumber),
            l_extendedprice: SqlDecimal::<15, 2>::new(i128::from(orderkey) * 10, 2).unwrap(),
            l_discount: SqlDecimal::<4, 2>::new(i128::from(linenumber), 2).unwrap(),
        },
    )
}

/// The rows of part `p`, each with weight 1. Every line's (orderkey,
/// linenumber) is unique across parts, as Lineitem's primary key requires.
fn part_group(p: i64) -> Vec<SourceRow> {
    let mut rows = vec![(
        Key::PartByPartkey(PartByPartkeyKey { partkey: p }),
        Payload::PartByPartkey(PartByPartkeyPayload {
            p_type: format!("TYPE {p}").into(),
        }),
        1,
    )];
    for (supply, &suppkey) in SUPPLIERS.iter().enumerate() {
        rows.push((
            Key::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyKey {
                partkey: p,
                suppkey,
            }),
            Payload::PartsuppByPartkeySuppkey(PartsuppByPartkeySuppkeyPayload {
                ps_availqty: (p % 1_000) as i32,
            }),
            1,
        ));
        let orderkey = order_of(p, supply);
        for linenumber in 1..=2 {
            rows.push((
                line_key(p, suppkey, orderkey, linenumber),
                line_payload(orderkey, linenumber),
                1,
            ));
        }
    }
    assert_eq!(rows.len(), ROWS_PER_PART);
    rows
}

/// The parts of run `run` in the compaction test.
fn parts_of_run(run: i64) -> impl Iterator<Item = i64> {
    (0..RUNS * PARTS_PER_RUN)
        .filter(move |i| i % RUNS == run)
        .map(|i| 2 * i)
}

/// The rows of `rows` with weights negated.
fn deleted(rows: Vec<SourceRow>) -> impl Iterator<Item = SourceRow> {
    rows.into_iter()
        .map(|(key, payload, weight)| (key, payload, -weight))
}

/// Folds and packs `row` independently of batch construction.
fn stored_row(row: &SourceRow) -> Row {
    (
        PartPartsuppLineitemDefinition::fold(&row.0)
            .as_bytes()
            .to_vec(),
        encode_payload(&row.1),
        row.2,
    )
}

fn apply<'a>(oracle: &mut Oracle, rows: impl IntoIterator<Item = &'a SourceRow>) {
    for row in rows {
        let (key, payload, weight) = stored_row(row);
        let entry = oracle.entry((key.clone(), payload.clone())).or_default();
        *entry += weight;
        if *entry == 0 {
            oracle.remove(&(key, payload));
        }
    }
}

fn oracle_of<'a>(rows: impl IntoIterator<Item = &'a SourceRow>) -> Oracle {
    let mut oracle = Oracle::new();
    apply(&mut oracle, rows);
    oracle
}

/// The oracle's rows in key order, or those with `prefix` if one is given.
fn expected_rows(oracle: &Oracle, prefix: Option<&[u8]>) -> Vec<Row> {
    oracle
        .iter()
        .filter(|((key, _), _)| prefix.is_none_or(|prefix| key.starts_with(prefix)))
        .map(|((key, payload), weight)| (key.clone(), payload.clone(), *weight))
        .collect()
}

fn read_cursor(cursor: &mut impl Cursor<DynData, DynData, (), DynZWeight>) -> Vec<Row> {
    let mut rows = Vec::new();
    while cursor.key_valid() {
        let key = cursor.key().downcast_checked::<Vec<u8>>().clone();
        while cursor.val_valid() {
            rows.push((
                key.clone(),
                cursor.val().downcast_checked::<StoredPayload>().clone(),
                *cursor.weight().downcast_checked::<ZWeight>(),
            ));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

fn read_rows(
    batch: &impl BatchReader<Key = DynData, Val = DynData, Time = (), R = DynZWeight>,
) -> Vec<Row> {
    read_cursor(&mut batch.cursor())
}

/// Reads the rows whose keys start with `prefix` through a cursor seek.
fn read_prefix(
    batch: &impl BatchReader<Key = DynData, Val = DynData, Time = (), R = DynZWeight>,
    prefix: &[u8],
) -> Vec<Row> {
    let mut cursor = batch.cursor();
    cursor.seek_key(prefix.to_vec().erase());
    read_cursor(&mut cursor)
        .into_iter()
        .take_while(|row| row.0.starts_with(prefix))
        .collect()
}

/// Reads part `p`'s group through `merge.seek_group`, with weights summed
/// across runs; cancelled values are left out.
fn read_group(merge: &mut RunMerge, p: i64) -> Vec<Row> {
    let group = prefix_partkey(p);
    merge.seek_group(&group);
    let mut rows = Vec::new();
    while let Some(key) = merge.key().map(<[u8]>::to_vec) {
        if !key.starts_with(&group) {
            break;
        }
        merge
            .take(|value, weight| {
                rows.push((
                    key.clone(),
                    value.downcast_checked::<StoredPayload>().clone(),
                    weight,
                ));
                Ok::<_, ()>(())
            })
            .unwrap();
        merge.step();
    }
    rows
}

/// The partkey of a stored row.
fn partkey_of(row: &Row) -> i64 {
    match PartPartsuppLineitemDefinition::unfold(&row.0).unwrap() {
        Key::PartByPartkey(key) => key.partkey,
        Key::PartsuppByPartkeySuppkey(key) => key.partkey,
        Key::LineitemByPartkeySuppkeyOrderkeyLinenumber(key) => key.partkey,
    }
}

fn file_run(factories: &IndexFactories, rows: Vec<SourceRow>) -> MergedIndexBatch {
    let run = build_batch_in(factories, rows).unwrap();
    assert_eq!(run.location(), BatchLocation::Storage);
    run
}

/// The transaction delta of the compaction test:
///
/// - part 10 is deleted, all seven rows;
/// - part 20's lines change payload (a delete and an insert each);
/// - the first line of part 30's first supply moves to part 40 (its
///   `l_partkey` changes), so it leaves one group and enters another;
/// - part `PARTS`, beyond every run, is inserted.
fn updates() -> Vec<SourceRow> {
    let mut rows: Vec<SourceRow> = deleted(part_group(10)).collect();
    for (key, payload, weight) in part_group(20) {
        if let Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(mut changed) = payload.clone() {
            changed.l_discount = SqlDecimal::<4, 2>::new(99, 2).unwrap();
            rows.push((key.clone(), payload, -weight));
            rows.push((
                key,
                Payload::LineitemByPartkeySuppkeyOrderkeyLinenumber(changed),
                weight,
            ));
        }
    }
    let (suppkey, orderkey) = (SUPPLIERS[0], order_of(30, 0));
    rows.push((
        line_key(30, suppkey, orderkey, 1),
        line_payload(orderkey, 1),
        -1,
    ));
    rows.push((
        line_key(40, suppkey, orderkey, 1),
        line_payload(orderkey, 1),
        1,
    ));
    rows.extend(part_group(PARTS));
    rows
}

/// Runs built with the index's factories go to files, compact into a file
/// that keeps the part-group prefix filter, and reopen from their path.
/// Snapshots taken before compaction and before the updates keep their
/// rows. After the updates, which move a line between parts, a full read,
/// every prefix read, and every group read agree with the oracle.
#[test]
fn file_runs_compact_with_their_prefix_filter_and_preserve_snapshots() {
    let storage_dir = tempfile::tempdir().unwrap();
    run_in_runtime(storage_config(&storage_dir, Some(0)), || {
        let factories = index_factories::<StoredPayload>(&TRIE);
        let mut trace = Spine::<MergedIndexBatch>::new(
            &factories,
            Arc::new("merged_index_p_storage_test".to_owned()),
            TraceRole::Integral,
        );

        let mut first_snapshot = None;
        let mut first_rows = Vec::new();
        for run in 0..RUNS {
            let rows: Vec<_> = parts_of_run(run).flat_map(part_group).collect();
            let batch = file_run(&factories, rows.clone());
            assert_eq!(batch.len(), ROWS_PER_PART * PARTS_PER_RUN as usize);
            TOKIO.block_on(trace.insert(batch));
            if run == 0 {
                first_rows = expected_rows(&oracle_of(&rows), None);
                first_snapshot = Some(trace.ro_snapshot());
            }
        }
        let first_snapshot = first_snapshot.unwrap();

        // Wait for a compacted run: one that holds more than an inserted run.
        let inserted_len = ROWS_PER_PART * PARTS_PER_RUN as usize;
        let deadline = Instant::now() + COMPACTION_DEADLINE;
        let compacted = loop {
            let snapshot = trace.ro_snapshot();
            if let Some(batch) = snapshot
                .batches()
                .iter()
                .find(|batch| batch.len() > inserted_len)
            {
                break Arc::clone(batch);
            }
            assert!(
                Instant::now() < deadline,
                "no compacted run after {COMPACTION_DEADLINE:?}: {:?}",
                snapshot
                    .batches()
                    .iter()
                    .map(|batch| (batch.location(), batch.len()))
                    .collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(compacted.location(), BatchLocation::Storage);
        assert_eq!(read_rows(&first_snapshot), first_rows);

        // The compacted file reopens with the same rows and the same filter.
        let path = compacted.file_reader().unwrap().path().clone();
        let reopened = MergedIndexBatch::from_path(&factories, &path).unwrap();
        assert_eq!(reopened.location(), BatchLocation::Storage);
        let compacted_rows = read_rows(compacted.as_ref());
        assert_eq!(read_rows(&reopened), compacted_rows);

        // The parts the compacted run lacks, the odd ones at least, lie
        // inside its key range: only the filter rejects them.
        let held: std::collections::BTreeSet<i64> = compacted_rows.iter().map(partkey_of).collect();
        assert!(held.len() <= PARTS as usize / 2);
        let spec = TRIE.root_prefix_spec();
        let mut rejected = 0;
        for p in 0..PARTS {
            for batch in [compacted.as_ref(), &reopened] {
                let maybe = batch.maybe_contains_prefix(prefix_partkey(p).erase(), &spec);
                assert!(maybe || !held.contains(&p), "false negative for part {p}");
                rejected += usize::from(!maybe);
            }
        }
        let absent = 2 * (PARTS as usize - held.len());
        assert!(
            rejected > absent * 9 / 10,
            "rejected {rejected} of {absent} absent probes"
        );
        drop(reopened);

        let all_parts: Vec<_> = (0..RUNS)
            .flat_map(parts_of_run)
            .flat_map(part_group)
            .collect();
        let mut oracle = oracle_of(&all_parts);
        let expected_before = expected_rows(&oracle, None);
        let before_updates = trace.ro_snapshot();
        assert_eq!(read_rows(&before_updates), expected_before);

        let updates = updates();
        TOKIO.block_on(trace.insert(file_run(&factories, updates.clone())));
        apply(&mut oracle, &updates);
        let after = trace.ro_snapshot();
        let expected = expected_rows(&oracle, None);
        // Part 10's rows leave and part PARTS's arrive; the rest replace rows.
        assert_eq!(expected.len(), expected_before.len());
        assert_eq!(read_rows(&after), expected);
        assert_eq!(read_rows(&before_updates), expected_before);
        assert_eq!(read_rows(&first_snapshot), first_rows);

        // Prefix reads: whole parts, and the supply that gained a line.
        for (prefix, count) in [
            (prefix_partkey(10), 0),
            (prefix_partkey(20), 7),
            (prefix_partkey(30), 6),
            (prefix_partkey(40), 8),
            (prefix_partkey(PARTS), 7),
            (prefix_partkey_suppkey(30, SUPPLIERS[0]), 2),
            (prefix_partkey_suppkey(40, SUPPLIERS[0]), 4),
        ] {
            let wanted = expected_rows(&oracle, Some(&prefix));
            assert_eq!(wanted.len(), count);
            assert_eq!(read_prefix(&after, &prefix), wanted);
        }

        // Group reads, as a scan session makes them: every part in order,
        // with the runs that cannot hold the part parked.
        let mut merge = RunMerge::with_group_prefix(&after, Some(spec));
        for p in 0..=PARTS {
            let prefix = prefix_partkey(p);
            assert_eq!(
                read_group(&mut merge, p),
                expected_rows(&oracle, Some(&prefix)),
                "part {p}"
            );
        }
    });
}

/// `seek_group` parks a run whose prefix filter rejects the part, and counts
/// the probe; the line that moved between parts cancels in its old group
/// and appears in its new one.
#[test]
fn seek_group_parks_runs_without_the_part_and_moves_a_line() {
    let storage_dir = tempfile::tempdir().unwrap();
    run_in_runtime(storage_config(&storage_dir, Some(0)), || {
        let factories = index_factories::<StoredPayload>(&TRIE);
        // Parts 1 and 3; parts 0 and 4, whose key range covers 1 and 3; and
        // a delta that moves part 1's first line to part 3.
        let (suppkey, orderkey) = (SUPPLIERS[0], order_of(1, 0));
        let moved = vec![
            (
                line_key(1, suppkey, orderkey, 1),
                line_payload(orderkey, 1),
                -1,
            ),
            (
                line_key(3, suppkey, orderkey, 1),
                line_payload(orderkey, 1),
                1,
            ),
        ];
        let runs = [
            [part_group(1), part_group(3)].concat(),
            [part_group(0), part_group(4)].concat(),
            moved,
        ];
        let oracle = oracle_of(runs.iter().flatten());
        let snapshot = SpineSnapshot::with_batches(
            &factories,
            runs.iter()
                .map(|rows| Arc::new(file_run(&factories, rows.clone())))
                .collect(),
        );

        for (part, rows, cancelled) in [(3, 8, 0), (1, 6, 1)] {
            let mut merge = RunMerge::with_group_prefix(&snapshot, Some(TRIE.root_prefix_spec()));
            let before = RunReads::current();
            let group = read_group(&mut merge, part);
            let reads = RunReads::current().since(before);
            assert_eq!(merge.parked_count(), 1, "part {part}");
            assert_eq!(reads.filtered_prefix_probes, 1, "part {part}");
            assert_eq!(merge.cancelled(), cancelled, "part {part}");
            assert_eq!(group.len(), rows, "part {part}");
            assert_eq!(group, expected_rows(&oracle, Some(&prefix_partkey(part))));
        }
    });
}

/// `build_batch_in` leaves the placement to the runtime, as `build_batch`
/// does (user decision of 2026-11-10): a step threshold of zero puts a
/// nonempty batch in a file, and an empty batch holds nothing to spill.
#[test]
fn build_batch_in_follows_the_runtimes_step_placement_in_files() {
    let storage_dir = tempfile::tempdir().unwrap();
    run_in_runtime(storage_config(&storage_dir, Some(0)), || {
        let factories = index_factories::<StoredPayload>(&TRIE);
        let empty = build_batch_in(&factories, Vec::new()).unwrap();
        assert_eq!(empty.location(), BatchLocation::Memory);
        assert_eq!(read_rows(&empty), Vec::new());

        let rows = part_group(1);
        let batch = build_batch_in(&factories, rows.clone()).unwrap();
        assert_eq!(batch.location(), BatchLocation::Storage);
        assert_eq!(read_rows(&batch), expected_rows(&oracle_of(&rows), None));
    });
}

/// The control of the test above: with no step threshold the batch is built
/// in memory.
#[test]
fn build_batch_in_follows_the_runtimes_step_placement_in_memory() {
    let storage_dir = tempfile::tempdir().unwrap();
    run_in_runtime(storage_config(&storage_dir, None), || {
        assert_eq!(Runtime::min_step_storage_bytes(), Some(usize::MAX));
        let factories = index_factories::<StoredPayload>(&TRIE);
        let rows = part_group(1);
        let batch = build_batch_in(&factories, rows.clone()).unwrap();
        assert_eq!(batch.location(), BatchLocation::Memory);
        assert_eq!(read_rows(&batch), expected_rows(&oracle_of(&rows), None));
    });
}
