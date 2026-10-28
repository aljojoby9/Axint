//! Commit graph indexed by commit id and by change id.
//!
//! A rebase inserts a new commit that keeps the original [`ChangeId`] and
//! increments `sequence_number`. The graph can list that evolution without
//! walking unrelated history.

use std::collections::{HashMap, HashSet, VecDeque};

use smallvec::SmallVec;

use crate::error::CoreError;
use crate::hash::AxintHash;
use crate::manifest::{ChangeId, CommitManifest};

/// One commit in the evolution graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitNode {
    /// Commit digest.
    pub commit_id: AxintHash,
    /// Stable change identity.
    pub change_id: ChangeId,
    /// Parent commits. A root has an empty list.
    pub parents: SmallVec<[AxintHash; 2]>,
    /// Generation of `change_id`.
    pub sequence_number: u64,
    /// First line is not special. The full message is stored.
    pub message: String,
    /// Root tree digest.
    pub root_tree: AxintHash,
}

impl CommitNode {
    /// Copies the graph fields out of a verified commit manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when a digest or the change id cannot be parsed.
    pub fn from_manifest(manifest: &CommitManifest) -> Result<Self, CoreError> {
        let parents = manifest.parents()?.into_iter().collect();
        Ok(Self {
            commit_id: manifest.id()?,
            change_id: manifest.change()?,
            parents,
            sequence_number: manifest.sequence_number,
            message: manifest.message.clone(),
            root_tree: manifest.root()?,
        })
    }
}

/// Directed graph of commits.
#[derive(Clone, Debug, Default)]
pub struct CommitGraph {
    nodes: HashMap<AxintHash, CommitNode>,
    by_change: HashMap<ChangeId, Vec<AxintHash>>,
}

impl CommitGraph {
    /// Creates an empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a commit. Inserting the same payload twice is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::DuplicateCommit`] when the digest is already bound
    /// to a different payload.
    pub fn insert(&mut self, node: CommitNode) -> Result<(), CoreError> {
        if let Some(existing) = self.nodes.get(&node.commit_id) {
            if existing != &node {
                return Err(CoreError::DuplicateCommit {
                    hash: node.commit_id.to_string(),
                });
            }
            return Ok(());
        }
        self.by_change.entry(node.change_id).or_default().push(node.commit_id);
        self.nodes.insert(node.commit_id, node);
        Ok(())
    }

    /// Borrows a commit.
    #[must_use]
    pub fn get(&self, id: &AxintHash) -> Option<&CommitNode> {
        self.nodes.get(id)
    }

    /// Reports whether the graph contains `id`.
    #[must_use]
    pub fn contains(&self, id: &AxintHash) -> bool {
        self.nodes.contains_key(id)
    }

    /// Commits that share a change id, oldest generation first.
    #[must_use]
    pub fn evolution(&self, change_id: &ChangeId) -> Vec<&CommitNode> {
        let mut nodes: Vec<&CommitNode> = self
            .by_change
            .get(change_id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.nodes.get(id))
            .collect();
        nodes.sort_by_key(|node| (node.sequence_number, node.commit_id));
        nodes
    }

    /// Next sequence number for a change. New changes start at zero.
    #[must_use]
    pub fn next_sequence(&self, change_id: &ChangeId) -> u64 {
        self.evolution(change_id)
            .into_iter()
            .map(|node| node.sequence_number)
            .max()
            .map_or(0, |sequence| sequence.saturating_add(1))
    }

    /// First-parent history, starting at `head`.
    #[must_use]
    pub fn first_parent_history(&self, head: &AxintHash) -> Vec<AxintHash> {
        let mut history = Vec::new();
        let mut cursor = Some(*head);
        let mut seen = HashSet::new();
        while let Some(id) = cursor {
            if !seen.insert(id) {
                break;
            }
            let Some(node) = self.nodes.get(&id) else {
                break;
            };
            history.push(id);
            cursor = node.parents.first().copied();
        }
        history
    }

