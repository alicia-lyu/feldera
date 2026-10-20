use dbsp::NumEntries;
use dbsp::algebra::{HasOne, HasZero, MulByRef, OptionWeightType};
use dbsp::dynamic::{HashRepr, OrdRepr};
use dbsp::trace::merged_index::{OrderedKey, PackedField};
use dbsp::utils::{IsNone, SupportsRoaring};
use feldera_types::serde_with_context::{
    DeserializeWithContext, SerializeWithContext, SqlSerdeConfig, serde_config::DecimalFormat,
};
use serde::{Deserializer, Serialize, Serializer};
use smallstr::SmallString;
use std::fmt::Write;

use crate::{DynamicDecimal, Fixed, FixedInteger};

impl<const P: usize, const S: usize> NumEntries for Fixed<P, S> {
    const CONST_NUM_ENTRIES: Option<usize> = Some(1);

    fn num_entries_shallow(&self) -> usize {
        1
    }

    fn num_entries_deep(&self) -> usize {
        1
    }
}

/// A `Fixed` archives to itself.
impl<const P: usize, const S: usize> HashRepr for Fixed<P, S> {
    const FAITHFUL: bool = true;

    #[inline]
    fn hash_repr<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(self, state)
    }
}

/// A `Fixed` archives to itself, so the comparison is `Ord` itself.
impl<const P: usize, const S: usize> OrdRepr<Fixed<P, S>> for Fixed<P, S> {
    #[inline]
    fn ord_cmp(&self, other: &Fixed<P, S>) -> core::cmp::Ordering {
        self.cmp(other)
    }
}

impl<const P: usize, const S: usize> IsNone for Fixed<P, S> {
    type Inner = Self;

    fn is_none(&self) -> bool {
        false
    }

    fn unwrap_or_self(&self) -> &Self::Inner {
        self
    }

    fn from_inner(inner: Self::Inner) -> Self {
        inner
    }
}

impl<const P: usize, const S: usize> SupportsRoaring for Fixed<P, S> {}

impl<const P: usize, const S: usize> OptionWeightType for Fixed<P, S> {}
impl<const P: usize, const S: usize> OptionWeightType for &Fixed<P, S> {}

impl<const P: usize, const S: usize> HasZero for Fixed<P, S> {
    fn is_zero(&self) -> bool {
        *self == Self::ZERO
    }

    fn zero() -> Self {
        Self::ZERO
    }
}

impl<const P: usize, const S: usize> HasOne for Fixed<P, S> {
    /// This will panic if 1 can't be represented in this type (that is, if `S >= P`).
    fn one() -> Self {
        Self::ONE
    }
}

impl<const P: usize, const S: usize> MulByRef<isize> for Fixed<P, S> {
    type Output = Self;

    fn mul_by_ref(&self, other: &isize) -> Self::Output {
        self.checked_mul_generic(FixedInteger::for_isize(*other))
            .unwrap()
    }
}

impl<const P: usize, const S: usize> MulByRef<i64> for Fixed<P, S> {
    type Output = Self;

    fn mul_by_ref(&self, other: &i64) -> Self::Output {
        self.checked_mul_generic(FixedInteger::for_i64(*other))
            .unwrap()
    }
}

impl<const P: usize, const S: usize> MulByRef<i32> for Fixed<P, S> {
    type Output = Self;

    fn mul_by_ref(&self, other: &i32) -> Self::Output {
        self.checked_mul_generic(FixedInteger::for_i32(*other))
            .unwrap()
    }
}

impl<const P: usize, const S: usize> SerializeWithContext<SqlSerdeConfig> for Fixed<P, S> {
    fn serialize_with_context<Ser>(
        &self,
        serializer: Ser,
        context: &SqlSerdeConfig,
    ) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: Serializer,
    {
        match context.decimal_format {
            DecimalFormat::Numeric => self.serialize(serializer),
            DecimalFormat::String => {
                let mut string = SmallString::<[u8; 64]>::new();
                write!(&mut string, "{self}").unwrap();
                serializer.serialize_str(&string)
            }
            DecimalFormat::I128 => serializer.serialize_i128(self.0),
        }
    }
}

impl SerializeWithContext<SqlSerdeConfig> for DynamicDecimal {
    fn serialize_with_context<Ser>(
        &self,
        serializer: Ser,
        context: &SqlSerdeConfig,
    ) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: Serializer,
    {
        match context.decimal_format {
            DecimalFormat::Numeric => self.serialize(serializer),
            DecimalFormat::String => {
                let mut string = SmallString::<[u8; 64]>::new();
                write!(&mut string, "{self}").unwrap();
                serializer.serialize_str(&string)
            }
            DecimalFormat::I128 => serializer.serialize_i128(self.significand()),
        }
    }
}

