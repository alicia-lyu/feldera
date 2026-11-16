use std::collections::{BTreeMap, BTreeSet};

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
    utils::Tup2,
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

// ---- Four readers, one of each static plan the Q3 circuit uses ----

/// Like `O⁻`: the `B` rows of the groups that `A` entries of `Dlt` touch, `(a, (b, p))`.
struct BByA;

impl ReaderDefinition for BByA {
    type Index = Pairs;
    type Key = u8;
    type Val = Tup2<u8, u8>;
    type DeltaKey = u8;
    type DeltaVal = u8;
    const PARTNER_STREAM: Option<u8> = Some(B);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(A);

    fn unfold(key: &Key, payload: &u8) -> Result<(u8, Tup2<u8, u8>), String> {
        match key {
            Key::B(a, b) => Ok((*a, Tup2(*b, *payload))),
            other => Err(format!("expected a B key, found {other:?}")),
        }
    }

    fn unfold_delta(key: &Key, payload: &u8) -> Result<(u8, u8), String> {
        match key {
            Key::A(a) => Ok((*a, *payload)),
            other => Err(format!("expected an A key, found {other:?}")),
        }
    }
}

/// Like `C⁺`: the `A` rows of the groups that `B` entries of `Dlt` touch, `(a, p)`.
struct AFromB;

impl ReaderDefinition for AFromB {
    type Index = Pairs;
    type Key = u8;
    type Val = u8;
    type DeltaKey = u8;
    type DeltaVal = Tup2<u8, u8>;
    const PARTNER_STREAM: Option<u8> = Some(A);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(B);

    fn unfold(key: &Key, payload: &u8) -> Result<(u8, u8), String> {
        match key {
            Key::A(a) => Ok((*a, *payload)),
            other => Err(format!("expected an A key, found {other:?}")),
        }
    }

    fn unfold_delta(key: &Key, payload: &u8) -> Result<(u8, Tup2<u8, u8>), String> {
        match key {
            Key::B(a, b) => Ok((*a, Tup2(*b, *payload))),
            other => Err(format!("expected a B key, found {other:?}")),
        }
    }
}

/// Like `L⁻`: the `B` rows at the `(a, b)` nodes that a join requested.
struct BAtPath;

impl ReaderDefinition for BAtPath {
    type Index = Pairs;
    type Key = Tup2<u8, u8>;
    type Val = u8;
    type DeltaKey = Tup2<u8, u8>;
    type DeltaVal = u8;
    const PARTNER_STREAM: Option<u8> = Some(B);
    const REQUEST_DEPTH: usize = 2;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Join;

    fn unfold(key: &Key, payload: &u8) -> Result<(Tup2<u8, u8>, u8), String> {
        match key {
            Key::B(a, b) => Ok((Tup2(*a, *b), *payload)),
            other => Err(format!("expected a B key, found {other:?}")),
        }
    }
}

/// Like `J⁺`: no stored partner, driven by the `B` entries of `Dlt` at the `(a, b)` node.
struct DerivedAtPath;

impl ReaderDefinition for DerivedAtPath {
    type Index = Pairs;
    type Key = Tup2<u8, u8>;
    type Val = u8;
    type DeltaKey = Tup2<u8, u8>;
    type DeltaVal = u8;
    const PARTNER_STREAM: Option<u8> = None;
    const REQUEST_DEPTH: usize = 2;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(B);

    fn unfold_delta(key: &Key, payload: &u8) -> Result<(Tup2<u8, u8>, u8), String> {
        match key {
            Key::B(a, b) => Ok((Tup2(*a, *b), *payload)),
            other => Err(format!("expected a B key, found {other:?}")),
        }
    }
}

/// Reverses the order of the keys: its driving delta must be refused.
struct Reversing;

impl ReaderDefinition for Reversing {
    type Index = Pairs;
    type Key = u8;
    type Val = u8;
    type DeltaKey = u8;
    type DeltaVal = u8;
    const PARTNER_STREAM: Option<u8> = Some(A);
    const REQUEST_DEPTH: usize = 1;
    const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(A);

    fn unfold(key: &Key, payload: &u8) -> Result<(u8, u8), String> {
        match key {
            Key::A(a) => Ok((u8::MAX - *a, *payload)),
            other => Err(format!("expected an A key, found {other:?}")),
        }
    }

