use knuffel::errors::DecodeError;
use miette::miette;
use std::str::FromStr;
use tiri_ipc::{ColumnDisplay, SizeChange};

use crate::appearance::{
    Border, FocusRing, InsertHint, Shadow, TabBar, TabIndicator, DEFAULT_BACKGROUND_COLOR,
};
use crate::utils::{expect_only_children, Flag, MergeWith};
use crate::{
    BorderRule, Color, FloatOrInt, InsertHintPart, ShadowRule, TabBarPart, TabIndicatorPart,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub focus_ring: FocusRing,
    pub border: Border,
    pub hide_edge_borders: HideEdgeBorders,
    pub hide_edge_borders_smart: bool,
    pub shadow: Shadow,
    pub tab_indicator: TabIndicator,
    pub tab_bar: TabBar,
    pub insert_hint: InsertHint,
    pub preset_column_widths: Vec<PresetSize>,
    pub default_column_width: Option<PresetSize>,
    pub preset_window_heights: Vec<PresetSize>,
    pub empty_workspace_above_first: bool,
    pub default_column_display: ColumnDisplay,
    pub gaps: f64,
    pub struts: Struts,
    pub background_color: Color,
    /// Split every new window against the shape of the node it lands beside.
    ///
    /// The dwindle mode of Hyprland, and what the `autotiling` script gives sway by issuing a
    /// `split h` or `split v` before each window maps.
    pub autotile: bool,
    /// How much wider than tall a node must be before [`Self::autotile`] splits it sideways.
    ///
    /// 1.0 is the plain "wider than tall" rule. Above it the mode prefers stacking, below it
    /// the mode prefers columns.
    pub autotile_ratio: f64,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            focus_ring: FocusRing::default(),
            border: Border::default(),
            hide_edge_borders: HideEdgeBorders::default(),
            hide_edge_borders_smart: false,
            shadow: Shadow::default(),
            tab_indicator: TabIndicator::default(),
            tab_bar: TabBar::default(),
            insert_hint: InsertHint::default(),
            preset_column_widths: vec![
                PresetSize::Proportion(1. / 3.),
                PresetSize::Proportion(0.5),
                PresetSize::Proportion(2. / 3.),
            ],
            default_column_width: Some(PresetSize::Proportion(0.5)),
            empty_workspace_above_first: false,
            default_column_display: ColumnDisplay::Normal,
            gaps: 8.,
            struts: Struts::default(),
            preset_window_heights: vec![
                PresetSize::Proportion(1. / 3.),
                PresetSize::Proportion(0.5),
                PresetSize::Proportion(2. / 3.),
            ],
            background_color: DEFAULT_BACKGROUND_COLOR,
            autotile: false,
            autotile_ratio: 1.,
        }
    }
}

impl MergeWith<LayoutPart> for Layout {
    fn merge_with(&mut self, part: &LayoutPart) {
        merge!(
            (self, part),
            focus_ring,
            border,
            shadow,
            tab_indicator,
            tab_bar,
            insert_hint,
            empty_workspace_above_first,
            gaps,
            autotile,
            autotile_ratio,
        );

        merge_clone!(
            (self, part),
            preset_column_widths,
            preset_window_heights,
            default_column_display,
            struts,
            background_color,
        );

        if let Some(x) = part.default_column_width {
            self.default_column_width = x.0;
        }
        if let Some(x) = part.hide_edge_borders {
            self.hide_edge_borders = x;
        }
        if let Some(x) = part.hide_edge_borders_smart {
            self.hide_edge_borders_smart.merge_with(&x);
        }

        if self.preset_column_widths.is_empty() {
            self.preset_column_widths = Layout::default().preset_column_widths;
        }

        if self.preset_window_heights.is_empty() {
            self.preset_window_heights = Layout::default().preset_window_heights;
        }
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct LayoutPart {
    #[knuffel(child)]
    pub focus_ring: Option<BorderRule>,
    #[knuffel(child)]
    pub border: Option<BorderRule>,
    #[knuffel(child, unwrap(argument, str))]
    pub hide_edge_borders: Option<HideEdgeBorders>,
    #[knuffel(child)]
    pub hide_edge_borders_smart: Option<Flag>,
    #[knuffel(child)]
    pub shadow: Option<ShadowRule>,
    #[knuffel(child)]
    pub tab_indicator: Option<TabIndicatorPart>,
    #[knuffel(child)]
    pub tab_bar: Option<TabBarPart>,
    #[knuffel(child)]
    pub insert_hint: Option<InsertHintPart>,
    #[knuffel(child, unwrap(children))]
    pub preset_column_widths: Option<Vec<PresetSize>>,
    #[knuffel(child)]
    pub default_column_width: Option<DefaultPresetSize>,
    #[knuffel(child, unwrap(children))]
    pub preset_window_heights: Option<Vec<PresetSize>>,
    #[knuffel(child)]
    pub empty_workspace_above_first: Option<Flag>,
    #[knuffel(child, unwrap(argument, str))]
    pub default_column_display: Option<ColumnDisplay>,
    #[knuffel(child, unwrap(argument))]
    pub gaps: Option<FloatOrInt<0, 65535>>,
    #[knuffel(child)]
    pub struts: Option<Struts>,
    #[knuffel(child)]
    pub background_color: Option<Color>,
    #[knuffel(child)]
    pub autotile: Option<Flag>,
    #[knuffel(child, unwrap(argument))]
    pub autotile_ratio: Option<FloatOrInt<0, 65535>>,
}

#[derive(knuffel::Decode, Debug, Clone, Copy, PartialEq)]
pub enum PresetSize {
    Proportion(#[knuffel(argument)] f64),
    Fixed(#[knuffel(argument)] i32),
}

impl From<PresetSize> for SizeChange {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(prop) => SizeChange::SetProportion(prop * 100.),
            PresetSize::Fixed(fixed) => SizeChange::SetFixed(fixed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DefaultPresetSize(pub Option<PresetSize>);

#[derive(knuffel::Decode, Debug, Default, Clone, Copy, PartialEq)]
pub struct Struts {
    #[knuffel(child, unwrap(argument), default)]
    pub left: FloatOrInt<-65535, 65535>,
    #[knuffel(child, unwrap(argument), default)]
    pub right: FloatOrInt<-65535, 65535>,
    #[knuffel(child, unwrap(argument), default)]
    pub top: FloatOrInt<-65535, 65535>,
    #[knuffel(child, unwrap(argument), default)]
    pub bottom: FloatOrInt<-65535, 65535>,
}

#[derive(knuffel::DecodeScalar, Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum HideEdgeBorders {
    #[default]
    None,
    Horizontal,
    Vertical,
    Both,
}

impl FromStr for HideEdgeBorders {
    type Err = miette::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "horizontal" => Ok(Self::Horizontal),
            "vertical" => Ok(Self::Vertical),
            "both" => Ok(Self::Both),
            _ => Err(miette!("invalid hide-edge-borders value: {s}")),
        }
    }
}

impl<S> knuffel::Decode<S> for DefaultPresetSize
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        expect_only_children(node, ctx);

        let mut children = node.children();

        if let Some(child) = children.next() {
            if let Some(unwanted_child) = children.next() {
                ctx.emit_error(DecodeError::unexpected(
                    unwanted_child,
                    "node",
                    "expected no more than one child",
                ));
            }
            PresetSize::decode_node(child, ctx).map(Some).map(Self)
        } else {
            Ok(Self(None))
        }
    }
}
