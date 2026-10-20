package org.dbsp.sqlCompiler.compiler.frontend.mergedIndex;

import org.dbsp.sqlCompiler.compiler.DBSPCompiler;
import org.dbsp.sqlCompiler.compiler.ProgramMetadata;
import org.dbsp.sqlCompiler.compiler.errors.SourcePositionRange;
import org.dbsp.sqlCompiler.compiler.frontend.calciteCompiler.ProgramIdentifier;
import org.dbsp.sqlCompiler.compiler.frontend.calciteCompiler.RelColumnMetadata;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.AllColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.ColumnRef;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Domain;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Identifier;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Node;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PayloadColumns;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.PlacedIndex;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Placement;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.KeyTrie.Via;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex.Column;
import org.dbsp.sqlCompiler.compiler.frontend.mergedIndex.ValidatedMergedIndex.ResolvedPlacement;
import org.dbsp.sqlCompiler.compiler.frontend.statements.IHasSchema;
import org.dbsp.sqlCompiler.ir.type.DBSPType;
import org.dbsp.sqlCompiler.ir.type.DBSPTypeCode;

import javax.annotation.Nullable;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;

/**
 * Checks a {@link KeyTrie} against the schema of a compiled SQL program,
 * following the "Validation" section of
 * {@code docs/design/merged-index/general-definition.md}, and resolves each
 * placement's key and payload columns.
 *
 * <p>Relation and column names in the spec are unquoted SQL identifiers, so
 * they are looked up in the compiler's canonical casing. Tables and views are
 * both accepted as relations, but a view has no primary key, so the
 * key-uniqueness rule rejects every placement on a view.
 */
public final class MergedIndexValidator {
    /** Key types with an order-preserving fixed-width encoding ({@code OrderedKey} in DBSP). */
    private static final Set<DBSPTypeCode> FIXED_WIDTH_KEY_TYPES = EnumSet.of(
            DBSPTypeCode.INT8, DBSPTypeCode.INT16, DBSPTypeCode.INT32, DBSPTypeCode.INT64, DBSPTypeCode.INT128,
            DBSPTypeCode.UINT8, DBSPTypeCode.UINT16, DBSPTypeCode.UINT32, DBSPTypeCode.UINT64, DBSPTypeCode.UINT128,
            DBSPTypeCode.BOOL, DBSPTypeCode.DATE, DBSPTypeCode.TIME, DBSPTypeCode.TIMESTAMP, DBSPTypeCode.DECIMAL);

    /** Payload types with a packed encoding ({@code PackedField} in DBSP): the key types and strings. */
    private static final Set<DBSPTypeCode> PACKED_PAYLOAD_TYPES = EnumSet.copyOf(FIXED_WIDTH_KEY_TYPES);

    static {
        PACKED_PAYLOAD_TYPES.add(DBSPTypeCode.STRING);
    }

    private final KeyTrie trie;
    private final DBSPCompiler compiler;
    private final List<SpecError> errors = new ArrayList<>();
    /** The NOT NULL type of each declared domain whose columns resolve, by domain name. */
    private final Map<String, DBSPType> domainTypes = new LinkedHashMap<>();

    private MergedIndexValidator(KeyTrie trie, DBSPCompiler compiler) {
        this.trie = trie;
        this.compiler = compiler;
    }

    /** Validates {@code trie} against the tables and views that {@code compiler} has compiled.
     * @throws MergedIndexSpecException with every error found. */
    public static ValidatedMergedIndex validate(KeyTrie trie, DBSPCompiler compiler) {
        MergedIndexValidator validator = new MergedIndexValidator(trie, compiler);
        validator.checkDomains();
        validator.checkCounts();
        validator.checkLevel(trie.children(), new ArrayList<>());
        List<ResolvedPlacement> resolved = validator.resolvePlacements();
        if (!validator.errors.isEmpty())
            throw new MergedIndexSpecException(validator.errors);
        return new ValidatedMergedIndex(trie, validator.domainTypes, resolved);
    }

