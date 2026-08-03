use std::collections::{HashMap, HashSet};

use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{Blocker, BlockerState};

use super::{ContainerTree, Layout, LayoutElement, LeafLayoutInfo, NodeData, NodeKey};
use crate::layout::tab_bar::tab_bar_row_height;
use crate::layout::tile::Tile;
use crate::utils::transaction::{Transaction, TransactionBlocker};

#[derive(Debug)]
pub(in crate::layout) struct LayoutData {
    pub(in crate::layout) leaf_layouts: Vec<LeafLayoutInfo>,
    container_geometries: HashMap<NodeKey, Rectangle<f64, Logical>>,
    tab_bar_offsets: HashMap<NodeKey, f64>,
    titlebar_flags: HashMap<NodeKey, bool>,
    tabbed_context_flags: HashMap<NodeKey, bool>,
}

#[derive(Debug, Clone, Copy, Default)]
struct LeafLayoutContext {
    tab_bar_offset: f64,
    draw_titlebar: bool,
    in_tabbed_context: bool,
}

#[derive(Debug)]
pub(super) struct PendingLayout {
    pub(in crate::layout) data: LayoutData,
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
    /// Calculate and apply layout to the tree.
    pub(in crate::layout) fn layout(&mut self) {
        self.layout_with_resize_animation(true);
    }

    /// Calculate and apply layout to the tree, with control over resize animation.
    pub(in crate::layout) fn layout_with_resize_animation(&mut self, animate_resize: bool) {
        let animate = !self.options.animations.off;
        self.layout_with_animations(animate, animate_resize);
    }

    /// Calculate and apply layout to the tree with explicit animation flags.
    pub(in crate::layout) fn layout_with_animation_flags(
        &mut self,
        animate: bool,
        animate_resize: bool,
    ) {
        self.layout_with_animations(animate, animate_resize);
    }

    fn layout_with_animations(&mut self, animate: bool, animate_resize: bool) {
        self.generation = self.generation.wrapping_add(1);
        let _ = animate;
        self.layout_atomic(animate_resize);
    }