    fn unfold_delta(key: &Key, payload: &u8) -> Result<(u8, u8), String> {
        Self::unfold(key, payload)
    }
}

type Row = (Vec<u8>, Vec<u8>, ZWeight);

fn row(key: Key, payload: u8, weight: ZWeight) -> Row {
    (Pairs::fold(&key).as_bytes().to_vec(), vec![payload], weight)
}

fn group_of(a: u8) -> Vec<u8> {
    KeyEncoder::new().field(1, &a).prefix()
}

fn path_of(a: u8, b: u8) -> Vec<u8> {
    KeyEncoder::new().field(1, &a).field(2, &b).prefix()
}

/// What the readers hold after one transaction.
struct Collected {
    b_by_a: Reader<BByA>,
    a_from_b: Reader<AFromB>,
    b_at_path: Reader<BAtPath>,
    derived: Reader<DerivedAtPath>,
    /// The driving deltas, which the readers do not own.
    delta_b_by_a: Delta<BByA>,
    delta_a_from_b: Delta<AFromB>,
    delta_derived: Delta<DerivedAtPath>,
    captured: Captured,
    counts: SessionCounts,
}

/// Captures `dlt`, scans the touched groups of `old` once, and routes each
/// position to the three stored readers. `b_at_path` has no driving stream, so
/// the test asks for the `(a, b)` nodes that `requested_paths` lists.
fn collect_all(
    old: &[Vec<Row>],
    dlt: &[Row],
    requested_paths: &[(u8, u8)],
    pass: Pass,
) -> Result<Collected, String> {
    let old = snapshot::<Vec<u8>>(
        &old.iter()
            .map(|rows| (rows.clone(), false))
            .collect::<Vec<_>>(),
    );
    let dlt: MergedIndexBatch = memory_run(dlt);
    let mut b_by_a = Reader::<BByA>::new()?;
    let mut a_from_b = Reader::<AFromB>::new()?;
    let mut b_at_path = Reader::<BAtPath>::new()?;
    let mut derived = Reader::<DerivedAtPath>::new()?;
    for &(a, b) in requested_paths {
        b_at_path.request(&path_of(a, b))?;
    }

    let mut delta_b_by_a = Delta::<BByA>::new()?;
    let mut delta_a_from_b = Delta::<AFromB>::new()?;
    let mut delta_derived = Delta::<DerivedAtPath>::new()?;
    // `b_at_path` is driven by a join: requests only, and no delta to capture.
    let captured = capture(
        &dlt,
        &mut [
            &mut Capture::new(&mut b_by_a, &mut delta_b_by_a),
            &mut Capture::new(&mut a_from_b, &mut delta_a_from_b),
            &mut Capture::new(&mut derived, &mut delta_derived),
        ],
    )?;
    let mut session = ScanSession::<Pairs>::new(&old, &captured.touched, pass);
    while session.next_group().is_some() {
        while session.key().is_some() {
            {
                let mut position = Position::at(&mut session);
                route(
                    &mut position,
                    &mut [&mut b_by_a, &mut a_from_b, &mut b_at_path],
                )?;
            }
            session.step();
        }
    }
    let mut counts = session.counts();
    drop(session);
    counts.payload_decodes_dlt = captured.payload_decodes_dlt;
    b_by_a.seal_partner()?;
    a_from_b.seal_partner()?;
    b_at_path.seal_partner()?;
    derived.seal_partner()?;
    Ok(Collected {
        b_by_a,
        a_from_b,
        b_at_path,
        derived,
        delta_b_by_a,
        delta_a_from_b,
        delta_derived,
        captured,
        counts,
    })
}

/// The rows of a batch or snapshot, with their weights, as its cursor yields them.
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

fn partner_rows<R: ReaderDefinition>(reader: &Reader<R>) -> Vec<(R::Key, R::Val, ZWeight)> {
    rows(reader.partner().unwrap().inner())
}

fn requests_of<R: ReaderDefinition>(reader: &Reader<R>) -> Vec<Vec<u8>> {
    reader.requests().iter().map(<[u8]>::to_vec).collect()
}

// ---- Requests ----

