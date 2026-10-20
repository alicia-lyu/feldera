package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PlacedIndex;
import org.dbsp.sqlCompiler.ir.type.DBSPType;

import java.util.List;
import java.util.Optional;

/**
 * A key trie that passed validation, with what only the schema can tell:
 * each placement's key and payload columns and their types.
 * Tags, ids, and names are still derived from {@link #trie}.
 *
 * @param placements One per placement of {@code trie}, in pre-order.
 */
public record ValidatedMergedIndex(KeyTrie trie, List<ResolvedPlacement> placements) {
    /** A column of the SQL program. {@code relation} is spelled as in the spec,
     * {@code column} as in the schema. */
    public record Column(String relation, String column, DBSPType type) {}

    /**
     * @param keyColumns     One per domain of the placement's path, in path order.
     *                       A column taken through a {@code via} names the via relation.
     * @param payloadColumns The payload, in payload order. For {@code *}, all
     *                       non-key columns of the relation, in table order.
     */
    public record ResolvedPlacement(PlacedIndex placed, List<Column> keyColumns, List<Column> payloadColumns) {}

    /** The placement with the given derived name, such as {@code OrdersByCustkeyOrderkey}. */
    public Optional<ResolvedPlacement> placement(String name) {
        return this.placements.stream().filter(resolved -> resolved.placed().name().equals(name)).findFirst();
    }
}
