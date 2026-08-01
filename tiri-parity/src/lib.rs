//! Observable layout model for comparing tiri against i3/sway.
//!
//! The point of this crate is to answer one question honestly: do two compositors behave
//! the same, as far as anyone can tell by looking at the screen? It deliberately cannot
//! answer "do they represent the layout the same way internally", because that question has
//! no right answer and chasing it is what turned the previous parity attempt into
//! whack-a-mole.
//!
//! See `docs/design/parity.md` for the measurements the normalization rules come from.

pub mod model;
pub mod sway;
pub mod tiri;

pub use model::{Container, Difference, FracRect, Layout, Node, Window, WindowId, Workspace};

/// Erase decoration from a model, in place.
///
/// Under a tabbed or stacked container every child occupies the same content area, inset by
/// a band of tabs or titles whose height is a theme decision — sway reserves 27px at the
/// default font, tiri whatever its own tab bar config says. That band is not layout
/// behaviour, so comparing it would report a difference on every tabbed test for reasons
/// nobody can act on.
///
/// What remains observable under such a container is which child is on top and in what
/// order the tabs sit, and those are compared normally.
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
            grow_to(child, area);
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
                    grow_to(child, rect);
                }
            }
            erase_nodes(&mut container.nodes);
        }
    }
}

/// Move and stretch `node`'s whole subtree so that `node` fills `target`.
///
/// The band a tab bar occupies shifts everything under it, not just the child itself, so
/// erasing it has to carry the descendants along. Anything nested keeps its position
/// *relative* to its parent, which is the part that is behaviour.
fn grow_to(node: &mut Node, target: FracRect) {
    let current = rect_of(node);
    if current.w <= 0.0 || current.h <= 0.0 {
        set_rect(node, target);
        return;
    }
    let scale_x = target.w / current.w;
    let scale_y = target.h / current.h;
    remap(node, |r| FracRect {
        x: target.x + (r.x - current.x) * scale_x,
        y: target.y + (r.y - current.y) * scale_y,
        w: r.w * scale_x,
        h: r.h * scale_y,
    });
}

fn remap(node: &mut Node, f: impl Fn(FracRect) -> FracRect + Copy) {
    set_rect(node, f(rect_of(node)));
    if let Node::Container(container) = node {
        for child in &mut container.nodes {
            remap(child, f);
        }
    }
}

fn rect_of(node: &Node) -> FracRect {
    match node {
        Node::Window(w) => w.rect,
        Node::Container(c) => c.rect,
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
