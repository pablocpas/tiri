use std::collections::{HashMap, HashSet};

use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{Blocker, BlockerState};

use super::{
    ContainerArena, Layout, LayoutElement, LeafLayoutInfo, LeafPresentation, NodeData, NodeKey,
};
use crate::layout::tab_bar::tab_bar_row_height;
use crate::layout::tile::Tile;
use crate::utils::transaction::{Transaction, TransactionBlocker};

#[derive(Debug)]
pub(in crate::layout) struct LayoutData {
    pub(in crate::layout) leaf_layouts: HashMap<NodeKey, LeafLayoutInfo>,
    /// The candidate pending box for every node this arrange visited.
    ///
    /// This remains in the transaction snapshot until commit. Once applied, the box is stored on
    /// the node itself for both views and split containers; `leaf_layouts` only carries the
    /// decorated presentation box.
    pub(super) node_geometries: HashMap<NodeKey, Rectangle<f64, Logical>>,
}

/// What an arrange pass carries from its root all the way down.
///
/// The two invariants — which branch is being arranged and the gap between its children —
/// and the two accumulators. They were nine loose parameters threaded through a recursion,
/// where the only ones that actually change on the way down are the node and its box.
struct LayoutWalk<'a> {
    gap: f64,
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

impl<W: LayoutElement> ContainerArena<W> {
    /// Calculate and apply layout to the tree.
    pub(in crate::layout) fn layout(&mut self, workspace: NodeKey) {
        self.layout_with_resize_animation(workspace, true);
    }

    /// Calculate and apply layout to the tree, with control over resize animation.
    pub(in crate::layout) fn layout_with_resize_animation(
        &mut self,
        workspace: NodeKey,
        animate_resize: bool,
    ) {
        let animate = !self.options(workspace).animations.off;
        self.layout_with_animations(workspace, animate, animate_resize);
    }

    /// Calculate and apply layout to the tree with explicit animation flags.
    pub(in crate::layout) fn layout_with_animation_flags(
        &mut self,
        workspace: NodeKey,
        animate: bool,
        animate_resize: bool,
    ) {
        self.layout_with_animations(workspace, animate, animate_resize);
    }

    fn layout_with_animations(&mut self, workspace: NodeKey, animate: bool, animate_resize: bool) {
        self.generation = self.generation.wrapping_add(1);
        let _ = animate;
        self.layout_atomic(workspace, animate_resize, None);
    }

    /// sway's `arrange_container`: arrange one real container against the pending box it is
    /// currently holding, without applying the workspace fullscreen branch first.
    pub(in crate::layout) fn layout_container_subtree(&mut self, workspace: NodeKey, key: NodeKey) {
        let Some(area) = self.node_geometry(key) else {
            return;
        };
        self.layout_container_subtree_in(workspace, key, area);
    }

