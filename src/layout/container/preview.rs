//! Preview geometry for insert hints (drag-and-drop and open-placement previews).

use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::utils::Size;

use super::ContainerTree;
use super::Layout;
use super::LayoutElement;
use super::NodeData;
use super::NodeKey;
use super::PreviewLeafGeometry;

impl<W: LayoutElement> ContainerTree<W> {
    pub(in crate::layout) fn preview_new_leaf_geometry(&self) -> Option<PreviewLeafGeometry> {
        let root_rect = self.layout_area();
        let Some(root_key) = self.root else {
            if let Some(layout) = self.pending_layout {
                let (rect, tab_bar_offset) =
                    self.preview_child_rect(layout, root_rect, 1, &[1.0], 0, true);
                return Some(PreviewLeafGeometry {
                    rect,
                    tab_bar_offset,
                });
            }
            return Some(PreviewLeafGeometry {
                rect: root_rect,
                tab_bar_offset: 0.0,
            });
        };

        if matches!(self.get_node(root_key), Some(NodeData::Leaf(_))) {
            let percents = self.preview_inserted_child_percents(&[], 1, 1);
            let (rect, tab_bar_offset) =
                self.preview_child_rect(Layout::SplitH, root_rect, 2, &percents, 1, true);
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
        let percents = self.preview_inserted_child_percents(
            parent.child_percents_slice(),
            child_count,
            insert_idx,
        );
        let (rect, tab_bar_offset) = self.preview_child_rect(
            parent.layout(),
            parent_rect,
            child_count + 1,
            &percents,
            insert_idx,
            true,
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
            let percents_sum: f64 = container.child_percents_slice().iter().copied().sum();
            let percents =
                self.get_normalized_child_percents(node_key, container.child_count(), percents_sum);
            let (child_rect, _) = self.preview_child_rect(
                container.layout(),
                rect,
                container.child_count(),
                &percents,
                idx,
                child_is_leaf,
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
    ) -> (Rectangle<f64, Logical>, f64) {
        let gap = self.options.layout.gaps;
        match layout {
            Layout::SplitH => {
                let total_gap = if child_count > 1 {
                    gap * (child_count as f64 - 1.0)
                } else {
                    0.0
                };
                let available_width = (rect.size.w - total_gap).max(0.0);
                let widths = self.distribute_split_lengths(available_width, child_count, percents);
                let mut cursor_x = rect.loc.x;
                let split_bar_height = self.split_title_bar_height();
                for idx in 0..child_count {
                    let width = *widths.get(idx).unwrap_or(&0.0);
                    if idx == child_idx {
                        let child_rect = Rectangle::new(
                            Point::from((cursor_x, rect.loc.y)),
                            Size::from((width, rect.size.h)),
                        );
                        let tab_bar_offset = if child_is_leaf && split_bar_height > 0.0 {
                            split_bar_height
                        } else {
                            0.0
                        };
                        return (child_rect, tab_bar_offset);
                    }
                    if idx + 1 < child_count {
                        cursor_x += width + gap;
                    }
                }
            }
            Layout::SplitV => {
                let total_gap = if child_count > 1 {
                    gap * (child_count as f64 - 1.0)
                } else {
                    0.0
                };
                let available_height = (rect.size.h - total_gap).max(0.0);
                let heights =
                    self.distribute_split_lengths(available_height, child_count, percents);
                let mut cursor_y = rect.loc.y;
                let split_bar_height = self.split_title_bar_height();
                for idx in 0..child_count {
                    let height = *heights.get(idx).unwrap_or(&0.0);
                    if idx == child_idx {
                        let child_rect = Rectangle::new(
                            Point::from((rect.loc.x, cursor_y)),
                            Size::from((rect.size.w, height)),
                        );
                        let tab_bar_offset = if child_is_leaf && split_bar_height > 0.0 {
                            split_bar_height
                        } else {
                            0.0
                        };
                        return (child_rect, tab_bar_offset);
                    }
                    if idx + 1 < child_count {
                        cursor_y += height + gap;
                    }
                }
            }
            Layout::Tabbed | Layout::Stacked => {
                // No gap padding for tabbed/stacked.
                let inner_rect = rect;

                let bar_row_height = self.tab_bar_row_height();
                let mut tab_offset = 0.0;
                if bar_row_height > 0.0 && child_count > 0 {
                    let bar_height = match layout {
                        Layout::Tabbed => bar_row_height,
                        Layout::Stacked => bar_row_height * child_count as f64,
                        _ => 0.0,
                    };
                    let total_bar_height = (bar_height + self.tab_bar_spacing())
                        .min(inner_rect.size.h)
                        .max(0.0);
                    tab_offset = total_bar_height;
                }

                let mut content_rect = inner_rect;
                if tab_offset > 0.0 {
                    content_rect.loc.y += tab_offset;
                    content_rect.size.h = (content_rect.size.h - tab_offset).max(0.0);
                }
                return (content_rect, 0.0);
            }
        }

        (rect, 0.0)
    }
}
