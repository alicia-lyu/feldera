use std::collections::{BTreeMap, BTreeSet};

use proptest::{
    collection::{btree_set, vec},
    prelude::*,
    test_runner::{Config, TestCaseError, TestRunner},
};

use super::*;
use crate::trace::{
    RunReads,
    merged_index::{
        ColumnRef, Domain, FoldedKey, KeyEncoder, KeyTrie, Node, Payload, Placement,
        run_merge::tests::{reads_of, snapshot},
    },
    test::run_in_circuit_with_storage,
};

/// One stored row: folded key, payload bytes, and weight.
type Row = (Vec<u8>, Vec<u8>, ZWeight);

/// One run of `Old`, and whether it is stored in a file.
type OldRun = (Vec<Row>, bool);

/// What one pass over a touched group saw: the folded key of every position
/// in order, and the decoded entries of the positions that the test kept.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Visited {
    group: Vec<u8>,
    positions: Vec<Vec<u8>>,
    kept: Vec<(Vec<u8>, Vec<(u8, ZWeight)>)>,
}

const PASSES: [Pass; 2] = [Pass::Seek, Pass::Step];

// ---- Fixture tries ----

const fn column(relation: &'static str, column: &'static str) -> ColumnRef {
    ColumnRef { relation, column }
}

const fn placement(relation: &'static str) -> Placement {
    Placement {
        relation,
        via: &[],
        payload: Payload::All,
    }
}

const fn leaf(domain: &'static str, placements: &'static [Placement]) -> Node {
    Node {
        domain,
        placements,
        children: &[],
    }
}

/// Two levels of one-byte domains: ids 1 (`A` at `a`) and 2 (`B` at `a.b`).
const TWO_LEVELS: KeyTrie = KeyTrie {
    name: "TwoLevels",
    domains: &[
        Domain {
            name: "a",
            columns: &[column("A", "a"), column("B", "a")],
            width: 1,
        },
        Domain {
            name: "b",
            columns: &[column("B", "b")],
            width: 1,
        },
    ],
    children: &[Node {
        domain: "a",
        placements: &[placement("A")],
        children: &[leaf("b", &[placement("B")])],
    }],
};

/// A two-byte root, two placements at the root node (ids 1 and 2), and a
/// branch below it: `A`, `A2` at `a`; `B` at `a.b` (3) with `C` at `a.b.c`
/// (4); `D` at `a.e` (5).
const BRANCHING: KeyTrie = KeyTrie {
    name: "Branching",
    domains: &[
        Domain {
            name: "a",
            columns: &[
                column("A", "a"),
                column("A2", "a"),
                column("B", "a"),
                column("C", "a"),
                column("D", "a"),
            ],
            width: 2,
        },
        Domain {
            name: "b",
            columns: &[column("B", "b"), column("C", "b")],
            width: 1,
        },
        Domain {
            name: "c",
            columns: &[column("C", "c")],
            width: 1,
        },
        Domain {
            name: "e",
            columns: &[column("D", "e")],
            width: 1,
        },
    ],
    children: &[Node {
        domain: "a",
        placements: &[placement("A"), placement("A2")],
        children: &[
            Node {
                domain: "b",
                placements: &[placement("B")],
                children: &[leaf("c", &[placement("C")])],
            },
            leaf("e", &[placement("D")]),
        ],
    }],
};

/// Like [`TWO_LEVELS`], but the root node has no placement, so no key ends at
/// the root node.
const NO_ROOT_ENTRY: KeyTrie = KeyTrie {
    name: "NoRootEntry",
    domains: &[
        Domain {
            name: "a",
            columns: &[column("B", "a")],
            width: 1,
        },
        Domain {
            name: "b",
            columns: &[column("B", "b")],
            width: 1,
        },
    ],
    children: &[Node {
        domain: "a",
        placements: &[],
        children: &[leaf("b", &[placement("B")])],
    }],
};

const TRIES: [KeyTrie; 3] = [TWO_LEVELS, BRANCHING, NO_ROOT_ENTRY];

// ---- Fixture definitions ----