#[test]
fn requests_move_forward_and_refuse_to_go_back() {
    let mut requests = Requests::default();
    for prefix in [[1, 2], [1, 2], [1, 5], [3, 0]] {
        requests.push(&prefix).unwrap();
    }
    // A repeat of the last request is ignored; going back is an error.
    assert_eq!(requests.len(), 3);
    assert!(requests.push(&[2, 0]).unwrap_err().contains("goes back"));

    // Group-level questions and path-level questions interleave in key order.
    assert!(!requests.has(&[0, 9]));
    assert!(requests.has_below(&[1]));
    assert!(requests.has(&[1, 2]));
    assert!(!requests.has(&[1, 3]));
    assert!(requests.has(&[1, 5]));
    assert!(!requests.has_below(&[2]));
    assert!(requests.has_below(&[3]));
    assert!(!requests.has_below(&[4]));
    assert!(requests.contains(&[1, 5]));
    assert!(!requests.contains(&[2, 0]));
    assert!(Requests::default().is_empty());
}

// ---- RowBuilder ----

/// One entry of `rows`, then its end.
fn entry(builder: &mut RowBuilder<u8, i64>, rows: &[(u8, i64, ZWeight)]) -> Result<(), String> {
    for &(key, value, weight) in rows {
        builder.push(key, value, weight)?;
    }
    builder.end_entry()
}

#[test]
fn a_row_builder_reorders_the_rows_of_one_entry_into_typed_order() {
    // Packed integers are little-endian, so the byte order of `256` (`00 01`) is below that of
    // `1` (`01 00`): `Dlt`'s `-old` (256) arrives before its `+new` (1), against the typed order.
    let mut builder = RowBuilder::<u8, i64>::new();
    entry(&mut builder, &[(7, 256, -1), (7, 1, 1)]).unwrap();
    let batch = builder.finish().unwrap();
    assert_eq!(
        rows::<u8, i64>(batch.inner()),
        vec![(7, 1, 1), (7, 256, -1)]
    );
}

#[test]
fn an_old_row_replaced_by_a_dlt_pair_consolidates_to_the_new_row() {
    // `Old` has c5 (+1); `Dlt` has -c5 and +c5': three rows of one entry.
    let mut builder = RowBuilder::<u8, i64>::new();
    entry(&mut builder, &[(5, 10, 1), (5, 10, -1), (5, 11, 1)]).unwrap();
    // A later entry of the same key, and then another key, in ascending order.
    entry(&mut builder, &[(5, 12, 2)]).unwrap();
    entry(&mut builder, &[(6, 1, 1)]).unwrap();
    // An entry whose rows all cancel leaves nothing, not even its key.
    entry(&mut builder, &[(8, 1, 1), (8, 1, -1)]).unwrap();
    entry(&mut builder, &[(9, 1, 1)]).unwrap();
    let batch = builder.finish().unwrap();
    assert_eq!(
        rows::<u8, i64>(batch.inner()),
        vec![(5, 11, 1), (5, 12, 2), (6, 1, 1), (9, 1, 1)]
    );
}

#[test]
fn a_row_builder_refuses_rows_of_different_entries_out_of_order() {
    let mut builder = RowBuilder::<u8, i64>::new();
    entry(&mut builder, &[(5, 3, 1)]).unwrap();
    // A smaller key, a smaller value of the same key, and the same row again.
    for (key, value) in [(4, 1), (5, 2), (5, 3)] {
        let mut builder = RowBuilder::<u8, i64>::new();
        entry(&mut builder, &[(5, 3, 1)]).unwrap();
        let error = entry(&mut builder, &[(key, value, 1)]).unwrap_err();
        assert!(error.contains("ascending order across entries"), "{error}");
    }
    // No sort rescues it: the earlier entry's rows were already pushed.
    entry(&mut builder, &[(5, 4, 1)]).unwrap();
}

#[test]
fn a_row_builder_refuses_more_than_three_rows_in_one_entry() {
    let mut builder = RowBuilder::<u8, i64>::new();
    for value in 0..MAX_ROWS_PER_ENTRY as i64 {
        builder.push(1, value, 1).unwrap();
    }
    let error = builder.push(1, 99, 1).unwrap_err();
    assert!(error.contains("more than 3 rows"), "{error}");
}

