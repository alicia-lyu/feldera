use proptest::{
    collection::{btree_set, vec},
    option,
    prelude::*,
    test_runner::{Config, TestCaseError, TestRunner},
};

use super::*;
use crate::trace::{
    RunReads,
    merged_index::{
        ColumnRef, Domain, KeyEncoder, Node, Payload, Placement,
        run_merge::tests::{memory_run, reads_of, snapshot},
    },
    test::run_in_circuit_with_storage,
};

/// One stored row: folded key, payload bytes, and weight.
type Row = (Vec<u8>, Vec<u8>, ZWeight);

/// One run of `Old`, and whether it is stored in a file.
type OldRun = (Vec<Row>, bool);

/// A group's expected buffers: `(old, dlt)` entries by stream id.
type Expected = BTreeMap<u8, (Vec<Entry<u8>>, Vec<Entry<u8>>)>;

/// What one run of the session hands to its callback.
type Groups = Vec<(Vec<u8>, GroupBuffers<u8>)>;

const KNOBS: [(Pass, GroupStart); 4] = [
    (Pass::Seek, GroupStart::PrefixSeek),
    (Pass::Seek, GroupStart::ProbeThenRange),
    (Pass::Step, GroupStart::PrefixSeek),
    (Pass::Step, GroupStart::ProbeThenRange),
];

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

/// Like [`TWO_LEVELS`], but the root node has no placement, so there is no
/// root entry to probe.
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

