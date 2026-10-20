// Rust guideline compliant 2026-10-20
//! Scope chains: the Rust translation of cordis' prototypal child contexts.
//!
//! A [`ScopeNode`] is an immutable persistent linked list. Deriving a child
//! context (`extend`/`isolate`/`intercept`) pushes a new node whose parent is
//! the current node; lookups walk the chain from the innermost node outward
//! and take the first hit, exactly like property shadowing on JS prototype
//! chains (paper Eq. 27-28, TS `Context.extend`).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::Value;

/// Label of the default (root) isolation realm.
pub const DEFAULT_LABEL: u64 = 0;

/// One immutable node of a scope chain.
#[derive(Debug, Default)]
pub struct ScopeNode {
    /// Enclosing scope, if this is not the root node.
    pub parent: Option<Arc<Self>>,
    /// Service name -> isolation label overrides introduced by this node.
    pub isolate: HashMap<String, u64>,
    /// Service name -> intercept config layers (innermost first).
    pub intercept: HashMap<String, VecDeque<Value>>,
}

impl ScopeNode {
    /// Creates the root scope node.
    #[must_use]
    pub fn root() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Derives a child node carrying one isolation override.
    #[must_use]
    pub fn isolated(parent: &Arc<Self>, name: &str, label: u64) -> Arc<Self> {
        let mut isolate = HashMap::new();
        isolate.insert(name.to_string(), label);
        Arc::new(Self {
            parent: Some(Arc::clone(parent)),
            isolate,
            intercept: HashMap::new(),
        })
    }

    /// Derives a child node carrying one intercept config layer.
    #[must_use]
    pub fn intercepted(parent: &Arc<Self>, name: &str, config: Value) -> Arc<Self> {
        let mut intercept = HashMap::new();
        intercept.insert(name.to_string(), VecDeque::from(vec![config]));
        Arc::new(Self {
            parent: Some(Arc::clone(parent)),
            isolate: HashMap::new(),
            intercept,
        })
    }

    /// Resolves the isolation label for `name`, walking outward to the root.
    #[must_use]
    pub fn effective_label(mut node: Option<&Arc<Self>>, name: &str) -> u64 {
        while let Some(current) = node {
            if let Some(label) = current.isolate.get(name) {
                return *label;
            }
            node = current.parent.as_ref();
        }
        DEFAULT_LABEL
    }

    /// Collects the intercept config layers for `name`, root-most first.
    ///
    /// TS merges ancestor entries first (`Service.resolveConfig`), so the
    /// returned iterator must be consumed in that order.
    #[must_use]
    pub fn intercept_layers<'a>(node: Option<&'a Arc<Self>>, name: &str) -> Vec<&'a Value> {
        let mut chain: Vec<&Arc<Self>> = Vec::new();
        let mut cursor = node;
        while let Some(current) = cursor {
            chain.push(current);
            cursor = current.parent.as_ref();
        }
        let mut layers = Vec::new();
        for scope in chain.iter().rev() {
            if let Some(queue) = scope.intercept.get(name) {
                for layer in queue {
                    layers.push(layer);
                }
            }
        }
        layers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effective_label_walks_innermost_first() {
        let root = ScopeNode::root();
        let mid = ScopeNode::isolated(&root, "db", 7);
        let leaf = ScopeNode::isolated(&mid, "cache", 9);
        assert_eq!(ScopeNode::effective_label(Some(&leaf), "db"), 7);
        assert_eq!(ScopeNode::effective_label(Some(&leaf), "cache"), 9);
        assert_eq!(
            ScopeNode::effective_label(Some(&leaf), "other"),
            DEFAULT_LABEL
        );
        assert_eq!(ScopeNode::effective_label(Some(&root), "db"), DEFAULT_LABEL);
    }

    #[test]
    fn isolation_shadowing_takes_nearest_node() {
        let root = ScopeNode::isolated(&ScopeNode::root(), "db", 1);
        let leaf = ScopeNode::isolated(&root, "db", 2);
        assert_eq!(ScopeNode::effective_label(Some(&leaf), "db"), 2);
        assert_eq!(ScopeNode::effective_label(Some(&root), "db"), 1);
    }

    #[test]
    fn intercept_layers_are_root_most_first() {
        let root = ScopeNode::intercepted(&ScopeNode::root(), "db", json!({"a": 1}));
        let mid = ScopeNode::intercepted(&root, "db", json!({"b": 2}));
        let leaf = ScopeNode::intercepted(&mid, "db", json!({"c": 3}));
        let layers = ScopeNode::intercept_layers(Some(&leaf), "db");
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], &json!({"a": 1}));
        assert_eq!(layers[2], &json!({"c": 3}));
    }
}
