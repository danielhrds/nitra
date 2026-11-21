//! The rooms in a sidebar (live and empty sections, search) and the selected
//! room on the right with its live screens (`preview-juncao.html`). The
//! sidebar can be resized by its edge or collapsed to a rail; a screen can
//! be focused (shown large) or maximized (alone, with its statistics).
//! Selecting a room only shows it; "Entrar na sala" joins. Laid out by hand
//! from the window size, drawn with the draw list; sample data.

use dear_imgui_rs::{DrawFlags, DrawListMut, FontId, MouseCursor, Ui, WindowFlags};

// ---------------------------------------------------------------- palette

const BG: u32 = 0x0E1220;
const SIDEBAR: u32 = 0x121729;
const CARD: u32 = 0x161B2E;
const PREVIEW: u32 = 0x0A0D18;
const ACTIVE: u32 = 0x2A3250;
const ACTIVE_BORDER: u32 = 0x3A4466;
const BORDER: u32 = 0x262E48;
const DIVIDER: u32 = 0x1F2640;
const ACCENT: u32 = 0x0047FF;
const TEXT: u32 = 0xE8ECF7;
const MUTED: u32 = 0x8A93AD;
const LIVE_DOT: u32 = 0xFF3B30;
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

const ROOMS: [Room; 8] = [
    Room { name: "Equipe design", watching: 5, screens: &[0, 1] },
    Room { name: "Revisão de código", watching: 3, screens: &[2] },
    Room { name: "Jogatina de sexta", watching: 8, screens: &[0, 1, 2] },
    Room { name: "Aula de matemática", watching: 12, screens: &[1] },
    Room { name: "Lo-fi e estudo", watching: 6, screens: &[0] },
    Room { name: "Suporte da Bia", watching: 1, screens: &[2] },
    Room { name: "Sala do Leo", watching: 0, screens: &[] },
    Room { name: "Reunião semanal", watching: 0, screens: &[] },
];
const STREAMERS: [&str; 3] = ["Ana", "Bruno", "Carla"];

/// Sidebar width limits when dragging its edge, and the collapsed rail.
const SIDEBAR_MIN: f32 = 200.0;
const SIDEBAR_MAX: f32 = 420.0;
const RAIL: f32 = 64.0;

/// What the stream statistics overlay shows (measured by the caller).
#[derive(Clone, Copy, Default)]
pub struct StreamStats {
    pub fps: f32,
    pub kbps: f32,
}

pub struct State {
    /// Room shown on the right (a click in the sidebar only selects it).
    selected: usize,
    /// Room we are in ("Entrar na sala").
    joined: Option<usize>,
    search: String,
    sidebar_width: f32,
    sidebar_collapsed: bool,
    /// Screen (index within the room) shown large.
    focused: usize,
    /// Screen (index within the room) shown alone with its statistics.
    maximized: Option<usize>,
    streaming: bool,
    pub stats: [StreamStats; 3],
}

impl Default for State {
    fn default() -> Self {
        Self {
            selected: 2,
            joined: None,
            search: String::new(),
            sidebar_width: 280.0,
            sidebar_collapsed: false,
            focused: 0,
            maximized: None,
            streaming: false,
            stats: [StreamStats::default(); 3],
        }
    }
}

impl State {
    /// Starting state for screenshots and checks (command-line flags).
    pub fn configure(&mut self, room: Option<usize>, joined: bool, focus: Option<usize>, maximize: Option<usize>, sidebar: Option<f32>, collapsed: bool) {
        if let Some(room) = room {
            self.selected = room.min(ROOMS.len() - 1);
        }
        if joined {
            self.joined = Some(self.selected);
        }
        if let Some(focus) = focus {
            self.focused = focus;
        }
        // Maximizing needs the live screens of the room we are in.
        self.maximized = maximize.filter(|_| self.is_live());
        if let Some(width) = sidebar {
            self.sidebar_width = width.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        }
        self.sidebar_collapsed = collapsed;
    }

    /// True in the room we joined: its screens are live. Elsewhere the
    /// selected room is a preview made of stills.
    pub fn is_live(&self) -> bool {
        self.joined == Some(self.selected)
    }