    pub(in crate::layout) fn layout_area(&self) -> Rectangle<f64, Logical> {
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

    /// Whether the pending snapshot still covers exactly the tree's current leaves.
    ///
    /// While a size transaction is in flight `layout_atomic` defers relayouts, so windows
    /// that came or went since the snapshot are not reflected in it. Passes that drive
    /// window state must not run over such a snapshot: flushing a configure from it sends
    /// the client bounds computed for a tree that no longer exists.
    pub(in crate::layout) fn pending_layout_is_stale(&self) -> bool {
        let Some(pending) = &self.pending_layouts else {
            return false;
        };

        let snapshot: HashSet<NodeKey> = pending
            .data
            .leaf_layouts
            .iter()
            .map(|info| info.key)
            .collect();
        // Only leaves reachable from the root count: the slotmap can still hold nodes that
        // were detached but not yet dropped.
        let current: HashSet<NodeKey> = self.dfs_leaf_keys().into_iter().collect();

        snapshot != current
    }

    fn layout_atomic(&mut self, animate_resize: bool) {
        if self.pending_layouts.is_some() && !self.apply_pending_layouts_if_ready() {
            self.pending_relayout = true;
            self.readdress_leaf_layouts();
            self.debug_layout_state("layout_atomic_pending");
            return;
        }
        self.pending_relayout = false;

        let root_key = self.root;
        if self.is_empty() {
            self.leaf_layouts.clear();
            self.pending_layouts = None;
            self.pending_transaction = None;
            self.pending_relayout = false;
            self.debug_layout_state("layout_atomic_empty");
            return;
        }

        let data = self.collect_layout_data(root_key);
        let changed = self.changed_layout_keys(&data);
        if changed.is_empty() {
            self.pending_layouts = None;
            self.pending_transaction = None;
            self.apply_layout_data(data);
            self.debug_layout_state("layout_atomic_apply");
            return;
        }

        let transaction = self
            .pending_transaction
            .take()
            .unwrap_or_else(Transaction::new);
        self.request_sizes_for_layout(&data, &changed, &transaction, animate_resize);
        let should_apply_now = transaction.is_last();
        self.pending_layouts = Some(PendingLayout {
            data,
            blocker: transaction.blocker(),
        });
        drop(transaction);
        if should_apply_now && self.apply_pending_layouts_if_ready() {
            return;
        }
        self.readdress_leaf_layouts();
        self.debug_layout_state("layout_atomic_requested");
    }

    /// Point the cached layout at where its leaves are *now*.
    ///
    /// While a resize is in flight the cached geometry is deliberately the old one — it is
    /// what is still on screen — but the path beside it is an address, not geometry, and an
    /// address of a tree that has moved on is simply wrong. A structural change during a
    /// transaction is what pulls the two apart: the leaves are the same, their rectangles
    /// are the same, and they are somewhere else.
    pub(in crate::layout) fn readdress_leaf_layouts(&mut self) {
        let addresses: Vec<Option<Vec<usize>>> = self
            .leaf_layouts
            .iter()
            .map(|info| self.find_node_path(info.key))
            .collect();
        let mut addresses = addresses.into_iter();
        self.leaf_layouts.retain_mut(|info| match addresses.next() {
            Some(Some(path)) => {
                info.path = path;
                true
            }
            // The leaf left the tree while the transaction was open; nothing on screen can
            // belong to it any more.
            _ => false,
        });
    }

    pub(in crate::layout) fn apply_pending_layouts_if_ready(&mut self) -> bool {
        let Some(pending) = &self.pending_layouts else {
            return false;
        };
        if pending.blocker.state() != BlockerState::Released {
            return false;
        }
        let pending = self.pending_layouts.take().unwrap();
        self.apply_layout_data(pending.data);
        self.debug_layout_state("layout_atomic_apply_pending");
        true
    }

    pub(in crate::layout) fn has_pending_layouts(&self) -> bool {
        self.pending_layouts.is_some()
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

    fn collect_layout_data(&self, root_key: NodeKey) -> LayoutData {
        let mut data = LayoutData {
            leaf_layouts: Vec::new(),
            container_geometries: HashMap::new(),
            tab_bar_offsets: HashMap::new(),
            titlebar_flags: HashMap::new(),
            tabbed_context_flags: HashMap::new(),
        };

        let mut path = Vec::new();
        let area = self.layout_area();
        self.collect_layout_node(
            root_key,
            area,
            &mut path,
            true,
            LeafLayoutContext::default(),
            &mut data,
        );
        data
    }

    /// Compute the rects the children of a container occupy inside `rect`.
    ///
    /// This is the single authority for the layout algorithm: split layouts distribute the
    /// gap-adjusted span according to `percents`; tabbed/stacked layouts give every child the
    /// shared content rect below the tab bar. The second return value is the tab-bar offset
    /// (0.0 for split layouts).
    pub(super) fn child_rects_for_layout(
        &self,
        layout: Layout,
        rect: Rectangle<f64, Logical>,
        child_count: usize,
        percents: &[f64],
    ) -> (Vec<Rectangle<f64, Logical>>, f64) {
        if child_count == 0 {
            return (Vec::new(), 0.0);
        }

        let gap = self.options.layout.gaps;
        match layout {
            Layout::SplitH | Layout::SplitV => {
                let horizontal = layout == Layout::SplitH;
                let span = if horizontal { rect.size.w } else { rect.size.h };
                let total_gap = if child_count > 1 {
                    gap * (child_count as f64 - 1.0)
                } else {
                    0.0
                };
                let available = (span - total_gap).max(0.0);
                let lengths = self.distribute_split_lengths(available, child_count, percents);

                let mut cursor = if horizontal { rect.loc.x } else { rect.loc.y };
                let mut rects = Vec::with_capacity(child_count);
                for idx in 0..child_count {
                    let length = *lengths.get(idx).unwrap_or(&0.0);
                    let child_rect = if horizontal {
                        Rectangle::new(
                            Point::from((cursor, rect.loc.y)),
                            Size::from((length, rect.size.h)),
                        )
                    } else {
                        Rectangle::new(
                            Point::from((rect.loc.x, cursor)),
                            Size::from((rect.size.w, length)),
                        )
                    };
                    rects.push(child_rect);
                    cursor += length + gap;
                }
                (rects, 0.0)
            }
            Layout::Tabbed | Layout::Stacked => {
                let bar_row_height = self.tab_bar_row_height();
                let mut tab_offset = 0.0;
                if bar_row_height > 0.0 {
                    let bar_height = match layout {
                        Layout::Tabbed => bar_row_height,
                        Layout::Stacked => bar_row_height * child_count as f64,
                        _ => 0.0,
                    };
                    tab_offset = (bar_height + self.tab_bar_spacing())
                        .min(rect.size.h)
                        .max(0.0);
                }

                let mut content_rect = rect;
                if tab_offset > 0.0 {
                    content_rect.loc.y += tab_offset;
                    content_rect.size.h = (content_rect.size.h - tab_offset).max(0.0);
                }
                (vec![content_rect; child_count], tab_offset)
            }
        }
    }

    fn collect_layout_node(
        &self,
        node_key: NodeKey,
        rect: Rectangle<f64, Logical>,
        path: &mut Vec<usize>,
        visible: bool,
        ctx: LeafLayoutContext,
        data: &mut LayoutData,
    ) {
        let (layout, child_count, focused_idx) = match self.get_node(node_key) {
            Some(NodeData::Leaf(tile)) => {
                let (offset, show_titlebar) = if tile.window().pending_sizing_mode().is_fullscreen()
                {
                    (0.0, false)
                } else {
                    (ctx.tab_bar_offset, ctx.draw_titlebar)
                };
                data.tab_bar_offsets.insert(node_key, offset);
                data.titlebar_flags.insert(node_key, show_titlebar);
                data.tabbed_context_flags
                    .insert(node_key, ctx.in_tabbed_context);
                data.leaf_layouts.push(LeafLayoutInfo {
                    key: node_key,
                    path: path.clone(),
                    rect,
                    visible,
                });
                return;
            }
            Some(NodeData::Container(container)) => {
                data.container_geometries.insert(node_key, rect);
                (
                    container.layout(),
                    container.child_count(),
                    container.focused_child_index(),
                )
            }
            None => return,
        };

        if child_count == 0 {
            return;
        }

        let percents =
            self.get_normalized_child_percents(node_key, child_count);
        let (child_rects, _) = self.child_rects_for_layout(layout, rect, child_count, &percents);

        match layout {
            Layout::SplitH | Layout::SplitV => {
                let split_bar_height = self.split_title_bar_height();

                for (idx, &child_rect) in child_rects.iter().enumerate() {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };

                    path.push(idx);
                    let (child_offset, child_titlebar) =
                        self.split_child_titlebar(child_key, split_bar_height);
                    let child_ctx = LeafLayoutContext {
                        tab_bar_offset: child_offset,
                        draw_titlebar: child_titlebar,
                        in_tabbed_context: ctx.in_tabbed_context,
                    };
                    self.collect_layout_node(child_key, child_rect, path, visible, child_ctx, data);
                    path.pop();
                }
            }
            Layout::Tabbed | Layout::Stacked => {
                let focused_idx = focused_idx.unwrap_or(0).min(child_count.saturating_sub(1));

                for (idx, &child_rect) in child_rects.iter().enumerate() {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };
                    path.push(idx);
                    let child_visible = visible && idx == focused_idx;
                    let child_ctx = LeafLayoutContext {
                        tab_bar_offset: 0.0,
                        draw_titlebar: false,
                        in_tabbed_context: true,
                    };
                    self.collect_layout_node(
                        child_key,
                        child_rect,
                        path,
                        child_visible,
                        child_ctx,
                        data,
                    );
                    path.pop();
                }
            }
        }
    }

