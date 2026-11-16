//! The circuit against a brute-force join, over transactions that the generated circuit's
//! RF1/RF2-only readers could not take: a customer replaced, an order moved to another
//! customer with its lines, and lines added to and removed from existing orders.

use std::{collections::BTreeMap, sync::Arc};

use dbsp::{
    circuit::CircuitConfig,
    dynamic::{DowncastTrait, DynData},
    trace::{
        merged_index::{MergedIndexBatch, Pass},
        BatchReader as DynamicBatchReader, BatchReaderFactories, Cursor, OrdIndexedWSetFactories,
        SpineSnapshot,
    },
    typed_batch::BatchReader,
    utils::Tup9,
    DynZWeight, OrdZSet, OutputHandle, Runtime,
};
use feldera_sqllib::{Date, SqlDecimal};
use merged_index_col::{
    build_batch, q3::Q3State, CustomerByCustkeyKey, CustomerByCustkeyPayload, Key,
    LineitemByCustkeyOrderkeyLinenumberKey, LineitemByCustkeyOrderkeyLinenumberPayload,
    OrdersByCustkeyOrderkeyKey, OrdersByCustkeyOrderkeyPayload, Payload, StoredPayload,
};

use super::{view, TrieInputs, ViewTuple};

type Row = (Key, Payload, i64);

fn customer(custkey: i64, segment: &str, weight: i64) -> Row {
    (
        Key::CustomerByCustkey(CustomerByCustkeyKey { custkey }),
        Payload::CustomerByCustkey(CustomerByCustkeyPayload {
            c_mktsegment: segment.into(),
        }),
        weight,
    )
}

fn order(custkey: i64, orderkey: i64, day: i32, weight: i64) -> Row {
    (
        Key::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyKey { custkey, orderkey }),
        Payload::OrdersByCustkeyOrderkey(OrdersByCustkeyOrderkeyPayload {
            o_orderdate: Date::from_days(day),
            o_shippriority: 7,
        }),
        weight,
    )
}

fn line(custkey: i64, orderkey: i64, linenumber: i32, price: i128, weight: i64) -> Row {
    (
        Key::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberKey {
            custkey,
            orderkey,
            linenumber,
        }),
        Payload::LineitemByCustkeyOrderkeyLinenumber(LineitemByCustkeyOrderkeyLinenumberPayload {
            l_shipdate: Date::from_days(30),
            l_extendedprice: SqlDecimal::new(price, 2).unwrap(),
            l_discount: SqlDecimal::new(4, 2).unwrap(),
        }),
        weight,
    )
}

/// The index state after some transactions: the sum of their rows. This is the reference's
/// source of truth for `V`.
#[derive(Default)]
struct Model(BTreeMap<(Key, Payload), i64>);

impl Model {
    fn apply(&mut self, rows: &[Row]) {
        for (key, payload, weight) in rows {
            *self.0.entry((key.clone(), payload.clone())).or_default() += weight;
        }
        self.0.retain(|_, weight| *weight != 0);
    }

    /// `V = C ⋈ O ⋈ L` over the entries of the model, with their weights multiplied.
    fn view(&self) -> BTreeMap<ViewTuple, i64> {
        let mut result = BTreeMap::new();
        for ((line_key, line_payload), line_weight) in &self.0 {
            let (
                Key::LineitemByCustkeyOrderkeyLinenumber(l),
                Payload::LineitemByCustkeyOrderkeyLinenumber(lp),
            ) = (line_key, line_payload)
            else {
                continue;
            };
            for ((order_key, order_payload), order_weight) in &self.0 {
                let (Key::OrdersByCustkeyOrderkey(o), Payload::OrdersByCustkeyOrderkey(op)) =
                    (order_key, order_payload)
                else {
                    continue;
                };
                if (o.custkey, o.orderkey) != (l.custkey, l.orderkey) {
                    continue;
                }
                for ((customer_key, customer_payload), customer_weight) in &self.0 {
                    let (Key::CustomerByCustkey(c), Payload::CustomerByCustkey(cp)) =
                        (customer_key, customer_payload)
                    else {
                        continue;
                    };
                    if c.custkey != o.custkey {
                        continue;
                    }
                    *result
                        .entry(Tup9::new(
                            c.custkey,
                            cp.c_mktsegment.clone(),
                            o.orderkey,
                            op.o_orderdate,
                            op.o_shippriority,
                            l.linenumber,
                            lp.l_extendedprice,
                            lp.l_discount,
                            lp.l_shipdate,
                        ))
                        .or_default() += line_weight * order_weight * customer_weight;
                }
            }
        }
        result.retain(|_, weight| *weight != 0);
        result
    }
}

