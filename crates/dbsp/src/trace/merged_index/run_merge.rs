//! A merging cursor over the runs of a merged-index snapshot.
//!
//! A snapshot of the index is several immutable runs, and an entry may be
//! split across them: a deletion not yet compacted sits in one run, the
//! insertion it cancels in another. [`RunMerge`] walks the union of the runs'
//! folded keys in order, and reads weights and values only when the caller
//! takes an entry.
//!
//! Compare [`CursorList`](crate::trace::cursor::CursorList), which consolidates
//! as it moves: at every landing it reads each run's first value and weight, to
//! skip a key whose weights cancel. A scan session passes many entries that it
//! buffers nothing from, so [`RunMerge::step`] and [`RunMerge::seek`] read only
//! keys. Over file runs, that is a key decode per landing and no value or
//! weight decode; over memory runs, a key read per landing and no value or
//! weight read (see [`RunReads`](crate::trace::RunReads)).
//!
//! # Position
//!
//! Every run has a cursor, and the merge's position is the smallest key among
//! them. [`RunMerge::key`] returns it, and the runs *at the key* are those whose
//! cursor is on it. A key whose entries all cancel is still a position: only
//! [`RunMerge::take`], which sums the weights, finds that out.
//!
//! # Taking an entry
//!
//! [`RunMerge::take`] visits the values at the current key in ascending order,
//! each with its weight summed over the runs, and skips a value whose weights
//! sum to zero. It counts the skipped values ([`RunMerge::cancelled`]): they
//! cost a value and a weight read each, an inherent price of reading an
//! uncompacted run set. `take` consumes the values of the runs at the key, so
//! it visits nothing the second time. [`RunMerge::step`] and
//! [`RunMerge::seek`] reposition the values of the keys they land on.
//!
//! # Group-scoped seeks
//!
//! [`RunMerge::seek_group`] seeks to a group's prefix, but first asks each run
//! behind it whether it can hold a key with that prefix
//! ([`BatchReader::maybe_contains_prefix`]: the run's key bounds, then its
//! prefix filter). A run that cannot is *parked*: it is not sought, and it takes
//! no part in the position until the next seek. Its keys behind the position
//! all lie outside the group, so the position is right for every key in the
//! group, and only there: once the smallest key leaves the group,
//! [`RunMerge::key`] returns `None`, since a parked run may hold smaller keys
//! beyond the group. The next seek moves every run that is behind its target,
//! the parked ones included.

use std::cmp::Ordering;

use crate::{
    ZWeight,
    dynamic::{DowncastTrait, DynData, Erase},
    storage::file::PrefixSpec,
    trace::{Batch, BatchReader, Cursor, SpineSnapshot},
};

use super::MergedIndexBatch;

type RunCursor<'a> = <MergedIndexBatch as BatchReader>::Cursor<'a>;

/// A merging cursor over the runs of a snapshot; see the [module docs](self).
pub struct RunMerge<'a> {
    runs: Vec<RunCursor<'a>>,

    /// The batch of each run, which answers group-scoped probes.
    batches: Vec<&'a MergedIndexBatch>,

    /// The prefix that [`RunMerge::seek_group`] probes with; without one it
    /// seeks every run, as [`RunMerge::seek`] does.
    group_prefix: Option<PrefixSpec<DynData>>,

    /// The runs that the last [`RunMerge::seek_group`] skipped, by index.
    parked: Vec<bool>,

    /// The group of the last [`RunMerge::seek_group`] while any run is parked:
    /// the position is valid only inside it.
    parked_group: Option<Vec<u8>>,

    /// The indexes of the runs at the smallest key, in run order. Empty when
    /// every run is exhausted.
    at_key: Vec<usize>,

    /// Values that [`RunMerge::take`] found to cancel across runs.
    cancelled: u64,

    /// Set by [`RunMerge::exhaust`]: the position is past every key, though
    /// the run cursors are not.
    exhausted: bool,
}

impl<'a> RunMerge<'a> {
    /// Opens one cursor per run of `snapshot`, and positions on the smallest
    /// first key. A file run decodes its first key and nothing else.
    pub fn new(snapshot: &'a SpineSnapshot<MergedIndexBatch>) -> Self {
        Self::with_group_prefix(snapshot, None)
    }

