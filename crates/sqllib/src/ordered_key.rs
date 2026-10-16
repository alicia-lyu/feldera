//! Merged-index key encodings for the SQL date and time types.
//!
//! Each type is a thin wrapper over an integer whose order is the type's
//! order, so it encodes as that integer.

use crate::{Date, Time, Timestamp};
use dbsp::trace::merged_index::OrderedKey;

impl OrderedKey for Date {
    const WIDTH: usize = i32::WIDTH;

    fn encode_ordered(&self, out: &mut Vec<u8>) {
        self.days().encode_ordered(out);
    }

    fn decode_ordered(bytes: &[u8]) -> Option<Self> {
        i32::decode_ordered(bytes).map(Date::from_days)
    }
}

impl OrderedKey for Time {
    const WIDTH: usize = u64::WIDTH;

    fn encode_ordered(&self, out: &mut Vec<u8>) {
        self.nanoseconds().encode_ordered(out);
    }

    fn decode_ordered(bytes: &[u8]) -> Option<Self> {
        u64::decode_ordered(bytes).map(Time::from_nanoseconds)
    }
}

impl OrderedKey for Timestamp {
    const WIDTH: usize = i64::WIDTH;

    fn encode_ordered(&self, out: &mut Vec<u8>) {
        self.microseconds().encode_ordered(out);
    }

    fn decode_ordered(bytes: &[u8]) -> Option<Self> {
        i64::decode_ordered(bytes).map(Timestamp::from_microseconds)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Date, Time, Timestamp};
    use dbsp::trace::merged_index::OrderedKey;
    use proptest::prelude::*;
    use std::fmt::Debug;

    /// Checks the `OrderedKey` contract on one pair of values.
    fn check_ordered<T: OrderedKey + Debug>(a: T, b: T) {
        let encode = |value: &T| {
            let mut bytes = Vec::new();
            value.encode_ordered(&mut bytes);
            bytes
        };
        let (encoded_a, encoded_b) = (encode(&a), encode(&b));
        assert_eq!(encoded_a.len(), T::WIDTH);
        assert_eq!(encoded_b.len(), T::WIDTH);
        assert_eq!(
            encoded_a.cmp(&encoded_b),
            a.cmp(&b),
            "order of {a:?}, {b:?}"
        );
        assert_eq!(T::decode_ordered(&encoded_a).as_ref(), Some(&a));
        assert_eq!(T::decode_ordered(&encoded_b).as_ref(), Some(&b));
        assert_eq!(T::decode_ordered(&encoded_a[1..]), None);
    }

    #[test]
    fn boundaries() {
        let days = [i32::MIN, -1, 0, 1, i32::MAX];
        let micros = [i64::MIN, -1, 0, 1, i64::MAX];
        let nanos = [0, 1, 86_399_999_999_999u64, u64::MAX];
        for a in days {
            for b in days {
                check_ordered(Date::from_days(a), Date::from_days(b));
            }
        }
        for a in micros {
            for b in micros {
                check_ordered(
                    Timestamp::from_microseconds(a),
                    Timestamp::from_microseconds(b),
                );
            }
        }
        for a in nanos {
            for b in nanos {
                check_ordered(Time::from_nanoseconds(a), Time::from_nanoseconds(b));
            }
        }
    }

    proptest! {
        #[test]
        fn date_contract(a: i32, b: i32) {
            check_ordered(Date::from_days(a), Date::from_days(b));
        }

        #[test]
        fn timestamp_contract(a: i64, b: i64) {
            check_ordered(Timestamp::from_microseconds(a), Timestamp::from_microseconds(b));
        }

        #[test]
        fn time_contract(a: u64, b: u64) {
            check_ordered(Time::from_nanoseconds(a), Time::from_nanoseconds(b));
        }
    }
}
