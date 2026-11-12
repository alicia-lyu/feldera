use std::collections::BTreeMap;

use proptest::{collection::vec, prelude::*};

use super::*;
use crate::{
    dynamic::DowncastTrait,
    trace::{
        BatchReader as DynamicBatchReader, Cursor,
        merged_index::{
            ColumnRef, Domain, FoldedKey, KeyDecoder, KeyEncoder, KeyTrie, MergedIndexBatch, Node,
            Pass, Payload, Placement, SessionCounts,
            run_merge::tests::{memory_run, snapshot},
        },
    },
};

// ---- Fixture: two one-byte levels, `A` at `a` (id 1) and `B` at `a.b` (id 2) ----

const A: u8 = 1;
const B: u8 = 2;

const TRIE: KeyTrie = KeyTrie {
    name: "Pairs",
    domains: &[
        Domain {
            name: "a",
            columns: &[
                ColumnRef {
                    relation: "A",
                    column: "a",
                },
                ColumnRef {
                    relation: "B",
                    column: "a",
                },
            ],
            width: 1,
        },
        Domain {
            name: "b",
            columns: &[ColumnRef {
                relation: "B",
                column: "b",
            }],
            width: 1,
        },
    ],
    children: &[Node {
        domain: "a",
        placements: &[Placement {
            relation: "A",
            via: &[],
            payload: Payload::All,
        }],
        children: &[Node {
            domain: "b",
            placements: &[Placement {
                relation: "B",
                via: &[],
                payload: Payload::All,
            }],
            children: &[],
        }],
    }],
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Key {
    A(u8),
    B(u8, u8),
}

/// An index of `A` and `B` whose payload is one stored byte.
struct Pairs;

impl MergedIndexDefinition for Pairs {
    type Key = Key;
    type Payload = u8;

    const TRIE: KeyTrie = TRIE;

    fn fold(key: &Key) -> FoldedKey {
        match key {
            Key::A(a) => KeyEncoder::new().field(1, a).finish(A),
            Key::B(a, b) => KeyEncoder::new().field(1, a).field(2, b).finish(B),
        }
    }

    fn unfold(bytes: &[u8]) -> Result<Key, String> {
        let (id, mut fields) = KeyDecoder::new(bytes)?;
        let a = fields.field(1)?;
        let key = match id {
            A => Key::A(a),
            B => Key::B(a, fields.field(2)?),
            _ => return Err(format!("unknown stream {id}")),
        };
        fields.finish()?;
        Ok(key)
    }

    fn decode_payload(_stream_id: u8, bytes: &[u8]) -> Result<u8, String> {
        bytes
            .first()
            .copied()
            .ok_or_else(|| "empty payload".to_string())
    }
}

// ---- Three readers: `A` by `a`, `B` by `a` (a range), and `B` by `(a, b)` (a path) ----

fn a_by_a(key: &Key, payload: &u8) -> Result<(u8, u8), String> {
    match key {
        Key::A(a) => Ok((*a, *payload)),
        other => Err(format!("expected an A key, found {other:?}")),
    }
}

fn b_by_a(key: &Key, payload: &u8) -> Result<(u8, Tup2<u8, u8>), String> {
    match key {
        Key::B(a, b) => Ok((*a, Tup2(*b, *payload))),
        other => Err(format!("expected a B key, found {other:?}")),
    }
}

fn b_by_ab(key: &Key, payload: &u8) -> Result<(Tup2<u8, u8>, u8), String> {
    match key {
        Key::B(a, b) => Ok((Tup2(*a, *b), *payload)),
        other => Err(format!("expected a B key, found {other:?}")),
    }
}

type Row = (Vec<u8>, Vec<u8>, ZWeight);

fn row(key: Key, payload: u8, weight: ZWeight) -> Row {
    (Pairs::fold(&key).as_bytes().to_vec(), vec![payload], weight)
}

/// What the three readers hold after one session.
struct Collected {
    a_by_a: ReaderRuns<u8, u8>,
    b_by_a: ReaderRuns<u8, Tup2<u8, u8>>,
    b_by_ab: ReaderRuns<Tup2<u8, u8>, u8>,
    touched: TouchedGroups,
    counts: SessionCounts,
}

fn collect_all(old: &[Vec<Row>], dlt: &[Row]) -> Result<Collected, String> {
    let old = snapshot::<Vec<u8>>(
        &old.iter()
            .map(|rows| (rows.clone(), false))
            .collect::<Vec<_>>(),
    );
    let dlt: MergedIndexBatch = memory_run(dlt);
    let mut session = ScanSession::<Pairs>::new(&old, &dlt, Pass::Seek);
    let mut a_reader = Reader::<Pairs, _, _>::new(A, a_by_a);
    let mut b_range = Reader::<Pairs, _, _>::new(B, b_by_a);
    let mut b_path = Reader::<Pairs, _, _>::new(B, b_by_ab);
    let touched = collect(
        &mut session,
        &mut [&mut a_reader, &mut b_range, &mut b_path],
    )?;
    Ok(Collected {
        a_by_a: a_reader.seal(),
        b_by_a: b_range.seal(),
        b_by_ab: b_path.seal(),
        touched,
        counts: session.counts(),
    })
}

/// The rows of a batch or snapshot, with their weights, as its cursor yields
/// them: a snapshot of several batches yields each batch's row apart.
fn rows<K: DBData, V: DBData>(batch: &impl DynamicBatchReader<Time = ()>) -> Vec<(K, V, ZWeight)> {
    let mut cursor = batch.cursor();
    let mut rows = Vec::new();
    while cursor.key_valid() {
        while cursor.val_valid() {
            rows.push((
                cursor.key().downcast_checked::<K>().clone(),
                cursor.val().downcast_checked::<V>().clone(),
                *cursor.weight().downcast_checked::<ZWeight>(),
            ));
            cursor.step_val();
        }
        cursor.step_key();
    }
    rows
}

/// The batches of an after read, apart.
fn halves<K: DBData, V: DBData>(
    runs: &ReaderRuns<K, V>,
) -> (Vec<(K, V, ZWeight)>, Vec<(K, V, ZWeight)>) {
    (rows(runs.before().inner()), rows(runs.delta().inner()))
}

fn group_of(a: u8) -> Vec<u8> {
    KeyEncoder::new().field(1, &a).prefix()
}

// ---- Tests ----

#[test]
fn readers_take_the_rows_of_their_stream_in_the_touched_groups() {
    let old = vec![vec![
        row(Key::A(1), 10, 1),
        row(Key::B(1, 1), 20, 1),
        row(Key::B(1, 2), 21, 1),
        row(Key::A(2), 11, 1),
        row(Key::B(2, 1), 23, 1),
    ]];
    // Replace `A(1)`'s payload and add `B(1, 3)`; group 2 is untouched.
    let dlt = vec![
        row(Key::A(1), 10, -1),
        row(Key::A(1), 12, 1),
        row(Key::B(1, 3), 22, 1),
    ];
    let collected = collect_all(&old, &dlt).unwrap();

    assert_eq!(
        halves(&collected.a_by_a),
        (vec![(1, 10, 1)], vec![(1, 10, -1), (1, 12, 1)])
    );
    assert_eq!(
        halves(&collected.b_by_a),
        (
            vec![(1, Tup2(1, 20), 1), (1, Tup2(2, 21), 1)],
            vec![(1, Tup2(3, 22), 1)]
        )
    );
    assert_eq!(
        halves(&collected.b_by_ab),
        (
            vec![(Tup2(1, 1), 20, 1), (Tup2(1, 2), 21, 1)],
            vec![(Tup2(1, 3), 22, 1)]
        )
    );
    assert!(collected.touched.contains(&group_of(1)));
    assert!(!collected.touched.contains(&group_of(2)));
    assert_eq!(collected.touched.len(), 1);
}

#[test]
fn the_after_read_keeps_old_and_dlt_apart() {
    let old = vec![vec![row(Key::A(1), 10, 1)]];
    let dlt = vec![row(Key::A(1), 10, -1), row(Key::A(1), 12, 1)];
    let collected = collect_all(&old, &dlt).unwrap();

    // The after read is two batches; `Old`'s row of `(1, 10)` and `Dlt`'s
    // deletion of it stay in different batches, so only a reader sums them.
    let after = collected.a_by_a.after();
    let batches = after.inner().batches();
    assert_eq!(batches.len(), 2);
    assert_eq!(rows::<u8, u8>(&*batches[0]), vec![(1, 10, 1)]);
    assert_eq!(rows::<u8, u8>(&*batches[1]), vec![(1, 10, -1), (1, 12, 1)]);
    assert_eq!(
        summed(rows::<u8, u8>(after.inner())),
        BTreeMap::from([((1, 12), 1)])
    );
    assert_eq!(collected.a_by_a.before().inner().batches().len(), 1);
}

#[test]
fn taking_the_delta_moves_it_and_leaves_the_before_read() {
    let old = vec![vec![row(Key::B(1, 1), 20, 1)]];
    let dlt = vec![row(Key::B(1, 2), 21, 1)];
    let mut collected = collect_all(&old, &dlt).unwrap();

    let delta = collected.b_by_a.take_delta();
    assert_eq!(
        rows::<u8, Tup2<u8, u8>>(delta.inner()),
        vec![(1, Tup2(2, 21), 1)]
    );
    assert_eq!(
        rows::<u8, Tup2<u8, u8>>(collected.b_by_a.before().inner()),
        vec![(1, Tup2(1, 20), 1)]
    );
    // The reader's `Dlt` batch is now empty, so its after read is its before read.
    assert!(rows::<u8, Tup2<u8, u8>>(collected.b_by_a.delta().inner()).is_empty());
    assert_eq!(
        summed(rows::<u8, Tup2<u8, u8>>(collected.b_by_a.after().inner())),
        BTreeMap::from([((1, Tup2(1, 20)), 1)])
    );

    // An after snapshot that still shares the batch makes the take a copy.
    let after = collected.b_by_ab.after();
    let shared = collected.b_by_ab.take_delta();
    assert_eq!(
        rows::<Tup2<u8, u8>, u8>(shared.inner()),
        vec![(Tup2(1, 2), 21, 1)]
    );
    assert_eq!(
        summed(rows::<Tup2<u8, u8>, u8>(after.inner())),
        BTreeMap::from([((Tup2(1, 1), 20), 1), ((Tup2(1, 2), 21), 1)])
    );
}

#[test]
fn a_second_reader_of_a_stream_costs_no_payload_decode() {
    let old = vec![vec![
        row(Key::A(1), 10, 1),
        row(Key::B(1, 1), 20, 1),
        row(Key::B(1, 2), 21, 1),
    ]];
    let dlt = vec![row(Key::B(1, 3), 22, 1)];
    let collected = collect_all(&old, &dlt).unwrap();

    // Two readers take stream `B`, but each entry is decoded once.
    assert_eq!(collected.counts.payload_decodes_old, 3);
    assert_eq!(collected.counts.payload_decodes_dlt, 1);
}

#[test]
fn a_projection_error_stops_the_collection() {
    let old = snapshot::<Vec<u8>>(&[(vec![row(Key::A(1), 10, 1)], false)]);
    let dlt: MergedIndexBatch = memory_run(&[row(Key::A(1), 12, 1)]);
    let mut session = ScanSession::<Pairs>::new(&old, &dlt, Pass::Seek);
    // A reader declared for stream `A` with a projection of `B` keys.
    let mut wrong = Reader::<Pairs, _, _>::new(A, b_by_ab);
    let error = collect(&mut session, &mut [&mut wrong]).unwrap_err();
    assert!(error.contains("expected a B key"), "{error}");
}

// ---- Against a brute force ----

/// The summed weight of each `(key, payload)` over `runs`, zero sums dropped.
fn consolidate<'a>(runs: impl IntoIterator<Item = &'a Vec<Row>>) -> BTreeMap<(Key, u8), ZWeight> {
    let mut sums = BTreeMap::<(Vec<u8>, u8), ZWeight>::new();
    for (key, payload, weight) in runs.into_iter().flatten() {
        *sums.entry((key.clone(), payload[0])).or_default() += weight;
    }
    sums.into_iter()
        .filter(|(_, weight)| *weight != 0)
        .map(|((key, payload), weight)| ((Pairs::unfold(&key).unwrap(), payload), weight))
        .collect()
}

