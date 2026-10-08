# DBSP physical design: advisor presentation plan

## Goal and deliverables

Prepare a separate 16:9 Beamer deck for a 20–25 minute advisor presentation, with 16 main slides and a short appendix. The audience knows storage systems and LSMs, but not incremental view maintenance (IVM). Earn attention with a clear physical-design question, then explain the complete maintained program and worked changes before discussing representation internals.

The editable LaTeX/TikZ source, presentation PDF, speaker-notes PDF, build instructions, evidence record, and this handoff live in this directory. The deck should build without the sibling reference checkout. No runtime or API changes are part of the presentation work.

## Audience guidance

- Keep the explanation clear and simple; introduce details only after the audience understands the state needed to compute a view change.
- Explain each IVM branch by naming its changed rows, the accumulated rows it reads, and the signed output it produces.
- Mention generality and composability as established theoretical foundations, but do not make them presentation themes.
- Put base storage, including disk backing, on slide 2.
- Present the complete IVM program and required integrators/indexes in two slides, then immediately walk through both joins.
- Introduce immutable batches with integrators. Explain records, nested values, and snapshot internals after the join algorithm.
- Explain how one integrator's trace grows and merges. Skip a general LSM tutorial.
- Explain referenced-batch merging, temporary coexistence, and the verified reclamation condition.
- Center the memory discussion on native accounting and controls. Do not imply measured per-state savings or a strict memory quota.

## Slide sequence

| Slide | Content |
| --- | --- |
| 1 | The physical-design question. Adapt the query-time/update-time backdrop and motivate accumulated state for computing a view change. |
| 2 | Base-table storage, including disk backing. Show primary-key state in a native trace/LSM, recent changes in memory, and immutable indexed files on disk. Explain that “input map” names update semantics rather than a disk-resident conventional map. |
| 3 | The implemented predicate-free three-way join. Show simplified SQL and small Customer, Orders, and Lineitem tables. |
| 4 | The complete logical IVM program, both branches of each join, and output integration. |
| 5 | Required accumulated state and access paths; distinguish base primary-key state from join-specific representations; introduce immutable weighted batches and an integrator's accumulated trace. |
| 6 | First join walkthrough for the simultaneous insertion of Order 43 and its Lineitem. Show each branch's changed and matching rows. |
| 7 | Second join walkthrough and the single resulting new view row. Keep the examples consistent with slide 6. |
| 8 | Records and nested values: `(K, V, weight)`, multiple values per key, multiplicities, and signed changes. |
| 9 | Memory arrays/offsets and corresponding nested groups in immutable indexed files. |
| 10 | Keyed matching reads: connect a changed key to accumulated indexed groups and join output. Qualify optional `fetch_join` as configuration-gated. |
| 11 | One integrator's growing LSM-like trace: successive delta batches, later consolidation, and replacement of source batches. |
| 12 | Before/current snapshots: immutable batch references, shared unchanged data, and pending changes. |
| 13 | Referenced-batch merge ownership before, during, and after a merge, including old snapshots retaining their source references. |
| 14 | Native memory accounting: retained state, temporary/pinned allocations, cache and lookup metadata, and other process allocations. |
| 15 | Actual memory-pressure responses: RSS-triggered spill policy, separate batch-count backpressure, and limits on immediate reclamation. |
| 16 | Connection to merged-index work. Distinguish native behavior, the existing restricted reconstruction prototype, and deferred extensions. |

Appendix material may define full payloads and show customer overwrite/deletion examples. Keep the main narrative focused on the two-join insertion.

## Program and running example

Let `B` be the projected Orders–Customer intermediate and `V` the maintained three-way join:

```text
ΔB = ΔO ⋈ C_current + O_before ⋈ ΔC
ΔV = ΔL ⋈ B_current + L_before ⋈ ΔB
V_current = V_before + ΔV
```

Slide 4 must show every branch and the output integration. Slide 5 identifies the state needed for those reads:

| Collection | Required access |
| --- | --- |
| Customer trace | Customer ID, current state |
| Orders trace | Customer ID, before state |
| Joined Orders–Customer `B` trace | Order ID, current state |
| Lineitem trace | Order ID, before state |
| Materialized `V` | Accumulated output |