    /// Streams on screen now.
    pub fn visible_streams(&self) -> Vec<usize> {
        let screens = ROOMS[self.selected].screens;
        match self.maximized {
            Some(i) if i < screens.len() => vec![screens[i]],
            _ => screens.to_vec(),
        }
    }

    fn select(&mut self, room: usize) {
        if room != self.selected {
            self.selected = room;
            self.focused = 0;
            self.maximized = None;
        }
    }
}

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

}

// ---------------------------------------------------------------- icons used by the sidebar layout

impl Painter<'_> {
    /// A panel with its left column: shows/hides the sidebar.
    fn icon_sidebar(&self, origin: [f32; 2], size: f32, color: u32) {
        let s = size / 24.0;
        self.draw
            .add_rect([origin[0] + 3.0 * s, origin[1] + 4.0 * s], [origin[0] + 21.0 * s, origin[1] + 20.0 * s], c(color))
            .rounding(2.5 * s)
            .thickness(2.0)
            .build();
        self.icon_lines(origin, s, color, 2.0, &[[9.5, 4.0, 9.5, 20.0]]);
    }

    /// A small tile growing into the big one: "focus this screen".
    fn icon_focus(&self, origin: [f32; 2], size: f32, color: u32) {
        let s = size / 24.0;
        self.draw
            .add_rect([origin[0] + 3.0 * s, origin[1] + 3.0 * s], [origin[0] + 21.0 * s, origin[1] + 21.0 * s], c(color))
            .rounding(2.5 * s)
            .thickness(2.0)
            .build();
        self.draw
            .add_rect([origin[0] + 6.5 * s, origin[1] + 6.5 * s], [origin[0] + 17.5 * s, origin[1] + 13.5 * s], c(color))
            .rounding(1.5 * s)
            .filled(true)
            .build();
    }

    fn icon_gear(&self, origin: [f32; 2], size: f32, color: u32) {
        let s = size / 24.0;
        let center = [origin[0] + 12.0 * s, origin[1] + 12.0 * s];
        self.draw.add_circle(center, 3.0 * s, c(color)).thickness(2.0).build();
        self.draw.add_circle(center, 7.0 * s, c(color)).thickness(2.0).build();
        for k in 0..8 {
            let a = std::f32::consts::FRAC_PI_4 * k as f32;
            let (dx, dy) = (a.cos(), a.sin());
            self.draw
                .add_line([center[0] + dx * 7.0 * s, center[1] + dy * 7.0 * s], [center[0] + dx * 10.0 * s, center[1] + dy * 10.0 * s], c(color))
                .thickness(2.4)
                .build();
        }
    }

    /// A round icon button; returns clicked.
    fn round_button(&self, id: &str, center: [f32; 2], d: f32, fill: u32, border: Option<u32>, icon: impl FnOnce([f32; 2])) -> bool {
        let (clicked, hovered) = self.hit(id, [center[0] - d / 2.0, center[1] - d / 2.0], [center[0] + d / 2.0, center[1] + d / 2.0]);
        self.draw.add_circle(center, d / 2.0, c(if hovered { hover(fill) } else { fill })).filled(true).build();
        if let Some(border) = border {
            self.draw.add_circle(center, d / 2.0 - 0.5, c(border)).thickness(1.0).build();
        }
        icon(center);
        clicked
    }

    fn initial_avatar(&self, center: [f32; 2], d: f32, fill: u32, color: u32, name: &str, size: f32) {
        self.draw.add_circle(center, d / 2.0, c(fill)).filled(true).build();
        let initial: String = name.chars().take(1).collect();
        let t = self.text_size(self.fonts.bold, size, &initial);
        self.text(self.fonts.bold, size, [center[0] - t[0] / 2.0, center[1] - t[1] / 2.0], color, &initial);
    }

    /// The sidebar logo: the blue pill at 26 px (padding 2/16/6 as in the design).
    fn small_logo(&self, x: f32, center_y: f32) -> f32 {
        let size = 26.0;
        let spacing = -0.09 * size;
        let widths: Vec<f32> = "ziz".chars().map(|ch| self.text_size(self.fonts.logo, size, &ch.to_string())[0]).collect();
        let text_w: f32 = widths.iter().sum::<f32>() + spacing * 2.0;
        let line = self.text_size(self.fonts.logo, size, "z")[1];
        let (pad_t, pad_x, pad_b) = (2.0, 16.0, 6.0);
        let h = pad_t + size + pad_b;
        let y = center_y - h / 2.0;
        let w = pad_x * 2.0 + text_w;
        self.rect([x, y], [x + w, y + h], Some(ACCENT), None, h / 2.0);
        let mut tx = x + pad_x;
        let ty = y + pad_t - (line - size) / 2.0;
        for (ch, width) in "ziz".chars().zip(&widths) {
            self.text(self.fonts.logo, size, [tx, ty], WHITE, &ch.to_string());
            tx += width + spacing;
        }
        w
    }

    /// Text with letter spacing in em (Dear ImGui has none): one glyph at a time.
    fn spaced_text(&self, font: FontId, size: f32, pos: [f32; 2], color: u32, text: &str, em: f32) {
        let mut x = pos[0];
        for ch in text.chars() {
            let s = ch.to_string();
            self.text(font, size, [x, pos[1]], color, &s);
            x += self.text_size(font, size, &s)[0] + em * size;
        }
    }

    /// `text` shortened with "…" to fit `width`.
    fn fit(&self, font: FontId, size: f32, text: &str, width: f32) -> String {
        if self.text_size(font, size, text)[0] <= width {
            return text.to_owned();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let candidate: String = chars.iter().collect::<String>().trim_end().to_owned() + "…";
            if self.text_size(font, size, &candidate)[0] <= width {
                return candidate;
            }
        }
        "…".to_owned()
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
            let screens = ROOMS[state.selected].screens;
            if let Some(i) = state.maximized.filter(|&i| i < screens.len() && state.is_live()) {
                maximized(&p, state, screens[i], size, source);
                return;
            }
            let sidebar_w = if state.sidebar_collapsed { RAIL } else { state.sidebar_width };
            if state.sidebar_collapsed {
                rail(&p, state, size[1]);
            } else {
                sidebar(&p, state, size[1]);
                resize_handle(&p, state, size[1]);
            }
            room_view(&p, state, [sidebar_w, 0.0], size, source);
        });
}

