//! Intermediate representation of a circuit graph suitable for
//! conversion to a visual format like dot.

use serde::{Serialize, ser::SerializeStruct};
use std::{
    collections::BTreeMap,
    fmt::{self, Debug, Display, Write},
};

type Id = String;

/// Presentation category for an existing circuit operator or cluster.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CircuitNodeStyle {
    /// Persistent integral or its prior-state snapshot.
    Accumulation,
    /// Operator that retains state for another purpose.
    OtherState,
    /// Operator whose state behavior has not been classified.
    Unclassified,
}

impl CircuitNodeStyle {
    fn appearance(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Accumulation => (
                "I",
                "#dbeafe",
                "#2563eb",
                "Integrator state: historical collection or its delayed snapshot",
            ),
            Self::OtherState => (
                "S",
                "#ede9fe",
                "#7c3aed",
                "Other state: retains state for another purpose",
            ),
            Self::Unclassified => (
                "U",
                "#eeeeee",
                "#777777",
                "Unclassified: state behavior has not been classified",
            ),
        }
    }
}

/// Visual representation of a circuit graph.
///
/// The graph consists of a tree of cluster nodes populated with simple nodes.
#[derive(Clone, Default, Serialize)]
pub struct Graph {
    nodes: ClusterNode,
    edges: Vec<Edge>,
}

impl Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Graph {{ \"{}\" }}", self.to_dot())
    }
}

impl Graph {
    pub(super) fn new(nodes: ClusterNode, edges: Vec<Edge>) -> Self {
        Self { nodes, edges }
    }

    /// Convert graph to dot format.
    pub fn to_dot(&self) -> String {
        self.render_dot(None)
    }

    /// Convert graph to dot format with presentation styles keyed by existing
    /// operator or cluster ID.
    ///
    /// Missing IDs are rendered as unclassified. Entries for IDs absent from the
    /// graph have no effect on the output.
    pub fn to_dot_with_styles(&self, styles: &BTreeMap<String, CircuitNodeStyle>) -> String {
        self.render_dot(Some(styles))
    }

    fn render_dot(&self, styles: Option<&BTreeMap<String, CircuitNodeStyle>>) -> String {
        let mut output = String::with_capacity((self.nodes.nodes.len() + self.edges.len()) * 50);
        output.push_str("digraph {\nnode [shape=box]\n");

        self.nodes
            .to_dot(&mut output, styles)
            .expect("writing to a string should never fail");

        for edge in self.edges.iter() {
            edge.to_dot(&mut output)
                .expect("writing to a string should never fail");
        }

        output.push_str("}\n");
        output
    }

    /// Convert graph to JSON format.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

impl Display for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.nodes.to_plain(Indentation(0), f)
    }
}

#[derive(Clone)]
pub(super) struct SimpleNode {
    id: Id,
    label: String,
    color: f64,
}

impl Serialize for SimpleNode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("SimpleNode", 2)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("label", &self.label)?;
        state.end()
    }
}

impl SimpleNode {
    pub(super) fn new(id: Id, label: String, color: f64) -> Self {
        Self { id, label, color }
    }

    fn to_dot(
        &self,
        output: &mut dyn Write,
        styles: Option<&BTreeMap<String, CircuitNodeStyle>>,
    ) -> fmt::Result {
        if let Some(styles) = styles {
            let (marker, fillcolor, bordercolor, tooltip) = styles
                .get(&self.id)
                .copied()
                .unwrap_or(CircuitNodeStyle::Unclassified)
                .appearance();
            return writeln!(
                output,
                "{}[label=\"{}\\l[{marker}]\" fillcolor=\"{fillcolor}\" color=\"{bordercolor}\" tooltip=\"{tooltip}\" style=filled]",
                self.id, self.label,
            );
        }

        let r = 1.0 - self.color;
        let gb = (r * r * r * 255.0) as u8;

        writeln!(
            output,
            "{}[label=\"{}\" fillcolor=\"#ff{gb:02x}{gb:02x}\" style=filled]",
            self.id, self.label,
        )
    }
    fn to_plain(&self, indent: Indentation, f: &mut fmt::Formatter) -> fmt::Result {
        for line in self.label.split("\\l").filter(|line| !line.is_empty()) {
            writeln!(f, "{indent}{line}")?;
        }
        Ok(())
    }
}

/// A cluster node represents a subcircuit or a region.
// TODO:
// * Visually distinguish subcircuits from regions (e.g., dashed vs solid
//   boundaries).
#[derive(Clone, Default, Serialize)]
pub(super) struct ClusterNode {
    id: Id,
    label: String,
    nodes: Vec<Node>,
}

impl ClusterNode {
    pub(super) fn new(id: Id, label: String, nodes: Vec<Node>) -> Self {
        Self { id, label, nodes }
    }

    // TODO: We add a pair of enter/exit nodes to each cluster and connect all
    // incoming/outgoing edges whose destination/source is the entire cluster to
    // these nodes.  This does not look great.  We should instead connect each
    // edge to a specific simple node, which requires extending the circuit
    // builder API.
    fn to_dot(
        &self,
        output: &mut dyn Write,
        styles: Option<&BTreeMap<String, CircuitNodeStyle>>,
    ) -> fmt::Result {
        writeln!(output, "subgraph cluster_{} {{", self.id)?;
        if let Some(style) = styles.and_then(|styles| styles.get(&self.id)) {
            let (marker, fillcolor, bordercolor, tooltip) = style.appearance();
            writeln!(output, "label=\"{}\\l[{marker}]\"", self.label)?;
            writeln!(
                output,
                "style=filled bgcolor=\"{fillcolor}\" color=\"{bordercolor}\" tooltip=\"{tooltip}\""
            )?;
        } else {
            writeln!(output, "label=\"{}\"", self.label)?;
        }
        writeln!(output, "enter_{}[style=invis]", self.id)?;
        writeln!(output, "exit_{}[style=invis]", self.id)?;
        for node in self.nodes.iter() {
            node.to_dot(output, styles)?;
        }
        writeln!(output, "}}")?;

        Ok(())
    }

