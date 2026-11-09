package org.dbsp.sqlCompiler.compiler.backend.rust;

import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.AllColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.ColumnRef;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Domain;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Identifier;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Node;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PayloadColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PlacedIndex;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Placement;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Via;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.MergedIndexSpecException;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.SpecError;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.SpecPosition;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex.Column;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex.ResolvedPlacement;
import org.dbsp.sqlCompiler.ir.type.DBSPType;
import org.dbsp.util.IIndentStream;
import org.dbsp.util.IndentStreamBuilder;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

/**
 * Prints the Rust module of a validated merged-index spec: the trie as data,
 * the tag and index-id constants, typed keys and payloads with their packed
 * codecs, the {@code MergedIndexDefinition} implementation, and the prefix
 * functions. See "Generated code" in docs/design/merged-index/general-definition.md.
 *
 * <p>Every item is derived from the trie here, at generation time, and printed
 * as constants and straight-line code, so the generated {@code fold} never walks
 * the trie. The output is readable but not formatted; callers run rustfmt.
 *
 * <p>Names: a placement {@code OrdersByCustkeyOrderkey} gets the structs
 * {@code OrdersByCustkeyOrderkeyKey} (fields named by domain) and
 * {@code OrdersByCustkeyOrderkeyPayload} (fields named by SQL column), the
 * constant {@code INDEX_ID_ORDERS_BY_CUSTKEY_ORDERKEY}, and a variant of the
 * {@code Key} and {@code Payload} enums. A domain {@code custkey} gets
 * {@code TAG_CUSTKEY}.
 */
public final class MergedIndexRustEmitter {
    /** Rust keywords, which become raw identifiers such as {@code r#type}. */
    private static final Set<String> RUST_KEYWORDS = Set.of(
            "as", "async", "await", "break", "const", "continue", "dyn", "else", "enum", "extern",
            "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
            "pub", "ref", "return", "static", "struct", "trait", "true", "type", "unsafe", "use",
            "where", "while", "abstract", "become", "box", "do", "final", "gen", "macro", "override",
            "priv", "try", "typeof", "unsized", "virtual", "yield");
    /** Keywords that cannot be raw identifiers. */
    private static final Set<String> RESERVED_NAMES = Set.of("self", "Self", "super", "crate", "_");
    private static final Pattern IDENTIFIER = Pattern.compile("[A-Za-z_][A-Za-z0-9_]*");
    private static final String MI = "merged_index::";

    private final ValidatedMergedIndex index;
    private final DBSPCompiler compiler;
    private final IIndentStream out = new IndentStreamBuilder();

    private MergedIndexRustEmitter(ValidatedMergedIndex index, DBSPCompiler compiler) {
        this.index = index;
        this.compiler = compiler;
    }

    /** The Rust module for {@code index}. {@code specName} names the spec in the header comment.
     * @throws MergedIndexSpecException if a derived name is not a usable Rust name. */
    public static String emit(ValidatedMergedIndex index, DBSPCompiler compiler, String specName) {
        MergedIndexRustEmitter emitter = new MergedIndexRustEmitter(index, compiler);
        emitter.checkNames();
        emitter.header(specName);
        emitter.constants();
        emitter.trie();
        for (ResolvedPlacement placement : index.placements())
            emitter.placementTypes(placement);
        emitter.enums();
        emitter.payloadCodec();
        emitter.definition();
        emitter.prefixes();
        return emitter.out.toString();
    }

    // Names

    /** The constant suffix of a camel-case or snake-case name: {@code OrdersByCustkey} becomes {@code ORDERS_BY_CUSTKEY}. */
    static String screamingSnake(String name) {
        StringBuilder result = new StringBuilder();
        for (int i = 0; i < name.length(); i++) {
            char c = name.charAt(i);
            if (Character.isUpperCase(c) && i > 0) {
                char previous = name.charAt(i - 1);
                if (Character.isLowerCase(previous) || Character.isDigit(previous))
                    result.append('_');
            }
            result.append(Character.toUpperCase(c));
        }
        return result.toString();
    }