fn difference(
    before: &BTreeMap<ViewTuple, i64>,
    after: &BTreeMap<ViewTuple, i64>,
) -> BTreeMap<ViewTuple, i64> {
    let mut delta = BTreeMap::new();
    for (row, weight) in before {
        *delta.entry(row.clone()).or_default() -= weight;
    }
    for (row, weight) in after {
        *delta.entry(row.clone()).or_default() += weight;
    }
    delta.retain(|_, weight| *weight != 0);
    delta
}

fn rows_of(zset: &OrdZSet<ViewTuple>) -> BTreeMap<ViewTuple, i64> {
    let mut cursor = zset.inner().cursor();
    let mut rows = BTreeMap::new();
    while cursor.key_valid() {
        *rows
            .entry(cursor.key().downcast_checked::<ViewTuple>().clone())
            .or_default() += **cursor.weight();
        cursor.step_key();
    }
    rows
}

/// Runs the transactions in order. Each is the rows of that transaction's `Dlt`; after each,
/// the circuit's output delta must be the brute-force change of `V`.
fn check(transactions: Vec<Vec<Row>>) {
    let state = Q3State::default();
    let build_state = state.clone();
    let (mut circuit, (inputs, output)): (_, (TrieInputs, OutputHandle<OrdZSet<ViewTuple>>)) =
        Runtime::init_circuit(CircuitConfig::with_workers(1), move |circuit| {
            let (inputs, view) = view(circuit, build_state.clone());
            Ok((inputs, view.output()))
        })
        .unwrap();

    let factories = OrdIndexedWSetFactories::<DynData, DynData, DynZWeight>::new::<
        Vec<u8>,
        StoredPayload,
        i64,
    >();
    let mut runs: Vec<Arc<MergedIndexBatch>> = Vec::new();
    let mut model = Model::default();
    for (number, rows) in transactions.into_iter().enumerate() {
        let before = model.view();
        model.apply(&rows);
        let after = model.view();

        let old = SpineSnapshot::with_batches(&factories, runs.clone());
        let dlt = Arc::new(build_batch(rows).unwrap());
        runs.push(dlt.clone());
        state.publish(old, dlt).unwrap();
        inputs.set(state.collect(Pass::Seek).unwrap().driving);
        circuit.transaction().unwrap();
        state.finish().unwrap();

        assert_eq!(
            rows_of(&output.consolidate()),
            difference(&before, &after),
            "transaction {number}"
        );
    }
}

/// The load, then a transaction that replaces a customer, moves an order to another customer
/// with its lines (they are deleted at one customer and inserted at the other), changes the
/// lines of another order, and inserts an order with line items.
#[test]
fn deltas_beyond_rf1_rf2_match_the_brute_force_join() {
    check(vec![
        vec![
            customer(1, "A", 1),
            customer(2, "B", 1),
            order(1, 10, 5, 1),
            line(1, 10, 1, 100, 1),
            line(1, 10, 2, 100, 1),
            order(2, 20, 6, 1),
            line(2, 20, 1, 300, 1),
        ],
        vec![
            customer(1, "A", -1),
            customer(1, "A2", 1),
            order(1, 10, 5, -1),
            order(2, 10, 5, 1),
            line(1, 10, 1, 100, -1),
            line(1, 10, 2, 100, -1),
            line(2, 10, 1, 100, 1),
            line(2, 10, 2, 100, 1),
            line(2, 20, 2, 400, 1),
            line(2, 20, 1, 300, -1),
            order(1, 30, 7, 1),
            line(1, 30, 1, 500, 1),
        ],
    ]);
}

/// Orders and customers that arrive, change, and leave in turn, with a transaction that
/// changes nothing.
#[test]
fn single_stream_transactions_and_empty_ones_match() {
    check(vec![
        vec![order(1, 10, 5, 1), line(1, 10, 1, 100, 1)],
        vec![customer(1, "A", 1)],
        vec![],
        vec![line(1, 10, 2, 200, 1)],
        vec![customer(1, "A", -1), customer(1, "B", 1)],
        vec![
            order(1, 10, 5, -1),
            line(1, 10, 1, 100, -1),
            line(1, 10, 2, 200, -1),
        ],
    ]);
}