    fn to_plain(&self, indent: Indentation, f: &mut fmt::Formatter) -> fmt::Result {
        for line in self.label.split("\\l").filter(|line| !line.is_empty()) {
            writeln!(f, "{indent}{line}")?;
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            node.to_plain(Indentation(indent.0 + 4), f)?;
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
pub(super) enum Node {
    Simple(SimpleNode),
    Cluster(ClusterNode),
}

impl Node {
    pub(super) fn cluster(self) -> Option<ClusterNode> {
        match self {
            Self::Simple(_) => None,
            Self::Cluster(cluster_node) => Some(cluster_node),
        }
    }

    fn to_dot(
        &self,
        output: &mut dyn Write,
        styles: Option<&BTreeMap<String, CircuitNodeStyle>>,
    ) -> fmt::Result {
        match self {
            Self::Simple(simple_node) => simple_node.to_dot(output, styles),
            Self::Cluster(cluster_node) => cluster_node.to_dot(output, styles),
        }
    }

    fn to_plain(&self, indent: Indentation, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Simple(simple_node) => simple_node.to_plain(indent, f),
            Self::Cluster(cluster_node) => cluster_node.to_plain(indent, f),
        }
    }
}

#[derive(Clone, Serialize)]
pub(super) struct Edge {
    from_node: Id,
    // Is `from_node` a cluster?
    from_cluster: bool,
    to_node: Id,
    // Is `to_node` a cluster?
    to_cluster: bool,
}

impl Edge {
    pub(super) fn new(from_node: Id, from_cluster: bool, to_node: Id, to_cluster: bool) -> Self {
        Self {
            from_node,
            from_cluster,
            to_node,
            to_cluster,
        }
    }

    fn to_dot(&self, output: &mut dyn Write) -> fmt::Result {
        if self.from_cluster {
            write!(output, "exit_")?;
        }
        write!(output, "{} -> ", self.from_node)?;

        if self.to_cluster {
            write!(output, "enter_")?;
        }
        writeln!(output, "{}", self.to_node)
    }
}

struct Indentation(usize);
impl Display for Indentation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for _ in 0..self.0 {
            f.write_char(' ')?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CircuitNodeStyle, ClusterNode, Edge, Graph, Node, SimpleNode};
    use std::collections::BTreeMap;

    fn nested_graph() -> Graph {
        Graph::new(
            ClusterNode::new(
                "root".into(),
                "Root".into(),
                vec![
                    Node::Simple(SimpleNode::new("source".into(), "Source".into(), 0.5)),
                    Node::Cluster(ClusterNode::new(
                        "inner".into(),
                        "Inner".into(),
                        vec![
                            Node::Simple(SimpleNode::new(
                                "accumulate".into(),
                                "Accumulate".into(),
                                0.25,
                            )),
                            Node::Simple(SimpleNode::new("state".into(), "Trace".into(), 0.75)),
                        ],
                    )),
                ],
            ),
            vec![
                Edge::new("source".into(), false, "inner".into(), true),
                Edge::new("accumulate".into(), false, "state".into(), false),
                Edge::new("inner".into(), true, "source".into(), false),
            ],
        )
    }

    #[test]
    fn styled_dot_preserves_topology_and_marks_all_nodes() {
        let graph = nested_graph();
        let original = graph.to_dot();
        let styles = BTreeMap::from([
            ("inner".into(), CircuitNodeStyle::Accumulation),
            ("state".into(), CircuitNodeStyle::OtherState),
            ("absent".into(), CircuitNodeStyle::Accumulation),
        ]);
        let styled = graph.to_dot_with_styles(&styles);

        for structural_line in original
            .lines()
            .filter(|line| line.starts_with("subgraph ") || line.contains(" -> "))
        {
            assert!(styled.lines().any(|line| line == structural_line));
        }
        assert_eq!(
            original.matches("subgraph cluster_").count(),
            styled.matches("subgraph cluster_").count()
        );
        assert!(styled.contains("source[label=\"Source\\l[U]\""));
        assert!(styled.contains("subgraph cluster_inner {\nlabel=\"Inner\\l[I]\""));
        assert!(styled.contains(
            "style=filled bgcolor=\"#dbeafe\" color=\"#2563eb\" tooltip=\"Integrator state: historical collection or its delayed snapshot\""
        ));
        assert!(styled.contains("accumulate[label=\"Accumulate\\l[U]\""));
        assert!(
            styled.contains("state[label=\"Trace\\l[S]\" fillcolor=\"#ede9fe\" color=\"#7c3aed\"")
        );
        assert!(styled.contains("source -> enter_inner"));
        assert!(styled.contains("exit_inner -> source"));
        assert!(!styled.contains("absent["));
        assert_eq!(original, graph.to_dot());
        assert!(original.contains("subgraph cluster_inner {\nlabel=\"Inner\""));
        assert!(original.contains("source[label=\"Source\" fillcolor=\"#ff1f1f\" style=filled]"));
    }
}
