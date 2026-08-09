use std::collections::{HashMap, HashSet};

use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{Blocker, BlockerState};

use super::{ContainerTree, Layout, LayoutElement, LeafLayoutInfo, NodeData, NodeKey};
use crate::layout::tab_bar::tab_bar_row_height;
use crate::layout::tile::Tile;
use crate::utils::transaction::{Transaction, TransactionBlocker};

impl LayoutData {
    /// Take another branch's results in.
    ///
    /// A branch that was arranged separately says nothing about the leaves outside it, so its
    /// held entries — the ones it carried over unchanged — are dropped rather than allowed to
    /// overwrite what the other branches just worked out.
    fn absorb(&mut self, other: LayoutData) {
        let mine: HashSet<NodeKey> = self.leaf_layouts.iter().map(|info| info.key).collect();
        self.leaf_layouts.extend(
            other
                .leaf_layouts
                .into_iter()
                .filter(|info| !mine.contains(&info.key)),
        );
        self.container_geometries.extend(other.container_geometries);
        self.tab_bar_offsets.extend(other.tab_bar_offsets);
        self.titlebar_flags.extend(other.titlebar_flags);
        self.tabbed_context_flags.extend(other.tabbed_context_flags);
    }
}

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

        let snapshot: HashSet<(NodeKey, NodeKey)> = pending
            .data
            .leaf_layouts
            .iter()
            .map(|info| (info.key, info.branch))
            .collect();
        // Only leaves reachable from the root count: the slotmap can still hold nodes that
        // were detached but not yet dropped.
        let current: HashSet<(NodeKey, NodeKey)> = self
            .dfs_leaf_keys()
            .into_iter()
            .map(|key| (key, self.branch_root(key)))
            .collect();

        snapshot != current
    }

    /// Point `workspace->fullscreen` at a node, or at nothing.
    ///
    /// Whoever grants or revokes fullscreen is the one who knows; the arrange pass only reads
    /// this. Setting it does not itself arrange — the caller was in the middle of a command
    /// and will.
    pub(in crate::layout) fn set_fullscreen_key(&mut self, key: Option<NodeKey>) {
        self.fullscreen_key = key;
    }

    fn layout_atomic(&mut self, animate_resize: bool) {
        if self.pending_layouts.is_some() && !self.apply_pending_layouts_if_ready() {
            // This call only records that another layout is wanted. It is not one of sway's
            // `arrange_workspace` passes, so it must not resolve fractions yet. Doing that
            // here normalized the same list again while a client configure was outstanding;
            // a later sibling swap then rounded the opposite half-pixel and made that pixel
            // survive the next resize and close.
            //
            // sway/tree/arrange.c:15-55,100-140
            self.pending_relayout = true;
            self.readdress_leaf_layouts();
            self.debug_layout_state("layout_atomic_pending");
            return;
        }
        self.pending_relayout = false;

        self.resolve_percents(self.root);
        for key in self.floating_roots().collect::<Vec<_>>() {
            self.resolve_percents(key);
        }

        let root_key = self.root;
        if self.is_empty() && self.floating_roots().next().is_none() {
            self.leaf_layouts.clear();
            self.pending_layouts = None;
            self.pending_transaction = None;
            self.pending_relayout = false;
            self.debug_layout_state("layout_atomic_empty");
            return;
        }

        // sway's `arrange_workspace` asks one question before it lays anything out: is there a
        // fullscreen container? If there is, it gives that container the output's box, arranges
        // it, and returns — the tiled tree underneath is never descended, so every node outside
        // the fullscreen keeps whatever box it last had.
        let mut data = match self
            .fullscreen_key
            .filter(|key| self.nodes.contains_key(*key))
        {
            Some(fullscreen_key) => self.collect_fullscreen_layout_data(fullscreen_key),
            None => self.collect_layout_data(root_key),
        };
        // `arrange_workspace` lays out the two sides with two calls — `arrange_children` for
        // the tiling list against the workspace's box, `arrange_floating` for the groups
        // against their own — and neither knows about the other. The fullscreen branch above
        // returns before either of them, which is why a fullscreen hides the floating windows
        // as well as the tiled ones.
        for root in self.floating_roots_with_areas() {
            let branch = self.collect_branch_layout_data(root.key, root.area);
            data.absorb(branch);
        }
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

    /// Fill in every share left unset, from the workspace down.
    ///
    /// This is where sway decides a size share, and the only place: `apply_horiz_layout` runs
    /// as `arrange_container` descends, and it writes what it worked out back onto the
    /// children. So it happens once per arrange rather than once per command, it covers every
    /// command rather than the ones that remembered to ask, and a command is free to leave a
    /// share unset while it moves the tree — which is what `cmd_move` does, and what makes
    /// the deferral mean anything.
    fn resolve_percents(&mut self, key: NodeKey) {
        let Some(children) = self.get_container(key).map(|c| c.children().to_vec()) else {
            return;
        };
        if let Some(container) = self.get_container_mut(key) {
            container.resolve_child_percents();
        }
        for child in children {
            self.resolve_percents(child);
        }
    }

    /// Every floating group with the box it is laid out in.
    fn floating_roots_with_areas(&self) -> Vec<super::FloatingRoot> {
        self.floating_roots_snapshot()
    }

    /// Point the cached layout at where its leaves are *now*.
    ///
    /// While a resize is in flight the cached geometry is deliberately the old one — it is
    /// what is still on screen — but the path beside it is an address, not geometry, and an
    /// address of a tree that has moved on is simply wrong. A structural change during a
    /// transaction is what pulls the two apart: the leaves are the same, their rectangles
    /// are the same, and they are somewhere else.
    pub(in crate::layout) fn readdress_leaf_layouts(&mut self) {
        let addresses: HashMap<NodeKey, (Vec<usize>, NodeKey)> = self
            .nodes
            .keys()
            .filter_map(|key| {
                matches!(self.get_node(key), Some(NodeData::Leaf(_))).then(|| {
                    Some((
                        key,
                        (self.branch_relative_path(key)?, self.branch_root(key)),
                    ))
                })?
            })
            .collect();
        self.leaf_layouts.retain_mut(|info| {
            let Some((path, branch)) = addresses.get(&info.key) else {
                // The leaf left the tree while the transaction was open; nothing on screen
                // can belong to it any more.
                return false;
            };
            info.path.clone_from(path);
            info.branch = *branch;
            true
        });
        if let Some(pending) = &mut self.pending_layouts {
            pending.data.leaf_layouts.retain_mut(|info| {
                let Some((path, branch)) = addresses.get(&info.key) else {
                    return false;
                };
                // A path is an address in one branch and follows structural changes there.
                // The branch beside a pending rectangle is provenance: changing it would
                // make a floating configure look valid after the node moved to tiling (or
                // vice versa), and the state pass would flush bounds for the wrong side.
                if info.branch == *branch {
                    info.path.clone_from(path);
                }
                true
            });
        }
    }

    /// Keep a leaf's node box when tree surgery changes how its parent decorates it.
    ///
    /// A leaf directly under tabs is the awkward representation boundary in tiri: the
    /// cached rectangle is its content box, while [`ContainerTree::node_geometry`] answers
    /// with the whole pending box sway keeps on the container. If `cmd_layout` flattens that
    /// parent and then finds that the surviving layout already has the requested value, it
    /// does not arrange. sway's box nevertheless stays whole; only the tab decoration that
    /// IPC derives from the old parent disappears. Preserve that box in both snapshots so a
    /// later transaction cannot put the removed decoration back.
    ///
    /// sway/commands/layout.c:134-196
    /// sway/tree/container.c:1534-1554
    pub(super) fn preserve_leaf_node_geometry(&mut self, key: NodeKey) {
        let preserve = |layouts: &mut Vec<LeafLayoutInfo>| {
            if let Some(info) = layouts.iter_mut().find(|info| info.key == key) {
                info.rect = info.node_rect;
            }
        };
        preserve(&mut self.leaf_layouts);
        if let Some(pending) = &mut self.pending_layouts {
            preserve(&mut pending.data.leaf_layouts);
        }
    }

    /// Update which already-arranged leaves are visible after seat focus changes.
    ///
    /// `cmd_focus` does not call `arrange_workspace`; focus changes the active child of a
    /// tabbed or stacked container without renormalizing any split fractions. Re-running the
    /// full layout here used to add a division that sway never performs, enough to move an
    /// exact half-pixel to the other end of a split. Rectangles stay as arranged; only the
    /// switcher visibility derived from the seat's focus order changes.
    ///
    /// sway/commands/focus.c:270-321
    /// sway/input/seat.c:1192-1270
    pub(in crate::layout) fn refresh_focus_visibility(&mut self) {
        let visibility: HashMap<NodeKey, bool> = self
            .nodes
            .keys()
            .filter(|key| matches!(self.get_node(*key), Some(NodeData::Leaf(_))))
            .map(|key| (key, self.focus_makes_leaf_visible(key)))
            .collect();
        let refresh = |layouts: &mut Vec<LeafLayoutInfo>| {
            for info in layouts {
                if let Some(visible) = visibility.get(&info.key) {
                    info.visible = *visible;
                }
            }
        };
        refresh(&mut self.leaf_layouts);
        if let Some(pending) = &mut self.pending_layouts {
            refresh(&mut pending.data.leaf_layouts);
        }
    }

    fn focus_makes_leaf_visible(&self, key: NodeKey) -> bool {
        if self
            .fullscreen_key
            .is_some_and(|fullscreen| !self.is_descendant(key, fullscreen))
        {
            return false;
        }

        let branch = self.branch_root(key);
        let mut child = key;
        while child != branch {
            let Some(parent) = self.parent_of(child) else {
                return false;
            };
            if self.get_container(parent).is_some_and(|container| {
                matches!(container.layout(), Layout::Tabbed | Layout::Stacked)
            }) && self.active_child(parent) != Some(child)
            {
                return false;
            }
            child = parent;
        }
        true
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

    /// Drop a size transaction superseded by a branch transfer.
    ///
    /// Floating a node immediately issues its restored size, so a configure for its old tiled
    /// box cannot govern the new branch. Maximizing a floating node is the same kind of
    /// superseding transition in the other direction: `container_set_floating` moves it first
    /// (`sway/tree/container.c:1004`) and the maximized arrange decides its next size. An ordinary
    /// floating-to-tiling move still waits for its outstanding configure.
    pub(in crate::layout) fn discard_layout_superseded_by_transfer(&mut self) {
        self.pending_layouts = None;
        self.pending_transaction = None;
        self.pending_relayout = false;
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
        let gap = self.gap_in(root_key);
        self.collect_layout_node(
            root_key,
            area,
            area,
            &mut path,
            true,
            LeafLayoutContext::default(),
            gap,
            root_key,
            &mut data,
        );
        data
    }

    /// The gap between a branch's children.
    ///
    /// `gaps inner` is a tiling setting: sway's `container_add_gaps` returns without doing
    /// anything for a floating container, so a group of windows floated together sits flush
    /// however the workspace is configured.
    pub(super) fn gap_in(&self, key: NodeKey) -> f64 {
        if self.is_floating(key) {
            0.0
        } else {
            self.options.layout.gaps
        }
    }

    /// Lay out only the fullscreen node's subtree, against the output.
    ///
    /// The other half of `arrange_workspace`'s fullscreen branch. sway reaches it with a whole
    /// tree of containers still holding the `pending` boxes it gave them last time and simply
    /// does not visit them, so that is what the leaves outside the subtree get here: the
    /// entries they already had. They are not stale — they are the answer, until the
    /// fullscreen goes away and the tree is arranged again.
    ///
    /// sway hands the node `output->lx/ly/width/height` — the whole output, bar and gaps
    /// included — and the layout area is used here instead. The difference is not observable:
    /// the one place the fullscreen rectangle is published is the IPC tree, and that already
    /// answers with the output's box regardless of what the node holds. What the node's box
    /// still drives is the size asked of the window, and a fullscreen tile is sized from the
    /// view rather than from its slot, so feeding the output box in twice only makes the
    /// unfullscreen look like a resize it is not.
    fn collect_fullscreen_layout_data(&self, fullscreen_key: NodeKey) -> LayoutData {
        self.collect_branch_layout_data(fullscreen_key, self.layout_area())
    }

    /// Arrange one branch inside one rectangle, holding everything outside it.
    ///
    /// The tiled tree is one branch of many now: the workspace's, plus a root per floating
    /// group, each of which is laid out in its own rectangle rather than in the workspace's.
    /// sway arranges them separately for the same reason — `arrange_workspace` calls
    /// `arrange_children` for the tiling and `arrange_floating` for the rest, and neither
    /// knows about the other.
    ///
    /// It arrived as the fullscreen branch, which is the same operation with one caller: give
    /// this node the box and do not descend anything else.
    pub(in crate::layout) fn collect_branch_layout_data(
        &self,
        branch_root: NodeKey,
        area: Rectangle<f64, Logical>,
    ) -> LayoutData {
        let mut data = LayoutData {
            leaf_layouts: Vec::new(),
            container_geometries: HashMap::new(),
            tab_bar_offsets: HashMap::new(),
            titlebar_flags: HashMap::new(),
            tabbed_context_flags: HashMap::new(),
        };

        // Seeded with where the node actually is, so the addresses beside the geometry stay
        // absolute — they are read as paths from the workspace, not from here.
        //
        // A floating root has no path from the workspace, because it hangs off the workspace's
        // other list rather than off its tiling tree, so its branch is addressed from itself.
        //
        // That is not a decision taken here — it is the one sway's IPC already took, and the
        // one tiri already follows. `get_tree` gives a workspace two arrays, `nodes` and
        // `floating_nodes`, and a floating container is addressed as its index in the second
        // plus a path within itself. `LayoutTree` has the same shape: `root` for the tiled
        // side and `floating` for the groups, each node's path being "within its tree". The
        // several roots were always there in what tiri publishes; only the arena was split.
        let mut path = match self.find_node_path(branch_root) {
            Some(path) => path,
            None if self.floating_roots().any(|root| root == branch_root) => Vec::new(),
            None => return self.collect_layout_data(self.root),
        };
        self.collect_layout_node(
            branch_root,
            area,
            area,
            &mut path,
            true,
            LeafLayoutContext::default(),
            self.gap_in(branch_root),
            self.branch_root(branch_root),
            &mut data,
        );

        // The held entries keep their rectangle — that is the box sway is not revisiting — but
        // not their address: the tree can be reshaped while a fullscreen is up, and a path
        // into a tree that has moved on is simply wrong. A leaf that is gone is dropped.
        let arranged: HashSet<NodeKey> = data.leaf_layouts.iter().map(|info| info.key).collect();
        let held: Vec<LeafLayoutInfo> = self
            .leaf_layouts
            .iter()
            .filter(|info| !arranged.contains(&info.key))
            .filter_map(|info| {
                Some(LeafLayoutInfo {
                    path: self.branch_relative_path(info.key)?,
                    ..info.clone()
                })
            })
            .collect();
        data.leaf_layouts.extend(held);
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
        gap: f64,
    ) -> (Vec<Rectangle<f64, Logical>>, f64) {
        if child_count == 0 {
            return (Vec::new(), 0.0);
        }

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
        node_rect: Rectangle<f64, Logical>,
        path: &mut Vec<usize>,
        visible: bool,
        ctx: LeafLayoutContext,
        gap: f64,
        branch: NodeKey,
        data: &mut LayoutData,
    ) {
        let (layout, child_count, focused_idx, percents) = match self.get_node(node_key) {
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
                    branch,
                    path: path.clone(),
                    rect,
                    node_rect,
                    visible,
                });
                return;
            }
            Some(NodeData::Container(container)) => {
                data.container_geometries.insert(node_key, rect);
                (
                    container.layout(),
                    container.child_count(),
                    self.active_child_index(node_key),
                    container.child_percents_slice().to_vec(),
                )
            }
            None => return,
        };

        if child_count == 0 {
            return;
        }

        let (child_rects, _) =
            self.child_rects_for_layout(layout, rect, child_count, &percents, gap);

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
                    self.collect_layout_node(
                        child_key, child_rect, child_rect, path, visible, child_ctx, gap, branch,
                        data,
                    );
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
                    // A view gets the switcher's whole parent box and IPC adds its title bar
                    // afterwards. A child container is itself placed below that bar.
                    //
                    // sway/tree/arrange.c:185-211
                    let child_node_rect =
                        if matches!(self.get_node(child_key), Some(NodeData::Leaf(_))) {
                            rect
                        } else {
                            child_rect
                        };
                    self.collect_layout_node(
                        child_key,
                        child_rect,
                        child_node_rect,
                        path,
                        child_visible,
                        child_ctx,
                        gap,
                        branch,
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

    pub(super) fn apply_layout_data(&mut self, data: LayoutData) {
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
        if sum <= f64::EPSILON {
            weights.fill(default);
        }

        // `apply_horiz_layout`/`apply_vert_layout` round every child's fraction in list
        // order immediately after normalizing it in place, then overwrite the last child's
        // size with the parent's remaining extent (`sway/tree/arrange.c:15-96,100-181`). Do
        // not normalize a copy here: a second division can move an exact half-pixel across
        // the rounding boundary. It is deliberately not a largest-remainder distribution:
        // seven equal children can therefore be six equal rounded spans and a wider last
        // one. That asymmetry survives later reparenting and is observable.
        let mut lengths_int = Vec::with_capacity(child_count);
        let mut used = 0i32;
        for (idx, weight) in weights.iter().copied().enumerate() {
            let length = if idx + 1 == child_count {
                (available_phys - used).max(0)
            } else {
                (available_phys as f64 * weight).round().max(0.0) as i32
            };
            lengths_int.push(length);
            used = used.saturating_add(length);
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
