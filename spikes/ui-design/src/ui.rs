//! The two screens of the design brief (`ziz-design-brief.md`): the room
//! list and a room with its live screens and the side panel. Laid out by
//! hand from the window size (Dear ImGui has no grid), drawn with the draw
//! list; widgets are invisible buttons with custom drawing. Sample data.

use dear_imgui_rs::{DrawFlags, DrawListMut, FontId, MouseCursor, Ui, WindowFlags};

// ---------------------------------------------------------------- palette

const BG: u32 = 0x0E1220;
const BAR: u32 = 0x161B2E;
const CARD: u32 = 0x161B2E;
const PREVIEW: u32 = 0x0A0D18;
const ACTIVE: u32 = 0x2A3250;
const ACTIVE_BORDER: u32 = 0x3A4466;
const AVATAR: u32 = 0x232A42;
const BORDER: u32 = 0x262E48;
const DIVIDER: u32 = 0x1F2640;
const ACCENT: u32 = 0x0047FF;
const TEXT: u32 = 0xE8ECF7;
const MUTED: u32 = 0x8A93AD;
const STREAMING_BG: u32 = 0x14244F;
const STREAMING_TEXT: u32 = 0x8FB0FF;
const LIVE_DOT: u32 = 0xFF3B30;
const EMPTY_BORDER: u32 = 0x2E3653;
const WHITE: u32 = 0xFFFFFF;

fn c(rgb: u32) -> [f32; 4] {
    rgba(rgb)
}

/// Slightly lighter, for hover.
fn hover(rgb: u32) -> u32 {
    let up = |v: u32| (v + (255 - v) / 10).min(255);
    (up((rgb >> 16) & 0xFF) << 16) | (up((rgb >> 8) & 0xFF) << 8) | up(rgb & 0xFF)
}

pub fn clear_color() -> [f32; 4] {
    [((BG >> 16) & 0xFF) as f32 / 255.0, ((BG >> 8) & 0xFF) as f32 / 255.0, (BG & 0xFF) as f32 / 255.0, 1.0]
}

// ---------------------------------------------------------------- data

pub struct Fonts {
    pub regular: FontId,
    pub medium: FontId,
    pub semibold: FontId,
    pub bold: FontId,
    pub logo: FontId,
    /// Line height over em size of Inter and of the logo font: Dear ImGui
    /// sizes fonts by line height (ascent - descent), CSS by em, so a CSS
    /// size is multiplied by this to match the brief.
    pub inter_scale: f32,
    pub logo_scale: f32,
}

impl Fonts {
    pub fn scale(&self, font: FontId) -> f32 {
        if font == self.logo { self.logo_scale } else { self.inter_scale }
    }
}

struct Room {
    name: &'static str,
    watching: u32,
    /// Streams (indices into the video tiles) live in this room.
    screens: &'static [usize],
}

const ROOMS: [Room; 6] = [
    Room { name: "Equipe design", watching: 5, screens: &[0, 1] },
    Room { name: "Revisão de código", watching: 3, screens: &[2] },
    Room { name: "Jogatina de sexta", watching: 8, screens: &[0, 1, 2] },
    Room { name: "Aula de matemática", watching: 12, screens: &[1] },
    Room { name: "Suporte da Bia", watching: 1, screens: &[2] },
    Room { name: "Sala do Leo", watching: 0, screens: &[] },
];
const STREAMERS: [&str; 3] = ["Ana", "Bruno", "Carla"];
const PEOPLE: [(&str, bool); 8] = [
    ("Ana", true),
    ("Bruno", true),
    ("Carla", true),
    ("Daniel", false),
    ("Eva", false),
    ("Felipe", false),
    ("Gabi", false),
    ("Hugo", false),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Preset {
    Light,
    Normal,
    High,
    Manual,
}

pub struct State {
    room: Option<usize>,
    tab: usize,
    search: String,
    panel: usize,
    preset: Preset,
    resolution: usize,
    fps: usize,
    bitrate: f32,
    featured: usize,
    maximized: bool,
    streaming: bool,
    /// Room list previews show recent stills (false: placeholders).
    pub live_previews: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            room: None,
            tab: 0,
            search: String::new(),
            panel: 0,
            preset: Preset::Manual,
            resolution: 1,
            fps: 2,
            bitrate: 6000.0,
            featured: 0,
            maximized: false,
            streaming: false,
            live_previews: true,
        }
    }
}

impl State {
    pub fn open_room(&mut self, index: usize) {
        self.room = Some(index);
    }

    pub fn in_room_list(&self) -> bool {
        self.room.is_none()
    }

    /// Streams on screen now.
    pub fn visible_streams(&self) -> Vec<usize> {
        match self.room {
            Some(room) => ROOMS[room].screens.to_vec(),
            None if self.live_previews => vec![0, 1, 2],
            None => Vec::new(),
        }
    }
}

/// Video for stream `index` at a size: a texture id, or None before the first picture.
/// Video for stream `index` drawn over a `rect` (w, h), cropped to fill
/// (`cover`) or letterboxed: the texture id and the picture size, or None
/// before the first picture. The texture is made at the size the picture is
/// drawn, in its own aspect ratio (no bars baked in, no blurry downscale).
pub type VideoSource<'a> = &'a mut dyn FnMut(usize, [f32; 2], bool) -> Option<(u64, (u32, u32))>;

// ---------------------------------------------------------------- drawing helpers

struct Painter<'a> {
    ui: &'a Ui,
    draw: DrawListMut<'a>,
    fonts: &'a Fonts,
}