/// Defines a test index over `$trie`. Its key is the folded bytes themselves,
/// and its payload is the first stored byte, whatever the stream. The session
/// never folds or unfolds, so `fold` and `unfold` only copy bytes.
macro_rules! test_definition {
    ($name:ident, $trie:expr) => {
        struct $name;

        impl MergedIndexDefinition for $name {
            type Key = Vec<u8>;
            type Payload = u8;

            const TRIE: KeyTrie = $trie;

            fn fold(key: &Vec<u8>) -> FoldedKey {
                FoldedKey(key.clone())
            }

            fn unfold(bytes: &[u8]) -> Result<Vec<u8>, String> {
                Ok(bytes.to_vec())
            }

            fn decode_payload(_stream_id: u8, bytes: &[u8]) -> Result<u8, String> {
                first_byte(bytes)
            }
        }
    };
}

fn first_byte(bytes: &[u8]) -> Result<u8, String> {
    bytes
        .first()
        .copied()
        .ok_or_else(|| "empty payload".to_string())
}

test_definition!(TwoLevelsIndex, TWO_LEVELS);
test_definition!(BranchingIndex, BRANCHING);
test_definition!(NoRootEntryIndex, NO_ROOT_ENTRY);

/// [`TWO_LEVELS`] again, but only stream 1 has a payload type: the decoder
/// keys on the stream id.
struct StreamOneIndex;

impl MergedIndexDefinition for StreamOneIndex {
    type Key = Vec<u8>;
    type Payload = u8;

    const TRIE: KeyTrie = TWO_LEVELS;

    fn fold(key: &Vec<u8>) -> FoldedKey {
        FoldedKey(key.clone())
    }

    fn unfold(bytes: &[u8]) -> Result<Vec<u8>, String> {
        Ok(bytes.to_vec())
    }

    fn decode_payload(stream_id: u8, bytes: &[u8]) -> Result<u8, String> {
        match stream_id {
            1 => first_byte(bytes),
            _ => Err(format!("unknown stream {stream_id}")),
        }
    }
}

// ---- Helpers ----

/// The folded key of placement `A` of [`TWO_LEVELS`].
fn a_key(a: u8) -> Vec<u8> {
    KeyEncoder::new().field(1, &a).finish(1).as_bytes().to_vec()
}

/// The folded key of placement `B` of [`TWO_LEVELS`].
fn b_key(a: u8, b: u8) -> Vec<u8> {
    KeyEncoder::new()
        .field(1, &a)
        .field(2, &b)
        .finish(2)
        .as_bytes()
        .to_vec()
}

fn row(key: Vec<u8>, payload: u8, weight: ZWeight) -> Row {
    (key, vec![payload], weight)
}

/// The folded key of the group of `a` in [`TWO_LEVELS`]: the tag and value of
/// the root domain.
fn group_of(a: u8) -> Vec<u8> {
    KeyEncoder::new().field(1, &a).prefix()
}

/// Runs a session over `old` and the touched groups. At each position of a
/// group it reads and decodes the payload of exactly the keys that `keep`
/// accepts, and steps on. Returns what it saw in each group.
fn run_session<D: MergedIndexDefinition<Payload = u8>>(
    old: &SpineSnapshot<MergedIndexBatch>,
    touched: &[Vec<u8>],
    pass: Pass,
    keep: impl Fn(&[u8]) -> bool,
) -> Result<(Vec<Visited>, SessionCounts), String> {
    let mut session = ScanSession::<D>::new(old, touched, pass);
    let mut visited = Vec::new();
    while let Some(group) = session.next_group() {
        let mut seen = Visited {
            group: group.to_vec(),
            positions: Vec::new(),
            kept: Vec::new(),
        };
        while let Some(key) = session.key() {
            let key = key.to_vec();
            if keep(&key) {
                let entries = session.take_and_decode()?;
                seen.kept.push((key.clone(), entries));
            }
            seen.positions.push(key);
            session.step();
        }
        visited.push(seen);
    }
    Ok((visited, session.counts()))
}

/// Keeps the keys of the streams in `streams`.
fn of_streams(streams: &[u8]) -> impl Fn(&[u8]) -> bool {
    let streams: BTreeSet<u8> = streams.iter().copied().collect();
    move |key| key.last().is_some_and(|id| streams.contains(id))
}

