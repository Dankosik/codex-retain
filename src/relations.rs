//! Removal ordering for organizational ownership and shared rollout history.

use anyhow::{Result, ensure};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Blocker {
    Children,
    History,
    Cycle,
}

impl fmt::Display for Blocker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Children => "a surviving child requires this parent",
            Self::History => "a surviving thread requires this history",
            Self::Cycle => "cyclic dependencies prevent ordered removal",
        })
    }
}

impl std::error::Error for Blocker {}

#[derive(Default)]
pub(crate) struct Dependencies {
    // Deduplicate a dependent/source pair even when both edge kinds apply.
    dependents: HashMap<Uuid, HashMap<Uuid, EdgeKinds>>,
    prerequisites: HashMap<Uuid, HashSet<Uuid>>,
    parents: HashMap<Uuid, HashSet<Uuid>>,
}

#[derive(Default)]
struct EdgeKinds {
    parent: bool,
    history: bool,
}

impl EdgeKinds {
    fn blocker(&self) -> Blocker {
        if self.parent {
            Blocker::Children
        } else {
            debug_assert!(self.history);
            Blocker::History
        }
    }
}

pub(crate) struct Plan {
    pub layers: Vec<Vec<Uuid>>,
    pub blocked: HashMap<Uuid, Blocker>,
}

impl Dependencies {
    pub(crate) fn add_parent(&mut self, child: Uuid, parent: Uuid) {
        self.parents.entry(child).or_default().insert(parent);
        self.dependents
            .entry(parent)
            .or_default()
            .entry(child)
            .or_default()
            .parent = true;
        self.prerequisites.entry(child).or_default().insert(parent);
    }

    pub(crate) fn add_history(&mut self, dependent: Uuid, source: Uuid) {
        self.dependents
            .entry(source)
            .or_default()
            .entry(dependent)
            .or_default()
            .history = true;
        self.prerequisites
            .entry(dependent)
            .or_default()
            .insert(source);
    }

    /// Produce deterministic leaves-first layers. Noncandidates remain alive
    /// and block every prerequisite reachable from them. Cycles stay untouched.
    pub(crate) fn plan(&self, candidates: &HashSet<Uuid>) -> Plan {
        let mut remaining: HashMap<_, _> = candidates
            .iter()
            .map(|&id| (id, self.dependents.get(&id).map_or(0, HashMap::len)))
            .collect();
        let mut ready: Vec<_> = remaining
            .iter()
            .filter_map(|(&id, &count)| (count == 0).then_some(id))
            .collect();
        let mut layers = Vec::new();
        while !ready.is_empty() {
            ready.sort_unstable();
            let layer = std::mem::take(&mut ready);
            for id in &layer {
                remaining.remove(id);
                if let Some(prerequisites) = self.prerequisites.get(id) {
                    for prerequisite in prerequisites {
                        if let Some(count) = remaining.get_mut(prerequisite) {
                            *count -= 1;
                            if *count == 0 {
                                ready.push(*prerequisite);
                            }
                        }
                    }
                }
            }
            layers.push(layer);
        }

        // Identify ordinary survivors first, then propagate that protection.
        // Remaining nodes unreachable from external survivors depend on cycles.
        let mut blocked = HashMap::new();
        let mut queue = VecDeque::new();
        for &id in remaining.keys() {
            if let Some(dependents) = self.dependents.get(&id) {
                for (dependent, kinds) in dependents {
                    if !candidates.contains(dependent) {
                        mark_blocked(&mut blocked, &mut queue, id, kinds.blocker());
                    }
                }
            }
        }
        while let Some(id) = queue.pop_front() {
            if let Some(prerequisites) = self.prerequisites.get(&id) {
                for &prerequisite in prerequisites {
                    if remaining.contains_key(&prerequisite) {
                        let kinds = &self.dependents[&prerequisite][&id];
                        mark_blocked(&mut blocked, &mut queue, prerequisite, kinds.blocker());
                    }
                }
            }
        }
        for id in remaining.into_keys() {
            blocked.entry(id).or_insert(Blocker::Cycle);
        }
        Plan { layers, blocked }
    }

    /// Organizational ancestors only; history sources do not own the caller.
    pub(crate) fn ancestors(&self, ids: &HashSet<Uuid>, limit: usize) -> Result<HashSet<Uuid>> {
        ensure!(
            ids.len() <= limit,
            "deletion guard set exceeds {limit} writer locks"
        );
        let mut ancestors = ids.clone();
        let mut queue: VecDeque<_> = ids.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            if let Some(parents) = self.parents.get(&id) {
                for &parent in parents {
                    if ancestors.insert(parent) {
                        ensure!(
                            ancestors.len() <= limit,
                            "deletion ancestry exceeds {limit} writer locks"
                        );
                        queue.push_back(parent);
                    }
                }
            }
        }
        Ok(ancestors)
    }

    /// Recheck a concrete group against a fresh graph before mutation.
    pub(crate) fn verify_removal(&self, deleting: &HashSet<Uuid>) -> Result<()> {
        let plan = self.plan(deleting);
        for reason in [Blocker::Children, Blocker::History, Blocker::Cycle] {
            if plan.blocked.values().any(|&blocked| blocked == reason) {
                return Err(reason.into());
            }
        }
        Ok(())
    }
}