impl Painter<'_> {
    fn text_size(&self, font: FontId, size: f32, text: &str) -> [f32; 2] {
        let _token = self.ui.push_font_with_size(Some(font), size * self.fonts.scale(font));
        self.ui.calc_text_size(text)
    }

    fn text(&self, font: FontId, size: f32, pos: [f32; 2], color: u32, text: &str) {
        // Whole pixels: text at fractional positions is resampled and blurs.
        let pos = [pos[0].round(), pos[1].round()];
        self.draw.add_text_with_font(font, size * self.fonts.scale(font), pos, c(color), text, 0.0, None);
    }

    fn rect(&self, min: [f32; 2], max: [f32; 2], fill: Option<u32>, border: Option<(u32, f32)>, rounding: f32) {
        if let Some(fill) = fill {
            self.draw.add_rect(min, max, c(fill)).filled(true).rounding(rounding).build();
        }
        if let Some((color, thickness)) = border {
            // Strokes centered on the edge: inset half the thickness to stay inside.
            let h = thickness / 2.0;
            self.draw
                .add_rect([min[0] + h, min[1] + h], [max[0] - h, max[1] - h], c(color))
                .rounding((rounding - h).max(0.0))
                .thickness(thickness)
                .build();
        }
    }

    /// An invisible button over a rectangle; (clicked, hovered).
    fn hit(&self, id: &str, min: [f32; 2], max: [f32; 2]) -> (bool, bool) {
        self.ui.set_cursor_screen_pos(min);
        let clicked = self.ui.invisible_button(id, [(max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0)]);
        let hovered = self.ui.is_item_hovered();
        if hovered {
            self.ui.set_mouse_cursor(Some(MouseCursor::Hand));
        }
        (clicked, hovered)
    }

    /// A pill button; returns clicked.
    fn pill(&self, id: &str, min: [f32; 2], size: [f32; 2], fill: u32, border: Option<u32>, label: &str, font: FontId, font_size: f32, color: u32) -> bool {
        let max = [min[0] + size[0], min[1] + size[1]];
        let (clicked, hovered) = self.hit(id, min, max);
        let fill = if hovered { hover(fill) } else { fill };
        self.rect(min, max, Some(fill), border.map(|b| (b, 1.0)), size[1] / 2.0);
        let t = self.text_size(font, font_size, label);
        self.text(font, font_size, [min[0] + (size[0] - t[0]) / 2.0, min[1] + (size[1] - t[1]) / 2.0], color, label);
        clicked
    }

    /// A badge pill with optional dot; returns its width.
    fn badge(&self, pos: [f32; 2], text: &str, bg: u32, fg: u32, dot: Option<u32>, pad: [f32; 2], size: f32, border: Option<u32>) -> f32 {
        let t = self.text_size(self.fonts.bold, size, text);
        let dot_w = if dot.is_some() { 8.0 + 6.0 } else { 0.0 };
        let w = pad[0] * 2.0 + dot_w + t[0];
        let h = pad[1] * 2.0 + t[1];
        self.rect(pos, [pos[0] + w, pos[1] + h], Some(bg), border.map(|b| (b, 1.0)), h / 2.0);
        if let Some(dot) = dot {
            self.draw.add_circle([pos[0] + pad[0] + 4.0, pos[1] + h / 2.0], 4.0, c(dot)).filled(true).build();
        }
        self.text(self.fonts.bold, size, [pos[0] + pad[0] + dot_w, pos[1] + pad[1]], fg, text);
        w
    }

    fn badge_height(&self, pad_y: f32, size: f32) -> f32 {
        pad_y * 2.0 + self.text_size(self.fonts.bold, size, "Ag")[1]
    }

    // Stroke icons on a 24-unit grid, like the SVGs of the brief.
    fn icon_lines(&self, origin: [f32; 2], scale: f32, color: u32, thickness: f32, lines: &[[f32; 4]]) {
        for l in lines {
            self.draw
                .add_line([origin[0] + l[0] * scale, origin[1] + l[1] * scale], [origin[0] + l[2] * scale, origin[1] + l[3] * scale], c(color))
                .thickness(thickness)
                .build();
        }
    }

    fn icon_monitor(&self, origin: [f32; 2], size: f32, color: u32, thickness: f32) {
        let s = size / 24.0;
        self.draw
            .add_rect([origin[0] + 3.0 * s, origin[1] + 4.0 * s], [origin[0] + 21.0 * s, origin[1] + 16.0 * s], c(color))
            .rounding(2.0 * s)
            .thickness(thickness)
            .build();
        self.icon_lines(origin, s, color, thickness, &[[8.0, 20.0, 16.0, 20.0], [12.0, 16.0, 12.0, 20.0]]);
    }

    fn icon_search(&self, origin: [f32; 2], size: f32, color: u32) {
        let s = size / 24.0;
        self.draw.add_circle([origin[0] + 11.0 * s, origin[1] + 11.0 * s], 7.0 * s, c(color)).thickness(2.2).build();
        self.icon_lines(origin, s, color, 2.2, &[[16.5, 16.5, 20.0, 20.0]]);
    }

    fn icon_plus(&self, origin: [f32; 2], size: f32, color: u32) {
        self.icon_lines(origin, size / 24.0, color, 3.0, &[[12.0, 5.0, 12.0, 19.0], [5.0, 12.0, 19.0, 12.0]]);
    }

    fn icon_back(&self, origin: [f32; 2], size: f32, color: u32) {
        self.icon_lines(origin, size / 24.0, color, 2.5, &[[15.0, 18.0, 9.0, 12.0], [9.0, 12.0, 15.0, 6.0]]);
    }

    fn icon_expand(&self, origin: [f32; 2], size: f32, color: u32) {
        let corners = [
            [4.0, 9.0, 4.0, 4.0], [4.0, 4.0, 9.0, 4.0],
            [20.0, 9.0, 20.0, 4.0], [20.0, 4.0, 15.0, 4.0],
            [4.0, 15.0, 4.0, 20.0], [4.0, 20.0, 9.0, 20.0],
            [20.0, 15.0, 20.0, 20.0], [20.0, 20.0, 15.0, 20.0],
        ];
        self.icon_lines(origin, size / 24.0, color, 2.4, &corners);
    }

    /// Dashed border of a rounded rectangle (Dear ImGui has no dashed lines):
    /// walks the outline and strokes every other stretch.
    fn dashed_rect(&self, min: [f32; 2], max: [f32; 2], rounding: f32, color: u32) {
        let (x0, y0, x1, y1, r) = (min[0] + 0.5, min[1] + 0.5, max[0] - 0.5, max[1] - 0.5, rounding);
        let mut points = Vec::new();
        let arc = |cx: f32, cy: f32, from: f32, points: &mut Vec<[f32; 2]>| {
            for i in 0..=8 {
                let a = from + std::f32::consts::FRAC_PI_2 * i as f32 / 8.0;
                points.push([cx + r * a.cos(), cy + r * a.sin()]);
            }
        };
        arc(x1 - r, y0 + r, -std::f32::consts::FRAC_PI_2, &mut points);
        arc(x1 - r, y1 - r, 0.0, &mut points);
        arc(x0 + r, y1 - r, std::f32::consts::FRAC_PI_2, &mut points);
        arc(x0 + r, y0 + r, std::f32::consts::PI, &mut points);
        points.push(points[0]);
        let (dash, gap) = (5.0, 4.0);
        let mut on = true;
        let mut left: f32 = dash;
        for pair in points.windows(2) {
            let (mut a, b) = (pair[0], pair[1]);
            let mut length = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            while length > 0.0 {
                let step = left.min(length);
                let t = step / length;
                let next = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                if on {
                    self.draw.add_line(a, next, c(color)).thickness(1.0).build();
                }
                a = next;
                length -= step;
                left -= step;
                if left <= 0.0 {
                    on = !on;
                    left = if on { dash } else { gap };
                }
            }
        }
    }

    /// Video in a rectangle: `cover` crops to fill (previews), otherwise
    /// letterboxes (shared screens must not be cropped). Returns false
    /// before the first picture.
    fn video(&self, source: VideoSource<'_>, stream: usize, min: [f32; 2], max: [f32; 2], rounding: f32, cover: bool) -> bool {
        let (w, h) = (max[0] - min[0], max[1] - min[1]);

        let Some((texture, picture)) = source(stream, [w, h], cover) else { return false };
        let aspect = picture.0 as f32 / picture.1.max(1) as f32;
        if cover {
            let (mut u0, mut v0, mut u1, mut v1) = (0.0, 0.0, 1.0, 1.0);
            if w / h > aspect {
                let visible = aspect / (w / h);
                v0 = (1.0 - visible) / 2.0;
                v1 = 1.0 - v0;
            } else {
                let visible = (w / h) / aspect;
                u0 = (1.0 - visible) / 2.0;
                u1 = 1.0 - u0;
            }
            self.draw.add_image_rounded(dear_imgui_rs::TextureId::from(texture), min, max, [u0, v0], [u1, v1], c(WHITE), rounding, DrawFlags::ROUND_ALL);
        } else {
            let (iw, ih) = if w / h > aspect { (h * aspect, h) } else { (w, w / aspect) };
            let x = min[0] + (w - iw) / 2.0;
            let y = min[1] + (h - ih) / 2.0;
            // Rounded only where the picture touches the tile's corners.
            let full = (iw - w).abs() < 1.0 && (ih - h).abs() < 1.0;
            let flags = if full {
                DrawFlags::ROUND_ALL
            } else if (iw - w).abs() < 1.0 {
                DrawFlags::ROUND_NONE
            } else {
                DrawFlags::ROUND_NONE
            };
            let r = if full { rounding } else { 0.0 };
            self.draw.add_image_rounded(dear_imgui_rs::TextureId::from(texture), [x, y], [x + iw, y + ih], [0.0, 0.0], [1.0, 1.0], c(WHITE), r, flags);
        }
        true
    }

    /// The logo: blue pill, "ziz" in Bricolage Grotesque 800 at 32 px with
    /// letters drawn one by one (-0.09 em: Dear ImGui has no letter spacing).
    /// Returns its width.
    fn logo(&self, x: f32, center_y: f32) -> f32 {
        let size = 32.0;
        let spacing = -0.09 * size;
        let widths: Vec<f32> = "ziz".chars().map(|ch| self.text_size(self.fonts.logo, size, &ch.to_string())[0]).collect();
        let text_w: f32 = widths.iter().sum::<f32>() + spacing * 2.0;
        let line = self.text_size(self.fonts.logo, size, "z")[1];
        let (pad_t, pad_x, pad_b) = (2.0, 20.0, 8.0);
        let h = pad_t + size + pad_b;
        let y = center_y - h / 2.0;
        let w = pad_x * 2.0 + text_w;
        self.rect([x, y], [x + w, y + h], Some(ACCENT), None, h / 2.0);
        // Line height 1: center the font's line box on the 32 px slot.
        let mut tx = x + pad_x;
        let ty = y + pad_t - (line - size) / 2.0;
        for (ch, width) in "ziz".chars().zip(&widths) {
            self.text(self.fonts.logo, size, [tx, ty], WHITE, &ch.to_string());
            tx += width + spacing;
        }
        w
    }

    /// A segmented control on a dark pill; returns the clicked option.
    fn segmented(&self, id: &str, min: [f32; 2], width: f32, options: &[&str], weights: &[f32], selected: Option<usize>, accent: bool, height: f32, font_size: f32) -> Option<usize> {
        let pad = 4.0;
        let gap = if height > 32.0 { 6.0 } else { 4.0 };
        let total_h = height + pad * 2.0;
        self.rect(min, [min[0] + width, min[1] + total_h], Some(BG), None, total_h / 2.0);
        let inner = width - pad * 2.0 - gap * (options.len() - 1) as f32;
        let sum: f32 = weights.iter().sum();
        let mut x = min[0] + pad;
        let mut clicked = None;
        for (i, option) in options.iter().enumerate() {
            let w = inner * weights[i] / sum;
            let bmin = [x, min[1] + pad];
            let bmax = [x + w, min[1] + pad + height];
            let (hit, hovered) = self.hit(&format!("{id}{i}"), bmin, bmax);
            if hit {
                clicked = Some(i);
            }
            let is = selected == Some(i);
            let mut color = TEXT;
            if is && accent {
                self.rect(bmin, bmax, Some(if hovered { hover(ACCENT) } else { ACCENT }), None, height / 2.0);
                color = WHITE;
            } else if is {
                self.rect(bmin, bmax, Some(ACTIVE), Some((ACTIVE_BORDER, 1.0)), height / 2.0);
            } else if hovered {
                self.rect(bmin, bmax, Some(hover(BG)), None, height / 2.0);
            }
            let t = self.text_size(self.fonts.semibold, font_size, option);
            self.text(self.fonts.semibold, font_size, [x + (w - t[0]) / 2.0, bmin[1] + (height - t[1]) / 2.0], color, option);
            x += w + gap;
        }
        clicked
    }
}

