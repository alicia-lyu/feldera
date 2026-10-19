//! Checks for client crates that implement [`MergedIndexDefinition`] by hand
//! or generate it. Failures panic, so these are for tests.

use super::{INDEX_TAG, MergedIndexDefinition, PlacedIndex};

/// Checks `D` against its own trie, for every key in `keys`.
///
/// Each key must round-trip through `fold` and `unfold`. The folded bytes must
/// also match one placement of `D::TRIE`: the final byte is the placement's
/// id, and the tags of the fields are the tags of the placement's path. This
/// ties the constants a definition hard-codes to the trie.
pub fn check_against_trie<D: MergedIndexDefinition>(keys: impl IntoIterator<Item = D::Key>) {
    let placements = D::TRIE.placements();
    for key in keys {
        let folded = D::fold(&key);
        assert_eq!(D::unfold(folded.as_bytes()).as_ref(), Ok(&key));

        let bytes = folded.as_bytes();
        let (&id, body) = bytes.split_last().expect("a folded key is not empty");
        let placed = placements
            .iter()
            .find(|placed| placed.id == id)
            .unwrap_or_else(|| panic!("{key:?} folds to placement id {id}, absent from the trie"));
        let fields = body
            .strip_suffix(&[INDEX_TAG])
            .expect("a folded key ends with the terminator and an id");
        let tags = path_tags::<D>(placed);
        assert!(
            has_tag_layout(fields, &tags),
            "{key:?} folds to {bytes:?}, which does not carry the tags {tags:?} of {}",
            placed.name()
        );
    }
}

/// The tags of a placement's path domains, in path order.
fn path_tags<D: MergedIndexDefinition>(placed: &PlacedIndex) -> Vec<u8> {
    placed
        .path
        .iter()
        .map(|domain| {
            D::TRIE
                .domain_tag(domain)
                .unwrap_or_else(|| panic!("domain {domain} has no tag"))
        })
        .collect()
}

/// Whether `fields` can be split into one `tag value` pair per entry of
/// `tags`. The widths of the values are not known here, so any split counts.
fn has_tag_layout(fields: &[u8], tags: &[u8]) -> bool {
    match tags.split_first() {
        None => fields.is_empty(),
        Some((&tag, later_tags)) => fields.split_first().is_some_and(|(&first, after_tag)| {
            first == tag
                && (1..=after_tag.len())
                    .any(|width| has_tag_layout(&after_tag[width..], later_tags))
        }),
    }
}
