package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.function.BiConsumer;

/**
 * The key trie of a merged index, as written in its spec.
 *
 * <p>This mirrors {@code KeyTrie} in {@code crates/dbsp/src/trace/merged_index/trie.rs}.
 * It stores only what the spec says, plus the spec position of each element
 * for error messages. Domain tags, placement ids, paths, and names are not
 * stored: each query recomputes them with a pre-order walk, visiting a node's
 * placements before its children, and children in the order they are written.
 *
 * <p>Relation and column names are kept as written in the spec. Key columns
 * and payload columns need the schema; {@link MergedIndexValidator} resolves them.
 */
public record KeyTrie(String name, List<Domain> domains, List<Node> children, SpecPosition position) {
    /** The most domains and the most placements a trie may have.
     * Tags and ids are one byte each, and tag 0 is reserved for the terminator. */
    public static final int MAX_TAGS = 255;

    /** A name in the spec, with its position. */
    public record Identifier(String name, SpecPosition position) {}

    /** A column of a relation, {@code Relation.column} in the spec. */
    public record ColumnRef(String relation, String column, SpecPosition position) {}

    /** Columns that join on equal values, and so share one position in the order. */
    public record Domain(String name, List<ColumnRef> columns, SpecPosition position) {}

    /** Takes a key value for {@code domain} from the column of {@code relation}
     * in that domain, because the placed relation has no such column. */
    public record Via(String domain, String relation, SpecPosition position) {}

    /** What a placement stores besides its key. Column names only. */
    public sealed interface Payload permits AllColumns, PayloadColumns {}

    /** {@code *}: every non-key column, in table order. A primary index. */
    public record AllColumns() implements Payload {}

    /** Exactly these columns, in this order. A secondary index. */
    public record PayloadColumns(List<Identifier> columns) implements Payload {}

    /** An index on {@code relation}, placed at the node that encloses it. */
    public record Placement(String relation, List<Via> vias, Payload payload, SpecPosition position) {}

    /** A node of the trie. The edge into the node is the domain {@code domain}. */
    public record Node(String domain, List<Placement> placements, List<Node> children, SpecPosition position) {}

    /** A placement together with what the pre-order walk knows about it.
     *
     * @param id   The placement's visit position in the walk, plus 1.
     * @param path The domain names on the way from the root to the placement's node. */
    public record PlacedIndex(int id, List<String> path, Placement placement) {
        /** {@code <Relation>By<Path>}, with each path domain in CamelCase. */
        public String name() {
            StringBuilder name = new StringBuilder(this.placement.relation()).append("By");
            for (String domain : this.path)
                name.append(camelCase(domain));
            return name.toString();
        }

        /** A primary index carries all non-key columns of its relation. */
        public boolean isPrimary() {
            return this.placement.payload() instanceof AllColumns;
        }
    }

    /** Every placement in pre-order. */
    public List<PlacedIndex> placements() {
        List<PlacedIndex> placed = new ArrayList<>();
        visitPreorder(this.children, new ArrayList<>(), (path, node) -> {
            for (Placement placement : node.placements())
                placed.add(new PlacedIndex(placed.size() + 1, List.copyOf(path), placement));
        });
        return placed;
    }

    /** The tag of a domain: its position of first appearance in the walk, plus 1.
     * Empty if no node uses the domain. */
    public OptionalInt domainTag(String domain) {
        List<String> appearances = new ArrayList<>();
        visitPreorder(this.children, new ArrayList<>(), (path, node) -> {
            if (!appearances.contains(node.domain()))
                appearances.add(node.domain());
        });
        int position = appearances.indexOf(domain);
        return position < 0 ? OptionalInt.empty() : OptionalInt.of(position + 1);
    }

    /** The placement with the given derived name, such as {@code OrdersByCustkeyOrderkey}. */
    public Optional<PlacedIndex> placement(String name) {
        return this.placements().stream().filter(placed -> placed.name().equals(name)).findFirst();
    }

    /** The declared domain with this name. */
    public Optional<Domain> domain(String name) {
        return this.domains.stream().filter(domain -> domain.name().equals(name)).findFirst();
    }

    /** Calls {@code visit} on every node in {@code nodes} and below, in pre-order.
     * The first argument is the node's path, which ends with its own domain. */
    static void visitPreorder(List<Node> nodes, List<String> path, BiConsumer<List<String>, Node> visit) {
        for (Node node : nodes) {
            path.add(node.domain());
            visit.accept(path, node);
            visitPreorder(node.children(), path, visit);
            path.remove(path.size() - 1);
        }
    }

    /** {@code custkey} becomes {@code Custkey}, and {@code order_key} becomes {@code OrderKey}. */
    static String camelCase(String snakeCase) {
        StringBuilder camel = new StringBuilder();
        for (String word : snakeCase.split("_")) {
            if (word.isEmpty())
                continue;
            camel.append(Character.toUpperCase(word.charAt(0))).append(word.substring(1));
        }
        return camel.toString();
    }
}