// ---------------------------------------------------------------- screens

pub fn build(ui: &Ui, fonts: &Fonts, state: &mut State, source: VideoSource<'_>) {
    let size = ui.io().display_size();
    ui.window("ziz")
        .position([0.0, 0.0], dear_imgui_rs::Condition::Always)
        .size(size, dear_imgui_rs::Condition::Always)
        .flags(WindowFlags::NO_DECORATION | WindowFlags::NO_MOVE | WindowFlags::NO_BACKGROUND | WindowFlags::NO_SAVED_SETTINGS | WindowFlags::NO_BRING_TO_FRONT_ON_FOCUS | WindowFlags::NO_SCROLL_WITH_MOUSE)
        .build(|| {
            let p = Painter { ui, draw: ui.get_window_draw_list(), fonts };
            match state.room {
                None => rooms_screen(&p, state, size, source),
                Some(room) => room_screen(&p, state, room, size, source),
            }
        });
}

fn top_bar(p: &Painter<'_>, width: f32) {
    p.rect([0.0, 0.0], [width, 88.0], Some(BAR), None, 0.0);
    p.draw.add_line([0.0, 87.5], [width, 87.5], c(DIVIDER)).thickness(1.0).build();
}

fn rooms_screen(p: &Painter<'_>, state: &mut State, [w, h]: [f32; 2], source: VideoSource<'_>) {
    top_bar(p, w);
    let mid = 44.0;
    let mut x = 32.0;
    x += p.logo(x, mid) + 24.0 + 12.0;
    for (i, tab) in ["Todas", "Ao vivo", "Minhas salas"].iter().enumerate() {
        let tw = p.text_size(p.fonts.semibold, 14.0, tab)[0] + 36.0;
        let selected = state.tab == i;
        let (fill, border, color) = if selected { (ACTIVE, ACTIVE_BORDER, WHITE) } else { (BAR, BORDER, TEXT) };
        if p.pill(&format!("tab{i}"), [x, mid - 20.0], [tw, 40.0], fill, Some(border), tab, p.fonts.semibold, 14.0, color) {
            state.tab = i;
        }
        x += tw + 8.0;
    }
    // Right side: create button and search field.
    let label = "Criar sala";
    let bw = 16.0 + 8.0 + p.text_size(p.fonts.bold, 15.0, label)[0] + 44.0;
    let bx = w - 32.0 - bw;
    let bmin = [bx, mid - 22.0];
    let (_, hovered) = p.hit("create", bmin, [bx + bw, mid + 22.0]);
    p.rect(bmin, [bx + bw, mid + 22.0], Some(if hovered { hover(ACCENT) } else { ACCENT }), Some((BORDER, 1.0)), 22.0);
    p.icon_plus([bx + 22.0, mid - 8.0], 16.0, WHITE);
    let ts = p.text_size(p.fonts.bold, 15.0, label);
    p.text(p.fonts.bold, 15.0, [bx + 22.0 + 16.0 + 8.0, mid - ts[1] / 2.0], WHITE, label);
    let sx = bx - 24.0 - 280.0;
    p.rect([sx, mid - 22.0], [sx + 280.0, mid + 22.0], Some(BAR), Some((BORDER, 1.0)), 22.0);
    p.icon_search([sx + 16.0, mid - 9.0], 18.0, MUTED);
    {
        let _font = p.ui.push_font_with_size(Some(p.fonts.medium), 14.0 * p.fonts.scale(p.fonts.medium));
        let _frame = p.ui.push_style_color(dear_imgui_rs::StyleColor::FrameBg, [0.0, 0.0, 0.0, 0.0]);
        let _text = p.ui.push_style_color(dear_imgui_rs::StyleColor::Text, rgba(TEXT));
        let _hint = p.ui.push_style_color(dear_imgui_rs::StyleColor::TextDisabled, rgba(MUTED));
        let fh = p.ui.frame_height();
        p.ui.set_cursor_screen_pos([sx + 16.0 + 18.0 + 6.0, mid - fh / 2.0]);
        p.ui.set_next_item_width(280.0 - 16.0 - 18.0 - 6.0 - 16.0);
        p.ui.input_text("##search", &mut state.search).hint("Buscar sala").build();
    }

    // Title.
    let ty = 88.0 + 28.0;
    let title = p.text_size(p.fonts.bold, 38.0, "Salas");
    p.text(p.fonts.bold, 38.0, [32.0, ty], TEXT, "Salas");
    let filter = state.search.to_lowercase();
    let rooms: Vec<usize> = (0..ROOMS.len())
        .filter(|&i| match state.tab {
            1 => !ROOMS[i].screens.is_empty(),
            2 => i == 2,
            _ => true,
        })
        .filter(|&i| filter.is_empty() || ROOMS[i].name.to_lowercase().contains(&filter))
        .collect();
    let live = rooms.iter().filter(|&&i| !ROOMS[i].screens.is_empty()).count();
    let meta = format!("{} salas · {} ao vivo", rooms.len(), live);
    let ms = p.text_size(p.fonts.regular, 15.0, &meta);
    // Baselines roughly aligned (title line box taller).
    p.text(p.fonts.regular, 15.0, [32.0 + title[0] + 16.0, ty + title[1] - ms[1] - 4.0], MUTED, &meta);

    // Grid: 3 columns, rows stretched to fill like the CSS grid.
    let top = ty + 38.0 + 20.0;
    let (left, right, bottom, gap) = (32.0, 40.0, 36.0, 28.0);
    let col_w = (w - left - right - gap * 2.0) / 3.0;
    let rows = rooms.len().div_ceil(3).max(1);
    // Natural height (no stretching to the window: one row would become a tower).
    let _ = (h, bottom, rows);
    let card_h = 14.0 + 150.0 + 16.0 + p.text_size(p.fonts.bold, 17.0, "Ag")[1] + 4.0 + p.text_size(p.fonts.regular, 13.0, "Ag")[1] + 14.0;
    for (slot, &room) in rooms.iter().enumerate() {
        let x = left + (slot % 3) as f32 * (col_w + gap);
        let y = top + (slot / 3) as f32 * (card_h + gap);
        if room_card(p, room, [x, y], [col_w, card_h], state.live_previews, source) {
            state.open_room(room);
        }
    }
}

