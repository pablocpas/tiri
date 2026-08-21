use std::borrow::Cow;

use anyhow::{bail, Context, Result};
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{self, Alignment, EllipsizeMode, FontDescription};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use tiri_config::{Color, TabBar};

use super::container::{Layout, TabBarInfo, TabBarTab};
use crate::render_helpers::texture::TextureBuffer;
use crate::render_helpers::RenderTarget;
use crate::utils::{round_logical_in_physical_max1, to_physical_precise_round};

fn sanitize_title(title: &str) -> Cow<'_, str> {
    if title.chars().all(|ch| !ch.is_control()) {
        let trimmed = title.trim();
        return if trimmed.is_empty() {
            Cow::Borrowed("untitled")
        } else {
            Cow::Borrowed(trimmed)
        };
    }

    let mut buf = String::with_capacity(title.len());
    for ch in title.chars() {
        if ch.is_control() {
            buf.push(' ');
        } else {
            buf.push(ch);
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        Cow::Borrowed("untitled")
    } else {
        Cow::Owned(trimmed.to_string())
    }
}

fn font_description_for_scale(config: &TabBar, scale: f64) -> FontDescription {
    let mut font = FontDescription::from_string(&config.font);
    let base_size = font.size() as f64;
    let size = if base_size > 0.0 {
        base_size
    } else {
        let fallback_px = parse_font_size(&config.font).unwrap_or(12.0);
        fallback_px * pango::SCALE as f64
    };
    let size = to_physical_precise_round::<f64>(scale, size).max(1.0);
    font.set_absolute_size(size);
    font
}

fn measure_font_height_px(font: &FontDescription) -> Option<i32> {
    let surface = ImageSurface::create(cairo::Format::ARgb32, 1, 1).ok()?;
    let cr = cairo::Context::new(&surface).ok()?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(font));
    layout.set_text("Ag");
    let (_w, h_px) = layout.pixel_size();
    (h_px > 0).then_some(h_px)
}

pub fn tab_bar_row_height(config: &TabBar, scale: f64) -> f64 {
    let mut height = config.height;
    if height <= 0.0 {
        let font = font_description_for_scale(config, scale);
        if let Some(h_px) = measure_font_height_px(&font) {
            let font_height = (h_px as f64) / scale;
            height = font_height + config.padding_y * 2.0;
        }
        if height <= 0.0 {
            let font_height = parse_font_size(&config.font).unwrap_or(12.0);
            height = font_height + config.padding_y * 2.0;
        }
    }

    round_logical_in_physical_max1(scale, height)
}

fn parse_font_size(font: &str) -> Option<f64> {
    let mut last = None;
    let mut buf = String::new();
    for ch in font.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            buf.push(ch);
        } else if !buf.is_empty() {
            if let Ok(val) = buf.parse::<f64>() {
                last = Some(val);
            }
            buf.clear();
        }
    }
    if !buf.is_empty() {
        if let Ok(val) = buf.parse::<f64>() {
            last = Some(val);
        }
    }
    last
}

fn set_source_color(cr: &cairo::Context, color: Color) {
    let [r, g, b, a] = color.to_array_unpremul();
    cr.set_source_rgba(f64::from(r), f64::from(g), f64::from(b), f64::from(a));
}

/// The i3 decoration states, for a tab.
///
/// `focused` is the selected tab of the container the seat is actually in;
/// `focused_inactive` is the selected tab of any other container — the one it would come
/// back to. Without that middle state a container you are not in reports nothing about
/// where its focus would land, which is the whole point of a tab bar in a tree. Matches
/// the rule the tiles use (`Tile::update_render_elements`), so a tab and the border of
/// the window under it always agree.
fn tab_colors(
    config: &TabBar,
    tab: &TabBarTab,
    is_active_workspace: bool,
) -> (Color, Color, Color) {
    if tab.is_urgent {
        (config.urgent_bg, config.urgent_fg, config.urgent_border)
    } else if tab.is_focused && tab.holds_focus && is_active_workspace {
        (config.active_bg, config.active_fg, config.active_border)
    } else if tab.is_focused {
        (
            config.focused_inactive_bg,
            config.focused_inactive_fg,
            config.focused_inactive_border,
        )
    } else {
        (
            config.inactive_bg,
            config.inactive_fg,
            config.inactive_border,
        )
    }
}

