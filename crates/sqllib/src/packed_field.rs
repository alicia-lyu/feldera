//! Merged-index packed encodings for the SQL date, time, and string types.
//!
//! Dates, times, and timestamps are thin wrappers over integers and encode as
//! those integers. A string is a LEB128 length followed by its UTF-8 bytes.

use crate::{Date, SqlString, Time, Timestamp};
use dbsp::trace::merged_index::PackedField;
use dbsp::trace::merged_index::packed::{decode_length, encode_length, take};

impl PackedField for Date {
    fn encode_packed(&self, out: &mut Vec<u8>) {
        self.days().encode_packed(out);
    }

    fn decode_packed(input: &mut &[u8]) -> Option<Self> {
        i32::decode_packed(input).map(Date::from_days)
    }
}

impl PackedField for Time {
    fn encode_packed(&self, out: &mut Vec<u8>) {
        self.nanoseconds().encode_packed(out);
    }

    fn decode_packed(input: &mut &[u8]) -> Option<Self> {
        u64::decode_packed(input).map(Time::from_nanoseconds)
    }
}

impl PackedField for Timestamp {
    fn encode_packed(&self, out: &mut Vec<u8>) {
        self.microseconds().encode_packed(out);
    }

    fn decode_packed(input: &mut &[u8]) -> Option<Self> {
        i64::decode_packed(input).map(Timestamp::from_microseconds)
    }
}

/// A string is its byte length as LEB128, then its UTF-8 bytes. Decoding
/// rejects a length that runs past the input and bytes that are not UTF-8.
impl PackedField for SqlString {
    fn encode_packed(&self, out: &mut Vec<u8>) {
        let bytes = self.str().as_bytes();
        encode_length(bytes.len(), out);
        out.extend_from_slice(bytes);
    }

    fn decode_packed(input: &mut &[u8]) -> Option<Self> {
        let len = decode_length(input)?;
        let text = std::str::from_utf8(take(input, len)?).ok()?;
        Some(SqlString::from(text))
    }
}

#[cfg(test)]
mod tests {
    use crate::{Date, SqlString, Time, Timestamp};
    use dbsp::trace::merged_index::PackedField;
    use proptest::prelude::*;
    use std::fmt::Debug;

    fn encode<T: PackedField>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.encode_packed(&mut bytes);
        bytes
    }

    /// Checks that a value round-trips, that a decoder leaves the bytes that
    /// follow it, and that every strict prefix of the encoding is rejected.
    fn check_round_trip<T: PackedField + PartialEq + Debug>(value: T) {
        let bytes = encode(&value);
        let mut with_tail = bytes.clone();
        with_tail.push(0xa5);
        let mut input = with_tail.as_slice();
        assert_eq!(T::decode_packed(&mut input).as_ref(), Some(&value));
        assert_eq!(input, [0xa5]);
        for len in 0..bytes.len() {
            assert!(
                T::decode_packed(&mut &bytes[..len]).is_none(),
                "{value:?} decoded from a {len}-byte prefix"
            );
        }
    }

    #[test]
    fn date_time_and_timestamp_boundaries_round_trip() {
        for days in [i32::MIN, -1, 0, 1, i32::MAX] {
            check_round_trip(Date::from_days(days));
        }
        for micros in [i64::MIN, -1, 0, 1, i64::MAX] {
            check_round_trip(Timestamp::from_microseconds(micros));
        }
        for nanos in [0, 1, 86_399_999_999_999u64, u64::MAX] {
            check_round_trip(Time::from_nanoseconds(nanos));
        }
    }

    #[test]
    fn string_is_length_then_utf8() {
        assert_eq!(encode(&SqlString::from("")), [0]);
        assert_eq!(encode(&SqlString::from("AB")), [2, b'A', b'B']);
        // "é" is two UTF-8 bytes.
        assert_eq!(encode(&SqlString::from("é")), [2, 0xc3, 0xa9]);
        let long = SqlString::from("x".repeat(200).as_str());
        let bytes = encode(&long);
        assert_eq!(bytes[..2], [0xc8, 0x01]);
        assert_eq!(bytes.len(), 202);
        check_round_trip(long);
        check_round_trip(SqlString::from("BUILDING"));
        check_round_trip(SqlString::from("日本語"));
    }

    #[test]
    fn malformed_strings_are_rejected() {
        let decode = |bytes: &[u8]| SqlString::decode_packed(&mut &bytes[..]);
        // The length runs past the input.
        assert_eq!(decode(&[3, b'a', b'b']), None);
        // Invalid UTF-8: a lone continuation byte, and an overlong encoding.
        assert_eq!(decode(&[1, 0x80]), None);
        assert_eq!(decode(&[2, 0xc0, 0x80]), None);
        // A padded length is a second encoding of the same string.
        assert_eq!(decode(&[0x81, 0x00, b'a']), None);
        assert_eq!(decode(&[0x80, 0x00]), None);
        // An unterminated or enormous length.
        assert_eq!(decode(&[0x80]), None);
        assert_eq!(decode(&[0xff; 12]), None);
        assert_eq!(decode(&[]), None);
    }

    proptest! {
        #[test]
        fn date_round_trips(days: i32) {
            check_round_trip(Date::from_days(days));
        }

        #[test]
        fn timestamp_round_trips(micros: i64) {
            check_round_trip(Timestamp::from_microseconds(micros));
        }

        #[test]
        fn time_round_trips(nanos: u64) {
            check_round_trip(Time::from_nanoseconds(nanos));
        }

        #[test]
        fn string_round_trips(text in "\\PC{0,300}") {
            check_round_trip(SqlString::from(text.as_str()));
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..40)) {
            // A successful decode must re-encode to the bytes it consumed.
            let mut input = bytes.as_slice();
            if let Some(value) = SqlString::decode_packed(&mut input) {
                let consumed = &bytes[..bytes.len() - input.len()];
                prop_assert_eq!(encode(&value), consumed);
            }
        }
    }
}
