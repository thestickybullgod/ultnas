//! Hierarchical namespace management.

use crate::error::UltnasCoreError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_SEGMENT_LEN: usize = 128;
const MAX_DEPTH: usize = 32;
const RESERVED: &[&str] = &["__ultnas__"];

/// A validated, hierarchical namespace path (e.g. `"projects/ultnas/docs"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct NamespacePath(Vec<String>);

impl NamespacePath {
    /// Parse from a `/`-delimited string. Returns an error on invalid segments.
    pub fn parse(s: &str) -> Result<Self, UltnasCoreError> {
        if s.is_empty() {
            return Ok(Self::root());
        }
        let segments: Vec<String> = s.split('/').map(|seg| seg.to_string()).collect();
        if segments.len() > MAX_DEPTH {
            return Err(UltnasCoreError::InvalidNamespace {
                reason: format!("exceeds maximum depth of {MAX_DEPTH}"),
            });
        }
        for seg in &segments {
            validate_segment(seg)?;
        }
        Ok(Self(segments))
    }

    /// The root namespace (zero-length path).
    pub fn root() -> Self {
        Self(vec![])
    }

    /// Number of path segments.
    pub fn depth(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if this is the root namespace.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Parent path, or `None` if already root.
    pub fn parent(&self) -> Option<NamespacePath> {
        if self.is_root() {
            None
        } else {
            Some(NamespacePath(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    /// Append a validated segment to produce a child path.
    pub fn child(&self, segment: &str) -> Result<NamespacePath, UltnasCoreError> {
        validate_segment(segment)?;
        if self.0.len() >= MAX_DEPTH {
            return Err(UltnasCoreError::InvalidNamespace {
                reason: format!("exceeds maximum depth of {MAX_DEPTH}"),
            });
        }
        let mut segs = self.0.clone();
        segs.push(segment.to_string());
        Ok(NamespacePath(segs))
    }

    /// `/`-joined string representation.
    pub fn as_str(&self) -> String {
        self.0.join("/")
    }

    /// Iterator over path segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|s| s.as_str())
    }
}

impl std::fmt::Display for NamespacePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

fn validate_segment(seg: &str) -> Result<(), UltnasCoreError> {
    if seg.is_empty() || seg.len() > MAX_SEGMENT_LEN {
        return Err(UltnasCoreError::InvalidNamespace {
            reason: format!("segment `{seg}` length must be 1–{MAX_SEGMENT_LEN}"),
        });
    }
    if RESERVED.contains(&seg) {
        return Err(UltnasCoreError::InvalidNamespace {
            reason: format!("segment `{seg}` is reserved"),
        });
    }
    if !seg
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(UltnasCoreError::InvalidNamespace {
            reason: format!("segment `{seg}` contains invalid characters (allowed: [a-zA-Z0-9_-])"),
        });
    }
    Ok(())
}

/// An in-memory B-tree of namespace paths.
#[derive(Debug, Default)]
pub struct NamespaceTree {
    nodes: BTreeMap<String, ()>,
}

impl NamespaceTree {
    /// Create an empty tree.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a namespace, creating intermediate nodes as needed.
    pub fn insert(&mut self, path: &NamespacePath) -> Result<(), UltnasCoreError> {
        // Insert all ancestors
        let mut current = NamespacePath::root();
        for seg in path.segments() {
            current = current.child(seg)?;
            self.nodes.insert(current.as_str(), ());
        }
        Ok(())
    }

    /// Check whether a namespace exists.
    pub fn contains(&self, path: &NamespacePath) -> bool {
        if path.is_root() {
            return true;
        }
        self.nodes.contains_key(&path.as_str())
    }

    /// List immediate children of a path.
    pub fn children(&self, path: &NamespacePath) -> Vec<NamespacePath> {
        let prefix = if path.is_root() {
            String::new()
        } else {
            format!("{}/", path.as_str())
        };
        self.nodes
            .keys()
            .filter(|k| {
                let without = k.strip_prefix(&prefix).unwrap_or("");
                !without.is_empty() && !without.contains('/')
            })
            .filter_map(|k| NamespacePath::parse(k).ok())
            .collect()
    }

    /// Total number of namespaces in the tree.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the tree contains no namespaces.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid() {
        let p = NamespacePath::parse("projects/ultnas/docs").unwrap();
        assert_eq!(p.depth(), 3);
        assert_eq!(p.as_str(), "projects/ultnas/docs");
    }

    #[test]
    fn root_is_empty() {
        let r = NamespacePath::root();
        assert!(r.is_root());
        assert_eq!(r.depth(), 0);
    }

    #[test]
    fn reserved_segment_rejected() {
        assert!(NamespacePath::parse("foo/__ultnas__/bar").is_err());
    }

    #[test]
    fn child_extends_path() {
        let p = NamespacePath::parse("a/b").unwrap();
        let c = p.child("c").unwrap();
        assert_eq!(c.as_str(), "a/b/c");
    }

    #[test]
    fn tree_insert_and_contains() {
        let mut tree = NamespaceTree::new();
        let p = NamespacePath::parse("a/b/c").unwrap();
        tree.insert(&p).unwrap();
        assert!(tree.contains(&p));
        assert!(tree.contains(&NamespacePath::parse("a/b").unwrap()));
        assert!(tree.contains(&NamespacePath::parse("a").unwrap()));
    }
}
