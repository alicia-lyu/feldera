package org.dbsp.sqlCompiler.compiler.backend.rust;

import org.dbsp.sqlCompiler.circuit.DBSPCircuit;
import org.dbsp.sqlCompiler.circuit.operator.DBSPJoinBaseOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPJoinIndexOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPJoinOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPMapIndexOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPSinkOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPSourceBaseOperator;
import org.dbsp.sqlCompiler.compiler.frontend.parser.SqlCreateView;
import org.dbsp.sqlCompiler.compiler.InputColumnMetadata;
import org.dbsp.sqlCompiler.compiler.TableMetadata;
import org.dbsp.sqlCompiler.compiler.errors.InternalCompilerError;
import org.dbsp.sqlCompiler.ir.expression.DBSPBaseTupleExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPCloneExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPClosureExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPDerefExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPFieldExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPSomeExpression;
import org.dbsp.sqlCompiler.ir.expression.DBSPVariablePath;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.Objects;

/** Binds the experimental Q3 implementation to the exact, supported circuit semantics. */
final class Q3MergedIndexBinding {
    final DBSPJoinBaseOperator ordersCustomers;
    final DBSPJoinBaseOperator lineitemsOrders;

    private Q3MergedIndexBinding(DBSPJoinBaseOperator ordersCustomers,
                                 DBSPJoinBaseOperator lineitemsOrders) {
        this.ordersCustomers = ordersCustomers;
        this.lineitemsOrders = lineitemsOrders;
    }

    static Q3MergedIndexBinding bind(DBSPCircuit circuit) {
        if (!circuit.incremental)
            throw unsupported("an incremental circuit is required");

        DBSPSourceBaseOperator customer = source(circuit, "customer",
                "c_custkey:i64:true", "c_mktsegment:s:false");
        DBSPSourceBaseOperator orders = source(circuit, "orders",
                "o_orderkey:i64:true", "o_custkey:i64:false", "o_orderdate:Date:false",
                "o_shippriority:i32:false");
        DBSPSourceBaseOperator lineitem = source(circuit, "lineitem",
                "l_orderkey:i64:true", "l_linenumber:i32:true",
                "l_extendedprice:DECIMAL(15, 2):false",
                "l_discount:DECIMAL(4, 2):false", "l_shipdate:Date:false");
        if (circuit.sourceOperators.size() != 3)
            throw unsupported("exactly three base sources are required");

        DBSPMapIndexOperator customerIndex = index(circuit, customer,
                new int[][][] {{{0, 0, 0}}, {{0, 1, 1}}});
        DBSPMapIndexOperator ordersIndex = index(circuit, orders,
                new int[][][] {{{0, 1, 1}}, {{0, 0, 0}, {0, 1, 2}, {0, 1, 3}}});
        DBSPMapIndexOperator lineIndex = index(circuit, lineitem,
                new int[][][] {{{0, 0, 0}}, {{0, 0, 1}, {0, 1, 2}, {0, 1, 3}, {0, 1, 4}}});

        List<DBSPJoinBaseOperator> joins = new ArrayList<>();
        for (DBSPOperator operator : circuit.getAllOperators()) {
            if (operator instanceof DBSPJoinBaseOperator join)
                joins.add(join);
        }
        if (joins.size() != 2)
            throw unsupported("exactly two joins are required");
        DBSPJoinBaseOperator first = joins.stream()
                .filter(join -> join.inputs.get(0).operator == ordersIndex &&
                        join.inputs.get(1).operator == customerIndex)
                .findFirst().orElseThrow(() -> unsupported("Orders-Customer join orientation changed"));
        DBSPJoinBaseOperator second = joins.stream()
                .filter(join -> join.inputs.get(0).operator == lineIndex &&
                        join.inputs.get(1).operator == first)
                .findFirst().orElseThrow(() -> unsupported("Lineitem-Orders join orientation changed"));
        if (!(first instanceof DBSPJoinIndexOperator) || !(second instanceof DBSPJoinOperator) ||
                !first.isMultiset || !second.isMultiset)
            throw unsupported("incremental multiset joins are required");
        requireProjection(first.getClosureFunction(),
                new int[][][] {{{1, 0}}, {{0, 0}, {2, 0}, {1, 1}, {1, 2}}}, true);
        requireProjection(second.getClosureFunction(),
                new int[][] {{2, 0}, {2, 1}, {0, 0}, {2, 2}, {2, 3},
                        {1, 0}, {1, 1}, {1, 2}, {1, 3}});

        long userViews = circuit.sinkOperators.values().stream()
                .filter(view -> !view.metadata.system).count();
        DBSPSinkOperator view = circuit.sinkOperators.values().stream()
                .filter(candidate -> !candidate.metadata.system).findFirst()
                .orElseThrow(() -> unsupported("materialized view V is required"));
        if (userViews != 1 || !view.viewName.name().equalsIgnoreCase("v") ||
                view.input().operator != second || view.metadata.size() != 9 ||
                view.metadata.viewKind != SqlCreateView.ViewKind.MATERIALIZED)
            throw unsupported("output view is not the supported Q3 projection");
        String[] outputColumns = {"c_custkey", "c_mktsegment", "o_orderkey", "o_orderdate",
                "o_shippriority", "l_linenumber", "l_extendedprice", "l_discount", "l_shipdate"};
        for (int i = 0; i < outputColumns.length; i++) {
            if (!view.metadata.columns.get(i).columnName.name().equalsIgnoreCase(outputColumns[i]))
                throw unsupported("output view columns changed");
        }
        return new Q3MergedIndexBinding(first, second);
    }

