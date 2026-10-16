//! Lazy schema cache backing the object navigator and editor completion.
//!
//! Drivers fill the cache with `insert_children` as the user expands tree
//! nodes; the editor samples `completion_words` for the active connection.

use std::collections::{HashMap, HashSet};

use dbstudio_core::{ConnectionId, ObjectNode, ObjectRef};

#[derive(Debug, Default)]
pub struct SchemaCache {
    children: HashMap<(ConnectionId, Vec<String>), Vec<ObjectNode>>,
}

impl SchemaCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_children(&mut self, parent: &ObjectRef, nodes: Vec<ObjectNode>) {
        self.children
            .insert((parent.connection, parent.path.clone()), nodes);
    }

    pub fn children_of(&self, parent: &ObjectRef) -> Option<&[ObjectNode]> {
        self.children
            .get(&(parent.connection, parent.path.clone()))
            .map(Vec::as_slice)
    }

    /// Drop everything belonging to a connection (after disconnect).
    pub fn invalidate(&mut self, connection: ConnectionId) {
        self.children
            .retain(|(conn, _), _| *conn != connection);
    }

    /// Identifiers observed so far for `connection`, de-duplicated and sorted
    /// shortest-first (a cheap relevance heuristic for completion).
    pub fn completion_words(&self, connection: ConnectionId) -> Vec<String> {
        let mut seen = HashSet::new();
        for ((conn, _), nodes) in &self.children {
            if *conn != connection {
                continue;
            }
            for node in nodes {
                seen.insert(node.label.clone());
            }
        }
        let mut words: Vec<String> = seen.into_iter().collect();
        words.sort_by_key(|w| (w.len(), w.to_ascii_lowercase()));
        words
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbstudio_core::{ObjectKind, ObjectRef};
    use uuid::Uuid;

    #[test]
    fn caches_and_completes() {
        let id = Uuid::new_v4();
        let mut cache = SchemaCache::new();
        let root = ObjectRef::root(id);
        cache.insert_children(
            &root,
            vec![
                ObjectNode::leaf(root.child("users"), ObjectKind::Table, "users"),
                ObjectNode::leaf(root.child("orders"), ObjectKind::Table, "orders"),
            ],
        );
        assert_eq!(cache.children_of(&root).unwrap().len(), 2);
        let words = cache.completion_words(id);
        assert!(words.contains(&"users".to_string()));
        cache.invalidate(id);
        assert!(cache.children_of(&root).is_none());
    }
}
