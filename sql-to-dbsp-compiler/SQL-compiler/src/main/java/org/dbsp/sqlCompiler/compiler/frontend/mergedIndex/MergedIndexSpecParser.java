package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.AllColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.ColumnRef;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Domain;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Identifier;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Node;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Payload;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PayloadColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Placement;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Via;

import java.util.ArrayList;
import java.util.List;

/**
 * Parses the text of a merged-index spec into a {@link KeyTrie}.
 * The parser checks syntax only; {@link MergedIndexValidator} checks meaning.
 *
 * <pre>
 * spec      := 'domains' '{' domainDef* '}' 'merged_index' ident '{' node* '}'
 * domainDef := ident '=' colref (',' colref)* ';'
 * colref    := ident '.' ident
 * node      := ident '{' placement* node* '}'
 * placement := ident ('[' via (',' via)* ']')? ':' payload ';'
 * via       := ident 'via' ident
 * payload   := '*' | ident (',' ident)*
 * </pre>
 *
 * <p>An identifier starts with a letter or underscore and continues with
 * letters, digits, and underscores. {@code --} starts a comment that runs to
 * the end of the line. Inside a node, placements come before child nodes,
 * because a node's placements are numbered before its children's.
 */
public final class MergedIndexSpecParser {
    private enum Kind { IDENTIFIER, SYMBOL, END }

    private record Token(Kind kind, String text, SpecPosition position) {
        boolean is(String symbol) {
            return this.kind == Kind.SYMBOL && this.text.equals(symbol);
        }

        String describe() {
            return this.kind == Kind.END ? "end of input" : "`" + this.text + "`";
        }
    }

    private static final String SYMBOLS = "{}[]=,;:.*";

    private final List<Token> tokens;
    private int next; // index of the next unconsumed token

    private MergedIndexSpecParser(List<Token> tokens) {
        this.tokens = tokens;
        this.next = 0;
    }

    /** Parses a whole spec.
     * @throws MergedIndexSpecException at the first syntax error. */
    public static KeyTrie parse(String text) {
        return new MergedIndexSpecParser(tokenize(text)).spec();
    }

    private KeyTrie spec() {
        this.expectKeyword("domains");
        this.expect("{");
        List<Domain> domains = new ArrayList<>();
        while (!this.peek().is("}"))
            domains.add(this.domainDef());
        this.expect("}");

        Token keyword = this.expectKeyword("merged_index");
        Token name = this.expectIdentifier();
        this.expect("{");
        List<Node> children = new ArrayList<>();
        while (!this.peek().is("}"))
            children.add(this.node());
        this.expect("}");
        this.expectEnd();
        return new KeyTrie(name.text(), List.copyOf(domains), List.copyOf(children), keyword.position());
    }

    private Domain domainDef() {
        Token name = this.expectIdentifier();
        this.expect("=");
        List<ColumnRef> columns = new ArrayList<>();
        do {
            columns.add(this.columnRef());
        } while (this.accept(","));
        this.expect(";");
        return new Domain(name.text(), List.copyOf(columns), name.position());
    }

    private ColumnRef columnRef() {
        Token relation = this.expectIdentifier();
        this.expect(".");
        Token column = this.expectIdentifier();
        return new ColumnRef(relation.text(), column.text(), relation.position());
    }

    private Node node() {
        Token domain = this.expectIdentifier();
        this.expect("{");
        List<Placement> placements = new ArrayList<>();
        List<Node> children = new ArrayList<>();
        while (!this.peek().is("}")) {
            if (this.peekAfter().is("{")) {
                children.add(this.node());
            } else {
                Token start = this.peek();
                if (!children.isEmpty())
                    throw error(start.position(), "placement " + start.describe()
                            + " follows a child node; write a node's placements before its children");
                placements.add(this.placement());
            }
        }
        this.expect("}");
        return new Node(domain.text(), List.copyOf(placements), List.copyOf(children), domain.position());
    }