/// The rooms that pass the search: (live, empty), in sidebar order.
fn filtered(search: &str) -> (Vec<usize>, Vec<usize>) {
    let filter = search.to_lowercase();
    let matches = |i: &usize| filter.is_empty() || ROOMS[*i].name.to_lowercase().contains(&filter);
    let live = (0..ROOMS.len()).filter(|&i| !ROOMS[i].screens.is_empty()).filter(matches).collect();
    let empty = (0..ROOMS.len()).filter(|&i| ROOMS[i].screens.is_empty()).filter(matches).collect();
    (live, empty)
}

fn sidebar(p: &Painter<'_>, state: &mut State, h: f32) {
    let w = state.sidebar_width;
    p.rect([0.0, 0.0], [w, h], Some(SIDEBAR), None, 0.0);
    p.draw.add_line([w - 0.5, 0.0], [w - 0.5, h], c(DIVIDER)).thickness(1.0).build();
    let (left, right) = (14.0, w - 14.0);
    let mut y = 20.0;

    // Logo and "create room".
    let header_h = 34.0;
    p.small_logo(left + 4.0, y + header_h / 2.0);
    p.round_button("create", [right - 4.0 - 17.0, y + header_h / 2.0], 34.0, ACCENT, Some(BORDER), |center| {
        p.icon_plus([center[0] - 8.0, center[1] - 8.0], 16.0, WHITE);
    });
    y += header_h + 14.0;

    // Search (filters the list).
    let sh = 38.0;
    p.rect([left, y], [right, y + sh], Some(BG), Some((BORDER, 1.0)), sh / 2.0);
    p.icon_search([left + 16.0, y + (sh - 16.0) / 2.0], 16.0, MUTED);
    {
        let _font = p.ui.push_font_with_size(Some(p.fonts.medium), 14.0 * p.fonts.scale(p.fonts.medium));
        let _frame = p.ui.push_style_color(dear_imgui_rs::StyleColor::FrameBg, [0.0, 0.0, 0.0, 0.0]);
        let _text = p.ui.push_style_color(dear_imgui_rs::StyleColor::Text, rgba(TEXT));
        let _hint = p.ui.push_style_color(dear_imgui_rs::StyleColor::TextDisabled, rgba(MUTED));
        let fh = p.ui.frame_height();
        p.ui.set_cursor_screen_pos([left + 16.0 + 16.0 + 6.0, y + (sh - fh) / 2.0]);
        p.ui.set_next_item_width((right - left - 16.0 - 16.0 - 6.0 - 12.0).max(10.0));
        p.ui.input_text("##search", &mut state.search).hint("Buscar sala").build();
    }
    y += sh + 14.0;

    // Sections.
    let (live, empty) = filtered(&state.search);
    let user_card_h = 54.0;
    let list_bottom = h - 14.0 - user_card_h - 10.0;
    for (title, rooms) in [("AO VIVO", &live), ("VAZIAS", &empty)] {
        if rooms.is_empty() {
            continue;
        }
        let label = format!("{title} · {}", rooms.len());
        p.spaced_text(p.fonts.bold, 11.0, [left + 6.0, y + 6.0], MUTED, &label, 0.06);
        y += 6.0 + p.text_size(p.fonts.bold, 11.0, "Ag")[1] + 14.0;
        for &room in rooms.iter() {
            let item_h = 36.0;
            if y + item_h > list_bottom {
                break;
            }
            if room_item(p, state, room, [left, y], [right, y + item_h]) {
                state.select(room);
            }
            y += item_h + 2.0;
        }
        y += 8.0;
    }

    // You.
    let cy = h - 14.0 - user_card_h;
    p.rect([left, cy], [right, cy + user_card_h], Some(CARD), Some((BORDER, 1.0)), 12.0);
    p.initial_avatar([left + 10.0 + 16.0, cy + user_card_h / 2.0], 32.0, ACTIVE, TEXT, "Você", 13.0);
    let tx = left + 10.0 + 32.0 + 10.0;
    let name_h = p.text_size(p.fonts.semibold, 14.0, "Ag")[1];
    let status_h = p.text_size(p.fonts.regular, 12.0, "Ag")[1];
    let ty = cy + (user_card_h - name_h - 2.0 - status_h) / 2.0;
    p.text(p.fonts.semibold, 14.0, [tx, ty], TEXT, "Você");
    let status = match state.joined {
        Some(room) if state.streaming => format!("Transmitindo em {}", ROOMS[room].name),
        Some(room) => format!("Na sala {}", ROOMS[room].name),
        None => "Fora de sala".to_owned(),
    };
    let status = p.fit(p.fonts.regular, 12.0, &status, right - 10.0 - 34.0 - 6.0 - tx);
    p.text(p.fonts.regular, 12.0, [tx, ty + name_h + 2.0], MUTED, &status);
    p.round_button("settings", [right - 10.0 - 17.0, cy + user_card_h / 2.0], 34.0, CARD, None, |center| {
        p.icon_gear([center[0] - 9.0, center[1] - 9.0], 18.0, MUTED);
    });
}

