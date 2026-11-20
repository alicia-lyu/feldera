//! The Q14 view circuit over MI_P, written by hand like `q3-trie-view-circuit`.
//!
//! The view is `V = Part ⋈ Lineitem` on `partkey` (`merged-index-p/schema.sql`),
//! maintained as one DBSP join whose two partner reads come from the merged
//! index (decision of 2026-11-11): `ΔV = ΔL ⋈ P⁺ + L⁻ ⋈ ΔP`, with `P⁺` and `L⁻`
//! the readers of [`merged_index_p::q14`].
//!
//! The inputs are the transaction's driving deltas `ΔP` and `ΔL`, which
//! [`Q14State::collect`] returns. The driver sets them into two input handles
//! ([`TrieInputs`]), each indexed by `partkey`, the join key and the trie's
//! root.
//!
//! `V`'s columns and JSON match the generated circuit's (`../q14.sql`). A
//! line's value carries `l_suppkey` (the reader needs it to keep rows in index
//! order); `V` drops it. There is no `ERROR_VIEW`.

use dbsp::{circuit::RootCircuit, utils::Tup7, Error, InputHandle, OrdZSet, Stream};
use dbsp_adapters::Catalog;
use feldera_sqllib::{Date, SqlDecimal, SqlString, Variant};
use feldera_types::{deserialize_table_record, serialize_table_record};
use merged_index_p::q14::{
    join_lineitems_parts, DrivingDeltas, LineitemByPartkeyVector, PartByPartkeyVector, Q14State,
};

/// A row of `V`: `(p_partkey, p_type, l_orderkey, l_linenumber,
/// l_extendedprice, l_discount, l_shipdate)`.
pub type ViewTuple = Tup7<i64, SqlString, i64, i32, SqlDecimal<15, 2>, SqlDecimal<4, 2>, Date>;

/// The circuit's two input handles. Each takes one whole driving delta,
/// indexed by `partkey`.
#[derive(Clone)]
pub struct TrieInputs {
    pub parts: InputHandle<PartByPartkeyVector>,
    pub lines: InputHandle<LineitemByPartkeyVector>,
}

impl TrieInputs {
    /// Hands one transaction's driving deltas to the circuit's one worker.
    /// `Input` emits them on the transaction's next step.
    pub fn set(&self, deltas: DrivingDeltas) {
        self.parts.set_for_worker(0, deltas.parts);
        self.lines.set_for_worker(0, deltas.lines);
    }
}

/// Builds the join in `circuit` and returns the input handles and the stream
/// of `V`'s changes.
pub fn view(
    circuit: &mut RootCircuit,
    state: Q14State,
) -> (TrieInputs, Stream<RootCircuit, OrdZSet<ViewTuple>>) {
    let (parts_by_partkey, parts) = circuit.add_input_stream::<PartByPartkeyVector>();
    let (lines_by_partkey, lines) = circuit.add_input_stream::<LineitemByPartkeyVector>();

    // A line's value is `(l_suppkey, l_orderkey, l_linenumber,
    // l_extendedprice, l_discount, l_shipdate)`; `V` drops `l_suppkey`.
    let view = join_lineitems_parts(
        &lines_by_partkey,
        &parts_by_partkey,
        state,
        |partkey, line, part| -> ViewTuple {
            Tup7::new(
                partkey.0,
                part.0.clone(),
                line.1,
                line.2,
                line.3,
                line.4,
                line.5,
            )
        },
    );
    (TrieInputs { parts, lines }, view)
}

/// `V` as the catalog registers it: the same record and relation as the
/// generated circuit's.
#[derive(Clone, Debug, Eq, PartialEq, Default, PartialOrd, Ord)]
pub struct ViewRecord {
    field0: i64,
    field1: SqlString,
    field2: i64,
    field3: i32,
    field4: SqlDecimal<15, 2>,
    field5: SqlDecimal<4, 2>,
    field6: Date,
}

impl From<ViewRecord> for ViewTuple {
    fn from(t: ViewRecord) -> Self {
        Tup7::new(
            t.field0, t.field1, t.field2, t.field3, t.field4, t.field5, t.field6,
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
        }
    }
}

deserialize_table_record!(ViewRecord["v", Variant, 7] {
    (field0, "p_partkey", false, i64, |_| None),
    (field1, "p_type", false, SqlString, |_| None),
    (field2, "l_orderkey", false, i64, |_| None),
    (field3, "l_linenumber", false, i32, |_| None),
    (field4, "l_extendedprice", false, SqlDecimal<15, 2>, |_| None),
    (field5, "l_discount", false, SqlDecimal<4, 2>, |_| None),
    (field6, "l_shipdate", false, Date, |_| None)
});
serialize_table_record!(ViewRecord[7]{
    field0["p_partkey"]: i64,
    field1["p_type"]: SqlString,
    field2["l_orderkey"]: i64,
    field3["l_linenumber"]: i32,
    field4["l_extendedprice"]: SqlDecimal<15, 2>,
    field5["l_discount"]: SqlDecimal<4, 2>,
    field6["l_shipdate"]: Date
});

const VIEW_SCHEMA: &str = r#"{"name":"v","case_sensitive":false,"fields":[{"name":"p_partkey","case_sensitive":false,"columntype":{"nullable":false,"type":"BIGINT"},"unused":false},{"name":"p_type","case_sensitive":false,"columntype":{"nullable":false,"precision":-1,"type":"VARCHAR"},"unused":false},{"name":"l_orderkey","case_sensitive":false,"columntype":{"nullable":false,"type":"BIGINT"},"unused":false},{"name":"l_linenumber","case_sensitive":false,"columntype":{"nullable":false,"type":"INTEGER"},"unused":false},{"name":"l_extendedprice","case_sensitive":false,"columntype":{"nullable":false,"precision":15,"scale":2,"type":"DECIMAL"},"unused":false},{"name":"l_discount","case_sensitive":false,"columntype":{"nullable":false,"precision":4,"scale":2,"type":"DECIMAL"},"unused":false},{"name":"l_shipdate","case_sensitive":false,"columntype":{"nullable":false,"type":"DATE"},"unused":false}],"materialized":true}"#;

/// Builds the circuit and registers `V` as a materialized output, as the
/// generated circuit does. Returns the catalog, which holds `V` alone, and the
/// input handles.
pub fn build(circuit: &mut RootCircuit, state: Q14State) -> Result<(Catalog, TrieInputs), Error> {
    let mut catalog = Catalog::new();
    let (inputs, view) = view(circuit, state);
    catalog.register_materialized_output_zset_persistent::<_, ViewRecord>(
        Some("q14-trie-view-circuit/v"),
        view,
        VIEW_SCHEMA,
    );
    Ok((catalog, inputs))
}

#[cfg(test)]
mod tests;