#[test]
fn fixture_tries_are_valid() {
    for trie in TRIES {
        trie.validate().unwrap();
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

fn decode_byte(_key: &[u8], payload: &[u8]) -> Result<u8, String> {
    payload
        .first()
        .copied()
        .ok_or_else(|| "empty payload".to_string())
}

fn ids(ids: &[u8]) -> BTreeSet<u8> {
    ids.iter().copied().collect()
}

/// Runs a session over `old` and `dlt`, rejecting old entries whose payload
/// byte is in `rejected`, and returns every group with the counts.
fn run_session(
    trie: &KeyTrie,
    old: &SpineSnapshot<MergedIndexBatch>,
    dlt: &MergedIndexBatch,
    (pass, start): (Pass, GroupStart),
    inputs: &BTreeSet<u8>,
    rejected: Option<&BTreeSet<u8>>,
) -> Result<(Groups, SessionCounts), String> {
    let mut session = ScanSession::new(trie, old, dlt, pass, start)?;
    let mut groups = Vec::new();
    let mut reject = rejected.map(|bytes| move |entry: &Entry<u8>| bytes.contains(&entry.payload));
    session.maintain(
        inputs,
        decode_byte,
        reject
            .as_mut()
            .map(|reject| reject as &mut dyn FnMut(&Entry<u8>) -> bool),
        |group, buffers| {
            groups.push((group.to_vec(), buffers.clone()));
            Ok(())
        },
    )?;
    Ok((groups, session.counts()))
}

// ---- The reference: consolidated brute force ----

/// The summed weight of each `(key, payload)`, runs consolidated first as
/// `build_batch` does, zero sums dropped.
fn consolidate<'a>(
    runs: impl IntoIterator<Item = &'a Vec<Row>>,
) -> BTreeMap<(Vec<u8>, Vec<u8>), ZWeight> {
    let mut sums = BTreeMap::new();
    for rows in runs {
        let mut within_run = BTreeMap::new();
        for (key, payload, weight) in rows {
            *within_run
                .entry((key.clone(), payload.clone()))
                .or_insert(0) += weight;
        }
        for (entry, weight) in within_run.into_iter().filter(|&(_, weight)| weight != 0) {
            *sums.entry(entry).or_insert(0) += weight;
        }
    }
    sums.retain(|_, weight| *weight != 0);
    sums
}

/// What `maintain` must hand to its callback: the groups of `dlt` in order;
/// in each, the consolidated old entries of `inputs` under the group, minus
/// the rest of the subtree of any node with a rejected entry, and the delta's
/// entries of `inputs` under the group.
fn reference(
    group_width: usize,
    old_runs: &[OldRun],
    dlt: &[Row],
    inputs: &BTreeSet<u8>,
    rejected: Option<&BTreeSet<u8>>,
) -> Vec<(Vec<u8>, Expected)> {
    let old = consolidate(old_runs.iter().map(|(rows, _)| rows));
    let mut old_by_key: BTreeMap<&Vec<u8>, Vec<(&Vec<u8>, ZWeight)>> = BTreeMap::new();
    for ((key, payload), weight) in &old {
        old_by_key.entry(key).or_default().push((payload, *weight));
    }
    let dlt = consolidate([&dlt.to_vec()]);
    let groups: BTreeSet<Vec<u8>> = dlt
        .keys()
        .map(|(key, _)| key[..group_width].to_vec())
        .collect();

    let entry = |key: &Vec<u8>, payload: &Vec<u8>, weight: ZWeight| Entry {
        key: key.clone(),
        payload: payload[0],
        weight,
    };
    groups
        .into_iter()
        .map(|group| {
            let mut expected = Expected::new();
            let mut skipped: Option<Vec<u8>> = None;
            for (key, values) in &old_by_key {
                if !key.starts_with(&group) {
                    continue;
                }
                if skipped.as_ref().is_some_and(|node| key.starts_with(node)) {
                    continue;
                }
                skipped = None;
                let id = *key.last().unwrap();
                if !inputs.contains(&id) {
                    continue;
                }
                let entries: Vec<_> = values
                    .iter()
                    .map(|&(payload, weight)| entry(key, payload, weight))
                    .collect();
                if rejected.is_some_and(|bytes| entries.iter().any(|e| bytes.contains(&e.payload)))
                {
                    skipped = Some(key[..key.len() - 2].to_vec());
                }
                expected.entry(id).or_default().0.extend(entries);
            }
            for ((key, payload), &weight) in &dlt {
                let id = *key.last().unwrap();
                if key.starts_with(&group) && inputs.contains(&id) {
                    expected
                        .entry(id)
                        .or_default()
                        .1
                        .push(entry(key, payload, weight));
                }
            }
            (group, expected)
        })
        .collect()
}

fn expected_of(buffers: &GroupBuffers<u8>) -> Expected {
    buffers
        .streams()
        .map(|(id, buffer)| (id, (buffer.old.clone(), buffer.dlt.clone())))
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
    dlt: Vec<RawRow>,
    inputs: BTreeSet<u8>,
    rejected: Option<BTreeSet<u8>>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        0..TRIES.len(),
        vec((vec(raw_row(), 0..10), any::<bool>()), 0..4),
        vec(raw_row(), 0..8),
        btree_set(1u8..=5, 1..6),
        option::of(btree_set(0u8..3, 1..3)),
    )
        .prop_map(|(trie, old_runs, dlt, inputs, rejected)| Case {
            trie,
            old_runs,
            dlt,
            inputs,
            rejected,
        })
}

/// All four knob combinations hand over the buffers of the reference.
fn check_case(case: &Case) -> Result<(), TestCaseError> {
    let trie = &TRIES[case.trie];
    let old_runs: Vec<OldRun> = case
        .old_runs
        .iter()
        .map(|(rows, in_file)| (rows.iter().map(|raw| fold(trie, raw)).collect(), *in_file))
        .collect();
    let dlt: Vec<Row> = case.dlt.iter().map(|raw| fold(trie, raw)).collect();

    let old = snapshot::<Vec<u8>>(&old_runs);
    let dlt_run = memory_run::<Vec<u8>>(&dlt);
    let group_width = 1 + trie.root_width().unwrap();
    let expected = reference(
        group_width,
        &old_runs,
        &dlt,
        &case.inputs,
        case.rejected.as_ref(),
    );
    let buffered_old: usize = expected
        .iter()
        .flat_map(|(_, streams)| streams.values().map(|(old, _)| old.len()))
        .sum();
    let buffered_dlt: usize = expected
        .iter()
        .flat_map(|(_, streams)| streams.values().map(|(_, dlt)| dlt.len()))
        .sum();

    for knobs in KNOBS {
        let (groups, counts) = run_session(
            trie,
            &old,
            &dlt_run,
            knobs,
            &case.inputs,
            case.rejected.as_ref(),
        )
        .map_err(TestCaseError::fail)?;
        let actual: Vec<_> = groups
            .iter()
            .map(|(group, buffers)| (group.clone(), expected_of(buffers)))
            .collect();
        prop_assert_eq!(&actual, &expected, "knobs {:?}", knobs);

        // One decode per buffered entry; a cancelled value is never decoded.
        prop_assert_eq!(counts.groups, expected.len() as u64);
        prop_assert_eq!(counts.payload_decodes_old, buffered_old as u64);
        prop_assert_eq!(counts.payload_decodes_dlt, buffered_dlt as u64);
        if knobs.0 == Pass::Step {
            prop_assert_eq!(counts.logical_seeks, 0);
        }
        if knobs.1 == GroupStart::PrefixSeek {
            prop_assert_eq!(counts.probes, 0);
        }
    }
    Ok(())
}

