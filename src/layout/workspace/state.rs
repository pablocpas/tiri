//! Workspace identity and output affinity, independent of layout-tree mutations.

use smithay::output::Output;
use tiri_config::OutputName;

use crate::utils::id::IdCounter;

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

static WORKSPACE_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    pub(super) fn next() -> Self {
        Self(WORKSPACE_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}