fn rgba(rgb: u32) -> [f32; 4] {
    [((rgb >> 16) & 0xFF) as f32 / 255.0, ((rgb >> 8) & 0xFF) as f32 / 255.0, (rgb & 0xFF) as f32 / 255.0, 1.0]
}

/// One card; returns true when "Entrar" is clicked.
fn room_card(p: &Painter<'_>, index: usize, min: [f32; 2], [w, h]: [f32; 2], live_previews: bool, source: VideoSource<'_>) -> bool {
    let room = &ROOMS[index];
    let empty = room.screens.is_empty();
    let max = [min[0] + w, min[1] + h];
    p.rect(min, max, Some(CARD), None, 16.0);
    if empty {
        p.dashed_rect(min, max, 16.0, EMPTY_BORDER);
    } else {
        p.rect(min, max, None, Some((BORDER, 1.0)), 16.0);
    }
    let pmin = [min[0] + 14.0, min[1] + 14.0];
    let pmax = [max[0] - 14.0, pmin[1] + 150.0];
    if empty {
        p.rect(pmin, pmax, Some(AVATAR), None, 14.0);
        let label = "Ninguém transmitindo";
        let ls = p.text_size(p.fonts.semibold, 13.0, label);
        let block = 30.0 + 8.0 + ls[1];
        let top = pmin[1] + (150.0 - block) / 2.0;
        p.icon_monitor([(pmin[0] + pmax[0]) / 2.0 - 15.0, top], 30.0, MUTED, 2.0);
        p.text(p.fonts.semibold, 13.0, [(pmin[0] + pmax[0] - ls[0]) / 2.0, top + 38.0], MUTED, label);
    } else {
        p.rect(pmin, pmax, Some(PREVIEW), None, 14.0);
        let shown = live_previews && p.video(source, room.screens[0], pmin, pmax, 14.0, true);
        if !shown {
            // The brief's placeholder: a window shape.
            p.rect([pmin[0] + 28.0, pmin[1] + 40.0], [pmax[0] - 28.0, pmax[1]], Some(0x1B2A55), None, 8.0);
            p.rect([pmin[0] + 28.0, pmin[1] + 40.0], [pmax[0] - 28.0, pmin[1] + 54.0], Some(0x2A3B6E), None, 8.0);
        }
        p.badge([pmin[0] + 10.0, pmin[1] + 10.0], "AO VIVO", BAR, TEXT, Some(LIVE_DOT), [10.0, 4.0], 12.0, None);
        let screens = if room.screens.len() == 1 { "1 tela".to_owned() } else { format!("{} telas", room.screens.len()) };
        let sw = p.text_size(p.fonts.bold, 12.0, &screens)[0] + 20.0;
        p.badge([pmax[0] - 10.0 - sw, pmin[1] + 10.0], &screens, ACCENT, WHITE, None, [10.0, 4.0], 12.0, None);
    }
    // Name, viewers and the button.
    // Name and button sit at the bottom of the card.
    let name = p.text_size(p.fonts.bold, 17.0, room.name);
    let meta = if empty { "Vazia".to_owned() } else { format!("{} assistindo", room.watching) };
    let ms = p.text_size(p.fonts.regular, 13.0, &meta);
    let block = name[1] + 4.0 + ms[1];
    let row_y = (max[1] - 14.0 - block).max(pmax[1] + 12.0);
    p.text(p.fonts.bold, 17.0, [pmin[0], row_y], TEXT, room.name);
    p.text(p.fonts.regular, 13.0, [pmin[0], row_y + name[1] + 4.0], MUTED, &meta);
    let bw = p.text_size(p.fonts.bold, 14.0, "Entrar")[0] + 36.0;
    let by = row_y + (block - 36.0) / 2.0;
    let (fill, color) = if empty { (CARD, TEXT) } else { (ACCENT, WHITE) };
    p.pill(&format!("enter{index}"), [pmax[0] - bw, by], [bw, 36.0], fill, Some(BORDER), "Entrar", p.fonts.bold, 14.0, color)
}

