/// Copyright 2021, MASSACHUSETTS INSTITUTE OF TECHNOLOGY
/// Subject to FAR 52.227-11 – Patent Rights – Ownership by the Contractor (May 2014)
/// SPDX-License-Identifier: MIT
use rustc::mir::{AllocationSet, Transaction, UniqueId};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

pub struct ConflictAnalysis {
    /// Map from every shared object to the transactions that use it.
    vertices: FxHashMap<UniqueId, FxHashSet<Transaction>>,
    /// Map where vertex u -> every vertex v it connects to
    edges: FxHashMap<UniqueId, FxHashSet<UniqueId>>,
}

impl ConflictAnalysis {
    pub fn new(allocation_sets: Vec<AllocationSet>) -> ConflictAnalysis {
        // Let K be the number of transactions.
        // Let |W| be the size of the largest set of shared objects. |W| = O(|V|)

        // Create vertices runs in O(K|W|).
        let mut vertices: FxHashMap<UniqueId, FxHashSet<Transaction>> = Default::default();
        let mut tx_to_objects: FxHashMap<Transaction, FxHashSet<UniqueId>> = Default::default();
        // O(K)
        for AllocationSet { lock, unlock, allocations } in &allocation_sets {
            // O(|W|)
            let is_write = allocations.iter().any(|&tx_use| tx_use.is_write);
            let transaction = Transaction { lock: *lock, unlock: *unlock, is_write };
            for transaction_use in allocations {
                vertices
                    .entry(transaction_use.shared_object)
                    .or_insert(Default::default())
                    .insert(transaction.clone());
                tx_to_objects
                    .entry(transaction.clone())
                    .or_insert(Default::default())
                    .insert(transaction_use.shared_object);
            }
        }

        let mut edges: FxHashMap<UniqueId, FxHashSet<UniqueId>> = Default::default();

        // Create edges runs in O(K|W|^2).
        // O(K)
        for shared_objects in tx_to_objects.values() {
            // O(|W|)
            for u in shared_objects.iter() {
                // O(|W|)
                for v in shared_objects.iter() {
                    if u != v {
                        debug!("[STM] adding edge {:?} <-> {:?}", u, v);
                        edges.entry(*u).or_insert(Default::default()).insert(*v);
                    }
                }
            }
        }

        let num_vertices = vertices.len();
        debug!("[STM] {} vertices", num_vertices);

        ConflictAnalysis { vertices, edges }
    }

    /// Compute the connected components of the graph to find the
    /// conflict sets for this program.
    pub fn perform(&self) -> Vec<FxHashSet<Transaction>> {
        let mut visited: FxHashSet<UniqueId> = Default::default();
        let mut conflict_sets: Vec<FxHashSet<Transaction>> = vec![];

        // DFS runs in O(|V| + |E|).
        for v in self.vertices.keys() {
            if !visited.contains(v) {
                let mut conflict_set: FxHashSet<Transaction> = Default::default();
                self.dfs_util(v, &mut visited, &mut conflict_set);
                conflict_sets.push(conflict_set);
            }
        }

        conflict_sets
    }

    fn dfs_util(
        &self,
        u: &UniqueId,
        visited: &mut FxHashSet<UniqueId>,
        conflict_set: &mut FxHashSet<Transaction>,
    ) {
        visited.insert(*u);
        let tx_ids = self.vertices.get(u).unwrap();
        conflict_set.extend(tx_ids.clone());
        if let Some(next) = self.edges.get(u) {
            for v in next {
                if !visited.contains(v) {
                    self.dfs_util(v, visited, conflict_set);
                }
            }
        }
    }
}
