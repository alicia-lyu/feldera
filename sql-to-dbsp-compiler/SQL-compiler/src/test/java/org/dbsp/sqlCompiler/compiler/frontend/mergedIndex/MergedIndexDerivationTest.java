package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PlacedIndex;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex.Column;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex.ResolvedPlacement;
import org.dbsp.sqlCompiler.ir.type.DBSPTypeCode;
import org.junit.Test;

import java.util.List;

import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.COL_SPEC;
import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.Q3_SCHEMA;
import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.validate;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertTrue;

/** What the compiler derives from a spec: tags, ids, names, key and payload columns. */
public class MergedIndexDerivationTest {
    private static final String SIBLING_SCHEMA = """
            CREATE TABLE Par (k BIGINT NOT NULL PRIMARY KEY);
            CREATE TABLE Alpha (id BIGINT NOT NULL PRIMARY KEY, k BIGINT NOT NULL, x INT NOT NULL, y INT NOT NULL);
            CREATE TABLE Beta (id BIGINT NOT NULL PRIMARY KEY, k BIGINT NOT NULL, w INT NOT NULL);
            """;

    /** Two sibling nodes below one root node, in the given order. */
    private static String siblings(String first, String second) {
        return """
                domains {
                  root = Par.k, Alpha.k, Beta.k;
                  a = Alpha.id;
                  b = Beta.id;
                }
                merged_index Siblings {
                  root {
                    Par: *;
                """ + first + second + "  }\n}";
    }

    private static final String A_NODE = "    a { Alpha: x, y; }\n";
    private static final String B_NODE = "    b { Beta: w; }\n";

    @Test
    public void colTagsAndIdsAreOneTwoThree() {
        KeyTrie trie = validate(Q3_SCHEMA, COL_SPEC).trie();
        assertEquals(1, trie.domainTag("custkey").orElseThrow());
        assertEquals(2, trie.domainTag("orderkey").orElseThrow());
        assertEquals(3, trie.domainTag("linenumber").orElseThrow());
        assertFalse(trie.domainTag("nationkey").isPresent());

        List<PlacedIndex> placements = trie.placements();
        assertEquals(List.of(1, 2, 3), placements.stream().map(PlacedIndex::id).toList());
        assertEquals(List.of("CustomerByCustkey", "OrdersByCustkeyOrderkey", "LineitemByCustkeyOrderkeyLinenumber"),
                placements.stream().map(PlacedIndex::name).toList());
        assertEquals(List.of("custkey", "orderkey"), placements.get(1).path());
        assertTrue(placements.get(0).isPrimary());
        assertFalse(placements.get(1).isPrimary());
        assertEquals(2, trie.placement("OrdersByCustkeyOrderkey").orElseThrow().id());
    }

    @Test
    public void siblingOrderFollowsTheSpec() {
        KeyTrie aFirst = validate(SIBLING_SCHEMA, siblings(A_NODE, B_NODE)).trie();
        assertEquals(1, aFirst.domainTag("root").orElseThrow());
        assertEquals(2, aFirst.domainTag("a").orElseThrow());
        assertEquals(3, aFirst.domainTag("b").orElseThrow());
        assertEquals(List.of("ParByRoot", "AlphaByRootA", "BetaByRootB"),
                aFirst.placements().stream().map(PlacedIndex::name).toList());

        KeyTrie bFirst = validate(SIBLING_SCHEMA, siblings(B_NODE, A_NODE)).trie();
        assertEquals(2, bFirst.domainTag("b").orElseThrow());
        assertEquals(3, bFirst.domainTag("a").orElseThrow());
        assertEquals(List.of("ParByRoot", "BetaByRootB", "AlphaByRootA"),
                bFirst.placements().stream().map(PlacedIndex::name).toList());
    }

