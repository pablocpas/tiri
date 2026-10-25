//! Workspace identity and output affinity, independent of layout-tree mutations.

use std::rc::Rc;

use smithay::output::Output;
use smithay::utils::Transform;
use tiri_config::{LayoutPart, OutputName};

use crate::layout::container::NodeKey;
use crate::layout::Options;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::layout) enum WorkspaceIdentity {
    Anonymous,
    Numeric { number: u32, name: String },
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::layout) enum WorkspaceLifetime {
    Persistent,
    Transient,
}

#[derive(Debug, Clone)]
pub struct OutputId(pub(super) String);

impl OutputId {
    pub fn new(output: &Output) -> Self {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        Self(output_name.format_make_model_serial_or_connector())
    }

    pub fn matches(&self, output: &Output) -> bool {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        output_name.matches(&self.0)
    }
}

/// A workspace's public identity: the number its node in the layout's tree is known by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    pub(in crate::layout) fn of(workspace: NodeKey) -> Self {
        Self(workspace.get())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}

/// What a workspace is, as opposed to what it holds.
///
/// Kept on the workspace's node in the layout's tree, which is the one place it lives;
/// [`super::Workspace`] is a handle to that node plus the state of its animations and gestures.
#[derive(Debug, Clone)]
pub(in crate::layout) struct WorkspaceProperties {
    pub identity: WorkspaceIdentity,
    /// Whether the workspace survives being empty and not shown.
    pub lifetime: WorkspaceLifetime,
    /// The output the workspace belongs to, which it goes back to when that output connects.
    ///
    /// Usually the output it is on, but it outlives a disconnection.
    pub original_output: OutputId,
    /// The latest scale and transform of its output, kept while it has none.
    ///
    /// Which output it is on is the tree's to say, together with that output's list of
    /// workspaces; see `ContainerTree::workspace_output`.
    pub output_scale: smithay::output::Scale,
    pub transform: Transform,
    /// The workspace's own layout overrides.
    pub layout_config: Option<LayoutPart>,
    /// The options its monitor handed down, before the workspace's overrides are merged in.
    pub base_options: Rc<Options>,
}

impl WorkspaceProperties {
    /// An anonymous workspace that has never been on an output, for driving the tree alone.
    #[cfg(test)]
    pub fn detached(base_options: Rc<Options>, scale: f64) -> Self {
        Self {
            identity: WorkspaceIdentity::Anonymous,
            lifetime: WorkspaceLifetime::Transient,
            original_output: OutputId(String::new()),
            output_scale: smithay::output::Scale::Fractional(scale),
            transform: Transform::Normal,
            layout_config: None,
            base_options,
        }
    }

    pub fn name(&self) -> Option<&String> {
        match &self.identity {
            WorkspaceIdentity::Anonymous => None,
            WorkspaceIdentity::Numeric { name, .. } | WorkspaceIdentity::Named(name) => Some(name),
        }
    }

    pub fn numeric_number(&self) -> Option<u32> {
        match &self.identity {
            WorkspaceIdentity::Numeric { number, .. } => Some(*number),
            WorkspaceIdentity::Anonymous | WorkspaceIdentity::Named(_) => None,
        }
    }

    pub fn has_persistent_identity(&self) -> bool {
        self.name().is_some() && self.lifetime == WorkspaceLifetime::Persistent
    }

    pub fn set_numeric_identity(&mut self, number: u32, lifetime: WorkspaceLifetime) {
        self.identity = WorkspaceIdentity::Numeric {
            number,
            name: number.to_string(),
        };
        self.lifetime = lifetime;
    }

    pub fn set_name(&mut self, name: String, lifetime: WorkspaceLifetime) {
        self.identity = WorkspaceIdentity::Named(name);
        self.lifetime = lifetime;
    }

    pub fn unname(&mut self) {
        self.identity = WorkspaceIdentity::Anonymous;
        self.lifetime = WorkspaceLifetime::Transient;
    }
}