    private static DBSPSourceBaseOperator source(DBSPCircuit circuit, String name, String... columns) {
        DBSPSourceBaseOperator source = circuit.sourceOperators.values().stream()
                .map(input -> input.asOperator())
                .filter(operator -> operator instanceof DBSPSourceBaseOperator)
                .map(operator -> (DBSPSourceBaseOperator) operator)
                .filter(operator -> operator.tableName.name().equalsIgnoreCase(name))
                .findFirst().orElseThrow(() -> unsupported("missing " + name + " source"));
        TableMetadata metadata = source.metadata;
        if (metadata.getColumnCount() != columns.length)
            throw unsupported(name + " schema changed");
        for (int i = 0; i < columns.length; i++) {
            InputColumnMetadata column = metadata.getColumnMetadata(i);
            String actual = column.name.name().toLowerCase(Locale.ROOT) + ":" +
                    column.type + ":" + column.isPrimaryKey;
            if (!actual.equals(columns[i]))
                throw unsupported(name + " schema changed: " + actual);
        }
        return source;
    }

    private static DBSPMapIndexOperator index(DBSPCircuit circuit, DBSPSourceBaseOperator source,
                                               int[][][] projection) {
        List<DBSPMapIndexOperator> matches = new ArrayList<>();
        for (DBSPOperator operator : circuit.getAllOperators()) {
            if (operator instanceof DBSPMapIndexOperator candidate &&
                    candidate.input().operator == source)
                matches.add(candidate);
        }
        if (matches.size() != 1)
            throw unsupported("expected one direct index projection for " + source.tableName.name());
        DBSPMapIndexOperator index = matches.get(0);
        requireProjection(index.getClosureFunction(), projection, false);
        return index;
    }

    private static void requireProjection(DBSPClosureExpression closure, int[][][] projection,
                                          boolean optional) {
        DBSPExpression body = optional && closure.body instanceof DBSPSomeExpression some ?
                some.expression : closure.body;
        DBSPBaseTupleExpression pair = tuple(body, projection.length);
        for (int i = 0; i < projection.length; i++) {
            DBSPBaseTupleExpression part = tuple(pair.get(i), projection[i].length);
            for (int j = 0; j < projection[i].length; j++)
                requireField(part.get(j), closure, projection[i][j]);
        }
    }

    private static void requireProjection(DBSPClosureExpression closure, int[][] projection) {
        DBSPBaseTupleExpression tuple = tuple(closure.body, projection.length);
        for (int i = 0; i < projection.length; i++)
            requireField(tuple.get(i), closure, projection[i]);
    }

    private static DBSPBaseTupleExpression tuple(DBSPExpression expression, int size) {
        if (expression instanceof DBSPBaseTupleExpression tuple && !tuple.isNull() &&
                tuple.size() == size)
            return tuple;
        throw unsupported("Q3 projection changed");
    }

    private static void requireField(DBSPExpression expression, DBSPClosureExpression closure,
                                     int[] path) {
        DBSPExpression current = expression;
        while (current instanceof DBSPCloneExpression clone)
            current = clone.expression;
        List<Integer> actual = new ArrayList<>();
        while (current instanceof DBSPFieldExpression field) {
            actual.add(0, field.fieldNo);
            current = field.expression;
            while (current instanceof DBSPDerefExpression deref)
                current = deref.expression;
        }
        while (current instanceof DBSPDerefExpression deref)
            current = deref.expression;
        if (!(current instanceof DBSPVariablePath variable))
            throw unsupported("Q3 projection contains a computed expression");
        int parameter = -1;
        for (int i = 0; i < closure.parameters.length; i++) {
            if (Objects.equals(variable.variable, closure.parameters[i].name)) {
                parameter = i;
                break;
            }
        }
        if (parameter < 0 || actual.size() + 1 != path.length || parameter != path[0])
            throw unsupported("Q3 projection field changed: " + expression + " expected " +
                    java.util.Arrays.toString(path));
        for (int i = 0; i < actual.size(); i++) {
            if (actual.get(i) != path[i + 1])
                throw unsupported("Q3 projection field changed: " + expression + " expected " +
                        java.util.Arrays.toString(path));
        }
    }

    private static InternalCompilerError unsupported(String reason) {
        return new InternalCompilerError("--q3-merged-index supports only the predicate-free Q3 baseline: " + reason);
    }
}