#[test]
fn a_reader_checks_its_definition_against_the_trie() {
    // The widths follow from the trie: one tag and one value byte per field.
    assert_eq!(Reader::<BByA>::new().unwrap().request_width(), 2);
    assert_eq!(Reader::<AFromB>::new().unwrap().request_width(), 2);
    assert_eq!(Reader::<BAtPath>::new().unwrap().request_width(), 4);
    assert_eq!(Reader::<DerivedAtPath>::new().unwrap().request_width(), 4);

    /// A request node deeper than the stream's path.
    struct TooDeep;
    impl ReaderDefinition for TooDeep {
        type Index = Pairs;
        type Key = u8;
        type Val = u8;
        type DeltaKey = u8;
        type DeltaVal = u8;
        const PARTNER_STREAM: Option<u8> = Some(A);
        const REQUEST_DEPTH: usize = 2;
        const DRIVING_DELTA: DrivingDelta = DrivingDelta::Join;
    }
    assert!(
        Reader::<TooDeep>::new()
            .err()
            .unwrap()
            .contains("fewer than")
    );

    /// A stream the trie does not have.
    struct NoSuchStream;
    impl ReaderDefinition for NoSuchStream {
        type Index = Pairs;
        type Key = u8;
        type Val = u8;
        type DeltaKey = u8;
        type DeltaVal = u8;
        const PARTNER_STREAM: Option<u8> = Some(9);
        const REQUEST_DEPTH: usize = 1;
        const DRIVING_DELTA: DrivingDelta = DrivingDelta::Join;
    }
    assert!(
        Reader::<NoSuchStream>::new()
            .err()
            .unwrap()
            .contains("no stream 9")
    );
}

// ---- Driving deltas and requests ----

#[test]
fn stream_driven_readers_take_their_driving_deltas_and_requests_from_dlt() {
    let old = vec![vec![
        row(Key::A(1), 10, 1),
        row(Key::B(1, 1), 20, 1),
        row(Key::A(2), 11, 1),
        row(Key::B(2, 1), 23, 1),
    ]];
    // Replace `A(1)`'s payload and insert `B(3, 1)`; group 2 is untouched.
    let dlt = vec![
        row(Key::A(1), 10, -1),
        row(Key::A(1), 12, 1),
        row(Key::B(3, 1), 30, 1),
    ];
    let collected = collect_all(&old, &dlt, &[], Pass::Seek).unwrap();

    assert_eq!(collected.captured.touched, [group_of(1), group_of(3)]);
    // `BByA` is driven by the `A` entries: group 1. `AFromB` by the `B` entries: group 3.
    assert_eq!(requests_of(&collected.b_by_a), [group_of(1)]);
    assert_eq!(requests_of(&collected.a_from_b), [group_of(3)]);
    assert_eq!(requests_of(&collected.derived), [path_of(3, 1)]);
    assert!(requests_of(&collected.b_at_path).is_empty());

    // Each driving delta is the `Dlt` batch of its stream, unfolded.
    let delta_a = collected.delta_b_by_a.into_batch().unwrap();
    assert_eq!(
        rows::<u8, u8>(delta_a.inner()),
        vec![(1, 10, -1), (1, 12, 1)]
    );
    let delta_b = collected.delta_a_from_b.into_batch().unwrap();
    assert_eq!(
        rows::<u8, Tup2<u8, u8>>(delta_b.inner()),
        vec![(3, Tup2(1, 30), 1)]
    );
    let delta_path = collected.delta_derived.into_batch().unwrap();
    assert_eq!(
        rows::<Tup2<u8, u8>, u8>(delta_path.inner()),
        vec![(Tup2(3, 1), 30, 1)]
    );
    // A join's driving delta holds no batch.
    assert!(Delta::<BAtPath>::new().err().unwrap().contains("is a join"));

    // The partner batches hold only the requested slice. `BByA` requests
    // group 1: its `B` row. `AFromB` requests group 3: `Old` has no `A(3)`.
    assert_eq!(partner_rows(&collected.b_by_a), vec![(1, Tup2(1, 20), 1)]);
    assert!(partner_rows(&collected.a_from_b).is_empty());
    // The `B(2, 1)` of the untouched group 2 is in no reader.
}

