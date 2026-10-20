//! Packed payload encodings for stored merged-index values.
//!
//! A payload is stored as opaque bytes that are read only together with the
//! key, whose placement id names the payload's fields. Nothing in the bytes
//! says which fields they hold, and nothing aligns them: fields are written
//! back to back, in little-endian order.
//!
//! Unlike [`OrderedKey`](super::OrderedKey), a packed encoding needs no byte
//! order and no fixed width. Stored values are compared only for equality,
//! because each folded key is unique.
//!
//! Decoding accepts only canonical bytes: one value has exactly one encoding,
//! and every other byte string is an error, never a panic.

use crate::storage::file::front_code::{read_leb128, write_leb128};

/// A payload field with a packed encoding.
pub trait PackedField: Sized {
    /// Appends the encoding of `self` to `out`.
    fn encode_packed(&self, out: &mut Vec<u8>);

    /// Reads one value from the front of `input` and advances `input` past
    /// it. Returns `None` unless the front of `input` is the canonical
    /// encoding of some value. On `None`, `input` is unspecified.
    fn decode_packed(input: &mut &[u8]) -> Option<Self>;
}

/// Splits `len` bytes off the front of `input`.
pub fn take<'a>(input: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (head, rest) = input.split_at_checked(len)?;
    *input = rest;
    Some(head)
}

/// Appends a length as LEB128, the prefix of every variable-length field.
pub fn encode_length(len: usize, out: &mut Vec<u8>) {
    write_leb128(out, len);
}

/// Reads a canonical LEB128 length from the front of `input`. A padded
/// encoding such as `[0x80, 0x00]` for zero is rejected, because the same
/// length would then have two encodings.
pub fn decode_length(input: &mut &[u8]) -> Option<usize> {
    let mut position = 0;
    let len = read_leb128(input, &mut position).ok()?;
    let mut canonical = Vec::with_capacity(position);
    write_leb128(&mut canonical, len);
    if input[..position] != canonical[..] {
        return None;
    }
    take(input, position)?;
    Some(len)
}

