//! i3-style hierarchical tiling layout
//!
//! This module implements an i3-style tiling window manager with hierarchical containers.
//! Windows are organized in a tree structure where:
//! - Internal nodes are containers with a layout mode (SplitH, SplitV, Tabbed, Stacked)
//! - Leaf nodes contain individual windows wrapped in Tiles
//! - Navigation and movement follow the tree hierarchy
//!
//! The implementation uses SlotMap for efficient O(1) node access and safe reference handling.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};
use tiri_config::utils::MergeWith as _;
use tiri_config::{Border, HideEdgeBorders, PresetSize, TabBar};
use tiri_ipc::{ColumnDisplay, LayoutTreeNode, SizeChange};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::container::{
    ContainerTree, DetachedContainer, DetachedNode, Direction, InsertParentInfo, Layout,
    LeafLayoutInfo, RootPolicy,
};
use super::focus_ring::{
    render_container_selection, ContainerSelectionStyle, FocusRingEdges, FocusRingIndicatorEdge,
    FocusRingRenderElement,
};
use super::legacy_column::{Column, ColumnWidth};
use super::monitor::{InsertPosition, SplitIndicator};
use super::tile::{Tile, TileRenderElement};
use super::tile::{TilePtrIter, TilePtrIterMut};
use super::viewport::FixedViewport;
use super::{
    ConfigureIntent, InteractiveResizeData, LayoutElement, Options, RemovedTile, ResizeHit,
};
use crate::animation::{Animation, Clock};
use crate::layout::tab_bar::{
    render_tab_bar, tab_bar_state_from_info, TabBarCacheEntry, TabBarRenderOutput,
};
use crate::niri_render_elements;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenRenderElement};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::transaction::Transaction;
use crate::utils::ResizeEdge;
use crate::utils::{round_logical_in_physical_max1, to_physical_precise_round};
use crate::window::ResolvedWindowRules;
use log::warn;

// ============================================================================
// MAIN STRUCTURES - i3-style container tree implementation
// ============================================================================

/// i3-style tiling space using hierarchical containers
#[derive(Debug)]
pub struct TilingSpace<W: LayoutElement> {
    /// Container tree managing window layout
    tree: ContainerTree<W>,
    /// Workspace-level layout state (sway workspace->layout equivalent).
    workspace_layout: Layout,
    /// Previous workspace split layout (sway workspace->prev_split_layout equivalent).
    workspace_prev_split_layout: Option<Layout>,
    /// View size (output size)
    view_size: Size<f64, Logical>,
    /// Working area (view_size minus gaps/bars)
    working_area: Rectangle<f64, Logical>,
    /// Viewport behavior. Fixed for i3/sway tiling; kept as a component to isolate niri merge
    /// points that still talk about viewport gestures.
    viewport: FixedViewport,
    /// Display scale
    scale: f64,
    /// Animation clock
    clock: Clock,
    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResizeState<W>>,
    /// Layout options
    options: Rc<Options>,
    /// Cached tab bar textures keyed by container path.
    tab_bar_cache: RefCell<HashMap<Vec<usize>, TabBarCacheEntry>>,
    /// Alternate tab bar cache for swap (avoids allocation).
    tab_bar_cache_alt: RefCell<HashMap<Vec<usize>, TabBarCacheEntry>>,
    /// Whether this workspace is active (for tab bar styling).
    is_active: bool,
    /// Currently fullscreen window (if any)
    fullscreen_window: Option<W::Id>,
    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,
    /// Cached offscreen texture for overview rendering.
    overview_offscreen: OffscreenBuffer,
    /// Stable workspace-sized background used under the overview offscreen.
    overview_background: SolidColorBuffer,
}

#[derive(Debug, Clone)]
struct ResizeTarget {
    parent_path: Vec<usize>,
    child_idx: usize,
    neighbor_idx: usize,
    original_span: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceLayoutTargetKind {
    RootLeaf,
    SyntheticRootContainer,
    SelectedRootContainer,
    SelectedContainer,
    FocusedContainer,
}

#[derive(Debug, Clone)]
struct InteractiveResizeState<W: LayoutElement> {
    window: W::Id,
    data: InteractiveResizeData,
    horizontal: Option<ResizeTarget>,
    vertical: Option<ResizeTarget>,
}

fn resize_edge_for_path_at_target(
    path: &[usize],
    target: &ResizeTarget,
    near_edge: ResizeEdge,
    far_edge: ResizeEdge,
) -> Option<ResizeEdge> {
    if !path.starts_with(&target.parent_path) {
        return None;
    }

    let next_idx = target.parent_path.len();
    if path.len() <= next_idx {
        return None;
    }

    let child_idx = path[next_idx];
    if child_idx == target.child_idx {
        return Some(if target.neighbor_idx > target.child_idx {
            far_edge
        } else {
            near_edge
        });
    }

    if child_idx == target.neighbor_idx {
        return Some(if target.neighbor_idx > target.child_idx {
            near_edge
        } else {
            far_edge
        });
    }

    None
}

niri_render_elements! {
    TilingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TabBar = PrimaryGpuTextureRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
        ContainerSelection = FocusRingRenderElement,
        SolidColor = SolidColorRenderElement,
        Offscreen = OffscreenRenderElement,
    }
}

/// Detached top-level tiling subtree.
///
/// This is the internal unit moved across workspaces and monitors in the i3-style tree.
#[derive(Debug)]
pub struct RootTilingSubtree<W: LayoutElement> {
    /// Detached subtree that preserves container structure.
    subtree: DetachedNode<W>,
}

/// Window height specification for tiling layout
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    Auto,
    Fixed(i32),
}

struct TileRenderPositions<'a, W: LayoutElement> {
    entries: Vec<(*const Tile<W>, Point<f64, Logical>, bool)>,
    index: usize,
    _marker: PhantomData<&'a Tile<W>>,
}

impl<'a, W: LayoutElement> TileRenderPositions<'a, W> {
    fn new(space: &'a TilingSpace<W>) -> Self {
        let scale = Scale::from(space.scale);
        let mut entries = Vec::new();
        let layouts = space.display_layouts();
        for info in layouts {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = space.tree.get_tile(info.key) {
                let mut pos = info.rect.loc + tile.render_offset();
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
                entries.push((tile as *const _, pos, info.visible));
            }
        }

        Self {
            entries,
            index: 0,
            _marker: PhantomData,
        }
    }
}

impl<'a, W: LayoutElement> Iterator for TileRenderPositions<'a, W> {
    type Item = (&'a Tile<W>, Point<f64, Logical>, bool);

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.entries.len() {
            return None;
        }

        let (ptr, pos, visible) = self.entries[self.index];
        self.index += 1;

        unsafe { ptr.as_ref().map(|tile| (tile, pos, visible)) }
    }
}

struct TileRenderPositionsMut<'a, W: LayoutElement> {
    space: *mut TilingSpace<W>,
    layouts: Vec<LeafLayoutInfo>,
    index: usize,
    round: bool,
    scale: Scale<f64>,
    _marker: PhantomData<&'a mut TilingSpace<W>>,
}

impl<'a, W: LayoutElement> TileRenderPositionsMut<'a, W> {
    fn new(space: &'a mut TilingSpace<W>, round: bool) -> Self {
        // Clone layouts here because we need mutable access to space later.
        // The layouts are small (just NodeKey + rect per tile).
        let layouts = space.display_layouts().to_vec();
        Self {
            space: space as *mut _,
            layouts,
            index: 0,
            round,
            scale: Scale::from(space.scale),
            _marker: PhantomData,
        }
    }
}

impl<'a, W: LayoutElement> Iterator for TileRenderPositionsMut<'a, W> {
    type Item = (&'a mut Tile<W>, Point<f64, Logical>);

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.layouts.len() {
            let info = self.layouts[self.index].clone();
            self.index += 1;

            unsafe {
                let space = &mut *self.space;
                // Use O(1) key lookup instead of O(depth) path lookup.
                if let Some(tile) = space.tree.get_tile_mut(info.key) {
                    let mut pos = info.rect.loc + tile.render_offset();
                    if self.round {
                        pos = pos
                            .to_physical_precise_round(self.scale)
                            .to_logical(self.scale);
                    }
                    return Some((tile, pos));
                }
            }
        }

        None
    }
}

// ============================================================================
// TilingSpace Implementation
// ============================================================================

impl<W: LayoutElement> TilingSpace<W> {
    fn render_fullscreen_window(&self) -> Option<W::Id> {
        let id = self.fullscreen_window.as_ref()?;
        let path = self.tree.find_window(id)?;
        let tile = self.tree.tile_at_path(&path)?;
        tile.window()
            .sizing_mode()
            .is_fullscreen()
            .then(|| id.clone())
    }

    fn pending_fullscreen_window(&self) -> Option<&W::Id> {
        self.fullscreen_window.as_ref()
    }

    /// Returns a reference to the current layout information, avoiding clones.
    fn display_layouts(&self) -> &[LeafLayoutInfo] {
        if self.tree.leaf_layouts().is_empty() {
            self.tree
                .pending_leaf_layouts()
                .unwrap_or_else(|| self.tree.leaf_layouts())
        } else {
            self.tree.leaf_layouts()
        }
    }

    fn effective_tab_bar_config(&self) -> TabBar {
        self.options.layout.tab_bar.clone()
    }

    fn update_workspace_layout_state(&mut self, layout: Layout) {
        if self.workspace_layout != layout
            && matches!(self.workspace_layout, Layout::SplitH | Layout::SplitV)
        {
            self.workspace_prev_split_layout = Some(self.workspace_layout);
        }
        self.workspace_layout = layout;
    }

    fn workspace_layout_target_kind(&self) -> WorkspaceLayoutTargetKind {
        if self.tree.selected_is_container() {
            if self.tree.selected_container_is_root() {
                if self.tree.root_is_synthetic_workspace_container() {
                    return WorkspaceLayoutTargetKind::SyntheticRootContainer;
                }
                return WorkspaceLayoutTargetKind::SelectedRootContainer;
            }

            return WorkspaceLayoutTargetKind::SelectedContainer;
        }

        if self.tree.focused_leaf_targets_workspace_layout() {
            if self.tree.focus_path().is_empty() {
                return WorkspaceLayoutTargetKind::RootLeaf;
            }
            return WorkspaceLayoutTargetKind::SyntheticRootContainer;
        }

        WorkspaceLayoutTargetKind::FocusedContainer
    }

    fn apply_workspace_layout_target(&mut self, layout: Layout) -> bool {
        if self.workspace_layout == layout {
            return false;
        }

        match self.workspace_layout_target_kind() {
            WorkspaceLayoutTargetKind::RootLeaf => self.tree.set_focused_layout(layout),
            WorkspaceLayoutTargetKind::SyntheticRootContainer => {
                self.tree.set_root_container_layout(layout)
            }
            WorkspaceLayoutTargetKind::SelectedRootContainer => {
                self.tree.set_layout_for_selected_container(layout)
            }
            WorkspaceLayoutTargetKind::SelectedContainer
            | WorkspaceLayoutTargetKind::FocusedContainer => false,
        }
    }

    fn available_span(&self, total: f64, child_count: usize) -> f64 {
        if child_count == 0 {
            return 0.0;
        }
        let gap = self.options.layout.gaps;
        (total - gap * (child_count as f64 - 1.0)).max(0.0)
    }

    fn percent_from_size_change(current_percent: f64, available: f64, change: SizeChange) -> f64 {
        if available <= 0.0 {
            return current_percent;
        }

        let to_proportion = |value: f64| {
            if value.abs() > 1.0 {
                value / 100.0
            } else {
                value
            }
        };

        let percent = match change {
            SizeChange::SetFixed(px) => px as f64 / available,
            SizeChange::AdjustFixed(delta) => current_percent + (delta as f64 / available),
            SizeChange::SetProportion(prop) => to_proportion(prop),
            SizeChange::AdjustProportion(delta) => current_percent + to_proportion(delta),
        };

        percent.clamp(0.0, 1.0)
    }

    fn resolve_preset_dimension(available: f64, preset: PresetSize) -> f64 {
        match preset {
            PresetSize::Proportion(prop) => {
                let proportion = if prop.abs() > 1.0 {
                    (prop / 100.0).clamp(0.0, 1.0)
                } else {
                    prop.clamp(0.0, 1.0)
                };
                available * proportion
            }
            PresetSize::Fixed(px) => px as f64,
        }
    }

    fn cycle_presets(
        &self,
        available: f64,
        current_percent: f64,
        presets: &[PresetSize],
        forwards: bool,
    ) -> Option<f64> {
        if presets.is_empty() || available <= 0.0 {
            return None;
        }

        let resolved: Vec<f64> = presets
            .iter()
            .map(|preset| Self::resolve_preset_dimension(available, *preset))
            .collect();

        if resolved.is_empty() {
            return None;
        }

        let epsilon = 0.5;
        let current_width = current_percent * available;

        let target_width = if forwards {
            resolved
                .iter()
                .copied()
                .find(|width| *width > current_width + epsilon)
                .unwrap_or_else(|| resolved[0])
        } else {
            resolved
                .iter()
                .copied()
                .rev()
                .find(|width| *width + epsilon < current_width)
                .unwrap_or_else(|| *resolved.last().unwrap())
        };

        Some((target_width / available).clamp(0.0, 1.0))
    }