fn room_screen(p: &Painter<'_>, state: &mut State, index: usize, [w, h]: [f32; 2], source: VideoSource<'_>) {
    let room = &ROOMS[index];
    top_bar(p, w);
    let mid = 44.0;
    let mut x = 32.0;
    x += p.logo(x, mid) + 20.0 + 8.0;
    // Back.
    let back_w = 16.0 + 16.0 + 6.0 + p.text_size(p.fonts.semibold, 14.0, "Salas")[0] + 16.0;
    let bmin = [x, mid - 20.0];
    let (clicked, hovered) = p.hit("back", bmin, [x + back_w, mid + 20.0]);
    p.rect(bmin, [x + back_w, mid + 20.0], Some(if hovered { hover(BAR) } else { BAR }), Some((BORDER, 1.0)), 20.0);
    p.icon_back([x + 16.0, mid - 8.0], 16.0, TEXT);
    let ts = p.text_size(p.fonts.semibold, 14.0, "Salas");
    p.text(p.fonts.semibold, 14.0, [x + 16.0 + 16.0 + 6.0, mid - ts[1] / 2.0], TEXT, "Salas");
    if clicked {
        state.room = None;
        state.maximized = false;
        state.featured = 0;
    }
    x += back_w + 20.0 + 8.0;
    let title = p.text_size(p.fonts.bold, 28.0, room.name);
    p.text(p.fonts.bold, 28.0, [x, mid - title[1] / 2.0], TEXT, room.name);
    x += title[0] + 20.0;
    if !room.screens.is_empty() {
        let bh = p.badge_height(4.0, 12.0);
        p.badge([x, mid - bh / 2.0], "AO VIVO", BAR, TEXT, Some(LIVE_DOT), [10.0, 4.0], 12.0, Some(BORDER));
    }
    // Right side.
    let leave = "Sair da sala";
    let lw = p.text_size(p.fonts.semibold, 14.0, leave)[0] + 36.0;
    let lx = w - 32.0 - lw;
    if p.pill("leave", [lx, mid - 20.0], [lw, 40.0], BAR, Some(BORDER), leave, p.fonts.semibold, 14.0, TEXT) {
        state.room = None;
        state.maximized = false;
        state.featured = 0;
    }
    let n = room.screens.len();
    let info = format!("{} {} · {} assistindo", n, if n == 1 { "tela" } else { "telas" }, room.watching);
    let is = p.text_size(p.fonts.regular, 14.0, &info);
    p.text(p.fonts.regular, 14.0, [lx - 20.0 - is[0], mid - is[1] / 2.0], MUTED, &info);

    // Content: screens and the side panel.
    let top = 88.0 + 24.0;
    let (left, right, bottom, gap) = (32.0, 38.0, 34.0, 28.0);
    let panel_w = if state.maximized { 0.0 } else { 320.0 };
    let main_max = [w - right - panel_w - if state.maximized { 0.0 } else { gap }, h - bottom];
    screens(p, state, room, [left, top], main_max, source);
    if !state.maximized {
        side_panel(p, state, [w - right - 320.0, top], [w - right, h - bottom]);
    }
}