    /** {@code name} as a Rust identifier: a keyword becomes a raw identifier. */
    static String rustIdentifier(String name) {
        return RUST_KEYWORDS.contains(name) ? "r#" + name : name;
    }

    private static String tagConstant(String domain) {
        return "TAG_" + domain.toUpperCase(Locale.ROOT);
    }

    private static String indexIdConstant(PlacedIndex placed) {
        return "INDEX_ID_" + screamingSnake(placed.name());
    }

    private static String prefixFunction(List<String> path) {
        return "prefix_" + String.join("_", path);
    }

    private String definitionName() {
        return this.index.trie().name() + "Definition";
    }

    /** Rejects specs whose derived Rust names are invalid or collide. The parser
     * admits only identifiers, but {@code *} brings in schema column names, and
     * distinct spec names can map to one Rust name. */
    private void checkNames() {
        List<SpecError> errors = new ArrayList<>();
        Map<String, SpecPosition> itemNames = new HashMap<>();
        KeyTrie trie = this.index.trie();
        this.claim(itemNames, errors, this.definitionName(), trie.position());
        for (String domain : trie.domainsInTagOrder()) {
            SpecPosition position = trie.domain(domain).map(Domain::position).orElse(trie.position());
            this.checkIdentifier(errors, domain, position);
            this.claim(itemNames, errors, tagConstant(domain), position);
        }
        for (ResolvedPlacement resolved : this.index.placements()) {
            PlacedIndex placed = resolved.placed();
            SpecPosition position = placed.placement().position();
            this.claim(itemNames, errors, placed.name() + "Key", position);
            this.claim(itemNames, errors, placed.name() + "Payload", position);
            this.claim(itemNames, errors, indexIdConstant(placed), position);
            for (Column column : resolved.payloadColumns())
                this.checkIdentifier(errors, column.column(), position);
        }
        KeyTrie.visitPreorder(trie.children(), new ArrayList<>(), (path, node) -> {
            this.claim(itemNames, errors, prefixFunction(path), node.position());
            for (Node child : node.children())
                this.claim(itemNames, errors, prefixFunction(path) + "_then_" + child.domain(), child.position());
        });
        if (!errors.isEmpty())
            throw new MergedIndexSpecException(errors);
    }

    private void claim(Map<String, SpecPosition> names, List<SpecError> errors, String name, SpecPosition position) {
        SpecPosition earlier = names.putIfAbsent(name, position);
        if (earlier != null)
            errors.add(new SpecError(position, "generated Rust name `" + name + "` is also derived at " + earlier));
    }

    private void checkIdentifier(List<SpecError> errors, String name, SpecPosition position) {
        if (!IDENTIFIER.matcher(name).matches() || RESERVED_NAMES.contains(name))
            errors.add(new SpecError(position, "`" + name + "` is not a usable Rust field name"));
    }

    private String rustType(DBSPType type) {
        return ToRustInnerVisitor.toRustString(this.compiler, type, null, false);
    }

    // Items

    private void header(String specName) {
        this.out.append("//! The merged index `").append(this.index.trie().name())
                .append("`, generated by the SQL compiler from `").append(specName).append("`.").newline()
                .append("//! Do not edit: change the spec and regenerate.").newline()
                .newline()
                .append("#![allow(non_camel_case_types, non_snake_case)]").newline()
                .newline()
                .append("use dbsp::trace::merged_index;").newline()
                .append("#[allow(unused_imports)]").newline()
                .append("use feldera_sqllib::*;").newline()
                .newline();
    }

    private void constants() {
        KeyTrie trie = this.index.trie();
        this.out.append("/// Domain tags: each domain's position of first appearance in the trie, plus 1.").newline();
        for (String domain : trie.domainsInTagOrder())
            this.out.append("pub const ").append(tagConstant(domain)).append(": u8 = ")
                    .append(trie.domainTag(domain).orElseThrow()).append(";").newline();
        this.out.newline()
                .append("/// Index ids: each placement's position in the trie's pre-order walk, plus 1.").newline();
        for (ResolvedPlacement resolved : this.index.placements())
            this.out.append("pub const ").append(indexIdConstant(resolved.placed())).append(": u8 = ")
                    .append(resolved.placed().id()).append(";").newline();
        this.out.newline();
    }

