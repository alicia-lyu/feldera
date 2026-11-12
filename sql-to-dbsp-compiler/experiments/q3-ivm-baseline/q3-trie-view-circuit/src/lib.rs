//! The Q3 view circuit, written by hand to meet the two assumptions of buffered
//! execution (`docs/design/merged-index/shared-traversal.md`).
//!
//! The view is `V = Customer ⋈ Orders ⋈ Lineitem`, maintained as two joins:
//!
//! * `J = O ⋈ C` on `(custkey)`. `J`'s rows are indexed by `(custkey,
//!   orderkey)`, the node at which `V` reads them.
//! * `V = L ⋈ J` on `(custkey, orderkey)`.
//!
//! **(A1) Join keys on the trie.** Each join's key is a trie node and a prefix
//! of its inputs' sort keys. The inputs are keyed by their full trie paths, which
//! each input row carries: `C` by `(custkey)`, `O` by `(custkey, orderkey)`, and
//! `L` by `(custkey, orderkey, linenumber)`. A line item carries its customer, and
//! `J` keeps `(custkey, orderkey)`.
//!
//! **(A2) Complete deltas.** The inputs are the transaction's `Dlt`: three sources
//! emit the readers' `Dlt` batches at the commit's flush ([`Q3State::customer_deltas`]
//! and its siblings), every row at its full trie path. The joins are the
//! snapshot-reader joins of `merged_index_col::q3`, so the four partner reads are
//! `C⁺`, `O⁻`, `J⁺`, and `L⁻`; only their keys differ from the generated circuit's.
//!
//! `V`'s output columns and JSON match the generated circuit's (`q3.sql`).
//! There is no `ERROR_VIEW`.

use dbsp::{
    circuit::{circuit_builder::Circuit, RootCircuit},
    operator::TransactionGenerator,
    utils::{Tup2, Tup3, Tup9},
    Error, OrdZSet, Stream,
};
use dbsp_adapters::Catalog;
use feldera_sqllib::{Date, SqlDecimal, SqlString, Variant};
use feldera_types::{deserialize_table_record, serialize_table_record};
use merged_index_col::q3::{join_lineitems_orders, join_orders_customers, Q3State};

/// A row of `V`: `(c_custkey, c_mktsegment, o_orderkey, o_orderdate,
/// o_shippriority, l_linenumber, l_extendedprice, l_discount, l_shipdate)`.
pub type ViewTuple =
    Tup9<i64, SqlString, i64, Date, i32, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

/// A source that emits one driving delta, `delta(state)`, at each transaction's flush, and
/// an empty batch on the other steps. The caller collects the readers before the transaction.
fn driving_delta<B: Default + Clone + 'static>(
    circuit: &mut RootCircuit,
    state: Q3State,
    delta: fn(&Q3State) -> Result<B, String>,
) -> Stream<RootCircuit, B> {
    circuit.add_source(TransactionGenerator::new(move |flush| {
        if flush {
            delta(&state).expect("Q3 driving delta: collect the readers before the transaction")
        } else {
            B::default()
        }
    }))
}

/// Builds the sources and joins in `circuit` and returns the stream of `V`.
pub fn view(circuit: &mut RootCircuit, state: Q3State) -> Stream<RootCircuit, OrdZSet<ViewTuple>> {
    // Each delta is already indexed by its join key, a prefix of its trie path.
    let customers_by_custkey = driving_delta(circuit, state.clone(), Q3State::customer_deltas);
    let orders_by_custkey = driving_delta(circuit, state.clone(), Q3State::order_deltas);
    let lines_by_order = driving_delta(circuit, state.clone(), Q3State::line_deltas);

    // `J` keeps `(custkey, orderkey)`: the node at which `V` reads it.
    let joined = join_orders_customers(
        &orders_by_custkey,
        &customers_by_custkey,
        state.clone(),
        |custkey, order, customer| {
            Some((
                Tup2::new(custkey.0, order.0),
                Tup3::new(customer.0.clone(), order.1, order.2),
            ))
        },
    );
    join_lineitems_orders(
        &lines_by_order,
        &joined,
        state,
        |node, line, order| -> ViewTuple {
            Tup9::new(
                node.0,
                order.0.clone(),
                node.1,
                order.1,
                order.2,
                line.0,
                line.1,
                line.2,
                line.3,
            )
        },
    )
}