#[test]
fn every_knob_combination_buffers_what_the_brute_force_reference_does() {
    run_in_circuit_with_storage(|| {
        let mut runner = TestRunner::new(Config::with_cases(400));
        runner
            .run(&case(), |case| check_case(&case))
            .unwrap_or_else(|failure| panic!("{failure}"));
    });
}

// ---- Counters and knobs on worked layouts ----

/// Six groups of two keys each, `A(v)` then `B(v, 1)`.
fn six_groups() -> Vec<Row> {
    (1..=6)
        .flat_map(|v| [row(a_key(v), v, 1), row(b_key(v, 1), v, 1)])
        .collect()
}

#[test]
fn positions_outside_inputs_and_the_group_end_cost_no_value_weight_or_payload_decode() {
    run_in_circuit_with_storage(|| {
        // Old: A(1), B(1,1), A(2), B(2,1). The delta touches group 1, and only
        // stream 1 (A) is an input.
        let old = snapshot(&[(
            vec![
                row(a_key(1), 1, 1),
                row(b_key(1, 1), 1, 1),
                row(a_key(2), 2, 1),
                row(b_key(2, 1), 2, 1),
            ],
            true,
        )]);
        let dlt = memory_run(&[row(a_key(1), 9, 1)]);
        for knobs in KNOBS {
            let ((groups, counts), reads) =
                reads_of(|| run_session(&TWO_LEVELS, &old, &dlt, knobs, &ids(&[1]), None).unwrap());
            assert_eq!(groups.len(), 1);
            assert_eq!(groups[0].1.old(1).len(), 1);
            assert_eq!(groups[0].1.dlt(1).len(), 1);

            // Positions: A(1) at open, B(1,1), and A(2), which ends the group.
            // Each costs a key decode. Only A(1) is taken, so one value, one
            // weight, and one payload are decoded.
            assert_eq!(counts.positions, 3, "{knobs:?}");
            assert_eq!(counts.payload_decodes_old, 1);
            assert_eq!(counts.payload_decodes_dlt, 1);
            assert_eq!(counts.logical_steps, 2);
            assert_eq!(
                reads,
                RunReads {
                    key_decodes: 3,
                    value_decodes: 1,
                    weight_decodes: 1,
                    ..RunReads::default()
                },
                "{knobs:?}"
            );
        }
    });
}

