use core::fmt;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt::{Debug, Display, Formatter},
    hash::Hash,
    ops::Deref,
    path::PathBuf,
    str::FromStr,
};

#[allow(unused_imports)]
use log::debug;

// TODO: It's annoying that users of this have to explicitly specify the ID type
// every time. It feels like we want that to be an associated type of the trait
// implementation. I tried that before and it didn't work, but this code was
// much less flexible back then, so could still be worh exploring.
pub trait GraphNode<I: Hash + Eq + Clone> {
    // Identifier for a node, unique among nodes in the set under consideration.
    fn id(&self) -> &I;
    // IDs of nodes that have an edge from this node to that node.
    fn child_ids(&self) -> &Vec<I>;
}

// Ajacency-list for a directed acyclic "graph" (dunno maybe incorrect
// terminology, it doesn't make any promises about connectedness so it might be
// zero or several actual "graphs"), where nodes are identified with a usize.
pub struct Dag<I: Hash + Eq + Clone + Debug, G: GraphNode<I>> {
    nodes: Vec<G>,
    // maps ids that nodes know about themselves to their index in `nodes`.
    _id_to_idx: HashMap<I, usize>,
    // edges[i] contains the destinations of the edges originating from node i.
    edges: Vec<Vec<usize>>,
    // indexes of nodes that aren't anyones child.
    root_idxs: Vec<usize>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum DagError<I> {
    // Two nodes had the same ID
    DuplicateId(I),
    // Node identified by `parent` referred to `child`, but the latter didn't exist.
    NoSuchChild { parent: I, child: I },
    // A cycle existed containing the node with this ID,
    Cycle(I),
}

impl<I: Debug> Display for DagError<I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            Self::DuplicateId(id) => write!(f, "duplicate key {:?}", id),
            Self::NoSuchChild { parent, child } => {
                write!(f, "{:?} refers to nonexistent {:?}", parent, child)
            }
            Self::Cycle(id) => write!(f, "cycle in graph, containing {:?}", id),
        }
    }
}

impl<I: Debug> Error for DagError<I> {}

impl<I: Hash + Eq + Clone + Debug, G: GraphNode<I>> Dag<I, G> {
    // TODO: unit test this.
    pub fn new(nodes: impl IntoIterator<Item = G>) -> Result<Self, DagError<I>>
    where
        G: GraphNode<I>,
    {
        let nodes: Vec<G> = nodes.into_iter().collect();

        // We eventually wanna have a vector and just index it by an integer, so
        // start by mapping the arbitrary "node IDs" to vec indexes.
        // At this point we also reject duplicates (this is why we don't just
        // wanna use `ollect`).
        let mut id_to_idx = HashMap::new();
        for (idx, node) in nodes.iter().enumerate() {
            let id = node.id();
            if id_to_idx.contains_key(id) {
                return Err(DagError::DuplicateId(id.clone()));
            }
            id_to_idx.insert(id.clone(), idx);
        }

        // Now build the adjacency list.
        let mut edges = Vec::new();
        for (idx, node) in nodes.iter().enumerate() {
            if idx >= edges.len() {
                edges.resize(idx + 1, Vec::new())
            }
            for child_id in node.child_ids() {
                let child_idx = id_to_idx
                    .get(child_id)
                    .ok_or_else(|| DagError::NoSuchChild {
                        parent: node.id().clone(),
                        child: child_id.clone(),
                    })?;
                edges[idx].push(*child_idx);
            }
        }

        // Now we validate the DAG (no cycles) and find root nodes.
        // Root nodes are those with no edges pointing to them.
        let mut root_idxs: HashSet<usize> = (0..edges.len()).collect();
        // This set is just used to avoid duplicating work.
        let mut visited: HashSet<usize> = HashSet::new();
        // This one actually detects cycles.
        let mut visited_stack: HashSet<usize> = HashSet::new();
        // This is a bit annoying in Rust because you cannot capture
        // environments into a named function but you cannot recurse into a
        // closure, so we just have to pass everything through args explicitly.
        // Returns the index of a node which was found to be part of a cycle
        // (in that case root_idxs won't be valid and we must bail).
        fn recurse(
            visited: &mut HashSet<usize>,
            visited_stack: &mut HashSet<usize>,
            start_idx: usize,
            edges: &Vec<Vec<usize>>,
            // Nodes will be removed from here if they are found to be another
            // node's child.
            root_idxs: &mut HashSet<usize>,
        ) -> Option<usize> {
            if visited_stack.contains(&start_idx) {
                return Some(start_idx);
            }
            if visited.contains(&start_idx) {
                // Already explored from this node and found no cycles.
                return None;
            }
            visited.insert(start_idx);
            visited_stack.insert(start_idx);
            for child in &edges[start_idx] {
                root_idxs.remove(child);
                if let Some(i) = recurse(visited, visited_stack, *child, edges, root_idxs) {
                    return Some(i);
                }
            }
            visited_stack.remove(&start_idx);
            None
        }
        for i in 0..edges.len() {
            if let Some(node_in_cycle) =
                recurse(&mut visited, &mut visited_stack, i, &edges, &mut root_idxs)
            {
                return Err(DagError::Cycle(nodes[node_in_cycle].id().clone()));
            }
        }

        Ok(Self {
            nodes,
            edges,
            _id_to_idx: id_to_idx,
            root_idxs: root_idxs.into_iter().collect(),
        })
    }

