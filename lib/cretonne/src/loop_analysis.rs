//! A loop analysis represented as mappings of loops to their header Ebb
//! and parent in the loop tree.

use dominator_tree::DominatorTree;
use entity::EntityMap;
use entity::{Keys, PrimaryMap};
use flowgraph::ControlFlowGraph;
use ir::{Ebb, Function, Layout};
use packed_option::PackedOption;
use std::vec::Vec;
use timing;

/// A opaque reference to a code loop.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Loop(u32);
entity_impl!(Loop, "loop");

/// Loop tree information for a single function.
///
/// Loops are referenced by the Loop object, and for each loop you can access its header EBB,
/// its eventual parent in the loop tree and all the EBB belonging to the loop.
pub struct LoopAnalysis {
    loops: PrimaryMap<Loop, LoopData>,
    ebb_loop_map: EntityMap<Ebb, PackedOption<Loop>>,
    valid: bool,
}

struct LoopData {
    header: Ebb,
    parent: PackedOption<Loop>,
}

impl LoopData {
    /// Creates a `LoopData` object with the loop header and its eventual parent in the loop tree.
    pub fn new(header: Ebb, parent: Option<Loop>) -> LoopData {
        LoopData {
            header: header,
            parent: parent.into(),
        }
    }
}

/// Methods for querying the loop analysis.
impl LoopAnalysis {
    /// Allocate a new blank loop analysis struct. Use `compute` to compute the loop analysis for
    /// a function.
    pub fn new() -> Self {
        Self {
            valid: false,
            loops: PrimaryMap::new(),
            ebb_loop_map: EntityMap::new(),
        }
    }

    /// Returns all the loops contained in a function.
    pub fn loops(&self) -> Keys<Loop> {
        self.loops.keys()
    }

    /// Returns the header EBB of a particular loop.
    ///
    /// The characteristic property of a loop header block is that it dominates some of its
    /// predecessors.
    pub fn loop_header(&self, lp: Loop) -> Ebb {
        self.loops[lp].header
    }

    /// Return the eventual parent of a loop in the loop tree.
    pub fn loop_parent(&self, lp: Loop) -> Option<Loop> {
        self.loops[lp].parent.expand()
    }

    /// Determine if an Ebb belongs to a loop by running a finger along the loop tree.
    ///
    /// Returns `true` if `ebb` is in loop `lp`.
    pub fn is_in_loop(&self, ebb: Ebb, lp: Loop) -> bool {
        let ebb_loop = self.ebb_loop_map[ebb];
        match ebb_loop.expand() {
            None => false,
            Some(ebb_loop) => self.is_child_loop(ebb_loop, lp),
        }
    }

    /// Determines if a loop is contained in another loop.
    ///
    /// `is_child_loop(child,parent)` returns `true` if and only if `child` is a child loop of
    /// `parent` (or `child == parent`).
    pub fn is_child_loop(&self, child: Loop, parent: Loop) -> bool {
        let mut finger = Some(child);
        while let Some(finger_loop) = finger {
            if finger_loop == parent {
                return true;
            }
            finger = self.loop_parent(finger_loop);
        }
        false
    }
}

impl LoopAnalysis {
    /// Detects the loops in a function. Needs the control flow graph and the dominator tree.
    pub fn compute(&mut self, func: &Function, cfg: &ControlFlowGraph, domtree: &DominatorTree) {
        let _tt = timing::loop_analysis();
        self.loops.clear();
        self.ebb_loop_map.clear();
        self.ebb_loop_map.resize(func.dfg.num_ebbs());
        self.find_loop_headers(cfg, domtree, &func.layout);
        self.discover_loop_blocks(cfg, domtree, &func.layout);
        self.valid = true;
    }

    /// Check if the loop analysis is in a valid state.
    ///
    /// Note that this doesn't perform any kind of validity checks. It simply checks if the
    /// `compute()` method has been called since the last `clear()`. It does not check that the
    /// loop analysis is consistent with the CFG.
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Clear all the data structures contanted in the loop analysis. This will leave the
    /// analysis in a similar state to a context returned by `new()` except that allocated
    /// memory be retained.
    pub fn clear(&mut self) {
        self.loops.clear();
        self.ebb_loop_map.clear();
        self.valid = false;
    }

