//! Size fractions and resize reference spans owned by the nodes they describe.

use super::{
    ChildFractions, ContainerTree, Layout, LayoutElement, NodeData, NodeKey, NodeSizing,
    ResizeDelta, ResizeReach, ResizeSpace, MIN_CHILD_PERCENT,
};

fn normalize_percents(percents: &mut [f64]) {
    let count = percents.len();
    if count == 0 {
        return;
    }
    let sum: f64 = percents
        .iter()
        .copied()
        .filter(|percent| percent.is_finite() && *percent >= 0.0)
        .sum();
    if sum <= f64::EPSILON
        || percents
            .iter()
            .any(|percent| !percent.is_finite() || *percent < 0.0)
    {
        percents.fill(1.0 / count as f64);
        return;
    }
    for percent in percents {
        *percent /= sum;
    }
}

impl<W: LayoutElement> ContainerTree<W> {
    fn node_sizing(&self, key: NodeKey) -> Option<&NodeSizing> {
        match self.get_node(key)? {
            NodeData::Container(container) => Some(&container.sizing),
            NodeData::Leaf(tile) => Some(tile.node_sizing()),
        }
    }

    fn node_sizing_mut(&mut self, key: NodeKey) -> Option<&mut NodeSizing> {
        match self.get_node_mut(key)? {
            NodeData::Container(container) => Some(&mut container.sizing),
            NodeData::Leaf(tile) => Some(tile.node_sizing_mut()),
        }
    }

    /// Both axis fractions carried by one node, whether either axis is active here.
    pub(super) fn node_fractions(&self, key: NodeKey) -> Option<ChildFractions> {
        Some(self.node_sizing(key)?.fractions)
    }

    pub(super) fn set_node_fractions(&mut self, key: NodeKey, fractions: ChildFractions) -> bool {
        let Some(sizing) = self.node_sizing_mut(key) else {
            return false;
        };
        sizing.fractions = fractions;
        true
    }

    /// Wipe both shares on the node itself.
    pub(super) fn unset_node_fractions(&mut self, key: NodeKey) -> bool {
        let Some(sizing) = self.node_sizing_mut(key) else {
            return false;
        };
        sizing.unset_fractions();
        true
    }

    fn node_fraction(&self, key: NodeKey, layout: Layout) -> Option<f64> {
        Some(self.node_sizing(key)?.fraction(layout))
    }

    fn set_node_fraction(&mut self, key: NodeKey, layout: Layout, fraction: f64) -> bool {
        let Some(sizing) = self.node_sizing_mut(key) else {
            return false;
        };
        sizing.set_fraction(layout, fraction);
        true
    }

    pub(super) fn node_child_total(&self, key: NodeKey, layout: Layout) -> Option<f64> {
        Some(self.node_sizing(key)?.child_total(layout))
    }

    pub(super) fn set_node_child_total(
        &mut self,
        key: NodeKey,
        layout: Layout,
        total: f64,
    ) -> bool {
        let Some(sizing) = self.node_sizing_mut(key) else {
            return false;
        };
        sizing.set_child_total(layout, total);
        true
    }

    #[cfg(test)]
    pub(in crate::layout) fn debug_node_sizing(
        &self,
        key: NodeKey,
    ) -> Option<(f64, f64, f64, f64)> {
        let sizing = self.node_sizing(key)?;
        Some((
            sizing.fractions.width,
            sizing.fractions.height,
            sizing.child_total_width,
            sizing.child_total_height,
        ))
    }

    /// Raw shares of a parent's children along that parent's active axis.
    pub(super) fn child_percents(&self, parent: NodeKey) -> Vec<f64> {
        let Some(container) = self.get_container(parent) else {
            return Vec::new();
        };
        let layout = container.layout();
        container
            .children()
            .iter()
            .map(|child| self.node_fraction(*child, layout).unwrap_or(0.0))
            .collect()
    }

    /// Fill and normalize a parent's active shares during arrange.
    pub(super) fn resolve_child_percents(&mut self, parent: NodeKey) {
        let Some(container) = self.get_container(parent) else {
            return;
        };
        let layout = container.layout();
        if !matches!(layout, Layout::SplitH | Layout::SplitV) {
            return;
        }
        let children = container.children().to_vec();
        let resolved = super::resolved_percents(&self.child_percents(parent), children.len());
        for (child, percent) in children.into_iter().zip(resolved) {
            self.set_node_fraction(child, layout, percent);
        }
    }

    pub(in crate::layout) fn recalculate_child_percents(&mut self, parent: NodeKey) -> bool {
        let Some(container) = self.get_container(parent) else {
            return false;
        };
        let layout = container.layout();
        let children = container.children().to_vec();
        if children.is_empty() {
            return true;
        }
        let percent = 1.0 / children.len() as f64;
        for child in children {
            self.set_node_fraction(child, layout, percent);
        }
        true
    }

    /// Size share of the `child_idx`-th child of `parent`.
    pub(in crate::layout) fn child_percent(
        &self,
        parent: NodeKey,
        child_idx: usize,
    ) -> Option<f64> {
        let container = self.get_container(parent)?;
        let child = container.child_key(child_idx)?;
        self.node_fraction(child, container.layout())
    }