impl<'de, C, AUX, const P: usize, const S: usize> DeserializeWithContext<'de, C, AUX>
    for Fixed<P, S>
{
    #[inline(never)]
    fn deserialize_with_context<D>(deserializer: D, _context: &'de C) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde::Deserialize::deserialize(deserializer)
    }
}

impl<'de, C, AUX> DeserializeWithContext<'de, C, AUX> for DynamicDecimal {
    #[inline(never)]
    fn deserialize_with_context<D>(deserializer: D, _context: &'de C) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde::Deserialize::deserialize(deserializer)
    }
}

/// A decimal encodes as its `i128` mantissa. Two values of the same type share
/// a scale, so mantissa order is value order. Decoding rejects mantissas
/// outside `MIN..=MAX`, which no value of this precision has.
impl<const P: usize, const S: usize> OrderedKey for Fixed<P, S> {
    const WIDTH: usize = i128::WIDTH;

    fn encode_ordered(&self, out: &mut Vec<u8>) {
        self.0.encode_ordered(out);
    }

    fn decode_ordered(bytes: &[u8]) -> Option<Self> {
        let mantissa = i128::decode_ordered(bytes)?;
        (Self::MIN.0..=Self::MAX.0)
            .contains(&mantissa)
            .then_some(Self(mantissa))
    }
}

/// A decimal packs as its mantissa in the narrowest integer that holds `P`
/// digits: `i16` up to 4, `i32` up to 9, `i64` up to 18, else `i128`. A
/// stored column has one type, so the width is a function of the type and
/// needs no tag. Decoding rejects mantissas outside `MIN..=MAX`: the narrow
/// integers hold values that `P` digits do not, such as 10000 in an `i16`.
impl<const P: usize, const S: usize> PackedField for Fixed<P, S> {
    fn encode_packed(&self, out: &mut Vec<u8>) {
        const INVARIANT: &str = "a decimal's mantissa fits its declared precision";
        match P {
            0..=4 => i16::try_from(self.0).expect(INVARIANT).encode_packed(out),
            5..=9 => i32::try_from(self.0).expect(INVARIANT).encode_packed(out),
            10..=18 => i64::try_from(self.0).expect(INVARIANT).encode_packed(out),
            _ => self.0.encode_packed(out),
        }
    }

    fn decode_packed(input: &mut &[u8]) -> Option<Self> {
        let mantissa = match P {
            0..=4 => i128::from(i16::decode_packed(input)?),
            5..=9 => i128::from(i32::decode_packed(input)?),
            10..=18 => i128::from(i64::decode_packed(input)?),
            _ => i128::decode_packed(input)?,
        };
        (Self::MIN.0..=Self::MAX.0)
            .contains(&mantissa)
            .then_some(Self(mantissa))
    }
}

#[cfg(test)]
mod ordered_key_test {
    use crate::Fixed;
    use dbsp::trace::merged_index::OrderedKey;
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

    type Price = Fixed<15, 2>;
    type Wide = Fixed<38, 10>;

    fn price(mantissa: i128) -> Price {
        Fixed(mantissa)
    }

    fn wide(mantissa: i128) -> Wide {
        Fixed(mantissa)
    }

    #[test]
    fn boundaries() {
        let step = price(1);
        let values = [Price::MIN, -step, Price::ZERO, step, Price::MAX];
        for a in values {
            for b in values {
                check_ordered(a, b);
            }
        }
        let values = [Wide::MIN, wide(-1), Wide::ZERO, wide(1), Wide::MAX];
        for a in values {
            for b in values {
                check_ordered(a, b);
            }
        }
    }

    #[test]
    fn rejects_out_of_range_mantissa() {
        assert_eq!(Price::decode_ordered(&encode(&(Price::MAX.0 + 1))), None);
        assert_eq!(Price::decode_ordered(&encode(&(Price::MIN.0 - 1))), None);
        assert_eq!(Price::decode_ordered(&encode(&i128::MIN)), None);
    }

    proptest! {
        #[test]
        fn price_contract(a in Price::MIN.0..=Price::MAX.0, b in Price::MIN.0..=Price::MAX.0) {
            check_ordered(price(a), price(b));
        }

        #[test]
        fn wide_contract(a in Wide::MIN.0..=Wide::MAX.0, b in Wide::MIN.0..=Wide::MAX.0) {
            check_ordered(wide(a), wide(b));
        }
    }
}