    fn window_path(&self, window: Option<&W::Id>) -> Option<Vec<usize>> {
        if let Some(id) = window {
            self.tree.find_window(id)
        } else {
            let selected_path = self.tree.selected_path();
            if selected_path.is_empty() {
                self.tree.focused_window().is_some().then(|| selected_path)
            } else {
                Some(selected_path)
            }
        }
    }

    fn window_container_metrics(
        &self,
        path: &[usize],
        layout: Layout,
    ) -> Option<(Vec<usize>, usize, f64, usize, Rectangle<f64, Logical>)> {
        let (parent_path, child_idx) = self.tree.find_parent_with_layout(path.to_vec(), layout)?;
        let (container_layout, rect, child_count) =
            self.tree.container_info(parent_path.as_slice())?;
        if container_layout != layout || child_count == 0 {
            return None;
        }

        let available = match layout {
            Layout::SplitH => self.available_span(rect.size.w, child_count),
            Layout::SplitV => self.available_span(rect.size.h, child_count),
            Layout::Tabbed | Layout::Stacked => return None,
        };

        if available <= 0.0 {
            return None;
        }

        Some((parent_path, child_idx, available, child_count, rect))
    }

    fn selected_geometry(&self) -> Option<Rectangle<f64, Logical>> {
        if self.display_layouts().is_empty() {
            return None;
        }
        let path = self.tree.selected_path();

        if self.tree.is_leaf_at_path(&path) {
            let info = self
                .display_layouts()
                .iter()
                .find(|info| info.path == path)?;
            return Some(info.rect);
        }

        // For container selection visuals, prefer the on-screen leaf geometry under this
        // container path. This stays in sync with what is currently rendered even when
        // container cached geometry is in transition.
        let mut bounds: Option<Rectangle<f64, Logical>> = None;
        for info in self
            .display_layouts()
            .iter()
            .filter(|info| info.path.starts_with(&path))
        {
            bounds = Some(match bounds {
                Some(acc) => {
                    let left = acc.loc.x.min(info.rect.loc.x);
                    let top = acc.loc.y.min(info.rect.loc.y);
                    let right = (acc.loc.x + acc.size.w).max(info.rect.loc.x + info.rect.size.w);
                    let bottom = (acc.loc.y + acc.size.h).max(info.rect.loc.y + info.rect.size.h);
                    Rectangle::new(
                        Point::from((left, top)),
                        Size::from((right - left, bottom - top)),
                    )
                }
                None => info.rect,
            });
        }

        bounds.or_else(|| self.tree.container_info(&path).map(|(_, rect, _)| rect))
    }

    pub fn selected_is_container(&self) -> bool {
        self.tree.selected_is_container()
    }

    pub(super) fn close_window_ids_for_active_selection(&self) -> Vec<W::Id> {
        if self.tree.selected_is_container() {
            let path = self.tree.selected_path();
            return self.tree.window_ids_under_path(&path);
        }

        self.tree
            .focused_window()
            .map(|window| vec![window.id().clone()])
            .unwrap_or_default()
    }

    pub(super) fn take_selected_subtree(
        &mut self,
    ) -> Option<(
        DetachedNode<W>,
        Option<InsertParentInfo>,
        Rectangle<f64, Logical>,
    )> {
        let path = self.tree.selected_path();
        let rect = self.selected_geometry()?;
        let (subtree, origin) = self.tree.take_subtree_at_path(&path)?;
        Some((subtree, origin, rect))
    }

    pub(super) fn take_workspace_subtree_for_floating(
        &mut self,
    ) -> Option<(DetachedNode<W>, Rectangle<f64, Logical>)> {
        let rect = self.working_area;

        let subtree = if self.tree.root_is_synthetic_workspace_container() {
            match self.tree.root_children_len() {
                0 => return None,
                1 => self.tree.take_root_child_subtree(0)?,
                _ => {
                    if !self
                        .tree
                        .wrap_synthetic_root_children_for_workspace_layout(self.workspace_layout)
                    {
                        return None;
                    }
                    self.tree.take_root_child_subtree(0)?
                }
            }
        } else {
            let (subtree, _) = self.tree.take_subtree_at_path(&[])?;
            match subtree {
                DetachedNode::Leaf(tile) => DetachedNode::Container(DetachedContainer::from_parts(
                    self.workspace_layout,
                    vec![DetachedNode::Leaf(tile)],
                    vec![1.0],
                    vec![0],
                    true,
                    Some(self.workspace_layout),
                )),
                subtree => subtree,
            }
        };

        self.sync_fullscreen_window();
        self.tree.layout();
        Some((subtree, rect))
    }

    fn container_available_span(
        &self,
        parent_path: &[usize],
        layout: Layout,
    ) -> Option<(f64, usize)> {
        let (container_layout, rect, child_count) = self.tree.container_info(parent_path)?;
        if container_layout != layout || child_count == 0 {
            return None;
        }

        let available = match layout {
            Layout::SplitH => self.available_span(rect.size.w, child_count),
            Layout::SplitV => self.available_span(rect.size.h, child_count),
            Layout::Tabbed | Layout::Stacked => return None,
        };

        if available <= 0.0 {
            return None;
        }

        Some((available, child_count))
    }

    fn resize_target_for_edge(
        &self,
        path: &[usize],
        edge: ResizeEdge,
        layout: Layout,
        pos: Option<Point<f64, Logical>>,
    ) -> Option<ResizeTarget> {
        let mut best: Option<(ResizeTarget, f64)> = None;
        let mut fallback = None;
        let mut current_path = path.to_vec();

        while !current_path.is_empty() {
            let child_idx = *current_path.last().unwrap();
            let parent_path = &current_path[..current_path.len() - 1];

            let Some((container_layout, _rect, child_count)) =
                self.tree.container_info(parent_path)
            else {
                current_path.pop();
                continue;
            };

            if container_layout == layout && child_count > 1 {
                let neighbor_idx = if edge == ResizeEdge::LEFT || edge == ResizeEdge::TOP {
                    child_idx.checked_sub(1)
                } else if edge == ResizeEdge::RIGHT || edge == ResizeEdge::BOTTOM {
                    (child_idx + 1 < child_count).then_some(child_idx + 1)
                } else {
                    None
                };

                if let Some(neighbor_idx) = neighbor_idx {
                    if let Some(child_rect) = self.tree.child_rect_at(parent_path, child_idx) {
                        let target = ResizeTarget {
                            parent_path: parent_path.to_vec(),
                            child_idx,
                            neighbor_idx,
                            original_span: if edge == ResizeEdge::LEFT || edge == ResizeEdge::RIGHT
                            {
                                child_rect.size.w
                            } else if edge == ResizeEdge::TOP || edge == ResizeEdge::BOTTOM {
                                child_rect.size.h
                            } else {
                                0.0
                            },
                        };

                        fallback.get_or_insert_with(|| target.clone());

                        if let Some(pos) = pos {
                            let Some(boundary) = self.resize_boundary_coord(&target, edge) else {
                                current_path.pop();
                                continue;
                            };

                            let dist = if edge == ResizeEdge::LEFT || edge == ResizeEdge::RIGHT {
                                (pos.x - boundary).abs()
                            } else if edge == ResizeEdge::TOP || edge == ResizeEdge::BOTTOM {
                                (pos.y - boundary).abs()
                            } else {
                                f64::MAX
                            };

                            let should_update = match &best {
                                None => true,
                                Some((_, best_dist)) => dist + f64::EPSILON < *best_dist,
                            };
                            if should_update {
                                best = Some((target, dist));
                            }
                        }
                    }
                }
            }

            current_path.pop();
        }

        best.map(|(target, _)| target).or(fallback)
    }

    fn resize_boundary_coord(&self, target: &ResizeTarget, edge: ResizeEdge) -> Option<f64> {
        let child_rect = self
            .tree
            .child_rect_at(target.parent_path.as_slice(), target.child_idx)?;
        let neighbor_rect = self
            .tree
            .child_rect_at(target.parent_path.as_slice(), target.neighbor_idx)?;

        if edge == ResizeEdge::LEFT || edge == ResizeEdge::RIGHT {
            let (left_edge, right_edge) = if neighbor_rect.loc.x < child_rect.loc.x {
                (neighbor_rect.loc.x + neighbor_rect.size.w, child_rect.loc.x)
            } else {
                (child_rect.loc.x + child_rect.size.w, neighbor_rect.loc.x)
            };
            return Some((left_edge + right_edge) / 2.0);
        }

        if edge == ResizeEdge::TOP || edge == ResizeEdge::BOTTOM {
            let (top_edge, bottom_edge) = if neighbor_rect.loc.y < child_rect.loc.y {
                (neighbor_rect.loc.y + neighbor_rect.size.h, child_rect.loc.y)
            } else {
                (child_rect.loc.y + child_rect.size.h, neighbor_rect.loc.y)
            };
            return Some((top_edge + bottom_edge) / 2.0);
        }

        None
    }

    fn compute_resize_targets(
        &self,
        window: &W::Id,
        mut edges: ResizeEdge,
        pos: Option<Point<f64, Logical>>,
    ) -> Option<(ResizeEdge, Option<ResizeTarget>, Option<ResizeTarget>)> {
        let Some(path) = self.tree.find_window(window) else {
            return None;
        };
        let Some(tile) = self.tree.tile_at_path(&path) else {
            return None;
        };

        if !tile.window().pending_sizing_mode().is_normal() {
            return None;
        }

        let mut horizontal = None;
        let mut vertical = None;

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let edge = if edges.contains(ResizeEdge::LEFT) {
                ResizeEdge::LEFT
            } else {
                ResizeEdge::RIGHT
            };
            horizontal = self.resize_target_for_edge(&path, edge, Layout::SplitH, pos);
            if horizontal.is_none() {
                edges.remove(ResizeEdge::LEFT_RIGHT);
            }
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            let edge = if edges.contains(ResizeEdge::TOP) {
                ResizeEdge::TOP
            } else {
                ResizeEdge::BOTTOM
            };
            vertical = self.resize_target_for_edge(&path, edge, Layout::SplitV, pos);
            if vertical.is_none() {
                edges.remove(ResizeEdge::TOP_BOTTOM);
            }
        }

        if edges.is_empty() {
            return None;
        }

