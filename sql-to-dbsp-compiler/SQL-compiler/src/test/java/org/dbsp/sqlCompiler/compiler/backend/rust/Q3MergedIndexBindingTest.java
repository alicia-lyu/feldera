package org.dbsp.sqlCompiler.compiler.backend.rust;

import com.beust.jcommander.JCommander;
import org.dbsp.sqlCompiler.circuit.DBSPCircuit;
import org.dbsp.sqlCompiler.compiler.CompilerOptions;
import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.compiler.errors.InternalCompilerError;
import org.dbsp.sqlCompiler.compiler.backend.rust.multi.ProjectDeclarations;
import org.dbsp.sqlCompiler.compiler.visitors.outer.CircuitPostfix;
import org.dbsp.util.IndentStreamBuilder;
import org.junit.Test;

import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertThrows;
import static org.junit.Assert.assertTrue;

/**
 * The specialized runtime must never be bound to a different SQL query. The tables are keyless:
 * the view circuit receives the net delta of the base tables, which live in another circuit.
 */
public class Q3MergedIndexBindingTest {
    private static final String QUERY = """
            CREATE TABLE Customer (
                c_custkey BIGINT NOT NULL,
                c_mktsegment VARCHAR NOT NULL
            );
            CREATE TABLE Orders (
                o_orderkey BIGINT NOT NULL,
                o_custkey BIGINT NOT NULL,
                o_orderdate DATE NOT NULL,
                o_shippriority INTEGER NOT NULL
            );
            CREATE TABLE Lineitem (
                l_orderkey BIGINT NOT NULL,
                l_linenumber INTEGER NOT NULL,
                l_extendedprice DECIMAL(15, 2) NOT NULL,
                l_discount DECIMAL(4, 2) NOT NULL,
                l_shipdate DATE NOT NULL
            );
            CREATE MATERIALIZED VIEW V AS
            SELECT c.c_custkey, c.c_mktsegment,
                   o.o_orderkey, o.o_orderdate, o.o_shippriority,
                   l.l_linenumber, l.l_extendedprice, l.l_discount, l.l_shipdate
            FROM Customer AS c
            JOIN Orders AS o ON c.c_custkey = o.o_custkey
            JOIN Lineitem AS l ON o.o_orderkey = l.l_orderkey;
            """;

    private static DBSPCircuit compile(String sql) {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.incrementalize = true;
        options.languageOptions.throwOnError = true;
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation(sql);
        return compiler.getFinalCircuit(false);
    }

    private static String emit(boolean specialized) {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.incrementalize = true;
        options.languageOptions.throwOnError = true;
        options.ioOptions.q3MergedIndex = specialized;
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation(QUERY);
        DBSPCircuit circuit = compiler.getFinalCircuit(false);
        IndentStreamBuilder builder = new IndentStreamBuilder();
        new ToRustVisitor(compiler, builder, circuit.metadata, new ProjectDeclarations(),
                new CircuitPostfix(compiler)).apply(circuit);
        return builder.toString();
    }

    @Test
    public void bindsBaseline() {
        assertNotNull(Q3MergedIndexBinding.bind(compile(QUERY)));
    }

    @Test
    public void emitsSpecializedJoinsOnlyWhenSelected() {
        String nativeRust = emit(false);
        String specializedRust = emit(true);
        assertTrue(nativeRust.contains(".join_index_balanced("));
        assertTrue(nativeRust.contains(".join_balanced("));
        assertFalse(nativeRust.contains("merged_index_col::q3"));
        assertTrue(specializedRust.contains("q3_state: merged_index_col::q3::Q3State"));
        assertTrue(specializedRust.contains("merged_index_col::q3::join_orders_customers("));
        assertTrue(specializedRust.contains("merged_index_col::q3::join_lineitems_orders("));
        assertFalse(specializedRust.contains(".join_index_balanced("));
        assertFalse(specializedRust.contains(".join_balanced("));
    }

    @Test
    public void rejectsKeyedBaseTables() {
        // Keyed tables compile to input maps, whose projections differ from the supported ones.
        String keyed = QUERY.replace("c_custkey BIGINT NOT NULL,", "c_custkey BIGINT NOT NULL PRIMARY KEY,");
        assertThrows(InternalCompilerError.class, () -> Q3MergedIndexBinding.bind(compile(keyed)));
    }

    @Test
    public void rejectsChangedSemantics() {
        String[] changed = {
                QUERY.replace("c.c_custkey = o.o_custkey", "c.c_custkey = o.o_orderkey"),
                QUERY.replace("o.o_orderkey = l.l_orderkey", "o.o_custkey = l.l_orderkey"),
                QUERY.replace("o.o_orderkey = l.l_orderkey;",
                        "o.o_orderkey = l.l_orderkey WHERE l.l_linenumber > 0;"),
                QUERY.replace("o.o_shippriority,", "o.o_shippriority + 1,"),
                QUERY.replace("CREATE MATERIALIZED VIEW V", "CREATE VIEW V")
        };
        for (String sql : changed) {
            assertThrows(InternalCompilerError.class, () -> Q3MergedIndexBinding.bind(compile(sql)));
        }
    }

    @Test
    public void rejectsIncompatibleOutputModes() {
        CompilerOptions options = new CompilerOptions();
        JCommander.newBuilder().addObject(options).build().parse(
                "query.sql", "-i", "--q3-merged-index", "--handles");
        assertFalse(options.ioOptions.validate(new DBSPCompiler(options)));
    }
}
