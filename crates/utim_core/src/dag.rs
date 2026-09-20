//! Asynchronous Directed Acyclic Graph (DAG) for unit dependency and ordering resolution.

use crate::unit::{SystemdUnit, UnitKind};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitState {
    Inactive,
    Activating,
    Active,
    Deactivating,
    Failed,
}

impl UnitState {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitState::Inactive => "inactive",
            UnitState::Activating => "activating",
            UnitState::Active => "active",
            UnitState::Deactivating => "deactivating",
            UnitState::Failed => "failed",
        }
    }
}

pub struct DagNode {
    pub unit: SystemdUnit,
    pub state: UnitState,
    pub pid: Option<i32>,
}

pub struct UnitDag {
    nodes: HashMap<String, DagNode>,
}

impl Default for UnitDag {
    fn default() -> Self {
        Self::new()
    }
}

impl UnitDag {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    pub fn insert(&mut self, unit: SystemdUnit) {
        let name = unit.name.clone();
        self.nodes.insert(
            name,
            DagNode {
                unit,
                state: UnitState::Inactive,
                pid: None,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&DagNode> {
        self.nodes.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut DagNode> {
        self.nodes.get_mut(name)
    }

    pub fn set_state(&mut self, name: &str, state: UnitState) {
        if let Some(node) = self.nodes.get_mut(name) {
            node.state = state;
        }
    }

    pub fn set_pid(&mut self, name: &str, pid: Option<i32>) {
        if let Some(node) = self.nodes.get_mut(name) {
            node.pid = pid;
        }
    }

    pub fn all_nodes(&self) -> &HashMap<String, DagNode> {
        &self.nodes
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Detect cycles using Tarjan's Strongly Connected Components (SCC) algorithm.
    /// Returns a list of cycles (components with > 1 node or a self-loop).
    pub fn detect_cycles(&self) -> Vec<Vec<String>> {
        struct TarjanContext<'a> {
            dag: &'a UnitDag,
            index: usize,
            indices: HashMap<String, usize>,
            lowlink: HashMap<String, usize>,
            on_stack: HashSet<String>,
            stack: Vec<String>,
            sccs: Vec<Vec<String>>,
        }

        impl<'a> TarjanContext<'a> {
            fn strongconnect(&mut self, v: &str) {
                self.indices.insert(v.to_string(), self.index);
                self.lowlink.insert(v.to_string(), self.index);
                self.index += 1;
                self.stack.push(v.to_string());
                self.on_stack.insert(v.to_string());

                let mut neighbors = Vec::new();
                if let Some(node) = self.dag.nodes.get(v) {
                    // Outgoing ordering edges: If A is After B, B comes before A (B -> A)
                    for after in &node.unit.unit.after {
                        if self.dag.nodes.contains_key(after) {
                            neighbors.push(after.clone());
                        }
                    }
                }

                for w in &neighbors {
                    if !self.indices.contains_key(w) {
                        self.strongconnect(w);
                        let w_low = self.lowlink[w];
                        let v_low = self.lowlink.get_mut(v).unwrap();
                        *v_low = (*v_low).min(w_low);
                    } else if self.on_stack.contains(w) {
                        let w_idx = self.indices[w];
                        let v_low = self.lowlink.get_mut(v).unwrap();
                        *v_low = (*v_low).min(w_idx);
                    }
                }

                if self.lowlink[v] == self.indices[v] {
                    let mut scc = Vec::new();
                    while let Some(w) = self.stack.pop() {
                        self.on_stack.remove(&w);
                        scc.push(w.clone());
                        if w == v {
                            break;
                        }
                    }
                    if scc.len() > 1 || (scc.len() == 1 && neighbors.contains(&scc[0])) {
                        self.sccs.push(scc);
                    }
                }
            }
        }

        let mut ctx = TarjanContext {
            dag: self,
            index: 0,
            indices: HashMap::new(),
            lowlink: HashMap::new(),
            on_stack: HashSet::new(),
            stack: Vec::new(),
            sccs: Vec::new(),
        };

        let keys: Vec<String> = self.nodes.keys().cloned().collect();
        for k in keys {
            if !ctx.indices.contains_key(&k) {
                ctx.strongconnect(&k);
            }
        }

        ctx.sccs
    }

    /// Compute topological ordering of units needed to reach `target_unit` using Kahn's algorithm.
    /// Breaks any cycle gracefully by dropping offending ordering constraints.
    pub fn resolve_start_queue(&self, target_unit: &str) -> Vec<String> {
        let mut needed: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        if self.nodes.contains_key(target_unit) {
            needed.insert(target_unit.to_string());
            queue.push_back(target_unit.to_string());
        }

        // Collect all transitive dependencies (Requires, Wants, BindsTo, After, and reverse WantedBy/RequiredBy)
        while let Some(curr) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&curr) {
                let mut deps = Vec::new();
                deps.extend(node.unit.unit.requires.clone());
                deps.extend(node.unit.unit.wants.clone());
                deps.extend(node.unit.unit.binds_to.clone());
                deps.extend(node.unit.unit.after.clone());

                // Find units that declare Before=curr
                for (other_name, other_node) in &self.nodes {
                    if other_node.unit.unit.before.contains(&curr) {
                        deps.push(other_name.clone());
                    }
                }

                for dep in deps {
                    if self.nodes.contains_key(&dep) && needed.insert(dep.clone()) {
                        queue.push_back(dep);
                    }
                }
            }
        }

        // Compute in-degrees for Kahn's topological sort based on 'After' and 'Before' ordering:
        // If A has After = [B], then B -> A (B must run before A), so in-degree of A is incremented.
        // If B has Before = [A], then B -> A as well.
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();

        for u in &needed {
            in_degree.insert(u.clone(), 0);
            adj.insert(u.clone(), Vec::new());
        }

        for u in &needed {
            if let Some(node) = self.nodes.get(u) {
                // u must run after 'after'
                for after in &node.unit.unit.after {
                    if needed.contains(after) {
                        adj.get_mut(after).unwrap().push(u.clone());
                        *in_degree.get_mut(u).unwrap() += 1;
                    }
                }
                // u must run before 'before'
                for before in &node.unit.unit.before {
                    if needed.contains(before) {
                        adj.get_mut(u).unwrap().push(before.clone());
                        *in_degree.get_mut(before).unwrap() += 1;
                    }
                }
            }
        }

        let mut ready: VecDeque<String> = VecDeque::new();
        for (u, &deg) in &in_degree {
            if deg == 0 {
                ready.push_back(u.clone());
            }
        }

        let mut sorted = Vec::new();
        while let Some(u) = ready.pop_front() {
            sorted.push(u.clone());
            if let Some(neighbors) = adj.get(&u) {
                for next in neighbors {
                    let deg = in_degree.get_mut(next).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        ready.push_back(next.clone());
                    }
                }
            }
        }

        // If cycle exists and some nodes remain with in-degree > 0, append them anyway
        for u in &needed {
            if !sorted.contains(u) {
                sorted.push(u.clone());
            }
        }

        sorted
    }