// ---- The reference: consolidated brute force ----

/// The consolidated entries of each key across `runs`: payload bytes (as the
/// test's decoded byte) with their weights summed, zero sums dropped. A run
/// consolidates its own duplicates first, as `build_batch` does. Also returns
/// every key that some run still holds: those are the positions.
fn consolidate(runs: &[OldRun]) -> (BTreeMap<Vec<u8>, Vec<(u8, ZWeight)>>, BTreeSet<Vec<u8>>) {
    let mut sums: BTreeMap<(Vec<u8>, u8), ZWeight> = BTreeMap::new();
    let mut positions = BTreeSet::new();
    for (rows, _) in runs {
        let mut within_run: BTreeMap<(Vec<u8>, u8), ZWeight> = BTreeMap::new();
        for (key, payload, weight) in rows {
            *within_run.entry((key.clone(), payload[0])).or_insert(0) += weight;
        }
        for ((key, payload), weight) in within_run {
            if weight != 0 {
                positions.insert(key.clone());
                *sums.entry((key, payload)).or_insert(0) += weight;
            }
        }
    }
    let mut entries: BTreeMap<Vec<u8>, Vec<(u8, ZWeight)>> = BTreeMap::new();
    for ((key, payload), weight) in sums {
        if weight != 0 {
            entries.entry(key).or_default().push((payload, weight));
        }
    }
    (entries, positions)
}

/// What a scan must see: in each touched group, every key of `Old` under it in
/// order, and the consolidated entries of the keys that `keep` accepts. A kept
/// key whose entries all cancel is kept with no entry.
fn reference(
    old_runs: &[OldRun],
    touched: &[Vec<u8>],
    keep: &dyn Fn(&[u8]) -> bool,
) -> Vec<Visited> {
    let (entries, positions) = consolidate(old_runs);
    touched
        .iter()
        .map(|group| {
            let positions: Vec<Vec<u8>> = positions
                .iter()
                .filter(|key| key.starts_with(group))
                .cloned()
                .collect();
            let kept = positions
                .iter()
                .filter(|key| keep(key))
                .map(|key| (key.clone(), entries.get(key).cloned().unwrap_or_default()))
                .collect();
            Visited {
                group: group.clone(),
                positions,
                kept,
            }
        })
        .collect()
}

// ---- Random cases ----

/// A row before it is folded: which placement, which value in each domain on
/// its path, and the payload and weight.
#[derive(Clone, Debug)]
struct RawRow {
    placement: usize,
    picks: [usize; 3],
    payload: u8,
    weight: ZWeight,
}

fn raw_row() -> impl Strategy<Value = RawRow> {
    (
        0usize..8,
        (0usize..2, 0usize..3, 0usize..3).prop_map(|(root, child, leaf)| [root, child, leaf]),
        0u8..3,
        prop_oneof![-2i64..=-1, 1i64..=2],
    )
        .prop_map(|(placement, picks, payload, weight)| RawRow {
            placement,
            picks,
            payload,
            weight,
        })
}

/// Values near the boundaries of a domain's bytes, so that a prefix's
/// successor must carry: `0xFF` and `0xFFFF` appear in every domain.
fn pick_value(width: usize, pick: usize) -> u64 {
    const NARROW: [u64; 4] = [0, 1, 254, 255];
    const WIDE: [u64; 4] = [0, 0x00FF, 0x0100, 0xFFFF];
    match width {
        1 => NARROW[pick % 4],
        2 => WIDE[pick % 4],
        _ => unreachable!("the fixtures use widths 1 and 2"),
    }
}

/// Folds `raw` into a row of a placement of `trie`, with `KeyEncoder`.
fn fold(trie: &KeyTrie, raw: &RawRow) -> Row {
    let placements = trie.placements();
    let placed = &placements[raw.placement % placements.len()];
    let mut encoder = KeyEncoder::new();
    for (depth, domain) in placed.path.iter().enumerate() {
        let tag = trie.domain_tag(domain).unwrap();
        let width = trie
            .domains
            .iter()
            .find(|candidate| candidate.name == *domain)
            .unwrap()
            .width;
        let value = pick_value(width, raw.picks[depth]);
        encoder = match width {
            1 => encoder.field(tag, &(value as u8)),
            _ => encoder.field(tag, &(value as u16)),
        };
    }
    let key = encoder.finish(placed.id).as_bytes().to_vec();
    (key, vec![raw.payload], raw.weight)
}