/// `Old` is `six_groups`; the delta touches groups 2 and 5. Positions in
/// order: A1 B1 A2 B2 A3 B3 A4 B4 A5 B5 A6 B6 (a letter for the stream, a
/// digit for the group). Only stream 1 is an input.
#[test]
fn seeking_and_stepping_pass_untouched_groups_with_the_same_outputs() {
    run_in_circuit_with_storage(|| {
        let old = snapshot(&[(six_groups(), true)]);
        let dlt = memory_run(&[row(a_key(2), 9, 1), row(a_key(5), 9, 1)]);
        let inputs = ids(&[1]);
        let mut outputs = Vec::new();
        let mut measured = Vec::new();
        for knobs in KNOBS {
            let ((groups, counts), reads) =
                reads_of(|| run_session(&TWO_LEVELS, &old, &dlt, knobs, &inputs, None).unwrap());
            outputs.push(groups);
            measured.push((counts, reads));
        }
        assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(outputs[0].len(), 2);

        // Seek + PrefixSeek. Open on A1 (1 position). Group 2: the seek lands
        // on A2 (position 2); collect takes A2, steps to B2 (3) and A3 (4),
        // which ends the group. Group 5: the seek from A3 lands on A5 (5);
        // collect steps to B5 (6) and A6 (7). Two seeks, four steps, seven
        // positions with a key decode each, two payloads.
        let (counts, reads) = &measured[0];
        assert_eq!(counts.groups, 2);
        assert_eq!(counts.cursor_opens, 2);
        assert_eq!(counts.logical_seeks, 2);
        assert_eq!(counts.probes, 0);
        assert_eq!(counts.logical_steps, 4);
        assert_eq!(counts.positions, 7);
        assert_eq!(counts.payload_decodes_old, 2);
        assert_eq!(counts.payload_decodes_dlt, 2);
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

        // Seek + ProbeThenRange. The probe finds A2, then A5, through the file
        // run's filter and lands there; the prefix seeks that follow move
        // nothing. So two probes, no logical seek, and the same seven
        // positions and decodes. The two run seeks are the probes'.
        let (counts, reads) = &measured[1];
        assert_eq!((counts.probes, counts.logical_seeks), (2, 0));
        assert_eq!((counts.logical_steps, counts.positions), (4, 7));
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

        // Step + PrefixSeek. Group 2 steps A1, B1 to reach A2 (2 steps), then
        // 2 steps end the group. Group 5 steps A3, B3, A4, B4 to reach A5 (4
        // steps), then 2. Ten steps, eleven positions, no seek.
        let (counts, reads) = &measured[2];
        assert_eq!((counts.logical_seeks, counts.probes), (0, 0));
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

        // Step + ProbeThenRange. The probes land on A2 and A5 as above, so
        // only the 2 + 2 steps of the groups remain.
        let (counts, reads) = &measured[3];
        assert_eq!((counts.logical_seeks, counts.probes), (0, 2));
        assert_eq!((counts.logical_steps, counts.positions), (4, 7));
        assert_eq!(reads.seeks, 2);
        assert_eq!(reads.key_decodes, 7);
    });
}

#[test]
fn a_rejected_node_is_passed_by_one_seek_or_by_steps() {
    run_in_circuit_with_storage(|| {
        // Group 1's root entry has payload 9, which the test rejects. The rest
        // of its node, B(1,1) and B(1,2), is passed unread.
        let old = snapshot(&[(
            vec![
                row(a_key(1), 9, 1),
                row(b_key(1, 1), 1, 1),
                row(b_key(1, 2), 1, 1),
                row(a_key(2), 1, 1),
            ],
            true,
        )]);
        let dlt = memory_run(&[row(a_key(1), 5, 1)]);
        let rejected = ids(&[9]);

        for pass in [Pass::Seek, Pass::Step] {
            let ((groups, counts), reads) = reads_of(|| {
                run_session(
                    &TWO_LEVELS,
                    &old,
                    &dlt,
                    (pass, GroupStart::PrefixSeek),
                    &ids(&[1, 2]),
                    Some(&rejected),
                )
                .unwrap()
            });
            // The rejected entry stays buffered; the B entries are not.
            assert_eq!(groups[0].1.old(1).len(), 1);
            assert!(groups[0].1.old(2).is_empty());
            assert_eq!(counts.payload_decodes_old, 1);
            assert_eq!(reads.value_decodes, 1);
            assert_eq!(reads.weight_decodes, 1);
            match pass {
                // One seek to [1, 2]: A(1), then A(2) are the only key decodes.
                Pass::Seek => {
                    assert_eq!((counts.logical_seeks, counts.logical_steps), (1, 0));
                    assert_eq!((counts.positions, reads.key_decodes), (2, 2));
                }
                // Steps through B(1,1), B(1,2), and A(2).
                Pass::Step => {
                    assert_eq!((counts.logical_seeks, counts.logical_steps), (0, 3));
                    assert_eq!((counts.positions, reads.key_decodes), (4, 4));
                }
            }
        }
    });
}