    /// Check which units in `Inactive` have all their `After` dependencies satisfied.
    pub fn ready_to_spawn(&self, pending: &[String]) -> Vec<String> {
        let mut spawnable = Vec::new();
        for name in pending {
            if let Some(node) = self.nodes.get(name) {
                if node.state != UnitState::Inactive {
                    continue;
                }

                // Check all After requirements
                let all_after_satisfied = node.unit.unit.after.iter().all(|after| {
                    if let Some(dep_node) = self.nodes.get(after) {
                        // Targets and oneshots count as satisfied if Active or Inactive (if succeeded)
                        dep_node.state == UnitState::Active
                            || (dep_node.unit.kind == UnitKind::Target && dep_node.state == UnitState::Active)
                    } else {
                        // Unloaded optional dependency treated as satisfied
                        true
                    }
                });

                if all_after_satisfied {
                    spawnable.push(name.clone());
                }
            }
        }
        spawnable
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::parse_unit;
    use std::path::Path;

    #[test]
    fn test_dag_kahn_topological_sort() {
        let mut dag = UnitDag::new();

        let basic_target = parse_unit(
            "basic.target",
            Path::new("/lib/systemd/system/basic.target"),
            "[Unit]\nDescription=Basic Target\n",
        );
        let sysinit_target = parse_unit(
            "sysinit.target",
            Path::new("/lib/systemd/system/sysinit.target"),
            "[Unit]\nDescription=System Initialization\nBefore=basic.target\n",
        );
        let udev_service = parse_unit(
            "systemd-udevd.service",
            Path::new("/lib/systemd/system/systemd-udevd.service"),
            "[Unit]\nDescription=udev Daemon\nAfter=sysinit.target\nBefore=basic.target\n",
        );
        let dbus_service = parse_unit(
            "dbus.service",
            Path::new("/lib/systemd/system/dbus.service"),
            "[Unit]\nDescription=D-Bus Daemon\nAfter=basic.target\n",
        );

        dag.insert(basic_target);
        dag.insert(sysinit_target);
        dag.insert(udev_service);
        dag.insert(dbus_service);

        let order = dag.resolve_start_queue("dbus.service");
        // sysinit must come before basic, udev after sysinit
        let idx_sysinit = order.iter().position(|x| x == "sysinit.target").unwrap();
        let idx_basic = order.iter().position(|x| x == "basic.target").unwrap();
        let idx_dbus = order.iter().position(|x| x == "dbus.service").unwrap();

        assert!(idx_sysinit < idx_basic);
        assert!(idx_basic < idx_dbus);
    }

    #[test]
    fn test_dag_cycle_detection() {
        let mut dag = UnitDag::new();

        let a = parse_unit("a.service", Path::new("/a.service"), "[Unit]\nAfter=b.service\n");
        let b = parse_unit("b.service", Path::new("/b.service"), "[Unit]\nAfter=c.service\n");
        let c = parse_unit("c.service", Path::new("/c.service"), "[Unit]\nAfter=a.service\n");

        dag.insert(a);
        dag.insert(b);
        dag.insert(c);

        let cycles = dag.detect_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 3);
    }
}
