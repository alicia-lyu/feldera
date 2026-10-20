package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import java.util.List;
import java.util.stream.Collectors;

/** A merged-index spec that does not parse or does not validate.
 * The parser reports its first error; the validator reports all of them. */
public class MergedIndexSpecException extends RuntimeException {
    public final List<SpecError> errors;

    public MergedIndexSpecException(List<SpecError> errors) {
        super(errors.stream().map(SpecError::toString).collect(Collectors.joining("\n")));
        if (errors.isEmpty())
            throw new IllegalArgumentException("a spec exception needs at least one error");
        this.errors = List.copyOf(errors);
    }
}
