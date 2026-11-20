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
//! `V`'s columns are those of `schema.sql`'s `V`. A line's value carries
//! `l_suppkey` (the reader needs it to keep rows in index order); `V` drops it.
//! There is no catalog registration yet: the harness of step 5 adds it.

use dbsp::{circuit::RootCircuit, utils::Tup7, InputHandle, OrdZSet, Stream};
use feldera_sqllib::{Date, SqlDecimal, SqlString};
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

#[cfg(test)]
mod tests;