    /// sway's `arrange_container` after its caller has just edited the pending box.
    ///
    /// A direct floating resize can do this while the same node owns workspace fullscreen.
    /// The edited box, not a fresh fullscreen workspace arrange, is then observable in IPC.
    pub(in crate::layout) fn layout_container_subtree_in(
        &mut self,
        workspace: NodeKey,
        key: NodeKey,
        area: Rectangle<f64, Logical>,
    ) {
        if self.get_node(key).is_none() || self.is_workspace(key) {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.layout_atomic(workspace, true, Some((key, area)));
    }

    pub(in crate::layout) fn layout_area(&self, workspace: NodeKey) -> Rectangle<f64, Logical> {
        let mut area = self.working_area(workspace);
        let gap = self.options(workspace).layout.gaps;
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
    pub(in crate::layout) fn pending_layout_is_stale(&self, workspace: NodeKey) -> bool {
        if self.pending_layouts.is_empty() {
            return false;
        }

        // Only leaves reachable from a root count: the store can still hold nodes that were
        // detached but not yet dropped.
        // A branch is stale when the leaves it describes are no longer the leaves that branch
        // has. The other branches' leaves are in `current` too and are none of its business —
        // and so are the branches of every other workspace, which is why this names one.
        self.pending_layouts
            .iter()
            .filter(|pending| self.workspace_of(pending.branch) == Some(workspace))
            .any(|pending| {
                let snapshot: HashSet<NodeKey> =
                    pending.data.leaf_layouts.keys().copied().collect();
                let now: HashSet<NodeKey> = self
                    .dfs_leaf_keys()
                    .into_iter()
                    .filter(|key| self.branch_root(*key) == pending.branch)
                    .collect();
                snapshot != now
            })
    }

    /// The one node currently owning workspace fullscreen.
    ///
    /// This is sway's `workspace->fullscreen`: it spans both the tiled tree and the floating
    /// roots. The window id and its side are projections of this key, never parallel state.
    pub(in crate::layout) fn fullscreen_key(&self, workspace: NodeKey) -> Option<NodeKey> {
        self.fullscreen_keys.get(&workspace).copied()
    }

    /// Window that directly owns fullscreen at the client protocol boundary.
    ///
    /// A fullscreen container has no such window: its descendants remain ordinary tiled
    /// clients while the container's subtree occupies the output.
    pub(in crate::layout) fn fullscreen_leaf_window_id(
        &self,
        workspace: NodeKey,
    ) -> Option<&W::Id> {
        self.fullscreen_key(workspace)
            .and_then(|key| self.get_tile(key))
            .map(|tile| tile.window().id())
    }

    pub(in crate::layout) fn window_owns_fullscreen(&self, workspace: NodeKey, id: &W::Id) -> bool {
        self.fullscreen_leaf_window_id(workspace)
            .is_some_and(|current| current == id)
    }

    /// A window representative for APIs that cannot name a container.
    ///
    /// sway's workspace pointer can name a container. Existing tiri inspection APIs expose
    /// window IDs, so descend through the seat's inactive-focus order rather than inventing a
    /// second fullscreen owner.
    pub(in crate::layout) fn fullscreen_representative_window_id(
        &self,
        workspace: NodeKey,
    ) -> Option<&W::Id> {
        let key = self.fullscreen_key(workspace)?;
        let leaf = self.focus_inactive_view(key)?;
        self.get_tile(leaf).map(|tile| tile.window().id())
    }

    /// Point `workspace->fullscreen` at a live node, or at nothing.
    ///
    /// Setting it does not arrange: callers still have side-specific window state to request
    /// before arranging. The workspace node is not a fullscreen target; every other live
    /// node can be one.
    pub(in crate::layout) fn set_fullscreen_key(
        &mut self,
        workspace: NodeKey,
        key: Option<NodeKey>,
    ) -> bool {
        if key.is_some_and(|key| self.is_workspace(key) || !self.holds_node(key)) {
            return false;
        }
        if self.fullscreen_key(workspace) == key {
            return false;
        }
        if let Some(old) = self.fullscreen_key(workspace) {
            self.set_node_fullscreen_restore_geometry(old, None);
        }
        if let Some(key) = key {
            let restore = self.node_geometry(key).unwrap_or_default();
            self.set_node_fullscreen_restore_geometry(key, Some(restore));
        }
        if let Some(key) = key {
            self.fullscreen_keys.insert(workspace, key);
        } else {
            self.fullscreen_keys.remove(&workspace);
        }
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
        let workspace = self
            .workspace_of(old_key)
            .or_else(|| self.workspace_of(new_key))
            .expect("fullscreen replacement belongs to a workspace");
        if self.fullscreen_key(workspace) != Some(old_key) {
            return;
        }
        let animate = !self.options(workspace).animations.off;
        let working_area_size = self.working_area(workspace).size;
        let replacement_restore = self.node_geometry(new_key).unwrap_or_default();
        if let Some(tile) = self.get_tile_mut(old_key) {
            tile.request_tile_size(working_area_size, animate, None);
        }

        if let Some(tile) = self.get_tile_mut(new_key) {
            tile.request_fullscreen(animate, None);
        }

        self.set_node_fullscreen_restore_geometry(old_key, None);
        self.set_node_fullscreen_restore_geometry(new_key, Some(replacement_restore));
        self.fullscreen_keys.insert(workspace, new_key);
    }

    fn layout_atomic(
        &mut self,
        workspace: NodeKey,
        animate_resize: bool,
        subtree: Option<(NodeKey, Rectangle<f64, Logical>)>,
    ) {
        self.apply_pending_layouts_if_ready();
        // This workspace's own branches, not the arena's. A configure outstanding on the
        // workspace next door says nothing about whether this one may arrange, and noting a
        // deferred relayout it does not need leaves it re-arranging every frame until the
        // neighbour's client answers.
        if self.has_pending_layouts(workspace) {
            // A branch still waiting is not re-arranged: this call only records that another
            // layout is wanted. It is not one of sway's `arrange_workspace` passes, so it must
            // not resolve fractions yet. Doing that here normalized the same list again while
            // a client configure was outstanding; a later sibling swap then rounded the
            // opposite half-pixel and made that pixel survive the next resize and close.
            //
            // sway/tree/arrange.c:15-55,100-140
            self.pending_relayout.insert(workspace);
            self.debug_layout_state(workspace, "layout_atomic_pending");
        } else {
            self.pending_relayout.remove(&workspace);
        }

        // This workspace, not the arena: arranging an empty workspace used to clear the caches
        // of every other one, and the window on the workspace next door lost the box it had
        // just been given.
        if self.first_leaf_key(workspace).is_none()
            && self.floating_roots(workspace).next().is_none()
        {
            let leaving: HashSet<NodeKey> = self
                .leaf_layouts
                .keys()
                .filter(|key| self.workspace_of(**key) == Some(workspace))
                .copied()
                .collect();
            self.leaf_layouts.retain(|key, _| !leaving.contains(key));
            let superseded: Vec<NodeKey> = self
                .pending_layouts
                .iter()
                .filter(|pending| self.workspace_of(pending.branch) == Some(workspace))
                .map(|pending| pending.branch)
                .collect();
            self.pending_layouts
                .retain(|pending| !superseded.contains(&pending.branch));
            self.pending_relayout.remove(&workspace);
            self.debug_layout_state(workspace, "layout_atomic_empty");
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
                std::iter::once(workspace)
                    .chain(self.floating_roots(workspace).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            },
            |(key, _)| vec![key],
        );
        for key in resolve_roots {
            if !waiting.contains(&self.branch_root(key)) {
                self.resolve_percents(key);
            }
        }

        let mut requested = false;
        let arrangements = match subtree {
            Some((key, area)) => vec![(
                self.branch_root(key),
                self.collect_branch_layout_data(workspace, key, area),
            )],
            None => self.arrange_each_branch(workspace),
        };
        for (branch, data) in arrangements {
            if waiting.contains(&branch) {
                // Still waiting on its own windows. Its neighbours are not.
                self.pending_relayout.insert(workspace);
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
            let transaction = if branch == workspace {
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

        if requested {
            self.debug_layout_state(workspace, "layout_atomic_requested");
        } else {
            self.debug_layout_state(workspace, "layout_atomic_apply");
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
    fn arrange_each_branch(&mut self, workspace: NodeKey) -> Vec<(NodeKey, LayoutData)> {
        if let Some(fullscreen_key) = self
            .fullscreen_key(workspace)
            .filter(|key| self.nodes.contains_key(*key))
        {
            let branch = self.branch_root(fullscreen_key);
            return vec![(
                branch,
                self.collect_fullscreen_layout_data(workspace, fullscreen_key),
            )];
        }

        let mut out = vec![(workspace, self.collect_layout_data(workspace))];
        for (root, area) in self.floating_roots_with_areas(workspace) {
            out.push((root, self.collect_branch_layout_data(workspace, root, area)));
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
            .node_geometries
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

    /// One workspace's floating groups with the boxes they are laid out in.
    fn floating_roots_with_areas(
        &self,
        workspace: NodeKey,
    ) -> Vec<(NodeKey, Rectangle<f64, Logical>)> {
        self.floating_roots_snapshot(workspace)
    }

    /// Keep a leaf's node box when tree surgery changes how its parent decorates it.
    ///
    /// A leaf directly under tabs is the awkward representation boundary in tiri: the
    /// cached rectangle is its content box, while [`ContainerArena::node_geometry`] answers
    /// with the whole pending box sway keeps on the container. If `cmd_layout` flattens that
    /// parent and then finds that the surviving layout already has the requested value, it
    /// does not arrange. sway's box nevertheless stays whole; only the tab decoration that
    /// IPC derives from the old parent disappears. Preserve that box in both snapshots so a
    /// later transaction cannot put the removed decoration back.
    ///
    /// sway/commands/layout.c:134-196
    /// sway/tree/container.c:1534-1554
    pub(super) fn preserve_leaf_node_geometry(&mut self, key: NodeKey) {
        let preserve = |layouts: &mut HashMap<NodeKey, LeafLayoutInfo>, geometry| {
            if let Some(info) = layouts.get_mut(&key) {
                info.rect = geometry;
            }
        };
        if let Some(geometry) = self.node_geometry(key) {
            preserve(&mut self.leaf_layouts, geometry);
        }
        for pending in &mut self.pending_layouts {
            if let Some(geometry) = pending.data.node_geometries.get(&key).copied() {
                preserve(&mut pending.data.leaf_layouts, geometry);
            }
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
        let refresh = |layouts: &mut HashMap<NodeKey, LeafLayoutInfo>| {
            for (key, info) in layouts {
                if let Some(visible) = visibility.get(key) {
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
        let Some(workspace) = self.workspace_of(key) else {
            return false;
        };
        if self
            .fullscreen_key(workspace)
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

    /// Whether a deferred arrange still describes the same live branch leaves.
    ///
    /// Geometry may intentionally stay old until the transaction completes. Stable node
    /// identities make structural addresses unnecessary; a leaf moving branch or disappearing
    /// is enough to invalidate the snapshot and request a fresh layout.
    fn pending_layout_is_current(&self, pending: &PendingLayout) -> bool {
        let snapshot: HashSet<NodeKey> = pending.data.leaf_layouts.keys().copied().collect();
        let current: HashSet<NodeKey> = self
            .dfs_leaf_keys()
            .into_iter()
            .filter(|key| self.branch_root(*key) == pending.branch)
            .collect();
        snapshot == current
    }

    /// Commit whatever is ready, then run the arrange `workspace` deferred while it waited.
    ///
    /// The two halves answer different questions and only one of them is about this
    /// workspace. Committing is the arena's: it walks every branch whose windows have
    /// answered, wherever they live. Whether this workspace still owes itself an arrange is
    /// its own, and reading it off the commit pass's "something happened" is what stranded
    /// it: the frame visits every workspace in turn, the first one to call this consumed the
    /// answer, and the workspace the deferred arrange belonged to was told nothing had
    /// happened and kept its old boxes until the next command arranged it by hand.
    pub(in crate::layout) fn apply_pending_layouts_and_deferred_relayout(
        &mut self,
        workspace: NodeKey,
    ) {
        self.apply_pending_layouts_if_ready();
        // Nothing outstanding here any more, so a deferred arrange can finally run. Asking
        // per workspace also means a neighbour still waiting cannot hold this one back.
        if !self.has_pending_layouts(workspace) && self.take_pending_relayout(workspace) {
            self.layout(workspace);
        }
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
            if !self.pending_layout_is_current(&pending) {
                if let Some(workspace) = self.workspace_of(pending.branch) {
                    self.pending_relayout.insert(workspace);
                }
                continue;
            }
            self.apply_layout_data(pending.branch, pending.data);
        }
        if let Some(workspace) = self.workspaces.first().copied() {
            self.debug_layout_state(workspace, "layout_atomic_apply_pending");
        }
        true
    }

    /// Whether one workspace is still waiting on windows it asked to resize.
    pub(in crate::layout) fn has_pending_layouts(&self, workspace: NodeKey) -> bool {
        self.pending_layouts
            .iter()
            .any(|pending| self.workspace_of(pending.branch) == Some(workspace))
    }

    /// Drop a size transaction superseded by a branch transfer.
    ///
    /// Floating a node immediately issues its restored size, so a configure for its old tiled
    /// box cannot govern the new branch. Maximizing a floating node is the same kind of
    /// superseding transition in the other direction: `container_set_floating` moves it first
    /// (`sway/tree/container.c:1004`) and the maximized arrange decides its next size. An ordinary
    /// floating-to-tiling move still waits for its outstanding configure.
    pub(in crate::layout) fn discard_layout_superseded_by_transfer(&mut self, workspace: NodeKey) {
        let superseded: Vec<NodeKey> = self
            .pending_layouts
            .iter()
            .filter(|pending| self.workspace_of(pending.branch) == Some(workspace))
            .map(|pending| pending.branch)
            .collect();
        self.pending_layouts
            .retain(|pending| !superseded.contains(&pending.branch));
        self.pending_transaction = None;
        self.pending_relayout.remove(&workspace);
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
                size: self
                    .workspace_of(
                        self.window_key(tile.window().id())
                            .expect("tile is in arena"),
                    )
                    .map(|workspace| self.view_size(workspace))
                    .unwrap_or_default()
                    .to_i32_round(),
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
            leaf_layouts: HashMap::new(),
            node_geometries: HashMap::new(),
        };

        let area = self.layout_area(root_key);
        let gap = self.gap_in(root_key);
        let mut walk = LayoutWalk {
            gap,
            data: &mut data,
        };
        self.collect_layout_node(
            root_key,
            area,
            area,
            true,
            LeafPresentation::default(),
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
        if self.is_in_floating_branch(key) {
            0.0
        } else {
            let workspace = self
                .workspace_of(key)
                .expect("a tiled node belongs to a workspace");
            self.options(workspace).layout.gaps
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
    fn collect_fullscreen_layout_data(
        &self,
        workspace: NodeKey,
        fullscreen_key: NodeKey,
    ) -> LayoutData {
        let area = if self.get_container(fullscreen_key).is_some() {
            Rectangle::from_size(self.view_size(workspace))
        } else {
            self.layout_area(workspace)
        };
        let mut data = self.collect_branch_layout_data(workspace, fullscreen_key, area);
        if self.get_tile(fullscreen_key).is_some() {
            if let Some(info) = data.leaf_layouts.get_mut(&fullscreen_key) {
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
        workspace: NodeKey,
        branch_root: NodeKey,
        area: Rectangle<f64, Logical>,
    ) -> LayoutData {
        let mut data = LayoutData {
            leaf_layouts: HashMap::new(),
            node_geometries: HashMap::new(),
        };

        // The transaction belongs to the node's actual branch, not necessarily to the tiled
        // workspace. This matters when fullscreen names a descendant of a floating group: the
        // subtree keeps its NodeKey authority but is arranged against the output.
        if !self.holds_node(branch_root) {
            return self.collect_layout_data(workspace);
        }
        let mut walk = LayoutWalk {
            gap: self.gap_in(branch_root),
            data: &mut data,
        };
        self.collect_layout_node(
            branch_root,
            area,
            area,
            true,
            LeafPresentation::default(),
            &mut walk,
        );

        // A pass describes its own branch and no other: what it does not mention is committed
        // separately, by whoever arranged it. Inside this branch, though, silence would be
        // taken for absence — so leaves this pass did not revisit keep their rectangle, which
        // is the box sway is not revisiting. Identity comes from the map key and topology from
        // the live tree; a leaf that is gone is dropped.
        let branch = self.branch_root(branch_root);
        let arranged: HashSet<NodeKey> = data.leaf_layouts.keys().copied().collect();
        let held: Vec<(LeafLayoutInfo, Rectangle<f64, Logical>)> = self
            .leaf_layouts
            .values()
            .filter(|info| !arranged.contains(&info.key))
            .filter(|info| self.branch_root(info.key) == branch)
            .filter_map(|info| Some((info.clone(), self.node_geometry(info.key)?)))
            .collect();
        for (info, geometry) in held {
            data.node_geometries.insert(info.key, geometry);
            data.leaf_layouts.insert(info.key, info);
        }

        // A window can map outside the fullscreen subtree. sway keeps that new tiling node
        // in the workspace with its untouched zero pending box until fullscreen ends. It has
        // no previous cache entry for us to carry above, but it still belongs to this branch:
        // omitting it makes `pending_layout_is_current` reject an otherwise valid
        // arrange of the fullscreen subtree because the snapshot appears to have lost a leaf.
        // Give every such new leaf the state sway exposes instead — present, hidden and with
        // a zero box — so snapshots remain complete without arranging outside fullscreen.
        let represented: HashSet<NodeKey> = data.leaf_layouts.keys().copied().collect();
        let zero = Rectangle::new(Point::from((0.0, 0.0)), Size::from((0.0, 0.0)));
        for key in self
            .dfs_leaf_keys()
            .into_iter()
            .filter(|key| self.branch_root(*key) == branch && !represented.contains(key))
        {
            data.leaf_layouts.insert(
                key,
                LeafLayoutInfo {
                    key,
                    rect: zero,
                    presentation: LeafPresentation::default(),
                    visible: false,
                    workspace_fullscreen: false,
                },
            );
            data.node_geometries.insert(key, zero);
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
        workspace: NodeKey,
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
                let lengths =
                    self.distribute_split_lengths(workspace, available, child_count, percents);

                let mut cursor = if horizontal { rect.loc.x } else { rect.loc.y };
                let end = if horizontal {
                    rect.loc.x + rect.size.w
                } else {
                    rect.loc.y + rect.size.h
                };
                let mut rects = Vec::with_capacity(child_count);
                for idx in 0..child_count {
                    // Sway computes the last child's size from the cursor after every earlier
                    // minimum-size collapse, not from the nominal fraction remainder. An
                    // earlier child narrower/shorter than 10 px becomes 0x0 and therefore
                    // leaves its unused span to that last child.
                    let length = if idx + 1 == child_count {
                        (end - cursor).max(0.0)
                    } else {
                        *lengths.get(idx).unwrap_or(&0.0)
                    };
                    let mut child_rect = if horizontal {
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
                    // `apply_horiz_layout` and `apply_vert_layout` clear both dimensions when
                    // either one falls below their hard 10 px layout floor. Keeping the cross
                    // axis here produced a 0x720 pending node where Sway reports 0x0.
                    //
                    // sway/tree/arrange.c:75-96,160-181
                    if child_rect.size.w < 10.0 || child_rect.size.h < 10.0 {
                        child_rect.size = Size::from((0.0, 0.0));
                    }
                    rects.push(child_rect);
                    cursor += if horizontal {
                        child_rect.size.w
                    } else {
                        child_rect.size.h
                    } + gap;
                }
                (rects, 0.0)
            }
            Layout::Tabbed | Layout::Stacked => {
                let tab_offset =
                    self.switcher_content_offset(workspace, layout, child_count, rect.size.h);

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
        workspace: NodeKey,
        layout: Layout,
        child_count: usize,
        height: f64,
    ) -> f64 {
        let row_height = self.tab_bar_row_height(workspace);
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
        ctx: LeafPresentation,
        walk: &mut LayoutWalk<'_>,
    ) {
        let workspace = self
            .workspace_of(node_key)
            .expect("an arranged node belongs to a workspace");
        let (layout, child_count, focused_idx, percents) = match self.get_node(node_key) {
            Some(node) if node.is_view() => {
                let tile = node.as_tile().expect("a view holds a tile");
                let (offset, show_titlebar) = if tile.window().pending_sizing_mode().is_fullscreen()
                {
                    (0.0, false)
                } else {
                    (ctx.tab_bar_offset, ctx.draw_titlebar)
                };
                walk.data.node_geometries.insert(node_key, node_rect);
                walk.data.leaf_layouts.insert(
                    node_key,
                    LeafLayoutInfo {
                        key: node_key,
                        rect,
                        presentation: LeafPresentation::new(
                            offset,
                            show_titlebar,
                            ctx.in_tabbed_context,
                        ),
                        visible,
                        workspace_fullscreen: false,
                    },
                );
                return;
            }
            Some(NodeData::Workspace(_)) | Some(NodeData::Container(_)) => {
                let container = self.get_container(node_key).expect("layout parent");
                walk.data.node_geometries.insert(node_key, rect);
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
            self.child_rects_for_layout(workspace, layout, rect, child_count, &percents, walk.gap);

        match layout {
            Layout::SplitH | Layout::SplitV => {
                let split_bar_height = self.split_title_bar_height(workspace);

                for (idx, &child_rect) in child_rects.iter().enumerate() {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };

                    let (child_offset, child_titlebar) =
                        self.split_child_titlebar(child_key, split_bar_height);
                    let child_ctx =
                        LeafPresentation::new(child_offset, child_titlebar, ctx.in_tabbed_context);
                    self.collect_layout_node(
                        child_key, child_rect, child_rect, visible, child_ctx, walk,
                    );
                }
            }
            Layout::Tabbed | Layout::Stacked => {
                let focused_idx = focused_idx.unwrap_or(0).min(child_count.saturating_sub(1));

                for (idx, &child_rect) in child_rects.iter().enumerate() {
                    let Some(child_key) = self.get_container_child_at(node_key, idx) else {
                        continue;
                    };
                    let child_visible = visible && idx == focused_idx;
                    let child_ctx = LeafPresentation::new(0.0, false, true);
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
                }
            }
        }
    }

    fn changed_layout_keys(&self, data: &LayoutData) -> HashSet<NodeKey> {
        let mut current = HashMap::new();
        for info in self.leaf_layouts.values() {
            let Some(tile) = self.get_tile(info.key) else {
                continue;
            };
            let request = self.layout_request_for(tile, info.rect.size, tile.tab_bar_offset());
            current.insert(info.key, request);
        }

        let mut changed = HashSet::new();
        for info in data.leaf_layouts.values() {
            let Some(tile) = self.get_tile(info.key) else {
                changed.insert(info.key);
                continue;
            };
            let request =
                self.layout_request_for(tile, info.rect.size, info.presentation.tab_bar_offset);
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
        for key in self.dfs_leaf_keys() {
            let Some(info) = data.leaf_layouts.get(&key) else {
                continue;
            };
            let Some(tile) = self.get_tile_mut(key) else {
                continue;
            };
            let tx = changed.contains(&key).then(|| transaction.clone());
            let size = Size::from((info.rect.size.w, info.rect.size.h));
            if tile.window().pending_sizing_mode().is_fullscreen() {
                tile.request_fullscreen(animate_resize, tx);
            } else {
                tile.request_tile_size_with_tab_bar_offset(
                    size,
                    info.presentation.tab_bar_offset,
                    animate_resize,
                    tx,
                );
            }
        }
    }

    /// Commit one branch's arrange.
    ///
    /// Only that branch's leaves are replaced: the others are either still on screen as they
    /// were, or waiting for their own windows, and neither is this pass's to overwrite. Render
    /// order comes from the current tree rather than insertion order in this identity map.
    pub(super) fn apply_layout_data(&mut self, branch: NodeKey, data: LayoutData) {
        let ordered_leaves = self.dfs_leaf_keys();
        for (key, rect) in data.node_geometries {
            self.set_node_geometry(key, rect);
        }
        for key in ordered_leaves {
            let Some(info) = data.leaf_layouts.get(&key) else {
                continue;
            };
            if let Some(tile) = self.get_tile_mut(key) {
                tile.set_tab_bar_offset(info.presentation.tab_bar_offset);
                tile.set_draw_titlebar(info.presentation.draw_titlebar);
                tile.set_in_tabbed_context(info.presentation.in_tabbed_context);
            }
        }
        // Out go this branch's old entries, and any entry for a leaf this pass is about to
        // describe: a leaf that crossed between branches is still one leaf, and its entry
        // under the branch it left would otherwise sit there beside its new one.
        let incoming: HashSet<NodeKey> = data.leaf_layouts.keys().copied().collect();
        // Out go old entries still belonging to this branch, plus leaves the tree no longer
        // holds. A leaf that crossed branches is retained until its new branch commits.
        let stale: Vec<NodeKey> = self
            .leaf_layouts
            .keys()
            .copied()
            .filter(|key| {
                !self.holds_node(*key)
                    || (self.branch_root(*key) == branch && !incoming.contains(key))
            })
            .collect();
        for key in stale {
            self.leaf_layouts.remove(&key);
        }
        self.leaf_layouts.extend(data.leaf_layouts);
    }

    pub(in crate::layout) fn tab_bar_row_height(&self, workspace: NodeKey) -> f64 {
        if self.options(workspace).layout.tab_bar.off {
            return 0.0;
        }
        tab_bar_row_height(
            &self.options(workspace).layout.tab_bar,
            self.scale(workspace),
        )
    }

    pub(in crate::layout) fn split_title_bar_height(&self, workspace: NodeKey) -> f64 {
        if !self.options(workspace).layout.tab_bar.show_in_split {
            return 0.0;
        }
        self.tab_bar_row_height(workspace)
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
        workspace: NodeKey,
        available: f64,
        child_count: usize,
        percents: &[f64],
    ) -> Vec<f64> {
        if child_count == 0 {
            return Vec::new();
        }

        let available = available.max(0.0);
        let available_phys = (available * self.scale(workspace)).round().max(0.0) as i32;
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
            .map(|v| v as f64 / self.scale(workspace))
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
        workspace: NodeKey,
        layout: Layout,
        rect: Rectangle<f64, Logical>,
        tab_count: usize,
    ) -> Option<(Rectangle<f64, Logical>, f64)> {
        if tab_count == 0 {
            return None;
        }

        let row_height = self.tab_bar_row_height(workspace);
        if row_height <= 0.0 {
            return None;
        }

        // The bar reaches down over the top border lane of the tile below it.
        //
        // i3's normal border style is a title bar on top and a border on the other three
        // sides: the tab *is* the top decoration, and nothing is drawn under it. The
        // children here are laid out below `switcher_content_offset`, which leaves their
        // border lane sitting between the bar and the window — two stacked decorations
        // where i3 has one. Growing the bar by that width covers the lane, and
        // `edge_visibility_for_tile` stops drawing it. Layout is untouched: only what is
        // painted (and what a click on the bar hits) changes.
        let border_ext = if self.options(workspace).layout.border.off {
            0.0
        } else {
            self.options(workspace).layout.border.width
        };

        let spacing = self.tab_bar_spacing();
        let base_height = match layout {
            Layout::Tabbed => row_height,
            Layout::Stacked => row_height * tab_count as f64,
            _ => 0.0,
        };
        let bar_height = (base_height + border_ext + spacing)
            .min(rect.size.h)
            .max(0.0);
        if bar_height <= 0.0 {
            return None;
        }

        let bar_rect = Rectangle::new(rect.loc, Size::from((rect.size.w, bar_height)));
        Some((bar_rect, row_height))
    }
}