    // Rules 1-3: domains, their types, and fixed width

    private void checkDomains() {
        Map<String, String> domainOfColumn = new HashMap<>(); // canonical "relation.column" -> domain
        for (int position = 0; position < this.trie.domains().size(); position++) {
            Domain domain = this.trie.domains().get(position);
            for (Domain earlier : this.trie.domains().subList(0, position)) {
                if (earlier.name().equals(domain.name()))
                    this.error(domain.position(), "domain `" + domain.name() + "` is declared twice");
            }

            Set<ProgramIdentifier> relationsInDomain = new HashSet<>();
            @Nullable ColumnRef firstTyped = null;
            @Nullable DBSPType domainType = null;
            for (ColumnRef ref : domain.columns()) {
                if (!relationsInDomain.add(this.canonical(ref.relation())))
                    this.error(ref.position(), "`" + ref.relation() + "` has two columns in domain `"
                            + domain.name() + "`");
                String columnKey = this.canonical(ref.relation()).name() + "." + this.canonical(ref.column()).name();
                String earlierDomain = domainOfColumn.putIfAbsent(columnKey, domain.name());
                if (earlierDomain != null && !earlierDomain.equals(domain.name()))
                    this.error(ref.position(), "column `" + describe(ref) + "` is already in domain `"
                            + earlierDomain + "`");

                Optional<IHasSchema> relation = this.relation(ref.relation());
                if (relation.isEmpty()) {
                    this.error(ref.position(), "unknown table or view `" + ref.relation() + "`");
                    continue;
                }
                Optional<RelColumnMetadata> column = this.column(relation.get(), ref.column());
                if (column.isEmpty()) {
                    this.error(ref.position(), "`" + ref.relation() + "` has no column `" + ref.column() + "`");
                    continue;
                }
                DBSPType type = this.typeOf(column.get());
                if (domainType == null) {
                    firstTyped = ref;
                    domainType = type;
                } else if (!type.sameTypeIgnoringNullability(domainType)) {
                    this.error(ref.position(), "column `" + describe(ref) + "` has type "
                            + typeName(column.get()) + ", but `" + describe(firstTyped) + "` in domain `"
                            + domain.name() + "` has type " + this.sqlType(firstTyped));
                }
            }
            if (domainType == null)
                continue;
            if (!FIXED_WIDTH_KEY_TYPES.contains(domainType.code))
                this.error(domain.position(), "domain `" + domain.name() + "` has type " + this.sqlType(firstTyped)
                        + ", which is not fixed-width; key types are integers, BOOLEAN, DATE, TIME, "
                        + "TIMESTAMP, and DECIMAL");
            this.domainTypes.putIfAbsent(domain.name(), domainType.withMayBeNull(false));
        }
    }

    // Rule 6: the trie is well formed

    private void checkCounts() {
        if (this.trie.domains().size() > KeyTrie.MAX_TAGS)
            this.error(this.trie.position(), "more than " + KeyTrie.MAX_TAGS + " domains");
        int placementCount = this.trie.placements().size();
        if (placementCount > KeyTrie.MAX_TAGS)
            this.error(this.trie.position(), "more than " + KeyTrie.MAX_TAGS + " placements");
        if (placementCount == 0)
            this.error(this.trie.position(), "merged index `" + this.trie.name() + "` places no relation");
    }

