use std::num::NonZeroU32;
use std::sync::Arc;

use fontdue::{Font, FontSettings, Metrics};
use softbuffer::{Context, Surface};
use winit::window::Window;

pub(crate) const TAB_BAR_HEIGHT_LOGICAL: f64 = 32.0;
pub(crate) const TAB_BORDER_WIDTH: i32 = 1;

const FONT_BYTES: &[u8] = include_bytes!("../assets/fonts/CascadiaCode-Regular.ttf");

const COLOR_BACKDROP: u32 = 0x000F1016;
const COLOR_STRIP_BG: u32 = 0x0016161E;
const COLOR_TAB_INACTIVE: u32 = 0x001A1B26;
const COLOR_TAB_HOVER: u32 = 0x00212231;
const COLOR_TAB_ACTIVE: u32 = 0x001F2335;
const COLOR_ACCENT: u32 = 0x007AA2F7;
const COLOR_SEPARATOR: u32 = 0x00232433;
const COLOR_TEXT: u32 = 0x00C0CAF5;
const COLOR_TEXT_DIM: u32 = 0x00565F89;
const COLOR_CLOSE_BG: u32 = 0x002A2E42;
const COLOR_CLOSE_GLYPH: u32 = 0x00F7768E;
const COLOR_WINDOW_CLOSE_BG: u32 = 0x00C42B1C;
const COLOR_WHITE: u32 = 0x00FFFFFF;
pub(crate) const COLOR_BORDER_ACTIVE: u32 = 0x00414868;
pub(crate) const COLOR_BORDER_INACTIVE: u32 = 0x00232433;

#[derive(Debug)]
pub(crate) enum Hit {
    Tab(usize),
    Close(usize),
    NewTab,
    Minimize,
    Maximize,
    CloseWindow,
    None,
}

pub(crate) struct TabModel {
    pub(crate) guest_index: usize,
    pub(crate) title: String,
    pub(crate) active: bool,
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

struct TabSlot {
    tab: Rect,
    close: Rect,
    guest_index: usize,
}

pub(crate) struct TabBar {
    _context: Context<Arc<Window>>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    font: Font,
    font_px: f32,
    scale: f64,
    buffer_width: u32,
    buffer_height: u32,
    layout: Vec<TabSlot>,
    new_tab_slot: Option<Rect>,
    window_controls: Option<[Rect; 3]>,
    hover_tab: Option<usize>,
    hover_close: Option<usize>,
    hover_new_tab: bool,
    hover_control: Option<usize>,
    focused: bool,
    dirty: bool,
}

fn blend_pixel(background: u32, foreground: u32, coverage: u32) -> u32 {
    let mut result = 0;
    for shift in [16, 8, 0] {
        let fg = (foreground >> shift) & 0xFF;
        let bg = (background >> shift) & 0xFF;
        let mixed = (fg * coverage + bg * (255 - coverage) + 127) / 255;
        result |= mixed << shift;
    }
    result
}

impl TabBar {
    pub(crate) fn new(window: &Arc<Window>) -> Result<Self, String> {
        let context =
            Context::new(Arc::clone(window)).map_err(|error| format!("context: {error}"))?;
        let surface = Surface::new(&context, Arc::clone(window))
            .map_err(|error| format!("surface: {error}"))?;
        let font = Font::from_bytes(
            FONT_BYTES,
            FontSettings {
                collection_index: 0,
                scale: 40.0,
                load_substitutions: false,
            },
        )
        .map_err(|error| format!("bundled font could not be parsed: {error:?}"))?;
        let scale = window.scale_factor();

        Ok(Self {
            _context: context,
            surface,
            font,
            font_px: Self::font_px_for(scale),
            scale,
            buffer_width: 0,
            buffer_height: 0,
            layout: Vec::new(),
            new_tab_slot: None,
            window_controls: None,
            hover_tab: None,
            hover_close: None,
            hover_new_tab: false,
            hover_control: None,
            focused: false,
            dirty: true,
        })
    }