/// `reader`'s expected rows: the consolidated entries of `stream` in the
/// touched groups, projected.
fn expected<K: DBData, V: DBData>(
    entries: &BTreeMap<(Key, u8), ZWeight>,
    touched: &BTreeSet<u8>,
    project: Projection<Pairs, K, V>,
) -> BTreeMap<(K, V), ZWeight> {
    let mut rows = BTreeMap::new();
    for ((key, payload), weight) in entries {
        let a = match key {
            Key::A(a) | Key::B(a, _) => *a,
        };
        if let (true, Ok((k, v))) = (touched.contains(&a), project(key, payload)) {
            *rows.entry((k, v)).or_default() += weight;
        }
    }
    rows.retain(|_, weight| *weight != 0);
    rows
}

fn summed<K: DBData, V: DBData>(rows: Vec<(K, V, ZWeight)>) -> BTreeMap<(K, V), ZWeight> {
    let mut sums = BTreeMap::new();
    for (k, v, weight) in rows {
        *sums.entry((k, v)).or_default() += weight;
    }
    sums.retain(|_, weight| *weight != 0);
    sums
}

fn raw_key() -> impl Strategy<Value = Key> {
    prop_oneof![
        (0u8..4).prop_map(Key::A),
        (0u8..4, 0u8..3).prop_map(|(a, b)| Key::B(a, b)),
    ]
}