/// One sidebar row; returns clicked (selects the room, never joins it).
fn room_item(p: &Painter<'_>, state: &State, room: usize, min: [f32; 2], max: [f32; 2]) -> bool {
    let (clicked, hovered) = p.hit(&format!("room{room}"), min, max);
    let selected = state.selected == room;
    if selected {
        p.rect(min, max, Some(ACTIVE), Some((ACTIVE_BORDER, 1.0)), 10.0);
    } else if hovered {
        p.rect(min, max, Some(hover(SIDEBAR)), None, 10.0);
    }
    let info = &ROOMS[room];
    let live = !info.screens.is_empty();
    let mid = (min[1] + max[1]) / 2.0;
    p.draw.add_circle([min[0] + 10.0 + 4.0, mid], 4.0, c(if live { LIVE_DOT } else { ACTIVE_BORDER })).filled(true).build();
    let font = if selected { p.fonts.semibold } else { p.fonts.medium };
    let count = if live { info.watching.to_string() } else { String::new() };
    let cw = if live { p.text_size(p.fonts.regular, 12.0, &count)[0] } else { 0.0 };
    let name_x = min[0] + 10.0 + 8.0 + 10.0;
    let name = p.fit(font, 14.0, info.name, max[0] - 10.0 - cw - 10.0 - name_x);
    let nh = p.text_size(font, 14.0, &name)[1];
    p.text(font, 14.0, [name_x, mid - nh / 2.0], if live { TEXT } else { MUTED }, &name);
    if live {
        let ch = p.text_size(p.fonts.regular, 12.0, &count)[1];
        p.text(p.fonts.regular, 12.0, [max[0] - 10.0 - cw, mid - ch / 2.0], MUTED, &count);
    }
    clicked
}

