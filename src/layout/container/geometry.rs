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

/// What an arrange pass carries from its root all the way down.
///
/// The two invariants — which branch is being arranged and the gap between its children —
/// and the two accumulators. They were nine loose parameters threaded through a recursion,
/// where the only ones that actually change on the way down are the node and its box.
struct LayoutWalk<'a> {
    branch: NodeKey,
    gap: f64,
    path: Vec<usize>,
    data: &'a mut LayoutData,
}

#[derive(Debug)]
/// One branch's arrange, held back until the windows it asked to resize have answered.
///
/// `arrange_workspace` lays the workspace out with two kinds of call — `arrange_children` for
/// the tiling list against the workspace's box, `arrange_floating` for each group against its
/// own — and neither knows about the other. A transaction is what makes one of those atomic
/// on screen: the new boxes wait until every window that has to change has acked, so a
/// workspace is never seen half-resized. That is a question about windows that share space,
/// and windows in different branches share none.
///
/// So there is one of these per branch, not one per workspace. With one per workspace, a
/// window that has been off the workspace and has no reason to answer a configure promptly —
/// a scratchpad window being shown — held every other window's arrange behind it for the
/// whole three-hundred-millisecond deadline, and before deadlines were armed at all, forever.
///
/// sway/tree/arrange.c:264-322
pub(super) struct PendingLayout {
    pub(super) branch: NodeKey,
    pub(in crate::layout) data: LayoutData,
    blocker: TransactionBlocker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutRequestMode {
    Normal,
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
        self.layout_atomic(animate_resize, None);
    }

