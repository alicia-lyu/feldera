//! The Q14 correctness harness: `V = Part ⋈ Lineitem` maintained by two
//! chained circuits in one DBSP runtime, natively or reconstructed from the
//! MI_P merged index (Cargo feature `merged-index`).
//!
//! It is step 5 of `docs/design/merged-index/new-merged-index.md`. The tests
//! run both modes on the same transactions, and their transcripts of `V` must
//! be byte-identical. The driver (`driver`, the binary) loads dbgen's files
//! and runs TPC-H's refreshes, timing each refresh's phases.

pub mod data;
pub mod driver;
#[cfg(feature = "merged-index")]
pub mod index;
pub mod pipeline;
pub mod workload;

#[cfg(test)]
mod verification;