/// The collapsed sidebar: create button, one round initial per room (a red
/// dot when live; name on hover), and you at the bottom.
fn rail(p: &Painter<'_>, state: &mut State, h: f32) {
    p.rect([0.0, 0.0], [RAIL, h], Some(SIDEBAR), None, 0.0);
    p.draw.add_line([RAIL - 0.5, 0.0], [RAIL - 0.5, h], c(DIVIDER)).thickness(1.0).build();
    let x = RAIL / 2.0;
    let mut y = 20.0 + 17.0;
    p.round_button("create", [x, y], 34.0, ACCENT, Some(BORDER), |center| {
        p.icon_plus([center[0] - 8.0, center[1] - 8.0], 16.0, WHITE);
    });
    y += 17.0 + 18.0;
    let (live, empty) = filtered("");
    let bottom = h - 14.0 - 40.0 - 10.0;
    for room in live.into_iter().chain(empty) {
        let d = 40.0;
        if y + d > bottom {
            break;
        }
        let center = [x, y + d / 2.0];
        let (clicked, hovered) = p.hit(&format!("rail{room}"), [x - d / 2.0, y], [x + d / 2.0, y + d]);
        let selected = state.selected == room;
        let fill = if selected {
            ACTIVE
        } else if hovered {
            hover(CARD)
        } else {
            CARD
        };
        let live_room = !ROOMS[room].screens.is_empty();
        p.initial_avatar(center, d, fill, if live_room { TEXT } else { MUTED }, ROOMS[room].name, 15.0);
        p.draw.add_circle(center, d / 2.0 - 0.5, c(if selected { ACTIVE_BORDER } else { BORDER })).thickness(1.0).build();
        if live_room {
            let dot = [center[0] + d / 2.0 - 5.0, center[1] - d / 2.0 + 5.0];
            p.draw.add_circle(dot, 5.5, c(SIDEBAR)).filled(true).build();
            p.draw.add_circle(dot, 4.0, c(LIVE_DOT)).filled(true).build();
        }
        if hovered {
            p.ui.tooltip_text(ROOMS[room].name);
        }
        if clicked {
            state.select(room);
        }
        y += d + 8.0;
    }
    p.initial_avatar([x, h - 14.0 - 20.0], 40.0, ACTIVE, TEXT, "Você", 14.0);
}

/// The sidebar's right edge: drag to resize.
fn resize_handle(p: &Painter<'_>, state: &mut State, h: f32) {
    let x = state.sidebar_width;
    p.ui.set_cursor_screen_pos([x - 3.0, 0.0]);
    p.ui.invisible_button("sidebar-edge", [6.0, h]);
    let (hovered, active) = (p.ui.is_item_hovered(), p.ui.is_item_active());
    if hovered || active {
        p.ui.set_mouse_cursor(Some(MouseCursor::ResizeEW));
        p.draw.add_line([x - 1.0, 0.0], [x - 1.0, h], c(ACCENT)).thickness(2.0).build();
    }
    if active {
        state.sidebar_width = p.ui.io().mouse_pos()[0].clamp(SIDEBAR_MIN, SIDEBAR_MAX);
    }
}