/// Common tab state for caching
#[derive(Debug, Clone, PartialEq)]
pub struct TabBarTabState {
    pub title: String,
    pub is_focused: bool,
    pub holds_focus: bool,
    pub is_urgent: bool,
    pub block_out: bool,
}

/// Common tab bar state for caching
#[derive(Debug, Clone, PartialEq)]
pub struct TabBarState {
    pub layout: Layout,
    pub size: Size<f64, Logical>,
    pub row_height: f64,
    pub scale: f64,
    pub config: TabBar,
    pub tabs: Vec<TabBarTabState>,
}

/// Common tab bar cache entry
#[derive(Debug, Clone)]
pub struct TabBarCacheEntry {
    pub state: TabBarState,
    pub buffer: TextureBuffer<GlesTexture>,
    pub tab_widths_px: Vec<i32>,
}

/// Helper to create tab bar state from info
pub fn tab_bar_state_from_info(
    info: &TabBarInfo,
    config: &TabBar,
    is_active: bool,
    scale: f64,
    target: RenderTarget,
) -> TabBarState {
    let tabs = info
        .tabs
        .iter()
        // The cache key has to separate everything `tab_colors` separates, and the
        // workspace flag folds into `holds_focus` there: an inactive workspace demotes
        // `focused` to `focused_inactive`, it does not demote the selected tab further.
        .map(|tab| TabBarTabState {
            title: tab.title.clone(),
            is_focused: tab.is_focused,
            holds_focus: tab.holds_focus && is_active,
            is_urgent: tab.is_urgent,
            block_out: target.should_block_out(tab.block_out_from),
        })
        .collect();

    TabBarState {
        layout: info.layout,
        size: info.rect.size,
        row_height: info.row_height,
        scale,
        config: config.clone(),
        tabs,
    }
}

pub struct TabBarRenderOutput {
    pub buffer: TextureBuffer<GlesTexture>,
    pub tab_widths_px: Vec<i32>,
}