    // Iterate over nodes, visiting children before their parents.
    pub fn bottom_up(&self) -> BottomUp<'_, I, G> {
        BottomUp {
            dag: self,
            visit_stack: Vec::new(),
            unvisited_roots: self.root_idxs.clone(),
        }
    }

    pub fn nodes(&self) -> impl Iterator<Item = &G> + Clone {
        self.nodes.iter()
    }
}

pub struct BottomUp<'a, I: Hash + Eq + Clone + Debug, G: GraphNode<I>> {
    dag: &'a Dag<I, G>,
    visit_stack: Vec<usize>,
    unvisited_roots: Vec<usize>,
}

impl<'a, I: Hash + Eq + Clone + Debug, G: GraphNode<I>> Iterator for BottomUp<'a, I, G> {
    type Item = &'a G;

    fn next(&mut self) -> Option<&'a G> {
        // I found the basic non-recursive DFS post-order algorithm here:
        // https://codingots.medium.com/tree-traversal-without-recursion-221cbea6d004
        // This is a translation of that, where "s1" is temp_stack and s2 is
        // self.visit_stack. In that version there is only one root node but
        // here we have several.
        // First phase is to build up the stack of nodes to visit using
        // temp_stack as an intermediate.
        if self.visit_stack.is_empty() {
            let mut temp_stack = vec![self.unvisited_roots.pop()?];
            while let Some(cur_idx) = temp_stack.pop() {
                self.visit_stack.push(cur_idx);
                for child_idx in &self.dag.edges[cur_idx] {
                    temp_stack.push(*child_idx);
                }
            }
        }
        // Now we just cruise down the to_visit stack drinking a large bottle of
        // cranberry juice listening to Fleetwood Mac.
        Some(&self.dag.nodes[self.visit_stack.pop().unwrap()])
    }
}

#[derive(Clone)]
pub struct DisplayablePathBuf(pub PathBuf);

impl FromStr for DisplayablePathBuf {
    type Err = <PathBuf as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PathBuf::from_str(s).map(Self)
    }
}

impl Display for DisplayablePathBuf {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.0.display(), f)
    }
}

impl Deref for DisplayablePathBuf {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    struct TestGraphNode {
        id: usize,
        child_ids: Vec<usize>,
    }

    impl GraphNode<usize> for TestGraphNode {
        fn id(&self) -> &usize {
            &self.id
        }

        fn child_ids(&self) -> &Vec<usize> {
            &self.child_ids
        }
    }

    fn nodes(edges: impl IntoIterator<Item = Vec<usize>>) -> Vec<TestGraphNode> {
        edges
            .into_iter()
            .enumerate()
            .map(|(id, child_ids)| TestGraphNode { id, child_ids })
            .collect()
    }

    #[test_case(vec![], None; "empty")]
    #[test_case(nodes([vec![1], vec![]]), None; "one edge")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![]]), None; "tree")]
    #[test_case(nodes([vec![1], vec![2, 3], vec![], vec![],
                       vec![5], vec![6, 7], vec![], vec![]]), None; "trees")]
    #[test_case(nodes([vec![0]]), Some(DagError::Cycle(0)); "self-link")]
    // Note we don't actually care that the Cycle is reported on node 0, but
    // luckily that's stable behaviour so it's just easy to assert it that way.
    #[test_case(nodes([vec![1], vec![2], vec![3], vec![0]]), Some(DagError::Cycle(0)); "a loop")]
    #[test_case(nodes([vec![1]]), Some(DagError::NoSuchChild{parent: 0, child: 1}); "no child")]
    #[test_log::test]
    fn test_graph_validity(edges: Vec<TestGraphNode>, want_err: Option<DagError<usize>>) {
        assert_eq!(Dag::new(edges).err(), want_err);
    }
}