fn room_view(p: &Painter<'_>, state: &mut State, origin: [f32; 2], [w, h]: [f32; 2], source: VideoSource<'_>) {
    let room = &ROOMS[state.selected];
    let (left, right, top, bottom) = (origin[0] + 36.0, w - 36.0, 30.0, h - 32.0);
    let mut y = top;

    // Title row: sidebar toggle, name, live badge.
    let title_h = p.text_size(p.fonts.bold, 30.0, "Ag")[1];
    let mid = y + title_h / 2.0;
    let toggle = p.round_button("toggle-sidebar", [left + 17.0, mid], 34.0, BG, Some(BORDER), |center| {
        p.icon_sidebar([center[0] - 9.0, center[1] - 9.0], 18.0, TEXT);
    });
    if toggle {
        state.sidebar_collapsed = !state.sidebar_collapsed;
    }
    let mut x = left + 34.0 + 14.0;
    let title = p.fit(p.fonts.bold, 30.0, room.name, right - x - 120.0);
    let ts = p.text_size(p.fonts.bold, 30.0, &title);
    p.text(p.fonts.bold, 30.0, [x, y], TEXT, &title);
    x += ts[0] + 14.0;
    if !room.screens.is_empty() {
        let bh = p.badge_height(3.0, 12.0);
        p.badge([x, mid - bh / 2.0], "Ao vivo", CARD, TEXT, Some(LIVE_DOT), [9.0, 3.0], 12.0, Some(BORDER));
    }
    y += title_h + 18.0;

    // Meta line.
    let n = room.screens.len();
    let meta = match n {
        0 => "Vazia · ninguém transmitindo".to_owned(),
        1 => format!("1 tela · {} assistindo", room.watching),
        _ => format!("{n} telas · {} assistindo", room.watching),
    };
    p.text(p.fonts.regular, 14.0, [left, y], MUTED, &meta);
    y += p.text_size(p.fonts.regular, 14.0, "Ag")[1] + 18.0;

    // Who is streaming.
    if n > 0 {
        let chip_h = 32.0;
        let label = "Transmitindo:";
        let ls = p.text_size(p.fonts.regular, 13.0, label);
        p.text(p.fonts.regular, 13.0, [left, y + (chip_h - ls[1]) / 2.0], MUTED, label);
        let mut cx = left + ls[0] + 4.0 + 8.0;
        for &stream in room.screens {
            let name = STREAMERS[stream];
            let nw = p.text_size(p.fonts.semibold, 13.0, name)[0];
            let cw = 4.0 + 24.0 + 8.0 + nw + 12.0;
            p.rect([cx, y], [cx + cw, y + chip_h], Some(CARD), Some((BORDER, 1.0)), chip_h / 2.0);
            p.initial_avatar([cx + 4.0 + 12.0, y + chip_h / 2.0], 24.0, ACCENT, WHITE, name, 11.0);
            let nh = p.text_size(p.fonts.semibold, 13.0, name)[1];
            p.text(p.fonts.semibold, 13.0, [cx + 4.0 + 24.0 + 8.0, y + (chip_h - nh) / 2.0], TEXT, name);
            cx += cw + 8.0;
        }
        y += chip_h + 18.0;
    }

    // Buttons at the bottom; the screens take the space in between.
    let button_h = 48.0;
    let by = bottom - button_h;
    screens(p, state, room, [left, y], [right, by - 18.0], source);
    room_buttons(p, state, [left, by], button_h);
}

fn room_buttons(p: &Painter<'_>, state: &mut State, [x, y]: [f32; 2], h: f32) {
    let here = state.joined == Some(state.selected);
    let mut x = x;
    let mut button = |id: &str, label: &str, primary: bool, font: FontId, size: f32, pad: f32| -> bool {
        let w = p.text_size(font, size, label)[0] + pad * 2.0;
        let (fill, color) = if primary { (ACCENT, WHITE) } else { (CARD, TEXT) };
        let clicked = p.pill(id, [x, y], [w, h], fill, Some(BORDER), label, font, size, color);
        x += w + 12.0;
        clicked
    };
    if here {
        let label = if state.streaming { "Parar transmissão" } else { "Transmitir minha tela" };
        if button("stream", label, true, p.fonts.bold, 15.0, 28.0) {
            state.streaming = !state.streaming;
        }
        if button("leave", "Sair da sala", false, p.fonts.semibold, 14.0, 18.0) {
            state.joined = None;
            state.streaming = false;
            state.maximized = None;
        }
    } else if button("join", "Entrar na sala", true, p.fonts.bold, 15.0, 28.0) {
        state.joined = Some(state.selected);
        state.streaming = false;
    }
    let _ = button("invite", "Copiar convite", false, p.fonts.semibold, 14.0, 18.0);
}