    /** Checks the sibling nodes in {@code nodes}, then recurses into each of them.
     * {@code path} holds the domains above {@code nodes}. */
    private void checkLevel(List<Node> nodes, List<String> path) {
        for (int position = 0; position < nodes.size(); position++) {
            Node node = nodes.get(position);
            if (this.trie.domain(node.domain()).isEmpty())
                this.error(node.position(), "node domain `" + node.domain() + "` is not declared");
            for (Node sibling : nodes.subList(0, position)) {
                if (sibling.domain().equals(node.domain()))
                    this.error(node.position(), "sibling nodes repeat domain `" + node.domain() + "`");
            }
            if (path.contains(node.domain()))
                this.error(node.position(), "domain `" + node.domain() + "` repeats on the path " + path);

            List<Placement> placements = node.placements();
            for (int index = 0; index < placements.size(); index++) {
                Placement placement = placements.get(index);
                for (Placement earlier : placements.subList(0, index)) {
                    if (this.canonical(earlier.relation()).equals(this.canonical(placement.relation())))
                        this.error(placement.position(), "`" + placement.relation()
                                + "` is placed twice at node `" + node.domain() + "`");
                }
            }

            path.add(node.domain());
            this.checkLevel(node.children(), path);
            path.remove(path.size() - 1);
        }
    }

    // Rules 4, 5, 7, and NOT NULL keys: each placement

    /** Resolves every placement whose path consists of declared domains.
     * Placements below an undeclared domain were reported by {@link #checkLevel}. */
    private List<ResolvedPlacement> resolvePlacements() {
        List<ResolvedPlacement> resolved = new ArrayList<>();
        for (PlacedIndex placed : this.trie.placements()) {
            if (placed.path().stream().allMatch(domain -> this.trie.domain(domain).isPresent()))
                this.resolvePlacement(placed).ifPresent(resolved::add);
        }
        return resolved;
    }

    private Optional<ResolvedPlacement> resolvePlacement(PlacedIndex placed) {
        Placement placement = placed.placement();
        Optional<IHasSchema> relation = this.relation(placement.relation());
        if (relation.isEmpty()) {
            this.error(placement.position(), "unknown table or view `" + placement.relation() + "`");
            return Optional.empty();
        }
        int errorsBefore = this.errors.size();
        for (int index = 0; index < placement.vias().size(); index++)
            this.checkVia(placed, placement.vias().subList(0, index), placement.vias().get(index));

        // The key: one column per path domain
        List<Column> keyColumns = new ArrayList<>();
        Set<ProgramIdentifier> ownKeyColumns = new HashSet<>();
        for (String domainName : placed.path()) {
            Domain domain = this.trie.domain(domainName).orElseThrow();
            Optional<ColumnRef> own = this.ownColumn(domain, placement.relation());
            Optional<Via> via = placement.vias().stream().filter(v -> v.domain().equals(domainName)).findFirst();
            Optional<ColumnRef> keyRef;
            if (own.isPresent()) {
                keyRef = own;
                ownKeyColumns.add(this.canonical(own.get().column()));
            } else if (via.isPresent()) {
                keyRef = this.ownColumn(domain, via.get().relation());
            } else {
                this.error(placement.position(), "`" + placement.relation() + "` has no column in domain `"
                        + domainName + "`; add `[" + domainName + " via <Relation>]`");
                keyRef = Optional.empty();
            }
            Optional<Column> keyColumn = keyRef.flatMap(this::lookup);
            if (keyColumn.isEmpty())
                continue;
            if (keyColumn.get().type().mayBeNull)
                this.error(placement.position(), "key column `" + describe(keyRef.get())
                        + "` is nullable; declare it NOT NULL");
            keyColumns.add(keyColumn.get());
        }

        // Rule 5: the key contains the primary key, so folded keys are unique
        List<RelColumnMetadata> columns = relation.get().getColumns();
        Set<ProgramIdentifier> primaryKey = new HashSet<>();
        for (RelColumnMetadata column : columns) {
            if (column.isPrimaryKey)
                primaryKey.add(column.getName());
        }
        if (primaryKey.isEmpty()) {
            this.error(placement.position(), "`" + placement.relation()
                    + "` has no primary key, so its folded keys could repeat");
        } else {
            for (RelColumnMetadata column : columns) {
                if (column.isPrimaryKey && !ownKeyColumns.contains(column.getName()))
                    this.error(placement.position(), "the key of `" + placement.relation()
                            + "` lacks primary-key column `" + column.getName().name() + "`");
            }
        }

        // Rule 7: the payload
        List<Column> payloadColumns = new ArrayList<>();
        if (placement.payload() instanceof AllColumns) {
            if (!placement.vias().isEmpty() || !ownKeyColumns.equals(primaryKey))
                this.error(placement.position(), "`*` requires the key of `" + placement.relation()
                        + "` to be exactly its primary key; list the payload columns instead");
            for (RelColumnMetadata column : columns) {
                if (!ownKeyColumns.contains(column.getName()) && this.checkPacked(column, placement.position()))
                    payloadColumns.add(this.toColumn(placement.relation(), column));
            }
        } else {
            PayloadColumns listed = (PayloadColumns) placement.payload();
            Set<ProgramIdentifier> seen = new HashSet<>();
            for (Identifier name : listed.columns()) {
                Optional<RelColumnMetadata> column = this.column(relation.get(), name.name());
                if (column.isEmpty()) {
                    this.error(name.position(), "`" + placement.relation() + "` has no column `" + name.name() + "`");
                } else if (ownKeyColumns.contains(column.get().getName())) {
                    this.error(name.position(), "payload column `" + name.name() + "` is a key column");
                } else if (!seen.add(column.get().getName())) {
                    this.error(name.position(), "payload column `" + name.name() + "` is listed twice");
                } else if (this.checkPacked(column.get(), name.position())) {
                    payloadColumns.add(this.toColumn(placement.relation(), column.get()));
                }
            }
        }

        if (this.errors.size() > errorsBefore)
            return Optional.empty();
        return Optional.of(new ResolvedPlacement(placed, List.copyOf(keyColumns), List.copyOf(payloadColumns)));
    }

