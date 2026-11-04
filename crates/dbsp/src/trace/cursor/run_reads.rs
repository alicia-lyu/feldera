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

use std::cell::Cell;

use serde::Serialize;

/// The reads of single runs on one thread; see the [module docs](self).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RunReads {
    pub seeks: u64,
    pub key_reads: u64,
    pub value_reads: u64,
    pub weight_reads: u64,
}

thread_local! {
    static SEEKS: Cell<u64> = const { Cell::new(0) };
    static KEY_READS: Cell<u64> = const { Cell::new(0) };
    static VALUE_READS: Cell<u64> = const { Cell::new(0) };
    static WEIGHT_READS: Cell<u64> = const { Cell::new(0) };
}

impl RunReads {
    /// This thread's running totals.
    pub fn current() -> Self {
        Self {
            seeks: SEEKS.get(),
            key_reads: KEY_READS.get(),
            value_reads: VALUE_READS.get(),
            weight_reads: WEIGHT_READS.get(),
        }
    }

    /// The reads between `earlier` and `self`, both taken on this thread with
    /// [`RunReads::current`], `earlier` first.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            seeks: self.seeks - earlier.seeks,
            key_reads: self.key_reads - earlier.key_reads,
            value_reads: self.value_reads - earlier.value_reads,
            weight_reads: self.weight_reads - earlier.weight_reads,
        }
    }
}

fn increment(counter: &'static std::thread::LocalKey<Cell<u64>>) {
    counter.set(counter.get() + 1);
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
}
