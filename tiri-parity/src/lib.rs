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
pub mod session;
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
            reseat(child, area);
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
                    reseat(child, rect);
                }
            }
            erase_nodes(&mut container.nodes);
        }
    }
}

/// Move `node` onto `rect`, and lay its subtree out inside it in the same proportions.
///
/// The children are mapped from *the box they actually occupy* onto the new rectangle,
/// rather than from their parent's declared one. That distinction is the whole trick: sway
/// subtracts one decoration band from a leaf and two from the split holding it, so a
/// window's rectangle can start above its parent's and no scaling relative to the parent
/// would cancel it. The extent the children share is a number both compositors agree on,
/// and dividing by it leaves exactly the proportions — which is the part that is layout.
fn reseat(node: &mut Node, rect: FracRect) {
    set_rect(node, rect);
    let Node::Container(container) = node else {
        return;
    };
    let Some(extent) = extent_of(&container.nodes) else {
        return;
    };
    if extent.w <= 0.0 || extent.h <= 0.0 {
        for child in &mut container.nodes {
            reseat(child, rect);
        }
        return;
    }

    let scale_x = rect.w / extent.w;
    let scale_y = rect.h / extent.h;
    for child in &mut container.nodes {
        let r = rect_of(child);
        reseat(
            child,
            FracRect {
                x: rect.x + (r.x - extent.x) * scale_x,
                y: rect.y + (r.y - extent.y) * scale_y,
                w: r.w * scale_x,
                h: r.h * scale_y,
            },
        );
    }
}

/// The smallest box containing all of `nodes`.
fn extent_of(nodes: &[Node]) -> Option<FracRect> {
    let mut nodes = nodes.iter().map(rect_of);
    let first = nodes.next()?;
    let (mut x0, mut y0) = (first.x, first.y);
    let (mut x1, mut y1) = (first.x + first.w, first.y + first.h);
    for r in nodes {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.w);
        y1 = y1.max(r.y + r.h);
    }
    Some(FracRect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
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
