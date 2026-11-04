//! Counts of what the cursors over single runs read.
//!
//! A *run* is one immutable sorted batch, in memory or in a file. The cursors
//! over a single run count four things in per-thread running totals:
//!
//! * `seeks`: key seeks that reach the run (`seek_key`, `seek_key_with`, and
//!   their reverse forms), in both in-memory and file runs. A
//!   `seek_key_exact` that a file run's filter rejects reaches no run and
//!   counts no seek. Seeks over a [`CursorList`](super::CursorList) count once
//!   per run.
//! * `key_reads`: keys a file run deserializes, one for each landing on a new
//!   key row and one for each key a predicate seek probes.
//! * `value_reads`: values a file run deserializes, one for each landing on a
//!   new value row and one for each value a predicate seek probes. Landing on
//!   a key also lands on its first value.
//! * `weight_reads`: weights a file run deserializes, one for each weight
//!   that `weight`, `map_times`, or `map_values` returns.
//!
//! In-memory runs deserialize nothing, so they count only seeks.
//!
//! A reverse key seek counts one key read for the row it lands on, although
//! the file reader may deserialize that key twice. Nothing that reads merged
//! indexes seeks backward.
//!
//! The totals are kept per thread and never reset. To count a region, take
//! [`RunReads::current`] before and after it on the same thread and subtract
//! with [`RunReads::since`]. Reads that other threads perform, such as
//! background compaction, are not in this thread's totals.
//!
//! # Attribution
//!
//! Each read is also counted for the thread's current *read owner*, the
//! component on whose behalf the run is read. The owners are the components of
//! [`WriteOwner`]; the default is [`WriteOwner::Query`], and
//! [`with_read_owner`] changes it for the duration of a closure.
//! [`RunReads::current_for`] gives one owner's totals, and the owners add up
//! to [`RunReads::current`].
//!
//! The read owner is separate from the write owner, so a read scope never
//! changes who owns the files or spines created inside it. A tokio task does
//! not inherit it.

use std::cell::Cell;
use std::thread::LocalKey;

use serde::Serialize;

use crate::storage::write_attribution::WriteOwner;

/// The reads of single runs on one thread; see the [module docs](self).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RunReads {
    pub seeks: u64,
    pub key_reads: u64,
    pub value_reads: u64,
    pub weight_reads: u64,
}

/// One running total per owner, indexed like [`WriteOwner::ALL`].
type PerOwner = [Cell<u64>; WriteOwner::ALL.len()];

thread_local! {
    static CURRENT_READ_OWNER: Cell<WriteOwner> = const { Cell::new(WriteOwner::Query) };
    static SEEKS: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static KEY_READS: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static VALUE_READS: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static WEIGHT_READS: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
}

impl RunReads {
    /// This thread's running totals, over all owners.
    pub fn current() -> Self {
        WriteOwner::ALL
            .into_iter()
            .map(Self::current_for)
            .fold(Self::default(), Self::plus)
    }

    /// This thread's running totals for the reads made on behalf of `owner`.
    pub fn current_for(owner: WriteOwner) -> Self {
        let total = |counter: &'static LocalKey<PerOwner>| {
            counter.with(|counts| counts[owner.index()].get())
        };
        Self {
            seeks: total(&SEEKS),
            key_reads: total(&KEY_READS),
            value_reads: total(&VALUE_READS),
            weight_reads: total(&WEIGHT_READS),
        }
    }

    /// The reads between `earlier` and `self`, both taken on this thread with
    /// [`RunReads::current`] (or both with [`RunReads::current_for`] and the
    /// same owner), `earlier` first.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            seeks: self.seeks - earlier.seeks,
            key_reads: self.key_reads - earlier.key_reads,
            value_reads: self.value_reads - earlier.value_reads,
            weight_reads: self.weight_reads - earlier.weight_reads,
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            seeks: self.seeks + other.seeks,
            key_reads: self.key_reads + other.key_reads,
            value_reads: self.value_reads + other.value_reads,
            weight_reads: self.weight_reads + other.weight_reads,
        }
    }
}

