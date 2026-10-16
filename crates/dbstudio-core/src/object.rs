use serde::{Deserialize, Serialize};

use crate::config::ConnectionId;

/// What a node in the object navigator represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectKind {
    Connection,
    Database,
    Schema,
    Collection,
    Table,
    View,
    Column,
    Index,
    Function,
    Procedure,
    Trigger,
    User,
}

/// Address of a navigator node: a connection plus a path of engine-specific
/// segments, e.g. `["app", "public", "users"]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    pub connection: ConnectionId,
    pub path: Vec<String>,
}

impl ObjectRef {
    pub fn root(connection: ConnectionId) -> Self {
        Self {
            connection,
            path: Vec::new(),
        }
    }

    pub fn child(&self, segment: impl Into<String>) -> Self {
        let mut path = self.path.clone();
        path.push(segment.into());
        Self {
            connection: self.connection,
            path,
        }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.path.is_empty() {
            None
        } else {
            let mut path = self.path.clone();
            path.pop();
            Some(Self {
                connection: self.connection,
                path,
            })
        }
    }

    pub fn depth(&self) -> usize {
        self.path.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectNode {
    pub object: ObjectRef,
    pub kind: ObjectKind,
    /// Primary label shown in the tree.
    pub label: String,
    /// Secondary label, e.g. a column type.
    pub detail: Option<String>,
    pub has_children: bool,
}

impl ObjectNode {
    pub fn leaf(object: ObjectRef, kind: ObjectKind, label: impl Into<String>) -> Self {
        Self {
            object,
            kind,
            label: label.into(),
            detail: None,
            has_children: false,
        }
    }

    pub fn branch(object: ObjectRef, kind: ObjectKind, label: impl Into<String>) -> Self {
        Self {
            object,
            kind,
            label: label.into(),
            detail: None,
            has_children: true,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn object_ref_paths() {
        let id = Uuid::new_v4();
        let root = ObjectRef::root(id);
        let table = root.child("app").child("public").child("users");
        assert_eq!(table.path, vec!["app", "public", "users"]);
        assert_eq!(table.depth(), 3);
        assert_eq!(table.parent().unwrap().path, vec!["app", "public"]);
        assert!(root.parent().is_none());
    }
}