fn screens(p: &Painter<'_>, state: &mut State, room: &Room, min: [f32; 2], max: [f32; 2], source: VideoSource<'_>) {
    let n = room.screens.len();
    if n == 0 {
        let label = "Ninguém transmitindo nesta sala";
        let ls = p.text_size(p.fonts.semibold, 15.0, label);
        p.rect(min, max, Some(PREVIEW), Some((BORDER, 1.0)), 14.0);
        p.text(p.fonts.semibold, 15.0, [(min[0] + max[0] - ls[0]) / 2.0, (min[1] + max[1] - ls[1]) / 2.0], MUTED, label);
        return;
    }
    let featured = state.featured.min(n - 1);
    let gap = 22.0;
    let (w, h) = (max[0] - min[0], max[1] - min[1]);
    let others: Vec<usize> = (0..n).filter(|&i| i != featured).collect();
    let (top_h, row_h) = if others.is_empty() || state.maximized {
        (h, 0.0)
    } else {
        let unit = (h - gap) / 2.55;
        (unit * 1.55, unit)
    };
    screen_tile(p, state, room, featured, true, min, [max[0], min[1] + top_h], source);
    if top_h < h {
        let y = min[1] + top_h + gap;
        let tw = (w - gap * (others.len() - 1) as f32) / others.len() as f32;
        for (k, &i) in others.iter().enumerate() {
            let x = min[0] + k as f32 * (tw + gap);
            screen_tile(p, state, room, i, false, [x, y], [x + tw, y + row_h], source);
        }
    }
}