#[allow(clippy::too_many_arguments)]
pub fn render_tab_bar(
    renderer: &mut GlesRenderer,
    config: &TabBar,
    layout: Layout,
    rect: Rectangle<f64, Logical>,
    row_height: f64,
    tabs: &[TabBarTab],
    is_active_workspace: bool,
    target: RenderTarget,
    scale: f64,
) -> Result<TabBarRenderOutput> {
    let tab_count = tabs.len();
    if tab_count == 0 || rect.size.w <= 0.0 || rect.size.h <= 0.0 {
        bail!("tab bar has no visible size");
    }

    let width_px: i32 = to_physical_precise_round::<i32>(scale, rect.size.w).max(1);
    let height_px: i32 = to_physical_precise_round::<i32>(scale, rect.size.h).max(1);
    let row_height_px: i32 = to_physical_precise_round::<i32>(scale, row_height).max(1);
    let padding_x_px: i32 = to_physical_precise_round::<i32>(scale, config.padding_x).max(0);
    let mut padding_y_px: i32 = to_physical_precise_round::<i32>(scale, config.padding_y).max(0);
    let separator_width_px: i32 =
        to_physical_precise_round::<i32>(scale, config.separator_width).max(0);
    let border_width_px: i32 = to_physical_precise_round::<i32>(scale, config.border_width).max(0);

    let mut font = font_description_for_scale(config, scale);
    let font_height_px = measure_font_height_px(&font).unwrap_or(row_height_px);

    let min_padding_y = row_height_px.saturating_sub(1) / 2;
    if padding_y_px > min_padding_y {
        padding_y_px = min_padding_y;
    }

    let text_area_height = row_height_px.saturating_sub(padding_y_px * 2).max(1);
    if font_height_px > text_area_height {
        let scale_factor = text_area_height as f64 / font_height_px as f64;
        let new_size = (font.size() as f64 * scale_factor).max(1.0);
        font.set_absolute_size(new_size);
    }

    let tab_widths = if layout == Layout::Tabbed {
        even_tab_widths_px(width_px, tab_count)
    } else {
        vec![width_px; tab_count]
    };

    let surface = ImageSurface::create(cairo::Format::ARgb32, width_px, height_px)?;
    let cr = cairo::Context::new(&surface)?;
    set_source_color(&cr, config.inactive_bg);
    cr.paint()?;

    let text_layout = pangocairo::functions::create_layout(&cr);
    text_layout.context().set_round_glyph_positions(false);
    text_layout.set_single_paragraph_mode(true);
    text_layout.set_font_description(Some(&font));
    text_layout.set_ellipsize(EllipsizeMode::End);
    text_layout.set_alignment(Alignment::Left);

    // The strip below the rows is painted in the selected tab's color further down, so
    // that tab and the window's frame are one shape. Known here because the rim of the tab
    // that opens into it must not cut across the seam.
    let row_count = if layout == Layout::Tabbed {
        1
    } else {
        tab_count
    };
    let extra_height = height_px - row_height_px.saturating_mul(row_count as i32);

    let mut cursor_x = 0;
    for (idx, tab) in tabs.iter().enumerate() {
        let width = tab_widths[idx];
        let (x, y, w, h) = if layout == Layout::Tabbed {
            (cursor_x, 0, width, row_height_px)
        } else {
            (0, idx as i32 * row_height_px, width_px, row_height_px)
        };
        let tab_border_width = border_width_px.min(w.saturating_sub(1) / 2).min(h / 2);
        let tab_padding_x = padding_x_px.min(w.saturating_sub(1) / 2);

        let (bg, mut fg, border) = tab_colors(config, tab, is_active_workspace);
        if target.should_block_out(tab.block_out_from) {
            fg = bg;
        }
        set_source_color(&cr, bg);
        cr.rectangle(f64::from(x), f64::from(y), f64::from(w), f64::from(h));
        cr.fill()?;

        if tab_border_width > 0 {
            set_source_color(&cr, border);
            let bw = tab_border_width;
            cr.rectangle(f64::from(x), f64::from(y), f64::from(w), f64::from(bw));
            // The selected tab runs into the strip below without a seam: they are the same
            // color and together they are the top of the frame around the window, the way
            // i3 gives a title bar and its `child_border` one value. A rim there draws a
            // line across the middle of that frame.
            let opens_into_strip = extra_height > 0
                && tab.is_focused
                && (layout == Layout::Tabbed || idx + 1 == tab_count);
            if !opens_into_strip {
                cr.rectangle(
                    f64::from(x),
                    f64::from(y + h - bw),
                    f64::from(w),
                    f64::from(bw),
                );
            }
            cr.rectangle(f64::from(x), f64::from(y), f64::from(bw), f64::from(h));
            cr.rectangle(
                f64::from(x + w - bw),
                f64::from(y),
                f64::from(bw),
                f64::from(h),
            );
            cr.fill()?;
        }

        let title = sanitize_title(&tab.title);
        let text_width = (w - tab_padding_x * 2).max(1);
        text_layout.set_width(text_width * pango::SCALE);
        text_layout.set_text(&title);
        let (_tw, th) = text_layout.pixel_size();
        let text_x = x + tab_padding_x;
        let text_area_height = (h - padding_y_px * 2).max(1);
        let text_y = y + padding_y_px + ((text_area_height - th) / 2).max(0);

        cr.save()?;
        cr.rectangle(f64::from(x), f64::from(y), f64::from(w), f64::from(h));
        cr.clip();

        set_source_color(&cr, fg);
        cr.move_to(f64::from(text_x), f64::from(text_y));
        pangocairo::functions::show_layout(&cr, &text_layout);
        cr.restore()?;

        if separator_width_px > 0 && idx + 1 < tab_count {
            set_source_color(&cr, config.separator_color);
            if layout == Layout::Tabbed {
                cr.rectangle(
                    f64::from(x + w - separator_width_px),
                    f64::from(y),
                    f64::from(separator_width_px),
                    f64::from(h),
                );
            } else {
                cr.rectangle(
                    f64::from(x),
                    f64::from(y + h - separator_width_px),
                    f64::from(w),
                    f64::from(separator_width_px),
                );
            }
            cr.fill()?;
        }

        cursor_x += w;
    }

    if extra_height > 0 {
        let focused = tabs.iter().find(|tab| tab.is_focused).unwrap_or(&tabs[0]);
        let (bg, _fg, _border) = tab_colors(config, focused, is_active_workspace);
        set_source_color(&cr, bg);
        cr.rectangle(
            0.0,
            f64::from(height_px - extra_height),
            f64::from(width_px),
            f64::from(extra_height),
        );
        cr.fill()?;
    }

    drop(text_layout);
    drop(cr);

    let data = surface
        .take_data()
        .context("failed to read tab bar surface data")?;
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width_px, height_px),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(TabBarRenderOutput {
        buffer,
        tab_widths_px: tab_widths,
    })
}

