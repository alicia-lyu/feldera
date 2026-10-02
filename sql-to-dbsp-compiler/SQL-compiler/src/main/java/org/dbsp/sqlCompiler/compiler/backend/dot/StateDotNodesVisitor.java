package org.dbsp.sqlCompiler.compiler.backend.dot;

import org.dbsp.sqlCompiler.circuit.operator.DBSPDelayedIntegralOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPIntegrateOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPSimpleOperator;
import org.dbsp.sqlCompiler.circuit.operator.IStateful;
import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.ir.IDBSPOuterNode;
import org.dbsp.util.IndentStream;

/** Presentation of explicit accumulation and compiler-declared state, without changing topology. */
public class StateDotNodesVisitor extends ToDotNodesVisitor {
    public StateDotNodesVisitor(DBSPCompiler compiler, IndentStream stream, int details) {
        super(compiler, stream, details);
    }

    @Override
    String shorten(String operation) {
        return operation;
    }

    @Override
    String getColor(DBSPSimpleOperator operator) {
        return "";
    }

    /** Annotate the node after its existing declaration, including nodes with record ports.
     * The base visitor remains responsible for labels, ports and nested clusters. */
    @Override
    public void pop(IDBSPOuterNode node) {
        if (node instanceof DBSPOperator operator) {
            if (operator instanceof DBSPIntegrateOperator || operator instanceof DBSPDelayedIntegralOperator) {
                this.stream.append(operator.getNodeName(false))
                        .append(" [style=filled fillcolor=\"#dbeafe\" color=\"#2563eb\" ")
                        .append("xlabel=\"[A]\" tooltip=\"Explicit accumulated collection or delayed trace\"]")
                        .newline();
            } else if (operator instanceof IStateful) {
                this.stream.append(operator.getNodeName(false))
                        .append(" [style=filled fillcolor=\"#ede9fe\" color=\"#7c3aed\" ")
                        .append("xlabel=\"[S]\" tooltip=\"Compiler-declared IStateful operator; not an explicit accumulation node\"]")
                        .newline();
            }
        }
        super.pop(node);
    }
}
