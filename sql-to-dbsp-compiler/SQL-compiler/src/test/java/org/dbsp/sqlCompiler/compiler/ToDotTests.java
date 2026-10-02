package org.dbsp.sqlCompiler.compiler;

import com.beust.jcommander.JCommander;
import com.beust.jcommander.ParameterException;
import org.dbsp.sqlCompiler.circuit.DBSPCircuit;
import org.dbsp.sqlCompiler.circuit.operator.DBSPConstantOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPDelayOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPDelayedIntegralOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPIntegrateOperator;
import org.dbsp.sqlCompiler.circuit.operator.DBSPNestedOperator;
import org.dbsp.sqlCompiler.compiler.backend.dot.StateDotNodesVisitor;
import org.dbsp.sqlCompiler.compiler.backend.dot.ToDot;
import org.dbsp.sqlCompiler.compiler.backend.dot.ToDotEdgesVisitor;
import org.dbsp.sqlCompiler.compiler.backend.dot.ToDotNodesVisitor;
import org.dbsp.sqlCompiler.compiler.frontend.calciteObject.CalciteEmptyRel;
import org.dbsp.sqlCompiler.ir.expression.DBSPZSetExpression;
import org.dbsp.sqlCompiler.ir.expression.literal.DBSPI32Literal;
import org.junit.Assert;
import org.junit.Rule;
import org.junit.Test;
import org.junit.rules.TemporaryFolder;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Set;
import java.util.TreeSet;
import java.util.regex.Pattern;

/** Artifact exports must retain the topology of the existing visitors. */
public class ToDotTests {
    @Rule
    public final TemporaryFolder temporary = new TemporaryFolder();

    private DBSPCircuit stateCircuit() {
        DBSPCircuit circuit = new DBSPCircuit(new ProgramMetadata(), true);
        var source = new DBSPConstantOperator(CalciteEmptyRel.INSTANCE,
                new DBSPZSetExpression(new DBSPI32Literal(1)), false);
        circuit.addOperator(source);
        var integrate = new DBSPIntegrateOperator(CalciteEmptyRel.INSTANCE, source.outputPort());
        circuit.addOperator(integrate);
        circuit.addOperator(new DBSPDelayedIntegralOperator(CalciteEmptyRel.INSTANCE, source.outputPort()));
        circuit.addOperator(new DBSPDelayOperator(CalciteEmptyRel.INSTANCE, null, integrate.outputPort()));
        var nested = new DBSPNestedOperator(CalciteEmptyRel.INSTANCE);
        nested.addOperator(new DBSPConstantOperator(CalciteEmptyRel.INSTANCE,
                new DBSPZSetExpression(new DBSPI32Literal(2)), false));
        circuit.addOperator(nested);
        return circuit;
    }

    private void export(DBSPCircuit circuit, Path directory, String stem, boolean styled) throws IOException {
        DBSPCompiler compiler = new DBSPCompiler(new CompilerOptions());
        ToDot.exportArtifacts(circuit, directory, stem,
                stream -> styled ? new StateDotNodesVisitor(compiler, stream, 1) :
                        new ToDotNodesVisitor(compiler, stream, 1),
                stream -> new ToDotEdgesVisitor(compiler, stream, 1));
    }

    private static Set<String> matches(String dot, String regex) {
        Set<String> result = new TreeSet<>();
        var matcher = Pattern.compile(regex, Pattern.MULTILINE).matcher(dot);
        while (matcher.find())
            result.add(matcher.group(1));
        return result;
    }

    private void compareTopology(DBSPCircuit circuit, Path directory) throws IOException {
        this.export(circuit, directory, "raw", false);
        this.export(circuit, directory, "styled", true);
        String raw = Files.readString(directory.resolve("raw.dot"));
        String styled = Files.readString(directory.resolve("styled.dot"));
        for (String regex : new String[]{"^\\s*(\\w+)\\s*\\[", "^\\s*(.+ -> [^;\\[]+)",
                "(subgraph cluster_\\d+)", "(<p[^>]+>)"})
            Assert.assertEquals(regex, matches(raw, regex), matches(styled, regex));
        Assert.assertTrue(Files.readString(directory.resolve("styled.svg")).contains("<svg"));
    }

