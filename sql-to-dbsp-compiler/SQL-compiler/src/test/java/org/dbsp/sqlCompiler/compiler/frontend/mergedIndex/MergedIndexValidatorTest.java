package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.junit.Test;

import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.COL_SPEC;
import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.Q3_SCHEMA;
import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.assertInvalid;
import static org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexFixtures.validate;
import static org.junit.Assert.assertEquals;

/**
 * One failing case per rule of the "Validation" section of general-definition.md.
 * Most cases change one line of {@link MergedIndexFixtures#COL_SPEC}, whose
 * lines are: 2-4 the domains, 7 {@code merged_index}, 8 the custkey node,
 * 9 Customer, 10 the orderkey node, 11 Orders, 12 the linenumber node, 13 Lineitem.
 */
public class MergedIndexValidatorTest {
    private static void assertColInvalid(String target, String replacement, int line, String fragment) {
        String spec = COL_SPEC.replace(target, replacement);
        assertEquals("the replacement must change the spec", false, spec.equals(COL_SPEC));
        assertInvalid(Q3_SCHEMA, spec, line, fragment);
    }

    @Test
    public void acceptsCol() {
        assertEquals(3, validate(Q3_SCHEMA, COL_SPEC).placements().size());
    }

    // Rule 1: domains

    @Test
    public void rejectsUnknownColumn() {
        assertColInvalid("Customer.c_custkey,", "Customer.c_nokey,", 2, "`Customer` has no column `c_nokey`");
    }

    @Test
    public void rejectsUnknownRelation() {
        assertColInvalid("Lineitem.l_linenumber;", "Lineitem.l_linenumber, Nation.n_key;", 4,
                "unknown table or view `Nation`");
    }

    @Test
    public void rejectsDomainDeclaredTwice() {
        assertColInvalid("linenumber = Lineitem.l_linenumber;", "custkey = Lineitem.l_linenumber;", 4,
                "domain `custkey` is declared twice");
    }

    @Test
    public void rejectsColumnInTwoDomains() {
        assertColInvalid("Lineitem.l_linenumber;", "Lineitem.l_linenumber, Orders.o_orderkey;", 4,
                "column `Orders.o_orderkey` is already in domain `orderkey`");
    }

    @Test
    public void rejectsTwoColumnsOfOneRelationInADomain() {
        assertColInvalid("Lineitem.l_orderkey;", "Lineitem.l_orderkey, Lineitem.l_linenumber;", 3,
                "`Lineitem` has two columns in domain `orderkey`");
    }

    // Rule 2: types agree, and keys are NOT NULL

    @Test
    public void rejectsDomainTypeMismatch() {
        assertColInvalid("Lineitem.l_linenumber;", "Lineitem.l_linenumber, Orders.o_orderdate;", 4,
                "column `Orders.o_orderdate` has type DATE");
    }

    @Test
    public void rejectsNullableKeyColumn() {
        String sql = """
                CREATE TABLE P (p_key BIGINT NOT NULL PRIMARY KEY, v INT);
                CREATE TABLE C (c_key BIGINT NOT NULL PRIMARY KEY, p_key BIGINT, v INT);
                """;
        String spec = """
                domains {
                  p_key = P.p_key, C.p_key;
                  c_key = C.c_key;
                }
                merged_index M {
                  p_key {
                    P: *;
                    c_key {
                      C: v;
                    }
                  }
                }""";
        assertInvalid(sql, spec, 9, "key column `C.p_key` is nullable");
    }

    // Rule 3: fixed width

    @Test
    public void rejectsVariableWidthDomain() {
        String spec = """
                domains {
                  segment = Customer.c_mktsegment;
                }
                merged_index M {
                  segment {
                    Customer: c_custkey;
                  }
                }""";
        assertInvalid(Q3_SCHEMA, spec, 2, "domain `segment` has type VARCHAR");
        assertInvalid(Q3_SCHEMA, spec, 2, "not fixed-width");
    }

    // Rule 4: every path domain resolves

    @Test
    public void rejectsPathDomainWithoutColumnOrVia() {
        assertColInvalid("Lineitem [custkey via Orders]:", "Lineitem:", 13,
                "`Lineitem` has no column in domain `custkey`");
    }

    @Test
    public void rejectsViaWhereRelationHasItsOwnColumn() {
        assertColInvalid("Orders: o_orderdate", "Orders [custkey via Customer]: o_orderdate", 11,
                "`Orders` has its own column in domain `custkey`");
    }

    @Test
    public void rejectsViaDomainOffThePath() {
        assertColInvalid("Customer: *;", "Customer [orderkey via Orders]: c_mktsegment;", 9,
                "via domain `orderkey` of `Customer` is not on the path");
    }

    @Test
    public void rejectsUndeclaredViaDomain() {
        assertColInvalid("[custkey via Orders]", "[custkey via Orders, nationkey via Orders]", 13,
                "via domain `nationkey` is not declared");
    }

    @Test
    public void rejectsTwoViasForOneDomain() {
        assertColInvalid("[custkey via Orders]", "[custkey via Orders, custkey via Orders]", 13,
                "`Lineitem` has two vias for domain `custkey`");
    }

    @Test
    public void rejectsUnknownViaRelation() {
        assertColInvalid("[custkey via Orders]", "[custkey via Nation]", 13, "unknown table or view `Nation`");
    }

    @Test
    public void rejectsViaRelationWithoutColumnInDomain() {
        assertColInvalid("Customer.c_custkey, Orders.o_custkey;", "Customer.c_custkey;", 13,
                "via relation `Orders` has no column in domain `custkey`");
    }

