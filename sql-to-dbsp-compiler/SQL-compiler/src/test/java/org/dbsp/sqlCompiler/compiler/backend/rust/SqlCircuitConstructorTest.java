package org.dbsp.sqlCompiler.compiler.backend.rust;

import org.dbsp.sqlCompiler.circuit.DBSPCircuit;
import org.dbsp.sqlCompiler.CompilerMain;
import org.dbsp.sqlCompiler.compiler.errors.CompilerMessages;
import org.dbsp.sqlCompiler.compiler.CompilerOptions;
import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.compiler.backend.rust.multi.ProjectDeclarations;
import org.dbsp.sqlCompiler.compiler.sql.tools.BaseSQLTests;
import org.dbsp.sqlCompiler.compiler.visitors.outer.CircuitPostfix;
import org.dbsp.util.IndentStream;
import org.dbsp.util.IndentStreamBuilder;
import org.dbsp.util.Utilities;
import org.junit.Assert;
import org.junit.Test;

import java.io.File;
import java.io.IOException;
import java.io.PrintStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.sql.SQLException;
import java.util.regex.Pattern;

/**
 * The generated crate exports its circuit constructor as {@code <circuit>::SqlCircuit::build}, an
 * associated function of a type inside a module named after the circuit. A free function named
 * {@code build} at the crate root would shadow a user function of that name, which the generated
 * code calls by its bare name and which the crate root imports with a glob. The module keeps
 * several circuits in one crate from defining {@code SqlCircuit} twice.
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

    /** A SQL-bodied function is declared inside the constructor, whose parameter is {@code circuit}. */
    private static final String SQL_FUNCTION_NAMED_LIKE_THE_PARAMETER = """
            CREATE FUNCTION circuit(x INT NOT NULL) RETURNS INT NOT NULL AS x + 1;
            CREATE TABLE T(y INT NOT NULL);
            CREATE VIEW V AS SELECT circuit(y) AS z FROM T;
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

    /** The compiler and the circuit of a program, which is named {@code circuitName}. */
    private record Compiled(DBSPCompiler compiler, DBSPCircuit circuit) {}

    private static Compiled compile(String sql, boolean handles, String circuitName) {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.incrementalize = true;
        options.languageOptions.throwOnError = true;
        options.ioOptions.emitHandles = handles;
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation(sql);
        DBSPCircuit circuit = compiler.getFinalCircuit(false);
        circuit.setName(circuitName);
        return new Compiled(compiler, circuit);
    }

    private static String emit(String sql, boolean handles) {
        return emit(sql, handles, "circuit");
    }

    private static String emit(String sql, boolean handles, String circuitName) {
        Compiled compiled = compile(sql, handles, circuitName);
        DBSPCompiler compiler = compiled.compiler();
        DBSPCircuit circuit = compiled.circuit();
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
            // The type is inside the circuit's module, not at the crate root.
            Assert.assertTrue(hasTopLevelLine(rust, "pub mod circuit {"));
            Assert.assertTrue(rust.contains("    use super::*;"));
            Assert.assertTrue(rust.contains("    pub enum SqlCircuit {}"));
            Assert.assertTrue(rust.contains("    impl SqlCircuit {"));
            Assert.assertFalse(hasTopLevelLine(rust, "pub enum SqlCircuit {}"));
            Assert.assertTrue(rust.contains("        pub fn build(__circuit: &mut RootCircuit"));
            Assert.assertFalse("build must not be a free function",
                    hasTopLevelLine(rust, "pub fn build("));
            Assert.assertTrue(rust.contains("circuit::SqlCircuit::build(circuit)"));
            // The generated code still calls the user's function by its bare name.
            Assert.assertTrue(Pattern.compile("[^:\\w]build\\(\\(\\*p\\d").matcher(rust).find());
        }
    }

    @Test
    public void everyCircuitHasItsOwnModule() {
        // Circuits that share a crate (the compiler's tests concatenate them) each define
        // `SqlCircuit`, so each must sit in a module named after the circuit.
        String first = emit(SQL_FUNCTION_PROGRAM, true, "circuit0");
        String second = emit(SQL_FUNCTION_PROGRAM, true, "circuit1");
        Assert.assertTrue(hasTopLevelLine(first, "pub mod circuit0 {"));
        Assert.assertTrue(hasTopLevelLine(second, "pub mod circuit1 {"));
        for (String rust : new String[] { first, second }) {
            Assert.assertEquals(1, rust.split("pub enum SqlCircuit", -1).length - 1);
            Assert.assertFalse(hasTopLevelLine(rust, "pub enum SqlCircuit {}"));
            Assert.assertFalse(hasTopLevelLine(rust, "impl SqlCircuit {"));
        }
        Assert.assertTrue(first.contains("circuit0::SqlCircuit::build(circuit)"));
        Assert.assertTrue(second.contains("circuit1::SqlCircuit::build(circuit)"));
    }

    @Test
    public void sqlFunctionNamedBuildStaysInsideTheConstructor() {
        // A function with a SQL body is declared inside the constructor, not at the crate root,
        // so it cannot duplicate the constructor's name.
        String rust = emit(SQL_FUNCTION_PROGRAM, true);
        Assert.assertFalse(hasTopLevelLine(rust, "pub fn build("));
        Assert.assertTrue(rust.contains("            pub fn build(x: i32) -> i32 {"));
    }

    @Test
    public void sqlFunctionNamedCircuitDoesNotShadowTheConstructorParameter() {
        // The function is declared inside the constructor. The constructor's parameter has
        // another name, so the calls on the circuit resolve to the parameter, not to the function.
        String rust = emit(SQL_FUNCTION_NAMED_LIKE_THE_PARAMETER, true);
        Assert.assertTrue(rust.contains("            pub fn circuit(x: i32) -> i32 {"));
        Assert.assertTrue(rust.contains("__circuit.add_input_zset"));
        Assert.assertFalse("a method call on `circuit` would resolve to the user's function",
                Pattern.compile("[^_\\w]circuit\\.(add_|recursive|set_|create_|open_|close_)")
                        .matcher(rust).find());
    }

    /** The message of the error that the compiler reports for {@code sql}; fails if there is none. */
    private static String compilationError(String sql) {
        try {
            emit(sql, true);
        } catch (RuntimeException error) {
            return error.getMessage();
        }
        throw new AssertionError("the program compiled, but it should be rejected");
    }

    private static String reservedNameMessage(String name) {
        return "function name `" + name + "` is reserved by the generated Rust code; " +
                "rename the function";
    }

    @Test
    public void sqlFunctionNamedLikeAGeneratedStreamIsRejected() {
        String sql = """
                CREATE FUNCTION "stream12"(x INT NOT NULL) RETURNS INT NOT NULL AS x + 1;
                CREATE TABLE T(y INT NOT NULL);
                CREATE VIEW V AS SELECT "stream12"(y) AS z FROM T;
                """;
        Assert.assertTrue(compilationError(sql).contains(reservedNameMessage("stream12")));
    }

    @Test
    public void rustFunctionNamedLikeTheEntryPointIsRejected() {
        // A Rust UDF is imported into the crate root, where the entry point `circuit` hides it.
        String sql = """
                CREATE FUNCTION circuit(x INT NOT NULL) RETURNS INT NOT NULL;
                CREATE TABLE T(y INT NOT NULL);
                CREATE VIEW V AS SELECT circuit(y) AS z FROM T;
                """;
        Assert.assertTrue(compilationError(sql).contains(reservedNameMessage("circuit")));
    }

    @Test
    public void reservedNamesAreMatchedByTheirRule() {
        // Exact names, a prefix with digits, and a prefix with any suffix.
        for (String reserved : new String[] { "__circuit", "catalog", "hash", "q3_state", "s0",
                "stream12", "p3", "handle", "handle_s3", "operator_ab12" })
            Assert.assertNotNull(reserved, GeneratedRustNames.checkFunctionName(reserved, true));
        // Names that only look close.
        for (String allowed : new String[] { "stream_x", "stream", "s", "sx1", "p", "pad1", "handles",
                "my_hash", "circuit", "udf", "SqlCircuit" })
            Assert.assertNull(allowed, GeneratedRustNames.checkFunctionName(allowed, true));
        // The entry point is reserved at the crate root only, so only for a Rust function.
        Assert.assertNull(GeneratedRustNames.checkFunctionName("circuit", true));
        Assert.assertNotNull(GeneratedRustNames.checkFunctionName("circuit", false));
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
    public void twoCircuitsInOneCrateCompile() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        // Concatenate two circuits into one crate, as BaseSQLTests does. Their user functions
        // have the same names, and the second also has a type named like the circuit.
        Compiled first = compile(SQL_FUNCTION_PROGRAM, true, "circuit0");
        Compiled second = compile(SQL_TYPE_NAMED_LIKE_THE_CIRCUIT, true, "circuit1");
        BaseSQLTests.createEmptyStubs();
        Utilities.createEmptyFile(Paths.get(BaseSQLTests.RUST_DIRECTORY, DBSPCompiler.UDF_FILE_NAME));
        try (PrintStream output = new PrintStream(Files.newOutputStream(Paths.get(BaseSQLTests.TEST_FILE_PATH)))) {
            RustFileWriter writer = new RustFileWriter(new CircuitPostfix(first.compiler()));
            writer.setOutputBuilder(new IndentStream(output));
            writer.add(first.circuit());
            writer.add(second.circuit());
            writer.write(first.compiler());
        }
        Utilities.compileAndCheckRust(BaseSQLTests.RUST_DIRECTORY, true, "");
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
    public void sqlFunctionNamedLikeTheConstructorParameterCompiles() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        compileAndCheck(SQL_FUNCTION_NAMED_LIKE_THE_PARAMETER, "");
    }

    @Test
    public void sqlFunctionNamedCloseToAGeneratedNameCompiles() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        compileAndCheck("""
                CREATE FUNCTION stream_x(x INT NOT NULL) RETURNS INT NOT NULL AS x + 1;
                CREATE TABLE T(y INT NOT NULL);
                CREATE VIEW V AS SELECT stream_x(y) AS z FROM T;
                """, "");
    }

    @Test
    public void sqlTypeNamedLikeTheCircuitCompiles() throws IOException, InterruptedException, SQLException {
        if (BaseSQLTests.skipRust)
            return;
        BaseSQLTests.setupCargoLock();
        compileAndCheck(SQL_TYPE_NAMED_LIKE_THE_CIRCUIT, "");
    }
}