macro_rules! integer_packed_field {
    ($($int:ty),*) => {$(
        impl PackedField for $int {
            fn encode_packed(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn decode_packed(input: &mut &[u8]) -> Option<Self> {
                let bytes = take(input, size_of::<$int>())?;
                Some(Self::from_le_bytes(bytes.try_into().ok()?))
            }
        }
    )*};
}

// `usize` and `isize` are omitted: their width depends on the platform, and a
// stored payload must not.
integer_packed_field!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

impl PackedField for bool {
    fn encode_packed(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn decode_packed(input: &mut &[u8]) -> Option<Self> {
        match take(input, 1)? {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }
}

/// Writes the fields of one payload, in order.
#[derive(Debug, Default)]
pub struct PayloadEncoder {
    bytes: Vec<u8>,
}

impl PayloadEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends the packed encoding of the next field.
    pub fn field<T: PackedField>(mut self, value: &T) -> Self {
        value.encode_packed(&mut self.bytes);
        self
    }

    /// The bytes of the payload.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Reads the fields of one payload written by [`PayloadEncoder`].
#[derive(Debug)]
pub struct PayloadDecoder<'a> {
    remaining: &'a [u8],
}

impl<'a> PayloadDecoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    /// Reads the next field.
    pub fn field<T: PackedField>(&mut self) -> Result<T, String> {
        T::decode_packed(&mut self.remaining).ok_or_else(|| {
            format!(
                "payload has a truncated or non-canonical {} field",
                std::any::type_name::<T>()
            )
        })
    }

    /// Checks that every byte was consumed.
    pub fn finish(self) -> Result<(), String> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "payload has {} trailing bytes",
                self.remaining.len()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PackedField, PayloadDecoder, PayloadEncoder, decode_length, encode_length};
    use proptest::prelude::*;
    use std::fmt::Debug;

    fn encode<T: PackedField>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.encode_packed(&mut bytes);
        bytes
    }

    /// Decodes `bytes` and requires that every byte was consumed.
    fn decode_exact<T: PackedField>(bytes: &[u8]) -> Option<T> {
        let mut input = bytes;
        let value = T::decode_packed(&mut input)?;
        input.is_empty().then_some(value)
    }

    /// Checks that a value round-trips and that every strict prefix of its
    /// encoding is rejected.
    fn check_round_trip<T: PackedField + PartialEq + Debug>(value: T) {
        let bytes = encode(&value);
        assert_eq!(decode_exact::<T>(&bytes).as_ref(), Some(&value));
        for len in 0..bytes.len() {
            assert!(
                T::decode_packed(&mut &bytes[..len]).is_none(),
                "{value:?} decoded from a {len}-byte prefix"
            );
        }
        // A decoder stops after one value and leaves the rest.
        let mut with_tail = bytes.clone();
        with_tail.extend_from_slice(&[0xa5, 0x5a]);
        let mut input = with_tail.as_slice();
        assert_eq!(T::decode_packed(&mut input).as_ref(), Some(&value));
        assert_eq!(input, [0xa5, 0x5a]);
    }

    #[test]
    fn integers_are_little_endian_and_unpadded() {
        assert_eq!(encode(&0x0102_0304u32), [4, 3, 2, 1]);
        assert_eq!(encode(&-2i16), [0xfe, 0xff]);
        assert_eq!(encode(&1u8), [1]);
        assert_eq!(encode(&true), [1]);
        assert_eq!(encode(&i128::MIN).len(), 16);
    }

    #[test]
    fn integer_boundaries_round_trip() {
        check_round_trip(i16::MIN);
        check_round_trip(i16::MAX);
        check_round_trip(i32::MIN);
        check_round_trip(i32::MAX);
        check_round_trip(i64::MIN);
        check_round_trip(i64::MAX);
        check_round_trip(i128::MIN);
        check_round_trip(i128::MAX);
        check_round_trip(u128::MAX);
        check_round_trip(0u8);
    }

    #[test]
    fn bool_accepts_only_zero_and_one() {
        check_round_trip(false);
        check_round_trip(true);
        assert_eq!(decode_exact::<bool>(&[2]), None);
        assert_eq!(decode_exact::<bool>(&[0xff]), None);
    }

    #[test]
    fn lengths_are_canonical_leb128() {
        for len in [0, 1, 127, 128, 16_383, 16_384, usize::MAX >> 1] {
            let mut bytes = Vec::new();
            encode_length(len, &mut bytes);
            let mut input = bytes.as_slice();
            assert_eq!(decode_length(&mut input), Some(len));
            assert!(input.is_empty());
        }
        // Padded encodings of 0 and 1, and an unterminated length.
        assert_eq!(decode_length(&mut &[0x80, 0x00][..]), None);
        assert_eq!(decode_length(&mut &[0x81, 0x00][..]), None);
        assert_eq!(decode_length(&mut &[0x80][..]), None);
        assert_eq!(decode_length(&mut &[][..]), None);
        assert_eq!(decode_length(&mut &[0xff; 11][..]), None);
    }

    #[test]
    fn payload_decoder_rejects_truncated_and_trailing_bytes() {
        let bytes = PayloadEncoder::new()
            .field(&7i32)
            .field(&true)
            .field(&-9i64)
            .finish();
        assert_eq!(bytes.len(), 4 + 1 + 8);

        let mut decoder = PayloadDecoder::new(&bytes);
        assert_eq!(decoder.field::<i32>(), Ok(7));
        assert_eq!(decoder.field::<bool>(), Ok(true));
        assert_eq!(decoder.field::<i64>(), Ok(-9));
        assert_eq!(decoder.finish(), Ok(()));

        let mut truncated = PayloadDecoder::new(&bytes[..bytes.len() - 1]);
        truncated.field::<i32>().unwrap();
        truncated.field::<bool>().unwrap();
        assert!(truncated.field::<i64>().is_err());

        let mut padded = bytes.clone();
        padded.push(0);
        let mut decoder = PayloadDecoder::new(&padded);
        decoder.field::<i32>().unwrap();
        decoder.field::<bool>().unwrap();
        decoder.field::<i64>().unwrap();
        assert_eq!(decoder.finish(), Err("payload has 1 trailing bytes".into()));
    }

    proptest! {
        #[test]
        fn i8_round_trips(value: i8) { check_round_trip(value); }
        #[test]
        fn i16_round_trips(value: i16) { check_round_trip(value); }
        #[test]
        fn i32_round_trips(value: i32) { check_round_trip(value); }
        #[test]
        fn i64_round_trips(value: i64) { check_round_trip(value); }
        #[test]
        fn i128_round_trips(value: i128) { check_round_trip(value); }
        #[test]
        fn u8_round_trips(value: u8) { check_round_trip(value); }
        #[test]
        fn u16_round_trips(value: u16) { check_round_trip(value); }
        #[test]
        fn u32_round_trips(value: u32) { check_round_trip(value); }
        #[test]
        fn u64_round_trips(value: u64) { check_round_trip(value); }
        #[test]
        fn u128_round_trips(value: u128) { check_round_trip(value); }

        #[test]
        fn length_round_trips(len in 0usize..(1 << 40)) {
            let mut bytes = Vec::new();
            encode_length(len, &mut bytes);
            prop_assert_eq!(decode_length(&mut bytes.as_slice()), Some(len));
        }
    }
}