#[derive(Clone, Debug)]
struct Case {
    trie: usize,
    old_runs: Vec<(Vec<RawRow>, bool)>,
    /// Rows whose groups are the touched groups.
    touching: Vec<RawRow>,
    kept_streams: BTreeSet<u8>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        0..TRIES.len(),
        vec((vec(raw_row(), 0..10), any::<bool>()), 0..4),
        vec(raw_row(), 0..8),
        btree_set(1u8..=5, 0..6),
    )
        .prop_map(|(trie, old_runs, touching, kept_streams)| Case {
            trie,
            old_runs,
            touching,
            kept_streams,
        })
}

/// Runs `case` over the definition that its trie index names.
fn check_case(case: &Case) -> Result<(), TestCaseError> {
    match case.trie {
        0 => check_case_of::<TwoLevelsIndex>(case),
        1 => check_case_of::<BranchingIndex>(case),
        2 => check_case_of::<NoRootEntryIndex>(case),
        other => unreachable!("trie index {other} is out of range"),
    }
}

/// Both passes visit every key of each touched group and decode exactly the
/// kept entries, as the reference does.
fn check_case_of<D: MergedIndexDefinition<Payload = u8>>(case: &Case) -> Result<(), TestCaseError> {
    let trie = &D::TRIE;
    let old_runs: Vec<OldRun> = case
        .old_runs
        .iter()
        .map(|(rows, in_file)| (rows.iter().map(|raw| fold(trie, raw)).collect(), *in_file))
        .collect();
    let group_width = 1 + trie.root_width();
    let touched: Vec<Vec<u8>> = case
        .touching
        .iter()
        .map(|raw| fold(trie, raw).0[..group_width].to_vec())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let old = snapshot::<Vec<u8>>(&old_runs);
    let kept_streams: Vec<u8> = case.kept_streams.iter().copied().collect();
    let expected = reference(&old_runs, &touched, &of_streams(&kept_streams));
    let kept_payloads: usize = expected
        .iter()
        .flat_map(|visited| visited.kept.iter().map(|(_, entries)| entries.len()))
        .sum();

    for pass in PASSES {
        let (visited, counts) = run_session::<D>(&old, &touched, pass, of_streams(&kept_streams))
            .map_err(TestCaseError::fail)?;
        prop_assert_eq!(&visited, &expected, "pass {:?}", pass);

        // One decode per kept payload; a cancelled value is never decoded.
        prop_assert_eq!(counts.groups, touched.len() as u64);
        prop_assert_eq!(counts.payload_decodes_old, kept_payloads as u64);
        prop_assert_eq!(counts.cursor_opens, 1);
        if pass == Pass::Step {
            prop_assert_eq!(counts.logical_seeks, 0);
        }
    }
    Ok(())
}

#[test]
fn both_passes_scan_the_touched_groups_as_the_brute_force_reference_does() {
    run_in_circuit_with_storage(|| {
        let mut runner = TestRunner::new(Config::with_cases(400));
        runner
            .run(&case(), |case| check_case(&case))
            .unwrap_or_else(|failure| panic!("{failure}"));
    });
}

// ---- Counters and passes on worked layouts ----

/// Six groups of two keys each, `A(v)` then `B(v, 1)`.
fn six_groups() -> Vec<Row> {
    (1..=6)
        .flat_map(|v| [row(a_key(v), v, 1), row(b_key(v, 1), v, 1)])
        .collect()
}

