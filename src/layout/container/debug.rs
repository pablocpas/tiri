//! Diagnostic dumps of the tree and layout state.

#[cfg(test)]
use super::layout_label;
use super::ContainerTree;
use super::LayoutElement;
#[cfg(test)]
use super::NodeData;
#[cfg(test)]
use super::NodeKey;

impl<W: LayoutElement> ContainerTree<W> {
    pub(super) fn debug_layout_state(&self, context: &'static str) {
        let window_count = self.window_count();
        let leaf_count = self.leaf_layouts.len();
        let pending_leaf_count = self
            .pending_layouts
            .as_ref()
            .map(|pending| pending.data.leaf_layouts.len())
            .unwrap_or(0);
        let has_pending = self.pending_layouts.is_some();

        if window_count <= 3 {
            debug!(
                context = context,
                window_count,
                leaf_count,
                pending_leaf_count,
                has_pending,
                working_area = ?self.working_area,
                view_size = ?self.view_size,
                scale = self.scale,
                root = ?self.root,
                focused = ?self.focused_key,
                "layout summary"
            );
            for info in &self.leaf_layouts {
                debug!(
                    context = context,
                    key = ?info.key,
                    rect = ?info.rect,
                    visible = info.visible,
                    path = ?info.path,
                    "leaf layout"
                );
            }
            if let Some(pending) = &self.pending_layouts {
                for info in &pending.data.leaf_layouts {
                    debug!(
                        context = context,
                        key = ?info.key,
                        rect = ?info.rect,
                        visible = info.visible,
                        path = ?info.path,
                        "pending leaf layout"
                    );
                }
            }
        }

        if leaf_count == 0 && pending_leaf_count > 0 {
            debug!(
                context = context,
                window_count, pending_leaf_count, "layout has no leaf layouts but pending exists"
            );
        }
        if window_count != leaf_count {
            debug!(
                context = context,
                window_count,
                leaf_count,
                pending_leaf_count,
                has_pending,
                "layout window/leaf mismatch"
            );
        }

        let zero_size = self
            .leaf_layouts
            .iter()
            .filter(|info| info.rect.size.w <= 0.0 || info.rect.size.h <= 0.0)
            .count();
        if zero_size > 0 {
            debug!(context = context, zero_size, "layout has zero-size leafs");
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_tree(&self) -> String
    where
        W::Id: std::fmt::Display,
    {
        let mut out = String::new();
        let Some(root_key) = self.root else {
            out.push_str("(empty)\n");
            return out;
        };

        let mut path = Vec::new();
        let focused_path = self.focus_path();
        self.debug_tree_node(root_key, &mut path, &mut out, &focused_path);
        out
    }

    #[cfg(test)]
    pub(super) fn debug_tree_node(
        &self,
        node_key: NodeKey,
        path: &mut Vec<usize>,
        out: &mut String,
        focused_path: &[usize],
    ) where
        W::Id: std::fmt::Display,
    {
        use std::fmt::Write as _;

        let indent = "  ".repeat(path.len());
        match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => {
                let focused = if *path == focused_path { " *" } else { "" };
                let _ = writeln!(out, "{indent}Window {}{focused}", tile.window().id());
            }
            Some(NodeData::Container(container)) => {
                let label = layout_label(container.layout());
                let _ = writeln!(out, "{indent}{label}");
                for (idx, child_key) in container.children.iter().enumerate() {
                    path.push(idx);
                    self.debug_tree_node(*child_key, path, out, focused_path);
                    path.pop();
                }
            }
            None => {
                let _ = writeln!(out, "{indent}(missing)");
            }
        }
    }
}