    @Test
    public void stylesRetainStateNodesAndClusters() throws IOException {
        Path directory = this.temporary.newFolder().toPath();
        this.compareTopology(this.stateCircuit(), directory);
        String dot = Files.readString(directory.resolve("styled.dot"));
        Assert.assertTrue(dot.contains("[I]"));
        Assert.assertTrue(dot.contains("[S]"));
        Assert.assertTrue(dot.contains("accumulate_delay_trace"));
        var delayedDeclaration = Pattern.compile("(stream[0-9]+).*accumulate_delay_trace").matcher(dot);
        Assert.assertTrue(delayedDeclaration.find());
        String delayedId = delayedDeclaration.group(1);
        Assert.assertTrue(dot.contains(delayedId + " [style=filled fillcolor=\"#dbeafe\""));
        Assert.assertFalse(dot.contains(delayedId + " [style=filled fillcolor=\"#ede9fe\""));
        Assert.assertTrue(dot.contains("subgraph cluster_"));
    }

    @Test
    public void recordPortsAndExpandedHook() throws IOException {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.incrementalize = true;
        Path directory = this.temporary.newFolder().toPath();
        options.ioOptions.dumpExpandedCircuit = directory.toString();
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation("CREATE TABLE t(x INT NOT NULL LATENESS 2, PRIMARY KEY(x)); CREATE VIEW v AS SELECT * FROM t;");
        DBSPCircuit circuit = compiler.getFinalCircuit(false);
        compiler.throwIfErrorsOccurred();
        Assert.assertNotNull(circuit);
        this.compareTopology(circuit, directory);
        Assert.assertFalse(matches(Files.readString(directory.resolve("raw.dot")), "(<p[^>]+>)").isEmpty());
        Assert.assertTrue(Files.readString(directory.resolve("expanded.metadata.txt")).contains("capture_stage="));
        Assert.assertTrue(Files.exists(directory.resolve("expanded.svg")));
    }

    @Test
    public void expandedJoinProductsDoNotClaimAdditionalState() throws IOException {
        CompilerOptions options = new CompilerOptions();
        options.languageOptions.incrementalize = true;
        Path directory = this.temporary.newFolder().toPath();
        options.ioOptions.dumpExpandedCircuit = directory.toString();
        DBSPCompiler compiler = new DBSPCompiler(options);
        compiler.submitStatementsForCompilation("""
                CREATE TABLE a(k INT NOT NULL, x INT);
                CREATE TABLE b(k INT NOT NULL, y INT);
                CREATE VIEW v AS SELECT a.k, x, y FROM a JOIN b ON a.k = b.k;
                """);
        Assert.assertNotNull(compiler.getFinalCircuit(false));
        compiler.throwIfErrorsOccurred();
        String dot = Files.readString(directory.resolve("expanded.dot"));
        var joins = Pattern.compile("(stream[0-9]+).*stream_join(?:_index)?").matcher(dot);
        int count = 0;
        while (joins.find()) {
            count++;
            Assert.assertFalse(dot.contains(joins.group(1) + " [style=filled"));
        }
        Assert.assertTrue(count > 0);
        Assert.assertTrue(dot.contains("accumulate_delay_trace"));
        Assert.assertTrue(dot.contains("[I]"));
    }

    @Test
    public void exportFailuresPreserveDot() throws IOException {
        Path directory = this.temporary.newFolder().toPath();
        Files.createDirectory(directory.resolve("fail.svg"));
        Assert.assertThrows(IOException.class, () -> this.export(this.stateCircuit(), directory, "fail", true));
        Assert.assertTrue(Files.readString(directory.resolve("fail.dot")).contains("digraph"));
        Path file = this.temporary.newFile().toPath();
        Assert.assertThrows(IOException.class, () -> this.export(this.stateCircuit(), file, "graph", true));
        Assert.assertThrows(IllegalArgumentException.class,
                () -> this.export(this.stateCircuit(), directory, "../outside", true));
    }

    @Test
    public void emptyCircuitAndCli() throws IOException {
        Path directory = this.temporary.newFolder().toPath();
        this.export(new DBSPCircuit(new ProgramMetadata(), false), directory, "empty", true);
        Assert.assertTrue(Files.exists(directory.resolve("empty.svg")));
        CompilerOptions options = new CompilerOptions();
        var parser = JCommander.newBuilder().addObject(options).build();
        parser.parse("--dump-expanded-circuit", directory.toString(), "input.sql");
        Assert.assertEquals(directory.toString(), options.ioOptions.dumpExpandedCircuit);
        Assert.assertThrows(ParameterException.class, () -> parser.parse("--dump-expanded-circuit"));
    }
}