/// Per-tab pixel widths for an evenly split tabbed bar. The remainder is distributed one
/// pixel at a time starting from the first tab, matching how the bar is rendered.
pub fn even_tab_widths_px(width_px: i32, tab_count: usize) -> Vec<i32> {
    let tab_count_i32 = tab_count as i32;
    let base = width_px / tab_count_i32;
    let mut widths = vec![base.max(1); tab_count];
    let remainder = width_px - base * tab_count_i32;
    for width in widths.iter_mut().take(remainder as usize) {
        *width += 1;
    }
    widths
}

/// Map a position to the tab index it hits inside a tab bar, or None when it misses the
/// bar (or the bar belongs to a split container).
///
/// `cached_widths` are the per-tab pixel widths of the rendered bar when available;
/// otherwise an even split matching the renderer is assumed. `hit_pad_px` expands the hit
/// box, making the bar's edges more forgiving to hit.
pub fn tab_bar_hit_index(
    info: &TabBarInfo,
    pos: Point<f64, Logical>,
    scale: f64,
    cached_widths: Option<&[i32]>,
    hit_pad_px: i32,
) -> Option<usize> {
    let tab_count = info.tabs.len();
    if tab_count == 0 {
        return None;
    }

    let scale_2d = Scale::from(scale);
    let bar_loc_px: Point<i32, Physical> = info.rect.loc.to_physical_precise_round(scale_2d);
    let pos_px: Point<i32, Physical> = pos.to_physical_precise_round(scale_2d) - bar_loc_px;
    let width_px = to_physical_precise_round::<i32>(scale, info.rect.size.w).max(1);
    let height_px = to_physical_precise_round::<i32>(scale, info.rect.size.h).max(1);

    if pos_px.x < -hit_pad_px
        || pos_px.y < -hit_pad_px
        || pos_px.x >= width_px + hit_pad_px
        || pos_px.y >= height_px + hit_pad_px
    {
        return None;
    }
    let pos_px: Point<i32, Physical> = Point::from((
        pos_px.x.clamp(0, width_px - 1),
        pos_px.y.clamp(0, height_px - 1),
    ));

    let row_height_px = to_physical_precise_round::<i32>(scale, info.row_height).max(1);
    let focused_idx = info.tabs.iter().position(|tab| tab.is_focused).unwrap_or(0);

    match info.layout {
        Layout::Tabbed => {
            if pos_px.y >= row_height_px {
                return Some(focused_idx);
            }
            let fallback;
            let widths: &[i32] = match cached_widths.filter(|widths| widths.len() == tab_count) {
                Some(widths) => widths,
                None => {
                    fallback = even_tab_widths_px(width_px, tab_count);
                    &fallback
                }
            };
            let mut cursor = 0;
            for (idx, width) in widths.iter().enumerate() {
                cursor += *width;
                if pos_px.x < cursor {
                    return Some(idx);
                }
            }
            Some(tab_count.saturating_sub(1))
        }
        Layout::Stacked => {
            let stack_height_px = row_height_px * tab_count as i32;
            if pos_px.y >= stack_height_px {
                return Some(focused_idx);
            }
            let max_idx = tab_count.saturating_sub(1) as i32;
            Some(((pos_px.y / row_height_px).min(max_idx)) as usize)
        }
        Layout::SplitH | Layout::SplitV => None,
    }
}
