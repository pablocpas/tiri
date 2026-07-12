use std::collections::{HashMap, HashSet};

use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{Blocker, BlockerState};

use super::{ContainerTree, Layout, LayoutElement, LeafLayoutInfo, NodeData, NodeKey};
use crate::layout::tab_bar::tab_bar_row_height;
use crate::layout::tile::Tile;
use crate::utils::transaction::{Transaction, TransactionBlocker};

#[derive(Debug)]
pub(super) struct LayoutPlan {
    pub(super) leaves: Vec<PlannedLeaf>,
    container_geometries: HashMap<NodeKey, Rectangle<f64, Logical>>,
}

#[derive(Debug)]
pub(super) struct PlannedLeaf {
    pub(super) layout: LeafLayoutInfo,
    tab_bar_offset: f64,
    draw_titlebar: bool,
    in_tabbed_context: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct LeafLayoutContext {
    tab_bar_offset: f64,
    draw_titlebar: bool,
    in_tabbed_context: bool,
}

#[derive(Debug)]
pub(super) struct PendingCommit {
    pub(super) plan: LayoutPlan,
    blocker: TransactionBlocker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutRequestMode {
    Normal,
    Maximized,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LayoutRequest {
    mode: LayoutRequestMode,
    size: Size<i32, Logical>,
}

impl<W: LayoutElement> ContainerTree<W> {
    /// Force a full layout pass and apply it through the transactional pipeline.
    pub fn layout(&mut self) {
        self.mark_dirty(super::Dirty::Topology);
        self.apply_with_resize_animation(true);
    }

    /// Force a full layout pass, with control over resize animation.
    pub fn layout_with_resize_animation(&mut self, animate_resize: bool) {
        self.mark_dirty(super::Dirty::Topology);
        self.apply_with_resize_animation(animate_resize);
    }

    /// Force and apply a layout pass with explicit animation flags.
    pub fn layout_with_animation_flags(&mut self, animate: bool, animate_resize: bool) {
        let _ = animate;
        self.mark_dirty(super::Dirty::Topology);
        self.apply_with_resize_animation(animate_resize);
    }

    pub(in crate::layout) fn apply(&mut self) {
        self.apply_with_resize_animation(true);
    }

    fn apply_with_resize_animation(&mut self, animate_resize: bool) {
        if self.pending_commit.is_some() && !self.apply_pending_commit_if_ready() {
            self.debug_layout_state("layout_atomic_pending");
            return;
        }

        let dirty = std::mem::take(&mut self.dirty);
        if dirty == super::Dirty::Clean {
            return;
        }
        if dirty == super::Dirty::Topology {
            self.prune_leaf_layouts();
        }
        self.layout_atomic(animate_resize);
    }

    pub fn layout_area(&self) -> Rectangle<f64, Logical> {
        let mut area = self.working_area;
        let gap = self.options.layout.gaps;
        if gap > 0.0 {
            area.loc.x += gap;
            area.loc.y += gap;
            area.size.w = (area.size.w - gap * 2.0).max(0.0);
            area.size.h = (area.size.h - gap * 2.0).max(0.0);
        }
        area
    }

    fn layout_atomic(&mut self, animate_resize: bool) {
        let Some(root_key) = self.root else {
            self.leaf_layouts.clear();
            self.pending_commit = None;
            self.next_transaction = None;
            self.debug_layout_state("layout_atomic_empty");
            return;
        };

        let plan = self.collect_layout_plan(root_key);
        let changed = self.changed_layout_keys(&plan);
        if changed.is_empty() {
            self.pending_commit = None;
            self.next_transaction = None;
            self.commit_layout_plan(plan);
            self.debug_layout_state("layout_atomic_apply");
            return;
        }

        let transaction = self
            .next_transaction
            .take()
            .unwrap_or_else(Transaction::new);
        self.request_sizes_for_layout(&plan, &changed, &transaction, animate_resize);
        let should_apply_now = transaction.is_last();
        self.pending_commit = Some(PendingCommit {
            plan,
            blocker: transaction.blocker(),
        });
        drop(transaction);
        if should_apply_now && self.apply_pending_commit_if_ready() {
            return;
        }
        self.debug_layout_state("layout_atomic_requested");
    }

    fn apply_pending_commit_if_ready(&mut self) -> bool {
        let Some(pending) = &self.pending_commit else {
            return false;
        };
        if pending.blocker.state() != BlockerState::Released {
            return false;
        }
        let pending = self.pending_commit.take().unwrap();
        self.commit_layout_plan(pending.plan);
        self.debug_layout_state("layout_atomic_apply_pending");
        true
    }

    pub(in crate::layout) fn has_pending_commit(&self) -> bool {
        self.pending_commit.is_some()
    }

    fn layout_request_for(
        &self,
        tile: &Tile<W>,
        tile_size: Size<f64, Logical>,
        tab_offset: f64,
    ) -> LayoutRequest {
        if tile.window().pending_sizing_mode().is_fullscreen() {
            LayoutRequest {
                mode: LayoutRequestMode::Fullscreen,
                size: self.view_size.to_i32_round(),
            }
        } else if tile.pending_maximized {
            LayoutRequest {
                mode: LayoutRequestMode::Maximized,
                size: tile_size.to_i32_round(),
            }
        } else {
            LayoutRequest {
                mode: LayoutRequestMode::Normal,
                size: tile.requested_window_size_for_tile(tile_size, tab_offset),
            }
        }
    }

    fn collect_layout_plan(&self, root_key: NodeKey) -> LayoutPlan {
        let mut plan = LayoutPlan {
            leaves: Vec::new(),
            container_geometries: HashMap::new(),
        };

        let mut path = Vec::new();
        let area = self.layout_area();
        self.collect_layout_node(
            root_key,
            area,
            &mut path,
            true,
            LeafLayoutContext::default(),
            &mut plan,
        );
        plan
    }

    fn collect_layout_node(
        &self,
        node_key: NodeKey,
        rect: Rectangle<f64, Logical>,
        path: &mut Vec<usize>,
        visible: bool,
        ctx: LeafLayoutContext,
        plan: &mut LayoutPlan,
    ) {
        let (layout, child_count, focused_idx, child_percents_sum) = match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => {
                let (offset, show_titlebar) = if tile.window().pending_sizing_mode().is_fullscreen()
                {
                    (0.0, false)
                } else {
                    (ctx.tab_bar_offset, ctx.draw_titlebar)
                };
                plan.leaves.push(PlannedLeaf {
                    layout: LeafLayoutInfo {
                        key: node_key,
                        path: path.clone(),
                        rect,
                        visible,
                    },
                    tab_bar_offset: offset,
                    draw_titlebar: show_titlebar,
                    in_tabbed_context: ctx.in_tabbed_context,
                });
                return;
            }
            Some(NodeData::Container(container)) => {
                plan.container_geometries.insert(node_key, rect);
                let percents = container.child_percents_slice();
                let sum: f64 = percents.iter().copied().sum();
                (
                    container.layout(),
                    container.child_count(),
                    container.focused_child_index(),
                    sum,
                )
            }
            None => return,
        };

        if child_count == 0 {
            return;
        }

        let gap = self.options.layout.gaps;

        match layout {
            Layout::SplitH => {
                let split_bar_height = self.split_title_bar_height();
                let total_gap = if child_count > 1 {
                    gap * (child_count as f64 - 1.0)
                } else {
                    0.0
                };
                let available_width = (rect.size.w - total_gap).max(0.0);
                let percents =
                    self.get_normalized_child_percents(node_key, child_count, child_percents_sum);
                let widths = self.distribute_split_lengths(available_width, child_count, &percents);
                let mut cursor_x = rect.loc.x;

                for idx in 0..child_count {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };
                    let width = *widths.get(idx).unwrap_or(&0.0);
                    let child_rect = Rectangle::new(
                        Point::from((cursor_x, rect.loc.y)),
                        Size::from((width, rect.size.h)),
                    );

                    path.push(idx);
                    let (child_offset, child_titlebar) =
                        self.split_child_titlebar(child_key, split_bar_height);
                    let child_ctx = LeafLayoutContext {
                        tab_bar_offset: child_offset,
                        draw_titlebar: child_titlebar,
                        in_tabbed_context: ctx.in_tabbed_context,
                    };
                    self.collect_layout_node(child_key, child_rect, path, visible, child_ctx, plan);
                    path.pop();

                    if idx + 1 < child_count {
                        cursor_x += width + gap;
                    }
                }
            }
            Layout::SplitV => {
                let split_bar_height = self.split_title_bar_height();
                let total_gap = if child_count > 1 {
                    gap * (child_count as f64 - 1.0)
                } else {
                    0.0
                };
                let available_height = (rect.size.h - total_gap).max(0.0);
                let percents =
                    self.get_normalized_child_percents(node_key, child_count, child_percents_sum);
                let heights =
                    self.distribute_split_lengths(available_height, child_count, &percents);
                let mut cursor_y = rect.loc.y;

                for idx in 0..child_count {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };
                    let height = *heights.get(idx).unwrap_or(&0.0);
                    let child_rect = Rectangle::new(
                        Point::from((rect.loc.x, cursor_y)),
                        Size::from((rect.size.w, height)),
                    );

                    path.push(idx);
                    let (child_offset, child_titlebar) =
                        self.split_child_titlebar(child_key, split_bar_height);
                    let child_ctx = LeafLayoutContext {
                        tab_bar_offset: child_offset,
                        draw_titlebar: child_titlebar,
                        in_tabbed_context: ctx.in_tabbed_context,
                    };
                    self.collect_layout_node(child_key, child_rect, path, visible, child_ctx, plan);
                    path.pop();

                    if idx + 1 < child_count {
                        cursor_y += height + gap;
                    }
                }
            }
            Layout::Tabbed | Layout::Stacked => {
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

                let focused_idx = focused_idx.unwrap_or(0).min(child_count.saturating_sub(1));

                for idx in 0..child_count {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };
                    path.push(idx);
                    let child_visible = visible && idx == focused_idx;
                    let mut content_rect = inner_rect;
                    if tab_offset > 0.0 {
                        content_rect.loc.y += tab_offset;
                        content_rect.size.h = (content_rect.size.h - tab_offset).max(0.0);
                    }
                    let child_ctx = LeafLayoutContext {
                        tab_bar_offset: 0.0,
                        draw_titlebar: false,
                        in_tabbed_context: true,
                    };
                    self.collect_layout_node(
                        child_key,
                        content_rect,
                        path,
                        child_visible,
                        child_ctx,
                        plan,
                    );
                    path.pop();
                }
            }
        }
    }

    fn changed_layout_keys(&self, plan: &LayoutPlan) -> HashSet<NodeKey> {
        let mut current = HashMap::new();
        for info in &self.leaf_layouts {
            let Some(tile) = self.get_tile(info.key) else {
                continue;
            };
            let request = self.layout_request_for(tile, info.rect.size, tile.tab_bar_offset());
            current.insert(info.key, request);
        }

        let mut changed = HashSet::new();
        for planned in &plan.leaves {
            let info = &planned.layout;
            let Some(tile) = self.get_tile(info.key) else {
                changed.insert(info.key);
                continue;
            };
            // Deliberately compare committed Tile state against the proposed plan.
            let request = self.layout_request_for(tile, info.rect.size, planned.tab_bar_offset);
            if current.get(&info.key).map_or(true, |old| *old != request) {
                changed.insert(info.key);
            }
        }

        changed
    }

    fn request_sizes_for_layout(
        &mut self,
        plan: &LayoutPlan,
        changed: &HashSet<NodeKey>,
        transaction: &Transaction,
        animate_resize: bool,
    ) {
        for planned in &plan.leaves {
            let info = &planned.layout;
            let Some(tile) = self.get_tile_mut(info.key) else {
                continue;
            };
            let old_offset = tile.tab_bar_offset();
            let old_titlebar = tile.draw_titlebar();
            let old_tabbed_context = tile.in_tabbed_context();
            tile.set_tab_bar_offset(planned.tab_bar_offset);
            tile.set_draw_titlebar(planned.draw_titlebar);
            tile.set_in_tabbed_context(planned.in_tabbed_context);

            let tx = changed.contains(&info.key).then(|| transaction.clone());
            let size = Size::from((info.rect.size.w, info.rect.size.h));
            if tile.window().pending_sizing_mode().is_fullscreen() {
                tile.request_fullscreen(animate_resize, tx);
            } else if tile.pending_maximized {
                tile.request_maximized(size, animate_resize, tx);
            } else {
                tile.request_tile_size(size, animate_resize, tx);
            }

            tile.set_tab_bar_offset(old_offset);
            tile.set_draw_titlebar(old_titlebar);
            tile.set_in_tabbed_context(old_tabbed_context);
        }
    }

    fn commit_layout_plan(&mut self, plan: LayoutPlan) {
        for (key, rect) in plan.container_geometries {
            if let Some(NodeData::Container(container)) = self.get_node_mut(key) {
                container.set_geometry(rect);
            }
        }
        let mut leaf_layouts = Vec::with_capacity(plan.leaves.len());
        for planned in plan.leaves {
            if let Some(tile) = self.get_tile_mut(planned.layout.key) {
                tile.set_tab_bar_offset(planned.tab_bar_offset);
                tile.set_draw_titlebar(planned.draw_titlebar);
                tile.set_in_tabbed_context(planned.in_tabbed_context);
                leaf_layouts.push(planned.layout);
            }
        }
        self.leaf_layouts = leaf_layouts;
    }

    pub(in crate::layout) fn tab_bar_row_height(&self) -> f64 {
        if self.options.layout.tab_bar.off {
            return 0.0;
        }
        tab_bar_row_height(&self.options.layout.tab_bar, self.scale)
    }

    pub(in crate::layout) fn split_title_bar_height(&self) -> f64 {
        if !self.options.layout.tab_bar.show_in_split {
            return 0.0;
        }
        self.tab_bar_row_height()
    }

    fn get_container_child_at(&self, container_key: NodeKey, idx: usize) -> Option<NodeKey> {
        match self.get_node(container_key) {
            Some(NodeData::Container(container)) => container.child_key(idx),
            _ => None,
        }
    }

    pub(in crate::layout) fn get_normalized_child_percents(
        &self,
        container_key: NodeKey,
        child_count: usize,
        percents_sum: f64,
    ) -> Vec<f64> {
        let Some(NodeData::Container(container)) = self.get_node(container_key) else {
            return vec![1.0 / child_count.max(1) as f64; child_count];
        };

        let percents = container.child_percents_slice();
        if percents_sum > f64::EPSILON {
            percents.iter().map(|p| p / percents_sum).collect()
        } else {
            vec![1.0 / child_count.max(1) as f64; child_count]
        }
    }

    pub(in crate::layout) fn distribute_split_lengths(
        &self,
        available: f64,
        child_count: usize,
        percents: &[f64],
    ) -> Vec<f64> {
        if child_count == 0 {
            return Vec::new();
        }

        let available = available.max(0.0);
        let available_phys = (available * self.scale).round().max(0.0) as i32;
        let default = 1.0 / child_count as f64;
        let mut weights: Vec<f64> = (0..child_count)
            .map(|idx| percents.get(idx).copied().unwrap_or(default).max(0.0))
            .collect();
        let sum: f64 = weights.iter().sum();
        if sum > f64::EPSILON {
            for w in &mut weights {
                *w /= sum;
            }
        } else {
            weights.fill(default);
        }

        let mut lengths_int = vec![0i32; child_count];
        let mut fractions = Vec::with_capacity(child_count);
        let mut used = 0i32;
        for (idx, weight) in weights.iter().copied().enumerate() {
            let raw = available_phys as f64 * weight;
            let base = raw.floor() as i32;
            lengths_int[idx] = base;
            used += base;
            fractions.push((idx, raw - base as f64));
        }

        let mut remainder = (available_phys - used).max(0);
        fractions.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut i = 0usize;
        while remainder > 0 && !fractions.is_empty() {
            let idx = fractions[i % fractions.len()].0;
            lengths_int[idx] += 1;
            remainder -= 1;
            i += 1;
        }

        lengths_int
            .into_iter()
            .map(|v| v as f64 / self.scale)
            .collect()
    }

    fn split_child_titlebar(&self, child_key: NodeKey, split_bar_height: f64) -> (f64, bool) {
        if split_bar_height <= 0.0 {
            return (0.0, false);
        }

        // CSD-aware (like the border's `draw_border_with_background = !has_ssd()`):
        // only draw the WM per-tile titlebar for windows we decorate (SSD). A CSD
        // window draws its own headerbar, so a WM titlebar would double up. The
        // tabbed/stacked tab bar is handled separately and stays always-on.
        match self.get_node(child_key) {
            Some(NodeData::Leaf(tile)) if tile.window().has_ssd() => (split_bar_height, true),
            _ => (0.0, false),
        }
    }

    pub(in crate::layout) fn tab_bar_spacing(&self) -> f64 {
        0.0
    }

    pub(in crate::layout) fn tab_bar_rect(
        &self,
        layout: Layout,
        rect: Rectangle<f64, Logical>,
        tab_count: usize,
    ) -> Option<(Rectangle<f64, Logical>, f64)> {
        if tab_count == 0 {
            return None;
        }

        let row_height = self.tab_bar_row_height();
        if row_height <= 0.0 {
            return None;
        }

        let ring_ext = if !self.options.layout.focus_ring.off {
            self.options.layout.focus_ring.width
        } else {
            0.0
        };
        let mut bar_rect = rect;
        if ring_ext > 0.0 {
            bar_rect.loc.x -= ring_ext;
            bar_rect.loc.y -= ring_ext;
            bar_rect.size.w += ring_ext * 2.0;
            bar_rect.size.h += ring_ext;
        }

        let spacing = self.tab_bar_spacing();
        let base_height = match layout {
            Layout::Tabbed => row_height,
            Layout::Stacked => row_height * tab_count as f64,
            _ => 0.0,
        };
        let bar_height = (base_height + ring_ext + spacing)
            .min(bar_rect.size.h)
            .max(0.0);
        if bar_height <= 0.0 {
            return None;
        }

        let bar_rect = Rectangle::new(bar_rect.loc, Size::from((bar_rect.size.w, bar_height)));
        Some((bar_rect, row_height))
    }
}
