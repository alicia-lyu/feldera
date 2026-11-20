//! The Q14 correctness harness: `V = Part ⋈ Lineitem` maintained by two
//! chained circuits in one DBSP runtime, natively or reconstructed from the
//! MI_P merged index (Cargo feature `merged-index`).
//!
//! It is step 5 of `docs/design/merged-index/new-merged-index.md`, reduced to
//! correctness: both modes run the same transactions, and their transcripts
//! of `V` must be byte-identical. Measurement is a later step.

pub mod data;
#[cfg(feature = "merged-index")]
pub mod index;
pub mod pipeline;

#[cfg(test)]
mod verification;
