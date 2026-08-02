//! Running a script against tiri and observing it after every command.
//!
//! Observation goes through the same IPC projection the compositor serves to real clients,
//! then through the shared normalizer. Using a test-only projection instead would mean
//! comparing sway against something no user can see, which is how a parity suite ends up
//! agreeing with itself.

use tiri_ipc::LayoutTreeRect;
use tiri_parity::{erase_decoration, tiri as tiri_model, Layout as ModelLayout, Workspace};

use crate::animation::Clock;
use crate::layout::container::Layout as TreeLayout;
use crate::layout::tests::{Op, TestWindow};
use crate::layout::{Layout, Options};
use std::time::Duration;

use super::script;

/// A script, and what tiri looked like after each of its commands.
pub(crate) struct Replay {
    pub steps: Vec<Observation>,
}

pub(crate) struct Observation {
    pub command: String,
    pub model: Workspace,
}

/// Run a script and observe after every command.
///
/// Observing per command is the point: it localizes a divergence to the one command that
/// caused it, instead of leaving a forty-step sequence to bisect by hand.
#[track_caller]
pub(crate) fn replay(text: &str) -> Replay {
    let steps = match script::parse(text) {
        Ok(steps) => steps,
        Err(err) => panic!("{err}"),
    };

    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), pinned_options());
    Op::AddOutput(1).apply(&mut layout);

    let window_count = steps
        .iter()
        .filter(|step| matches!(step.op, Op::AddWindow { .. }))
        .count();

    let mut observations = Vec::with_capacity(steps.len());
    for step in steps {
        step.op.apply(&mut layout);
        layout.verify_invariants();
        settle(&mut layout, window_count);
        observations.push(Observation {
            command: step.command,
            model: observe(&layout),
        });
    }

    Replay {
        steps: observations,
    }
}

impl Replay {
    /// Each command followed by what the workspace looked like after it, with decoration
    /// erased — the same view a recording is compared against.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            let mut model = step.model.clone();
            erase_decoration(&mut model);
            out.push_str("$ ");
            out.push_str(&step.command);
            out.push('\n');
            out.push_str(&model.render());
            out.push('\n');
        }
        out
    }
}

/// Let every window acknowledge its pending configure, so the observation is of settled
/// state.
///
/// A resize is a transaction: the new geometry only becomes current once the clients have
/// committed to it. Observing before that would compare tiri mid-flight against a sway tree
/// that is always settled, and report differences no user could ever see. One round per
/// window is enough in principle; the loop just refuses to assume that.
fn settle(layout: &mut Layout<TestWindow>, window_count: usize) {
    for _ in 0..window_count.max(1) {
        let before = observe(layout).render();
        for id in 1..=window_count {
            Op::Communicate(id).apply(layout);
        }
        // A committed size only becomes the tree's current geometry when the frame that
        // follows applies the pending layout, so the harness has to render like the
        // compositor does. Without this, IPC would be read between the commit and the
        // frame, which is a moment no user ever sees.
        layout.update_render_elements(None);
        if observe(layout).render() == before {
            return;
        }
    }
}

/// Normalize the active workspace as an outside observer would see it.
fn observe(layout: &Layout<TestWindow>) -> Workspace {
    let workspace = layout
        .active_workspace()
        .expect("a script always runs on an output with a workspace");
    let area = workspace.working_area();
    let area = LayoutTreeRect {
        x: area.loc.x,
        y: area.loc.y,
        width: area.size.w,
        height: area.size.h,
    };
    let workspace_layout = model_layout(workspace.debug_workspace_layout());
    // The tree has no node for "the workspace is what focus parent selected" when its only
    // child is a window, so the workspace answers that separately.
    let workspace_selected = workspace.tiling_targets_workspace();

    // TestWindow ids are the order the script opened them, which is exactly the identity the
    // model wants, so the map is the identity over the windows that exist.
    let tree = layout.layout_tree();
    let order = window_ids(&tree.root)
        .into_iter()
        .chain(tree.floating.iter().flat_map(window_ids_in))
        .map(|id| (id, id as u32))
        .collect();

    tiri_model::normalize(&tree, workspace_layout, workspace_selected, area, &order)
        .unwrap_or_else(|err| panic!("cannot normalize tiri's layout tree: {err:?}"))
}

fn window_ids(root: &Option<tiri_ipc::LayoutTreeNode>) -> Vec<u64> {
    root.as_ref().map(window_ids_in).unwrap_or_default()
}

fn window_ids_in(node: &tiri_ipc::LayoutTreeNode) -> Vec<u64> {
    let mut out = Vec::new();
    collect_window_ids(node, &mut out);
    out
}

fn collect_window_ids(node: &tiri_ipc::LayoutTreeNode, out: &mut Vec<u64>) {
    if let Some(id) = node.window_id {
        out.push(id);
    }
    for child in &node.children {
        collect_window_ids(child, out);
    }
}

fn model_layout(layout: TreeLayout) -> ModelLayout {
    match layout {
        TreeLayout::SplitH => ModelLayout::SplitH,
        TreeLayout::SplitV => ModelLayout::SplitV,
        TreeLayout::Tabbed => ModelLayout::Tabbed,
        TreeLayout::Stacked => ModelLayout::Stacked,
    }
}

/// Gaps, borders and focus rings off.
///
/// The model compares fractions of the working area, so a stray gap would move every
/// rectangle by an amount that looks like a divergence but is only configuration. The
/// recorder pins sway the same way.
fn pinned_options() -> Options {
    Options {
        layout: tiri_config::Layout {
            gaps: 0.,
            border: tiri_config::Border {
                off: true,
                ..Default::default()
            },
            focus_ring: tiri_config::FocusRing {
                off: true,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}