    fn font_px_for(scale: f64) -> f32 {
        ((14.0 * scale) as f32).round().clamp(11.0, 26.0)
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn update_scale(&mut self, scale: f64) {
        if (scale - self.scale).abs() > f64::EPSILON {
            self.scale = scale;
            self.font_px = Self::font_px_for(scale);
            self.dirty = true;
        }
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        if focused != self.focused {
            self.focused = focused;
            self.dirty = true;
        }
    }

    pub(crate) fn set_hover(&mut self, hit: Hit) {
        let (tab, close, new_tab, control) = match hit {
            Hit::Tab(index) => (Some(index), None, false, None),
            Hit::Close(index) => (None, Some(index), false, None),
            Hit::NewTab => (None, None, true, None),
            Hit::Minimize => (None, None, false, Some(0)),
            Hit::Maximize => (None, None, false, Some(1)),
            Hit::CloseWindow => (None, None, false, Some(2)),
            Hit::None => (None, None, false, None),
        };

        if tab != self.hover_tab
            || close != self.hover_close
            || new_tab != self.hover_new_tab
            || control != self.hover_control
        {
            self.hover_tab = tab;
            self.hover_close = close;
            self.hover_new_tab = new_tab;
            self.hover_control = control;
            self.dirty = true;
        }
    }

    pub(crate) fn clear_hover(&mut self) {
        self.set_hover(Hit::None);
    }

    pub(crate) fn hit_test(&self, x: i32, y: i32) -> Hit {
        if let Some(controls) = &self.window_controls {
            for (index, rect) in controls.iter().enumerate() {
                if rect.contains(x, y) {
                    return match index {
                        0 => Hit::Minimize,
                        1 => Hit::Maximize,
                        _ => Hit::CloseWindow,
                    };
                }
            }
        }

        if let Some(new_tab) = self.new_tab_slot
            && new_tab.contains(x, y)
        {
            return Hit::NewTab;
        }

        for slot in &self.layout {
            if slot.close.contains(x, y) {
                return Hit::Close(slot.guest_index);
            }
            if slot.tab.contains(x, y) {
                return Hit::Tab(slot.guest_index);
            }
        }

        Hit::None
    }

    pub(crate) fn draw(&mut self, window: &Window, tabs: &[TabModel]) {
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }

        let width = size.width.max(1);
        let height = size.height.max(1);

        if width != self.buffer_width || height != self.buffer_height {
            let resize = self.surface.resize(
                NonZeroU32::new(width).expect("width is nonzero"),
                NonZeroU32::new(height).expect("height is nonzero"),
            );
            if let Err(error) = resize {
                eprintln!("Could not resize the tab bar surface: {error}");
                return;
            }
            self.buffer_width = width;
            self.buffer_height = height;
        }

        let Ok(mut buffer) = self.surface.buffer_mut() else {
            return;
        };

        let pixels: &mut [u32] = &mut buffer;
        let stride = width as i32;
        let canvas_height = height as i32;
        pixels.fill(COLOR_BACKDROP);

        let strip_height =
            ((TAB_BAR_HEIGHT_LOGICAL * self.scale).round() as i32).min(canvas_height);
        for y in 0..strip_height {
            let row_start = (y * stride) as usize;
            let row = &mut pixels[row_start..row_start + stride as usize];
            row.fill(COLOR_STRIP_BG);
        }

        self.layout.clear();
        self.new_tab_slot = None;
        self.window_controls = None;

        let scaled = |value: f64| -> i32 { (value * self.scale).round() as i32 };
        let padding = scaled(6.0);
        let gap = scaled(4.0);
        let inset_y = scaled(4.0);
        let min_tab_width = scaled(90.0);
        let max_tab_width = scaled(220.0);
        let control_size = strip_height;
        let controls_space = control_size * 3;

        let line_metrics = self.font.horizontal_line_metrics(self.font_px);
        let (ascent, descent) = match line_metrics {
            Some(metrics) => (metrics.ascent, metrics.descent),
            None => (self.font_px * 0.8, self.font_px * 0.2),
        };

        let count = tabs.len() as i32;
        if strip_height > inset_y * 2 {
            let tab_height = strip_height - inset_y * 2;
            let new_tab_size = tab_height;
            let button_space = new_tab_size + gap;
            let available =
                (stride - padding * 2 - controls_space - button_space - gap * (count - 1)).max(0);
            let tab_width = if count > 0 && available >= count * min_tab_width {
                (available / count).min(max_tab_width)
            } else {
                min_tab_width
            };

            let mut current_x = padding;

            for (slot, model) in tabs.iter().enumerate() {
                let tab_x = padding + slot as i32 * (tab_width + gap);
                current_x = tab_x + tab_width + gap;
                let tab_rect = Rect {
                    x: tab_x,
                    y: inset_y,
                    w: tab_width,
                    h: tab_height,
                };
                let close_side = (tab_height - 2 * scaled(7.0)).max(scaled(10.0));
                let close_rect = Rect {
                    x: tab_x + tab_width - close_side - scaled(6.0),
                    y: inset_y + (tab_height - close_side) / 2,
                    w: close_side,
                    h: close_side,
                };

                let hovered = self.hover_tab == Some(model.guest_index);
                let background = if model.active {
                    COLOR_TAB_ACTIVE
                } else if hovered {
                    COLOR_TAB_HOVER
                } else {
                    COLOR_TAB_INACTIVE
                };
                Self::fill_rect(pixels, stride, canvas_height, tab_rect, background);

                if model.active {
                    let accent_height = scaled(2.0).max(2);
                    Self::fill_rect(
                        pixels,
                        stride,
                        canvas_height,
                        Rect {
                            x: tab_rect.x,
                            y: tab_rect.y + tab_rect.h - accent_height,
                            w: tab_rect.w,
                            h: accent_height,
                        },
                        COLOR_ACCENT,
                    );
                }

                if slot > 0 {
                    let separator_x = tab_rect.x - ((gap + 1) / 2).max(1);
                    Self::fill_rect(
                        pixels,
                        stride,
                        canvas_height,
                        Rect {
                            x: separator_x,
                            y: inset_y + scaled(3.0),
                            w: 1,
                            h: tab_height - scaled(6.0),
                        },
                        COLOR_SEPARATOR,
                    );
                }

                let text_color = if model.active || hovered {
                    COLOR_TEXT
                } else {
                    COLOR_TEXT_DIM
                };
                let baseline =
                    inset_y as f32 + (tab_height as f32 - (ascent - descent)) / 2.0 + ascent;
                Self::draw_text(
                    pixels,
                    stride,
                    canvas_height,
                    &self.font,
                    self.font_px,
                    &model.title,
                    tab_rect.x + scaled(12.0),
                    baseline.round() as i32,
                    close_rect.x - scaled(8.0),
                    text_color,
                );

                if self.hover_close == Some(model.guest_index) {
                    Self::fill_rect(pixels, stride, canvas_height, close_rect, COLOR_CLOSE_BG);
                }
                let glyph_color = if self.hover_close == Some(model.guest_index) {
                    COLOR_CLOSE_GLYPH
                } else {
                    COLOR_TEXT_DIM
                };
                let glyph_inset = (close_rect.w as f32 * 0.3).round() as i32;
                let (lx0, ly0) = (close_rect.x + glyph_inset, close_rect.y + glyph_inset);
                let (lx1, ly1) = (
                    close_rect.x + close_rect.w - glyph_inset - 1,
                    close_rect.y + close_rect.h - glyph_inset - 1,
                );
                Self::draw_line(
                    pixels,
                    stride,
                    canvas_height,
                    lx0,
                    ly0,
                    lx1,
                    ly1,
                    glyph_color,
                );
                Self::draw_line(
                    pixels,
                    stride,
                    canvas_height,
                    lx0,
                    ly1,
                    lx1,
                    ly0,
                    glyph_color,
                );

                self.layout.push(TabSlot {
                    tab: tab_rect,
                    close: close_rect,
                    guest_index: model.guest_index,
                });
            }

            if current_x + new_tab_size <= stride - padding - controls_space {
                let new_tab_rect = Rect {
                    x: current_x,
                    y: inset_y,
                    w: new_tab_size,
                    h: tab_height,
                };
                self.new_tab_slot = Some(new_tab_rect);

                let bg = if self.hover_new_tab {
                    COLOR_TAB_HOVER
                } else {
                    COLOR_TAB_INACTIVE
                };
                Self::fill_rect(pixels, stride, canvas_height, new_tab_rect, bg);

                let glyph_color = if self.hover_new_tab {
                    COLOR_TEXT
                } else {
                    COLOR_TEXT_DIM
                };
                let cx = new_tab_rect.x + new_tab_rect.w / 2;
                let cy = new_tab_rect.y + new_tab_rect.h / 2;
                let arm = scaled(4.0).max(3);
                Self::draw_line(
                    pixels,
                    stride,
                    canvas_height,
                    cx - arm,
                    cy,
                    cx + arm,
                    cy,
                    glyph_color,
                );
                Self::draw_line(
                    pixels,
                    stride,
                    canvas_height,
                    cx,
                    cy - arm,
                    cx,
                    cy + arm,
                    glyph_color,
                );
            }

            let controls_origin_x = stride - controls_space;
            if controls_origin_x >= 0 {
                let control_rects = [
                    Rect {
                        x: controls_origin_x,
                        y: 0,
                        w: control_size,
                        h: strip_height,
                    },
                    Rect {
                        x: controls_origin_x + control_size,
                        y: 0,
                        w: control_size,
                        h: strip_height,
                    },
                    Rect {
                        x: controls_origin_x + control_size * 2,
                        y: 0,
                        w: control_size,
                        h: strip_height,
                    },
                ];
                self.window_controls = Some(control_rects);

                for rect in &control_rects[1..] {
                    Self::fill_rect(
                        pixels,
                        stride,
                        canvas_height,
                        Rect {
                            x: rect.x,
                            y: 0,
                            w: 1,
                            h: strip_height,
                        },
                        COLOR_SEPARATOR,
                    );
                }

                let maximized = window.is_maximized();
                let glyph = scaled(4.0).max(3);
                let baseline = scaled(2.0).max(2);

                for (index, rect) in control_rects.iter().enumerate() {
                    let cx = rect.x + rect.w / 2;
                    let cy = rect.y + rect.h / 2;
                    let hovered = self.hover_control == Some(index);

                    match index {
                        0 => {
                            if hovered {
                                Self::fill_rect(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    *rect,
                                    COLOR_TAB_HOVER,
                                );
                            }
                            let color = if hovered { COLOR_TEXT } else { COLOR_TEXT_DIM };
                            Self::draw_line(
                                pixels,
                                stride,
                                canvas_height,
                                cx - glyph,
                                cy + baseline,
                                cx + glyph,
                                cy + baseline,
                                color,
                            );
                        }
                        1 => {
                            if hovered {
                                Self::fill_rect(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    *rect,
                                    COLOR_TAB_HOVER,
                                );
                            }
                            let color = if hovered { COLOR_TEXT } else { COLOR_TEXT_DIM };
                            if maximized {
                                let front_x0 = cx - glyph;
                                let front_y0 = cy - glyph + baseline;
                                let front_x1 = cx + glyph;
                                let front_y1 = cy + glyph + baseline;
                                let back_x0 = cx - glyph + baseline;
                                let back_y0 = cy - glyph - baseline;
                                let back_x1 = cx + glyph + baseline;
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    back_x0,
                                    back_y0,
                                    back_x1,
                                    back_y0,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    back_x1,
                                    back_y0,
                                    back_x1,
                                    cy + glyph - baseline,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    front_x0,
                                    front_y0,
                                    front_x1,
                                    front_y0,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    front_x0,
                                    front_y1,
                                    front_x1,
                                    front_y1,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    front_x0,
                                    front_y0,
                                    front_x0,
                                    front_y1,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    front_x1,
                                    front_y0,
                                    front_x1,
                                    front_y1,
                                    color,
                                );
                            } else {
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    cx - glyph,
                                    cy - glyph,
                                    cx + glyph,
                                    cy - glyph,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    cx - glyph,
                                    cy + glyph,
                                    cx + glyph,
                                    cy + glyph,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    cx - glyph,
                                    cy - glyph,
                                    cx - glyph,
                                    cy + glyph,
                                    color,
                                );
                                Self::draw_line(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    cx + glyph,
                                    cy - glyph,
                                    cx + glyph,
                                    cy + glyph,
                                    color,
                                );
                            }
                        }
                        _ => {
                            if hovered {
                                Self::fill_rect(
                                    pixels,
                                    stride,
                                    canvas_height,
                                    *rect,
                                    COLOR_WINDOW_CLOSE_BG,
                                );
                            }
                            let color = if hovered { COLOR_WHITE } else { COLOR_TEXT_DIM };
                            Self::draw_line(
                                pixels,
                                stride,
                                canvas_height,
                                cx - glyph,
                                cy - glyph,
                                cx + glyph,
                                cy + glyph,
                                color,
                            );
                            Self::draw_line(
                                pixels,
                                stride,
                                canvas_height,
                                cx - glyph,
                                cy + glyph,
                                cx + glyph,
                                cy - glyph,
                                color,
                            );
                        }
                    }
                }
            }
        }

