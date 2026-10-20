package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

/** A 1-based line and column in the text of a merged-index spec. */
public record SpecPosition(int line, int column) {
    @Override
    public String toString() {
        return this.line + ":" + this.column;
    }
}