fn screen_tile(p: &Painter<'_>, state: &mut State, room: &Room, i: usize, featured: bool, min: [f32; 2], max: [f32; 2], source: VideoSource<'_>) {
    let stream = room.screens[i];
    p.rect(min, max, Some(PREVIEW), None, 14.0);
    p.video(source, stream, [min[0] + 1.0, min[1] + 1.0], [max[0] - 1.0, max[1] - 1.0], 13.0, false);
    p.rect(min, max, None, Some((BORDER, 1.0)), 14.0);
    if featured {
        // The 2 px blue ring outside the border (box-shadow 0 0 0 2px).
        p.draw.add_rect([min[0] - 1.0, min[1] - 1.0], [max[0] + 1.0, max[1] + 1.0], c(ACCENT)).rounding(15.0).thickness(2.0).build();
    }
    let name = format!("Tela d{} {}", if STREAMERS[stream] == "Bruno" { "o" } else { "a" }, STREAMERS[stream]);
    if featured {
        let size = 13.0;
        let t = p.text_size(p.fonts.bold, size, &name);
        let bh = 6.0 * 2.0 + t[1];
        let bw = 14.0 * 2.0 + 15.0 + 8.0 + t[0];
        let bmin = [min[0] + 14.0, max[1] - 14.0 - bh];
        p.rect(bmin, [bmin[0] + bw, bmin[1] + bh], Some(BAR), None, bh / 2.0);
        p.icon_monitor([bmin[0] + 14.0, bmin[1] + (bh - 15.0) / 2.0], 15.0, TEXT, 2.2);
        p.text(p.fonts.bold, size, [bmin[0] + 14.0 + 15.0 + 8.0, bmin[1] + 6.0], TEXT, &name);
    } else {
        let bh = p.badge_height(5.0, 12.0);
        p.badge([min[0] + 12.0, max[1] - 12.0 - bh], &name, BAR, TEXT, None, [12.0, 5.0], 12.0, None);
    }
    // Round button: fullscreen (featured) or "make featured".
    let (d, inset, icon) = if featured { (40.0, 12.0, 18.0) } else { (34.0, 10.0, 15.0) };
    let center = [max[0] - inset - d / 2.0, min[1] + inset + d / 2.0];
    let (clicked, hovered) = p.hit(&format!("expand{i}"), [center[0] - d / 2.0, center[1] - d / 2.0], [center[0] + d / 2.0, center[1] + d / 2.0]);
    p.draw.add_circle(center, d / 2.0, c(if hovered { hover(BAR) } else { BAR })).filled(true).build();
    p.icon_expand([center[0] - icon / 2.0, center[1] - icon / 2.0], icon, TEXT);
    if clicked {
        if featured {
            state.maximized = !state.maximized;
        } else {
            state.featured = i;
        }
    }
}

const PRESETS: [(Preset, usize, usize, f32); 3] = [(Preset::Light, 2, 1, 2500.0), (Preset::Normal, 1, 2, 6000.0), (Preset::High, 0, 2, 12000.0)];