    /// Like [`new`](Self::new), with `group_prefix` as the prefix that
    /// [`seek_group`](Self::seek_group) probes each run with: an index's
    /// group prefix ([`KeyTrie::root_prefix_spec`](super::KeyTrie::root_prefix_spec)).
    pub fn with_group_prefix(
        snapshot: &'a SpineSnapshot<MergedIndexBatch>,
        group_prefix: Option<PrefixSpec<DynData>>,
    ) -> Self {
        let batches: Vec<&'a MergedIndexBatch> =
            snapshot.batches().iter().map(|run| &**run).collect();
        let mut merge = Self {
            runs: batches.iter().map(|run| run.cursor()).collect(),
            parked: vec![false; batches.len()],
            batches,
            group_prefix,
            parked_group: None,
            at_key: Vec::new(),
            cancelled: 0,
            exhausted: false,
        };
        merge.find_smallest_key();
        merge
    }

    /// The number of runs, whether or not they are exhausted.
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// The smallest key over the runs, or `None` when every run is exhausted.
    /// After a [`seek_group`](Self::seek_group) that parked some run, `None`
    /// also once the smallest key leaves the group.
    pub fn key(&self) -> Option<&[u8]> {
        let key = self.at_key.first().map(|&run| key_of(&self.runs[run]))?;
        match &self.parked_group {
            Some(group) if !key.starts_with(group) => None,
            _ => Some(key),
        }
    }

    /// How many runs the last [`seek_group`](Self::seek_group) parked.
    pub fn parked_count(&self) -> usize {
        self.parked.iter().filter(|&&parked| parked).count()
    }

    /// How many values [`take`](Self::take) has found to cancel across runs.
    pub fn cancelled(&self) -> u64 {
        self.cancelled
    }

    /// Moves past the current key in every run at it. Reads no value or weight.
    /// Does nothing when every run is exhausted.
    pub fn step(&mut self) {
        for &run in &self.at_key {
            self.runs[run].step_key();
        }
        self.find_smallest_key();
    }

    /// Moves every run whose key is below `target` to its first key at or past
    /// `target`, one seek per such run. Reads no value or weight. Returns
    /// whether any run moved, so `false` means the position was already at or
    /// past `target`.
    pub fn seek(&mut self, target: &[u8]) -> bool {
        if self.exhausted {
            return false;
        }
        let target = target.to_vec();
        self.unpark();
        let mut moved = false;
        for run in &mut self.runs {
            if run.key_valid() && key_of(run) < target.as_slice() {
                run.seek_key(target.erase());
                moved = true;
            }
        }
        self.find_smallest_key();
        moved
    }

    /// Seeks to `group`, a group prefix, as [`seek`](Self::seek) does, except
    /// that a run behind `group` that cannot hold a key with `group`'s prefix
    /// is parked instead of sought; see the [module docs](self#group-scoped-seeks).
    /// Returns whether any run moved.
    ///
    /// Without a group prefix (see [`with_group_prefix`](Self::with_group_prefix))
    /// this is [`seek`](Self::seek).
    pub fn seek_group(&mut self, group: &[u8]) -> bool {
        let Some(spec) = self.group_prefix else {
            return self.seek(group);
        };
        if self.exhausted {
            return false;
        }
        self.unpark();
        let probe = group.to_vec();
        let mut moved = false;
        for (index, run) in self.runs.iter_mut().enumerate() {
            if !run.key_valid() || key_of(run) >= group {
                continue;
            }
            if self.batches[index].maybe_contains_prefix(probe.erase(), &spec) {
                run.seek_key(probe.erase());
                moved = true;
            } else {
                self.parked[index] = true;
            }
        }
        if self.parked.contains(&true) {
            self.parked_group = Some(probe);
        }
        self.find_smallest_key();
        moved
    }

    /// Returns every parked run to the position.
    fn unpark(&mut self) {
        self.parked.fill(false);
        self.parked_group = None;
    }

    /// Moves past every key, as a seek to a target above all keys would. No
    /// such target can be named for a prefix of `0xFF` bytes, whose range
    /// runs to the end of the index (see
    /// [`prefix_successor`](super::prefix_successor)). Reads nothing.
    pub fn exhaust(&mut self) {
        self.exhausted = true;
        self.at_key.clear();
    }

