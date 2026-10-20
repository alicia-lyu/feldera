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
    /** Two sibling nodes below the root, in the given order. */
    private static String siblings(String first, String second) {
        return """
                domains {
                  custkey  = Customer.c_custkey, Orders.o_custkey;
                  orderkey = Orders.o_orderkey, Lineitem.l_orderkey;
                }
                merged_index Siblings {
                """ + first + second + "}";
    }

    private static final String CUSTKEY_NODE = "  custkey { Customer: *; }\n";
    private static final String ORDERKEY_NODE = "  orderkey { Orders: *; }\n";

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
        KeyTrie customerFirst = validate(Q3_SCHEMA, siblings(CUSTKEY_NODE, ORDERKEY_NODE)).trie();
        assertEquals(1, customerFirst.domainTag("custkey").orElseThrow());
        assertEquals(2, customerFirst.domainTag("orderkey").orElseThrow());
        assertEquals(List.of("CustomerByCustkey", "OrdersByOrderkey"),
                customerFirst.placements().stream().map(PlacedIndex::name).toList());

        KeyTrie ordersFirst = validate(Q3_SCHEMA, siblings(ORDERKEY_NODE, CUSTKEY_NODE)).trie();
        assertEquals(1, ordersFirst.domainTag("orderkey").orElseThrow());
        assertEquals(2, ordersFirst.domainTag("custkey").orElseThrow());
        assertEquals(List.of("OrdersByOrderkey", "CustomerByCustkey"),
                ordersFirst.placements().stream().map(PlacedIndex::name).toList());
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

        ValidatedMergedIndex orders = validate(Q3_SCHEMA, siblings(ORDERKEY_NODE, ""));
        assertEquals(List.of("o_custkey", "o_orderdate", "o_shippriority"),
                columnNames(orders.placement("OrdersByOrderkey").orElseThrow().payloadColumns()));

        String keyInTheMiddle = "CREATE TABLE W (a INT, k BIGINT NOT NULL PRIMARY KEY, b DATE);";
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
