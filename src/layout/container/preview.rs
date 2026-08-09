//! Preview geometry for insert hints (drag-and-drop and open-placement previews).

use smithay::utils::Logical;
use smithay::utils::Rectangle;

use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::PreviewLeafGeometry;

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn preview_new_leaf_geometry(&self) -> Option<PreviewLeafGeometry> {
        let root_rect = self.layout_area();
        let root_key = self.root;

        // Nothing in the workspace yet: the window arriving gets it all, laid out by the
        // workspace's own orientation.
        if self.is_empty() {
            let layout = self.root_container_layout();
            let (rect, tab_bar_offset) = self.preview_child_rect(
                layout,
                root_rect,
                1,
                &[1.0],
                0,
                true,
                self.gap_in(root_key),
            );
            return Some(PreviewLeafGeometry {
                rect,
                tab_bar_offset,
            });
        }

        let focus_path = self.selected_path();
        let (parent_path, insert_idx) = if focus_path.is_empty() {
            (Vec::new(), None)
        } else {
            let mut parent_path = focus_path.clone();
            let insert_idx = parent_path.pop().map(|idx| idx + 1);
            (parent_path, insert_idx)
        };

        let parent_key = if parent_path.is_empty() {
            root_key
        } else {
            self.get_node_key_at_path(&parent_path)?
        };
        let parent_rect = self.preview_rect_for_path(root_key, root_rect, &parent_path)?;
        let parent = self.get_container(parent_key)?;
        let child_count = parent.child_count();
        let insert_idx = insert_idx.unwrap_or(child_count).min(child_count);
        let current = self.child_percents(parent_key);
        let percents = self.preview_inserted_child_percents(&current, child_count, insert_idx);
        let (rect, tab_bar_offset) = self.preview_child_rect(
            parent.layout(),
            parent_rect,
            child_count + 1,
            &percents,
            insert_idx,
            true,
            self.gap_in(parent_key),
        );

        Some(PreviewLeafGeometry {
            rect,
            tab_bar_offset,
        })
    }

    pub(super) fn preview_rect_for_path(
        &self,
        root_key: NodeKey,
        root_rect: Rectangle<f64, Logical>,
        path: &[usize],
    ) -> Option<Rectangle<f64, Logical>> {
        let mut rect = root_rect;
        let mut node_key = root_key;
        for &idx in path {
            let container = self.get_container(node_key)?;
            let child_key = container.child_key(idx)?;
            let child_is_leaf = matches!(self.get_node(child_key), Some(NodeData::Leaf(_)));
            let percents = self.get_normalized_child_percents(node_key, container.child_count());
            let (child_rect, _) = self.preview_child_rect(
                container.layout(),
                rect,
                container.child_count(),
                &percents,
                idx,
                child_is_leaf,
                self.gap_in(node_key),
            );
            if child_is_leaf {
                return None;
            }
            rect = child_rect;
            node_key = child_key;
        }
        Some(rect)
    }

    pub(super) fn preview_inserted_child_percents(
        &self,
        current: &[f64],
        old_len: usize,
        insert_idx: usize,
    ) -> Vec<f64> {
        if old_len == 0 {
            return vec![1.0];
        }

        let mut percents = if current.len() == old_len {
            current.to_vec()
        } else {
            vec![1.0 / old_len as f64; old_len]
        };

        Self::normalize_child_percents_for_preview(&mut percents);

        let new_share = 1.0 / (old_len as f64 + 1.0);
        for percent in &mut percents {
            *percent *= 1.0 - new_share;
        }

        let insert_idx = insert_idx.min(percents.len());
        percents.insert(insert_idx, new_share);
        Self::normalize_child_percents_for_preview(&mut percents);
        percents
    }

    pub(super) fn normalize_child_percents_for_preview(percents: &mut [f64]) {
        if percents.is_empty() {
            return;
        }
        let mut sum = 0.0;
        for percent in percents.iter() {
            if !percent.is_finite() || *percent < 0.0 {
                sum = 0.0;
                break;
            }
            sum += *percent;
        }
        if sum <= f64::EPSILON {
            let value = 1.0 / percents.len() as f64;
            for percent in percents.iter_mut() {
                *percent = value;
            }
            return;
        }
        for percent in percents.iter_mut() {
            *percent /= sum;
        }
    }

    pub(super) fn preview_child_rect(
        &self,
        layout: Layout,
        rect: Rectangle<f64, Logical>,
        child_count: usize,
        percents: &[f64],
        child_idx: usize,
        child_is_leaf: bool,
        gap: f64,
    ) -> (Rectangle<f64, Logical>, f64) {
        let (child_rects, _) =
            self.child_rects_for_layout(layout, rect, child_count, percents, gap);
        match layout {
            Layout::SplitH | Layout::SplitV => {
                let Some(child_rect) = child_rects.get(child_idx).copied() else {
                    return (rect, 0.0);
                };
                let split_bar_height = self.split_title_bar_height();
                let tab_bar_offset = if child_is_leaf && split_bar_height > 0.0 {
                    split_bar_height
                } else {
                    0.0
                };
                (child_rect, tab_bar_offset)
            }
            Layout::Tabbed | Layout::Stacked => {
                let content_rect = child_rects.first().copied().unwrap_or(rect);
                (content_rect, 0.0)
            }
        }
    }
}