    @Test
    public void rejectsViaRelationNotReferencedByPrimaryKey() {
        assertColInvalid("[custkey via Orders]", "[custkey via Customer]", 13,
                "primary-key column `Customer.c_custkey` of the via relation is in no domain where `Lineitem`");
    }

    // Rule 5: keys are unique

    @Test
    public void rejectsKeyWithoutThePrimaryKey() {
        String spec = """
                domains {
                  orderkey = Orders.o_orderkey, Lineitem.l_orderkey;
                }
                merged_index M {
                  orderkey {
                    Lineitem: l_shipdate;
                  }
                }""";
        assertInvalid(Q3_SCHEMA, spec, 6, "the key of `Lineitem` lacks primary-key column `l_linenumber`");
    }

    @Test
    public void rejectsTableWithoutPrimaryKey() {
        String spec = """
                domains {
                  a = T.a;
                }
                merged_index M {
                  a {
                    T: b;
                  }
                }""";
        assertInvalid("CREATE TABLE T (a INT NOT NULL, b INT);", spec, 6, "`T` has no primary key");
    }

    @Test
    public void rejectsView() {
        String sql = Q3_SCHEMA + "CREATE VIEW W AS SELECT c_custkey, c_mktsegment FROM Customer;";
        String spec = """
                domains {
                  custkey = W.c_custkey;
                }
                merged_index M {
                  custkey {
                    W: c_mktsegment;
                  }
                }""";
        assertInvalid(sql, spec, 6, "`W` has no primary key");
    }

    // Rule 6: the trie is well formed

    @Test
    public void rejectsRepeatedSiblingDomain() {
        String spec = """
                domains { k = Customer.c_custkey; }
                merged_index M {
                  k { Customer: *; }
                  k { Customer: *; }
                }""";
        assertInvalid(Q3_SCHEMA, spec, 4, "sibling nodes repeat domain `k`");
    }

    @Test
    public void rejectsDomainRepeatedOnAPath() {
        String spec = """
                domains { k = Customer.c_custkey; }
                merged_index M {
                  k {
                    k { Customer: *; }
                  }
                }""";
        assertInvalid(Q3_SCHEMA, spec, 4, "domain `k` repeats on the path [k]");
    }

    @Test
    public void rejectsRelationPlacedTwiceAtANode() {
        assertColInvalid("Customer: *;", "Customer: *; Customer: c_mktsegment;", 9,
                "`Customer` is placed twice at node `custkey`");
    }

    @Test
    public void rejectsUndeclaredNodeDomain() {
        assertColInvalid("linenumber {", "lineno {", 12, "node domain `lineno` is not declared");
    }

    @Test
    public void rejectsMoreThan255DomainsAndPlacements() {
        StringBuilder domains = new StringBuilder();
        StringBuilder nodes = new StringBuilder();
        for (int index = 0; index <= KeyTrie.MAX_TAGS; index++) {
            domains.append("d").append(index).append(" = Customer.c_custkey; ");
            nodes.append("d").append(index).append(" { Customer: *; } ");
        }
        String spec = "domains { " + domains + "}\nmerged_index M {\n" + nodes + "}";
        assertInvalid(Q3_SCHEMA, spec, 2, "more than 255 domains");
        assertInvalid(Q3_SCHEMA, spec, 2, "more than 255 placements");
    }

    // Rule 7: payloads are valid

    @Test
    public void rejectsStarWhereKeyIsNotThePrimaryKey() {
        assertColInvalid("Orders: o_orderdate, o_shippriority;", "Orders: *;", 11,
                "`*` requires the key of `Orders` to be exactly its primary key");
    }

    @Test
    public void rejectsUnknownPayloadColumn() {
        assertColInvalid("l_extendedprice, l_discount;", "l_tax;", 13, "`Lineitem` has no column `l_tax`");
    }

    @Test
    public void rejectsKeyColumnInPayload() {
        assertColInvalid("o_orderdate, o_shippriority", "o_orderdate, o_orderkey", 11,
                "payload column `o_orderkey` is a key column");
    }

    @Test
    public void rejectsNullablePayloadColumn() {
        String sql = "CREATE TABLE T (k BIGINT NOT NULL PRIMARY KEY, v INT);";
        assertInvalid(sql, "domains { k = T.k; }\nmerged_index M { k {\nT: v; } }", 3,
                "payload column `v` is nullable");
        assertInvalid(sql, "domains { k = T.k; }\nmerged_index M { k {\nT: *; } }", 3,
                "payload column `v` is nullable");
    }

    @Test
    public void rejectsPayloadTypeWithoutPackedEncoding() {
        String sql = "CREATE TABLE T (k BIGINT NOT NULL PRIMARY KEY, v DOUBLE NOT NULL);";
        assertInvalid(sql, "domains { k = T.k; }\nmerged_index M { k {\nT: v; } }", 3,
                "payload column `v` has type DOUBLE NOT NULL, which has no packed encoding");
    }

    @Test
    public void rejectsTrieWithoutPlacements() {
        assertInvalid(Q3_SCHEMA, "domains { k = Customer.c_custkey; }\nmerged_index M { k { } }", 2,
                "merged index `M` places no relation");
    }

    @Test
    public void rejectsDuplicatePayloadColumn() {
        assertColInvalid("o_orderdate, o_shippriority", "o_orderdate, o_orderdate", 11,
                "payload column `o_orderdate` is listed twice");
    }
}