Include base input state in the storage inventory, while distinguishing its primary-key role from join-specific access paths. Customer's primary key is Customer ID; Orders' is Order ID; Lineitem's is `(Order ID, Line number)`. The first join needs Orders grouped by Customer ID, and `B` provides the projected, rekeyed relation for the second join. A delayed read selects already accumulated state at the required visibility; it is not another independent integrator. These are logical roles and do not imply a fixed count of independent physical allocations, because consumers can share trace construction.

Use Customer 7, Order 42, and Lineitem `(42,1)` as the initial rows. Insert Order 43 for Customer 7 and Lineitem `(43,1)` together. In the first join, `ΔO ⋈ C_current` reads the accumulated Customer 7 row and emits positive `B43`; `O_before ⋈ ΔC` has no changed Customer and emits nothing. Then `ΔL ⋈ B_current` reads `B43` and emits the new view row once; `L_before ⋈ ΔB` finds no old Lineitem 43 and emits nothing. Explain current and before visibility while pointing to each actual logical matching row. Keep payloads unchanged across slides; full field definitions may live in the appendix. The maintained view is not full TPC-H Q3: omit its filters, aggregation, ranking, and limit. Do not substitute the historical aggregate-first circuit for the implemented program.

Customer overwrite/deletion and other signed cases belong in speaker notes or the appendix. A verifier checks branches against complete before/after evaluation, including concurrent insertion, deletion, overwrite, multiplicity, and cancellation cases.

## Physical-design facts and qualifications

### Base storage and disk backing

“Input map” describes primary-key insert/update/delete behavior. The retained table uses DBSP indexed trace machinery. The inspected call path is `add_input_map_persistent → input_upsert → retained Spine<OrdIndexedZSet> → OrdIndexedWSet / FallbackIndexedWSet → memory-backed or file-backed indexed batches`. `InputUpsert` reads the previous row through a trace cursor and emits signed replacements; `UntimedTraceAppend` retains those changes. With storage configured, insertion and merge policies can create immutable indexed layer files. An overwrite such as Order 42's ship-priority change reads the old row and produces `-old + new`.

Describe this as primary-key table state backed by the native trace/LSM. The previous row remains accessible when retained state includes disk batches. Disk support does not establish optimal disk performance for every update workload, and storage placement does not establish checkpoint durability. Rust type names belong in notes rather than the slide.

### Records, lookup, and batches

Memory indexed batches contain sorted keys, value-group offsets, values, and weights. Immutable files hold corresponding nested key/value groups and tree indexes. The spine organizes retained batches; indexes inside each file are a separate structure. Memory and file batches provide compatible cursor operations, and a trace can contain both. A keyed read can identify the accumulated group matching a changed key. The optional `fetch_join` path fetches delta keys before the join and is configuration-gated; ordinary joins may choose their cursor-driving direction from estimated key counts.

Show one named integrator accepting successive immutable batches. A logical transaction delta can contain multiple physical batches. Inserted batches enter retained state separately; a later merge can replace contributions from multiple deltas with their weighted sum, and cancellation can remove records. Integration does not require retaining each original delta indefinitely. In these root-circuit relation batches, records do not carry per-record logical epoch timestamps; DBSP supports timed batches elsewhere. A merged batch can contain contributions from several epochs, so “before/current” describes visible state rather than uniform provenance of every batch.

### Snapshot sharing and reclamation

Snapshots clone strong references to batches. Merge slots and merge machinery retain input batches while constructing output. On completion the spine installs the output and releases its source references. An old snapshot can keep source memory alive until the last strong owning reference disappears. During this period the merge output and old inputs coexist. Snapshot creation copies references, not records.

Spilling a shared memory batch writes a file-backed copy while readers may keep the original in memory, so the spill itself does not guarantee immediate reclamation. A newly created temporary file batch keeps its file alive through reader ownership; checkpoint-marked and reopened persistent files have distinct deletion behavior. Keep this limited retention qualification separate from checkpoint/recovery design.

### Memory accounting and controls

Count shared allocations once. Relevant resident allocations include base input state; retained join and output state; transaction and in-flight batches; merge-building work; obsolete sources pinned by snapshots; storage cache; resident lookup filters/metadata; and other process allocations. Operator allocation estimates, batch/merge metrics, cache occupancy, and filter memory are diagnostics, not interchangeable measures of RSS. Do not blindly sum per-operator estimates or equate file size with resident memory.