#[test]
fn an_entry_outside_every_request_is_dropped_unread() {
    let old = vec![vec![
        row(Key::A(1), 10, 1),
        row(Key::B(1, 1), 20, 1),
        row(Key::B(1, 2), 21, 1),
    ]];
    // `Dlt` touches group 1 through an `A` entry only. `BAtPath` requests
    // `(1, 2)` alone.
    let dlt = vec![row(Key::A(1), 11, 1)];
    let collected = collect_all(&old, &dlt, &[(1, 2)], Pass::Seek).unwrap();

    // `A(1)` is requested by nobody (`AFromB` is driven by `B` entries), and
    // of the two `B` entries only the requested `(1, 2)` and group 1's `B`
    // rows for `BByA` are kept: `BByA` keeps both, `BAtPath` only `(1, 2)`.
    assert!(partner_rows(&collected.a_from_b).is_empty());
    assert_eq!(
        partner_rows(&collected.b_by_a),
        vec![(1, Tup2(1, 20), 1), (1, Tup2(2, 21), 1)]
    );
    assert_eq!(
        partner_rows(&collected.b_at_path),
        vec![(Tup2(1, 2), 21, 1)]
    );
    // Payloads read: `B(1,1)` and `B(1,2)` for `BByA`, with `B(1,2)` shared by
    // `BAtPath`: two decodes, not three. `A(1)` is read by nobody.
    assert_eq!(collected.counts.payload_decodes_old, 2);
    // `Dlt`'s one entry drives `BByA`: one decode.
    assert_eq!(collected.counts.payload_decodes_dlt, 1);
}

#[test]
fn no_driving_change_means_no_rows_and_no_payload_read() {
    let old = vec![vec![row(Key::A(1), 10, 1), row(Key::B(1, 1), 20, 1)]];
    for pass in [Pass::Seek, Pass::Step] {
        let collected = collect_all(&old, &[], &[], pass).unwrap();
        assert!(collected.captured.touched.is_empty());
        assert!(partner_rows(&collected.b_by_a).is_empty());
        assert!(partner_rows(&collected.a_from_b).is_empty());
        assert!(requests_of(&collected.b_by_a).is_empty());
        assert!(rows::<u8, u8>(collected.delta_b_by_a.into_batch().unwrap().inner()).is_empty());
        assert_eq!(collected.counts.groups, 0);
        assert_eq!(collected.counts.payload_decodes_old, 0);
        assert_eq!(collected.counts.payload_decodes_dlt, 0);
    }
}

#[test]
fn a_dlt_entry_nobody_drives_costs_no_decode() {
    // Only `BAtPath` has no driving stream; with `Dlt` entries of `A` and `B`,
    // each entry is decoded once if some reader drives it.
    let dlt = vec![row(Key::A(1), 10, 1), row(Key::B(1, 1), 20, 1)];
    let collected = collect_all(&[], &dlt, &[], Pass::Seek).unwrap();
    // `A(1)` drives `BByA`; `B(1,1)` drives `AFromB` and `DerivedAtPath`
    // (two readers, one decode).
    assert_eq!(collected.counts.payload_decodes_dlt, 2);
}

#[test]
fn a_delta_row_conversion_that_goes_back_is_an_error() {
    let dlt: MergedIndexBatch = memory_run(&[row(Key::A(1), 1, 1), row(Key::A(2), 1, 1)]);
    let mut reversing = Reader::<Reversing>::new().unwrap();
    let mut delta = Delta::<Reversing>::new().unwrap();
    let error = capture(&dlt, &mut [&mut Capture::new(&mut reversing, &mut delta)]).unwrap_err();
    assert!(error.contains("ascending order"), "{error}");
}

#[test]
fn a_conversion_error_stops_the_collection() {
    // A reader that converts the wrong stream's entries.
    struct Wrong;
    impl ReaderDefinition for Wrong {
        type Index = Pairs;
        type Key = Tup2<u8, u8>;
        type Val = u8;
        type DeltaKey = Tup2<u8, u8>;
        type DeltaVal = u8;
        const PARTNER_STREAM: Option<u8> = Some(A);
        const REQUEST_DEPTH: usize = 1;
        const DRIVING_DELTA: DrivingDelta = DrivingDelta::Stream(A);

        fn unfold(key: &Key, payload: &u8) -> Result<(Tup2<u8, u8>, u8), String> {
            BAtPath::unfold(key, payload)
        }

        fn unfold_delta(key: &Key, payload: &u8) -> Result<(Tup2<u8, u8>, u8), String> {
            BAtPath::unfold(key, payload)
        }
    }
    let dlt: MergedIndexBatch = memory_run(&[row(Key::A(1), 1, 1)]);
    let mut wrong = Reader::<Wrong>::new().unwrap();
    let mut delta = Delta::<Wrong>::new().unwrap();
    let error = capture(&dlt, &mut [&mut Capture::new(&mut wrong, &mut delta)]).unwrap_err();
    assert!(error.contains("expected a B key"), "{error}");
}