    private void trie() {
        KeyTrie trie = this.index.trie();
        this.out.append("/// The spec's trie, as data.").newline()
                .append("pub const TRIE: ").append(MI).append("KeyTrie = ").append(MI).append("KeyTrie {").increase()
                .append("name: ").append(quote(trie.name())).append(",").newline()
                .append("domains: &[").increase();
        for (Domain domain : trie.domains()) {
            this.out.append(MI).append("Domain {").increase()
                    .append("name: ").append(quote(domain.name())).append(",").newline()
                    .append("columns: &[").increase();
            for (ColumnRef column : domain.columns())
                this.out.append(MI).append("ColumnRef { relation: ").append(quote(column.relation()))
                        .append(", column: ").append(quote(column.column())).append(" },").newline();
            // The width comes from the Rust type, so the Java trie needs no such field.
            this.out.decrease().append("],").newline()
                    .append("width: <").append(this.rustType(this.index.domainTypes().get(domain.name())))
                    .append(" as ").append(MI).append("OrderedKey>::WIDTH,").newline()
                    .decrease().append("},").newline();
        }
        this.out.decrease().append("],").newline();
        this.nodes(trie.children());
        this.out.decrease().append("};").newline().newline();
    }

    /** Prints {@code children: &[...],} for {@code nodes}. */
    private void nodes(List<Node> nodes) {
        if (nodes.isEmpty()) {
            this.out.append("children: &[],").newline();
            return;
        }
        this.out.append("children: &[").increase();
        for (Node node : nodes) {
            this.out.append(MI).append("Node {").increase()
                    .append("domain: ").append(quote(node.domain())).append(",").newline()
                    .append("placements: &[").increase();
            for (Placement placement : node.placements())
                this.placement(placement);
            this.out.decrease().append("],").newline();
            this.nodes(node.children());
            this.out.decrease().append("},").newline();
        }
        this.out.decrease().append("],").newline();
    }

    private void placement(Placement placement) {
        this.out.append(MI).append("Placement {").increase()
                .append("relation: ").append(quote(placement.relation())).append(",").newline()
                .append("via: &[");
        for (Via via : placement.vias())
            this.out.append(MI).append("Via { domain: ").append(quote(via.domain()))
                    .append(", relation: ").append(quote(via.relation())).append(" }, ");
        this.out.append("],").newline().append("payload: ");
        if (placement.payload() instanceof AllColumns) {
            this.out.append(MI).append("Payload::All");
        } else {
            this.out.append(MI).append("Payload::Columns(&[");
            for (Identifier column : ((PayloadColumns) placement.payload()).columns())
                this.out.append(quote(column.name())).append(", ");
            this.out.append("])");
        }
        this.out.append(",").newline()
                .decrease().append("},").newline();
    }

    private static final String DERIVES = "#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]";

