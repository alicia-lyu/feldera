# Feldera I/O adapter library

This crate provides base traits and types to implement input and output
adapters for the Feldera I/O adapter framework.

Catalog materialized handles expose snapshots of maintained relations. Their
`SerBatchReader::batches()` method returns immutable batches whose
`SerBatch::location()` reports memory or file storage without traversing rows.
Snapshots retain the underlying batches; release them before measuring settled
storage after compaction.
