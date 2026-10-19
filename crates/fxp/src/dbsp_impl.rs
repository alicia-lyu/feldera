use dbsp::NumEntries;
use dbsp::algebra::{HasOne, HasZero, MulByRef, OptionWeightType};
use dbsp::dynamic::{HashRepr, OrdRepr};
use dbsp::trace::merged_index::OrderedKey;
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
