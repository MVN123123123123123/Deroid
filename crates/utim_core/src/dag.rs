//! Asynchronous Directed Acyclic Graph (DAG) for unit dependency and ordering resolution.

use crate::unit::SystemdUnit;
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

    /// Remove a unit (masking, unit file deletion on daemon-reload).
    /// Also drops reverse-dependency edges implicitly on next resolve.
    pub fn remove(&mut self, name: &str) -> bool {
        self.nodes.remove(name).is_some()
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

                let mut neighbors_set = HashSet::new();
                if let Some(node) = self.dag.nodes.get(v) {
                    for after in &node.unit.unit.after {
                        if self.dag.nodes.contains_key(after) {
                            neighbors_set.insert(after.clone());
                        }
                    }
                }
                for (other_name, other_node) in &self.dag.nodes {
                    // P1: zero-alloc borrowed comparison (no String alloc).
                    if other_node.unit.unit.before.iter().any(|b| b == v) {
                        neighbors_set.insert(other_name.clone());
                    }
                }
                let mut neighbors: Vec<String> = neighbors_set.into_iter().collect();
                neighbors.sort();

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

        // Collect all transitive requirements (Requires/Wants/BindsTo).
        // NOTE (B2): After=/Before= are ordering only, not requirements, and
        // must not pull unrelated units into the transaction (ordering edges
        // are built below).
        while let Some(curr) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&curr) {
                let mut deps = Vec::new();
                deps.extend(node.unit.unit.requires.iter().cloned());
                deps.extend(node.unit.unit.wants.iter().cloned());
                deps.extend(node.unit.unit.binds_to.iter().cloned());

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

        let mut edges: HashSet<(String, String)> = HashSet::new();

        for u in &needed {
            if let Some(node) = self.nodes.get(u) {
                // u must run after 'after': edge after -> u
                for after in &node.unit.unit.after {
                    if needed.contains(after) && edges.insert((after.clone(), u.clone())) {
                        adj.get_mut(after).unwrap().push(u.clone());
                        *in_degree.get_mut(u).unwrap() += 1;
                    }
                }
                // u must run before 'before': edge u -> before
                for before in &node.unit.unit.before {
                    if needed.contains(before) && edges.insert((u.clone(), before.clone())) {
                        adj.get_mut(u).unwrap().push(before.clone());
                        *in_degree.get_mut(before).unwrap() += 1;
                    }
                }
            }
        }

        let mut ready: VecDeque<String> = VecDeque::new();
        let mut ready_nodes: Vec<String> = Vec::new();
        for (u, &deg) in &in_degree {
            if deg == 0 {
                ready_nodes.push(u.clone());
            }
        }
        ready_nodes.sort();
        for u in ready_nodes {
            ready.push_back(u);
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
        // (sorted for determinism). Uses a HashSet for O(1) membership.
        let sorted_set: HashSet<&String> = sorted.iter().collect();
        let mut leftover: Vec<&String> =
            needed.iter().filter(|u| !sorted_set.contains(*u)).collect();
        leftover.sort();
        for u in leftover {
            sorted.push(u.clone());
        }

        sorted
    }

    /// Check which pending units have all their ordering constraints satisfied.
    /// After= deps count as satisfied when Active, Failed (non-required), or
    /// Inactive-and-not-pending (condition-skipped, C6). Before= only gates
    /// while the predecessor is pending.
    pub fn ready_to_spawn(&self, pending: &[String]) -> Vec<String> {
        let pending_set: HashSet<&String> = pending.iter().collect();
        let mut spawnable = Vec::new();
        for name in pending {
            if let Some(node) = self.nodes.get(name) {
                if node.state != UnitState::Inactive {
                    continue;
                }

                // Check all After requirements. Active always satisfies.
                // Failed satisfies only for non-required deps (Wants/After
                // degrade gracefully); a failed Requires/BindsTo blocks the
                // dependent so boot doesn't run on broken requirements.
                let all_after_satisfied = node.unit.unit.after.iter().all(|after| {
                    if let Some(dep_node) = self.nodes.get(after) {
                        if dep_node.state == UnitState::Active {
                            return true;
                        }
                        if dep_node.state == UnitState::Failed {
                            let required = node.unit.unit.requires.iter().any(|r| r == after)
                                || node.unit.unit.binds_to.iter().any(|b| b == after);
                            return !required;
                        }
                        // C6: a condition-skipped dep is Inactive AND removed
                        // from pending: it can never change again, so it must
                        // not block dependents (else one failed condition
                        // wedges the whole boot).
                        if dep_node.state == UnitState::Inactive && !pending_set.contains(after) {
                            return true;
                        }
                        false
                    } else {
                        // Unloaded optional dependency treated as satisfied
                        true
                    }
                });

                if !all_after_satisfied {
                    continue;
                }

                // A unit declaring Before=name only gates `name` while that
                // unit is itself pending in this transaction and not Active.
                let all_before_satisfied = self.nodes.iter().all(|(other_name, other_node)| {
                    if other_name == name {
                        return true;
                    }
                    if other_node.unit.unit.before.contains(name) {
                        if pending_set.contains(other_name) {
                            other_node.state == UnitState::Active
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                });

                if all_before_satisfied {
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
            "[Unit]\nDescription=Basic Target\nWants=sysinit.target\nAfter=sysinit.target\n",
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
            "[Unit]\nDescription=D-Bus Daemon\nWants=basic.target\nAfter=basic.target\n",
        );

        dag.insert(basic_target);
        dag.insert(sysinit_target);
        dag.insert(udev_service);
        dag.insert(dbus_service);

        let order = dag.resolve_start_queue("dbus.service");
        // sysinit must come before basic, basic before dbus (Wants+ordering)
        let idx_sysinit = order.iter().position(|x| x == "sysinit.target").unwrap();
        let idx_basic = order.iter().position(|x| x == "basic.target").unwrap();
        let idx_dbus = order.iter().position(|x| x == "dbus.service").unwrap();

        assert!(idx_sysinit < idx_basic);
        assert!(idx_basic < idx_dbus);

        // systemd parity: After-only units are ordering constraints, not
        // requirements — udev (After=sysinit.target only) is NOT pulled in.
        assert!(!order.contains(&"systemd-udevd.service".to_string()));
    }

    #[test]
    fn test_dag_cycle_detection() {
        let mut dag = UnitDag::new();

        let a = parse_unit(
            "a.service",
            Path::new("/a.service"),
            "[Unit]\nAfter=b.service\n",
        );
        let b = parse_unit(
            "b.service",
            Path::new("/b.service"),
            "[Unit]\nAfter=c.service\n",
        );
        let c = parse_unit(
            "c.service",
            Path::new("/c.service"),
            "[Unit]\nAfter=a.service\n",
        );

        dag.insert(a);
        dag.insert(b);
        dag.insert(c);

        let cycles = dag.detect_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 3);
    }

    #[test]
    fn test_dag_before_ordering_and_deduplication() {
        let mut dag = UnitDag::new();

        // Symmetrically declared Before and After with Wants pull:
        // A declares Before=B, and B declares Wants=A + After=A.
        // Edge must be deduplicated so in_degree of B is 1, not 2.
        let a = parse_unit(
            "a.service",
            Path::new("/a.service"),
            "[Unit]\nBefore=b.service\n",
        );
        let b = parse_unit(
            "b.service",
            Path::new("/b.service"),
            "[Unit]\nWants=a.service\nAfter=a.service\nBefore=c.service\n",
        );
        let c = parse_unit(
            "c.service",
            Path::new("/c.service"),
            "[Unit]\nWants=b.service\nAfter=b.service\n",
        );

        dag.insert(a);
        dag.insert(b);
        dag.insert(c);

        let queue = dag.resolve_start_queue("c.service");
        assert_eq!(queue, vec!["a.service", "b.service", "c.service"]);

        // Verify ready_to_spawn respects Before=
        let spawnable_init = dag.ready_to_spawn(&queue);
        // a.service can spawn, but b.service cannot spawn because a.service is not active yet!
        assert_eq!(spawnable_init, vec!["a.service"]);

        // Mark a active
        dag.set_state("a.service", UnitState::Active);
        let spawnable_step1 = dag.ready_to_spawn(&queue);
        assert_eq!(spawnable_step1, vec!["b.service"]);

        // Mark b active
        dag.set_state("b.service", UnitState::Active);
        let spawnable_step2 = dag.ready_to_spawn(&queue);
        assert_eq!(spawnable_step2, vec!["c.service"]);

        // Verify cycle detection with Before=
        let mut cycle_dag = UnitDag::new();
        let u1 = parse_unit(
            "u1.service",
            Path::new("/u1.service"),
            "[Unit]\nBefore=u2.service\n",
        );
        let u2 = parse_unit(
            "u2.service",
            Path::new("/u2.service"),
            "[Unit]\nBefore=u1.service\n",
        );
        cycle_dag.insert(u1);
        cycle_dag.insert(u2);
        let cycles = cycle_dag.detect_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].len(), 2);
    }
}
