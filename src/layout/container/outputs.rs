//! The outputs as the layout's tree knows them.
//!
//! sway's `sway_output` keeps the workspaces on it in a list, and each `sway_workspace`
//! points back at its output. Both sides are kept here, behind one API, so that "which
//! workspaces are on this output, in what order" and "which output is this workspace on" are
//! one fact stored twice for lookup and never two facts that can disagree.
//!
//! As in sway, an output is not a node: a container's `parent` stops at its workspace, and no
//! walk up the tree has to step over an output on the way.

use std::collections::HashMap;

use smithay::output::Output;

use super::{ContainerTree, LayoutElement, NodeKey};
use crate::utils::id::IdCounter;

static OUTPUT_ID_COUNTER: IdCounter = IdCounter::new();

/// Stable identity of an output in the layout's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputKey(u64);

impl OutputKey {
    fn next() -> Self {
        Self(OUTPUT_ID_COUNTER.next())
    }
}

/// One output's part of the model: the workspaces on it, in order.
#[derive(Debug)]
pub(in crate::layout) struct OutputEntry {
    /// The output, or `None` for the fallback: sway's `root->fallback_output`, which holds the
    /// workspaces while no output is connected.
    output: Option<Output>,
    workspaces: Vec<NodeKey>,
}

#[derive(Debug, Default)]
pub(in crate::layout) struct Outputs {
    entries: HashMap<OutputKey, OutputEntry>,
}

impl<W: LayoutElement> ContainerTree<W> {
    fn output_entry(&self, output: OutputKey) -> &OutputEntry {
        self.outputs
            .entries
            .get(&output)
            .expect("output key must name an output of this tree")
    }

    fn output_entry_mut(&mut self, output: OutputKey) -> &mut OutputEntry {
        self.outputs
            .entries
            .get_mut(&output)
            .expect("output key must name an output of this tree")
    }

    /// Add an output with no workspaces on it yet; `None` adds the fallback.
    pub(in crate::layout) fn add_output(&mut self, output: Option<Output>) -> OutputKey {
        let key = OutputKey::next();
        self.outputs.entries.insert(
            key,
            OutputEntry {
                output,
                workspaces: Vec::new(),
            },
        );
        key
    }

    /// Remove an output, taking its workspaces off it. They are returned in order and are on
    /// no output afterwards.
    pub(in crate::layout) fn remove_output(&mut self, output: OutputKey) -> Vec<NodeKey> {
        let entry = self
            .outputs
            .entries
            .remove(&output)
            .expect("output key must name an output of this tree");
        for &workspace in &entry.workspaces {
            self.workspace_data_mut(workspace).output = None;
        }
        entry.workspaces
    }

    /// The workspaces on `output`, in order.
    pub(in crate::layout) fn output_workspaces(&self, output: OutputKey) -> &[NodeKey] {
        &self.output_entry(output).workspaces
    }

    /// The output `workspace` is on, if any.
    pub(in crate::layout) fn workspace_output_key(&self, workspace: NodeKey) -> Option<OutputKey> {
        self.workspace_data(workspace).output
    }

    /// The output `workspace` is on, if it is on a connected one.
    pub(in crate::layout) fn workspace_output(&self, workspace: NodeKey) -> Option<&Output> {
        self.workspace_output_key(workspace)
            .and_then(|output| self.output_entry(output).output.as_ref())
    }

    /// Put `workspace`, which is on no output, on `output` at `idx`.
    pub(in crate::layout) fn attach_workspace(
        &mut self,
        output: OutputKey,
        idx: usize,
        workspace: NodeKey,
    ) {
        let data = self.workspace_data_mut(workspace);
        assert!(
            data.output.is_none(),
            "a workspace must leave its output before it joins another"
        );
        data.output = Some(output);
        self.output_entry_mut(output)
            .workspaces
            .insert(idx, workspace);
    }

    /// Take the workspace at `idx` off `output`. It is on no output afterwards.
    pub(in crate::layout) fn detach_workspace(&mut self, output: OutputKey, idx: usize) -> NodeKey {
        let workspace = self.output_entry_mut(output).workspaces.remove(idx);
        self.workspace_data_mut(workspace).output = None;
        workspace
    }

    /// Reorder the workspaces on `output`.
    ///
    /// The callback gets a slice, so it can permute the workspaces and nothing else: what is
    /// on the output does not change, only the order.
    pub(in crate::layout) fn reorder_output_workspaces<R>(
        &mut self,
        output: OutputKey,
        reorder: impl FnOnce(&mut [NodeKey]) -> R,
    ) -> R {
        let workspaces = &mut self.output_entry_mut(output).workspaces;
        #[cfg(debug_assertions)]
        let before = {
            let mut before = workspaces.clone();
            before.sort();
            before
        };
        let result = reorder(workspaces);
        #[cfg(debug_assertions)]
        {
            let mut after = workspaces.clone();
            after.sort();
            assert_eq!(before, after, "a reorder must keep the same workspaces");
        }
        result
    }

    /// Take `workspace` off whichever output it is on.
    pub(in crate::layout) fn detach_workspace_from_its_output(&mut self, workspace: NodeKey) {
        let Some(output) = self.workspace_output_key(workspace) else {
            return;
        };
        let idx = self
            .output_workspaces(output)
            .iter()
            .position(|key| *key == workspace)
            .expect("a workspace's output must list it");
        self.detach_workspace(output, idx);
    }

    /// Every output lists exactly the workspaces that point back at it.
    pub(in crate::layout) fn verify_outputs(&self) {
        let mut listed = HashMap::new();
        for (&output, entry) in &self.outputs.entries {
            for &workspace in &entry.workspaces {
                assert!(
                    listed.insert(workspace, output).is_none(),
                    "a workspace must be listed on one output once"
                );
                assert_eq!(
                    self.workspace_output_key(workspace),
                    Some(output),
                    "a listed workspace must point back at its output"
                );
            }
        }
        for &workspace in &self.workspaces {
            if let Some(output) = self.workspace_output_key(workspace) {
                assert_eq!(
                    listed.get(&workspace),
                    Some(&output),
                    "a workspace pointing at an output must be listed there"
                );
            }
        }
    }
}