    /** The key struct and the payload struct of one placement, with the payload's codec. */
    private void placementTypes(ResolvedPlacement resolved) {
        PlacedIndex placed = resolved.placed();
        String name = placed.name();
        this.out.append("/// The key of `").append(name).append("`: one field per domain on its path.").newline()
                .append(DERIVES).newline()
                .append("pub struct ").append(name).append("Key {").increase();
        for (int i = 0; i < placed.path().size(); i++)
            this.out.append("pub ").append(rustIdentifier(placed.path().get(i))).append(": ")
                    .append(this.rustType(resolved.keyColumns().get(i).type())).append(",").newline();
        this.out.decrease().append("}").newline().newline();

        List<Column> payload = resolved.payloadColumns();
        this.out.append("/// The payload of `").append(name).append("`, in payload order.").newline()
                .append(DERIVES).newline()
                .append("pub struct ").append(name).append("Payload {").increase();
        for (Column column : payload)
            this.out.append("pub ").append(rustIdentifier(column.column())).append(": ")
                    .append(this.rustType(column.type())).append(",").newline();
        this.out.decrease().append("}").newline().newline();

        this.out.append("impl ").append(name).append("Payload {").increase()
                .append("/// Packs the fields back to back, in order.").newline()
                .append("pub fn encode(&self) -> Vec<u8> {").increase()
                .append(MI).append("PayloadEncoder::new()");
        for (Column column : payload)
            this.out.append(".field(&self.").append(rustIdentifier(column.column())).append(")");
        this.out.append(".finish()").newline()
                .decrease().append("}").newline().newline()
                .append("/// The inverse of [`Self::encode`]. Rejects truncated, over-long, and out-of-range bytes.")
                .newline()
                .append("pub fn decode(bytes: &[u8]) -> Result<Self, String> {").increase()
                .append(payload.isEmpty() ? "let " : "let mut ")
                .append("fields = ").append(MI).append("PayloadDecoder::new(bytes);").newline()
                .append("let payload = Self {").increase();
        for (Column column : payload)
            this.out.append(rustIdentifier(column.column())).append(": fields.field()?,").newline();
        this.out.decrease().append("};").newline()
                .append("fields.finish()?;").newline()
                .append("Ok(payload)").newline()
                .decrease().append("}").newline()
                .decrease().append("}").newline().newline();
    }

    private void enums() {
        this.out.append("/// The key of any placement.").newline()
                .append(DERIVES).newline()
                .append("pub enum Key {").increase();
        for (ResolvedPlacement resolved : this.index.placements()) {
            String name = resolved.placed().name();
            this.out.append(name).append("(").append(name).append("Key),").newline();
        }
        this.out.decrease().append("}").newline().newline()
                .append("/// The payload of any placement. The index stores the bytes of [`encode_payload`] instead.")
                .newline()
                .append(DERIVES).newline()
                .append("pub enum Payload {").increase();
        for (ResolvedPlacement resolved : this.index.placements()) {
            String name = resolved.placed().name();
            this.out.append(name).append("(").append(name).append("Payload),").newline();
        }
        this.out.decrease().append("}").newline().newline()
                .append("impl Payload {").increase()
                .append("/// Whether this payload belongs to the placement that `key` names.").newline();
        if (this.index.placements().size() == 1) {
            // Any test would be irrefutable, which is a lint error; one placement always matches.
            this.out.append("pub fn matches(&self, _key: &Key) -> bool {").increase()
                    .append("true").newline();
        } else {
            this.out.append("pub fn matches(&self, key: &Key) -> bool {").increase()
                    .append("matches!(").increase()
                    .append("(key, self),").newline();
            List<ResolvedPlacement> placements = this.index.placements();
            for (int i = 0; i < placements.size(); i++) {
                String name = placements.get(i).placed().name();
                this.out.append(i == 0 ? "" : "| ").append("(Key::").append(name).append("(_), Payload::")
                        .append(name).append("(_))").newline();
            }
            this.out.decrease().append(")").newline();
        }
        this.out.decrease().append("}").newline()
                .decrease().append("}").newline().newline();
    }

    private void payloadCodec() {
        this.out.append("/// Packs `payload`, without a tag: the key names its placement.").newline()
                .append("pub fn encode_payload(payload: &Payload) -> Vec<u8> {").increase()
                .append("match payload {").increase();
        for (ResolvedPlacement resolved : this.index.placements())
            this.out.append("Payload::").append(resolved.placed().name()).append("(p) => p.encode(),").newline();
        this.out.decrease().append("}").newline()
                .decrease().append("}").newline().newline()
                .append("/// The inverse of [`encode_payload`]. The key's placement names the fields to read.")
                .newline()
                .append("pub fn decode_payload(key: &Key, bytes: &[u8]) -> Result<Payload, String> {").increase()
                .append("match key {").increase();
        for (ResolvedPlacement resolved : this.index.placements()) {
            String name = resolved.placed().name();
            this.out.append("Key::").append(name).append("(_) => ").append(name)
                    .append("Payload::decode(bytes).map(Payload::").append(name).append("),").newline();
        }
        this.out.decrease().append("}").newline()
                .decrease().append("}").newline().newline();
    }