fn mark_blocked(
    blocked: &mut HashMap<Uuid, Blocker>,
    queue: &mut VecDeque<Uuid>,
    id: Uuid,
    reason: Blocker,
) {
    match blocked.entry(id) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(reason);
            queue.push_back(id);
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            // Stable precedence when both ownership and history block a node.
            if reason == Blocker::Children {
                entry.insert(reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(number: u128) -> Uuid {
        Uuid::from_u128(number)
    }

    #[test]
    fn long_chain_removes_leaves_before_parents_without_recursion() {
        let mut graph = Dependencies::default();
        for number in 1..300 {
            graph.add_parent(id(number), id(number - 1));
        }
        let candidates = (0..300).map(id).collect();
        let plan = graph.plan(&candidates);
        assert!(plan.blocked.is_empty());
        assert_eq!(
            plan.layers,
            (0..300)
                .rev()
                .map(|number| vec![id(number)])
                .collect::<Vec<_>>()
        );
        assert_eq!(
            graph.ancestors(&HashSet::from([id(299)]), 512).unwrap(),
            candidates
        );
        assert!(graph.ancestors(&HashSet::from([id(299)]), 128).is_err());
    }

    #[test]
    fn surviving_parent_does_not_prevent_expired_child_removal() {
        let mut graph = Dependencies::default();
        graph.add_parent(id(2), id(1));
        let candidates = HashSet::from([id(2)]);
        let plan = graph.plan(&candidates);
        assert_eq!(plan.layers, vec![vec![id(2)]]);
        assert!(plan.blocked.is_empty());
        graph.verify_removal(&candidates).unwrap();
    }

    #[test]
    fn surviving_child_protects_every_ancestor() {
        let mut graph = Dependencies::default();
        graph.add_parent(id(3), id(2));
        graph.add_parent(id(2), id(1));
        let candidates = HashSet::from([id(1), id(2)]);
        let plan = graph.plan(&candidates);
        assert!(plan.layers.is_empty());
        assert_eq!(
            plan.blocked,
            HashMap::from([(id(1), Blocker::Children), (id(2), Blocker::Children)])
        );
        assert_eq!(
            graph
                .verify_removal(&candidates)
                .unwrap_err()
                .downcast_ref::<Blocker>(),
            Some(&Blocker::Children)
        );
    }

    #[test]
    fn history_is_directional_and_does_not_create_guard_ancestors() {
        let mut graph = Dependencies::default();
        graph.add_history(id(2), id(1));
        assert_eq!(
            graph.plan(&HashSet::from([id(1)])).blocked[&id(1)],
            Blocker::History
        );
        assert_eq!(
            graph.plan(&HashSet::from([id(2)])).layers,
            vec![vec![id(2)]]
        );
        assert_eq!(
            graph.ancestors(&HashSet::from([id(2)]), 128).unwrap(),
            HashSet::from([id(2)])
        );
        assert_eq!(
            graph
                .verify_removal(&HashSet::from([id(1)]))
                .unwrap_err()
                .downcast_ref::<Blocker>(),
            Some(&Blocker::History)
        );
        graph
            .verify_removal(&HashSet::from([id(1), id(2)]))
            .unwrap();
    }

    #[test]
    fn mixed_cycles_are_preserved_while_independent_leaves_are_removed() {
        let mut graph = Dependencies::default();
        graph.add_parent(id(1), id(2));
        graph.add_history(id(2), id(1));
        graph.add_parent(id(2), id(3));
        let plan = graph.plan(&HashSet::from([id(1), id(2), id(3), id(4)]));
        assert_eq!(plan.layers, vec![vec![id(4)]]);
        assert_eq!(
            plan.blocked,
            HashMap::from([
                (id(1), Blocker::Cycle),
                (id(2), Blocker::Cycle),
                (id(3), Blocker::Cycle)
            ])
        );
        assert_eq!(
            graph.ancestors(&HashSet::from([id(1)]), 128).unwrap(),
            HashSet::from([id(1), id(2), id(3)])
        );
        assert_eq!(
            graph
                .verify_removal(&HashSet::from([id(1), id(2)]))
                .unwrap_err()
                .downcast_ref::<Blocker>(),
            Some(&Blocker::Cycle)
        );
    }

    #[test]
    fn deduplicates_edges_and_orders_parallel_layers() {
        let mut graph = Dependencies::default();
        for _ in 0..3 {
            graph.add_parent(id(3), id(1));
            graph.add_history(id(3), id(1));
            graph.add_parent(id(2), id(1));
        }
        let plan = graph.plan(&HashSet::from([id(3), id(1), id(2)]));
        assert_eq!(plan.layers, vec![vec![id(2), id(3)], vec![id(1)]]);
        assert!(plan.blocked.is_empty());
    }

    #[test]
    fn orphan_dependents_block_sources_and_use_immediate_edge_kind() {
        let mut graph = Dependencies::default();
        graph.add_history(id(99), id(2));
        graph.add_parent(id(2), id(1));
        let plan = graph.plan(&HashSet::from([id(1), id(2)]));
        assert_eq!(
            plan.blocked,
            HashMap::from([(id(1), Blocker::Children), (id(2), Blocker::History)])
        );
        assert!(plan.layers.is_empty());
    }
}
