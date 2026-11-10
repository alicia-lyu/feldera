//! The static key trie of a merged index.
//!
//! A [`KeyTrie`] is a nested value that mirrors the index spec in
//! `docs/design/merged-index/general-definition.md`. It stores only what the
//! spec says. Domain tags, placement ids, paths, names, and key columns are
//! not stored: every query below recomputes them by walking the tree in
//! pre-order. Tries are three or four levels deep, so clarity beats speed
//! here. The hot path, folding, uses constants instead.

/// The most domains and the most placements a trie may have.
/// Tags and ids are one byte each, and tag 0 is reserved for the terminator.
const MAX_TAGS: usize = u8::MAX as usize;

/// A column of a base relation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnRef {
    pub relation: &'static str,
    pub column: &'static str,
}

/// Columns that join on equal values, and so share one position in the order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Domain {
    pub name: &'static str,
    pub columns: &'static [ColumnRef],
    /// The byte width of the domain's encoded value: [`OrderedKey::WIDTH`]
    /// of its type. The trie has no types, so it cannot derive this.
    ///
    /// [`OrderedKey::WIDTH`]: super::OrderedKey::WIDTH
    pub width: usize,
}

/// Takes a key value for `domain` from the column of `relation` in that domain
/// instead of from the placed relation, which has no such column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Via {
    pub domain: &'static str,
    pub relation: &'static str,
}

/// What a placement stores besides its key. Column names only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Payload {
    /// Every non-key column, in table order. A primary index.
    All,
    /// Exactly these columns, in this order. A secondary index.
    Columns(&'static [&'static str]),
}

/// An index on `relation`, placed at the node that encloses it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placement {
    pub relation: &'static str,
    pub via: &'static [Via],
    pub payload: Payload,
}

/// A node of the trie. The edge into the node is the domain `domain`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node {
    pub domain: &'static str,
    pub placements: &'static [Placement],
    pub children: &'static [Node],
}

/// A merged index's trie. The root is implicit and has `children`, which
/// [`validate`](KeyTrie::validate) requires to be exactly one node: the root
/// domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyTrie {
    pub name: &'static str,
    pub domains: &'static [Domain],
    pub children: &'static [Node],
}

/// A placement together with what the pre-order walk knows about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedIndex {
    /// The placement's visit position in the walk, plus 1.
    pub id: u8,
    /// The domain names on the way from the root to the placement's node.
    pub path: Vec<&'static str>,
    pub placement: &'static Placement,
}

impl PlacedIndex {
    /// `<Relation>By<Path>`, with each path domain in CamelCase.
    pub fn name(&self) -> String {
        let mut name = format!("{}By", self.placement.relation);
        for domain in &self.path {
            name.push_str(&camel_case(domain));
        }
        name
    }

    /// A primary index carries all non-key columns of its relation.
    pub fn is_primary(&self) -> bool {
        self.placement.payload == Payload::All
    }

    /// One column per path domain: the relation's own column in that domain,
    /// or else the column of the `via` relation.
    ///
    /// # Panics
    ///
    /// Panics if some path domain does not resolve, which
    /// [`KeyTrie::validate`] rejects.
    pub fn key_columns(&self, trie: &KeyTrie) -> Vec<ColumnRef> {
        self.path
            .iter()
            .map(|domain| {
                trie.resolve_column(self.placement, domain)
                    .unwrap_or_else(|error| panic!("invalid key trie: {error}"))
            })
            .collect()
    }
}

/// `custkey` becomes `Custkey`, and `order_key` becomes `OrderKey`.
fn camel_case(snake_case: &str) -> String {
    let mut camel = String::with_capacity(snake_case.len());
    for word in snake_case.split('_').filter(|word| !word.is_empty()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            camel.extend(first.to_uppercase());
            camel.push_str(chars.as_str());
        }
    }
    camel
}