    private Placement placement() {
        Token relation = this.expectIdentifier();
        List<Via> vias = new ArrayList<>();
        if (this.accept("[")) {
            do {
                vias.add(this.via());
            } while (this.accept(","));
            this.expect("]");
        }
        this.expect(":");
        Payload payload = this.payload();
        this.expect(";");
        return new Placement(relation.text(), List.copyOf(vias), payload, relation.position());
    }

    private Via via() {
        Token domain = this.expectIdentifier();
        this.expectKeyword("via");
        Token relation = this.expectIdentifier();
        return new Via(domain.text(), relation.text(), domain.position());
    }

    private Payload payload() {
        if (this.accept("*"))
            return new AllColumns();
        List<Identifier> columns = new ArrayList<>();
        do {
            Token column = this.expectIdentifier();
            columns.add(new Identifier(column.text(), column.position()));
        } while (this.accept(","));
        return new PayloadColumns(List.copyOf(columns));
    }

    // Token access

    private Token peek() {
        return this.tokens.get(this.next);
    }

    /** The token after the next one, or the end token. */
    private Token peekAfter() {
        return this.tokens.get(Math.min(this.next + 1, this.tokens.size() - 1));
    }

    private Token advance() {
        Token token = this.peek();
        if (token.kind() != Kind.END)
            this.next++;
        return token;
    }

    /** Consumes the next token if it is {@code symbol}. */
    private boolean accept(String symbol) {
        if (!this.peek().is(symbol))
            return false;
        this.advance();
        return true;
    }

    private void expect(String symbol) {
        Token token = this.peek();
        if (!token.is(symbol))
            throw error(token.position(), "expected `" + symbol + "`, found " + token.describe());
        this.advance();
    }

    private Token expectIdentifier() {
        Token token = this.peek();
        if (token.kind() != Kind.IDENTIFIER)
            throw error(token.position(), "expected a name, found " + token.describe());
        return this.advance();
    }

    private Token expectKeyword(String keyword) {
        Token token = this.peek();
        if (token.kind() != Kind.IDENTIFIER || !token.text().equals(keyword))
            throw error(token.position(), "expected `" + keyword + "`, found " + token.describe());
        return this.advance();
    }

    private void expectEnd() {
        Token token = this.peek();
        if (token.kind() != Kind.END)
            throw error(token.position(), "expected end of input, found " + token.describe());
    }

    private static MergedIndexSpecException error(SpecPosition position, String message) {
        return new MergedIndexSpecException(List.of(new SpecError(position, message)));
    }

    // Lexing

    /** Splits {@code text} into identifiers and one-character symbols,
     * skipping whitespace and comments. The list ends with an END token. */
    private static List<Token> tokenize(String text) {
        List<Token> tokens = new ArrayList<>();
        int line = 1;
        int column = 1;
        int i = 0;
        while (i < text.length()) {
            char c = text.charAt(i);
            SpecPosition position = new SpecPosition(line, column);
            if (c == '\n') {
                line++;
                column = 1;
                i++;
            } else if (Character.isWhitespace(c)) {
                column++;
                i++;
            } else if (c == '-' && i + 1 < text.length() && text.charAt(i + 1) == '-') {
                while (i < text.length() && text.charAt(i) != '\n') {
                    column++;
                    i++;
                }
            } else if (isIdentifierStart(c)) {
                int start = i;
                while (i < text.length() && isIdentifierPart(text.charAt(i)))
                    i++;
                column += i - start;
                tokens.add(new Token(Kind.IDENTIFIER, text.substring(start, i), position));
            } else if (SYMBOLS.indexOf(c) >= 0) {
                tokens.add(new Token(Kind.SYMBOL, String.valueOf(c), position));
                column++;
                i++;
            } else {
                throw error(position, "unexpected character `" + c + "`");
            }
        }
        tokens.add(new Token(Kind.END, "", new SpecPosition(line, column)));
        return tokens;
    }

    private static boolean isIdentifierStart(char c) {
        return c == '_' || (c < 128 && Character.isLetter(c));
    }

    private static boolean isIdentifierPart(char c) {
        return isIdentifierStart(c) || (c >= '0' && c <= '9');
    }
}
