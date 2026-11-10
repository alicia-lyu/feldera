package org.dbsp.sqlCompiler.compiler.backend.rust;

import javax.annotation.Nullable;
import java.util.List;
import java.util.regex.Pattern;

/** The Rust identifiers that the generated code defines where a user function name can collide
 * with them, in one place. A user function is called by its bare name from the generated code,
 * and a Rust UDF is imported into the crate root by a glob. So a user function that has the name
 * of a generated item or local variable shadows it, or is shadowed by it, and rustc reports an
 * unrelated error far from the SQL declaration. The compiler rejects such a name instead,
 * when the function is declared. */
public final class GeneratedRustNames {
    private GeneratedRustNames() {}

    /** Name of the generated type whose associated function `build` constructs the circuit.
     * The type lives in a module named after the circuit (see {@link #ENTRY_POINT}), so that
     * several circuits in one crate do not define it twice. The compiler does not otherwise
     * generate Rust items with this name (user types are named after a hash), and a user function
     * cannot clash with a type that has no value. */
    public static final String CIRCUIT_TYPE_NAME = "SqlCircuit";

    /** Name of the circuit parameter of the generated constructor `SqlCircuit::build`. A function
     * with a SQL body is declared inside the constructor, where an item shadows a parameter of the
     * same name, so the parameter must not be a name that a SQL function could take, such as
     * `circuit`. The leading underscores keep it clear of the compiler's other generated names. */
    public static final String CIRCUIT_VARIABLE = "__circuit";

    /** Name of the generated entry point, a free function of the crate root. The module that holds
     * {@link #CIRCUIT_TYPE_NAME} has the same name, which does not clash with it: a module is in
     * the type namespace and a function in the value namespace. So a user function of this name
     * never clashes with the module, and is reserved only because a Rust UDF of this name is
     * hidden by the entry point. */
    public static final String ENTRY_POINT = "circuit";

    /** How a reserved name is matched against a function name. */
    private enum Match {
        /** The whole name. */
        EXACT,
        /** The text followed by one or more digits, as in `stream12`. */
        TEXT_AND_DIGITS,
        /** The text followed by anything, as in `handle_s3`. */
        TEXT_AND_SUFFIX
    }

    private record Reserved(String text, Match match) {
        boolean matches(String name) {
            return switch (this.match) {
                case EXACT -> name.equals(this.text);
                case TEXT_AND_DIGITS -> Pattern.matches(Pattern.quote(this.text) + "[0-9]+", name);
                case TEXT_AND_SUFFIX -> name.startsWith(this.text);
            };
        }
    }

    /** Names that the generated constructor body defines: its parameter, its locals, and the
     * parameters of the closures that it contains. A function with a SQL body is declared in
     * that body, and every user function is called from it. */
    private static final List<Reserved> BODY_NAMES = List.of(
            new Reserved(CIRCUIT_VARIABLE, Match.EXACT),
            // Catalog, output hashes, and the Q3 state of the reconstructed circuit.
            new Reserved("catalog", Match.EXACT),
            new Reserved("hash", Match.EXACT),
            new Reserved("streams", Match.EXACT),
            new Reserved("q3_state", Match.EXACT),
            // Streams: compact names `s<N>`, and `stream<N>` when an operator has none.
            new Reserved("s", Match.TEXT_AND_DIGITS),
            new Reserved("stream", Match.TEXT_AND_DIGITS),
            // Input handles: `handle_s<N>`, and plain `handle` in multi-crate output.
            new Reserved("handle", Match.EXACT),
            new Reserved("handle_", Match.TEXT_AND_SUFFIX),
            // Operator names derived from a hash.
            new Reserved("operator_", Match.TEXT_AND_SUFFIX),
            // Closure parameters and locals of the canonical form: `p<N>`.
            new Reserved("p", Match.TEXT_AND_DIGITS));

    /** Names that the generated crate root defines in the value namespace, where a function lives.
     * A Rust UDF is imported there by a glob, so a crate-root item of the same name hides it.
     * The type `SqlCircuit`, the circuit's module, and the modules `stubs` and `udf` are in the
     * type namespace, so a function of that name does not clash with them (a UDF named `udf`
     * compiles). */
    private static final List<Reserved> CRATE_ROOT_NAMES = List.of(
            new Reserved(ENTRY_POINT, Match.EXACT));

    /** True if the generated constructor body uses this name. */
    public static boolean reservedInConstructorBody(String name) {
        return BODY_NAMES.stream().anyMatch(reserved -> reserved.matches(name));
    }

    /** True if the generated crate root defines this name. */
    public static boolean reservedAtCrateRoot(String name) {
        return CRATE_ROOT_NAMES.stream().anyMatch(reserved -> reserved.matches(name));
    }

    /** Checks the name of a user function.
     *
     * @param name       Name of the function, as it appears in the generated Rust code.
     * @param sqlBodied  True for a function with a SQL body, which the generated code declares
     *                   inside the constructor; false for one implemented in Rust (`udf.rs`),
     *                   which the crate root imports.
     * @return  An error message if the name is reserved, otherwise null. */
    @Nullable
    public static String checkFunctionName(String name, boolean sqlBodied) {
        boolean reserved = reservedInConstructorBody(name) ||
                (!sqlBodied && reservedAtCrateRoot(name));
        if (!reserved)
            return null;
        return "function name `" + name + "` is reserved by the generated Rust code; " +
                "rename the function";
    }
}