    /// Visits each value at the current key in ascending order, with its
    /// weight summed over the runs at the key. A value whose weights sum to
    /// zero is skipped and counted in [`cancelled`](Self::cancelled).
    ///
    /// Stops at the first error of `visit` and returns it, leaving the runs
    /// part-way through their values. Consumes the values: after a complete
    /// `take`, a second visits nothing, until the position moves.
    pub fn take<E>(
        &mut self,
        mut visit: impl FnMut(&DynData, ZWeight) -> Result<(), E>,
    ) -> Result<(), E> {
        // The runs holding the value being summed, reused across values.
        let mut holding: Vec<usize> = Vec::with_capacity(self.at_key.len());
        loop {
            let Some(smallest) = self.smallest_value() else {
                return Ok(());
            };
            holding.clear();
            holding.extend(self.at_key.iter().copied().filter(|&run| {
                let cursor = &self.runs[run];
                cursor.val_valid() && cursor.val().cmp(self.runs[smallest].val()) == Ordering::Equal
            }));

            let mut total: ZWeight = 0;
            for &run in &holding {
                let weight = *self.runs[run].weight().downcast_checked::<ZWeight>();
                total = total
                    .checked_add(weight)
                    .expect("merged-index weights overflow when summed across runs");
            }
            if total == 0 {
                self.cancelled += 1;
            } else {
                visit(self.runs[holding[0]].val(), total)?;
            }
            for &run in &holding {
                self.runs[run].step_val();
            }
        }
    }

    /// The run at the current key whose current value is smallest, or `None`
    /// when every run at the key has run out of values.
    fn smallest_value(&self) -> Option<usize> {
        let mut smallest: Option<usize> = None;
        for &run in &self.at_key {
            let cursor = &self.runs[run];
            if !cursor.val_valid() {
                continue;
            }
            let is_smaller = smallest
                .is_none_or(|best| cursor.val().cmp(self.runs[best].val()) == Ordering::Less);
            if is_smaller {
                smallest = Some(run);
            }
        }
        smallest
    }

    /// Finds the runs at the smallest key, by a linear scan over the runs.
    fn find_smallest_key(&mut self) {
        self.at_key.clear();
        if self.exhausted {
            return;
        }
        let mut smallest: Option<&[u8]> = None;
        for (index, run) in self.runs.iter().enumerate() {
            if !run.key_valid() || self.parked[index] {
                continue;
            }
            let key = key_of(run);
            match smallest.map(|best| key.cmp(best)) {
                Some(Ordering::Greater) => {}
                Some(Ordering::Equal) => self.at_key.push(index),
                Some(Ordering::Less) | None => {
                    smallest = Some(key);
                    self.at_key.clear();
                    self.at_key.push(index);
                }
            }
        }
    }
}

