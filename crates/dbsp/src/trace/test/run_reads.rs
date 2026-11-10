//! The read counts of single-run cursors; see [`RunReads`].
//!
//! Every run here holds the keys 1, 2, and 3. Key 1 has the values 10 and 11,
//! key 2 has 20, and key 3 has 30.

use std::cell::Cell;

use super::{DynI32, indexed_zset_tuples, run_in_circuit_with_storage};
use crate::{
    DynZWeight, ZWeight,
    dynamic::{DowncastTrait, Erase},
    storage::write_attribution::WriteOwner,
    trace::{
        Batch, BatchLocation, BatchReader, BatchReaderFactories, FileIndexedWSetFactories,
        RunReads, VecIndexedWSet, VecIndexedWSetFactories,
        cursor::{Cursor, CursorList},
        ord::FileIndexedWSet,
        with_read_owner,
    },
    utils::Tup2,
};

type FileRun = FileIndexedWSet<DynI32, DynI32, DynZWeight>;
type VecRun = VecIndexedWSet<DynI32, DynI32, DynZWeight>;

fn tuples() -> Vec<Tup2<Tup2<i32, i32>, ZWeight>> {
    vec![
        Tup2(Tup2(1, 10), 1),
        Tup2(Tup2(1, 11), 1),
        Tup2(Tup2(2, 20), 1),
        Tup2(Tup2(3, 30), 1),
    ]
}

fn file_run() -> FileRun {
    file_run_of(tuples())
}

fn file_run_of(tuples: Vec<Tup2<Tup2<i32, i32>, ZWeight>>) -> FileRun {
    let factories =
        <FileIndexedWSetFactories<DynI32, DynI32, DynZWeight>>::new::<i32, i32, ZWeight>();
    FileRun::dyn_from_tuples(&factories, (), &mut indexed_zset_tuples(tuples))
}

fn vec_run() -> VecRun {
    let factories =
        <VecIndexedWSetFactories<DynI32, DynI32, DynZWeight>>::new::<i32, i32, ZWeight>();
    VecRun::dyn_from_tuples(&factories, (), &mut indexed_zset_tuples(tuples()))
}

/// The reads that `f` makes on this thread.
fn reads_of<T>(f: impl FnOnce() -> T) -> (T, RunReads) {
    let before = RunReads::current();
    let result = f();
    (result, RunReads::current().since(before))
}

/// Reads with no filter rejection.
fn reads(seeks: u64, key_decodes: u64, value_decodes: u64, weight_decodes: u64) -> RunReads {
    RunReads {
        seeks,
        filter_rejections: 0,
        key_decodes,
        value_decodes,
        weight_decodes,
    }
}

fn key_of<C: Cursor<DynI32, DynI32, (), DynZWeight>>(cursor: &C) -> i32 {
    *cursor.key().downcast_checked::<i32>()
}

#[test]
fn file_run_counts_each_deserialization() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        assert_eq!(run.location(), BatchLocation::Storage);

        // Opening lands on key 1 and reads no value.
        let (mut cursor, opened) = reads_of(|| run.cursor());
        assert_eq!(opened, reads(0, 1, 0, 0));

        // The first value move positions on the key's first value (10) and
        // then reads the next (11); a weight is read only on request.
        let ((), stepped) = reads_of(|| cursor.step_val());
        assert_eq!(stepped, reads(0, 0, 2, 0));
        let ((), weighed) = reads_of(|| {
            cursor.weight();
        });
        assert_eq!(weighed, reads(0, 0, 0, 1));

        // A key step reads the new key and no value.
        let ((), stepped) = reads_of(|| cursor.step_key());
        assert_eq!(key_of(&cursor), 2);
        assert_eq!(stepped, reads(0, 1, 0, 0));

        // A seek to the current key reads no key and no value.
        let ((), stayed) = reads_of(|| cursor.seek_key(2_i32.erase()));
        assert_eq!(stayed, reads(1, 0, 0, 0));

        let ((), moved) = reads_of(|| cursor.seek_key(3_i32.erase()));
        assert_eq!(key_of(&cursor), 3);
        assert_eq!(moved, reads(1, 1, 0, 0));

        // Stepping past the last key lands on nothing.
        let ((), ended) = reads_of(|| cursor.step_key());
        assert!(!cursor.key_valid());
        assert_eq!(ended, reads(0, 0, 0, 0));
    });
}