impl KeyTrie {
    /// Every placement in pre-order, children in the order they are written.
    ///
    /// # Panics
    ///
    /// Panics if there are more than 255 placements, which
    /// [`validate`](Self::validate) rejects.
    pub fn placements(&self) -> Vec<PlacedIndex> {
        let mut placed = Vec::new();
        visit_preorder(self.children, &mut Vec::new(), &mut |path, node| {
            for placement in node.placements {
                let id = u8::try_from(placed.len() + 1).expect("a trie has at most 255 placements");
                placed.push(PlacedIndex {
                    id,
                    path: path.to_vec(),
                    placement,
                });
            }
        });
        placed
    }

    /// The tag of a domain: its position of first appearance in the walk, plus 1.
    pub fn domain_tag(&self, domain: &str) -> Option<u8> {
        let mut appearances: Vec<&'static str> = Vec::new();
        visit_preorder(self.children, &mut Vec::new(), &mut |_, node| {
            if !appearances.contains(&node.domain) {
                appearances.push(node.domain);
            }
        });
        let position = appearances.iter().position(|seen| *seen == domain)?;
        u8::try_from(position + 1).ok()
    }

    /// The placement with the given derived name, such as `OrdersByCustkeyOrderkey`.
    pub fn placement(&self, name: &str) -> Option<PlacedIndex> {
        self.placements()
            .into_iter()
            .find(|placed| placed.name() == name)
    }

    /// The byte width of the value that follows the first tag of a key: the
    /// width of the one root domain. Every key begins with the root domain's
    /// tag and value, so a key's group is its first `1 + root_width()` bytes.
    ///
    /// # Panics
    ///
    /// Panics unless the trie has one top-level node whose domain is declared.
    /// [`validate`](Self::validate) rejects any other trie.
    pub fn root_width(&self) -> usize {
        let [root] = self.children else {
            panic!(
                "invalid key trie `{}`: {} top-level nodes, not one",
                self.name,
                self.children.len()
            );
        };
        self.domain(root.domain)
            .unwrap_or_else(|| panic!("invalid key trie: domain `{}` is not declared", root.domain))
            .width
    }

    /// Checks the rules that need no schema. Type, NOT NULL, and primary-key
    /// checks belong to the SQL compiler, which knows the schema.
    pub fn validate(&self) -> Result<(), String> {
        self.check_declared_domains()?;
        self.check_counts()?;
        self.check_one_root()?;
        self.validate_level(self.children, &mut Vec::new())
    }

