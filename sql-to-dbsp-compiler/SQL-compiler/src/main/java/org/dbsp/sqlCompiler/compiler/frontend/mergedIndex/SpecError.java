package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

/** A problem in a merged-index spec, at a position of the spec text.
 * The spec is not the SQL program, so this does not use the compiler's
 * {@code SourcePositionRange}, which always refers to the SQL file. */
public record SpecError(SpecPosition position, String message) {
    @Override
    public String toString() {
        return this.position + ": " + this.message;
    }
}
