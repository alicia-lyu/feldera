package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.dbsp.sqlCompiler.compiler.CompilerOptions;
import org.dbsp.sqlCompiler.compiler.DBSPCompiler;

import static org.junit.Assert.assertThrows;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

/** The TPC-H Q3 schema, the COL spec, and helpers shared by the merged-index tests. */
final class MergedIndexFixtures {
    private MergedIndexFixtures() {}

    static final String Q3_SCHEMA = """
            CREATE TABLE Customer (
                c_custkey BIGINT NOT NULL PRIMARY KEY,
                c_mktsegment VARCHAR NOT NULL
            );
            CREATE TABLE Orders (
                o_orderkey BIGINT NOT NULL PRIMARY KEY,
                o_custkey BIGINT NOT NULL,
                o_orderdate DATE NOT NULL,
                o_shippriority INTEGER NOT NULL
            );
            CREATE TABLE Lineitem (
                l_orderkey BIGINT NOT NULL,
                l_linenumber INTEGER NOT NULL,
                l_extendedprice DECIMAL(15, 2) NOT NULL,
                l_discount DECIMAL(4, 2) NOT NULL,
                l_shipdate DATE NOT NULL,
                PRIMARY KEY (l_orderkey, l_linenumber)
            );
            """;

    /** The spec of COL, as in general-definition.md. */
    static final String COL_SPEC = """
            domains {
              custkey    = Customer.c_custkey, Orders.o_custkey;
              orderkey   = Orders.o_orderkey, Lineitem.l_orderkey;
              linenumber = Lineitem.l_linenumber;
            }

            merged_index CustomerOrdersLineitem {
              custkey {
                Customer: *;
                orderkey {
                  Orders: o_orderdate, o_shippriority;
                  linenumber {
                    Lineitem [custkey via Orders]: l_shipdate, l_extendedprice, l_discount;
                  }
                }
              }
            }
            """;

    /** Compiles {@code sql} to its schema, without generating Rust. */
    static DBSPCompiler compile(String sql) {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.throwOnError = true;
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation(sql);
        compiler.getFinalCircuit(false);
        return compiler;
    }

    static ValidatedMergedIndex validate(String sql, String spec) {
        return MergedIndexValidator.validate(MergedIndexSpecParser.parse(spec), compile(sql));
    }

    /** Asserts that validating {@code spec} against {@code sql} reports an error
     * on {@code line} whose message contains {@code fragment}. */
    static void assertInvalid(String sql, String spec, int line, String fragment) {
        MergedIndexSpecException exception = assertThrows(MergedIndexSpecException.class,
                () -> validate(sql, spec));
        assertHasError(exception, line, fragment);
    }

    /** Asserts that parsing {@code spec} fails at {@code line}:{@code column} with {@code fragment}. */
    static void assertSyntaxError(String spec, int line, int column, String fragment) {
        MergedIndexSpecException exception = assertThrows(MergedIndexSpecException.class,
                () -> MergedIndexSpecParser.parse(spec));
        assertTrue(exception.getMessage(), exception.errors.size() == 1);
        SpecError error = exception.errors.get(0);
        assertTrue(exception.getMessage(), error.position().equals(new SpecPosition(line, column)));
        assertTrue(exception.getMessage(), error.message().contains(fragment));
    }

    static void assertHasError(MergedIndexSpecException exception, int line, String fragment) {
        for (SpecError error : exception.errors) {
            if (error.position().line() == line && error.message().contains(fragment))
                return;
        }
        fail("expected an error on line " + line + " containing \"" + fragment + "\", got:\n"
                + exception.getMessage());
    }
}