    private void definition() {
        String definition = this.definitionName();
        this.out.append("/// Folds keys into bytes and back: `[tag value]* 0 index_id`.").newline()
                .append("pub struct ").append(definition).append(";").newline().newline()
                .append("impl ").append(MI).append("MergedIndexDefinition for ").append(definition).append(" {")
                .increase()
                .append("type Key = Key;").newline().newline()
                .append("const TRIE: ").append(MI).append("KeyTrie = TRIE;").newline().newline()
                .append("fn fold(key: &Key) -> ").append(MI).append("FoldedKey {").increase()
                .append("match key {").increase();
        for (ResolvedPlacement resolved : this.index.placements()) {
            PlacedIndex placed = resolved.placed();
            this.out.append("Key::").append(placed.name()).append("(k) => ").append(MI).append("KeyEncoder::new()");
            for (String domain : placed.path())
                this.out.append(".field(").append(tagConstant(domain)).append(", &k.")
                        .append(rustIdentifier(domain)).append(")");
            this.out.append(".finish(").append(indexIdConstant(placed)).append("),").newline();
        }
        this.out.decrease().append("}").newline()
                .decrease().append("}").newline().newline()
                .append("fn unfold(bytes: &[u8]) -> Result<Key, String> {").increase()
                .append("let (index_id, mut fields) = ").append(MI).append("KeyDecoder::new(bytes)?;").newline()
                .append("let key = match index_id {").increase();
        for (ResolvedPlacement resolved : this.index.placements()) {
            PlacedIndex placed = resolved.placed();
            this.out.append(indexIdConstant(placed)).append(" => Key::").append(placed.name()).append("(")
                    .append(placed.name()).append("Key {").increase();
            // Struct fields are evaluated in the order written, which is path order.
            for (String domain : placed.path())
                this.out.append(rustIdentifier(domain)).append(": fields.field(").append(tagConstant(domain))
                        .append(")?,").newline();
            this.out.decrease().append("}),").newline();
        }
        this.out.append("_ => return Err(format!(\"unknown index id {index_id}\")),").newline()
                .decrease().append("};").newline()
                .append("fields.finish()?;").newline()
                .append("Ok(key)").newline()
                .decrease().append("}").newline()
                .decrease().append("}").newline().newline();
    }

    /** One function per node, the bytes shared by every key in its sort range, and
     * one per child edge, those bytes followed by the child's tag. */
    private void prefixes() {
        KeyTrie.visitPreorder(this.index.trie().children(), new ArrayList<>(), (path, node) -> {
            String name = prefixFunction(path);
            this.prefix(name, path, "The bytes shared by every key below `" + String.join(" > ", path) + "`.",
                    ".prefix()");
            for (Node child : node.children())
                this.prefix(name + "_then_" + child.domain(), path,
                        "The bytes shared by every key below `" + String.join(" > ", path) + " > "
                                + child.domain() + "`, through its tag.",
                        ".then_tag(" + tagConstant(child.domain()) + ")");
        });
    }

    private void prefix(String name, List<String> path, String comment, String ending) {
        this.out.append("/// ").append(comment).newline()
                .append("pub fn ").append(name).append("(");
        for (String domain : path)
            this.out.append(rustIdentifier(domain)).append(": ")
                    .append(this.rustType(this.index.domainTypes().get(domain))).append(", ");
        this.out.append(") -> Vec<u8> {").increase()
                .append(MI).append("KeyEncoder::new()");
        for (String domain : path)
            this.out.append(".field(").append(tagConstant(domain)).append(", &")
                    .append(rustIdentifier(domain)).append(")");
        this.out.append(ending).newline()
                .decrease().append("}").newline().newline();
    }

    private static String quote(String text) {
        return "\"" + text.replace("\\", "\\\\").replace("\"", "\\\"") + "\"";
    }
}