/// `V` as the catalog registers it: the same record and relation as the generated circuit's.
#[derive(Clone, Debug, Eq, PartialEq, Default, PartialOrd, Ord)]
pub struct ViewRecord {
    field0: i64,
    field1: SqlString,
    field2: i64,
    field3: Date,
    field4: i32,
    field5: i32,
    field6: SqlDecimal<15, 2>,
    field7: SqlDecimal<4, 2>,
    field8: Date,
}

impl From<ViewRecord> for ViewTuple {
    fn from(t: ViewRecord) -> Self {
        Tup9::new(
            t.field0, t.field1, t.field2, t.field3, t.field4, t.field5, t.field6, t.field7,
            t.field8,
        )
    }
}

impl From<ViewTuple> for ViewRecord {
    fn from(t: ViewTuple) -> Self {
        Self {
            field0: t.0,
            field1: t.1,
            field2: t.2,
            field3: t.3,
            field4: t.4,
            field5: t.5,
            field6: t.6,
            field7: t.7,
            field8: t.8,
        }
    }
}

deserialize_table_record!(ViewRecord["v", Variant, 9] {
    (field0, "c_custkey", false, i64, |_| None),
    (field1, "c_mktsegment", false, SqlString, |_| None),
    (field2, "o_orderkey", false, i64, |_| None),
    (field3, "o_orderdate", false, Date, |_| None),
    (field4, "o_shippriority", false, i32, |_| None),
    (field5, "l_linenumber", false, i32, |_| None),
    (field6, "l_extendedprice", false, SqlDecimal<15, 2>, |_| None),
    (field7, "l_discount", false, SqlDecimal<4, 2>, |_| None),
    (field8, "l_shipdate", false, Date, |_| None)
});
serialize_table_record!(ViewRecord[9]{
    field0["c_custkey"]: i64,
    field1["c_mktsegment"]: SqlString,
    field2["o_orderkey"]: i64,
    field3["o_orderdate"]: Date,
    field4["o_shippriority"]: i32,
    field5["l_linenumber"]: i32,
    field6["l_extendedprice"]: SqlDecimal<15, 2>,
    field7["l_discount"]: SqlDecimal<4, 2>,
    field8["l_shipdate"]: Date
});

const VIEW_SCHEMA: &str = r#"{"name":"v","case_sensitive":false,"fields":[{"name":"c_custkey","case_sensitive":false,"columntype":{"nullable":false,"type":"BIGINT"},"unused":false},{"name":"c_mktsegment","case_sensitive":false,"columntype":{"nullable":false,"precision":-1,"type":"VARCHAR"},"unused":false},{"name":"o_orderkey","case_sensitive":false,"columntype":{"nullable":false,"type":"BIGINT"},"unused":false},{"name":"o_orderdate","case_sensitive":false,"columntype":{"nullable":false,"type":"DATE"},"unused":false},{"name":"o_shippriority","case_sensitive":false,"columntype":{"nullable":false,"type":"INTEGER"},"unused":false},{"name":"l_linenumber","case_sensitive":false,"columntype":{"nullable":false,"type":"INTEGER"},"unused":false},{"name":"l_extendedprice","case_sensitive":false,"columntype":{"nullable":false,"precision":15,"scale":2,"type":"DECIMAL"},"unused":false},{"name":"l_discount","case_sensitive":false,"columntype":{"nullable":false,"precision":4,"scale":2,"type":"DECIMAL"},"unused":false},{"name":"l_shipdate","case_sensitive":false,"columntype":{"nullable":false,"type":"DATE"},"unused":false}],"materialized":true}"#;

/// Builds the circuit and registers `V` as a materialized output, as the generated circuit
/// does. Returns the catalog, which holds `V` alone.
pub fn build(circuit: &mut RootCircuit, state: Q3State) -> Result<Catalog, Error> {
    let mut catalog = Catalog::new();
    let view = view(circuit, state);
    catalog.register_materialized_output_zset_persistent::<_, ViewRecord>(
        Some("q3-trie-view-circuit/v"),
        view,
        VIEW_SCHEMA,
    );
    Ok(catalog)
}

#[cfg(test)]
mod tests;