fn side_panel(p: &Painter<'_>, state: &mut State, min: [f32; 2], max: [f32; 2]) {
    p.rect(min, max, Some(CARD), Some((BORDER, 1.0)), 16.0);
    let x = min[0] + 20.0;
    let width = max[0] - min[0] - 40.0;
    let mut y = min[1] + 18.0;
    let people = format!("Pessoas ({})", PEOPLE.len());
    if let Some(tab) = p.segmented("panel", [x, y], width, &["Transmissão", &people], &[1.0, 1.0], Some(state.panel), false, 34.0, 13.0) {
        state.panel = tab;
    }
    y += 34.0 + 8.0 + 14.0;
    if state.panel == 1 {
        for (name, streaming) in PEOPLE {
            let r = 18.0;
            p.draw.add_circle([x + r, y + r], r, c(if streaming { ACCENT } else { AVATAR })).filled(true).build();
            let initial = &name[..1];
            let is = p.text_size(p.fonts.bold, 14.0, initial);
            p.text(p.fonts.bold, 14.0, [x + r - is[0] / 2.0, y + r - is[1] / 2.0], if streaming { WHITE } else { TEXT }, initial);
            let ns = p.text_size(p.fonts.semibold, 14.0, name);
            p.text(p.fonts.semibold, 14.0, [x + 2.0 * r + 12.0, y + r - ns[1] / 2.0], TEXT, name);
            if streaming {
                let bh = p.badge_height(3.0, 11.0);
                let bw = p.text_size(p.fonts.bold, 11.0, "transmitindo")[0] + 20.0;
                p.badge([x + width - bw, y + r - bh / 2.0], "transmitindo", STREAMING_BG, STREAMING_TEXT, None, [10.0, 3.0], 11.0, None);
            }
            y += 2.0 * r + 12.0;
        }
        return;
    }
    let label = |y: f32, text: &str| p.text(p.fonts.semibold, 12.0, [x, y], MUTED, text);
    let label_h = p.text_size(p.fonts.semibold, 12.0, "Ag")[1];
    // Preset.
    label(y, "Predefinição");
    y += label_h + 8.0;
    let selected = match state.preset {
        Preset::Light => 0,
        Preset::Normal => 1,
        Preset::High => 2,
        Preset::Manual => 3,
    };
    if let Some(i) = p.segmented("preset", [x, y], width, &["Leve", "Normal", "Alta", "Manual"], &[1.0, 1.0, 1.0, 1.2], Some(selected), false, 30.0, 12.0) {
        if i < 3 {
            let (preset, resolution, fps, bitrate) = PRESETS[i];
            state.preset = preset;
            state.resolution = resolution;
            state.fps = fps;
            state.bitrate = bitrate;
        } else {
            state.preset = Preset::Manual;
        }
    }
    y += 38.0 + 14.0;
    // Resolution.
    label(y, "Resolução");
    y += label_h + 8.0;
    if let Some(i) = p.segmented("res", [x, y], width, &["Fonte", "1080p", "720p"], &[1.0, 1.0, 1.0], Some(state.resolution), true, 30.0, 12.0) {
        state.resolution = i;
        state.preset = Preset::Manual;
    }
    y += 38.0 + 8.0;
    p.text(p.fonts.regular, 11.0, [x, y], MUTED, "Fonte = resolução nativa da tela (2560×1440)");
    y += p.text_size(p.fonts.regular, 11.0, "Ag")[1] + 14.0;
    // FPS.
    label(y, "Quadros por segundo");
    y += label_h + 8.0;
    if let Some(i) = p.segmented("fps", [x, y], width, &["15", "30", "60"], &[1.0, 1.0, 1.0], Some(state.fps), true, 30.0, 12.0) {
        state.fps = i;
        state.preset = Preset::Manual;
    }
    y += 38.0 + 14.0;
    // Bitrate.
    label(y, "Bitrate");
    let value = format!("{} kbps", thousands(state.bitrate.round() as u32));
    let vw = p.text_size(p.fonts.bold, 12.0, &value)[0] + 20.0;
    let vh = p.badge_height(3.0, 12.0);
    p.badge([x + width - vw, y + label_h / 2.0 - vh / 2.0], &value, BG, TEXT, None, [10.0, 3.0], 12.0, Some(BORDER));
    y += label_h.max(vh) + 10.0;
    if slider(p, "bitrate", [x, y], width, &mut state.bitrate, 500.0, 20000.0) {
        state.preset = Preset::Manual;
    }
    y += 18.0 + 10.0;
    p.text(p.fonts.regular, 11.0, [x, y], MUTED, "500 kbps");
    let hi = "20.000 kbps";
    p.text(p.fonts.regular, 11.0, [x + width - p.text_size(p.fonts.regular, 11.0, hi)[0], y], MUTED, hi);

    // Bottom: estimated upload and the main button.
    let button_y = max[1] - 20.0 - 52.0;
    let upload_label = "Upload estimado:";
    let upload = format!("~{} MB/s", decimal(state.bitrate / 8000.0));
    let uy = button_y - 14.0 - p.text_size(p.fonts.regular, 12.0, "Ag")[1];
    p.text(p.fonts.regular, 12.0, [x, uy], MUTED, upload_label);
    let lw = p.text_size(p.fonts.regular, 12.0, upload_label)[0] + 4.0;
    p.text(p.fonts.semibold, 12.0, [x + lw, uy], TEXT, &upload);
    let text = if state.streaming { "Parar transmissão" } else { "Transmitir minha tela" };
    let (clicked, hovered) = p.hit("go", [x, button_y], [x + width, button_y + 52.0]);
    p.rect([x, button_y], [x + width, button_y + 52.0], Some(if hovered { hover(ACCENT) } else { ACCENT }), Some((BORDER, 1.0)), 26.0);
    let t = p.text_size(p.fonts.bold, 15.0, text);
    let content = 18.0 + 10.0 + t[0];
    let cx = x + (width - content) / 2.0;
    p.icon_monitor([cx, button_y + 17.0], 18.0, WHITE, 2.2);
    p.text(p.fonts.bold, 15.0, [cx + 28.0, button_y + (52.0 - t[1]) / 2.0], WHITE, text);
    if clicked {
        state.streaming = !state.streaming;
    }
}

/// The bitrate slider: thin track, blue fill, light knob with a blue ring.
fn slider(p: &Painter<'_>, id: &str, min: [f32; 2], width: f32, value: &mut f32, lo: f32, hi: f32) -> bool {
    p.ui.set_cursor_screen_pos(min);
    p.ui.invisible_button(id, [width, 18.0]);
    let mut changed = false;
    if p.ui.is_item_active() {
        let mx = p.ui.io().mouse_pos()[0];
        let t = ((mx - min[0]) / width).clamp(0.0, 1.0);
        let v = (lo + t * (hi - lo)) / 100.0;
        let new = v.round() * 100.0;
        if (new - *value).abs() > f32::EPSILON {
            *value = new;
            changed = true;
        }
    }
    if p.ui.is_item_hovered() || p.ui.is_item_active() {
        p.ui.set_mouse_cursor(Some(MouseCursor::Hand));
    }
    let t = (*value - lo) / (hi - lo);
    p.rect([min[0], min[1] + 6.0], [min[0] + width, min[1] + 12.0], Some(BG), Some((BORDER, 1.0)), 3.0);
    p.rect([min[0], min[1] + 6.0], [min[0] + width * t, min[1] + 14.0], Some(ACCENT), None, 4.0);
    let kx = min[0] + width * t;
    p.draw.add_circle([kx, min[1] + 9.0], 9.0, c(ACCENT)).filled(true).build();
    p.draw.add_circle([kx, min[1] + 9.0], 6.0, c(TEXT)).filled(true).build();
    changed
}

fn thousands(value: u32) -> String {
    let s = value.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

fn decimal(value: f32) -> String {
    format!("{value:.2}").replace('.', ",")
}