#[test]
fn file_run_filter_rejection_counts_no_seek() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let mut cursor = run.cursor();
        let (found, rejected) = reads_of(|| cursor.seek_key_exact(100_i32.erase(), None));
        assert!(!found);
        // The filter answers without a seek or a decode.
        assert_eq!(
            rejected,
            RunReads {
                filter_rejections: 1,
                ..RunReads::default()
            }
        );

        // A key the filter admits is sought, and counts no rejection.
        let (found, admitted) = reads_of(|| cursor.seek_key_exact(3_i32.erase(), None));
        assert!(found);
        assert_eq!(admitted.seeks, 1);
        assert_eq!(admitted.filter_rejections, 0);
    });
}

#[test]
fn file_run_predicate_seek_counts_each_probe() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let mut cursor = run.cursor();
        let probes = Cell::new(0_u64);
        let predicate = |key: &DynI32| {
            probes.set(probes.get() + 1);
            *key.downcast_checked::<i32>() >= 3
        };
        let ((), sought) = reads_of(|| cursor.seek_key_with(&predicate));
        assert_eq!(key_of(&cursor), 3);
        assert!(probes.get() > 0);
        // Every probe, then the landed key; no value.
        assert_eq!(sought, reads(1, probes.get() + 1, 0, 0));
    });
}

#[test]
fn file_run_map_values_reads_each_weight() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let mut cursor = run.cursor();
        let mut weights = Vec::new();
        let ((), mapped) = reads_of(|| {
            cursor.map_values(&mut |_, weight| weights.push(**weight));
        });
        assert_eq!(weights, vec![1, 1]);
        // Two weights; the first value, one step onto value 11, then one step
        // past the end.
        assert_eq!(mapped, reads(0, 0, 2, 2));
    });
}

#[test]
fn a_landing_reads_no_value() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let ((), landed) = reads_of(|| {
            let mut cursor = run.cursor();
            cursor.step_key();
            cursor.seek_key(3_i32.erase());
            cursor.rewind_keys();
            cursor.fast_forward_keys();
            cursor.step_key_reverse();
            cursor.seek_key_reverse(1_i32.erase());
            assert!(cursor.key_valid());
        });
        // Only the keys: the opening, the step, the seek, the rewind, the
        // fast-forward, the reverse step, and the reverse seek.
        assert_eq!(landed, reads(2, 7, 0, 0));
    });
}

#[test]
fn the_first_value_read_counts_once() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let mut cursor = run.cursor();
        let (valid, first) = reads_of(|| cursor.val_valid());
        assert!(valid);
        assert_eq!(first, reads(0, 0, 1, 0));

        // The cell is filled: neither `val` nor `val_valid` reads again.
        let (value, again) = reads_of(|| {
            assert!(cursor.val_valid());
            *cursor.val().downcast_checked::<i32>()
        });
        assert_eq!(value, 10);
        assert_eq!(again, reads(0, 0, 0, 0));

        // The next key's value is read once more, not before.
        let ((), moved) = reads_of(|| cursor.step_key());
        assert_eq!(moved, reads(0, 1, 0, 0));
        let (value, second) = reads_of(|| *cursor.val().downcast_checked::<i32>());
        assert_eq!(value, 20);
        assert_eq!(second, reads(0, 0, 1, 0));
    });
}

#[test]
fn value_moves_work_on_a_lazy_cursor() {
    run_in_circuit_with_storage(|| {
        let run = file_run();

        // `step_val` before any `val`.
        let mut cursor = run.cursor();
        cursor.step_val();
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 11);
        cursor.step_val();
        assert!(!cursor.val_valid());

        // `seek_val` before any `val`.
        let mut cursor = run.cursor();
        cursor.seek_val(11_i32.erase());
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 11);

        // `weight` before any `val`, on a key reached by a seek.
        let mut cursor = run.cursor();
        cursor.seek_key(3_i32.erase());
        let (weight, weighed) = reads_of(|| **cursor.weight());
        assert_eq!(weight, 1);
        assert_eq!(weighed, reads(0, 0, 1, 1));
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 30);

        // `rewind_vals` and `fast_forward_vals` on an unfilled cursor.
        let mut cursor = run.cursor();
        cursor.fast_forward_vals();
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 11);
        cursor.rewind_vals();
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 10);

        // Values of every key, through the hint of the one before.
        let mut cursor = run.cursor();
        let mut seen = Vec::new();
        while cursor.key_valid() {
            seen.push((key_of(&cursor), *cursor.val().downcast_checked::<i32>()));
            cursor.step_key();
        }
        assert_eq!(seen, vec![(1, 10), (2, 20), (3, 30)]);
    });
}