    /** Rule 4 for one {@code via} of a placement. {@code earlier} are the vias written before it. */
    private void checkVia(PlacedIndex placed, List<Via> earlier, Via via) {
        String relation = placed.placement().relation();
        Optional<Domain> domain = this.trie.domain(via.domain());
        if (domain.isEmpty()) {
            this.error(via.position(), "via domain `" + via.domain() + "` is not declared");
            return;
        }
        if (!placed.path().contains(via.domain()))
            this.error(via.position(), "via domain `" + via.domain() + "` of `" + relation
                    + "` is not on the path " + placed.path());
        if (earlier.stream().anyMatch(e -> e.domain().equals(via.domain())))
            this.error(via.position(), "`" + relation + "` has two vias for domain `" + via.domain() + "`");
        if (this.ownColumn(domain.get(), relation).isPresent())
            this.error(via.position(), "`" + relation + "` has its own column in domain `" + via.domain()
                    + "`, so it needs no via");

        // The via relation supplies the value: it has a column in the domain ...
        Optional<IHasSchema> viaRelation = this.relation(via.relation());
        if (viaRelation.isEmpty()) {
            this.error(via.position(), "unknown table or view `" + via.relation() + "`");
            return;
        }
        if (this.ownColumn(domain.get(), via.relation()).isEmpty())
            this.error(via.position(), "via relation `" + via.relation() + "` has no column in domain `"
                    + via.domain() + "`");

        // ... and each row of the placed relation refers to exactly one of its rows.
        List<RelColumnMetadata> viaPrimaryKey = viaRelation.get().getColumns().stream()
                .filter(column -> column.isPrimaryKey).toList();
        if (viaPrimaryKey.isEmpty())
            this.error(via.position(), "via relation `" + via.relation() + "` has no primary key");
        for (RelColumnMetadata keyColumn : viaPrimaryKey) {
            Optional<Domain> keyDomain = this.domainOf(via.relation(), keyColumn.getName());
            if (keyDomain.isEmpty() || this.ownColumn(keyDomain.get(), relation).isEmpty())
                this.error(via.position(), "primary-key column `" + via.relation() + "."
                        + keyColumn.getName().name() + "` of the via relation is in no domain where `"
                        + relation + "` has its own column");
        }
    }