#[test]
fn a_prefix_with_no_successor_is_skipped_to_the_end() {
    // Root tag and value are all 0xFF, so the rejected node has no successor
    // and the skip runs to the end of the index.
    let root_entry = KeyEncoder::new()
        .field(u8::MAX, &u8::MAX)
        .finish(1)
        .as_bytes()
        .to_vec();
    let old = snapshot(&[(vec![row(root_entry.clone(), 9, 1)], false)]);
    let dlt = memory_run(&[row(root_entry, 5, 1)]);
    let rejected = ids(&[9]);
    for pass in [Pass::Seek, Pass::Step] {
        let (groups, counts) = run_session(
            &TWO_LEVELS,
            &old,
            &dlt,
            (pass, GroupStart::PrefixSeek),
            &ids(&[1]),
            Some(&rejected),
        )
        .unwrap();
        assert_eq!(groups[0].1.old(1).len(), 1);
        assert_eq!(counts.logical_seeks, u64::from(pass == Pass::Seek));
        assert_eq!(counts.logical_steps, u64::from(pass == Pass::Step));
    }
}

#[test]
fn a_probe_is_made_only_when_the_trie_has_a_root_entry() {
    run_in_circuit_with_storage(|| {
        let old = snapshot(&[(vec![row(b_key(1, 1), 1, 1), row(b_key(3, 1), 1, 1)], true)]);
        let dlt = memory_run(&[row(b_key(3, 1), 5, 1)]);
        let probing = (Pass::Seek, GroupStart::ProbeThenRange);
        let (_, counts) =
            run_session(&NO_ROOT_ENTRY, &old, &dlt, probing, &ids(&[1]), None).unwrap();
        assert_eq!(counts.probes, 0);
        let (_, counts) = run_session(&TWO_LEVELS, &old, &dlt, probing, &ids(&[1]), None).unwrap();
        assert_eq!(counts.probes, 1);
    });
}

// ---- Errors ----

/// Two top-level nodes, whose keys begin with different tags.
const TWO_ROOTS: KeyTrie = KeyTrie {
    children: &[leaf("a", &[placement("A")]), leaf("b", &[placement("B")])],
    ..TWO_LEVELS
};

#[test]
fn a_trie_without_one_root_node_is_rejected() {
    let old = snapshot::<Vec<u8>>(&[]);
    let dlt = memory_run::<Vec<u8>>(&[]);
    let error = ScanSession::new(&TWO_ROOTS, &old, &dlt, Pass::Seek, GroupStart::PrefixSeek)
        .err()
        .unwrap();
    assert!(error.contains("one top-level node"), "{error}");
}

#[test]
fn a_malformed_old_key_is_an_error() {
    // [1, 1, 5, 5] has no terminator before the id.
    let old = snapshot(&[(vec![row(vec![1, 1, 5, 5], 1, 1)], false)]);
    let dlt = memory_run(&[row(a_key(1), 5, 1)]);
    let error = run_session(
        &TWO_LEVELS,
        &old,
        &dlt,
        (Pass::Seek, GroupStart::PrefixSeek),
        &ids(&[1]),
        None,
    )
    .err()
    .unwrap();
    assert!(error.contains("malformed folded key"), "{error}");
}

#[test]
fn a_decode_error_stops_the_session() {
    let old = snapshot(&[(vec![row(a_key(1), 1, 1)], false)]);
    let dlt = memory_run(&[row(a_key(1), 5, 1)]);
    let mut session =
        ScanSession::new(&TWO_LEVELS, &old, &dlt, Pass::Seek, GroupStart::PrefixSeek).unwrap();
    let result = session.maintain(
        &ids(&[1]),
        |_, _| Err::<u8, _>("bad payload".to_string()),
        None,
        |_, _| panic!("no group completes"),
    );
    assert_eq!(result, Err("bad payload".to_string()));
}
