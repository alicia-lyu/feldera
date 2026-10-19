//! Fixed-width key encodings whose byte order is the value order.
//!
//! A folded key compares with `memcmp`, so each key field must be written as
//! bytes that sort like the value. Unsigned integers are written big-endian.
//! Signed integers flip the sign bit first, so that negative values, whose
//! two's-complement high bit is set, sort before non-negative ones.
//!
//! Every encoding has a fixed width, and decoding accepts only canonical
//! bytes. A folded key therefore parses into exactly one sequence of values.

/// A fixed-width key encoding whose byte order is the value order.
///
/// For all `a` and `b`, comparing the encodings of `a` and `b` as byte
/// strings gives `a.cmp(&b)`.
pub trait OrderedKey: Ord + Sized {
    /// The number of bytes in every encoding.
    const WIDTH: usize;

    /// Appends exactly `WIDTH` bytes to `out`.
    fn encode_ordered(&self, out: &mut Vec<u8>);

    /// The inverse of [`encode_ordered`](Self::encode_ordered). Returns `None`
    /// unless `bytes` is the canonical encoding of some value.
    fn decode_ordered(bytes: &[u8]) -> Option<Self>;
}

macro_rules! unsigned_ordered_key {
    ($($int:ty),*) => {$(
        impl OrderedKey for $int {
            const WIDTH: usize = size_of::<$int>();

            fn encode_ordered(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_be_bytes());
            }

            fn decode_ordered(bytes: &[u8]) -> Option<Self> {
                Some(Self::from_be_bytes(bytes.try_into().ok()?))
            }
        }
    )*};
}

macro_rules! signed_ordered_key {
    ($($int:ty => $unsigned:ty),*) => {$(
        impl OrderedKey for $int {
            const WIDTH: usize = size_of::<$int>();

            fn encode_ordered(&self, out: &mut Vec<u8>) {
                let sign_flipped = (*self as $unsigned) ^ (1 << (<$unsigned>::BITS - 1));
                out.extend_from_slice(&sign_flipped.to_be_bytes());
            }

            fn decode_ordered(bytes: &[u8]) -> Option<Self> {
                let sign_flipped = <$unsigned>::from_be_bytes(bytes.try_into().ok()?);
                Some((sign_flipped ^ (1 << (<$unsigned>::BITS - 1))) as $int)
            }
        }
    )*};
}

// `usize` and `isize` are omitted: their width depends on the platform, and a
// stored key must not.
unsigned_ordered_key!(u8, u16, u32, u64, u128);
signed_ordered_key!(i8 => u8, i16 => u16, i32 => u32, i64 => u64, i128 => u128);

impl OrderedKey for bool {
    const WIDTH: usize = 1;

    fn encode_ordered(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn decode_ordered(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0] => Some(false),
            [1] => Some(true),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OrderedKey;
    use proptest::prelude::*;
    use std::fmt::Debug;

    fn encode<T: OrderedKey>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.encode_ordered(&mut bytes);
        bytes
    }

    /// Checks the `OrderedKey` contract on one pair of values.
    fn check_ordered<T: OrderedKey + Debug>(a: T, b: T) {
        let (encoded_a, encoded_b) = (encode(&a), encode(&b));
        assert_eq!(encoded_a.len(), T::WIDTH, "width of {a:?}");
        assert_eq!(encoded_b.len(), T::WIDTH, "width of {b:?}");
        assert_eq!(
            encoded_a.cmp(&encoded_b),
            a.cmp(&b),
            "order of {a:?}, {b:?}"
        );
        assert_eq!(T::decode_ordered(&encoded_a).as_ref(), Some(&a));
        assert_eq!(T::decode_ordered(&encoded_b).as_ref(), Some(&b));
        assert_eq!(T::decode_ordered(&encoded_a[1..]), None, "short input");
        let mut long = encoded_a;
        long.push(0);
        assert_eq!(T::decode_ordered(&long), None, "long input");
    }

    /// Checks every pair drawn from boundary values of an integer type.
    macro_rules! check_boundaries {
        ($int:ty, [$($value:expr),*]) => {{
            let values: Vec<$int> = vec![$($value),*];
            for &a in &values {
                for &b in &values {
                    check_ordered(a, b);
                }
            }
        }};
    }

    #[test]
    fn signed_boundaries() {
        check_boundaries!(i8, [i8::MIN, i8::MIN + 1, -1, 0, 1, i8::MAX - 1, i8::MAX]);
        check_boundaries!(i16, [i16::MIN, -1, 0, 1, i16::MAX]);
        check_boundaries!(i32, [i32::MIN, -1, 0, 1, i32::MAX]);
        check_boundaries!(i64, [i64::MIN, -1, 0, 1, i64::MAX]);
        check_boundaries!(i128, [i128::MIN, -1, 0, 1, i128::MAX]);
    }

    #[test]
    fn unsigned_boundaries() {
        check_boundaries!(u8, [0, 1, 0x7f, 0x80, u8::MAX]);
        check_boundaries!(u16, [0, 1, u16::MAX]);
        check_boundaries!(u32, [0, 1, u32::MAX]);
        check_boundaries!(u64, [0, 1, u64::MAX]);
        check_boundaries!(u128, [0, 1, u128::MAX]);
    }

    #[test]
    fn bool_order_and_canonical_form() {
        for a in [false, true] {
            for b in [false, true] {
                check_ordered(a, b);
            }
        }
        assert_eq!(bool::decode_ordered(&[2]), None);
        assert_eq!(bool::decode_ordered(&[0xff]), None);
    }

    /// The encodings of `i32` and `i64` must stay byte-identical to the ones
    /// `MergedIndex::prefix` writes, which stored COL keys already use.
    #[test]
    fn matches_existing_folded_key_bytes() {
        assert_eq!(encode(&0i64), [0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            encode(&-1i64),
            [0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(encode(&i64::MIN), [0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(encode(&0i32), [0x80, 0, 0, 0]);
        assert_eq!(encode(&-1i32), [0x7f, 0xff, 0xff, 0xff]);
        assert_eq!(encode(&i32::MAX), [0xff, 0xff, 0xff, 0xff]);
        for value in [i64::MIN, -7, 0, 7, i64::MAX] {
            let legacy = ((value as u64) ^ 0x8000_0000_0000_0000).to_be_bytes();
            assert_eq!(encode(&value), legacy);
        }
        for value in [i32::MIN, -7, 0, 7, i32::MAX] {
            let legacy = ((value as u32) ^ 0x8000_0000).to_be_bytes();
            assert_eq!(encode(&value), legacy);
        }
    }

    proptest! {
        #[test]
        fn i8_contract(a: i8, b: i8) { check_ordered(a, b); }
        #[test]
        fn i16_contract(a: i16, b: i16) { check_ordered(a, b); }
        #[test]
        fn i32_contract(a: i32, b: i32) { check_ordered(a, b); }
        #[test]
        fn i64_contract(a: i64, b: i64) { check_ordered(a, b); }
        #[test]
        fn i128_contract(a: i128, b: i128) { check_ordered(a, b); }
        #[test]
        fn u8_contract(a: u8, b: u8) { check_ordered(a, b); }
        #[test]
        fn u16_contract(a: u16, b: u16) { check_ordered(a, b); }
        #[test]
        fn u32_contract(a: u32, b: u32) { check_ordered(a, b); }
        #[test]
        fn u64_contract(a: u64, b: u64) { check_ordered(a, b); }
        #[test]
        fn u128_contract(a: u128, b: u128) { check_ordered(a, b); }
    }
}