fn rows_strategy() -> impl Strategy<Value = Vec<Row>> {
    vec((raw_key(), 0u8..3, -2i64..=2), 0..12).prop_map(|rows| {
        rows.into_iter()
            .filter(|(_, _, weight)| *weight != 0)
            .map(|(key, payload, weight)| row(key, payload, weight))
            .collect()
    })
}

proptest! {
    #[test]
    fn readers_match_a_brute_force(old in vec(rows_strategy(), 1..3), dlt in rows_strategy()) {
        let collected = collect_all(&old, &dlt).unwrap();
        let old_entries = consolidate(&old);
        let dlt_entries = consolidate([&dlt]);
        let touched: BTreeSet<u8> = dlt_entries
            .keys()
            .map(|(key, _)| match key { Key::A(a) | Key::B(a, _) => *a })
            .collect();
        prop_assert_eq!(collected.touched.len(), touched.len());
        for a in &touched {
            prop_assert!(collected.touched.contains(&group_of(*a)));
        }

        macro_rules! check {
            ($runs:expr, $project:expr) => {
                prop_assert_eq!(
                    summed(rows($runs.before().inner())),
                    expected(&old_entries, &touched, $project)
                );
                prop_assert_eq!(
                    summed(rows($runs.delta().inner())),
                    expected(&dlt_entries, &touched, $project)
                );
                let mut after = expected(&old_entries, &touched, $project);
                for (row, weight) in expected(&dlt_entries, &touched, $project) {
                    *after.entry(row).or_default() += weight;
                }
                after.retain(|_, weight| *weight != 0);
                prop_assert_eq!(summed(rows($runs.after().inner())), after);
            };
        }
        check!(collected.a_by_a, a_by_a);
        check!(collected.b_by_a, b_by_a);
        check!(collected.b_by_ab, b_by_ab);
    }
}