#[test]
fn a_clone_works_before_and_after_its_value_is_filled() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let mut cursor = run.cursor();
        let before = cursor.clone();

        // The original fills; the clone made before is still empty.
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 10);
        let (value, filled) = reads_of(|| *before.val().downcast_checked::<i32>());
        assert_eq!(value, 10);
        assert_eq!(filled, reads(0, 0, 1, 0));

        // A clone made after the fill carries the filled value and reads
        // nothing.
        let mut after = cursor.clone();
        let (value, copied) = reads_of(|| *after.val().downcast_checked::<i32>());
        assert_eq!(value, 10);
        assert_eq!(copied, reads(0, 0, 0, 0));

        // The three move independently.
        cursor.step_key();
        after.step_val();
        assert_eq!(*cursor.val().downcast_checked::<i32>(), 20);
        assert_eq!(*after.val().downcast_checked::<i32>(), 11);
        assert_eq!(*before.val().downcast_checked::<i32>(), 10);
    });
}

#[test]
fn vec_run_counts_seeks_only() {
    let run = vec_run();
    let ((), all) = reads_of(|| {
        let mut cursor = run.cursor();
        cursor.step_val();
        cursor.weight();
        cursor.step_key();
        cursor.seek_key(3_i32.erase());
        cursor.seek_key_with(&|key| *key.downcast_checked::<i32>() >= 3);
        assert!(cursor.seek_key_exact(3_i32.erase(), None));
        cursor.map_values(&mut |_, _| ());
    });
    assert_eq!(all, reads(3, 0, 0, 0));
}

#[test]
fn cursor_list_seeks_every_run() {
    let first = vec_run();
    let second = vec_run();
    let mut cursor = CursorList::new(
        first.factories().weight_factory(),
        vec![first.cursor(), second.cursor()],
    );
    let ((), sought) = reads_of(|| cursor.seek_key(2_i32.erase()));
    assert_eq!(key_of(&cursor), 2);
    assert_eq!(sought, reads(2, 0, 0, 0));
}

#[test]
fn cursor_list_reads_values_only_of_the_runs_at_its_smallest_key() {
    run_in_circuit_with_storage(|| {
        // `first` holds keys 1, 2, and 3; `second` holds key 2 alone, with
        // the same value as `first` has there.
        let first = file_run();
        let second = file_run_of(vec![Tup2(Tup2(2, 20), 1)]);
        let (mut cursor, opened) = reads_of(|| {
            CursorList::new(
                first.factories().weight_factory(),
                vec![first.cursor(), second.cursor()],
            )
        });
        assert_eq!(key_of(&cursor), 1);
        // Each run decodes its first key. Only `first` is at the smallest key,
        // so only it decodes a value, and only its weight is read to skip
        // zero weights.
        assert_eq!(opened, reads(0, 2, 1, 1));

        // Both runs are at key 2: each decodes a value and a weight.
        let ((), stepped) = reads_of(|| cursor.step_key());
        assert_eq!(key_of(&cursor), 2);
        assert_eq!(stepped, reads(0, 1, 2, 2));
    });
}

#[test]
fn a_read_scope_attributes_a_file_run_to_its_owner() {
    run_in_circuit_with_storage(|| {
        let run = file_run();
        let index_before = RunReads::current_for(WriteOwner::MergedIndex);
        let query_before = RunReads::current_for(WriteOwner::Query);
        with_read_owner(WriteOwner::MergedIndex, || {
            let mut cursor = run.cursor();
            cursor.seek_key(3_i32.erase());
        });
        // The opening reads key 1; the seek reads key 3. Neither reads a value.
        assert_eq!(
            RunReads::current_for(WriteOwner::MergedIndex).since(index_before),
            reads(1, 2, 0, 0)
        );
        assert_eq!(
            RunReads::current_for(WriteOwner::Query).since(query_before),
            RunReads::default()
        );
    });
}