| Mechanism | Verified behavior |
| --- | --- |
| Process RSS controller | Samples resident memory about once per second and changes spill policy relative to configured `max_rss_bytes`. |
| Diagnostic accounting | Reports operator allocation estimates, batch/merge metrics, cache occupancy, and filter memory. These are not RSS. |
| Loose-batch backpressure | Pauses insertion at 128 loose batches to bound merger backlog; this is a count threshold, not a byte quota. |

With storage configured, the raw RSS responses are:

| Raw RSS fraction | Implemented response |
| --- | --- |
| Below 85% | Foreground insertion usually leaves placement to background merging; merged output follows its configured storage threshold. |
| At least 85% | Merge outputs target storage; sufficiently large inserted batches spill. |
| At least 90% | Nonempty inserted batches target storage; loose memory batches can trigger merges below normal count thresholds. |
| At least 95% | Transient per-step batches also target storage. |

Explain hysteresis in speaker notes: effective High persists until raw pressure becomes Low; Critical persists until raw pressure reaches Moderate or Low. The controller is not a strict allocator quota and does not resize the storage cache through this pressure path. Snapshot-pinned memory can persist after a spill or merge completes.

### Merged-index connection

Explain that the existing opt-in RF1/RF2 prototype reconstructs requested ranges into temporary native batches for selected joins. Label this as prototype behavior, not native execution. Its supported workload and excluded update cases must follow the current experiment documentation. Distinguish retained native state and reads from the prototype path, and identify broader update support, direct reconstructed cursors, and comparative performance or memory claims as deferred. No measured gains are established by this deck.

## Evidence, verification, and build acceptance

The companion [source evidence record](SOURCE-EVIDENCE.md) documents the revision, source hashes, working-tree qualifications, configuration, code references, and visual provenance. Source claims should link to pinned GitHub paths at that revision when the files are clean; hashes identify exact working tree content where inspected native-spine or experimental sources are dirty.

Before delivery:

- Confirm the complete program and worked algorithms precede storage internals.
- Verify examples against complete before/after evaluation, covering concurrent insertions, deletion, overwrite, multiplicities, and cancellation.
- Verify base-state disk backing against the concrete type and call chain.
- Compile both PDFs and inspect every rendered slide for legibility, clipping, grammar, consistent payloads, citations, and illustrative-versus-measured labels.
- Keep all figures schematic unless explicitly labeled with measured data.
- Do not regenerate or cite an opt-in generated circuit as native behavior.

The local algebra checker has passed its 729-case enumeration. This is a verification of the documented examples and algebra; it is not a full runtime retest. No generated circuit screenshot is used in this deck, so the deck does not require in-place circuit regeneration.

## Visual reference and provenance

Use `/Users/alicialyu/Local/query_execution_using_MI/advisor-deck/main.tex` as the visual reference. The copied opening backdrop lives at `figures/query-update-backdrop.tex`; later figures are editable native TikZ. Preserve the 16:9 format, Libertine/NewTX typography, ink titles, teal rules, and the opening figure's teal and amber meanings. Record the exact source SHA-256, copied-figure hash, and any local edits in the evidence record.

## Deferred topics

Keep these outside the deck unless a later planning decision adds them:

- Measured per-state or peak-memory models and savings claims.
- Live memory-pressure demonstrations or benchmarks.
- Detailed `fetch_join` tuning, filter parameters, or file-layout tuning.
- Full checkpoint/recovery design beyond necessary retention qualifications.
- Multiworker or distributed execution.
- Broader merged-index update support or comparative performance claims.

## Persistent advisor guidance task

Create `~/.codex/advisor-working-style.md` and add a concise advisor-task pointer to `~/.codex/AGENTS.md`. Collect dated evidence from this conversation, advisor-related history, meeting notes, and the reference deck. Separate explicit preferences, inferred useful practices, and context-dependent project priorities. Preserve clarity and simplicity, earning attention before detail, worked IVM examples, whole-program context before representation internals, avoiding familiar background material, clear structure and central claims, polished grammar, and avoiding the phrases “terminal merged index” and “residual query execution.” Record this deck's limited emphasis on generality/composability while preserving the older generality ambition as context-dependent. Treat the latest supplied AGENTS.md as authoritative, preserve unrelated work, stage specific files, ignore generated artifacts, and fetch/merge the tracking branch before any commit.
