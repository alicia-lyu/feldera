//! Deterministic verification of both builds: a transcript of general-delta
//! transactions, each checked against the in-memory reference; `verify.sh`
//! compares the two builds' transcripts byte for byte. A proptest runs random
//! general changes against the reference.

use std::collections::BTreeMap;

use anyhow::Result;
use dbsp::circuit::CircuitConfig;
#[cfg(feature = "merged-index")]
use dbsp::trace::merged_index::Pass;
use proptest::prelude::*;
use serde_json::{json, Value};

use crate::data::{difference, Change, Line, Part, Partsupp, Reference};
use crate::pipeline::Pipeline;

fn part(key: i64, part_type: &str) -> Part {
    Part {
        key,
        part_type: part_type.into(),
    }
}

fn partsupp(part_key: i64, supplier_key: i64, available: i32) -> Partsupp {
    Partsupp {
        part_key,
        supplier_key,
        available,
    }
}

fn line(order_key: i64, number: i32, part_key: i64, supplier_key: i64) -> Line {
    Line {
        order_key,
        number,
        part_key,
        supplier_key,
        price_cents: 100 * order_key + i64::from(number),
        discount_hundredths: 4,
        ship_date: "1996-03-13".into(),
    }
}

fn inserted<T>(rows: impl IntoIterator<Item = T>) -> Vec<(T, bool)> {
    rows.into_iter().map(|row| (row, true)).collect()
}

fn deleted<T>(rows: impl IntoIterator<Item = T>) -> Vec<(T, bool)> {
    rows.into_iter().map(|row| (row, false)).collect()
}