    @Test
    public void domainDeclarationOrderDoesNotSetTags() {
        String reversed = COL_SPEC
                .replace("custkey    = Customer.c_custkey, Orders.o_custkey;",
                        "linenumber = Lineitem.l_linenumber;")
                .replaceFirst("linenumber = Lineitem.l_linenumber;\n}",
                        "custkey    = Customer.c_custkey, Orders.o_custkey;\n}");
        KeyTrie trie = validate(Q3_SCHEMA, reversed).trie();
        assertEquals("linenumber", trie.domains().get(0).name());
        assertEquals(1, trie.domainTag("custkey").orElseThrow());
        assertEquals(3, trie.domainTag("linenumber").orElseThrow());
    }

    @Test
    public void viaResolvesToTheViaRelationsColumn() {
        ResolvedPlacement lineitem = validate(Q3_SCHEMA, COL_SPEC)
                .placement("LineitemByCustkeyOrderkeyLinenumber").orElseThrow();
        List<Column> key = lineitem.keyColumns();
        assertEquals(List.of("Orders.o_custkey", "Lineitem.l_orderkey", "Lineitem.l_linenumber"),
                key.stream().map(column -> column.relation() + "." + column.column()).toList());
        assertEquals(List.of(DBSPTypeCode.INT64, DBSPTypeCode.INT64, DBSPTypeCode.INT32),
                key.stream().map(column -> column.type().code).toList());
        assertTrue(key.stream().noneMatch(column -> column.type().mayBeNull));
    }

    @Test
    public void listedPayloadKeepsItsOrder() {
        ResolvedPlacement lineitem = validate(Q3_SCHEMA, COL_SPEC)
                .placement("LineitemByCustkeyOrderkeyLinenumber").orElseThrow();
        assertEquals(List.of("l_shipdate", "l_extendedprice", "l_discount"), columnNames(lineitem.payloadColumns()));
        assertEquals(DBSPTypeCode.DECIMAL, lineitem.payloadColumns().get(1).type().code);
    }

    @Test
    public void starExpandsInTableOrder() {
        ValidatedMergedIndex col = validate(Q3_SCHEMA, COL_SPEC);
        assertEquals(List.of("c_mktsegment"),
                columnNames(col.placement("CustomerByCustkey").orElseThrow().payloadColumns()));

        ValidatedMergedIndex orders = validate(Q3_SCHEMA,
                "domains { orderkey = Orders.o_orderkey; }\nmerged_index M { orderkey { Orders: *; } }");
        assertEquals(List.of("o_custkey", "o_orderdate", "o_shippriority"),
                columnNames(orders.placement("OrdersByOrderkey").orElseThrow().payloadColumns()));

        String keyInTheMiddle = "CREATE TABLE W (a INT NOT NULL, k BIGINT NOT NULL PRIMARY KEY, b DATE NOT NULL);";
        ValidatedMergedIndex w = validate(keyInTheMiddle,
                "domains { k = W.k; } merged_index M { k { W: *; } }");
        assertEquals(List.of("a", "b"), columnNames(w.placement("WByK").orElseThrow().payloadColumns()));
    }

    @Test
    public void relationNamesMatchSqlCaseInsensitively() {
        String lowercase = COL_SPEC.replace("Customer: *;", "customer: *;");
        ValidatedMergedIndex col = validate(Q3_SCHEMA, lowercase);
        assertTrue(col.placement("customerByCustkey").isPresent());
        assertEquals(List.of("c_mktsegment"),
                columnNames(col.placement("customerByCustkey").orElseThrow().payloadColumns()));
    }

    @Test
    public void camelCaseJoinsSnakeCaseWords() {
        assertEquals("OrderKey", KeyTrie.camelCase("order_key"));
        assertEquals("Custkey", KeyTrie.camelCase("custkey"));
        assertEquals("AB", KeyTrie.camelCase("_a__b_"));
    }

    private static List<String> columnNames(List<Column> columns) {
        return columns.stream().map(Column::column).toList();
    }
}
