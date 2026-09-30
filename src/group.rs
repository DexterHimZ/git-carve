//! Grouping. M1 ships only the trivial grouping (one group per file, joined by hard binds);
//! the code graph (SPEC §6) replaces it in M2.

use crate::atom::{AtomId, Changes};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Commit order; each group becomes one commit.
    pub groups: Vec<Vec<AtomId>>,
    /// Atoms that stay uncommitted in the working tree.
    pub leave: Vec<AtomId>,
}

pub fn per_file(changes: &Changes) -> Plan {
    let mut sets = UnionFind::new(changes.atoms.len());
    for atoms in &changes.file_atoms {
        for pair in atoms.windows(2) {
            sets.union(pair[0], pair[1]);
        }
    }
    for bind in &changes.binds {
        sets.union(bind.a.0 as usize - 1, bind.b.0 as usize - 1);
    }
    let mut components: Vec<Vec<usize>> = Vec::new();
    let mut slot = vec![usize::MAX; changes.atoms.len()];
    for i in 0..changes.atoms.len() {
        let root = sets.find(i);
        if slot[root] == usize::MAX {
            slot[root] = components.len();
            components.push(Vec::new());
        }
        components[slot[root]].push(i);
    }
    // Components come out ordered by their smallest atom, which is (path, base line) order.
    let mut plan = Plan {
        groups: Vec::new(),
        leave: Vec::new(),
    };
    for component in components {
        let ids = component.iter().map(|&i| changes.atoms[i].id);
        if component.iter().all(|&i| changes.atoms[i].leave_by_default) {
            plan.leave.extend(ids);
        } else {
            plan.groups.push(ids.collect());
        }
    }
    plan
}

pub struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    pub fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    /// Joins two sets; the smaller root wins, which keeps results deterministic.
    pub fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a.max(b)] = a.min(b);
        }
    }
}