        if !window.is_maximized() {
            let border_color = if self.focused {
                COLOR_BORDER_ACTIVE
            } else {
                COLOR_BORDER_INACTIVE
            };
            let max_x = stride - 1;
            let max_y = canvas_height - 1;
            Self::draw_line(pixels, stride, canvas_height, 0, 0, max_x, 0, border_color);
            Self::draw_line(
                pixels,
                stride,
                canvas_height,
                0,
                max_y,
                max_x,
                max_y,
                border_color,
            );
            Self::draw_line(pixels, stride, canvas_height, 0, 0, 0, max_y, border_color);
            Self::draw_line(
                pixels,
                stride,
                canvas_height,
                max_x,
                0,
                max_x,
                max_y,
                border_color,
            );
        }

        if let Err(error) = buffer.present() {
            eprintln!("Could not present the tab bar frame: {error}");
            self.dirty = true;
        }
    }

    fn fill_rect(pixels: &mut [u32], stride: i32, height: i32, rect: Rect, color: u32) {
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = (rect.x + rect.w).min(stride);
        let y1 = (rect.y + rect.h).min(height);

        for y in y0..y1 {
            let row_start = (y * stride) as usize;
            let row = &mut pixels[row_start..row_start + stride as usize];
            row[x0 as usize..x1 as usize].fill(color);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        pixels: &mut [u32],
        stride: i32,
        height: i32,
        font: &Font,
        font_px: f32,
        text: &str,
        start_x: i32,
        baseline: i32,
        max_x: i32,
        color: u32,
    ) {
        let mut pen = start_x as f32;

        for character in text.chars() {
            let glyph_index = font.lookup_glyph_index(character);
            let (metrics, bitmap) = font.rasterize_indexed(glyph_index, font_px);

            let advance = metrics.advance_width;
            if pen + advance > max_x as f32 {
                let (ellipsis_metrics, ellipsis_bitmap) = font.rasterize('…', font_px);
                if pen + ellipsis_metrics.advance_width <= max_x as f32 {
                    Self::blit_glyph(
                        pixels,
                        stride,
                        height,
                        &ellipsis_bitmap,
                        &ellipsis_metrics,
                        pen.round() as i32,
                        baseline,
                        color,
                    );
                }
                break;
            }

            Self::blit_glyph(
                pixels,
                stride,
                height,
                &bitmap,
                &metrics,
                pen.round() as i32,
                baseline,
                color,
            );
            pen += advance;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn blit_glyph(
        pixels: &mut [u32],
        stride: i32,
        height: i32,
        bitmap: &[u8],
        metrics: &Metrics,
        pen_x: i32,
        baseline: i32,
        color: u32,
    ) {
        if metrics.width == 0 || metrics.height == 0 {
            return;
        }

        let left = pen_x + metrics.xmin;
        let top = baseline - metrics.ymin - metrics.height as i32;

        for row in 0..metrics.height {
            let y = top + row as i32;
            if y < 0 || y >= height {
                continue;
            }

            for col in 0..metrics.width {
                let coverage = bitmap[row * metrics.width + col] as u32;
                if coverage == 0 {
                    continue;
                }

                let x = left + col as i32;
                if x < 0 || x >= stride {
                    continue;
                }

                let index = (y * stride + x) as usize;
                pixels[index] = blend_pixel(pixels[index], color, coverage);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        pixels: &mut [u32],
        stride: i32,
        height: i32,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        color: u32,
    ) {
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);

        for t in 0..=steps {
            let x = x0 + (x1 - x0) * t / steps;
            let y = y0 + (y1 - y0) * t / steps;
            if x < 0 || x >= stride || y < 0 || y >= height {
                continue;
            }

            pixels[(y * stride + x) as usize] = color;
        }
    }
}