    // Lookups

    private ProgramIdentifier canonical(String name) {
        return this.compiler.canonicalName(name, false);
    }

    private Optional<IHasSchema> relation(String name) {
        ProgramIdentifier id = this.canonical(name);
        ProgramMetadata metadata = this.compiler.metadata;
        if (metadata.hasTable(id))
            return Optional.of(metadata.getTableDescription(id));
        if (metadata.hasView(id))
            return Optional.of(metadata.getViewDescription(id));
        return Optional.empty();
    }

    private Optional<RelColumnMetadata> column(IHasSchema relation, String name) {
        int index = relation.getColumnIndex(this.canonical(name));
        return index < 0 ? Optional.empty() : Optional.of(relation.getColumns().get(index));
    }

    /** The column {@code ref} names, or empty if it does not exist, which {@link #checkDomains} reports. */
    private Optional<Column> lookup(ColumnRef ref) {
        return this.relation(ref.relation())
                .flatMap(relation -> this.column(relation, ref.column()))
                .map(column -> this.toColumn(ref.relation(), column));
    }

    /** The column of {@code relation} in {@code domain}, if any. */
    private Optional<ColumnRef> ownColumn(Domain domain, String relation) {
        return domain.columns().stream()
                .filter(ref -> this.canonical(ref.relation()).equals(this.canonical(relation)))
                .findFirst();
    }

    /** The domain that contains column {@code column} of {@code relation}, if any. */
    private Optional<Domain> domainOf(String relation, ProgramIdentifier column) {
        return this.trie.domains().stream()
                .filter(domain -> domain.columns().stream().anyMatch(ref ->
                        this.canonical(ref.relation()).equals(this.canonical(relation))
                                && this.canonical(ref.column()).equals(column)))
                .findFirst();
    }

    private Column toColumn(String relation, RelColumnMetadata column) {
        return new Column(relation, column.getName().name(), this.typeOf(column));
    }

    /** Reports, at {@code position}, a payload column whose values have no packed encoding.
     * @return whether the column can be packed. */
    private boolean checkPacked(RelColumnMetadata column, SpecPosition position) {
        DBSPType type = this.typeOf(column);
        String name = column.getName().name();
        if (type.mayBeNull) {
            this.error(position, "payload column `" + name + "` is nullable; packed payloads have no NULL");
            return false;
        }
        if (!PACKED_PAYLOAD_TYPES.contains(type.code)) {
            this.error(position, "payload column `" + name + "` has type " + typeName(column)
                    + ", which has no packed encoding; payload types are integers, BOOLEAN, DATE, TIME, "
                    + "TIMESTAMP, DECIMAL, and VARCHAR");
            return false;
        }
        return true;
    }

    private DBSPType typeOf(RelColumnMetadata column) {
        return this.compiler.getTypeCompiler().convertType(SourcePositionRange.INVALID, column.getType(), false);
    }

    /** The SQL type of the column {@code ref} names, which must exist. */
    private String sqlType(@Nullable ColumnRef ref) {
        return this.lookupMetadata(ref).map(MergedIndexValidator::typeName).orElse("unknown");
    }

    private Optional<RelColumnMetadata> lookupMetadata(@Nullable ColumnRef ref) {
        if (ref == null)
            return Optional.empty();
        return this.relation(ref.relation()).flatMap(relation -> this.column(relation, ref.column()));
    }

    private static String typeName(RelColumnMetadata column) {
        return column.getType().getFullTypeString();
    }

    private static String describe(@Nullable ColumnRef ref) {
        return ref == null ? "?" : ref.relation() + "." + ref.column();
    }

    private void error(SpecPosition position, String message) {
        this.errors.add(new SpecError(position, message));
    }
}
