package org.dbsp.sqlCompiler.compiler.backend.dot;

import org.dbsp.sqlCompiler.circuit.DBSPCircuit;
import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.compiler.visitors.outer.CircuitTransform;
import org.dbsp.sqlCompiler.compiler.visitors.outer.CircuitVisitor;
import org.dbsp.util.IndentStream;
import org.dbsp.util.Utilities;

import javax.annotation.Nullable;
import java.io.File;
import java.io.IOException;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/** Dump a graph in the graphviz dot format. */
public class ToDot {
    public static void customDump(String fileName, @Nullable String outputFormat,
                                  DBSPCircuit circuit,
                                  ToDotEdgesVisitor.VisitorConstructor nodesVisitor,
                                  ToDotEdgesVisitor.VisitorConstructor edgesVisitor) {
        if (circuit.isEmpty())
            return;
        File tmp = null;
        try {
            tmp = File.createTempFile("tmp", ".dot");
            tmp.deleteOnExit();
            writeDot(circuit, tmp.toPath(), nodesVisitor, edgesVisitor);
            if (outputFormat != null) {
                System.out.println("Writing " + fileName + " " + circuit.id);
                Utilities.runProcess(".", "dot", "-T", outputFormat,
                        "-o", fileName, tmp.getAbsolutePath());
            }
        } catch (Exception ex) {
            if (tmp != null) {
                try {
                    System.out.println(Utilities.readFile(tmp.toPath()));
                } catch (IOException e) {
                    throw new RuntimeException(e);
                }
            }
            throw new RuntimeException(ex);
        }
    }

    /** Write the topology with the supplied visitors, retaining a DOT file even if rendering fails. */
    private static void writeDot(DBSPCircuit circuit, Path file,
                                 ToDotEdgesVisitor.VisitorConstructor nodesVisitor,
                                 ToDotEdgesVisitor.VisitorConstructor edgesVisitor) throws IOException {
        try (PrintWriter writer = new PrintWriter(Files.newBufferedWriter(file, StandardCharsets.UTF_8))) {
            IndentStream stream = new IndentStream(writer);
            stream.append("digraph ")
                    .append(circuit.id)
                    .append(" {")
                    .increase()
                    .append("ordering=\"in\"").newline();
            nodesVisitor.create(stream).apply(circuit);
            edgesVisitor.create(stream).apply(circuit);
            stream.decrease().append("}").newline();
            if (writer.checkError())
                throw new IOException("Could not write DOT file " + file);
        }
    }

    /** Export persistent DOT and SVG using the existing topology visitors.
     * The stem must be a file name, not a path.  Empty circuits produce an empty graph. */
    public static void exportArtifacts(DBSPCircuit circuit, Path directory, String stem,
                                       ToDotEdgesVisitor.VisitorConstructor nodesVisitor,
                                       ToDotEdgesVisitor.VisitorConstructor edgesVisitor) throws IOException {
        if (stem.isEmpty() || stem.equals(".") || stem.equals("..") ||
                stem.contains("/") || stem.contains("\\"))
            throw new IllegalArgumentException("Artifact stem must be a file name: " + stem);
        Files.createDirectories(directory);
        Path dot = directory.resolve(stem + ".dot");
        Path svg = directory.resolve(stem + ".svg");
        writeDot(circuit, dot, nodesVisitor, edgesVisitor);
        try {
            Utilities.runProcess(".", "dot", "-Tsvg", "-o", svg.toString(), dot.toString());
        } catch (InterruptedException ex) {
            Thread.currentThread().interrupt();
            throw new IOException("Interrupted rendering " + dot, ex);
        } catch (RuntimeException ex) {
            throw new IOException("Could not render " + dot + "; raw DOT is preserved", ex);
        }
    }

    public static void dump(DBSPCompiler compiler, String fileName, int details,
                            @Nullable String outputFormat, DBSPCircuit circuit) {
        customDump(fileName, outputFormat, circuit,
                stream -> new ToDotNodesVisitor(compiler, stream, details),
                stream -> new ToDotEdgesVisitor(compiler, stream, details));
    }

    static int counter = 0;

    /** Returns a circuit transform which can be inserted in the CircuitOptimizer to dump the
     * circuit at some point.
     * details < 2: just graph topology
     * details = 2: dump edge types, abbreviated if too large
     * details = 3: dump source positions
     * details >= 4: dump node functions and full types */
    public static CircuitTransform dumper(DBSPCompiler compiler, String file, int details) {
        return new CircuitTransform() {
            @Override
            public String getName() {
                return "toDot";
            }

            @Override
            public DBSPCircuit apply(DBSPCircuit circuit) {
                ToDot.dump(compiler, counter++ + file, details, "png", circuit);
                return circuit;
            }

            @Override
            public String toString() {
                return "toDot";
            }
        };
    }
}