    /// Give one child a requested share and redistribute the remainder across its siblings.
    pub(in crate::layout) fn set_child_percent(
        &mut self,
        parent: NodeKey,
        child_idx: usize,
        layout: Layout,
        percent: f64,
    ) -> bool {
        let Some(container) = self.get_container(parent) else {
            return false;
        };
        if container.layout() != layout || child_idx >= container.child_count() {
            return false;
        }
        let children = container.children().to_vec();
        let mut percents = self.child_percents(parent);
        let len = percents.len();
        if len == 1 {
            return self.set_node_fraction(children[0], layout, 1.0);
        }

        let min = MIN_CHILD_PERCENT;
        let max = 1.0 - min * (len as f64 - 1.0);
        let new_percent = percent.clamp(min, max.max(min));
        percents[child_idx] = new_percent;

        let remaining = (1.0 - new_percent).max(min * (len as f64 - 1.0));
        let others_sum: f64 = percents
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx != child_idx)
            .map(|(_, value)| *value)
            .sum();
        if others_sum <= f64::EPSILON {
            let share = remaining / (len as f64 - 1.0);
            for (idx, value) in percents.iter_mut().enumerate() {
                if idx != child_idx {
                    *value = share;
                }
            }
        } else {
            let scale = remaining / others_sum;
            for (idx, value) in percents.iter_mut().enumerate() {
                if idx != child_idx {
                    *value *= scale;
                }
            }
        }
        normalize_percents(&mut percents);
        for (child, percent) in children.into_iter().zip(percents) {
            self.set_node_fraction(child, layout, percent);
        }
        true
    }

    /// Resize one child, taking equal pixel amounts from the selected siblings.
    ///
    /// Before changing anything sway reconstructs every sibling's fraction from its rounded
    /// pending span and the `child_total_*` recorded on that same node by arrange. This is why
    /// the denominator cannot be recomputed from today's siblings after tree surgery.
    ///
    /// sway/commands/resize.c:117-163
    pub(in crate::layout) fn resize_child(
        &mut self,
        parent: NodeKey,
        child_idx: usize,
        layout: Layout,
        reach: ResizeReach,
        delta: ResizeDelta,
        space: ResizeSpace,
    ) -> bool {
        let Some(container) = self.get_container(parent) else {
            return false;
        };
        if container.layout() != layout || child_idx >= container.child_count() {
            return false;
        }
        let children = container.children().to_vec();
        let len = children.len();
        if space.child_spans.len() != len {
            return false;
        }
        let Some(child_total) = self.node_child_total(children[child_idx], layout) else {
            return false;
        };
        if child_total <= 0.0 {
            return false;
        }

        let mut payers = Vec::with_capacity(len.saturating_sub(1));
        match reach {
            ResizeReach::Siblings => payers.extend((0..len).filter(|idx| *idx != child_idx)),
            ResizeReach::Before if child_idx > 0 => payers.push(child_idx - 1),
            ResizeReach::After if child_idx + 1 < len => payers.push(child_idx + 1),
            ResizeReach::Before | ResizeReach::After => {}
        }
        let Some(each) = (!payers.is_empty()).then(|| delta.pixels / payers.len() as f64) else {
            return false;
        };
        let payer_check_size = each.ceil();
        if space.child_spans[child_idx] + delta.pixels < space.min_size
            || payers
                .iter()
                .any(|idx| space.child_spans[*idx] - payer_check_size < space.min_size)
        {
            return false;
        }

        let mut snapped = Vec::with_capacity(len);
        for (child, span) in children.iter().zip(&space.child_spans) {
            let Some(total) = self.node_child_total(*child, layout) else {
                return false;
            };
            if total <= 0.0 {
                return false;
            }
            snapped.push(*span / total);
        }
        let amount_fraction = delta.pixels / child_total;
        snapped[child_idx] += amount_fraction;
        let payer_fraction = amount_fraction / payers.len() as f64;
        for payer in payers {
            snapped[payer] -= payer_fraction;
        }
        for (child, percent) in children.into_iter().zip(snapped) {
            self.set_node_fraction(child, layout, percent);
        }
        true
    }

    pub(super) fn set_child_percent_pair(
        &mut self,
        parent: NodeKey,
        idx: usize,
        neighbor_idx: usize,
        percent: f64,
    ) -> bool {
        let Some(container) = self.get_container(parent) else {
            return false;
        };
        let layout = container.layout();
        let children = container.children().to_vec();
        let mut percents = self.child_percents(parent);
        let len = percents.len();
        if len < 2 || idx >= len || neighbor_idx >= len || idx == neighbor_idx {
            return false;
        }
        let total = percents[idx] + percents[neighbor_idx];
        if total <= f64::EPSILON || total < MIN_CHILD_PERCENT * 2.0 {
            return false;
        }
        let new_percent = percent.clamp(MIN_CHILD_PERCENT, total - MIN_CHILD_PERCENT);
        let neighbor_percent = total - new_percent;
        if (percents[idx] - new_percent).abs() <= f64::EPSILON
            && (percents[neighbor_idx] - neighbor_percent).abs() <= f64::EPSILON
        {
            return false;
        }
        percents[idx] = new_percent;
        percents[neighbor_idx] = neighbor_percent;
        self.set_node_fraction(children[idx], layout, new_percent);
        self.set_node_fraction(children[neighbor_idx], layout, neighbor_percent);
        true
    }
}