/// One transaction of a transcript: its name, its change, and whether the
/// circuits run it in three steps.
type Fixture = (&'static str, Change, bool);

/// Every kind of general change, each in its own transaction. An insertion of
/// an existing primary key replaces the row (an upsert).
fn general_fixtures() -> Vec<Fixture> {
    vec![
        (
            "load",
            Change {
                parts: inserted([part(1, "A"), part(2, "B"), part(3, "C"), part(4, "D")]),
                partsupps: inserted([
                    partsupp(1, 10, 5),
                    partsupp(1, 20, 5),
                    partsupp(2, 10, 5),
                    partsupp(3, 10, 5),
                ]),
                // Part 1's second supplier carries a smaller orderkey; part 9
                // does not exist yet, so its line joins nothing.
                lines: inserted([
                    line(100, 1, 1, 10),
                    line(100, 2, 1, 10),
                    line(99, 1, 1, 20),
                    line(101, 1, 2, 10),
                    line(102, 1, 3, 10),
                    line(103, 1, 9, 10),
                ]),
            },
            false,
        ),
        ("empty", Change::default(), false),
        (
            "retype-a-part",
            Change {
                parts: inserted([part(1, "AA")]),
                ..Change::default()
            },
            false,
        ),
        (
            "move-lines-between-parts-and-suppliers",
            Change {
                lines: inserted([
                    line(101, 1, 3, 10),
                    line(102, 1, 3, 20),
                    line(104, 1, 1, 20),
                ]),
                ..Change::default()
            },
            true,
        ),
        (
            "delete-a-part-keep-its-lines",
            Change {
                parts: deleted([part(3, "C")]),
                ..Change::default()
            },
            false,
        ),
        (
            "insert-a-part-for-waiting-lines",
            Change {
                parts: inserted([part(9, "I")]),
                ..Change::default()
            },
            false,
        ),
        (
            "partsupp-only",
            Change {
                partsupps: [
                    (partsupp(1, 10, 6), true),
                    (partsupp(2, 10, 5), false),
                    (partsupp(4, 30, 1), true),
                ]
                .into(),
                ..Change::default()
            },
            false,
        ),
        (
            "part-returns-with-a-new-type-and-a-line",
            Change {
                parts: inserted([part(3, "CC")]),
                lines: inserted([line(105, 1, 3, 10)]),
                ..Change::default()
            },
            true,
        ),
        (
            "delete-lines-and-a-part",
            Change {
                parts: deleted([part(4, "D")]),
                lines: deleted([line(100, 1, 1, 10), line(103, 1, 9, 10)]),
                ..Change::default()
            },
            false,
        ),
        ("empty-in-steps", Change::default(), true),
    ]
}

/// The harness's configuration in tests: one worker, small splitter chunks,
/// memory-backed state.
fn config() -> CircuitConfig {
    CircuitConfig::with_workers(1).with_splitter_chunk_size_records(8)
}

/// Runs `changes` through a fresh pipeline. After each, the net delta must
/// hold one entry per changed row, and the committed change of `V` and the
/// materialized `V` must equal the reference's. Returns the transcript: each
/// transaction's change of `V` and `V`.
fn replay(pipeline: &mut Pipeline, fixtures: Vec<Fixture>) -> Result<Vec<Value>> {
    pipeline.enable_contract_check();
    let mut reference = Reference::default();
    let mut transcript = Vec::new();
    for (name, change, stepped) in fixtures {
        let before = reference.evaluate();
        reference.apply(&change);
        let after = reference.evaluate();

        pipeline.transact(change, stepped)?;
        let (delta, view) = pipeline.view_output()?;
        assert_eq!(delta, difference(&before, &after), "{name}: change of V");
        assert_eq!(view, after, "{name}: V");
        transcript.push(json!({
            "transaction": name,
            "delta": delta.into_iter().collect::<Vec<_>>(),
            "view": view.into_iter().collect::<Vec<_>>(),
        }));
    }
    Ok(transcript)
}

/// Writes a transcript to the file that `variable` names, if it is set. The
/// native and reconstructed builds' transcripts must be byte-identical.
fn write_transcript(variable: &str, transcript: &[Value]) -> Result<()> {
    if let Some(path) = std::env::var_os(variable) {
        std::fs::write(path, serde_json::to_vec_pretty(transcript)?)?;
    }
    Ok(())
}

#[test]
fn general_transcript() -> Result<()> {
    let transcript = replay(&mut Pipeline::new(config())?, general_fixtures())?;
    assert_eq!(transcript.len(), general_fixtures().len());
    write_transcript("Q14_GENERAL_TRANSCRIPT", &transcript)
}

/// The base circuit's net delta: an upsert is a deletion of the old row and an
/// insertion of the new one, and a deletion carries the whole row.
#[test]
fn the_net_delta_has_whole_rows_for_upserts_and_deletions() -> Result<()> {
    let mut pipeline = Pipeline::new(config())?;
    pipeline.transact(
        Change {
            parts: inserted([part(1, "A")]),
            lines: inserted([line(100, 1, 1, 10)]),
            ..Change::default()
        },
        false,
    )?;
    let net_delta = pipeline
        .transact(
            Change {
                parts: inserted([part(1, "B")]),
                lines: deleted([Line {
                    // A deletion by key: the other columns do not matter.
                    part_key: 0,
                    supplier_key: 0,
                    ..line(100, 1, 1, 10)
                }]),
                ..Change::default()
            },
            false,
        )?
        .net_delta;
    let mut parts = net_delta.parts;
    parts.sort();
    assert_eq!(parts, [(part(1, "A"), false), (part(1, "B"), true)]);
    assert_eq!(net_delta.lines, [(line(100, 1, 1, 10), false)]);
    Ok(())
}

/// The scan session's two passes give the same transcript.
#[cfg(feature = "merged-index")]
#[test]
fn the_transcript_does_not_depend_on_the_pass() -> Result<()> {
    let mut transcripts = Vec::new();
    for pass in [Pass::Seek, Pass::Step] {
        let mut pipeline = Pipeline::new(config())?;
        pipeline.set_session_pass(pass);
        transcripts.push(serde_json::to_vec(&replay(
            &mut pipeline,
            general_fixtures(),
        )?)?);
    }
    assert_eq!(transcripts[0], transcripts[1]);
    Ok(())
}

/// One random state of small tables: 4 parts (each absent or of 2 types),
/// Partsupp rows over 2 suppliers, and 6 line items, each absent or on a
/// random part and supplier.
fn arbitrary_state() -> impl Strategy<Value = Reference> {
    let parts = prop::collection::vec(prop::option::of(0..2usize), 4);
    let partsupps = prop::collection::vec(prop::option::of(0..2i32), 8);
    let lines = prop::collection::vec(prop::option::of((0..4i64, 0..2i64)), 6);
    (parts, partsupps, lines).prop_map(|(parts, partsupps, lines)| {
        let mut state = Reference::default();
        let mut change = Change::default();
        for (key, part_type) in parts.into_iter().enumerate() {
            if let Some(part_type) = part_type {
                change
                    .parts
                    .push((part(key as i64, ["A", "B"][part_type]), true));
            }
        }
        for (slot, available) in partsupps.into_iter().enumerate() {
            if let Some(available) = available {
                let (part_key, supplier_key) = (slot as i64 / 2, slot as i64 % 2);
                change
                    .partsupps
                    .push((partsupp(part_key, supplier_key, available), true));
            }
        }
        for (slot, owner) in lines.into_iter().enumerate() {
            if let Some((part_key, supplier_key)) = owner {
                // Orderkeys fall as `slot` rises, so a later supplier's line
                // can carry a smaller orderkey.
                let order_key = 10 - slot as i64 / 2;
                change.lines.push((
                    line(order_key, slot as i32 % 2, part_key, supplier_key),
                    true,
                ));
            }
        }
        state.apply(&change);
        state
    })
}

/// The change from `before` to `after`: upserts of the new and changed rows,
/// deletions of the rest.
fn change_between(before: &Reference, after: &Reference) -> Change {
    fn rows<K: Ord, T: Clone + PartialEq>(
        before: &BTreeMap<K, T>,
        after: &BTreeMap<K, T>,
    ) -> Vec<(T, bool)> {
        let upserts = after
            .iter()
            .filter(|&(key, row)| before.get(key) != Some(row))
            .map(|(_, row)| (row.clone(), true));
        let deletions = before
            .iter()
            .filter(|(key, _)| !after.contains_key(key))
            .map(|(_, row)| (row.clone(), false));
        upserts.chain(deletions).collect()
    }
    Change {
        parts: rows(&before.parts, &after.parts),
        partsupps: rows(&before.partsupps, &after.partsupps),
        lines: rows(&before.lines, &after.lines),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Random sequences of table states: each transaction is the general
    /// change between two of them, in one step or three.
    #[test]
    fn random_general_changes_match_the_reference(
        states in prop::collection::vec(arbitrary_state(), 1..6),
        stepped in any::<bool>(),
    ) {
        let mut before = Reference::default();
        let fixtures = states
            .iter()
            .map(|after| {
                let change = change_between(&before, after);
                before = after.clone();
                ("random", change, stepped)
            })
            .collect();
        let mut pipeline = Pipeline::new(config()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        replay(&mut pipeline, fixtures).map_err(|e| TestCaseError::fail(e.to_string()))?;
    }
}