fn screens(p: &Painter<'_>, state: &mut State, room: &Room, min: [f32; 2], max: [f32; 2], source: VideoSource<'_>) {
    let n = room.screens.len();
    if n == 0 {
        let label = "Ninguém transmitindo nesta sala";
        let ls = p.text_size(p.fonts.semibold, 15.0, label);
        p.rect(min, max, Some(PREVIEW), Some((BORDER, 1.0)), 14.0);
        let block = 30.0 + 10.0 + ls[1];
        let top = (min[1] + max[1] - block) / 2.0;
        p.icon_monitor([(min[0] + max[0]) / 2.0 - 15.0, top], 30.0, MUTED, 2.0);
        p.text(p.fonts.semibold, 15.0, [(min[0] + max[0] - ls[0]) / 2.0, top + 40.0], MUTED, label);
        return;
    }
    // The focused screen on top across the width (1.5 parts), the others
    // side by side below (1 part), like the CSS grid of the design.
    let focused = state.focused.min(n - 1);
    let gap = 16.0;
    let (w, h) = (max[0] - min[0], max[1] - min[1]);
    let others: Vec<usize> = (0..n).filter(|&i| i != focused).collect();
    let top_h = if others.is_empty() { h } else { (h - gap) * 1.5 / 2.5 };
    screen_tile(p, state, room, focused, true, n, min, [max[0], min[1] + top_h], source);
    if !others.is_empty() {
        let y = min[1] + top_h + gap;
        let tw = (w - gap * (others.len() - 1) as f32) / others.len() as f32;
        for (k, &i) in others.iter().enumerate() {
            let x = min[0] + k as f32 * (tw + gap);
            screen_tile(p, state, room, i, false, n, [x, y], [x + tw, max[1]], source);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn screen_tile(p: &Painter<'_>, state: &mut State, room: &Room, i: usize, focused: bool, count: usize, min: [f32; 2], max: [f32; 2], source: VideoSource<'_>) {
    let stream = room.screens[i];
    p.rect(min, max, Some(PREVIEW), None, 14.0);
    if !p.video(source, stream, [min[0] + 1.0, min[1] + 1.0], [max[0] - 1.0, max[1] - 1.0], 13.0, false) {
        // No picture yet: the design's placeholder, a window shape.
        let inset = ((max[0] - min[0]) * 0.05).max(20.0);
        let top = min[1] + ((max[1] - min[1]) * 0.08).max(18.0);
        p.rect([min[0] + inset, top], [max[0] - inset, max[1]], Some(0x1B2A55), None, 8.0);
        p.rect([min[0] + inset, top], [max[0] - inset, top + 14.0], Some(0x2A3B6E), None, 8.0);
    }
    p.rect(min, max, None, Some((BORDER, 1.0)), 14.0);
    if focused && count > 1 {
        // The focused screen: a 2 px blue ring outside the border.
        p.draw.add_rect([min[0] - 1.0, min[1] - 1.0], [max[0] + 1.0, max[1] + 1.0], c(ACCENT)).rounding(15.0).thickness(2.0).build();
    }
    let name = screen_name(stream);
    let size = 12.0;
    let t = p.text_size(p.fonts.bold, size, &name);
    let bh = 5.0 * 2.0 + t[1];
    let bw = 12.0 * 2.0 + 13.0 + 6.0 + t[0];
    let bmin = [min[0] + 12.0, max[1] - 12.0 - bh];
    p.rect(bmin, [bmin[0] + bw, bmin[1] + bh], Some(CARD), Some((BORDER, 1.0)), bh / 2.0);
    p.icon_monitor([bmin[0] + 12.0, bmin[1] + (bh - 13.0) / 2.0], 13.0, TEXT, 2.2);
    p.text(p.fonts.bold, size, [bmin[0] + 12.0 + 13.0 + 6.0, bmin[1] + 5.0], TEXT, &name);

    // Top right: maximize (live only: a preview is a still), and focus on
    // the screens not focused.
    let live = state.is_live();
    let d = 34.0;
    let mut cx = max[0] - 10.0 - d / 2.0;
    let cy = min[1] + 10.0 + d / 2.0;
    if live {
        if p.round_button(&format!("max{i}"), [cx, cy], d, CARD, Some(BORDER), |center| {
            p.icon_expand([center[0] - 7.5, center[1] - 7.5], 15.0, TEXT);
        }) {
            state.maximized = Some(i);
        }
        cx -= d + 8.0;
    }
    if !focused {
        if p.round_button(&format!("focus{i}"), [cx, cy], d, CARD, Some(BORDER), |center| {
            p.icon_focus([center[0] - 7.5, center[1] - 7.5], 15.0, TEXT);
        }) {
            state.focused = i;
        }
    }
    // Double click on the picture (not on a button): maximize.
    let over_tile = p.ui.is_mouse_hovering_rect(min, max) && !p.ui.is_any_item_hovered();
    if live && over_tile && p.ui.is_mouse_double_clicked(dear_imgui_rs::MouseButton::Left) {
        state.maximized = Some(i);
    }
}

fn screen_name(stream: usize) -> String {
    let name = STREAMERS[stream];
    format!("Tela d{} {name}", if name == "Bruno" { "o" } else { "a" })
}

/// One screen alone: the video and its statistics, nothing else of the
/// app; "Voltar" (or Esc) returns to the room.
fn maximized(p: &Painter<'_>, state: &mut State, stream: usize, [w, h]: [f32; 2], source: VideoSource<'_>) {
    p.rect([0.0, 0.0], [w, h], Some(0x000000), None, 0.0);
    let picture = source(stream, [w, h], false).map(|(_, size)| size);
    p.video(source, stream, [0.0, 0.0], [w, h], 0.0, false);
    if p.ui.is_key_pressed(dear_imgui_rs::Key::Escape) {
        state.maximized = None;
    }
    // Statistics, top left.
    let stats = state.stats[stream];
    let resolution = picture.map_or("—".to_owned(), |(pw, ph)| format!("{pw}×{ph}"));
    let lines = [
        screen_name(stream),
        format!("{resolution} · {:.0} fps", stats.fps),
        format!("{} kbps · H.264 · decodificação por hardware", thousands(stats.kbps.round() as u32)),
    ];
    let (pad, line_gap) = (14.0, 4.0);
    let style = |k: usize| if k == 0 { (p.fonts.bold, 14.0, TEXT) } else { (p.fonts.medium, 13.0, MUTED) };
    let sizes: Vec<[f32; 2]> = lines.iter().enumerate().map(|(k, l)| p.text_size(style(k).0, style(k).1, l)).collect();
    let box_w = sizes.iter().map(|s| s[0]).fold(0.0, f32::max) + pad * 2.0;
    let box_h = sizes.iter().map(|s| s[1]).sum::<f32>() + line_gap * 2.0 + pad * 2.0;
    let origin = [16.0, 16.0];
    p.draw
        .add_rect(origin, [origin[0] + box_w, origin[1] + box_h], [0.03, 0.04, 0.08, 0.78])
        .filled(true)
        .rounding(12.0)
        .build();
    p.rect(origin, [origin[0] + box_w, origin[1] + box_h], None, Some((BORDER, 1.0)), 12.0);
    let mut y = origin[1] + pad;
    for (k, line) in lines.iter().enumerate() {
        let (font, size, color) = style(k);
        p.text(font, size, [origin[0] + pad, y], color, line);
        y += sizes[k][1] + line_gap;
    }
    // Back, top right.
    let label = "Voltar";
    let lw = p.text_size(p.fonts.semibold, 14.0, label)[0];
    let bw = 14.0 + 16.0 + 6.0 + lw + 18.0;
    let bmin = [w - 16.0 - bw, 16.0];
    let bmax = [w - 16.0, 16.0 + 40.0];
    let (clicked, hovered) = p.hit("back", bmin, bmax);
    p.rect(bmin, bmax, Some(if hovered { hover(CARD) } else { CARD }), Some((BORDER, 1.0)), 20.0);
    p.icon_back([bmin[0] + 14.0, bmin[1] + 12.0], 16.0, TEXT);
    let ls = p.text_size(p.fonts.semibold, 14.0, label);
    p.text(p.fonts.semibold, 14.0, [bmin[0] + 14.0 + 16.0 + 6.0, bmin[1] + (40.0 - ls[1]) / 2.0], TEXT, label);
    if clicked {
        state.maximized = None;
    }
}

fn rgba(rgb: u32) -> [f32; 4] {
    [((rgb >> 16) & 0xFF) as f32 / 255.0, ((rgb >> 8) & 0xFF) as f32 / 255.0, (rgb & 0xFF) as f32 / 255.0, 1.0]
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
