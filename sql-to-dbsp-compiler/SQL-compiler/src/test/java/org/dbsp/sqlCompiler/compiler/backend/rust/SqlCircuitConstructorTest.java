package org.dbsp.sqlCompiler.compiler.backend.rust;

import org.dbsp.sqlCompiler.circuit.DBSPCircuit;
import org.dbsp.sqlCompiler.CompilerMain;
import org.dbsp.sqlCompiler.compiler.errors.CompilerMessages;
import org.dbsp.sqlCompiler.compiler.CompilerOptions;
import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.compiler.backend.rust.multi.ProjectDeclarations;
import org.dbsp.sqlCompiler.compiler.sql.tools.BaseSQLTests;
import org.dbsp.sqlCompiler.compiler.visitors.outer.CircuitPostfix;
import org.dbsp.util.IndentStreamBuilder;
import org.dbsp.util.Utilities;
import org.junit.Assert;
import org.junit.Test;

import java.io.File;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.sql.SQLException;
import java.util.regex.Pattern;

/**
 * The generated crate exports its circuit constructor as {@code SqlCircuit::build}, an associated
 * function. A free function named {@code build} at the crate root would shadow a user function
 * of that name, which the generated code calls by its bare name and which the crate root
 * imports with a glob.
 */
public class SqlCircuitConstructorTest {
    private static final String RUST_UDF_PROGRAM = """
            CREATE FUNCTION build(x INT NOT NULL) RETURNS INT NOT NULL;
            CREATE TABLE T(y INT NOT NULL);
            CREATE VIEW V AS SELECT build(y) AS z FROM T;
            """;

    private static final String SQL_FUNCTION_PROGRAM = """
            CREATE FUNCTION build(x INT NOT NULL) RETURNS INT NOT NULL AS x + 1;
            CREATE TABLE T(y INT NOT NULL);
            CREATE VIEW V AS SELECT build(y) AS z FROM T;
            """;

    private static final String SQL_TYPE_NAMED_LIKE_THE_CIRCUIT = """
            CREATE TYPE SqlCircuit AS (a INT NOT NULL);
            CREATE TABLE T(x SqlCircuit NOT NULL, y INT NOT NULL);
            CREATE VIEW V AS SELECT x, y FROM T;
            """;

    /** The implementation of the program's Rust function {@code build}. */
    private static final String RUST_UDF_BUILD = """
            use feldera_sqllib::*;

            pub fn build(x: i32) -> Result<i32, Box<dyn std::error::Error>> {
                Ok(x + 1)
            }
            """;

    private static String emit(String sql, boolean handles) {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.incrementalize = true;
        options.languageOptions.throwOnError = true;
        options.ioOptions.emitHandles = handles;
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation(sql);
        DBSPCircuit circuit = compiler.getFinalCircuit(false);
        IndentStreamBuilder builder = new IndentStreamBuilder();
        new ToRustVisitor(compiler, builder, circuit.metadata, new ProjectDeclarations(),
                new CircuitPostfix(compiler)).apply(circuit);
        return builder.toString();
    }

    /** True if the Rust source has a line that starts, with no indentation, with the prefix. */
    private static boolean hasTopLevelLine(String rust, String prefix) {
        return Pattern.compile("^" + Pattern.quote(prefix), Pattern.MULTILINE).matcher(rust).find();
    }

    @Test
    public void constructorIsAnAssociatedFunction() {
        for (boolean handles : new boolean[] { false, true }) {
            String rust = emit(RUST_UDF_PROGRAM, handles);
            Assert.assertTrue(hasTopLevelLine(rust, "pub enum SqlCircuit {}"));
            Assert.assertTrue(hasTopLevelLine(rust, "impl SqlCircuit {"));
            Assert.assertTrue(rust.contains("    pub fn build(circuit: &mut RootCircuit"));
            Assert.assertFalse("build must not be a free function",
                    hasTopLevelLine(rust, "pub fn build("));
            Assert.assertTrue(rust.contains("SqlCircuit::build(circuit)"));
            // The generated code still calls the user's function by its bare name.
            Assert.assertTrue(Pattern.compile("[^:\\w]build\\(\\(\\*p\\d").matcher(rust).find());
        }
    }

    @Test
    public void sqlFunctionNamedBuildStaysInsideTheConstructor() {
        // A function with a SQL body is declared inside the constructor, not at the crate root,
        // so it cannot duplicate the constructor's name.
        String rust = emit(SQL_FUNCTION_PROGRAM, true);
        Assert.assertFalse(hasTopLevelLine(rust, "pub fn build("));
        Assert.assertTrue(rust.contains("        pub fn build(x: i32) -> i32 {"));
    }

    @Test
    public void userTypesCannotBeNamedLikeTheCircuit() {
        // User types are emitted under a hash of their definition, so no SQL type can produce a
        // Rust item named SqlCircuit.
        String rust = emit(SQL_TYPE_NAMED_LIKE_THE_CIRCUIT, true);
        Assert.assertEquals(1, rust.split("enum SqlCircuit", -1).length - 1);
        Assert.assertFalse(rust.contains("struct SqlCircuit"));
        Assert.assertFalse(rust.contains("type SqlCircuit"));
    }

    /** Compiles {@code sql} into the shared test crate, with {@code udf} as its {@code udf.rs}, and
     * type-checks the crate. */
    private static void compileAndCheck(String sql, String udf) throws IOException, InterruptedException, SQLException {
        File script = BaseSQLTests.createInputScript(sql);
        Path udfFile = Paths.get(BaseSQLTests.RUST_DIRECTORY, DBSPCompiler.UDF_FILE_NAME);
        try {
            Files.writeString(udfFile, udf, StandardCharsets.UTF_8);
            CompilerMessages messages = CompilerMain.execute(
                    "--handles", "-i", "--alltables", "-q", "--ignoreOrder",
                    "-o", BaseSQLTests.TEST_FILE_PATH, script.getPath());
            messages.print();
            Assert.assertEquals(0, messages.errorCount());
            // Handles mode keeps the adapters crate out of the build.
            Utilities.compileAndCheckRust(BaseSQLTests.RUST_DIRECTORY, true, "");
        } finally {
            // Leave the shared crate as the other tests expect it.
            Utilities.createEmptyFile(udfFile);
            BaseSQLTests.createEmptyStubs();
        }
    }

    @Test
    public void rustFunctionNamedBuildCompilesAndIsCalled() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        compileAndCheck(RUST_UDF_PROGRAM, RUST_UDF_BUILD);
    }

    @Test
    public void sqlFunctionNamedBuildCompiles() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        compileAndCheck(SQL_FUNCTION_PROGRAM, "");
    }

    @Test
    public void sqlTypeNamedLikeTheCircuitCompiles() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        compileAndCheck(SQL_TYPE_NAMED_LIKE_THE_CIRCUIT, "");
    }
}