        Some((edges, horizontal, vertical))
    }

    fn interactive_resize_data_for_path(
        path: &[usize],
        resize: &InteractiveResizeState<W>,
    ) -> Option<InteractiveResizeData> {
        let mut edges = ResizeEdge::empty();

        if let Some(target) = resize.horizontal.as_ref() {
            if let Some(edge) =
                resize_edge_for_path_at_target(path, target, ResizeEdge::LEFT, ResizeEdge::RIGHT)
            {
                edges |= edge;
            }
        }

        if let Some(target) = resize.vertical.as_ref() {
            if let Some(edge) =
                resize_edge_for_path_at_target(path, target, ResizeEdge::TOP, ResizeEdge::BOTTOM)
            {
                edges |= edge;
            }
        }

        (!edges.is_empty()).then_some(InteractiveResizeData { edges })
    }

    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let tree = ContainerTree::new(view_size, working_area, scale, options.clone());
        let background_color = options.layout.background_color;

        Self {
            tree,
            workspace_layout: Layout::SplitH,
            workspace_prev_split_layout: None,
            view_size,
            working_area,
            viewport: FixedViewport,
            scale,
            clock,
            interactive_resize: None,
            options,
            tab_bar_cache: RefCell::new(HashMap::new()),
            tab_bar_cache_alt: RefCell::new(HashMap::new()),
            is_active: false,
            fullscreen_window: None,
            closing_windows: Vec::new(),
            overview_offscreen: OffscreenBuffer::default(),
            overview_background: SolidColorBuffer::new(view_size, background_color),
        }
    }

    // Basic getters using ContainerTree
    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tree.all_windows().into_iter()
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        TilePtrIter::new(self.tree.tile_ptrs())
    }

    pub fn active_tile(&self) -> Option<&Tile<W>> {
        self.tree.focused_tile()
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        self.tree.focused_window_mut()
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.tree.focused_tile().map_or(false, |tile| {
            tile.window().pending_sizing_mode().is_fullscreen()
                || tile.window().is_pending_windowed_fullscreen()
        })
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    pub fn verify_invariants(&self) {
        self.tree.verify_invariants();
    }

    #[cfg(test)]
    pub fn debug_tree(&self) -> String
    where
        W::Id: std::fmt::Display,
    {
        self.tree.debug_tree()
    }

    #[cfg(test)]
    pub fn focus_path(&self) -> Vec<usize> {
        self.tree.focus_path()
    }

    #[cfg(test)]
    pub fn debug_workspace_layout(&self) -> Layout {
        self.workspace_layout
    }

    #[cfg(test)]
    pub fn debug_root_is_synthetic_workspace_container(&self) -> bool {
        self.tree.root_is_synthetic_workspace_container()
    }

    pub fn selected_path(&self) -> Vec<usize> {
        self.tree.selected_path()
    }

    pub fn select_container_path(&mut self, path: &[usize]) -> bool {
        self.tree.select_container_at_path(path)
    }

    pub fn remove_window(&mut self, window: &W) -> Option<RemovedTile<W>> {
        let window_id = window.id();
        let tile = self.tree.remove_window(&window_id)?;

        if self
            .fullscreen_window
            .as_ref()
            .is_some_and(|id| id == window_id)
        {
            self.fullscreen_window = None;
        }

        // Create RemovedTile
        Some(RemovedTile {
            tile,
            width: ColumnWidth::default(),
            is_full_width: false,
            is_floating: false,
        })
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<smithay::utils::Serial>) {
        let Some(path) = self.tree.find_window(window) else {
            return;
        };
        let Some(tile) = self.tree.tile_at_path_mut(&path) else {
            return;
        };

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        tile.update_window();
    }

    pub fn render_elements<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        tiling_focus_ring: bool,
    ) -> Vec<TilingSpaceRenderElement<R>> {
        // Pre-allocate: ~4 elements per tile + closing windows + tab bars
        let tile_count = self.tree.window_count();
        let estimated_capacity = tile_count * 4 + self.closing_windows.len() + tile_count / 2;
        let mut elements = Vec::with_capacity(estimated_capacity);
        let mut active_elements = Vec::with_capacity(8);
        let scale = Scale::from(self.scale);
        let focus_path = self.tree.focus_path();
        let selection_is_container = self.tree.selected_is_container();
        let fullscreen_id = self.render_fullscreen_window();
        let windowed_fullscreen_id = if fullscreen_id.is_none() {
            self.tree.focused_tile().and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };
        let has_fullscreen_like = fullscreen_id.is_some() || windowed_fullscreen_id.is_some();
        let view_rect = Rectangle::from_size(self.view_size);

        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            elements.push(TilingSpaceRenderElement::ClosingWindow(elem));
        }

        // Render container selection before regular tiling elements so it ends up
        // visually on top after the global reverse-order composition pass.
        if selection_is_container && (tiling_focus_ring || self.is_active) {
            if let Some(rect) = self.selected_geometry() {
                let mut selection_border = self.options.layout.border;
                if let Some(focus_info) = self
                    .display_layouts()
                    .iter()
                    .find(|info| info.path == focus_path)
                {
                    if let Some(tile) = self.tree.get_tile(focus_info.key) {
                        if let Some(width) = tile.effective_border_width() {
                            selection_border.width = width;
                        }
                    }
                }
                render_container_selection(
                    ctx.renderer,
                    rect,
                    view_rect,
                    self.scale,
                    self.is_active,
                    self.options.layout.focus_ring,
                    selection_border,
                    ContainerSelectionStyle::Tiling,
                    &mut |elem| elements.push(TilingSpaceRenderElement::ContainerSelection(elem)),
                );
            }
        }

        let render_layouts = self.display_layouts();
        for info in render_layouts.iter().rev() {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile(info.key) {
                let is_fullscreen_tile = fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_windowed_fullscreen_tile = windowed_fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_fullscreen_like_tile = is_fullscreen_tile || is_windowed_fullscreen_tile;
                let show_tile = if has_fullscreen_like {
                    is_fullscreen_like_tile
                } else {
                    info.visible
                };

                if !show_tile {
                    continue;
                }

                let mut pos = info.rect.loc + tile.render_offset();
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
                if is_fullscreen_like_tile {
                    pos = Point::from((0.0, 0.0));
                }

                let is_focused =
                    self.is_active && info.path == focus_path && !selection_is_container;
                let draw_focus = tiling_focus_ring && is_focused;
                let target_elements = if info.path == focus_path {
                    &mut active_elements
                } else {
                    &mut elements
                };
                let tile_xray_pos = xray_pos.offset(pos);
                tile.render(ctx.r(), pos, tile_xray_pos, draw_focus, &mut |elem| {
                    target_elements.push(TilingSpaceRenderElement::from(elem));
                });
            }
        }

        elements.extend(active_elements);

        if !has_fullscreen_like && !self.options.layout.tab_bar.off {
            let tab_bar_infos = self.tree.tab_bar_layouts();
            let mut cache = self.tab_bar_cache.borrow_mut();
            let mut next_cache = self.tab_bar_cache_alt.borrow_mut();
            next_cache.clear();
            let gles = ctx.renderer.as_gles_renderer();
            let tab_bar_config = self.effective_tab_bar_config();
            let is_active_workspace = self.is_active;
            let target = ctx.target;
            for info in tab_bar_infos {
                let state = tab_bar_state_from_info(
                    &info,
                    &tab_bar_config,
                    is_active_workspace,
                    self.scale,
                    target,
                );
                let (buffer, tab_widths_px) = match cache.get(&info.path) {
                    Some(entry) if entry.state == state => {
                        (entry.buffer.clone(), entry.tab_widths_px.clone())
                    }
                    _ => match render_tab_bar(
                        gles,
                        &tab_bar_config,
                        info.layout,
                        info.rect,
                        info.row_height,
                        &info.tabs,
                        is_active_workspace,
                        target,
                        self.scale,
                    ) {
                        Ok(TabBarRenderOutput {
                            buffer,
                            tab_widths_px,
                        }) => (buffer, tab_widths_px),
                        Err(err) => {
                            warn!("tab bar render failed: {err}");
                            continue;
                        }
                    },
                };

                let mut location = info.rect.loc;
                location = location.to_physical_precise_round(scale).to_logical(scale);
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer.clone(),
                    location,
                    1.0,
                    None,
                    None,
                    Kind::Unspecified,
                );
                elements.push(TilingSpaceRenderElement::TabBar(
                    PrimaryGpuTextureRenderElement(elem),
                ));

                next_cache.insert(
                    info.path,
                    TabBarCacheEntry {
                        state,
                        buffer,
                        tab_widths_px,
                    },
                );
            }
            // Swap caches: next becomes current, current will be cleared on next frame
            std::mem::swap(&mut *cache, &mut *next_cache);
        } else {
            self.tab_bar_cache.borrow_mut().clear();
        }

        elements
    }

    pub fn render<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        tiling_focus_ring: bool,
        push: &mut dyn FnMut(TilingSpaceRenderElement<R>),
    ) {
        for elem in self.render_elements(ctx, xray_pos, tiling_focus_ring) {
            push(elem);
        }
    }

    pub fn render_as_offscreen(
        &self,
        renderer: &mut GlesRenderer,
        target: RenderTarget,
        tiling_focus_ring: bool,
    ) -> Option<OffscreenRenderElement> {
        for tile in self.tiles() {
            tile.window().set_offscreen_data(None);
        }

        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
        };
        let mut elements = self.render_elements(ctx, XrayPos::default(), tiling_focus_ring);
        if elements.is_empty() {
            return None;
        }

        let background = SolidColorRenderElement::from_buffer(
            &self.overview_background,
            Point::from((0., 0.)),
            1.,
            Kind::Unspecified,
        );
        elements.push(background.into());

        self.overview_offscreen
            .render(renderer, Scale::from(self.scale), &elements)
            .map(|(elem, _sync, data)| {
                for tile in self.tiles() {
                    tile.window().set_offscreen_data(Some(data.clone()));
                }

                elem
            })
            .map_err(|err| warn!("error rendering tiling space to offscreen: {err:?}"))
            .ok()
    }

    // Layout operations using ContainerTree
    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options.clone();
        self.overview_background
            .update(view_size, options.layout.background_color);
        self.tree
            .update_config(view_size, working_area, scale, options);
        self.tree.layout();
    }

    pub fn set_view_size(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        self.view_size = view_size;
        self.working_area = working_area;
        self.overview_background.resize(view_size);
        self.tree.set_view_size(view_size, working_area);
        // Recalculate layout on resize
        self.tree.layout();
    }

    pub fn advance_animations(&mut self) {
        for tile in self.tiles_mut() {
            tile.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.tiles().any(|tile| tile.are_animations_ongoing()) || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        self.is_active = is_active;
        let applied = self.tree.apply_pending_layouts_if_ready();
        if applied && self.tree.take_pending_relayout() {
            self.tree.layout();
        }
        let has_pending = self.tree.has_pending_layouts();
        let state_layouts = if has_pending {
            self.tree
                .pending_leaf_layouts_cloned()
                .unwrap_or_else(|| self.tree.leaf_layouts_cloned())
        } else {
            self.tree.leaf_layouts_cloned()
        };
        let workspace_view = Rectangle::from_size(self.view_size);
        let focus_path = self.tree.focus_path();
        let selection_is_container = self.tree.selected_is_container();
        let scale = Scale::from(self.scale);
        let logical_fullscreen_id = self.pending_fullscreen_window().cloned();
        let visual_fullscreen_id = self.render_fullscreen_window();
        let windowed_fullscreen_id = if visual_fullscreen_id.is_none() {
            self.tree.focused_tile().and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };
        let has_fullscreen_like =
            visual_fullscreen_id.is_some() || windowed_fullscreen_id.is_some();
        let layout_rect = self.tree.layout_area();
        let is_single_window = self.tree.window_count() <= 1;
        // Clone here because we need mutable access to tree in the loop below.
        let render_layouts = self.display_layouts().to_vec();
        let render_edges: Vec<(FocusRingEdges, Option<FocusRingIndicatorEdge>)> = render_layouts
            .iter()
            .map(|info| {
                let edges = edge_visibility_for_tile(
                    &self.options,
                    layout_rect,
                    info.rect,
                    self.scale,
                    is_single_window,
                );
                let indicator_edge = split_indicator_edge_for_tile(&self.tree, &info.path, edges);
                (edges, indicator_edge)
            })
            .collect();

        for info in state_layouts {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile_mut(info.key) {
                let resize = self
                    .interactive_resize
                    .as_ref()
                    .and_then(|resize| Self::interactive_resize_data_for_path(&info.path, resize));
                Self::update_window_state(
                    tile,
                    &info,
                    &focus_path,
                    is_active,
                    self.options.deactivate_unfocused_windows,
                    resize,
                    !has_pending,
                    self.working_area.size,
                    &self.options,
                    logical_fullscreen_id.as_ref(),
                    windowed_fullscreen_id.as_ref(),
                    self.view_size,
                );
            }
        }

        for (info, (edges, indicator_edge)) in render_layouts.into_iter().zip(render_edges) {
            // Computed before the mutable tile borrow below (immutable tree access).
            let is_focus_head = self.tree.path_is_parent_focus_head(&info.path);
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile_mut(info.key) {
                let is_fullscreen_tile = visual_fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_windowed_fullscreen_tile = windowed_fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_fullscreen_like_tile = is_fullscreen_tile || is_windowed_fullscreen_tile;

                let mut pos = info.rect.loc + tile.render_offset();
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
                if is_fullscreen_like_tile {
                    pos = Point::from((0.0, 0.0));
                }

                let mut tile_view_rect = workspace_view;
                tile_view_rect.loc -= pos;

                if is_fullscreen_like_tile {
                    tile_view_rect = workspace_view;
                }

                let show_tile = if has_fullscreen_like {
                    is_fullscreen_like_tile
                } else {
                    info.visible
                };
                if show_tile {
                    let is_focused =
                        is_active && info.path == focus_path && !selection_is_container;
                    tile.update_render_elements(
                        is_active,
                        is_focused,
                        is_focus_head,
                        edges,
                        indicator_edge,
                        tile_view_rect,
                    );
                }
            }
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        self.interactive_resize_begin_internal(window, edges, None)
    }

    pub fn interactive_resize_begin_at(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.interactive_resize_begin_internal(window, edges, Some(pos))
    }

    fn interactive_resize_begin_internal(
        &mut self,
        window: W::Id,
        edges: ResizeEdge,
        pos: Option<Point<f64, Logical>>,
    ) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some((edges, horizontal, vertical)) = self.compute_resize_targets(&window, edges, pos)
        else {
            return false;
        };

        self.interactive_resize = Some(InteractiveResizeState {
            window,
            data: InteractiveResizeData { edges },
            horizontal,
            vertical,
        });
        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
            return false;
        };

        if window != &resize.window {
            return false;
        }

        let mut changed = false;

        if resize.data.edges.intersects(ResizeEdge::LEFT_RIGHT) {
            if let Some(target) = resize.horizontal.as_ref() {
                if let Some((available, _child_count)) =
                    self.container_available_span(&target.parent_path, Layout::SplitH)
                {
                    let mut dx = delta.x;
                    if resize.data.edges.contains(ResizeEdge::LEFT) {
                        dx = -dx;
                    }

                    let base = target.original_span.max(1.0);
                    let window_width = (base + dx).round() as i32;
                    let current_percent = self
                        .tree
                        .child_percent_at(target.parent_path.as_slice(), target.child_idx)
                        .unwrap_or(1.0);
                    let percent = Self::percent_from_size_change(
                        current_percent,
                        available,
                        SizeChange::SetFixed(window_width),
                    );

                    if self.tree.set_child_percent_pair_at(
                        target.parent_path.as_slice(),
                        target.child_idx,
                        target.neighbor_idx,
                        Layout::SplitH,
                        percent,
                    ) {
                        changed = true;
                    }
                }
            }
        }

        if resize.data.edges.intersects(ResizeEdge::TOP_BOTTOM) {
            if let Some(target) = resize.vertical.as_ref() {
                if let Some((available, _child_count)) =
                    self.container_available_span(&target.parent_path, Layout::SplitV)
                {
                    let mut dy = delta.y;
                    if resize.data.edges.contains(ResizeEdge::TOP) {
                        dy = -dy;
                    }

                    let base = target.original_span.max(1.0);
                    let window_height = (base + dy).round() as i32;
                    let current_percent = self
                        .tree
                        .child_percent_at(target.parent_path.as_slice(), target.child_idx)
                        .unwrap_or(1.0);
                    let percent = Self::percent_from_size_change(
                        current_percent,
                        available,
                        SizeChange::SetFixed(window_height),
                    );

                    if self.tree.set_child_percent_pair_at(
                        target.parent_path.as_slice(),
                        target.child_idx,
                        target.neighbor_idx,
                        Layout::SplitV,
                        percent,
                    ) {
                        changed = true;
                    }
                }
            }
        }

        if changed {
            self.tree.layout_with_animation_flags(false, false);
        }

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };

        if let Some(window) = window {
            if window != &resize.window {
                return;
            }
        }

        self.interactive_resize = None;
    }

    pub fn cancel_resize_for_window(&mut self, window: &W) {
        if self
            .interactive_resize
            .as_ref()
            .is_some_and(|resize| &resize.window == window.id())
        {
            self.interactive_resize = None;
        }
    }

    pub fn resize_edges_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.resize_hit_under(pos).map(|hit| hit.edges)
    }

    pub fn resize_hit_under(&mut self, pos: Point<f64, Logical>) -> Option<ResizeHit<W::Id>> {
        let has_fullscreen_like = self.render_fullscreen_window().is_some()
            || self
                .tree
                .focused_tile()
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen());
        if has_fullscreen_like {
            return None;
        }

        let (path, rect) = self.closest_leaf_rect(pos)?;
        let tile = self.tree.tile_at_path(&path)?;
        if !tile.window().pending_sizing_mode().is_normal() {
            return None;
        }

        let border = tile.effective_border_width().unwrap_or(0.0) * 2.0;
        let threshold = super::RESIZE_EDGE_THRESHOLD.max(border);
        let gap_half = self.options.layout.gaps / 2.0;
        let edge_threshold = threshold.max(gap_half);
        let cross_threshold = threshold;

        let clamp_x = pos.x.clamp(rect.loc.x, rect.loc.x + rect.size.w);
        let clamp_y = pos.y.clamp(rect.loc.y, rect.loc.y + rect.size.h);
        let pos_within = Point::from((clamp_x - rect.loc.x, clamp_y - rect.loc.y));
        let edges =
            super::resize_edges_for_point(pos_within, rect.size, tile.effective_border_width());

        let mut best: Option<(ResizeEdge, f64)> = None;
        let mut consider_edge = |edge: ResizeEdge, dist: f64, cross_ok: bool, layout: Layout| {
            if !edges.contains(edge) || !cross_ok || dist > edge_threshold {
                return;
            }
            if self
                .resize_target_for_edge(&path, edge, layout, Some(pos))
                .is_none()
            {
                return;
            }
            let score = dist / edge_threshold.max(1.0);
            if best.map_or(true, |(_, best_score)| score < best_score) {
                best = Some((edge, score));
            }
        };

        let left_dist = (pos.x - rect.loc.x).abs();
        let right_dist = (pos.x - (rect.loc.x + rect.size.w)).abs();
        let top_dist = (pos.y - rect.loc.y).abs();
        let bottom_dist = (pos.y - (rect.loc.y + rect.size.h)).abs();

        let cross_ok_y = pos.y + cross_threshold >= rect.loc.y
            && pos.y - cross_threshold <= rect.loc.y + rect.size.h;
        let cross_ok_x = pos.x + cross_threshold >= rect.loc.x
            && pos.x - cross_threshold <= rect.loc.x + rect.size.w;

        consider_edge(ResizeEdge::LEFT, left_dist, cross_ok_y, Layout::SplitH);
        consider_edge(ResizeEdge::RIGHT, right_dist, cross_ok_y, Layout::SplitH);
        consider_edge(ResizeEdge::TOP, top_dist, cross_ok_x, Layout::SplitV);
        consider_edge(ResizeEdge::BOTTOM, bottom_dist, cross_ok_x, Layout::SplitV);

        let (edge, _) = best?;

        Some(ResizeHit {
            window: tile.window().id().clone(),
            edges: edge,
            cursor: edge.cursor_icon(),
            is_floating: false,
        })
    }

    // Focus operations using ContainerTree
    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if self.tree.focus_window_by_id(window) {
            self.tree.layout();
            true
        } else {
            false
        }
    }

    fn focus_in_direction_with_fullscreen_scope(
        &mut self,
        direction: Direction,
        allow_wrap: bool,
    ) -> bool {
        // Model rule: with active fullscreen in tiling, directional focus can move inside the
        // fullscreen subtree, but must not escape to another root sibling.
        let fullscreen_scope = self.fullscreen_window.as_ref().map(|id| {
            let root_idx = self
                .tree
                .find_window(id)
                .and_then(|path| path.first().copied());
            (id.clone(), root_idx)
        });

        // If fullscreen is active but we cannot determine its root scope, be conservative.
        if fullscreen_scope
            .as_ref()
            .is_some_and(|(_, root_idx)| root_idx.is_none())
        {
            return false;
        }

        let focused = if fullscreen_scope.is_some() || !allow_wrap {
            self.tree.focus_in_direction_no_wrap(direction)
        } else {
            self.tree.focus_in_direction(direction)
        };
        if !focused {
            return false;
        }

        if let Some((fullscreen_id, Some(scope_root_idx))) = fullscreen_scope {
            let escaped_scope = self.tree.focus_path().first().copied() != Some(scope_root_idx);
            if escaped_scope {
                let _ = self.tree.focus_window_by_id(&fullscreen_id);
                self.tree.layout();
                return false;
            }
        }

        self.tree.layout();
        true
    }

    pub fn focus_left(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Left, true)
    }

    pub fn focus_left_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Left, false)
    }

    pub fn focus_right(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Right, true)
    }

    pub fn focus_right_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Right, false)
    }

    pub fn focus_down(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Down, true)
    }

    pub fn focus_down_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Down, false)
    }

    pub fn focus_up(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Up, true)
    }

    pub fn focus_up_no_wrap(&mut self) -> bool {
        self.focus_in_direction_with_fullscreen_scope(Direction::Up, false)
    }

    pub fn focus_parent(&mut self) -> bool {
        if self.fullscreen_window.is_some() {
            return false;
        }

        let selected = self.tree.select_parent();
        if selected {
            // Force immediate redraw for container-selection visuals.
            self.tree.layout();
        }
        selected
    }

    pub fn focus_child(&mut self) -> bool {
        let selected = self.tree.select_child();
        if selected {
            self.tree.layout();
        }
        selected
    }

    pub fn focus_parent_targets_workspace(&self) -> bool {
        if self.fullscreen_window.is_some() {
            return false;
        }

        if self.tree.selected_is_container() {
            return self.tree.selected_container_is_root();
        }

        self.tree.focused_leaf_targets_workspace_layout()
    }

    pub fn clear_selection_context(&mut self) {
        self.tree.clear_selection();
    }

    pub(super) fn root_layout_and_child_count(&self) -> Option<(Layout, usize)> {
        self.tree
            .container_info(&[])
            .map(|(layout, _rect, child_count)| (layout, child_count))
    }

    pub fn select_root_container(&mut self) -> bool {
        self.tree.select_root_container()
    }

    pub(super) fn inactive_tiling_reference_for_parent_of_selected_reference(
        &self,
    ) -> Option<super::container::InactiveTilingReference> {
        self.tree
            .inactive_tiling_reference_for_parent_of_selected_reference()
    }

    pub(super) fn inactive_tiling_reference_for_selected_or_focused(
        &self,
    ) -> Option<super::container::InactiveTilingReference> {
        self.tree
            .inactive_tiling_reference_for_selected_or_focused()
    }

    pub(super) fn inactive_tiling_reference_for_parent_of_window(
        &self,
        window: &W::Id,
    ) -> Option<super::container::InactiveTilingReference> {
        self.tree
            .inactive_tiling_reference_for_parent_of_window(window)
    }

    pub(super) fn inactive_tiling_reference_chain_for_focused_reference(
        &self,
    ) -> Vec<super::container::InactiveTilingReference> {
        self.tree
            .inactive_tiling_reference_chain_for_focused_reference()
    }

    pub(super) fn inactive_tiling_reference_chain_for_focused_leaf(
        &self,
    ) -> Vec<super::container::InactiveTilingReference> {
        self.tree.inactive_tiling_reference_chain_for_focused_leaf()
    }

    pub(super) fn insert_parent_info_from_inactive_tiling_reference(
        &self,
        reference: &super::container::InactiveTilingReference,
    ) -> Option<super::container::InsertParentInfo> {
        self.tree
            .insert_parent_info_from_inactive_tiling_reference(reference)
    }

    pub(super) fn insert_parent_info_from_inactive_tiling_reference_strict(
        &self,
        reference: &super::container::InactiveTilingReference,
    ) -> Option<super::container::InsertParentInfo> {
        self.tree
            .insert_parent_info_from_inactive_tiling_reference_strict(reference)
    }

    pub(super) fn inactive_tiling_reference_is_root_container_strict(
        &self,
        reference: &super::container::InactiveTilingReference,
    ) -> bool {
        self.tree
            .inactive_tiling_reference_is_root_container_strict(reference)
    }

    pub(super) fn has_inactive_tiling_reference(
        &self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
    ) -> bool {
        self.tree.has_inactive_tiling_reference(reference, strict)
    }

    pub(super) fn focus_inactive_tiling_reference(
        &mut self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
    ) -> bool {
        self.tree.focus_inactive_tiling_reference(reference, strict)
    }

    pub(super) fn window_for_inactive_tiling_reference(
        &self,
        reference: &super::container::InactiveTilingReference,
        strict: bool,
    ) -> Option<&W> {
        self.tree
            .window_for_inactive_tiling_reference(reference, strict)
    }

    pub fn wrap_root_for_sibling_insert(&mut self) -> bool {
        let changed = self.tree.wrap_root_for_sibling_insert();
        if changed {
            self.tree.layout();
        }
        changed
    }

    fn active_selection_layout(&self) -> Option<Layout> {
        if self.tree.selected_is_container() {
            let path = self.tree.selected_path();
            return self.tree.container_info(&path).map(|(layout, _, _)| layout);
        }
        self.tree.focused_layout()
    }

    fn next_layout_all(current: Layout) -> Layout {
        match current {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::Stacked,
            Layout::Stacked => Layout::Tabbed,
            Layout::Tabbed => Layout::SplitH,
        }
    }

    fn split_for_active_selection(&mut self, layout: Layout) -> bool {
        let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
        self.tree
            .split_target(layout, target, RootPolicy::ImplicitWorkspace)
    }

    fn set_layout_for_active_selection(&mut self, layout: Layout) -> bool {
        match self.workspace_layout_target_kind() {
            WorkspaceLayoutTargetKind::SelectedContainer
            | WorkspaceLayoutTargetKind::FocusedContainer => {
                let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
                self.tree
                    .set_layout_for_target(layout, target, RootPolicy::ImplicitWorkspace)
            }
            WorkspaceLayoutTargetKind::RootLeaf
            | WorkspaceLayoutTargetKind::SyntheticRootContainer
            | WorkspaceLayoutTargetKind::SelectedRootContainer => {
                self.apply_workspace_layout_target(layout)
            }
        }
    }

    fn toggle_split_for_active_selection(&mut self) -> bool {
        if matches!(
            self.workspace_layout_target_kind(),
            WorkspaceLayoutTargetKind::RootLeaf
                | WorkspaceLayoutTargetKind::SyntheticRootContainer
                | WorkspaceLayoutTargetKind::SelectedRootContainer
        ) {
            let next = match self.workspace_layout {
                Layout::SplitH => Layout::SplitV,
                Layout::SplitV => Layout::SplitH,
                Layout::Tabbed | Layout::Stacked => {
                    self.workspace_prev_split_layout.unwrap_or(Layout::SplitH)
                }
            };
            return self.apply_workspace_layout_target(next);
        }

        if self.tree.selected_is_container() {
            let path = self.tree.selected_path();
            if let Some((current, _, _)) = self.tree.container_info(&path) {
                let next = match current {
                    Layout::SplitH => Layout::SplitV,
                    Layout::SplitV => Layout::SplitH,
                    Layout::Tabbed | Layout::Stacked => Layout::SplitH,
                };
                let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
                return self.tree.set_layout_for_target(
                    next,
                    target,
                    RootPolicy::ImplicitWorkspace,
                );
            }
        }

        let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
        self.tree
            .toggle_split_for_target(target, RootPolicy::ImplicitWorkspace)
    }

    fn toggle_layout_all_for_active_selection(&mut self) -> bool {
        if matches!(
            self.workspace_layout_target_kind(),
            WorkspaceLayoutTargetKind::RootLeaf
                | WorkspaceLayoutTargetKind::SyntheticRootContainer
                | WorkspaceLayoutTargetKind::SelectedRootContainer
        ) {
            let next = Self::next_layout_all(self.workspace_layout);
            return self.apply_workspace_layout_target(next);
        }

        if self.tree.selected_is_container() {
            let path = self.tree.selected_path();
            if let Some((current, _, _)) = self.tree.container_info(&path) {
                let next = Self::next_layout_all(current);
                let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
                return self.tree.set_layout_for_target(
                    next,
                    target,
                    RootPolicy::ImplicitWorkspace,
                );
            }
        }

        let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
        self.tree
            .toggle_layout_all_for_target(target, RootPolicy::ImplicitWorkspace)
    }

    fn move_command_target(&mut self, direction: Direction) -> bool {
        let target = self.tree.command_target(RootPolicy::ImplicitWorkspace);
        let result = self.tree.move_target_in_direction(direction, target);
        if result {
            self.tree.layout();
        }
        result
    }

    // Move operations using ContainerTree
    pub fn move_left(&mut self) -> bool {
        self.move_command_target(Direction::Left)
    }

    pub fn move_right(&mut self) -> bool {
        self.move_command_target(Direction::Right)
    }

    pub fn move_down(&mut self) -> bool {
        self.move_command_target(Direction::Down)
    }

    pub fn move_up(&mut self) -> bool {
        self.move_command_target(Direction::Up)
    }

    // Container operations (replacing column operations)
    pub fn consume_into_column(&mut self) {
        // In i3 model: create vertical split
        if self.split_for_active_selection(Layout::SplitV) {
            self.tree.layout();
        }
    }

    pub fn expel_from_column(&mut self) {
        // In i3 model: create horizontal split
        if self.split_for_active_selection(Layout::SplitH) {
            self.tree.layout();
        }
    }

    /// Split focused window horizontally (i3-style)
    pub fn split_horizontal(&mut self) {
        if self.split_for_active_selection(Layout::SplitH) {
            self.tree.layout();
        }
    }

    /// Split focused window vertically (i3-style)
    pub fn split_vertical(&mut self) {
        if self.split_for_active_selection(Layout::SplitV) {
            self.tree.layout();
        }
    }

    /// Split workspace root like workspace root split.
    pub fn split_workspace_horizontal(&mut self) {
        self.split_workspace(Layout::SplitH);
    }

    /// Split workspace root like workspace root split.
    pub fn split_workspace_vertical(&mut self) {
        self.split_workspace(Layout::SplitV);
    }

    fn split_workspace(&mut self, layout: Layout) {
        if self.tree.is_empty() {
            self.set_workspace_layout_hint(layout);
            return;
        }

        if self.workspace_layout == layout {
            return;
        }

        if self
            .tree
            .wrap_synthetic_root_children_for_workspace_layout(layout)
        {
            self.set_workspace_layout_hint(layout);
            self.tree.layout();
            return;
        }

        self.tree.set_workspace_layout_hint(layout);
        if self.tree.wrap_root_for_sibling_insert() {
            self.set_workspace_layout_hint(layout);
            self.tree.layout();
            return;
        }

        self.set_workspace_layout_hint(layout);
        if self.tree.split_focused(layout) {
            self.tree.layout();
        }
    }

    /// Set layout mode for focused container
    pub fn set_layout_mode(&mut self, layout: Layout) {
        if self.set_layout_for_active_selection(layout) {
            self.tree.layout();
        }
    }

    /// Set workspace-level layout target (root container) like sway workspace path.
    pub fn set_workspace_layout_mode(&mut self, layout: Layout) {
        if self.tree.is_empty() {
            self.set_workspace_layout_hint(layout);
            return;
        }

        if self.workspace_layout == layout {
            return;
        }

        if self
            .tree
            .wrap_synthetic_root_children_for_workspace_layout(layout)
        {
            self.set_workspace_layout_hint(layout);
            self.tree.layout();
            return;
        }

        self.tree.set_workspace_layout_hint(layout);
        if self.tree.wrap_root_for_sibling_insert() {
            self.set_workspace_layout_hint(layout);
            self.tree.layout();
            return;
        }

        self.set_workspace_layout_hint(layout);
        if self.tree.set_focused_layout(layout) {
            self.tree.layout();
        }
    }

    pub fn set_root_layout_mode(&mut self, layout: Layout) -> bool {
        let changed = self.tree.set_root_container_layout(layout);
        if changed {
            self.tree.layout();
        }
        changed
    }

    pub fn collapse_redundant_root_single_child_split(&mut self) -> bool {
        let changed = self.tree.collapse_redundant_root_single_child_split();
        if changed {
            self.tree.layout();
        }
        changed
    }

    /// Toggle between horizontal and vertical split for the focused container.
    pub fn toggle_split_layout(&mut self) {
        if self.toggle_split_for_active_selection() {
            self.tree.layout();
        }
    }

    pub fn toggle_workspace_split_layout(&mut self) {
        let next = match self.workspace_layout {
            Layout::SplitH => Layout::SplitV,
            Layout::SplitV => Layout::SplitH,
            Layout::Tabbed | Layout::Stacked => {
                self.workspace_prev_split_layout.unwrap_or(Layout::SplitH)
            }
        };
        self.set_workspace_layout_hint(next);
    }

    /// Cycle focused container layout in sway-style order.
    pub fn toggle_layout_all(&mut self) {
        if self.toggle_layout_all_for_active_selection() {
            self.tree.layout();
        }
    }

    pub fn toggle_workspace_layout_all(&mut self) {
        let next = Self::next_layout_all(self.workspace_layout);
        self.set_workspace_layout_mode(next);
    }

    /// Set the width of the currently focused root-level column
    pub fn set_column_width(&mut self, change: SizeChange) {
        let Some(idx) = self.tree.focused_root_index() else {
            return;
        };

        let Some((layout, rect, child_count)) = self.tree.container_info(&[]) else {
            return;
        };
        if layout != Layout::SplitH || child_count == 0 {
            return;
        }

        let gaps = self.options.layout.gaps;
        let available_width = (rect.size.w - gaps * (child_count as f64 - 1.0)).max(1.0);
        if available_width <= 0.0 {
            return;
        }

        let current_percent = self.tree.child_percent_at(&[], idx).unwrap_or(1.0);
        let new_percent = Self::percent_from_size_change(current_percent, available_width, change);

        if self
            .tree
            .set_child_percent_at(&[], idx, Layout::SplitH, new_percent)
        {
            self.tree.layout();
        }
    }
    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        let Some(path) = self.window_path(window) else {
            return;
        };

        let Some((parent_path, _, _, _child_count, _rect)) =
            self.window_container_metrics(&path, Layout::SplitV)
        else {
            return;
        };

        if let Some(container) = self.tree.container_at_path_mut(parent_path.as_slice()) {
            if container.layout() == Layout::SplitV {
                container.recalculate_percentages();
                self.tree.layout();
            }
        }
    }

    /// Toggle fullscreen state for a window
    pub fn toggle_fullscreen(&mut self, window: &W) {
        let currently = self.is_fullscreen(window);
        let _ = self.set_fullscreen(window.id(), !currently);
    }
    pub fn toggle_width(&mut self, forwards: bool) {
        let Some(idx) = self.tree.focused_root_index() else {
            return;
        };

        let Some((layout, rect, child_count)) = self.tree.container_info(&[]) else {
            return;
        };
        if layout != Layout::SplitH || child_count == 0 {
            return;
        }

        let available = self.available_span(rect.size.w, child_count);
        if available <= 0.0 {
            return;
        }

        let current_percent = self.tree.child_percent_at(&[], idx).unwrap_or(1.0);
        let presets = &self.options.layout.preset_column_widths;

        if let Some(percent) = self.cycle_presets(available, current_percent, presets, forwards) {
            if self
                .tree
                .set_child_percent_at(&[], idx, Layout::SplitH, percent)
            {
                self.tree.layout();
            }
        }
    }

    #[cfg(test)]
    pub fn view_pos(&self) -> f64 {
        self.viewport.position()
    }

    #[cfg(test)]
    pub fn active_column_idx(&self) -> usize {
        self.tree.focused_root_index().unwrap_or(0)
    }

    fn layout_area(&self) -> Rectangle<f64, Logical> {
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

    const DROP_LAYOUT_BORDER: f64 = 30.0;
    const DROP_CENTER_RATIO: f64 = 0.3;

    fn closest_edge(rect: Rectangle<f64, Logical>, pos: Point<f64, Logical>) -> (Direction, f64) {
        let left = (pos.x - rect.loc.x).abs();
        let right = (rect.loc.x + rect.size.w - pos.x).abs();
        let top = (pos.y - rect.loc.y).abs();
        let bottom = (rect.loc.y + rect.size.h - pos.y).abs();

        let mut dir = Direction::Left;
        let mut min = left;

        if right < min {
            min = right;
            dir = Direction::Right;
        }
        if top < min {
            min = top;
            dir = Direction::Up;
        }
        if bottom < min {
            min = bottom;
            dir = Direction::Down;
        }

        (dir, min)
    }

    fn leaf_rect_for_path(&self, path: &[usize]) -> Option<Rectangle<f64, Logical>> {
        let scale = Scale::from(self.scale);
        let info = self
            .display_layouts()
            .iter()
            .find(|info| info.path == path)?;
        let tile = self.tree.get_tile(info.key)?;
        let mut tile_pos = info.rect.loc + tile.render_offset();
        tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
        Some(Rectangle::new(tile_pos, tile.tile_size()))
    }

    fn closest_leaf_rect(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Vec<usize>, Rectangle<f64, Logical>)> {
        let scale = Scale::from(self.scale);
        let fullscreen_id = self.render_fullscreen_window();
        let windowed_fullscreen_id = if fullscreen_id.is_none() {
            self.tree.focused_tile().and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };
        let has_fullscreen_like = fullscreen_id.is_some() || windowed_fullscreen_id.is_some();

        let mut nearest: Option<(Vec<usize>, Rectangle<f64, Logical>, f64)> = None;

        for info in self.display_layouts() {
            if let Some(tile) = self.tree.get_tile(info.key) {
                let is_fullscreen_tile = fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_windowed_fullscreen_tile = windowed_fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_fullscreen_like_tile = is_fullscreen_tile || is_windowed_fullscreen_tile;
                if has_fullscreen_like && !is_fullscreen_like_tile {
                    continue;
                }
                if !info.visible && !is_fullscreen_like_tile {
                    continue;
                }

                let base_pos = if is_fullscreen_like_tile {
                    Point::from((0.0, 0.0))
                } else {
                    info.rect.loc
                };
                let mut tile_pos = base_pos + tile.render_offset();
                tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);
                let tile_rect = Rectangle::new(tile_pos, tile.tile_size());

                if tile_rect.contains(pos) {
                    return Some((info.path.clone(), tile_rect));
                }

                let dx = if pos.x < tile_rect.loc.x {
                    tile_rect.loc.x - pos.x
                } else if pos.x > tile_rect.loc.x + tile_rect.size.w {
                    pos.x - (tile_rect.loc.x + tile_rect.size.w)
                } else {
                    0.0
                };
                let dy = if pos.y < tile_rect.loc.y {
                    tile_rect.loc.y - pos.y
                } else if pos.y > tile_rect.loc.y + tile_rect.size.h {
                    pos.y - (tile_rect.loc.y + tile_rect.size.h)
                } else {
                    0.0
                };
                let dist2 = dx * dx + dy * dy;

                let replace = nearest.as_ref().is_none_or(|(_, _, best)| dist2 < *best);
                if replace {
                    nearest = Some((info.path.clone(), tile_rect, dist2));
                }
            }
        }

        nearest.map(|(path, rect, _)| (path, rect))
    }

    fn indicator_rect(
        rect: Rectangle<f64, Logical>,
        direction: Direction,
        thickness: f64,
    ) -> Rectangle<f64, Logical> {
        let thickness = thickness.max(1.0);
        match direction {
            Direction::Left => Rectangle::new(
                rect.loc,
                Size::from((thickness.min(rect.size.w), rect.size.h)),
            ),
            Direction::Right => Rectangle::new(
                Point::from((
                    rect.loc.x + rect.size.w - thickness.min(rect.size.w),
                    rect.loc.y,
                )),
                Size::from((thickness.min(rect.size.w), rect.size.h)),
            ),
            Direction::Up => Rectangle::new(
                rect.loc,
                Size::from((rect.size.w, thickness.min(rect.size.h))),
            ),
            Direction::Down => Rectangle::new(
                Point::from((
                    rect.loc.x,
                    rect.loc.y + rect.size.h - thickness.min(rect.size.h),
                )),
                Size::from((rect.size.w, thickness.min(rect.size.h))),
            ),
        }
    }

    fn inset_rect(rect: Rectangle<f64, Logical>, inset: f64) -> Rectangle<f64, Logical> {
        let inset = inset.min(rect.size.w / 2.0).min(rect.size.h / 2.0).max(0.0);
        Rectangle::new(
            Point::from((rect.loc.x + inset, rect.loc.y + inset)),
            Size::from((rect.size.w - 2.0 * inset, rect.size.h - 2.0 * inset)),
        )
    }

    /// Determine insert position from pointer location
    pub(super) fn insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        if self.tree.is_empty() {
            return InsertPosition::NewColumn(0);
        }

        let layout_area = self.layout_area();
        if pos.y < layout_area.loc.y + Self::DROP_LAYOUT_BORDER {
            return InsertPosition::SplitRoot {
                direction: Direction::Up,
                indicator: SplitIndicator::LayoutBorder,
            };
        }
        if pos.y > layout_area.loc.y + layout_area.size.h - Self::DROP_LAYOUT_BORDER {
            return InsertPosition::SplitRoot {
                direction: Direction::Down,
                indicator: SplitIndicator::LayoutBorder,
            };
        }

        let Some((path, rect)) = self.closest_leaf_rect(pos) else {
            return InsertPosition::NewColumn(0);
        };

        let parent_layout = self
            .tree
            .parent_layout_for_path(&path)
            .unwrap_or(Layout::SplitH);

        if matches!(parent_layout, Layout::SplitH | Layout::Tabbed) {
            if pos.y < rect.loc.y + Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    path,
                    direction: Direction::Up,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
            if pos.y > rect.loc.y + rect.size.h - Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    path,
                    direction: Direction::Down,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
        } else if matches!(parent_layout, Layout::SplitV | Layout::Stacked) {
            if pos.x < rect.loc.x + Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    path,
                    direction: Direction::Left,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
            if pos.x > rect.loc.x + rect.size.w - Self::DROP_LAYOUT_BORDER {
                return InsertPosition::Split {
                    path,
                    direction: Direction::Right,
                    indicator: SplitIndicator::LayoutBorder,
                };
            }
        }

        let (direction, dist) = Self::closest_edge(rect, pos);
        let thickness = f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO;
        if dist > thickness {
            InsertPosition::Swap { path, direction }
        } else {
            InsertPosition::Split {
                path,
                direction,
                indicator: SplitIndicator::Center,
            }
        }
    }

    /// Get hint area for insertion position
    pub(super) fn insert_hint_area(
        &self,
        position: &InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        match position {
            InsertPosition::NewColumn(_) => Some(self.layout_area()),
            InsertPosition::Swap { path, .. } => {
                let rect = self.leaf_rect_for_path(path)?;
                let thickness = f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO;
                Some(Self::inset_rect(rect, thickness))
            }
            InsertPosition::Split {
                path,
                direction,
                indicator,
            } => {
                let rect = self.leaf_rect_for_path(path)?;
                let thickness = match indicator {
                    SplitIndicator::LayoutBorder => Self::DROP_LAYOUT_BORDER,
                    SplitIndicator::Center => {
                        f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO
                    }
                };
                Some(Self::indicator_rect(rect, *direction, thickness))
            }
            InsertPosition::SplitRoot {
                direction,
                indicator,
            } => {
                let rect = self.layout_area();
                let thickness = match indicator {
                    SplitIndicator::LayoutBorder => Self::DROP_LAYOUT_BORDER,
                    SplitIndicator::Center => {
                        f64::min(rect.size.w, rect.size.h) * Self::DROP_CENTER_RATIO
                    }
                };
                Some(Self::indicator_rect(rect, *direction, thickness))
            }
            InsertPosition::Floating => None,
        }
    }

    // Window queries
    fn tab_bar_hit(&self, pos: Point<f64, Logical>) -> Option<(&W, super::HitType)> {
        if self.render_fullscreen_window().is_some() || self.options.layout.tab_bar.off {
            return None;
        }

        let scale = Scale::from(self.scale);
        let tab_bar_infos = self.tree.tab_bar_layouts();
        if tab_bar_infos.is_empty() {
            return None;
        }

        let cache = self.tab_bar_cache.borrow();
        for info in tab_bar_infos {
            let tab_count = info.tabs.len();
            if tab_count == 0 {
                continue;
            }

            let bar_loc_px: Point<i32, Physical> = info.rect.loc.to_physical_precise_round(scale);
            let pos_px: Point<i32, Physical> = pos.to_physical_precise_round(scale) - bar_loc_px;
            let width_px = to_physical_precise_round::<i32>(self.scale, info.rect.size.w).max(1);
            let height_px = to_physical_precise_round::<i32>(self.scale, info.rect.size.h).max(1);

            if pos_px.x < 0 || pos_px.y < 0 || pos_px.x >= width_px || pos_px.y >= height_px {
                continue;
            }

            let row_height_px =
                to_physical_precise_round::<i32>(self.scale, info.row_height).max(1);
            let focused_idx = info.tabs.iter().position(|tab| tab.is_focused).unwrap_or(0);

            let tab_idx = match info.layout {
                Layout::Tabbed => {
                    if pos_px.y >= row_height_px {
                        focused_idx
                    } else if let Some(widths) = cache.get(&info.path).and_then(|entry| {
                        if entry.tab_widths_px.len() == tab_count {
                            Some(entry.tab_widths_px.as_slice())
                        } else {
                            None
                        }
                    }) {
                        let mut cursor = 0;
                        let mut found = None;
                        for (idx, width) in widths.iter().enumerate() {
                            let end = cursor + *width;
                            if pos_px.x < end {
                                found = Some(idx);
                                break;
                            }
                            cursor = end;
                        }
                        found.unwrap_or_else(|| tab_count.saturating_sub(1))
                    } else {
                        let base = width_px / tab_count as i32;
                        let mut cursor = 0;
                        let mut found = None;
                        for idx in 0..tab_count {
                            let mut width = base;
                            if idx + 1 == tab_count {
                                width += width_px - base * tab_count as i32;
                            }
                            let end = cursor + width;
                            if pos_px.x < end {
                                found = Some(idx);
                                break;
                            }
                            cursor = end;
                        }
                        found.unwrap_or_else(|| tab_count.saturating_sub(1))
                    }
                }
                Layout::Stacked => {
                    let stack_height_px = row_height_px * tab_count as i32;
                    if pos_px.y >= stack_height_px {
                        focused_idx
                    } else {
                        let max_idx = tab_count.saturating_sub(1) as i32;
                        (pos_px.y / row_height_px).min(max_idx) as usize
                    }
                }
                _ => continue,
            };

            if let Some(window) = self.tree.window_for_tab(&info.path, tab_idx) {
                return Some((
                    window,
                    super::HitType::Activate {
                        is_tab_indicator: true,
                    },
                ));
            }
        }

        None
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, super::HitType)> {
        let scale = Scale::from(self.scale);
        let fullscreen_id = self.render_fullscreen_window();
        let windowed_fullscreen_id = if fullscreen_id.is_none() {
            self.tree.focused_tile().and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };
        let has_fullscreen_like = fullscreen_id.is_some() || windowed_fullscreen_id.is_some();

        if let Some(hit) = self.tab_bar_hit(pos) {
            return Some(hit);
        }

        let render_layouts = self.display_layouts();
        for info in render_layouts.iter().rev() {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile(info.key) {
                let is_fullscreen_tile = fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_windowed_fullscreen_tile = windowed_fullscreen_id
                    .as_ref()
                    .is_some_and(|id| id == tile.window().id());
                let is_fullscreen_like_tile = is_fullscreen_tile || is_windowed_fullscreen_tile;
                if has_fullscreen_like && !is_fullscreen_like_tile {
                    continue;
                }
                if !info.visible && !is_fullscreen_like_tile {
                    continue;
                }

                // Fullscreen-like tiles are rendered relative to the workspace origin.
                let base_pos = if is_fullscreen_like_tile {
                    Point::from((0.0, 0.0))
                } else {
                    info.rect.loc
                };
                let mut tile_pos = base_pos + tile.render_offset();
                tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                if let Some(hit) = super::HitType::hit_tile(tile, tile_pos, pos) {
                    return Some(hit);
                }
            }
        }

        None
    }

    pub fn window_loc(&self, window: &W) -> Option<Point<f64, Logical>> {
        let path = self.tree.find_window(window.id())?;
        let layouts = self.display_layouts();
        let info = layouts.iter().find(|layout| layout.path == path)?;
        let tile = self.tree.tile_at_path(&path)?;
        let scale = Scale::from(self.scale);

        let mut tile_pos = info.rect.loc + tile.render_offset();
        tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

        Some(tile_pos + tile.window_loc())
    }

    pub fn window_size(&self, window: &W) -> Option<Size<f64, Logical>> {
        let path = self.tree.find_window(window.id())?;
        let tile = self.tree.tile_at_path(&path)?;
        Some(tile.window_size())
    }

    pub fn is_fullscreen(&self, window: &W) -> bool {
        self.fullscreen_window
            .as_ref()
            .is_some_and(|id| id == window.id())
    }

    pub fn has_fullscreen_window(&self) -> bool {
        self.fullscreen_window.is_some()
    }

    /// Set the display mode for the focused container
    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        let layout = match display {
            ColumnDisplay::Normal => Layout::SplitV,
            ColumnDisplay::Tabbed => Layout::Tabbed,
        };

        if self.set_layout_for_active_selection(layout) {
            self.tree.layout();
        }
    }

    /// Toggle between tabbed and normal (split) layout for focused container
    pub fn toggle_column_tabbed_display(&mut self) {
        let current = self.active_selection_layout();
        let target = match current {
            Some(Layout::Tabbed) => Layout::SplitV,
            _ => Layout::Tabbed,
        };

        if self.set_layout_for_active_selection(target) {
            self.tree.layout();
        }
    }

    // Additional methods needed by workspace.rs
    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        TilePtrIterMut::new(self.tree.tile_ptrs_mut())
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        TileRenderPositions::new(self)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        TileRenderPositionsMut::new(self, round)
    }

    pub fn tiles_with_ipc_layouts(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, tiri_ipc::WindowLayout)> + '_ {
        let scale = Scale::from(self.scale);
        let legacy_positions = self.legacy_tiling_positions();

        self.tree.leaf_layouts().iter().filter_map(move |info| {
            let tile = self.tree.tile_at_path(&info.path)?;
            let mut layout = tile.ipc_layout_template();
            let tile_size = tile.tile_size();
            layout.tile_size = (tile_size.w, tile_size.h);
            let window_size = tile.window_size().to_i32_round();
            layout.window_size = (window_size.w, window_size.h);
            let mut pos = info.rect.loc + tile.render_offset();
            pos = pos.to_physical_precise_round(scale).to_logical(scale);
            layout.tile_pos_in_workspace_view = Some((pos.x, pos.y));
            let window_offset = tile.window_loc();
            layout.window_offset_in_tile = (window_offset.x, window_offset.y);
            layout.pos_in_tiling_layout = legacy_positions.get(&info.path).copied();
            Some((tile, layout))
        })
    }

    fn legacy_tiling_positions(&self) -> HashMap<Vec<usize>, (usize, usize)> {
        let mut positions = HashMap::new();

        if self.tree.root_children_len() == 0 {
            return positions;
        }

        if self.tree.root_container().is_none() {
            positions.insert(Vec::new(), (1, 1));
            return positions;
        }

        for root_idx in 0..self.tree.root_children_len() {
            for (leaf_idx, path) in self
                .tree
                .leaf_paths_under(&[root_idx])
                .into_iter()
                .enumerate()
            {
                positions.insert(path, (root_idx + 1, leaf_idx + 1));
            }
        }

        positions
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tiles().any(|tile| tile.are_transitions_ongoing()) || !self.closing_windows.is_empty()
    }

    pub fn update_shaders(&mut self) {
        for tile in self.tiles_mut() {
            tile.update_shaders();
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        self.tree.focused_window()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn clear_pending_layout_hint(&mut self) {
        self.tree.clear_pending_layout();
    }

    pub fn set_pending_layout_hint(&mut self, layout: Layout) {
        self.tree.set_pending_layout(layout);
    }

    pub fn set_workspace_layout_hint(&mut self, layout: Layout) {
        self.update_workspace_layout_state(layout);
        self.tree.set_workspace_layout_hint(layout);
    }

    pub fn focus_is_root_leaf(&self) -> bool {
        self.tree.focus_path().is_empty()
    }

    pub fn add_tile(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        _width: ColumnWidth,
        _is_full_width: bool,
        _height: Option<WindowHeight>,
    ) {
        if let Some(index) = col_idx {
            self.tree.insert_leaf_at(index, tile, activate);
        } else {
            self.tree.insert_window_with_focus(tile, activate);
        }
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_tile_right_of(
        &mut self,
        next_to: &W::Id,
        tile: Tile<W>,
        activate: bool,
        _width: ColumnWidth,
        _is_full_width: bool,
    ) {
        self.tree.insert_leaf_after(next_to, tile, activate);
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_tile_to_root_container(
        &mut self,
        root_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        if self
            .tree
            .insert_leaf_in_root_container(root_idx, tile_idx, tile, activate)
        {
            self.sync_fullscreen_window();
            self.tree.layout();
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.add_tile_to_root_container(col_idx, tile_idx, tile, activate);
    }

    pub(super) fn insert_subtree_with_parent_info(
        &mut self,
        info: &InsertParentInfo,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        self.tree
            .insert_subtree_with_parent_info(info, subtree, focus);
        self.tree.layout();
    }

    pub fn insert_subtree_at_root(&mut self, index: usize, subtree: DetachedNode<W>, focus: bool) {
        self.tree.insert_subtree_at_root(index, subtree, focus);
        self.tree.layout();
    }

    pub fn insert_subtree_with_focus(&mut self, subtree: DetachedNode<W>, focus: bool) {
        self.tree.insert_subtree_with_focus(subtree, focus);
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_subtree_as_workspace_tiling_fallback(
        &mut self,
        subtree: DetachedNode<W>,
        focus: bool,
    ) {
        if self.tree.is_empty() {
            self.tree.insert_subtree_with_focus(subtree, focus);
        } else {
            let index = self.tree.root_children_len();
            self.tree.insert_subtree_at_root(index, subtree, focus);
        }
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_tile_as_workspace_tiling_fallback(&mut self, tile: Tile<W>, activate: bool) {
        if self.tree.is_empty() {
            self.tree.insert_window_with_focus(tile, activate);
        } else {
            let index = self.tree.root_children_len();
            self.tree.insert_leaf_at(index, tile, activate);
        }
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub(super) fn insert_parent_info_for_window(
        &self,
        window: &W::Id,
    ) -> Option<super::container::InsertParentInfo> {
        self.tree.insert_parent_info_for_window(window)
    }

    pub(super) fn replace_tile_at_path(
        &mut self,
        path: &[usize],
        tile: Tile<W>,
    ) -> Option<Tile<W>> {
        self.tree.replace_leaf_at_path(path, tile)
    }

    pub(super) fn is_leaf_at_path(&self, path: &[usize]) -> bool {
        self.tree.is_leaf_at_path(path)
    }

    pub(super) fn insert_tile_with_parent_info(
        &mut self,
        info: &super::container::InsertParentInfo,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        if self.tree.insert_leaf_with_parent_info(info, tile, activate) {
            self.sync_fullscreen_window();
            self.tree.layout();
            return true;
        }

        false
    }

    pub fn insert_tile_split(
        &mut self,
        target_path: &[usize],
        direction: Direction,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        if self
            .tree
            .insert_leaf_split(target_path, direction, tile, activate)
        {
            self.sync_fullscreen_window();
            self.tree.layout();
            return true;
        }

        false
    }

    pub fn insert_tile_split_root(
        &mut self,
        direction: Direction,
        tile: Tile<W>,
        activate: bool,
    ) -> bool {
        if self.tree.insert_leaf_split_root(direction, tile, activate) {
            self.sync_fullscreen_window();
            self.tree.layout();
            return true;
        }

        false
    }

    pub fn active_tile_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let focus_path = self.tree.focus_path();
        self.tree
            .leaf_layouts()
            .iter()
            .find(|info| info.path == focus_path)
            .and_then(|info| {
                let mut rect = info.rect;
                let tile = self.tree.tile_at_path(&info.path)?;
                rect.loc += tile.render_offset();
                Some(rect)
            })
    }

    /// Get mutable reference to the currently focused tile
    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        self.tree.focused_tile_mut()
    }

    pub fn add_root_tiling_subtree(
        &mut self,
        root_idx: Option<usize>,
        subtree: RootTilingSubtree<W>,
        activate: bool,
        _height: Option<WindowHeight>,
    ) {
        let idx = root_idx.unwrap_or_else(|| self.tree.root_children_len());
        self.tree
            .insert_subtree_at_root(idx, subtree.into_subtree(), activate);
        self.sync_fullscreen_window();
        self.tree.layout();
    }

    pub fn add_column(
        &mut self,
        col_idx: Option<usize>,
        column: Column<W>,
        activate: bool,
        height: Option<WindowHeight>,
    ) {
        self.add_root_tiling_subtree(col_idx, column.into(), activate, height);
    }
    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        self.tree.set_pending_transaction(transaction.clone());
        let tile = self
            .tree
            .remove_window(window)
            .expect("attempted to remove missing window");

        if self
            .fullscreen_window
            .as_ref()
            .is_some_and(|id| id == window)
        {
            self.fullscreen_window = None;
        }

        RemovedTile {
            tile,
            width: ColumnWidth::default(),
            is_full_width: false,
            is_floating: false,
        }
    }
    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let id = self.tree.focused_tile()?.window().id().clone();
        let removed = self.remove_tile(&id, transaction);
        if self
            .fullscreen_window
            .as_ref()
            .is_some_and(|win_id| win_id == &id)
        {
            self.fullscreen_window = None;
        }
        Some(removed)
    }
    pub fn remove_active_root_tiling_subtree(&mut self) -> Option<RootTilingSubtree<W>> {
        let idx = self.tree.focused_root_index()?;
        let subtree = self.tree.take_root_child_subtree(idx)?;
        let subtree = RootTilingSubtree::from_subtree(subtree);

        if let Some(full_id) = self.fullscreen_window.clone() {
            if self.tree.find_window(&full_id).is_none() {
                self.fullscreen_window = None;
            }
        }

        self.tree.layout();
        Some(subtree)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        self.remove_active_root_tiling_subtree().map(Into::into)
    }

    pub fn new_window_size(
        &self,
        _width: Option<PresetSize>,
        _height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let Some(preview) = self.tree.preview_new_leaf_geometry() else {
            return Size::from((800, 600));
        };

        let mut size = preview.rect.size;
        let mut border_config = self.options.layout.border.merged_with(&rules.border);
        border_config.width = round_logical_in_physical_max1(self.scale, border_config.width);

        if !border_config.off {
            let width = border_config.width * 2.0;
            size.w = f64::max(1.0, size.w - width);
            size.h = f64::max(1.0, size.h - width);
        }
        if preview.tab_bar_offset > 0.0 {
            size.h = f64::max(1.0, size.h - preview.tab_bar_offset);
        }

        size.to_i32_floor()
    }

    pub fn new_window_toplevel_bounds(&self, _rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        Size::from((800, 600))
    }

    pub fn focus_root_container_first(&mut self) {
        self.tree.focus_root_child(0);
        self.tree.layout();
    }

    pub fn focus_first_leaf(&mut self) {
        if self.tree.focus_leaf_in_root_child(0, 1) {
            self.tree.layout();
        } else if self.tree.focus_root_child(0) {
            self.tree.layout();
        }
    }

    pub fn focus_root_container_last(&mut self) {
        let len = self.tree.root_children_len();
        if len > 0 {
            self.tree.focus_root_child(len - 1);
            self.tree.layout();
        }
    }

    /// Root containers are 1-based to match user-facing commands.
    pub fn focus_root_container(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        self.tree.focus_root_child(idx - 1);
        self.tree.layout();
    }

    /// Leaves inside the current root container are 1-based.
    pub fn focus_leaf_in_root_container(&mut self, index: u8) {
        if index == 0 {
            return;
        }
        let root_idx = match self.tree.focused_root_index() {
            Some(idx) => idx,
            None => return,
        };
        self.tree.focus_leaf_in_root_child(root_idx, index as usize);
        self.tree.layout();
    }

    pub fn focus_column_first(&mut self) {
        self.focus_root_container_first();
    }

    pub fn focus_column_last(&mut self) {
        self.focus_root_container_last();
    }

    pub fn focus_column(&mut self, idx: usize) {
        self.focus_root_container(idx);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        self.focus_leaf_in_root_container(index);
    }

    pub fn focus_down_or_left(&mut self) {
        let focused = self.tree.focus_in_direction(Direction::Down)
            || self.tree.focus_in_direction(Direction::Left);
        if focused {
            self.tree.layout();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        let focused = self.tree.focus_in_direction(Direction::Down)
            || self.tree.focus_in_direction(Direction::Right);
        if focused {
            self.tree.layout();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        let focused = self.tree.focus_in_direction(Direction::Up)
            || self.tree.focus_in_direction(Direction::Left);
        if focused {
            self.tree.layout();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        let focused = self.tree.focus_in_direction(Direction::Up)
            || self.tree.focus_in_direction(Direction::Right);
        if focused {
            self.tree.layout();
        }
    }

    pub fn focus_top(&mut self) {
        self.tree.focus_top_in_current_column();
    }

    pub fn focus_bottom(&mut self) {
        self.tree.focus_bottom_in_current_column();
    }

    fn move_root_child_with_layout(&mut self, current: usize, target: usize) -> bool {
        if current == target {
            return false;
        }
        if target >= self.tree.root_children_len() {
            return false;
        }
        let moved = self.tree.move_root_child(current, target);
        if moved {
            self.tree.layout();
        }
        moved
    }

    pub fn move_root_container_to_first(&mut self) {
        if let Some(idx) = self.tree.focused_root_index() {
            self.move_root_child_with_layout(idx, 0);
        }
    }

    pub fn move_root_container_to_last(&mut self) {
        let len = self.tree.root_children_len();
        if len == 0 {
            return;
        }
        if let Some(idx) = self.tree.focused_root_index() {
            self.move_root_child_with_layout(idx, len - 1);
        }
    }

    pub fn move_root_container_left(&mut self) -> bool {
        let Some(idx) = self.tree.focused_root_index() else {
            return false;
        };
        if idx == 0 {
            return false;
        }

        self.move_root_child_with_layout(idx, idx - 1)
    }

    pub fn move_root_container_right(&mut self) -> bool {
        let Some(idx) = self.tree.focused_root_index() else {
            return false;
        };
        let len = self.tree.root_children_len();
        if idx + 1 >= len {
            return false;
        }

        self.move_root_child_with_layout(idx, idx + 1)
    }

    pub fn move_root_container_to_index(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        let target = idx - 1;
        if let Some(current) = self.tree.focused_root_index() {
            if current == target {
                return;
            }
            self.move_root_child_with_layout(current, target);
        }
    }

    pub fn move_column_to_first(&mut self) {
        self.move_root_container_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        self.move_root_container_to_last();
    }

    pub fn move_column_left(&mut self) -> bool {
        self.move_root_container_left()
    }

    pub fn move_column_right(&mut self) -> bool {
        self.move_root_container_right()
    }

    pub fn move_column_to_index(&mut self, idx: usize) {
        self.move_root_container_to_index(idx);
    }

    fn consume_or_expel_window(&mut self, window: Option<&W::Id>, direction: Direction) {
        if let Some(id) = window {
            self.tree.focus_window_by_id(id);
        }

        if !self.move_command_target(direction) {
            self.tree.split_focused(Layout::SplitV);
            self.tree.layout();
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        self.consume_or_expel_window(window, Direction::Left);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        self.consume_or_expel_window(window, Direction::Right);
    }

    pub fn toggle_full_width(&mut self) {
        let Some(tile) = self.tree.focused_tile() else {
            return;
        };
        let id = tile.window().id().clone();
        let currently_fullscreen = self
            .fullscreen_window
            .as_ref()
            .is_some_and(|win_id| win_id == tile.window().id());
        let _ = self.set_fullscreen(&id, !currently_fullscreen);
    }

    fn toggle_window_dimension(
        &mut self,
        window: Option<&W::Id>,
        layout: Layout,
        presets: &[PresetSize],
        forwards: bool,
    ) {
        let Some(path) = self.window_path(window) else {
            return;
        };
        let Some((parent_path, child_idx, available, _, _)) =
            self.window_container_metrics(&path, layout)
        else {
            return;
        };
        let current_percent = self
            .tree
            .child_percent_at(parent_path.as_slice(), child_idx)
            .unwrap_or(1.0);

        if let Some(percent) = self.cycle_presets(available, current_percent, presets, forwards) {
            if self
                .tree
                .set_child_percent_at(parent_path.as_slice(), child_idx, layout, percent)
            {
                self.tree.layout();
            }
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        let presets = self.options.layout.preset_window_heights.clone();
        self.toggle_window_dimension(window, Layout::SplitV, &presets, forwards);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        let presets = self.options.layout.preset_column_widths.clone();
        self.toggle_window_dimension(window, Layout::SplitH, &presets, forwards);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        let Some(path) = self.window_path(window) else {
            return;
        };
        let Some((parent_path, child_idx, available, _, _)) =
            self.window_container_metrics(&path, Layout::SplitH)
        else {
            return;
        };

        let current_percent = self
            .tree
            .child_percent_at(parent_path.as_slice(), child_idx)
            .unwrap_or(1.0);
        let percent = Self::percent_from_size_change(current_percent, available, change);

        if self.tree.set_child_percent_at(
            parent_path.as_slice(),
            child_idx,
            Layout::SplitH,
            percent,
        ) {
            self.tree.layout();
        }
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        let Some(path) = self.window_path(window) else {
            return;
        };
        let Some((parent_path, child_idx, available, _, _)) =
            self.window_container_metrics(&path, Layout::SplitV)
        else {
            return;
        };

        let current_percent = self
            .tree
            .child_percent_at(parent_path.as_slice(), child_idx)
            .unwrap_or(1.0);
        let percent = Self::percent_from_size_change(current_percent, available, change);

        if self.tree.set_child_percent_at(
            parent_path.as_slice(),
            child_idx,
            Layout::SplitV,
            percent,
        ) {
            self.tree.layout();
        }
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let selected_container = self.tree.selected_container_key();

        if is_fullscreen {
            if self
                .fullscreen_window
                .as_ref()
                .is_some_and(|id| id == window)
            {
                return false;
            }

            let already_focused = self
                .tree
                .focused_window()
                .is_some_and(|focused| focused.id() == window);
            if selected_container.is_none()
                && !already_focused
                && !self.tree.focus_window_by_id(window)
            {
                return false;
            }

            if let Some(path) = self.tree.find_window(window) {
                if let Some(tile) = self.tree.tile_at_path_mut(&path) {
                    tile.pending_maximized |= tile.window().pending_sizing_mode().is_maximized();
                    tile.request_fullscreen(!self.options.animations.off, None);
                }
            }

            self.fullscreen_window = Some(window.clone());
            self.tree.layout();
            if let Some(selected_key) = selected_container {
                self.tree.set_selected_container_key(selected_key);
            }
            true
        } else {
            let Some(path) = self.tree.find_window(window) else {
                return false;
            };
            let Some(tile) = self.tree.tile_at_path_mut(&path) else {
                return false;
            };
            let is_window_fullscreen = tile.window().pending_sizing_mode().is_fullscreen();
            let fullscreen_matches = self
                .fullscreen_window
                .as_ref()
                .is_some_and(|id| id == window);
            if !is_window_fullscreen && !fullscreen_matches {
                return false;
            }

            if tile.pending_maximized {
                tile.request_maximized(self.working_area.size, !self.options.animations.off, None);
            } else {
                tile.request_tile_size(self.working_area.size, !self.options.animations.off, None);
            }

            self.fullscreen_window = None;
            self.tree.layout();
            if let Some(selected_key) = selected_container {
                self.tree.set_selected_container_key(selected_key);
            }
            true
        }
    }

    fn sync_fullscreen_window(&mut self) {
        if let Some(id) = self.fullscreen_window.as_ref() {
            // Keep compositor-level fullscreen sticky while the tracked window still exists.
            // This matches sway behavior better than relying on pending_sizing_mode() snapshots.
            if self.tree.find_window(id).is_some() {
                return;
            }
        }

        let next_fullscreen = self
            .tiles()
            .find(|tile| tile.window().pending_sizing_mode().is_fullscreen())
            .map(|tile| tile.window().id().clone());
        self.fullscreen_window = next_fullscreen;
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let Some(path) = self.tree.find_window(window) else {
            return false;
        };
        let Some(tile) = self.tree.tile_at_path_mut(&path) else {
            return false;
        };

        tile.pending_maximized = maximize;
        self.tree.layout();
        true
    }

    pub fn center_column(&mut self) {}
    pub fn center_window(&mut self, _window: Option<&W::Id>) {}
    pub fn center_visible_columns(&mut self) {}

    pub fn expand_column_to_available_width(&mut self) {
        let Some(idx) = self.tree.focused_root_index() else {
            return;
        };
        if self
            .tree
            .set_child_percent_at(&[], idx, Layout::SplitH, 1.0)
        {
            self.tree.layout();
        }
    }

    pub fn swap_window_in_direction(&mut self, direction: Direction) {
        self.move_command_target(direction);
    }

    pub fn start_open_animation(&mut self, _id: &W::Id) -> bool {
        let Some(path) = self.tree.find_window(_id) else {
            return false;
        };
        if let Some(tile) = self.tree.tile_at_path_mut(&path) {
            tile.start_open_animation();
            return true;
        }
        false
    }
    pub fn start_close_animation_for_window<R: NiriRenderer>(
        &mut self,
        renderer: &mut R,
        window: &W::Id,
        blocker: crate::utils::transaction::TransactionBlocker,
    ) {
        let Some(path) = self.tree.find_window(window) else {
            return;
        };

        let Some((rect, visible)) = self
            .tree
            .leaf_layouts()
            .iter()
            .find(|info| info.path == path)
            .map(|info| (info.rect, info.visible))
        else {
            return;
        };

        if !visible {
            return;
        }

        let Some(tile) = self.tree.tile_at_path_mut(&path) else {
            return;
        };

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();
        let tile_pos = rect.loc + tile.render_offset();

        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let scale = Scale::from(self.scale);
        let res = ClosingWindow::new(
            renderer.as_gles_renderer(),
            snapshot,
            scale,
            tile_size,
            tile_pos,
            blocker,
            anim,
        );
        match res {
            Ok(closing) => {
                self.closing_windows.push(closing);
            }
            Err(err) => {
                warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        let applied = self.tree.apply_pending_layouts_if_ready();
        if applied && self.tree.take_pending_relayout() {
            self.tree.layout();
        }
        let has_pending = self.tree.has_pending_layouts();
        let layouts = if has_pending {
            self.tree
                .pending_leaf_layouts_cloned()
                .unwrap_or_else(|| self.tree.leaf_layouts_cloned())
        } else {
            self.tree.leaf_layouts_cloned()
        };
        let focus_path = self.tree.focus_path();
        let fullscreen_id = self.pending_fullscreen_window().cloned();
        let windowed_fullscreen_id = if fullscreen_id.is_none() {
            self.tree.focused_tile().and_then(|tile| {
                tile.window()
                    .is_pending_windowed_fullscreen()
                    .then(|| tile.window().id().clone())
            })
        } else {
            None
        };

        for info in layouts {
            // Use O(1) key lookup instead of O(depth) path lookup.
            if let Some(tile) = self.tree.get_tile_mut(info.key) {
                let deactivate_unfocused = self.options.deactivate_unfocused_windows && !is_focused;

                let resize = self
                    .interactive_resize
                    .as_ref()
                    .and_then(|resize| Self::interactive_resize_data_for_path(&info.path, resize));
                Self::update_window_state(
                    tile,
                    &info,
                    &focus_path,
                    is_active,
                    deactivate_unfocused,
                    resize,
                    !has_pending,
                    self.working_area.size,
                    &self.options,
                    fullscreen_id.as_ref(),
                    windowed_fullscreen_id.as_ref(),
                    self.view_size,
                );
            }
        }
    }
    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer (e.g. waybar) when a window is fullscreen
        self.render_fullscreen_window().is_some()
            || self
                .tree
                .focused_tile()
                .is_some_and(|tile| tile.window().is_pending_windowed_fullscreen())
    }

    pub fn activation_view_distance(&self, _window: &W::Id) -> f64 {
        self.viewport.activation_distance()
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        // Find the tile for this window and return its popup target rectangle
        for info in self.display_layouts() {
            if let Some(tile) = self.tree.get_tile(info.key) {
                if tile.window().id() == window {
                    // Similar to tiling layout: constrain horizontally to window,
                    // vertically to the working area
                    let width = tile.window_size().w;
                    let height = self.working_area.size.h;

                    let mut target = Rectangle::from_size(Size::from((width, height)));
                    target.loc.y += self.working_area.loc.y;
                    target.loc.y -= info.rect.loc.y;
                    target.loc.y -= tile.window_loc().y;

                    return Some(target);
                }
            }
        }
        None
    }

    pub fn horizontal_view_gesture_begin(&mut self, is_touchpad: bool) {
        self.viewport.begin_horizontal_gesture(is_touchpad);
    }

    pub fn horizontal_view_gesture_update(
        &mut self,
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.viewport
            .update_horizontal_gesture(delta, timestamp, is_touchpad)
    }

    pub fn horizontal_view_gesture_end(&mut self, cancelled: Option<bool>) -> bool {
        self.viewport.end_horizontal_gesture(cancelled)
    }
}

impl TilingSpace<crate::window::Mapped> {
    pub(crate) fn layout_tree(&self) -> Option<LayoutTreeNode> {
        self.tree.layout_tree()
    }

    pub(crate) fn layout_tree_unfocused(&self) -> Option<LayoutTreeNode> {
        self.tree.layout_tree_unfocused()
    }
}

impl<W: LayoutElement> TilingSpace<W> {
    fn update_window_state(
        tile: &mut Tile<W>,
        info: &LeafLayoutInfo,
        focus_path: &[usize],
        workspace_active: bool,
        deactivate_unfocused: bool,
        interactive_resize: Option<InteractiveResizeData>,
        request_size: bool,
        working_area_size: Size<f64, Logical>,
        options: &Options,
        fullscreen_id: Option<&W::Id>,
        windowed_fullscreen_id: Option<&W::Id>,
        view_size: Size<f64, Logical>,
    ) {
        let window_id = tile.window().id().clone();
        let is_focused_tile = info.path == focus_path;
        let is_fullscreen_tile = fullscreen_id.is_some_and(|id| id == &window_id);
        let is_windowed_fullscreen_tile = windowed_fullscreen_id.is_some_and(|id| id == &window_id);
        let is_fullscreen_like_tile = is_fullscreen_tile || is_windowed_fullscreen_tile;
        let has_fullscreen_like = fullscreen_id.is_some() || windowed_fullscreen_id.is_some();

        if request_size {
            if is_fullscreen_tile {
                tile.request_fullscreen(false, None);
            } else if is_windowed_fullscreen_tile {
                tile.request_windowed_fullscreen(false, None);
            } else {
                let target_size = Size::from((info.rect.size.w, info.rect.size.h));
                tile.request_tile_size(target_size, false, None);
            }
        }

        let mut active = workspace_active && is_focused_tile;

        if has_fullscreen_like && !is_fullscreen_like_tile {
            active = false;
        } else if deactivate_unfocused {
            active &= info.visible;
        }

        let active_in_column = is_focused_tile && (!has_fullscreen_like || is_fullscreen_like_tile);

        let window = tile.window_mut();
        window.set_active_in_column(active_in_column);
        window.set_floating(false);
        window.set_activated(active);
        window.set_interactive_resize(interactive_resize);

        let border_config = options.layout.border.merged_with(&window.rules().border);

        let bounds = if is_fullscreen_like_tile {
            view_size.to_i32_floor()
        } else {
            let max_bounds = compute_toplevel_bounds(
                border_config,
                working_area_size,
                Size::from((0.0, 0.0)),
                options.layout.gaps,
            );
            let mut logical_bounds: Size<i32, Logical> =
                Size::from((info.rect.size.w, info.rect.size.h)).to_i32_floor();
            logical_bounds.w = logical_bounds.w.min(max_bounds.w);
            logical_bounds.h = logical_bounds.h.min(max_bounds.h);
            logical_bounds
        };

        window.set_bounds(bounds);

        match window.configure_intent() {
            ConfigureIntent::CanSend | ConfigureIntent::ShouldSend => {
                window.send_pending_configure();
            }
            _ => {}
        }

        window.refresh();
    }
}

impl<W: LayoutElement> RootTilingSubtree<W> {
    pub fn new(tile: Tile<W>) -> Self {
        Self {
            subtree: DetachedNode::Leaf(tile),
        }
    }

    pub fn from_tiles(tiles: Vec<Tile<W>>) -> Self {
        if tiles.is_empty() {
            return Self {
                subtree: DetachedNode::Container(DetachedContainer::new(
                    Layout::SplitV,
                    Vec::new(),
                )),
            };
        }

        if tiles.len() == 1 {
            return Self::new(tiles.into_iter().next().unwrap());
        }

        let children = tiles
            .into_iter()
            .map(DetachedNode::Leaf)
            .collect::<Vec<_>>();
        Self {
            subtree: DetachedNode::Container(DetachedContainer::new(Layout::SplitV, children)),
        }
    }

    pub fn tiles(&self) -> Vec<&Tile<W>> {
        self.subtree.tiles()
    }

    pub fn contains(&self, window: &W) -> bool {
        self.subtree.contains_window(window.id())
    }

    pub fn from_subtree(subtree: DetachedNode<W>) -> Self {
        Self { subtree }
    }

    pub fn into_subtree(self) -> DetachedNode<W> {
        self.subtree
    }

    pub fn into_tiles(self) -> Vec<Tile<W>> {
        self.subtree.into_tiles()
    }
}

impl Default for WindowHeight {
    fn default() -> Self {
        Self::Auto
    }
}

fn compute_toplevel_bounds(
    border_config: Border,
    working_area_size: Size<f64, Logical>,
    extra_size: Size<f64, Logical>,
    gaps: f64,
) -> Size<i32, Logical> {
    let mut border = 0.0;
    if !border_config.off {
        border = border_config.width * 2.0;
    }

    Size::from((
        f64::max(
            working_area_size.w - gaps * 2.0 - extra_size.w - border,
            1.0,
        ),
        f64::max(
            working_area_size.h - gaps * 2.0 - extra_size.h - border,
            1.0,
        ),
    ))
    .to_i32_floor()
}

fn edge_visibility_for_tile(
    options: &Options,
    layout_rect: Rectangle<f64, Logical>,
    tile_rect: Rectangle<f64, Logical>,
    scale: f64,
    is_single_window: bool,
) -> FocusRingEdges {
    if options.layout.hide_edge_borders_smart && is_single_window {
        return FocusRingEdges::none();
    }

    let mut edges = FocusRingEdges::all();
    let hide_mode = options.layout.hide_edge_borders;
    if hide_mode == HideEdgeBorders::None {
        return edges;
    }

    let eps = 0.5 / scale.max(1e-6);
    let left = (tile_rect.loc.x - layout_rect.loc.x).abs() <= eps;
    let right = (tile_rect.loc.x + tile_rect.size.w - (layout_rect.loc.x + layout_rect.size.w))
        .abs()
        <= eps;
    let top = (tile_rect.loc.y - layout_rect.loc.y).abs() <= eps;
    let bottom = (tile_rect.loc.y + tile_rect.size.h - (layout_rect.loc.y + layout_rect.size.h))
        .abs()
        <= eps;

    let hide_horizontal = matches!(
        hide_mode,
        HideEdgeBorders::Horizontal | HideEdgeBorders::Both
    );
    let hide_vertical = matches!(hide_mode, HideEdgeBorders::Vertical | HideEdgeBorders::Both);

    if hide_horizontal {
        if top {
            edges.top = false;
        }
        if bottom {
            edges.bottom = false;
        }
    }

    if hide_vertical {
        if left {
            edges.left = false;
        }
        if right {
            edges.right = false;
        }
    }

    edges
}

fn split_indicator_edge_for_tile<W: LayoutElement>(
    tree: &ContainerTree<W>,
    path: &[usize],
    edges: FocusRingEdges,
) -> Option<FocusRingIndicatorEdge> {
    let layout = tree.single_child_split_layout_for_path(path)?;
    match layout {
        Layout::SplitH => edges.right.then_some(FocusRingIndicatorEdge::Right),
        Layout::SplitV => edges.bottom.then_some(FocusRingIndicatorEdge::Bottom),
        _ => None,
    }
}
