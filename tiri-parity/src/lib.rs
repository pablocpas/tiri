//! Observable layout model for comparing tiri against i3/sway.
//!
//! The point of this crate is to answer one question honestly: do two compositors behave
//! the same, as far as anyone can tell by looking at the screen? It deliberately cannot
//! answer "do they represent the layout the same way internally", because that question has
//! no right answer and chasing it is what turned the previous parity attempt into
//! whack-a-mole.
//!
//! See `docs/design/parity.md` for the measurements the normalization rules come from.

pub mod fixture;
pub mod model;
pub mod sway;
pub mod tiri;

pub use fixture::Fixture;
pub use model::{Container, Difference, FracRect, Layout, Node, Window, WindowId, Workspace};

/// Erase decoration from a model, in place.
///
/// Under a tabbed or stacked container every child occupies the same content area, inset by
/// a band of tabs or titles whose height is a theme decision — sway reserves 27px at the
/// default font, tiri whatever its own tab bar config says. That band is not layout
/// behaviour, so comparing it would report a difference on every tabbed test for reasons
/// nobody can act on.
///
/// The erasure covers the whole subtree, not just the container's own children, because
/// sway's numbers below such a container are not self-consistent: it subtracts one band
/// from a leaf and two from the split holding it, so a window's rectangle can start above
/// its parent's. There is no reading of that which says anything about layout.
///
/// The cost is stated plainly: a genuine geometry bug *inside* a tabbed container would not
/// be caught here. What remains compared is the shape of the subtree, the order of the tabs,
/// which child is on top, and where the container itself sits — which is what a tabbed
/// layout is about.
pub fn erase_decoration(workspace: &mut Workspace) {
    // The workspace is itself a container, so it can be the tabbed one.
    if matches!(workspace.layout, Layout::Tabbed | Layout::Stacked) {
        let area = FracRect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
        for child in &mut workspace.nodes {
            flatten_onto(child, area);
        }
    }
    erase_nodes(&mut workspace.nodes);
}

fn erase_nodes(nodes: &mut [Node]) {
    for node in nodes {
        if let Node::Container(container) = node {
            if matches!(container.layout, Layout::Tabbed | Layout::Stacked) {
                let rect = container.rect;
                for child in &mut container.nodes {
                    flatten_onto(child, rect);
                }
                continue;
            }
            erase_nodes(&mut container.nodes);
        }
    }
}

/// Give `node` and everything below it the same rectangle.
fn flatten_onto(node: &mut Node, rect: FracRect) {
    set_rect(node, rect);
    if let Node::Container(container) = node {
        for child in &mut container.nodes {
            flatten_onto(child, rect);
        }
    }
}

fn set_rect(node: &mut Node, rect: FracRect) {
    match node {
        Node::Window(w) => w.rect = rect,
        Node::Container(c) => c.rect = rect,
    }
}

#[cfg(test)]
mod tests;