    /// sway's `arrange_container`: arrange one real container against the pending box it is
    /// currently holding, without applying the workspace fullscreen branch first.
    pub(in crate::layout) fn layout_container_subtree(&mut self, key: NodeKey) {
        if self.get_container(key).is_none() {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.layout_atomic(true, Some(key));
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
        if self.pending_layouts.is_empty() {
            return false;
        }

        // Only leaves reachable from a root count: the store can still hold nodes that were
        // detached but not yet dropped.
        let current: HashSet<(NodeKey, NodeKey)> = self
            .dfs_leaf_keys()
            .into_iter()
            .map(|key| (key, self.branch_root(key)))
            .collect();

        // A branch is stale when the leaves it describes are no longer the leaves that branch
        // has. The other branches' leaves are in `current` too and are none of its business.
        self.pending_layouts.iter().any(|pending| {
            let snapshot: HashSet<(NodeKey, NodeKey)> = pending
                .data
                .leaf_layouts
                .iter()
                .map(|info| (info.key, info.branch))
                .collect();
            let now: HashSet<(NodeKey, NodeKey)> = current
                .iter()
                .filter(|(_, branch)| *branch == pending.branch)
                .copied()
                .collect();
            snapshot != now
        })
    }

    /// The one node currently owning workspace fullscreen.
    ///
    /// This is sway's `workspace->fullscreen`: it spans both the tiled tree and the floating
    /// roots. The window id and its side are projections of this key, never parallel state.
    pub(in crate::layout) fn fullscreen_key(&self) -> Option<NodeKey> {
        self.fullscreen_key
    }

    /// Window that directly owns fullscreen at the client protocol boundary.
    ///
    /// A fullscreen container has no such window: its descendants remain ordinary tiled
    /// clients while the container's subtree occupies the output.
    pub(in crate::layout) fn fullscreen_leaf_window_id(&self) -> Option<&W::Id> {
        self.fullscreen_key
            .and_then(|key| self.get_tile(key))
            .map(|tile| tile.window().id())
    }

    pub(in crate::layout) fn window_owns_fullscreen(&self, id: &W::Id) -> bool {
        self.fullscreen_leaf_window_id()
            .is_some_and(|current| current == id)
    }

    /// A window representative for APIs that cannot name a container.
    ///
    /// sway's workspace pointer can name a container. Existing tiri inspection APIs expose
    /// window IDs, so descend through the seat's inactive-focus order rather than inventing a
    /// second fullscreen owner.
    pub(in crate::layout) fn fullscreen_representative_window_id(&self) -> Option<&W::Id> {
        let key = self.fullscreen_key?;
        let leaf = self.focus_inactive_view(key)?;
        self.get_tile(leaf).map(|tile| tile.window().id())
    }

    /// Point `workspace->fullscreen` at a live node, or at nothing.
    ///
    /// Setting it does not arrange: callers still have side-specific window state to request
    /// before arranging. The workspace node is not a fullscreen target; every other live
    /// node can be one.
    pub(in crate::layout) fn set_fullscreen_key(&mut self, key: Option<NodeKey>) -> bool {
        if key.is_some_and(|key| key == self.root || !self.holds_node(key)) {
            return false;
        }
        if self.fullscreen_key == key {
            return false;
        }
        if let Some(old) = self.fullscreen_key {
            self.set_node_fullscreen_restore_geometry(old, None);
        }
        if let Some(key) = key {
            let restore = self.node_geometry(key).unwrap_or_default();
            self.set_node_fullscreen_restore_geometry(key, Some(restore));
        }
        self.fullscreen_key = key;
        true
    }

    /// The box this node held immediately before entering workspace fullscreen.
    pub(in crate::layout) fn fullscreen_restore_geometry(
        &self,
        key: NodeKey,
    ) -> Option<Rectangle<f64, Logical>> {
        self.node_sizing(key)?.fullscreen_restore_geometry
    }

    fn set_node_fullscreen_restore_geometry(
        &mut self,
        key: NodeKey,
        geometry: Option<Rectangle<f64, Logical>>,
    ) {
        if let Some(sizing) = self.node_sizing_mut(key) {
            sizing.fullscreen_restore_geometry = geometry;
        }
    }

    /// Transfer fullscreen exactly as sway's `container_replace` does.
    ///
    /// The workspace authority moves to the replacement node. Client fullscreen state only
    /// exists on leaves, so wrapping a fullscreen leaf revokes that state, while collapsing a
    /// fullscreen container onto a leaf grants it to the replacement.
    pub(super) fn transfer_fullscreen_to_replacement(
        &mut self,
        old_key: NodeKey,
        new_key: NodeKey,
    ) {
        if self.fullscreen_key != Some(old_key) {
            return;
        }

        let animate = !self.options.animations.off;
        let working_area_size = self.working_area.size;
        let replacement_restore = self.node_geometry(new_key).unwrap_or_default();
        if let Some(tile) = self.get_tile_mut(old_key) {
            tile.request_tile_size(working_area_size, animate, None);
        }

        if let Some(tile) = self.get_tile_mut(new_key) {
            tile.request_fullscreen(animate, None);
        }

        self.set_node_fullscreen_restore_geometry(old_key, None);
        self.set_node_fullscreen_restore_geometry(new_key, Some(replacement_restore));
        self.fullscreen_key = Some(new_key);
    }

    fn layout_atomic(&mut self, animate_resize: bool, subtree: Option<NodeKey>) {
        self.apply_pending_layouts_if_ready();
        if !self.pending_layouts.is_empty() {
            // A branch still waiting is not re-arranged: this call only records that another
            // layout is wanted. It is not one of sway's `arrange_workspace` passes, so it must
            // not resolve fractions yet. Doing that here normalized the same list again while
            // a client configure was outstanding; a later sibling swap then rounded the
            // opposite half-pixel and made that pixel survive the next resize and close.
            //
            // sway/tree/arrange.c:15-55,100-140
            self.pending_relayout = true;
            self.debug_layout_state("layout_atomic_pending");
        } else {
            self.pending_relayout = false;
        }

        let root_key = self.root;
        if self.is_empty() && self.floating_roots().next().is_none() {
            self.leaf_layouts.clear();
            self.pending_layouts.clear();
            self.pending_transaction = None;
            self.pending_relayout = false;
            self.debug_layout_state("layout_atomic_empty");
            return;
        }

        // A branch that is still waiting is not touched at all — not arranged and, more
        // importantly, not resolved. Resolving a branch's shares is a write: doing it while
        // one of its configures is outstanding normalized the same list twice, and a later
        // sibling swap then rounded the opposite half-pixel and made that pixel survive the
        // next resize and close.
        //
        // sway/tree/arrange.c:15-55,100-140
        let waiting: HashSet<NodeKey> = self
            .pending_layouts
            .iter()
            .map(|pending| pending.branch)
            .collect();
        let resolve_roots = subtree.map_or_else(
            || {
                std::iter::once(root_key)
                    .chain(self.floating_roots().collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            },
            |key| vec![key],
        );
        for key in resolve_roots {
            if !waiting.contains(&self.branch_root(key)) {
                self.resolve_percents(key);
            }
        }

        let mut requested = false;
        let arrangements = match subtree {
            Some(key) => self.node_geometry(key).map_or_else(Vec::new, |area| {
                vec![(
                    self.branch_root(key),
                    self.collect_branch_layout_data(key, area),
                )]
            }),
            None => self.arrange_each_branch(),
        };
        for (branch, data) in arrangements {
            if waiting.contains(&branch) {
                // Still waiting on its own windows. Its neighbours are not.
                self.pending_relayout = true;
                continue;
            }

            self.record_child_totals(&data);
            let changed = self.changed_layout_keys(&data);
            if changed.is_empty() {
                self.apply_layout_data(branch, data);
                continue;
            }

            // The caller's transaction, when there is one, belongs to the tiled side: every
            // caller that sets one is removing a tiled window, and its close animation has to
            // stay in step with the windows that grow into the space.
            let transaction = if branch == root_key {
                self.pending_transaction.take()
            } else {
                None
            }
            .unwrap_or_else(Transaction::new);

            self.request_sizes_for_layout(&data, &changed, &transaction, animate_resize);
            let ready_now = transaction.is_last();
            self.pending_layouts.push(PendingLayout {
                branch,
                data,
                blocker: transaction.blocker(),
            });
            drop(transaction);
            if ready_now {
                self.apply_pending_layouts_if_ready();
            } else {
                requested = true;
            }
        }
        self.pending_transaction = None;

        // A pass does not necessarily reach every branch. One waiting on its own configures is
        // skipped on purpose, and a fullscreen makes `arrange_each_branch` return that branch
        // alone — sway's `arrange_workspace` never descends the rest either. Those branches
        // keep the geometry they last had, which is what is still on screen, but a path is an
        // address rather than geometry: the tree may have moved under it while it was not
        // being arranged. Re-point every cached leaf at where it is now; for the branches this
        // pass did arrange, the addresses it just wrote are these same ones.
        self.readdress_leaf_layouts();

        if requested {
            self.debug_layout_state("layout_atomic_requested");
        } else {
            self.debug_layout_state("layout_atomic_apply");
        }
    }

    /// Every branch of the workspace, each laid out in its own box.
    ///
    /// sway's `arrange_workspace` asks one question before it lays anything out: is there a
    /// fullscreen container? If there is, it gives that container the output's box, arranges
    /// it, and returns — the tiled tree underneath is never descended, so every node outside
    /// the fullscreen keeps whatever box it last had, and the floating groups are never
    /// reached either. That is why a fullscreen hides them as well as the tiled windows, and
    /// it falls out rather than being arranged for.
    ///
    /// sway/tree/arrange.c:264-322
    fn arrange_each_branch(&mut self) -> Vec<(NodeKey, LayoutData)> {
        let root_key = self.root;
        if let Some(fullscreen_key) = self
            .fullscreen_key
            .filter(|key| self.nodes.contains_key(*key))
        {
            let branch = self.branch_root(fullscreen_key);
            return vec![(branch, self.collect_fullscreen_layout_data(fullscreen_key))];
        }

        let mut out = vec![(root_key, self.collect_layout_data(root_key))];
        for (root, area) in self.floating_roots_with_areas() {
            out.push((root, self.collect_branch_layout_data(root, area)));
        }
        out
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
        self.resolve_child_percents(key);
        for child in children {
            self.resolve_percents(child);
        }
    }

    /// Remember the exact gap-adjusted span each split used as its denominator.
    ///
    /// `apply_horiz_layout` and `apply_vert_layout` write this value onto every child before
    /// rounding that child's pending size. A later resize divides the rounded size by the
    /// stored value; recomputing it from a possibly changed sibling list loses that history.
    ///
    /// sway/tree/arrange.c:72-83,157-169
    fn record_child_totals(&mut self, data: &LayoutData) {
        let arranged: Vec<(NodeKey, Rectangle<f64, Logical>)> = data
            .container_geometries
            .iter()
            .map(|(key, rect)| (*key, *rect))
            .collect();
        for (parent, rect) in arranged {
            let Some(container) = self.get_container(parent) else {
                continue;
            };
            let layout = container.layout();
            if !matches!(layout, Layout::SplitH | Layout::SplitV) {
                continue;
            }
            let children = container.children().to_vec();
            let span = match layout {
                Layout::SplitH => rect.size.w,
                Layout::SplitV => rect.size.h,
                Layout::Tabbed | Layout::Stacked => unreachable!(),
            };
            let gaps = self.gap_in(parent) * children.len().saturating_sub(1) as f64;
            let total = (span - gaps).max(0.0);
            for child in children {
                self.set_node_child_total(child, layout, total);
            }
        }
    }

    /// Every floating group with the box it is laid out in.
    fn floating_roots_with_areas(&self) -> Vec<(NodeKey, Rectangle<f64, Logical>)> {
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
                self.get_node(key)
                    .is_some_and(|node| node.is_view())
                    .then(|| {
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
        for pending in &mut self.pending_layouts {
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
        for pending in &mut self.pending_layouts {
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
            .filter(|key| self.get_node(*key).is_some_and(|node| node.is_view()))
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
        for pending in &mut self.pending_layouts {
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

    /// Whether a deferred arrange still addresses the same live branch shape.
    ///
    /// Geometry may intentionally stay old until the transaction completes; node paths may
    /// not. A leaf that moved branches, gained a wrapper, or disappeared makes the snapshot
    /// unusable as committed cache data. The pending relayout will calculate a fresh one.
    fn pending_layout_addresses_are_current(&self, pending: &PendingLayout) -> bool {
        let described_branches: HashSet<NodeKey> = pending
            .data
            .leaf_layouts
            .iter()
            .map(|info| info.branch)
            .collect();
        let snapshot: HashSet<(NodeKey, NodeKey)> = pending
            .data
            .leaf_layouts
            .iter()
            .map(|info| (info.key, info.branch))
            .collect();
        let current: HashSet<(NodeKey, NodeKey)> = self
            .dfs_leaf_keys()
            .into_iter()
            .map(|key| (key, self.branch_root(key)))
            .filter(|(_, branch)| described_branches.contains(branch))
            .collect();

        snapshot == current
            && pending.data.leaf_layouts.iter().all(|info| {
                self.branch_relative_path(info.key).as_deref() == Some(info.path.as_slice())
            })
    }

    /// Commit every branch whose windows have answered. A branch still waiting keeps its
    /// place in the queue and its neighbours do not wait with it.
    pub(in crate::layout) fn apply_pending_layouts_if_ready(&mut self) -> bool {
        let ready: Vec<PendingLayout> = {
            let mut ready = Vec::new();
            let mut idx = 0;
            while idx < self.pending_layouts.len() {
                if self.pending_layouts[idx].blocker.state() == BlockerState::Released {
                    ready.push(self.pending_layouts.remove(idx));
                } else {
                    idx += 1;
                }
            }
            ready
        };
        if ready.is_empty() {
            return false;
        }
        for pending in ready {
            if !self.pending_layout_addresses_are_current(&pending) {
                self.pending_relayout = true;
                continue;
            }
            self.apply_layout_data(pending.branch, pending.data);
        }
        self.debug_layout_state("layout_atomic_apply_pending");
        true
    }

    pub(in crate::layout) fn has_pending_layouts(&self) -> bool {
        !self.pending_layouts.is_empty()
    }

    /// Drop a size transaction superseded by a branch transfer.
    ///
    /// Floating a node immediately issues its restored size, so a configure for its old tiled
    /// box cannot govern the new branch. Maximizing a floating node is the same kind of
    /// superseding transition in the other direction: `container_set_floating` moves it first
    /// (`sway/tree/container.c:1004`) and the maximized arrange decides its next size. An ordinary
    /// floating-to-tiling move still waits for its outstanding configure.
    pub(in crate::layout) fn discard_layout_superseded_by_transfer(&mut self) {
        self.pending_layouts.clear();
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

        let path = Vec::new();
        let area = self.layout_area();
        let gap = self.gap_in(root_key);
        let mut walk = LayoutWalk {
            branch: root_key,
            gap,
            path,
            data: &mut data,
        };
        self.collect_layout_node(
            root_key,
            area,
            area,
            true,
            LeafLayoutContext::default(),
            &mut walk,
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
    /// sway hands a fullscreen container `output->lx/ly/width/height`: the complete output,
    /// including the area normally reserved for layers and outer gaps. A fullscreen leaf is
    /// still sized through its client fullscreen request, so it keeps its ordinary slot here
    /// until that configure commits; a container has no such protocol state and must receive
    /// the output box directly.
    fn collect_fullscreen_layout_data(&self, fullscreen_key: NodeKey) -> LayoutData {
        let area = if self.get_container(fullscreen_key).is_some() {
            Rectangle::from_size(self.view_size)
        } else {
            self.layout_area()
        };
        let mut data = self.collect_branch_layout_data(fullscreen_key, area);
        if self.get_tile(fullscreen_key).is_some() {
            if let Some(info) = data
                .leaf_layouts
                .iter_mut()
                .find(|info| info.key == fullscreen_key)
            {
                info.workspace_fullscreen = true;
            }
        }
        data
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

        // Addresses are relative to the node's actual branch, not necessarily to the tiled
        // workspace root. This matters when fullscreen names a descendant of a floating
        // group: the subtree keeps its NodeKey authority but is arranged against the output.
        let path = match self.branch_relative_path(branch_root) {
            Some(path) => path,
            None => return self.collect_layout_data(self.root),
        };
        let mut walk = LayoutWalk {
            branch: self.branch_root(branch_root),
            gap: self.gap_in(branch_root),
            path,
            data: &mut data,
        };
        self.collect_layout_node(
            branch_root,
            area,
            area,
            true,
            LeafLayoutContext::default(),
            &mut walk,
        );

        // A pass describes its own branch and no other: what it does not mention is committed
        // separately, by whoever arranged it. Inside this branch, though, silence would be
        // taken for absence — so leaves this pass did not revisit keep their rectangle, which
        // is the box sway is not revisiting. Not their address: the tree can be reshaped while
        // a fullscreen is up, and a path into a tree that has moved on is simply wrong. A leaf
        // that is gone is dropped.
        let branch = self.branch_root(branch_root);
        let arranged: HashSet<NodeKey> = data.leaf_layouts.iter().map(|info| info.key).collect();
        let held: Vec<LeafLayoutInfo> = self
            .leaf_layouts
            .iter()
            .filter(|info| !arranged.contains(&info.key))
            .filter(|info| self.branch_root(info.key) == branch)
            .filter_map(|info| {
                Some(LeafLayoutInfo {
                    path: self.branch_relative_path(info.key)?,
                    ..info.clone()
                })
            })
            .collect();
        data.leaf_layouts.extend(held);

        // A window can map outside the fullscreen subtree. sway keeps that new tiling node
        // in the workspace with its untouched zero pending box until fullscreen ends. It has
        // no previous cache entry for us to carry above, but it still belongs to this branch:
        // omitting it makes `pending_layout_addresses_are_current` reject an otherwise valid
        // arrange of the fullscreen subtree because the snapshot appears to have lost a leaf.
        // Give every such new leaf the state sway exposes instead — present, hidden and with
        // a zero box — so snapshots remain complete without arranging outside fullscreen.
        let represented: HashSet<NodeKey> = data.leaf_layouts.iter().map(|info| info.key).collect();
        let zero = Rectangle::new(Point::from((0.0, 0.0)), Size::from((0.0, 0.0)));
        for key in self
            .dfs_leaf_keys()
            .into_iter()
            .filter(|key| self.branch_root(*key) == branch && !represented.contains(key))
        {
            let Some(path) = self.branch_relative_path(key) else {
                continue;
            };
            data.leaf_layouts.push(LeafLayoutInfo {
                key,
                branch,
                path,
                rect: zero,
                node_rect: zero,
                visible: false,
                workspace_fullscreen: false,
            });
        }
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
                let tab_offset = self.switcher_content_offset(layout, child_count, rect.size.h);

                let mut content_rect = rect;
                if tab_offset > 0.0 {
                    content_rect.loc.y += tab_offset;
                    content_rect.size.h = (content_rect.size.h - tab_offset).max(0.0);
                }
                (vec![content_rect; child_count], tab_offset)
            }
        }
    }

    /// Decoration inset a tabbed or stacked parent applies to each child's IPC/content box.
    ///
    /// The parent layout can change while workspace fullscreen prevents its children from
    /// being arranged. Sway then keeps each child's pending node box and derives its exposed
    /// rectangle using the new parent layout, so layout and IPC must share this calculation.
    pub(super) fn switcher_content_offset(
        &self,
        layout: Layout,
        child_count: usize,
        height: f64,
    ) -> f64 {
        let row_height = self.tab_bar_row_height();
        if row_height <= 0.0 {
            return 0.0;
        }
        let bar_height = match layout {
            Layout::Tabbed => row_height,
            Layout::Stacked => row_height * child_count as f64,
            Layout::SplitH | Layout::SplitV => return 0.0,
        };
        (bar_height + self.tab_bar_spacing()).min(height).max(0.0)
    }

    fn collect_layout_node(
        &self,
        node_key: NodeKey,
        rect: Rectangle<f64, Logical>,
        node_rect: Rectangle<f64, Logical>,
        visible: bool,
        ctx: LeafLayoutContext,
        walk: &mut LayoutWalk<'_>,
    ) {
        let (layout, child_count, focused_idx, percents) = match self.get_node(node_key) {
            Some(node) if node.is_view() => {
                let tile = node.as_tile().expect("a view holds a tile");
                let (offset, show_titlebar) = if tile.window().pending_sizing_mode().is_fullscreen()
                {
                    (0.0, false)
                } else {
                    (ctx.tab_bar_offset, ctx.draw_titlebar)
                };
                walk.data.tab_bar_offsets.insert(node_key, offset);
                walk.data.titlebar_flags.insert(node_key, show_titlebar);
                walk.data
                    .tabbed_context_flags
                    .insert(node_key, ctx.in_tabbed_context);
                walk.data.leaf_layouts.push(LeafLayoutInfo {
                    key: node_key,
                    branch: walk.branch,
                    path: walk.path.clone(),
                    rect,
                    node_rect,
                    visible,
                    workspace_fullscreen: false,
                });
                return;
            }
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                let container = self.get_container(node_key).expect("layout parent");
                walk.data.container_geometries.insert(node_key, rect);
                (
                    container.layout(),
                    container.child_count(),
                    self.active_child_index(node_key),
                    self.child_percents(node_key),
                )
            }
            None => return,
        };

        if child_count == 0 {
            return;
        }

        let (child_rects, _) =
            self.child_rects_for_layout(layout, rect, child_count, &percents, walk.gap);

        match layout {
            Layout::SplitH | Layout::SplitV => {
                let split_bar_height = self.split_title_bar_height();

                for (idx, &child_rect) in child_rects.iter().enumerate() {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };

                    walk.path.push(idx);
                    let (child_offset, child_titlebar) =
                        self.split_child_titlebar(child_key, split_bar_height);
                    let child_ctx = LeafLayoutContext {
                        tab_bar_offset: child_offset,
                        draw_titlebar: child_titlebar,
                        in_tabbed_context: ctx.in_tabbed_context,
                    };
                    self.collect_layout_node(
                        child_key, child_rect, child_rect, visible, child_ctx, walk,
                    );
                    walk.path.pop();
                }
            }
            Layout::Tabbed | Layout::Stacked => {
                let focused_idx = focused_idx.unwrap_or(0).min(child_count.saturating_sub(1));

                for (idx, &child_rect) in child_rects.iter().enumerate() {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };
                    walk.path.push(idx);
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
                        if self.get_node(child_key).is_some_and(|node| node.is_view()) {
                            rect
                        } else {
                            child_rect
                        };
                    self.collect_layout_node(
                        child_key,
                        child_rect,
                        child_node_rect,
                        child_visible,
                        child_ctx,
                        walk,
                    );
                    walk.path.pop();
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
            } else {
                tile.request_tile_size(size, animate_resize, tx);
            }

            tile.set_tab_bar_offset(old_offset);
            tile.set_draw_titlebar(old_titlebar);
            tile.set_in_tabbed_context(old_tabbed_context);
        }
    }

    /// Commit one branch's arrange.
    ///
    /// Only that branch's leaves are replaced: the others are either still on screen as they
    /// were, or waiting for their own windows, and neither is this pass's to overwrite. They
    /// are put back in a fixed order — the tiled side, then the floating groups in the order
    /// the workspace holds them — so that what reads the cache back in order (hit-testing,
    /// rendering) gets the same answer whichever branch was arranged last.
    pub(super) fn apply_layout_data(&mut self, branch: NodeKey, data: LayoutData) {
        for (key, rect) in data.container_geometries {
            if let Some(container) = self.get_container_mut(key) {
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
        // Out go this branch's old entries, and any entry for a leaf this pass is about to
        // describe: a leaf that crossed between branches is still one leaf, and its entry
        // under the branch it left would otherwise sit there beside its new one.
        let incoming: HashSet<NodeKey> = data.leaf_layouts.iter().map(|info| info.key).collect();
        // And out go the leaves the tree no longer holds. A whole-workspace replace used to
        // drop those by construction; keeping the other branches' entries means saying so.
        let gone: HashSet<NodeKey> = self
            .leaf_layouts
            .iter()
            .map(|info| info.key)
            .filter(|key| !self.holds_node(*key))
            .collect();
        self.leaf_layouts.retain(|info| {
            info.branch != branch && !incoming.contains(&info.key) && !gone.contains(&info.key)
        });
        self.leaf_layouts.extend(data.leaf_layouts);
        self.sort_leaf_layouts_by_branch();
    }

    /// The tiled side first, then the floating groups in the workspace's order.
    fn sort_leaf_layouts_by_branch(&mut self) {
        let order: HashMap<NodeKey, usize> = std::iter::once(self.root)
            .chain(self.floating_roots())
            .enumerate()
            .map(|(idx, key)| (key, idx))
            .collect();
        self.leaf_layouts
            .sort_by_key(|info| order.get(&info.branch).copied().unwrap_or(usize::MAX));
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
        self.get_container(container_key)?.child_key(idx)
    }

    pub(in crate::layout) fn get_normalized_child_percents(
        &self,
        container_key: NodeKey,
        child_count: usize,
    ) -> Vec<f64> {
        let Some(_) = self.get_container(container_key) else {
            return vec![1.0 / child_count.max(1) as f64; child_count];
        };
        super::resolved_percents(&self.child_percents(container_key), child_count)
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

        let is_leaf = self.get_node(child_key).is_some_and(|node| node.is_view());
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