    /// Reports whether `ancestor` is reachable by walking parents from `descendant`.
    ///
    /// A commit is an ancestor of itself.
    #[must_use]
    pub fn is_ancestor(&self, ancestor: &AxintHash, descendant: &AxintHash) -> bool {
        if ancestor == descendant {
            return true;
        }
        let mut queue = VecDeque::from([*descendant]);
        let mut seen = HashSet::new();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            if id == *ancestor {
                return true;
            }
            if let Some(node) = self.nodes.get(&id) {
                queue.extend(node.parents.iter().copied());
            }
        }
        false
    }

    /// Best merge bases of `left` and `right`.
    ///
    /// A best base is a common ancestor that is not an ancestor of another
    /// common ancestor. The result is ordered by digest so callers can pick a
    /// deterministic base when history criss-crosses.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingObject`] when either commit is absent.
    pub fn merge_bases(&self, left: &AxintHash, right: &AxintHash) -> Result<Vec<AxintHash>, CoreError> {
        self.require(left)?;
        self.require(right)?;
        if left == right {
            return Ok(vec![*left]);
        }
        let left_ancestors = self.ancestor_distances(left);
        let mut common = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([*right]);
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id) {
                continue;
            }
            if left_ancestors.contains_key(&id) {
                common.push(id);
            }
            if let Some(node) = self.nodes.get(&id) {
                queue.extend(node.parents.iter().copied());
            }
        }
        let mut best = Vec::new();
        for candidate in &common {
            let dominated = common
                .iter()
                .any(|other| other != candidate && self.is_ancestor(candidate, other));
            if !dominated {
                best.push(*candidate);
            }
        }
        best.sort();
        best.dedup();
        Ok(best)
    }

    /// One deterministic merge base.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::MissingObject`] when either commit is absent.
    pub fn merge_base(&self, left: &AxintHash, right: &AxintHash) -> Result<Option<AxintHash>, CoreError> {
        Ok(self.merge_bases(left, right)?.into_iter().next())
    }

    fn require(&self, id: &AxintHash) -> Result<(), CoreError> {
        if self.nodes.contains_key(id) {
            Ok(())
        } else {
            Err(CoreError::MissingObject { hash: id.to_string() })
        }
    }

    fn ancestor_distances(&self, start: &AxintHash) -> HashMap<AxintHash, u32> {
        let mut distances = HashMap::new();
        let mut queue = VecDeque::from([*start]);
        distances.insert(*start, 0_u32);
        while let Some(id) = queue.pop_front() {
            let Some(distance) = distances.get(&id).copied() else {
                continue;
            };
            let Some(node) = self.nodes.get(&id) else {
                continue;
            };
            for parent in &node.parents {
                if let std::collections::hash_map::Entry::Vacant(slot) = distances.entry(*parent) {
                    slot.insert(distance.saturating_add(1));
                    queue.push_back(*parent);
                }
            }
        }
        distances
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(byte: u8, change: ChangeId, sequence: u64, parents: &[u8]) -> CommitNode {
        CommitNode {
            commit_id: AxintHash::from_bytes([byte; 32]),
            change_id: change,
            parents: parents.iter().map(|parent| AxintHash::from_bytes([*parent; 32])).collect(),
            sequence_number: sequence,
            message: format!("c{byte}"),
            root_tree: AxintHash::ZERO,
        }
    }

    #[test]
    fn merge_base_of_a_fork_is_the_fork_point() -> Result<(), CoreError> {
        let mut graph = CommitGraph::new();
        let change_a = ChangeId::generate();
        let change_b = ChangeId::generate();
        let change_c = ChangeId::generate();
        graph.insert(node(1, change_a, 0, &[]))?;
        graph.insert(node(2, change_b, 0, &[1]))?;
        graph.insert(node(3, change_c, 0, &[1]))?;
        let base = graph.merge_base(&AxintHash::from_bytes([2; 32]), &AxintHash::from_bytes([3; 32]))?;
        assert_eq!(base, Some(AxintHash::from_bytes([1; 32])));
        assert!(graph.is_ancestor(&AxintHash::from_bytes([1; 32]), &AxintHash::from_bytes([3; 32])));
        Ok(())
    }

    #[test]
    fn change_evolution_keeps_rebase_generations() -> Result<(), CoreError> {
        let mut graph = CommitGraph::new();
        let change = ChangeId::generate();
        graph.insert(node(1, change, 0, &[]))?;
        graph.insert(node(2, change, 1, &[9]))?;
        let evolution = graph.evolution(&change);
        assert_eq!(evolution.len(), 2);
        assert_eq!(evolution[0].sequence_number, 0);
        assert_eq!(evolution[1].sequence_number, 1);
        assert_eq!(graph.next_sequence(&change), 2);
        assert_eq!(graph.next_sequence(&ChangeId::generate()), 0);
        Ok(())
    }
}