#[test]
fn a_derived_reader_takes_the_rows_the_routing_builds() {
    let mut derived = Reader::<DerivedAtPath>::new().unwrap();
    derived.push_row(Tup2(1, 2), 5, 3).unwrap();
    derived.push_row(Tup2(1, 2), 4, 1).unwrap();
    derived.end_entry().unwrap();
    derived.push_ordered_row(Tup2(2, 0), 1, 1).unwrap();
    assert!(derived.partner().is_err());
    derived.seal_partner().unwrap();
    assert_eq!(
        partner_rows(&derived),
        vec![(Tup2(1, 2), 4, 1), (Tup2(1, 2), 5, 3), (Tup2(2, 0), 1, 1)]
    );
    // A sealed reader takes no more rows.
    assert!(derived.push_row(Tup2(3, 0), 1, 1).is_err());
}

fn sealed_rows<R: ReaderDefinition>(reader: &SealedReader<R>) -> Vec<(R::Key, R::Val, ZWeight)> {
    rows(reader.partner().inner())
}

/// Fails to compile unless `T` can be shared between threads.
fn assert_sync<T: Sync>() {}

#[test]
fn a_sealed_reader_keeps_its_requests_and_partner_and_is_sync() {
    assert_sync::<SealedReader<DerivedAtPath>>();

    let mut derived = Reader::<DerivedAtPath>::new().unwrap();
    derived.request(&[1, 2]).unwrap();
    derived.request(&[1, 3]).unwrap();
    derived.push_ordered_row(Tup2(1, 2), 5, 3).unwrap();
    derived.push_ordered_row(Tup2(2, 0), 1, 1).unwrap();
    // The partner batch is still open, and `into_sealed` seals it.
    let sealed = derived.into_sealed().unwrap();
    assert_eq!(
        sealed.requests().iter().collect::<Vec<_>>(),
        vec![&[1u8, 2][..], &[1, 3][..]]
    );
    assert!(sealed.requests().contains(&[1, 3]));
    assert!(!sealed.requests().contains(&[9]));
    assert_eq!(
        sealed_rows(&sealed),
        vec![(Tup2(1, 2), 5, 3), (Tup2(2, 0), 1, 1)]
    );

    // A reader that was already sealed passes through unchanged.
    let mut already = Reader::<DerivedAtPath>::new().unwrap();
    already.push_ordered_row(Tup2(1, 2), 5, 3).unwrap();
    already.seal_partner().unwrap();
    assert_eq!(
        sealed_rows(&already.into_sealed().unwrap()),
        vec![(Tup2(1, 2), 5, 3)]
    );
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

fn group_value(key: &Key) -> u8 {
    match key {
        Key::A(a) | Key::B(a, _) => *a,
    }
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

/// A logical `Old` (each key holds one payload with weight 1 or 2), split at random into two
/// runs, and a `Dlt` that replaces, deletes, or inserts some keys: for each changed key, `-old`
/// if it has one and `+new` if it gets one. A folded key then yields at most three rows (its `Old`
/// row and `Dlt`'s pair), as the index guarantees.
fn old_runs_and_dlt() -> impl Strategy<Value = (Vec<Vec<Row>>, Vec<Row>)> {
    let old = proptest::collection::btree_map(raw_key(), (0u8..3, 1i64..=2, any::<bool>()), 0..10);
    let changes = proptest::collection::btree_map(raw_key(), proptest::option::of(0u8..3), 0..8);
    (old, changes).prop_map(|(old, changes)| {
        let mut runs = vec![Vec::new(), Vec::new()];
        for (key, (payload, weight, second)) in &old {
            runs[usize::from(*second)].push(row(key.clone(), *payload, *weight));
        }
        let mut dlt = Vec::new();
        for (key, new) in changes {
            if let Some((payload, weight, _)) = old.get(&key) {
                dlt.push(row(key.clone(), *payload, -*weight));
            }
            if let Some(payload) = new {
                dlt.push(row(key, payload, 1));
            }
        }
        (runs, dlt)
    })
}

proptest! {
    /// Every reader's requests, driving delta, and partner batch equal what a
    /// brute force over the consolidated `Old` and `Dlt` gives, whatever the
    /// split of `Old` into runs and the pass.
    #[test]
    fn readers_match_a_brute_force(
        (old, dlt) in old_runs_and_dlt(),
        paths in vec((0u8..4, 0u8..3), 0..4),
        step_pass in any::<bool>(),
    ) {
        let mut paths = paths;
        paths.sort_unstable();
        paths.dedup();
        let pass = if step_pass { Pass::Step } else { Pass::Seek };
        let collected = collect_all(&old, &dlt, &paths, pass).unwrap();
        let old_entries = consolidate(&old);
        let dlt_entries = consolidate([&dlt]);

        let touched: BTreeSet<u8> = dlt_entries.keys().map(|(key, _)| group_value(key)).collect();
        prop_assert_eq!(
            collected.captured.touched.clone(),
            touched.iter().map(|a| group_of(*a)).collect::<Vec<_>>()
        );

        // `BByA`: driven by `A` entries; keeps `B` rows of the groups they touch.
        let a_groups: BTreeSet<u8> = dlt_entries
            .keys()
            .filter(|(key, _)| matches!(key, Key::A(_)))
            .map(|(key, _)| group_value(key))
            .collect();
        prop_assert_eq!(
            requests_of(&collected.b_by_a),
            a_groups.iter().map(|a| group_of(*a)).collect::<Vec<_>>()
        );
        let expected_b_by_a: Vec<_> = old_entries
            .iter()
            .filter_map(|((key, payload), weight)| match key {
                Key::B(a, b) if a_groups.contains(a) => Some((*a, Tup2(*b, *payload), *weight)),
                _ => None,
            })
            .collect();
        prop_assert_eq!(summed(partner_rows(&collected.b_by_a)), summed(expected_b_by_a));
        let expected_delta_a: Vec<_> = dlt_entries
            .iter()
            .filter_map(|((key, payload), weight)| match key {
                Key::A(a) => Some((*a, *payload, *weight)),
                _ => None,
            })
            .collect();
        prop_assert_eq!(
            summed(rows::<u8, u8>(collected.delta_b_by_a.into_batch().unwrap().inner())),
            summed(expected_delta_a)
        );

        // `AFromB`: driven by `B` entries; keeps `A` rows of the groups they touch.
        let b_groups: BTreeSet<u8> = dlt_entries
            .keys()
            .filter(|(key, _)| matches!(key, Key::B(..)))
            .map(|(key, _)| group_value(key))
            .collect();
        prop_assert_eq!(
            requests_of(&collected.a_from_b),
            b_groups.iter().map(|a| group_of(*a)).collect::<Vec<_>>()
        );
        let expected_a_from_b: Vec<_> = old_entries
            .iter()
            .filter_map(|((key, payload), weight)| match key {
                Key::A(a) if b_groups.contains(a) => Some((*a, *payload, *weight)),
                _ => None,
            })
            .collect();
        prop_assert_eq!(summed(partner_rows(&collected.a_from_b)), summed(expected_a_from_b));

        // `BAtPath`: the requested `(a, b)` nodes, kept only in touched groups.
        let expected_b_at_path: Vec<_> = old_entries
            .iter()
            .filter_map(|((key, payload), weight)| match key {
                Key::B(a, b) if touched.contains(a) && paths.contains(&(*a, *b)) => {
                    Some((Tup2(*a, *b), *payload, *weight))
                }
                _ => None,
            })
            .collect();
        prop_assert_eq!(summed(partner_rows(&collected.b_at_path)), summed(expected_b_at_path));

        // `DerivedAtPath`: requests at the `(a, b)` node of each `B` entry.
        let b_paths: BTreeSet<(u8, u8)> = dlt_entries
            .keys()
            .filter_map(|(key, _)| match key {
                Key::B(a, b) => Some((*a, *b)),
                Key::A(_) => None,
            })
            .collect();
        prop_assert_eq!(
            requests_of(&collected.derived),
            b_paths.iter().map(|(a, b)| path_of(*a, *b)).collect::<Vec<_>>()
        );
    }
}