#[test]
fn positions_nobody_keeps_and_the_group_end_cost_no_value_weight_or_payload_decode() {
    run_in_circuit_with_storage(|| {
        // Old: A(1), B(1,1), A(2), B(2,1). Group 1 is touched, and the test
        // keeps only stream 1 (A).
        let old = snapshot(&[(
            vec![
                row(a_key(1), 1, 1),
                row(b_key(1, 1), 1, 1),
                row(a_key(2), 2, 1),
                row(b_key(2, 1), 2, 1),
            ],
            true,
        )]);
        for pass in PASSES {
            let ((visited, counts), reads) = reads_of(|| {
                run_session::<TwoLevelsIndex>(&old, &[group_of(1)], pass, of_streams(&[1])).unwrap()
            });
            assert_eq!(visited.len(), 1);
            assert_eq!(visited[0].positions, [a_key(1), b_key(1, 1)]);
            assert_eq!(visited[0].kept, [(a_key(1), vec![(1, 1)])]);

            // Positions: A(1) at open, B(1,1), and A(2), which ends the group.
            // Each costs a key decode. Only A(1) is taken, so one value, one
            // weight, and one payload are decoded. The session never reads
            // `Dlt`, so no memory run is read.
            assert_eq!(counts.positions, 3, "{pass:?}");
            assert_eq!(counts.payload_decodes_old, 1);
            assert_eq!(counts.logical_steps, 2);
            assert_eq!(
                reads,
                RunReads {
                    key_decodes: 3,
                    value_decodes: 1,
                    weight_decodes: 1,
                    ..RunReads::default()
                },
                "{pass:?}"
            );
        }
    });
}

/// `Old` is `six_groups`; groups 2 and 5 are touched. Positions in order: A1
/// B1 A2 B2 A3 B3 A4 B4 A5 B5 A6 B6 (a letter for the stream, a digit for the
/// group). The test keeps stream 1.
#[test]
fn seeking_and_stepping_pass_untouched_groups_with_the_same_outputs() {
    run_in_circuit_with_storage(|| {
        let old = snapshot(&[(six_groups(), true)]);
        let touched = [group_of(2), group_of(5)];
        let mut outputs = Vec::new();
        let mut measured = Vec::new();
        for pass in PASSES {
            let ((visited, counts), reads) = reads_of(|| {
                run_session::<TwoLevelsIndex>(&old, &touched, pass, of_streams(&[1])).unwrap()
            });
            outputs.push(visited);
            measured.push((counts, reads));
        }
        assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(outputs[0].len(), 2);

        // Seek. Open on A1 (1 position). Group 2: the seek lands on A2
        // (position 2); the scan takes A2, steps to B2 (3) and A3 (4), which
        // ends the group. Group 5: the seek from A3 lands on A5 (5); the scan
        // steps to B5 (6) and A6 (7). Two seeks, four steps, seven positions
        // with a key decode each, two payloads.
        let (counts, reads) = &measured[0];
        assert_eq!(counts.groups, 2);
        assert_eq!(counts.cursor_opens, 1);
        assert_eq!(counts.logical_seeks, 2);
        assert_eq!(counts.logical_steps, 4);
        assert_eq!(counts.positions, 7);
        assert_eq!(counts.payload_decodes_old, 2);
        assert_eq!(
            *reads,
            RunReads {
                seeks: 2,
                key_decodes: 7,
                value_decodes: 2,
                weight_decodes: 2,
                ..RunReads::default()
            }
        );

        // Step. Group 2 steps A1, B1 to reach A2 (2 steps), then 2 steps end
        // the group. Group 5 steps A3, B3, A4, B4 to reach A5 (4 steps), then
        // 2. Ten steps, eleven positions, no seek.
        let (counts, reads) = &measured[1];
        assert_eq!(counts.logical_seeks, 0);
        assert_eq!((counts.logical_steps, counts.positions), (10, 11));
        assert_eq!(
            *reads,
            RunReads {
                key_decodes: 11,
                value_decodes: 2,
                weight_decodes: 2,
                ..RunReads::default()
            }
        );
    });
}