    fn changed_layout_keys(&self, data: &LayoutData) -> HashSet<NodeKey> {
        let mut current = HashMap::new();
        for info in &self.leaf_layouts {
            let Some(tile) = self.get_tile(info.key) else {
                continue;
            };
            let request = self.layout_request_for(tile, info.rect.size, tile.tab_bar_offset());
            current.insert(info.key, request);
        }

        let mut changed = HashSet::new();
        for info in &data.leaf_layouts {
            let offset = data.tab_bar_offsets.get(&info.key).copied().unwrap_or(0.0);
            let Some(tile) = self.get_tile(info.key) else {
                changed.insert(info.key);
                continue;
            };
            let request = self.layout_request_for(tile, info.rect.size, offset);
            if current.get(&info.key).is_none_or(|old| *old != request) {
                changed.insert(info.key);
            }
        }

        changed
    }

    fn request_sizes_for_layout(
        &mut self,
        data: &LayoutData,
        changed: &HashSet<NodeKey>,
        transaction: &Transaction,
        animate_resize: bool,
    ) {
        for info in &data.leaf_layouts {
            let Some(tile) = self.get_tile_mut(info.key) else {
                continue;
            };
            let offset = data.tab_bar_offsets.get(&info.key).copied().unwrap_or(0.0);
            let show_titlebar = data.titlebar_flags.get(&info.key).copied().unwrap_or(false);
            let in_tabbed_context = data
                .tabbed_context_flags
                .get(&info.key)
                .copied()
                .unwrap_or(false);
            let old_offset = tile.tab_bar_offset();
            let old_titlebar = tile.draw_titlebar();
            let old_tabbed_context = tile.in_tabbed_context();
            tile.set_tab_bar_offset(offset);
            tile.set_draw_titlebar(show_titlebar);
            tile.set_in_tabbed_context(in_tabbed_context);

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

    fn apply_layout_data(&mut self, data: LayoutData) {
        for (key, rect) in data.container_geometries {
            if let Some(NodeData::Container(container)) = self.get_node_mut(key) {
                container.set_geometry(rect);
            }
        }
        for (key, offset) in data.tab_bar_offsets {
            if let Some(tile) = self.get_tile_mut(key) {
                tile.set_tab_bar_offset(offset);
            }
        }
        for (key, show_titlebar) in data.titlebar_flags {
            if let Some(tile) = self.get_tile_mut(key) {
                tile.set_draw_titlebar(show_titlebar);
            }
        }
        for (key, in_tabbed_context) in data.tabbed_context_flags {
            if let Some(tile) = self.get_tile_mut(key) {
                tile.set_in_tabbed_context(in_tabbed_context);
            }
        }
        self.leaf_layouts = data.leaf_layouts;
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
    ) -> Vec<f64> {
        let Some(NodeData::Container(container)) = self.get_node(container_key) else {
            return vec![1.0 / child_count.max(1) as f64; child_count];
        };
        super::resolved_percents(container.child_percents_slice(), child_count)
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

        let is_leaf = matches!(self.get_node(child_key), Some(NodeData::Leaf(_)));
        if is_leaf {
            (split_bar_height, true)
        } else {
            (0.0, false)
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
