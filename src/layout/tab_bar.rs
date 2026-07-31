use std::borrow::Cow;

use anyhow::{bail, Context, Result};
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{self, Alignment, EllipsizeMode, FontDescription};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Logical, Rectangle, Size, Transform};
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

fn tab_colors(
    config: &TabBar,
    tab: &TabBarTab,
    is_active_workspace: bool,
) -> (Color, Color, Color) {
    if tab.is_urgent {
        (config.urgent_bg, config.urgent_fg, config.urgent_border)
    } else if tab.is_focused && is_active_workspace {
        (config.active_bg, config.active_fg, config.active_border)
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
        .map(|tab| TabBarTabState {
            title: tab.title.clone(),
            is_focused: tab.is_focused && is_active,
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
        let tab_count_i32 = tab_count as i32;
        let base = width_px / tab_count_i32;
        let mut widths = vec![base.max(1); tab_count];
        let remainder = width_px - base * tab_count_i32;
        for width in widths.iter_mut().take(remainder as usize) {
            *width += 1;
        }
        widths
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
            cr.rectangle(
                f64::from(x),
                f64::from(y + h - bw),
                f64::from(w),
                f64::from(bw),
            );
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

    let row_count = if layout == Layout::Tabbed {
        1
    } else {
        tab_count
    };
    let extra_height = height_px - row_height_px.saturating_mul(row_count as i32);
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
