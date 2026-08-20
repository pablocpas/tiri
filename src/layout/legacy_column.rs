//! Compatibility wrappers for public APIs that still use niri's old column vocabulary.
//!
//! The layout core is an i3/sway-style container tree. These types keep existing config, IPC and
//! test surfaces stable while preventing the core model from depending on "column"
//! concepts.

use super::container::DetachedNode;
use super::container_tree::RootTilingSubtree;
use super::tile::Tile;
use super::LayoutElement;

/// Legacy wrapper preserving the old niri-inspired "column" vocabulary at public seams.
#[derive(Debug)]
pub struct Column<W: LayoutElement> {
    subtree: RootTilingSubtree<W>,
}

/// Legacy width specification for top-level tiling containers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    Proportion(f64),
    Fixed(i32),
}

impl<W: LayoutElement> Column<W> {
    pub fn new(tile: Tile<W>) -> Self {
        RootTilingSubtree::new(tile).into()
    }

    pub fn from_tiles(tiles: Vec<Tile<W>>) -> Self {
        RootTilingSubtree::from_tiles(tiles).into()
    }

    pub fn tiles(&self) -> Vec<&Tile<W>> {
        self.subtree.tiles()
    }

    pub fn contains(&self, window: &W) -> bool {
        self.subtree.contains(window)
    }

    pub fn from_subtree(subtree: DetachedNode<W>) -> Self {
        RootTilingSubtree::from_subtree(subtree).into()
    }

    pub fn into_subtree(self) -> DetachedNode<W> {
        self.subtree.into_subtree()
    }

    pub fn into_tiles(self) -> Vec<Tile<W>> {
        self.subtree.into_tiles()
    }
}

impl<W: LayoutElement> From<Column<W>> for RootTilingSubtree<W> {
    fn from(value: Column<W>) -> Self {
        value.subtree
    }
}

impl<W: LayoutElement> From<RootTilingSubtree<W>> for Column<W> {
    fn from(value: RootTilingSubtree<W>) -> Self {
        Self { subtree: value }
    }
}

impl Default for ColumnWidth {
    fn default() -> Self {
        Self::Proportion(1.0)
    }
}