#[cfg(test)]
mod packed_field_test {
    use crate::Fixed;
    use dbsp::trace::merged_index::PackedField;
    use proptest::prelude::*;
    use std::fmt::Debug;

    fn encode<T: PackedField>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        value.encode_packed(&mut bytes);
        bytes
    }

    fn decode<T: PackedField>(bytes: &[u8]) -> Option<T> {
        T::decode_packed(&mut &bytes[..])
    }

    /// Checks that a value encodes in `width` bytes, round-trips, and is
    /// rejected from every strict prefix of its encoding.
    fn check_round_trip<T: PackedField + PartialEq + Debug>(value: T, width: usize) {
        let bytes = encode(&value);
        assert_eq!(bytes.len(), width, "width of {value:?}");
        assert_eq!(decode::<T>(&bytes).as_ref(), Some(&value));
        for len in 0..width {
            assert_eq!(decode::<T>(&bytes[..len]), None, "{len}-byte prefix");
        }
    }

    /// Checks the limits of `Fixed<P, S>`: that its mantissa boundaries
    /// round-trip in `width` bytes, and that a mantissa one past either
    /// limit is rejected even where the narrow integer can hold it.
    fn check_limits<const P: usize, const S: usize>(width: usize) {
        for mantissa in [Fixed::<P, S>::MIN.0, -1, 0, 1, Fixed::<P, S>::MAX.0] {
            check_round_trip(Fixed::<P, S>(mantissa), width);
        }
        for mantissa in [Fixed::<P, S>::MAX.0 + 1, Fixed::<P, S>::MIN.0 - 1] {
            let le_bytes = mantissa.to_le_bytes();
            // The little-endian low bytes are the narrow integer's encoding,
            // and the mantissa fits that integer for every P tested here.
            assert_eq!(decode::<Fixed<P, S>>(&le_bytes[..width]), None);
        }
    }

    #[test]
    fn limits_round_trip_at_each_narrowing_width() {
        check_limits::<1, 0>(2);
        check_limits::<4, 2>(2);
        check_limits::<5, 2>(4);
        check_limits::<9, 3>(4);
        check_limits::<10, 3>(8);
        check_limits::<15, 2>(8);
        check_limits::<18, 4>(8);
        check_limits::<19, 4>(16);
        check_limits::<38, 10>(16);
    }

    #[test]
    fn narrowing_is_little_endian() {
        assert_eq!(encode(&Fixed::<4, 2>(0x0102)), [2, 1]);
        assert_eq!(encode(&Fixed::<9, 3>(-1)), [0xff; 4]);
    }

    type P15 = Fixed<15, 2>;

    proptest! {
        #[test]
        fn p4_round_trips(mantissa in Fixed::<4, 2>::MIN.0..=Fixed::<4, 2>::MAX.0) {
            check_round_trip(Fixed::<4, 2>(mantissa), 2);
        }

        #[test]
        fn p9_round_trips(mantissa in Fixed::<9, 3>::MIN.0..=Fixed::<9, 3>::MAX.0) {
            check_round_trip(Fixed::<9, 3>(mantissa), 4);
        }

        #[test]
        fn p15_round_trips(mantissa in P15::MIN.0..=P15::MAX.0) {
            check_round_trip(Fixed::<15, 2>(mantissa), 8);
        }

        #[test]
        fn p38_round_trips(mantissa in Fixed::<38, 10>::MIN.0..=Fixed::<38, 10>::MAX.0) {
            check_round_trip(Fixed::<38, 10>(mantissa), 16);
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..20)) {
            // A successful decode must re-encode to the bytes it consumed.
            let mut input = bytes.as_slice();
            if let Some(value) = P15::decode_packed(&mut input) {
                prop_assert_eq!(encode(&value), &bytes[..8]);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::sync::{Arc, Mutex};

    use dbsp::{
        Runtime, ZWeight, algebra::DefaultSemigroup, indexed_zset, operator::Fold,
        typed_batch::OrdIndexedZSet, utils::Tup2,
    };

    use crate::Fixed;

    // This is copied from `crates/dbsp/src/operator/dynamic/aggregate.rs`
    // with the value type changed from `i64` to `Fixed<10,0>`.
    fn count_test(workers: usize) {
        type D = Fixed<10, 0>;
        const fn d(x: i32) -> D {
            D::for_i32(x)
        }

        let count_weighted_output: Arc<Mutex<OrdIndexedZSet<u64, ZWeight>>> =
            Arc::new(Mutex::new(indexed_zset! {}));
        let sum_weighted_output: Arc<Mutex<OrdIndexedZSet<u64, D>>> =
            Arc::new(Mutex::new(indexed_zset! {}));
        let count_distinct_output: Arc<Mutex<OrdIndexedZSet<u64, D>>> =
            Arc::new(Mutex::new(indexed_zset! {}));
        let sum_distinct_output: Arc<Mutex<OrdIndexedZSet<u64, D>>> =
            Arc::new(Mutex::new(indexed_zset! {}));

        let count_weighted_output_clone = count_weighted_output.clone();
        let count_distinct_output_clone = count_distinct_output.clone();
        let sum_weighted_output_clone = sum_weighted_output.clone();
        let sum_distinct_output_clone = sum_distinct_output.clone();

        let (mut dbsp, input_handle) = Runtime::init_circuit(workers, move |circuit| {
            let (input_stream, input_handle) = circuit.add_input_indexed_zset::<u64, D>();
            input_stream
                .weighted_count()
                .gather(0)
                .inspect(move |batch| {
                    if Runtime::worker_index() == 0 {
                        *count_weighted_output.lock().unwrap() = batch.clone();
                    }
                });

            input_stream
                .aggregate_linear(|value: &D| *value)
                .gather(0)
                .inspect(move |batch| {
                    if Runtime::worker_index() == 0 {
                        *sum_weighted_output.lock().unwrap() = batch.clone();
                    }
                });

            input_stream
                .aggregate(<Fold<_, _, DefaultSemigroup<_>, _, _>>::new(
                    d(0),
                    |sum: &mut D, _v: &D, _w| *sum += d(1),
                ))
                .gather(0)
                .inspect(move |batch| {
                    if Runtime::worker_index() == 0 {
                        *count_distinct_output.lock().unwrap() = batch.clone();
                    }
                });

            input_stream
                .aggregate(<Fold<_, _, DefaultSemigroup<_>, _, _>>::new(
                    d(0),
                    |sum: &mut D, v: &D, _w| *sum += v,
                ))
                .gather(0)
                .inspect(move |batch| {
                    if Runtime::worker_index() == 0 {
                        *sum_distinct_output.lock().unwrap() = batch.clone();
                    }
                });

            Ok(input_handle)
        })
        .unwrap();

        input_handle.append(&mut vec![Tup2(1u64, Tup2(d(1), 1)), Tup2(1, Tup2(d(2), 2))]);
        dbsp.transaction().unwrap();
        assert_eq!(
            &*count_distinct_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {d(2) => 1}}
        );
        assert_eq!(
            &*sum_distinct_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {d(3) => 1}}
        );
        assert_eq!(
            &*count_weighted_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {3 => 1}}
        );
        assert_eq!(
            &*sum_weighted_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {d(5) => 1}}
        );

        input_handle.append(&mut vec![
            Tup2(2, Tup2(d(2), 1)),
            Tup2(2, Tup2(d(4), 1)),
            Tup2(1, Tup2(d(2), -1)),
        ]);
        dbsp.transaction().unwrap();
        assert_eq!(
            &*count_distinct_output_clone.lock().unwrap(),
            &indexed_zset! {2 => {d(2) => 1}}
        );
        assert_eq!(
            &*sum_distinct_output_clone.lock().unwrap(),
            &indexed_zset! {2 => {d(6) => 1}}
        );
        assert_eq!(
            &*count_weighted_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {3 => -1, 2 => 1}, 2 => {2 => 1}}
        );
        assert_eq!(
            &*sum_weighted_output_clone.lock().unwrap(),
            &indexed_zset! {2 => {d(6) => 1}, 1 => {d(5) => -1, d(3) => 1}}
        );

        input_handle.append(&mut vec![Tup2(1, Tup2(d(3), 1)), Tup2(1, Tup2(d(2), -1))]);
        dbsp.transaction().unwrap();
        assert_eq!(
            &*count_distinct_output_clone.lock().unwrap(),
            &indexed_zset! {}
        );
        assert_eq!(
            &*sum_distinct_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {d(3) => -1, d(4) => 1}}
        );
        assert_eq!(
            &*count_weighted_output_clone.lock().unwrap(),
            &indexed_zset! {}
        );
        assert_eq!(
            &*sum_weighted_output_clone.lock().unwrap(),
            &indexed_zset! {1 => {d(3) => -1, d(4) => 1}}
        );

        dbsp.kill().unwrap();
    }

    #[test]
    fn count_test1() {
        count_test(1);
    }

    #[test]
    fn count_test4() {
        count_test(4);
    }
}