/// The owner that this thread's run reads are now counted for.
pub fn current_read_owner() -> WriteOwner {
    CURRENT_READ_OWNER.get()
}

/// Runs `f` with `owner` as this thread's read owner, then restores the
/// previous owner, even if `f` panics.
pub fn with_read_owner<R>(owner: WriteOwner, f: impl FnOnce() -> R) -> R {
    struct Restore(WriteOwner);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_READ_OWNER.set(self.0);
        }
    }
    let _restore = Restore(CURRENT_READ_OWNER.replace(owner));
    f()
}

fn increment(counter: &'static LocalKey<PerOwner>) {
    let owner = CURRENT_READ_OWNER.get();
    counter.with(|counts| {
        let count = &counts[owner.index()];
        count.set(count.get() + 1);
    });
}

pub(crate) fn count_seek() {
    increment(&SEEKS);
}

pub(crate) fn count_key_read() {
    increment(&KEY_READS);
}

pub(crate) fn count_value_read() {
    increment(&VALUE_READS);
}

pub(crate) fn count_weight_read() {
    increment(&WEIGHT_READS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_each_count() {
        let earlier = RunReads::current();
        count_seek();
        count_key_read();
        count_key_read();
        count_value_read();
        count_weight_read();
        count_weight_read();
        count_weight_read();
        assert_eq!(
            RunReads::current().since(earlier),
            RunReads {
                seeks: 1,
                key_reads: 2,
                value_reads: 1,
                weight_reads: 3,
            }
        );
    }

    #[test]
    fn counts_stay_on_their_thread() {
        let earlier = RunReads::current();
        std::thread::spawn(count_seek).join().unwrap();
        assert_eq!(RunReads::current().since(earlier), RunReads::default());
    }

    const ONE_SEEK: RunReads = RunReads {
        seeks: 1,
        key_reads: 0,
        value_reads: 0,
        weight_reads: 0,
    };

    /// The reads that `f` makes, in total and for each owner.
    fn reads_by_owner(f: impl FnOnce()) -> (RunReads, [RunReads; 3]) {
        let earlier_total = RunReads::current();
        let earlier = WriteOwner::ALL.map(RunReads::current_for);
        f();
        let later = WriteOwner::ALL.map(RunReads::current_for);
        let by_owner = std::array::from_fn(|i| later[i].since(earlier[i]));
        (RunReads::current().since(earlier_total), by_owner)
    }

    #[test]
    fn reads_count_for_the_query_by_default() {
        let (total, [query, base, index]) = reads_by_owner(count_seek);
        assert_eq!(total, ONE_SEEK);
        assert_eq!(query, ONE_SEEK);
        assert_eq!(base, RunReads::default());
        assert_eq!(index, RunReads::default());
    }

    #[test]
    fn a_read_scope_attributes_its_reads() {
        let (total, [query, _, index]) = reads_by_owner(|| {
            with_read_owner(WriteOwner::MergedIndex, count_seek);
            count_seek();
        });
        assert_eq!(total.seeks, 2);
        assert_eq!(index, ONE_SEEK);
        assert_eq!(query, ONE_SEEK);
    }

    #[test]
    fn nested_scopes_restore_the_outer_owner() {
        let (_, [query, base, index]) = reads_by_owner(|| {
            with_read_owner(WriteOwner::MergedIndex, || {
                with_read_owner(WriteOwner::BaseRelation, count_seek);
                assert_eq!(current_read_owner(), WriteOwner::MergedIndex);
                count_seek();
            });
        });
        assert_eq!(current_read_owner(), WriteOwner::Query);
        assert_eq!(base, ONE_SEEK);
        assert_eq!(index, ONE_SEEK);
        assert_eq!(query, RunReads::default());
    }

    #[test]
    fn a_panic_restores_the_owner() {
        let (_, [query, _, index]) = reads_by_owner(|| {
            let result = std::panic::catch_unwind(|| {
                with_read_owner(WriteOwner::MergedIndex, || panic!("inside the scope"))
            });
            assert!(result.is_err());
            count_seek();
        });
        assert_eq!(query, ONE_SEEK);
        assert_eq!(index, RunReads::default());
    }
}
