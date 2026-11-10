//! Counts of what the cursors over single runs read.
//!
//! A *run* is one immutable sorted batch, in memory or in a file. The cursors
//! over a single run count five things in per-thread running totals:
//!
//! * `seeks`: key seeks that reach the run (`seek_key`, `seek_key_with`, and
//!   their reverse forms), in both in-memory and file runs. A
//!   `seek_key_exact` that a file run's filter rejects reaches no run and
//!   counts no seek. Seeks over a [`CursorList`](super::CursorList) count once
//!   per run.
//! * `filter_rejections`: `seek_key_exact` calls that a file run's filter
//!   answers without seeking. They cost no decode, but they are probes.
//! * `key_decodes`: keys a file run deserializes, one for each landing on a new
//!   key row and one for each key a predicate seek probes.
//! * `value_decodes`: values a file run deserializes, one for each landing on a
//!   new value row and one for each value a predicate seek probes. Landing on
//!   a key reads no value. The first read of a value, weight, or value move
//!   after a landing positions on the key's first value, and counts it once.
//! * `weight_decodes`: weights a file run deserializes, one for each weight
//!   that `weight`, `map_times`, or `map_values` returns.
//!
//! In-memory runs deserialize nothing, so they count only seeks.
//!
//! Only the cursors of indexed Z-set runs (`VecIndexedWSet` and
//! `FileIndexedWSet`) count. Runs of other batch types, such as Z-sets, are
//! not counted.
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
use std::ops::AddAssign;
use std::thread::LocalKey;

use serde::Serialize;

use crate::storage::write_attribution::WriteOwner;

/// The reads of single runs on one thread; see the [module docs](self).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RunReads {
    pub seeks: u64,
    pub filter_rejections: u64,
    pub key_decodes: u64,
    pub value_decodes: u64,
    pub weight_decodes: u64,
}

/// One running total per owner, indexed like [`WriteOwner::ALL`].
type PerOwner = [Cell<u64>; WriteOwner::ALL.len()];

thread_local! {
    static CURRENT_READ_OWNER: Cell<WriteOwner> = const { Cell::new(WriteOwner::Query) };
    static SEEKS: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static FILTER_REJECTIONS: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static KEY_DECODES: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static VALUE_DECODES: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
    static WEIGHT_DECODES: PerOwner = const { [const { Cell::new(0) }; WriteOwner::ALL.len()] };
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
            filter_rejections: total(&FILTER_REJECTIONS),
            key_decodes: total(&KEY_DECODES),
            value_decodes: total(&VALUE_DECODES),
            weight_decodes: total(&WEIGHT_DECODES),
        }
    }

    /// The reads between `earlier` and `self`, both taken on this thread with
    /// [`RunReads::current`] (or both with [`RunReads::current_for`] and the
    /// same owner), `earlier` first.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            seeks: self.seeks - earlier.seeks,
            filter_rejections: self.filter_rejections - earlier.filter_rejections,
            key_decodes: self.key_decodes - earlier.key_decodes,
            value_decodes: self.value_decodes - earlier.value_decodes,
            weight_decodes: self.weight_decodes - earlier.weight_decodes,
        }
    }

    fn plus(mut self, other: Self) -> Self {
        self += other;
        self
    }
}

impl AddAssign for RunReads {
    fn add_assign(&mut self, other: Self) {
        self.seeks += other.seeks;
        self.filter_rejections += other.filter_rejections;
        self.key_decodes += other.key_decodes;
        self.value_decodes += other.value_decodes;
        self.weight_decodes += other.weight_decodes;
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

pub(crate) fn count_filter_rejection() {
    increment(&FILTER_REJECTIONS);
}

pub(crate) fn count_key_decode() {
    increment(&KEY_DECODES);
}

pub(crate) fn count_value_decode() {
    increment(&VALUE_DECODES);
}

pub(crate) fn count_weight_decode() {
    increment(&WEIGHT_DECODES);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_subtracts_each_count() {
        let earlier = RunReads::current();
        count_seek();
        count_filter_rejection();
        count_filter_rejection();
        count_filter_rejection();
        count_filter_rejection();
        count_key_decode();
        count_key_decode();
        count_value_decode();
        count_weight_decode();
        count_weight_decode();
        count_weight_decode();
        assert_eq!(
            RunReads::current().since(earlier),
            RunReads {
                seeks: 1,
                filter_rejections: 4,
                key_decodes: 2,
                value_decodes: 1,
                weight_decodes: 3,
            }
        );
    }

    #[test]
    fn add_assign_adds_each_count() {
        let mut sum = RunReads {
            seeks: 1,
            filter_rejections: 5,
            key_decodes: 2,
            value_decodes: 3,
            weight_decodes: 4,
        };
        sum += RunReads {
            seeks: 10,
            filter_rejections: 50,
            key_decodes: 20,
            value_decodes: 30,
            weight_decodes: 40,
        };
        assert_eq!(
            sum,
            RunReads {
                seeks: 11,
                filter_rejections: 55,
                key_decodes: 22,
                value_decodes: 33,
                weight_decodes: 44,
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
        filter_rejections: 0,
        key_decodes: 0,
        value_decodes: 0,
        weight_decodes: 0,
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