    fn domain(&self, name: &str) -> Option<&'static Domain> {
        self.domains.iter().find(|domain| domain.name == name)
    }

    fn check_declared_domains(&self) -> Result<(), String> {
        for (position, domain) in self.domains.iter().enumerate() {
            if self.domains[..position]
                .iter()
                .any(|earlier| earlier.name == domain.name)
            {
                return Err(format!("domain `{}` is declared twice", domain.name));
            }
            if domain.width == 0 {
                return Err(format!("domain `{}` has width 0", domain.name));
            }
        }
        Ok(())
    }

    /// A key trie has exactly one root domain, so that a key's group is its
    /// first bytes. Two root domains need two key tries, each stored as its own
    /// merged index. Several streams may still reside at the root node.
    fn check_one_root(&self) -> Result<(), String> {
        match self.children.len() {
            1 => Ok(()),
            0 => Err(format!(
                "key trie `{}` has no top-level node; it needs exactly one root domain",
                self.name
            )),
            count => Err(format!(
                "key trie `{}` has {count} top-level nodes; it needs exactly one root domain, \
                 so use one key trie for each root domain",
                self.name
            )),
        }
    }

    fn check_counts(&self) -> Result<(), String> {
        if self.domains.len() > MAX_TAGS {
            return Err(format!("more than {MAX_TAGS} domains"));
        }
        let mut placement_count = 0;
        visit_preorder(self.children, &mut Vec::new(), &mut |_, node| {
            placement_count += node.placements.len();
        });
        if placement_count > MAX_TAGS {
            return Err(format!("more than {MAX_TAGS} placements"));
        }
        Ok(())
    }

    /// Checks the sibling nodes in `nodes`, then recurses into each of them.
    /// `path` holds the domains above `nodes`.
    fn validate_level(
        &self,
        nodes: &'static [Node],
        path: &mut Vec<&'static str>,
    ) -> Result<(), String> {
        for (position, node) in nodes.iter().enumerate() {
            if self.domain(node.domain).is_none() {
                return Err(format!("node domain `{}` is not declared", node.domain));
            }
            if nodes[..position]
                .iter()
                .any(|sibling| sibling.domain == node.domain)
            {
                return Err(format!(
                    "sibling nodes repeat domain `{}` below {path:?}",
                    node.domain
                ));
            }
            if path.contains(&node.domain) {
                return Err(format!(
                    "domain `{}` repeats on the path {path:?}",
                    node.domain
                ));
            }
            path.push(node.domain);
            self.validate_placements(node, path)?;
            self.validate_level(node.children, path)?;
            path.pop();
        }
        Ok(())
    }

    /// Checks the placements of `node`, whose path from the root is `path`.
    fn validate_placements(&self, node: &Node, path: &[&'static str]) -> Result<(), String> {
        for (position, placement) in node.placements.iter().enumerate() {
            if node.placements[..position]
                .iter()
                .any(|earlier| earlier.relation == placement.relation)
            {
                return Err(format!(
                    "relation `{}` is placed twice at {path:?}",
                    placement.relation
                ));
            }
            for (via_position, via) in placement.via.iter().enumerate() {
                if self.domain(via.domain).is_none() {
                    return Err(format!("via domain `{}` is not declared", via.domain));
                }
                if !path.contains(&via.domain) {
                    return Err(format!(
                        "via domain `{}` of `{}` is not on the path {path:?}",
                        via.domain, placement.relation
                    ));
                }
                if placement.via[..via_position]
                    .iter()
                    .any(|earlier| earlier.domain == via.domain)
                {
                    return Err(format!(
                        "`{}` has two vias for domain `{}`",
                        placement.relation, via.domain
                    ));
                }
            }
            for domain in path {
                self.resolve_column(placement, domain)?;
            }
        }
        Ok(())
    }

    /// The key column that gives `placement` its value in `domain`.
    fn resolve_column(
        &self,
        placement: &Placement,
        domain_name: &str,
    ) -> Result<ColumnRef, String> {
        let domain = self
            .domain(domain_name)
            .ok_or_else(|| format!("domain `{domain_name}` is not declared"))?;
        let relation = placement.relation;
        let mut own_columns = domain
            .columns
            .iter()
            .filter(|column| column.relation == relation);
        let own_column = own_columns.next();
        if own_columns.next().is_some() {
            return Err(format!(
                "`{relation}` has two columns in domain `{domain_name}`"
            ));
        }
        let via = placement.via.iter().find(|via| via.domain == domain_name);
        match (own_column, via) {
            (Some(column), None) => Ok(*column),
            (Some(_), Some(_)) => Err(format!(
                "`{relation}` has its own column in domain `{domain_name}` and also a via"
            )),
            (None, None) => Err(format!(
                "`{relation}` has no column in domain `{domain_name}` and no via"
            )),
            (None, Some(via)) => domain
                .columns
                .iter()
                .find(|column| column.relation == via.relation)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "via relation `{}` has no column in domain `{domain_name}`",
                        via.relation
                    )
                }),
        }
    }
}

/// Calls `visit` on every node in `nodes` and below, in pre-order. The first
/// argument is the node's path, which ends with its own domain.
fn visit_preorder(
    nodes: &'static [Node],
    path: &mut Vec<&'static str>,
    visit: &mut impl FnMut(&[&'static str], &'static Node),
) {
    for node in nodes {
        path.push(node.domain);
        visit(path, node);
        visit_preorder(node.children, path, visit);
        path.pop();
    }
}