#[test]
fn a_group_is_scanned_contiguously_with_no_skip() {
    run_in_circuit_with_storage(|| {
        // Group 1 holds A(1) and three B entries, and nobody keeps any B.
        // The scan still reads a key at each of the five positions (the four
        // of the group and the one that ends it): skipping what nobody
        // requests is a deferred refinement.
        let old = snapshot(&[(
            vec![
                row(a_key(1), 1, 1),
                row(b_key(1, 1), 1, 1),
                row(b_key(1, 2), 1, 1),
                row(b_key(1, 3), 1, 1),
                row(a_key(2), 1, 1),
            ],
            true,
        )]);
        for pass in PASSES {
            let ((visited, counts), reads) = reads_of(|| {
                run_session::<TwoLevelsIndex>(&old, &[group_of(1)], pass, of_streams(&[1])).unwrap()
            });
            assert_eq!(visited[0].positions.len(), 4);
            assert_eq!((counts.logical_steps, counts.positions), (4, 5), "{pass:?}");
            assert_eq!(reads.key_decodes, 5);
            assert_eq!(reads.value_decodes, 1);
        }
    });
}

#[test]
fn a_taken_position_is_taken_once() {
    let old = snapshot(&[(vec![row(a_key(1), 7, 3)], false)]);
    let touched = [group_of(1)];
    let mut session = ScanSession::<TwoLevelsIndex>::new(&old, &touched, Pass::Seek);
    assert_eq!(session.next_group(), Some(group_of(1).as_slice()));
    assert_eq!(session.take_and_decode(), Ok(vec![(7, 3)]));
    assert_eq!(session.take_and_decode(), Ok(vec![]));
    assert_eq!(session.counts().payload_decodes_old, 1);
    session.step();
    assert_eq!(session.key(), None);
    assert!(session.next_group().is_none());
}

#[test]
fn nothing_is_read_outside_the_touched_groups_and_an_empty_list_touches_no_group() {
    let old = snapshot(&[(six_groups(), false)]);
    let mut session = ScanSession::<TwoLevelsIndex>::new(&old, &[], Pass::Seek);
    assert!(session.next_group().is_none());
    assert_eq!(session.key(), None);
    assert!(session.take_and_decode().is_err());
    assert_eq!(session.counts().groups, 0);

    // A group past the end of `Old` is entered and holds no position.
    let touched = [group_of(9)];
    for pass in PASSES {
        let (visited, counts) =
            run_session::<TwoLevelsIndex>(&old, &touched, pass, of_streams(&[1, 2])).unwrap();
        assert_eq!(visited[0].positions, Vec::<Vec<u8>>::new());
        assert_eq!(counts.groups, 1);
        assert_eq!(counts.payload_decodes_old, 0);
    }
}

// ---- Errors ----

#[test]
fn a_malformed_old_key_is_an_error() {
    // [1, 1, 5, 5] has no terminator before the id.
    let old = snapshot(&[(vec![row(vec![1, 1, 5, 5], 1, 1)], false)]);
    let error = run_session::<TwoLevelsIndex>(&old, &[group_of(1)], Pass::Seek, |_| true)
        .err()
        .unwrap();
    assert!(error.contains("malformed folded key"), "{error}");
}

#[test]
fn a_decode_error_stops_the_session() {
    // The old payload is empty, which the decoder rejects.
    let old = snapshot(&[(vec![(a_key(1), Vec::<u8>::new(), 1)], false)]);
    let result = run_session::<TwoLevelsIndex>(&old, &[group_of(1)], Pass::Seek, |_| true);
    assert_eq!(result.err(), Some("empty payload".to_string()));
}

#[test]
fn the_decoder_is_chosen_by_the_stream_id() {
    // A(1) is stream 1 and B(1, 1) is stream 2, which the definition cannot
    // decode. Keeping stream 1 alone never reaches that decoder.
    let old = snapshot(&[(vec![row(a_key(1), 1, 1), row(b_key(1, 1), 1, 1)], false)]);
    let touched = [group_of(1)];
    let (visited, _) =
        run_session::<StreamOneIndex>(&old, &touched, Pass::Seek, of_streams(&[1])).unwrap();
    assert_eq!(visited[0].kept.len(), 1);

    let error =
        run_session::<StreamOneIndex>(&old, &touched, Pass::Seek, of_streams(&[1, 2])).unwrap_err();
    assert_eq!(error, "unknown stream 2");
}