/// The folded key under a run's cursor, which must be valid.
fn key_of<'c>(run: &'c RunCursor<'_>) -> &'c [u8] {
    run.key().downcast_checked::<Vec<u8>>()
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use proptest::{
        collection::vec,
        prelude::*,
        test_runner::{Config, TestCaseError, TestRunner},
    };

    use super::*;
    use crate::{
        DBData, DynZWeight,
        dynamic::DynData,
        storage::{attribution::Component, file::FilterSpec},
        trace::{
            Batch, BatchLocation, BatchReaderFactories, Builder, RunReads,
            merged_index::{
                FoldedKey,
                batch::{build_batch, build_batch_in},
            },
            ord::OrdIndexedWSetFactories,
            test::run_in_circuit_with_storage,
            with_read_owner,
        },
    };

    /// One stored row: folded key bytes, payload, and weight.
    type Row = (Vec<u8>, u8, ZWeight);

    /// What `take` reports for one key: payloads with their summed weights.
    type Taken = BTreeMap<Vec<u8>, Vec<(u8, ZWeight)>>;

    /// Builds one in-memory run from rows of payload type `P`.
    pub(crate) fn memory_run<P: DBData>(rows: &[(Vec<u8>, P, ZWeight)]) -> MergedIndexBatch {
        build_batch(
            rows.iter()
                .map(|(key, payload, weight)| (FoldedKey(key.clone()), payload.clone(), *weight)),
        )
        .unwrap()
    }

    /// Copies `run`, whose payloads have type `P`, into a batch in a file.
    /// Needs a runtime with storage.
    pub(crate) fn file_run<P: DBData>(run: &MergedIndexBatch) -> MergedIndexBatch {
        let factories =
            OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, P, ZWeight>();
        let mut builder = <MergedIndexBatch as Batch>::Builder::with_capacity_in_location(
            &factories,
            run.key_count(),
            run.len(),
            Some(BatchLocation::Storage),
        );
        let mut cursor = run.cursor();
        while cursor.key_valid() {
            while cursor.val_valid() {
                let weight = *cursor.weight().downcast_checked::<ZWeight>();
                builder.push_val_diff(cursor.val(), weight.erase());
                cursor.step_val();
            }
            builder.push_key(cursor.key());
            cursor.step_key();
        }
        builder.done()
    }

    /// A snapshot of the runs, each in a file or in memory as listed.
    pub(crate) fn snapshot<P: DBData>(
        runs: &[(Vec<(Vec<u8>, P, ZWeight)>, bool)],
    ) -> SpineSnapshot<MergedIndexBatch> {
        let factories =
            OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, P, ZWeight>();
        let batches = runs
            .iter()
            .map(|(rows, in_file)| {
                let run = memory_run(rows);
                Arc::new(if *in_file { file_run::<P>(&run) } else { run })
            })
            .collect();
        SpineSnapshot::with_batches(&factories, batches)
    }

    /// The summed weight of each `(key, payload)` that some run stores, with
    /// the zero sums kept so that tests can count them.
    fn brute_force_sums(runs: &[(Vec<Row>, bool)]) -> BTreeMap<(Vec<u8>, u8), ZWeight> {
        let mut sums = BTreeMap::new();
        for (rows, _) in runs {
            // A run consolidates its own duplicates first, and stores no zero
            // weight, as `build_batch` does.
            let mut within_run = BTreeMap::new();
            for (key, payload, weight) in rows {
                *within_run.entry((key.clone(), *payload)).or_insert(0) += weight;
            }
            for (entry, weight) in within_run.into_iter().filter(|&(_, weight)| weight != 0) {
                *sums.entry(entry).or_insert(0) += weight;
            }
        }
        sums
    }

    /// Every key of the union of the runs, with `take`'s expected output.
    fn brute_force_taken(runs: &[(Vec<Row>, bool)]) -> Taken {
        let mut taken = Taken::new();
        for ((key, payload), weight) in brute_force_sums(runs) {
            let values = taken.entry(key).or_default();
            if weight != 0 {
                values.push((payload, weight));
            }
        }
        taken
    }

    fn take_all(merge: &mut RunMerge<'_>) -> Vec<(u8, ZWeight)> {
        let mut values = Vec::new();
        merge
            .take(|value, weight| -> Result<(), ()> {
                values.push((*value.downcast_checked::<u8>(), weight));
                Ok(())
            })
            .unwrap();
        values
    }

    /// Random rows over a small key space, so that runs overlap and cancel.
    fn rows() -> impl Strategy<Value = Vec<Row>> {
        let key = vec(0u8..4, 1..3);
        let weight = prop_oneof![-2i64..=-1, 1i64..=2];
        vec((key, 0u8..3, weight), 0..12)
    }

    /// Several runs, each in a file or not.
    fn runs() -> impl Strategy<Value = Vec<(Vec<Row>, bool)>> {
        vec((rows(), any::<bool>()), 0..5)
    }

    /// Runs `check` on random run sets, inside one runtime with storage.
    /// Building a runtime per case would dominate the test.
    fn check_runs(check: fn(&[(Vec<Row>, bool)]) -> Result<(), TestCaseError>) {
        run_in_circuit_with_storage(move || {
            let mut runner = TestRunner::new(Config::with_cases(96));
            runner
                .run(&runs(), |runs| check(&runs))
                .unwrap_or_else(|failure| panic!("{failure}"));
        });
    }

    /// A walk over every key of the union, taking each, matches the brute
    /// force and counts the cancelled values.
    fn check_walk_by_steps(runs: &[(Vec<Row>, bool)]) -> Result<(), TestCaseError> {
        let snapshot = snapshot(runs);
        let mut merge = RunMerge::new(&snapshot);
        let mut taken = Taken::new();
        while let Some(key) = merge.key().map(<[u8]>::to_vec) {
            let values = take_all(&mut merge);
            prop_assert!(taken.insert(key, values).is_none(), "a key came up twice");
            merge.step();
        }
        prop_assert_eq!(&taken, &brute_force_taken(runs));
        let cancelled = brute_force_sums(runs)
            .values()
            .filter(|&&weight| weight == 0)
            .count() as u64;
        prop_assert_eq!(merge.cancelled(), cancelled);
        Ok(())
    }

    #[test]
    fn stepping_through_several_runs_matches_the_consolidated_brute_force() {
        check_runs(check_walk_by_steps);
    }

    /// One random move of the merge, for the model test below.
    #[derive(Clone, Debug)]
    enum Move {
        Step,
        Seek(Vec<u8>),
        TakeThenStep,
    }

    fn moves() -> impl Strategy<Value = Vec<Move>> {
        vec(
            prop_oneof![
                Just(Move::Step),
                vec(0u8..5, 0..3).prop_map(Move::Seek),
                Just(Move::TakeThenStep),
            ],
            0..12,
        )
    }

    /// Random steps, seeks, and takes follow a model that holds the position
    /// as a key of the brute force's union.
    #[test]
    fn seeks_steps_and_takes_follow_a_model_of_the_union() {
        run_in_circuit_with_storage(|| {
            let mut runner = TestRunner::new(Config::with_cases(96));
            runner
                .run(&(runs(), moves()), |(runs, moves)| {
                    let snapshot = snapshot(&runs);
                    let expected = brute_force_taken(&runs);
                    let mut merge = RunMerge::new(&snapshot);
                    // The model's position: the first union key at or past `from`.
                    let first_at_or_after = |from: &[u8]| {
                        expected
                            .range(from.to_vec()..)
                            .next()
                            .map(|(key, _)| key.clone())
                    };
                    let mut position = expected.keys().next().cloned();
                    for step in moves {
                        prop_assert_eq!(merge.key(), position.as_deref());
                        match step {
                            Move::Step => {
                                merge.step();
                                position = position.and_then(|key| {
                                    let mut after = key;
                                    after.push(0);
                                    first_at_or_after(&after)
                                });
                            }
                            Move::Seek(target) => {
                                let moved = merge.seek(&target);
                                let behind = position.as_ref().is_some_and(|key| *key < target);
                                prop_assert_eq!(moved, behind);
                                if behind {
                                    position = first_at_or_after(&target);
                                }
                            }
                            Move::TakeThenStep => {
                                if let Some(key) = position.clone() {
                                    prop_assert_eq!(take_all(&mut merge), expected[&key].clone());
                                    prop_assert!(take_all(&mut merge).is_empty());
                                }
                                merge.step();
                                position = position.and_then(|key| {
                                    let mut after = key;
                                    after.push(0);
                                    first_at_or_after(&after)
                                });
                            }
                        }
                    }
                    prop_assert_eq!(merge.key(), position.as_deref());
                    Ok(())
                })
                .unwrap_or_else(|failure| panic!("{failure}"));
        });
    }

    fn row(key: &[u8], payload: u8, weight: ZWeight) -> Row {
        (key.to_vec(), payload, weight)
    }

    /// The reads that `f` makes on this thread.
    pub(crate) fn reads_of<T>(f: impl FnOnce() -> T) -> (T, RunReads) {
        let before = RunReads::current();
        let result = f();
        (result, RunReads::current().since(before))
    }

    #[test]
    fn a_cancelled_value_is_counted_and_not_visited() {
        // The memory run deletes payload 2 of the file run and inserts payload
        // 4 in its place, so payload 2 cancels across the runs.
        run_in_circuit_with_storage(|| {
            let runs = vec![
                (
                    vec![row(b"k", 1, 1), row(b"k", 2, 1), row(b"k", 3, 1)],
                    true,
                ),
                (vec![row(b"k", 2, -1), row(b"k", 4, 7)], false),
            ];
            let snapshot = snapshot(&runs);
            let mut merge = RunMerge::new(&snapshot);
            assert_eq!(merge.run_count(), 2);
            assert_eq!(take_all(&mut merge), vec![(1, 1), (3, 1), (4, 7)]);
            assert_eq!(merge.cancelled(), 1);
            merge.step();
            assert_eq!(merge.key(), None);
        });
    }

    #[test]
    fn a_key_whose_values_all_cancel_is_still_a_position() {
        let runs = vec![
            (vec![row(b"a", 1, 1), row(b"b", 1, 1)], false),
            (vec![row(b"a", 1, -1)], false),
        ];
        let snapshot = snapshot(&runs);
        let mut merge = RunMerge::new(&snapshot);
        assert_eq!(merge.key(), Some(&b"a"[..]));
        assert!(take_all(&mut merge).is_empty());
        merge.step();
        assert_eq!(merge.key(), Some(&b"b"[..]));
    }

    #[test]
    fn an_empty_snapshot_has_no_key() {
        let snapshot = snapshot::<u8>(&[]);
        let mut merge = RunMerge::new(&snapshot);
        assert_eq!(merge.key(), None);
        merge.step();
        assert!(!merge.seek(b"a"));
        assert!(take_all(&mut merge).is_empty());
    }

    #[test]
    fn exhaust_moves_past_every_key_and_reads_nothing() {
        run_in_circuit_with_storage(|| {
            let runs = vec![(vec![row(b"a", 1, 1), row(b"b", 1, 1)], true)];
            let snapshot = snapshot(&runs);
            let mut merge = RunMerge::new(&snapshot);
            let (_, exhausted) = reads_of(|| merge.exhaust());
            assert_eq!(exhausted, RunReads::default());
            assert_eq!(merge.key(), None);
            // Nothing moves an exhausted merge.
            merge.step();
            assert!(!merge.seek(b"a"));
            assert_eq!(merge.key(), None);
            assert!(take_all(&mut merge).is_empty());
        });
    }

    #[test]
    fn take_stops_at_the_first_error() {
        let runs = vec![(vec![row(b"a", 1, 1), row(b"a", 2, 1)], false)];
        let snapshot = snapshot(&runs);
        let mut merge = RunMerge::new(&snapshot);
        let mut visited = 0;
        let result = merge.take(|_, _| {
            visited += 1;
            Err("stop")
        });
        assert_eq!(result, Err("stop"));
        assert_eq!(visited, 1);
    }

    #[test]
    fn seek_moves_only_the_runs_below_the_target() {
        // Run 0 starts at b, run 1 at d. A seek to c moves run 0 only.
        run_in_circuit_with_storage(|| {
            let runs = vec![
                (vec![row(b"b", 1, 1), row(b"e", 1, 1)], true),
                (vec![row(b"d", 1, 1)], true),
            ];
            let snapshot = snapshot(&runs);
            let mut merge = RunMerge::new(&snapshot);
            let (moved, sought) = reads_of(|| merge.seek(b"c"));
            assert!(moved);
            assert_eq!(merge.key(), Some(&b"d"[..]));
            assert_eq!(sought.seeks, 1);
            // Run 0 lands on e. Run 1 is on d already, so it is not sought.
            assert_eq!(sought.key_decodes, 1);

            // Seeking to d or earlier moves nothing.
            let (moved, sought) = reads_of(|| merge.seek(b"d"));
            assert!(!moved);
            assert_eq!(sought, RunReads::default());
        });
    }

    #[test]
    fn stepping_and_seeking_past_entries_decode_no_value_or_weight() {
        run_in_circuit_with_storage(|| {
            // Two file runs of eight keys each, interleaved, one value each.
            let evens: Vec<Row> = (0..8).map(|i| row(&[2 * i], 1, 1)).collect();
            let odds: Vec<Row> = (0..8).map(|i| row(&[2 * i + 1], 1, 1)).collect();
            let snapshot = snapshot(&[(evens, true), (odds, true)]);

            let (positions, stepped) = reads_of(|| {
                let mut merge = RunMerge::new(&snapshot);
                let mut positions = 0;
                while merge.key().is_some() {
                    positions += 1;
                    merge.step();
                }
                positions
            });
            assert_eq!(positions, 16);
            // One key decode per landing, and the two openings; none of the
            // other columns.
            assert_eq!(stepped.key_decodes, 16);
            assert_eq!(stepped.value_decodes, 0);
            assert_eq!(stepped.weight_decodes, 0);

            let (_, sought) = reads_of(|| {
                let mut merge = RunMerge::new(&snapshot);
                merge.seek(&[9]);
                merge.seek(&[200]);
            });
            assert_eq!(sought.value_decodes, 0);
            assert_eq!(sought.weight_decodes, 0);
        });
    }

    #[test]
    fn take_decodes_each_value_and_weight_once() {
        run_in_circuit_with_storage(|| {
            // Key a has two values in one file run, and key b one value in each
            // of two, where the second run's cancels.
            let runs = vec![
                (
                    vec![row(b"a", 1, 1), row(b"a", 2, 1), row(b"b", 1, 1)],
                    true,
                ),
                (vec![row(b"b", 1, -1)], true),
            ];
            let snapshot = snapshot(&runs);
            let mut merge = RunMerge::new(&snapshot);
            let (values, first) = reads_of(|| take_all(&mut merge));
            assert_eq!(values, vec![(1, 1), (2, 1)]);
            // Only the first run is at a: two values and two weights.
            assert_eq!(first.value_decodes, 2);
            assert_eq!(first.weight_decodes, 2);

            merge.step();
            let (values, second) = reads_of(|| take_all(&mut merge));
            assert!(values.is_empty());
            // Both runs are at b: one cancelled value, two reads of each.
            assert_eq!(second.value_decodes, 2);
            assert_eq!(second.weight_decodes, 2);
            assert_eq!(merge.cancelled(), 1);
        });
    }

    #[test]
    fn memory_runs_read_keys_while_stepping_and_each_value_and_weight_when_taken() {
        // Two memory runs: key a has two values in the first; key b has one
        // value in each, and the second run's cancels.
        let runs = vec![
            (
                vec![row(b"a", 1, 1), row(b"a", 2, 1), row(b"b", 1, 1)],
                false,
            ),
            (vec![row(b"b", 1, -1)], false),
        ];
        let snapshot = snapshot(&runs);
        let (mut merge, opened) = reads_of(|| RunMerge::new(&snapshot));
        // Each run reads its first key at the opening.
        assert_eq!(opened.memory_key_reads, 2);

        let (values, first) = reads_of(|| take_all(&mut merge));
        assert_eq!(values, vec![(1, 1), (2, 1)]);
        // Only the first run is at a: two values and two weights.
        assert_eq!(first.memory_value_reads, 2);
        assert_eq!(first.memory_weight_reads, 2);
        assert_eq!(first.memory_key_reads, 0);

        // The step lands the first run on b: one key read. The second run is
        // already at b.
        let (_, stepped) = reads_of(|| merge.step());
        assert_eq!(stepped.memory_key_reads, 1);
        assert_eq!(stepped.memory_value_reads, 0);
        assert_eq!(stepped.memory_weight_reads, 0);

        // Both runs are at b: one cancelled value, one read of each per run.
        let (values, second) = reads_of(|| take_all(&mut merge));
        assert!(values.is_empty());
        assert_eq!(second.memory_value_reads, 2);
        assert_eq!(second.memory_weight_reads, 2);
        assert_eq!(merge.cancelled(), 1);
    }

    #[test]
    fn reads_count_for_the_current_read_owner() {
        run_in_circuit_with_storage(|| {
            let snapshot = snapshot(&[(vec![row(b"a", 1, 1)], true)]);
            let before = RunReads::current_for(Component::MergedIndex);
            with_read_owner(Component::MergedIndex, || {
                let mut merge = RunMerge::new(&snapshot);
                take_all(&mut merge);
            });
            let reads = RunReads::current_for(Component::MergedIndex).since(before);
            assert_eq!((reads.key_decodes, reads.value_decodes), (1, 1));
        });
    }

    /// The factories of runs whose files get a prefix filter over the first
    /// `N` key bytes and no whole-key filter.
    fn prefix_factories<const N: usize>() -> OrdIndexedWSetFactories<DynData, DynData, DynZWeight> {
        OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<Vec<u8>, u8, ZWeight>()
            .with_filter_spec(FilterSpec::prefix_only(PrefixSpec::bytes::<N>()))
    }

    /// A run in a file with a prefix filter over the first `N` key bytes.
    fn prefix_file_run<const N: usize>(rows: &[Row]) -> MergedIndexBatch {
        let rows: Vec<_> = rows
            .iter()
            .map(|(key, payload, weight)| (FoldedKey(key.clone()), *payload, *weight))
            .collect();
        let run = build_batch_in(&prefix_factories::<N>(), rows.clone()).unwrap();
        if run.location() == BatchLocation::Storage || run.is_empty() {
            return run;
        }
        // Below the storage threshold the builder keeps the run in memory;
        // copy it into a file, as `file_run` does, with the same factories.
        let mut builder = <MergedIndexBatch as Batch>::Builder::with_capacity_in_location(
            &prefix_factories::<N>(),
            run.key_count(),
            run.len(),
            Some(BatchLocation::Storage),
        );
        let mut cursor = run.cursor();
        while cursor.key_valid() {
            while cursor.val_valid() {
                let weight = *cursor.weight().downcast_checked::<ZWeight>();
                builder.push_val_diff(cursor.val(), weight.erase());
                cursor.step_val();
            }
            builder.push_key(cursor.key());
            cursor.step_key();
        }
        builder.done()
    }

    /// Rows whose keys are a group byte (0 to 7) and one more byte, so that a
    /// one-byte prefix names the group.
    fn grouped_rows() -> impl Strategy<Value = Vec<Row>> {
        let key = (0u8..8, 0u8..4).prop_map(|(group, rest)| vec![group, rest]);
        let weight = prop_oneof![-2i64..=-1, 1i64..=2];
        vec((key, 0u8..3, weight), 0..12)
    }

    /// Group-scoped seeks over file runs with prefix filters, and over memory
    /// runs, visit exactly the keys of each requested group, with the same
    /// sums as the brute force; groups that a run lacks park it.
    #[test]
    fn group_seeks_with_prefix_filters_match_the_brute_force() {
        run_in_circuit_with_storage(|| {
            let strategy = (
                vec((grouped_rows(), any::<bool>()), 0..6),
                proptest::collection::btree_set(0u8..8, 0..8),
            );
            let mut runner = TestRunner::new(Config::with_cases(96));
            runner
                .run(&strategy, |(runs, groups)| {
                    let batches: Vec<_> = runs
                        .iter()
                        .map(|(rows, in_file)| {
                            Arc::new(if *in_file {
                                prefix_file_run::<1>(rows)
                            } else {
                                memory_run(rows)
                            })
                        })
                        .collect();
                    let snapshot = SpineSnapshot::with_batches(&prefix_factories::<1>(), batches);
                    let mut merge =
                        RunMerge::with_group_prefix(&snapshot, Some(PrefixSpec::bytes::<1>()));
                    let expected = brute_force_taken(&runs);
                    for group in groups {
                        merge.seek_group(&[group]);
                        let mut taken = Taken::new();
                        while let Some(key) = merge.key().map(<[u8]>::to_vec) {
                            if key[0] != group {
                                break;
                            }
                            taken.insert(key, take_all(&mut merge));
                            merge.step();
                        }
                        let wanted: Taken = expected
                            .iter()
                            .filter(|(key, _)| key[0] == group)
                            .map(|(key, values)| (key.clone(), values.clone()))
                            .collect();
                        prop_assert_eq!(taken, wanted, "group {}", group);
                    }
                    Ok(())
                })
                .unwrap_or_else(|failure| panic!("{failure}"));
        });
    }

    /// A run without the group is not sought: it is parked, and the probe that
    /// skipped it is counted.
    #[test]
    fn a_run_without_the_group_is_parked_and_counted() {
        run_in_circuit_with_storage(|| {
            let present = prefix_file_run::<1>(&[(vec![1, 0], 1, 1), (vec![3, 0], 1, 1)]);
            let absent = prefix_file_run::<1>(&[(vec![0, 0], 1, 1), (vec![4, 0], 1, 1)]);
            let snapshot = SpineSnapshot::with_batches(
                &prefix_factories::<1>(),
                vec![Arc::new(present), Arc::new(absent)],
            );
            let mut merge = RunMerge::with_group_prefix(&snapshot, Some(PrefixSpec::bytes::<1>()));
            let before = RunReads::current();
            assert!(merge.seek_group(&[3]));
            let reads = RunReads::current().since(before);
            assert_eq!(merge.parked_count(), 1);
            assert_eq!(reads.filtered_prefix_probes, 1);
            assert_eq!(reads.seeks, 1);
            assert_eq!(merge.key(), Some(&[3u8, 0][..]));
            merge.step();
            // The parked run's key [4, 0] lies beyond the group: the position
            // ends with the group.
            assert_eq!(merge.key(), None);
            // The next seek unparks it.
            merge.seek(&[4]);
            assert_eq!(merge.parked_count(), 0);
            assert_eq!(merge.key(), Some(&[4u8, 0][..]));
        });
    }

    /// A run of more prefixes than fit inline stores a partitioned filter,
    /// which has no false negatives, survives a reopen from its path, and
    /// refuses factories that ask for another prefix.
    #[test]
    fn a_partitioned_prefix_filter_reloads_and_refuses_another_prefix() {
        run_in_circuit_with_storage(|| {
            let groups = 3 * crate::storage::file::INLINE_PREFIXES as u16;
            let rows: Vec<Row> = (0..groups)
                .flat_map(|group| {
                    let [high, low] = (group * 2).to_be_bytes();
                    [(vec![high, low, 0], 1, 1), (vec![high, low, 1], 2, 1)]
                })
                .collect();
            let run = prefix_file_run::<2>(&rows);
            assert_eq!(run.location(), BatchLocation::Storage);
            let path = run.file_reader().unwrap().path().clone();
            let reopened =
                <MergedIndexBatch as Batch>::from_path(&prefix_factories::<2>(), &path).unwrap();
            let spec = PrefixSpec::bytes::<2>();
            let mut rejected = 0;
            for group in 0..2 * groups {
                let probe = group.to_be_bytes().to_vec();
                let stored = group % 2 == 0;
                for batch in [&run, &reopened] {
                    let maybe = batch.maybe_contains_prefix(probe.erase(), &spec);
                    assert!(maybe || !stored, "false negative for group {group}");
                    rejected += usize::from(!maybe);
                }
            }
            // Every absent group lies inside the key range, so only the filter
            // can reject it: most absent groups, at the default rate.
            assert!(
                rejected > 2 * groups as usize * 9 / 10,
                "rejected {rejected}"
            );
            assert!(
                <MergedIndexBatch as Batch>::from_path(&prefix_factories::<3>(), &path).is_err()
            );
        });
    }
}
