package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.AllColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.ColumnRef;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Domain;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Identifier;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Node;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PayloadColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Placement;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Via;
import org.junit.Test;

import java.util.List;

import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.COL_SPEC;
import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.assertSyntaxError;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;

/** Syntax of merged-index specs. */
public class MergedIndexSpecParserTest {
    @Test
    public void parsesCol() {
        KeyTrie trie = MergedIndexSpecParser.parse(COL_SPEC);
        assertEquals("CustomerOrdersLineitem", trie.name());

        List<Domain> domains = trie.domains();
        assertEquals(List.of("custkey", "orderkey", "linenumber"), domains.stream().map(Domain::name).toList());
        ColumnRef orderCustomer = domains.get(0).columns().get(1);
        assertEquals("Orders", orderCustomer.relation());
        assertEquals("o_custkey", orderCustomer.column());
        assertEquals(new SpecPosition(2, 36), orderCustomer.position());

        Node custkey = trie.children().get(0);
        assertEquals("custkey", custkey.domain());
        assertEquals(new SpecPosition(8, 3), custkey.position());
        Placement customer = custkey.placements().get(0);
        assertEquals("Customer", customer.relation());
        assertTrue(customer.payload() instanceof AllColumns);

        Node orderkey = custkey.children().get(0);
        Placement orders = orderkey.placements().get(0);
        assertEquals(List.of("o_orderdate", "o_shippriority"), payloadNames(orders));
        assertTrue(orders.vias().isEmpty());

        Node linenumber = orderkey.children().get(0);
        Placement lineitem = linenumber.placements().get(0);
        assertEquals(List.of(new Via("custkey", "Orders", new SpecPosition(13, 19))), lineitem.vias());
        assertEquals(List.of("l_shipdate", "l_extendedprice", "l_discount"), payloadNames(lineitem));
        assertTrue(linenumber.children().isEmpty());
    }

    @Test
    public void skipsCommentsAndWhitespace() {
        KeyTrie trie = MergedIndexSpecParser.parse("""
                -- a leading comment
                domains{k=T.a;}-- trailing
                merged_index M {
                  k { T : * ; } -- after a node
                }
                -- at the end""");
        assertEquals("M", trie.name());
        assertEquals("T", trie.children().get(0).placements().get(0).relation());
    }

    @Test
    public void acceptsAnEmptyTrieAndSeveralVias() {
        assertTrue(MergedIndexSpecParser.parse("domains {} merged_index M {}").children().isEmpty());
        Placement placement = MergedIndexSpecParser.parse(
                "domains {} merged_index M { a { b { T [a via U, b via V]: x; } } }")
                .children().get(0).children().get(0).placements().get(0);
        assertEquals(List.of("a", "b"), placement.vias().stream().map(Via::domain).toList());
    }

    @Test
    public void rejectsEmptyInput() {
        assertSyntaxError("", 1, 1, "expected `domains`, found end of input");
    }

    @Test
    public void rejectsMissingSemicolon() {
        assertSyntaxError("""
                domains { k = T.a }
                merged_index M {}""", 1, 19, "expected `;`, found `}`");
    }

    @Test
    public void rejectsUnknownKeyword() {
        assertSyntaxError("domains {} merged_view M {}", 1, 12, "expected `merged_index`, found `merged_view`");
    }

    @Test
    public void rejectsMissingVia() {
        assertSyntaxError("domains {} merged_index M { k { T [k from U]: *; } }", 1, 38, "expected `via`");
    }

    @Test
    public void rejectsPlacementAfterChild() {
        assertSyntaxError("""
                domains {}
                merged_index M {
                  a {
                    b { U: *; }
                    T: *;
                  }
                }""", 5, 5, "placement `T` follows a child node");
    }

    @Test
    public void rejectsUnexpectedCharacter() {
        assertSyntaxError("domains { k = T.a; }\nmerged_index M { k { T: \"a\"; } }", 2, 25, "unexpected character");
    }

    @Test
    public void rejectsEmptyPayloadAndTrailingInput() {
        assertSyntaxError("domains {} merged_index M { k { T: ; } }", 1, 36, "expected a name, found `;`");
        assertSyntaxError("domains {} merged_index M {} extra", 1, 30, "expected end of input");
    }

    private static List<String> payloadNames(Placement placement) {
        return ((PayloadColumns) placement.payload()).columns().stream().map(Identifier::name).toList();
    }
}