    // Traverses the CFG in reverse postorder and create a loop object for every EBB having a
    // back edge.
    fn find_loop_headers(
        &mut self,
        cfg: &ControlFlowGraph,
        domtree: &DominatorTree,
        layout: &Layout,
    ) {
        // We traverse the CFG in reverse postorder
        for &ebb in domtree.cfg_postorder().iter().rev() {
            for (_, pred_inst) in cfg.pred_iter(ebb) {
                // If the ebb dominates one of its predecessors it is a back edge
                if domtree.dominates(ebb, pred_inst, layout) {
                    // This ebb is a loop header, so we create its associated loop
                    let lp = self.loops.push(LoopData::new(ebb, None));
                    self.ebb_loop_map[ebb] = lp.into();
                    break;
                    // We break because we only need one back edge to identify a loop header.
                }
            }
        }
    }

    // Intended to be called after `find_loop_headers`. For each detected loop header,
    // discovers all the ebb belonging to the loop and its inner loops. After a call to this
    // function, the loop tree is fully constructed.
    fn discover_loop_blocks(
        &mut self,
        cfg: &ControlFlowGraph,
        domtree: &DominatorTree,
        layout: &Layout,
    ) {
        let mut stack: Vec<Ebb> = Vec::new();
        // We handle each loop header in reverse order, corresponding to a pseudo postorder
        // traversal of the graph.
        for lp in self.loops().rev() {
            for (pred, pred_inst) in cfg.pred_iter(self.loops[lp].header) {
                // We follow the back edges
                if domtree.dominates(self.loops[lp].header, pred_inst, layout) {
                    stack.push(pred);
                }
            }
            while let Some(node) = stack.pop() {
                let continue_dfs: Option<Ebb>;
                match self.ebb_loop_map[node].expand() {
                    None => {
                        // The node hasn't been visited yet, we tag it as part of the loop
                        self.ebb_loop_map[node] = PackedOption::from(lp);
                        continue_dfs = Some(node);
                    }
                    Some(node_loop) => {
                        // We copy the node_loop into a mutable reference passed along the while
                        let mut node_loop = node_loop;
                        // The node is part of a loop, which can be lp or an inner loop
                        let mut node_loop_parent_option = self.loops[node_loop].parent;
                        while let Some(node_loop_parent) = node_loop_parent_option.expand() {
                            if node_loop_parent == lp {
                                // We have encounterd lp so we stop (already visited)
                                break;
                            } else {
                                //
                                node_loop = node_loop_parent;
                                // We lookup the parent loop
                                node_loop_parent_option = self.loops[node_loop].parent;
                            }
                        }
                        // Now node_loop_parent is either:
                        // - None and node_loop is an new inner loop of lp
                        // - Some(...) and the initial node_loop was a known inner loop of lp
                        match node_loop_parent_option.expand() {
                            Some(_) => continue_dfs = None,
                            None => {
                                if node_loop != lp {
                                    self.loops[node_loop].parent = lp.into();
                                    continue_dfs = Some(self.loops[node_loop].header)
                                } else {
                                    // If lp is a one-block loop then we make sure we stop
                                    continue_dfs = None
                                }
                            }
                        }
                    }
                }
                // Now we have handled the popped node and need to continue the DFS by adding the
                // predecessors of that node
                if let Some(continue_dfs) = continue_dfs {
                    for (pred, _) in cfg.pred_iter(continue_dfs) {
                        stack.push(pred)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod test {

    use cursor::{Cursor, FuncCursor};
    use dominator_tree::DominatorTree;
    use flowgraph::ControlFlowGraph;
    use ir::{types, Function, InstBuilder};
    use loop_analysis::{Loop, LoopAnalysis};
    use std::vec::Vec;

    #[test]
    fn nested_loops_detection() {
        let mut func = Function::new();
        let ebb0 = func.dfg.make_ebb();
        let ebb1 = func.dfg.make_ebb();
        let ebb2 = func.dfg.make_ebb();
        let ebb3 = func.dfg.make_ebb();
        let cond = func.dfg.append_ebb_param(ebb0, types::I32);

        {
            let mut cur = FuncCursor::new(&mut func);

            cur.insert_ebb(ebb0);
            cur.ins().jump(ebb1, &[]);

            cur.insert_ebb(ebb1);
            cur.ins().jump(ebb2, &[]);

            cur.insert_ebb(ebb2);
            cur.ins().brnz(cond, ebb1, &[]);
            cur.ins().jump(ebb3, &[]);

            cur.insert_ebb(ebb3);
            cur.ins().brnz(cond, ebb0, &[]);
        }

        let mut loop_analysis = LoopAnalysis::new();
        let mut cfg = ControlFlowGraph::new();
        let mut domtree = DominatorTree::new();
        cfg.compute(&func);
        domtree.compute(&func, &cfg);
        loop_analysis.compute(&func, &cfg, &domtree);

        let loops = loop_analysis.loops().collect::<Vec<Loop>>();
        assert_eq!(loops.len(), 2);
        assert_eq!(loop_analysis.loop_header(loops[0]), ebb0);
        assert_eq!(loop_analysis.loop_header(loops[1]), ebb1);
        assert_eq!(loop_analysis.loop_parent(loops[1]), Some(loops[0]));
        assert_eq!(loop_analysis.loop_parent(loops[0]), None);
        assert_eq!(loop_analysis.is_in_loop(ebb0, loops[0]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb0, loops[1]), false);
        assert_eq!(loop_analysis.is_in_loop(ebb1, loops[1]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb1, loops[0]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb2, loops[1]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb2, loops[0]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb3, loops[0]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb0, loops[1]), false);
    }

    #[test]
    fn complex_loop_detection() {
        let mut func = Function::new();
        let ebb0 = func.dfg.make_ebb();
        let ebb1 = func.dfg.make_ebb();
        let ebb2 = func.dfg.make_ebb();
        let ebb3 = func.dfg.make_ebb();
        let ebb4 = func.dfg.make_ebb();
        let ebb5 = func.dfg.make_ebb();
        let cond = func.dfg.append_ebb_param(ebb0, types::I32);

        {
            let mut cur = FuncCursor::new(&mut func);

            cur.insert_ebb(ebb0);
            cur.ins().brnz(cond, ebb1, &[]);
            cur.ins().jump(ebb3, &[]);

            cur.insert_ebb(ebb1);
            cur.ins().jump(ebb2, &[]);

            cur.insert_ebb(ebb2);
            cur.ins().brnz(cond, ebb1, &[]);
            cur.ins().jump(ebb5, &[]);

            cur.insert_ebb(ebb3);
            cur.ins().jump(ebb4, &[]);

            cur.insert_ebb(ebb4);
            cur.ins().brnz(cond, ebb3, &[]);
            cur.ins().jump(ebb5, &[]);

            cur.insert_ebb(ebb5);
            cur.ins().brnz(cond, ebb0, &[]);
        }

        let mut loop_analysis = LoopAnalysis::new();
        let mut cfg = ControlFlowGraph::new();
        let mut domtree = DominatorTree::new();
        cfg.compute(&func);
        domtree.compute(&func, &cfg);
        loop_analysis.compute(&func, &cfg, &domtree);

        let loops = loop_analysis.loops().collect::<Vec<Loop>>();
        assert_eq!(loops.len(), 3);
        assert_eq!(loop_analysis.loop_header(loops[0]), ebb0);
        assert_eq!(loop_analysis.loop_header(loops[1]), ebb1);
        assert_eq!(loop_analysis.loop_header(loops[2]), ebb3);
        assert_eq!(loop_analysis.loop_parent(loops[1]), Some(loops[0]));
        assert_eq!(loop_analysis.loop_parent(loops[2]), Some(loops[0]));
        assert_eq!(loop_analysis.loop_parent(loops[0]), None);
        assert_eq!(loop_analysis.is_in_loop(ebb0, loops[0]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb1, loops[1]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb2, loops[1]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb3, loops[2]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb4, loops[2]), true);
        assert_eq!(loop_analysis.is_in_loop(ebb5, loops[0]), true);
    }
}
