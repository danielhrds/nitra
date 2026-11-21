//! The window's drawing (the sidebar design of the prototype, `db76675`):
//! the rooms on the left (live and empty sections, search; resizable and
//! collapsible), the selected room on the right with its participants and
//! live screens, and the dialogs. It reads the [`Model`] and returns what the
//! user asked for as [`Action`]s; the app does the rest. Selecting a room
//! only shows it; "Entrar na sala" enters. Laid out by hand from the window
//! size and drawn with the draw list.

use std::time::Instant;

use dear_imgui_rs::{FontId, MouseCursor, StyleColor, Ui, WindowFlags};
use rooms::Role;

use super::model::{Connection, Model, RoomState};
use super::share::{self, ShareSource};

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
const ONLINE_DOT: u32 = 0x34C759;
const DANGER: u32 = 0xC9302C;
const WHITE: u32 = 0xFFFFFF;
const TITLE_BG: u32 = 0x0B0F1B;
/// Windows' red for a hovered close button.
const CLOSE_HOVER: u32 = 0xE81123;

/// Height of the app's own title bar.
pub const TITLE_H: f32 = 30.0;
/// Size of the window button glyphs (as Windows draws them: 10 px).
const CAPTION_GLYPH_PX: f32 = 10.0;

/// The window's state the title bar shows.
pub struct Chrome {
    pub maximized: bool,
    /// Title bar button under the mouse (`window::TITLE_*`, 0 = none).
    pub hovered: u8,
}

pub fn rgba(rgb: u32) -> [f32; 4] {
    [
        ((rgb >> 16) & 0xFF) as f32 / 255.0,
        ((rgb >> 8) & 0xFF) as f32 / 255.0,
        (rgb & 0xFF) as f32 / 255.0,
        1.0,
    ]
}

fn c(rgb: u32) -> [f32; 4] {
    rgba(rgb)
}

/// Slightly lighter, for hover.
fn hover(rgb: u32) -> u32 {
    let up = |v: u32| (v + (255 - v) / 10).min(255);
    (up((rgb >> 16) & 0xFF) << 16) | (up((rgb >> 8) & 0xFF) << 8) | up(rgb & 0xFF)
}

pub fn clear_color() -> [f32; 4] {
    rgba(BG)
}

/// Dear ImGui's own widgets (dialogs, text fields) in the same palette.
pub fn apply_style(style: &mut dear_imgui_rs::Style) {
    style.set_window_rounding(14.0);
    style.set_popup_rounding(14.0);
    style.set_frame_rounding(10.0);
    style.set_window_padding([22.0, 20.0]);
    style.set_frame_padding([12.0, 9.0]);
    style.set_item_spacing([10.0, 12.0]);
    style.set_window_border_size(1.0);
    for (color, value) in [
        (StyleColor::WindowBg, CARD),
        (StyleColor::PopupBg, CARD),
        (StyleColor::Border, BORDER),
        (StyleColor::FrameBg, BG),
        (StyleColor::FrameBgHovered, hover(BG)),
        (StyleColor::FrameBgActive, hover(BG)),
        (StyleColor::TitleBg, SIDEBAR),
        (StyleColor::TitleBgActive, SIDEBAR),
        (StyleColor::Button, ACTIVE),
        (StyleColor::ButtonHovered, hover(ACTIVE)),
        (StyleColor::ButtonActive, ACTIVE_BORDER),
        (StyleColor::Text, TEXT),
        (StyleColor::TextDisabled, MUTED),
        (StyleColor::Separator, DIVIDER),
    ] {
        style.set_color(color, rgba(value));
    }
    let mut dim = rgba(0x05070D);
    dim[3] = 0.7;
    style.set_color(StyleColor::ModalWindowDimBg, dim);
}

// ---------------------------------------------------------------- state

pub struct Fonts {
    pub regular: FontId,
    pub medium: FontId,
    pub semibold: FontId,
    pub bold: FontId,
    /// The Windows icon font (Segoe Fluent Icons, or MDL2 Assets on Windows
    /// 10): its sizes are pixels, not design sizes.
    pub icons: FontId,
    /// Line height over em size of the text fonts: Dear ImGui sizes fonts by
    /// line height, the design by em, so sizes are multiplied by this.
    pub text_scale: f32,
}

impl Fonts {
    pub fn scale(&self, font: FontId) -> f32 {
        if font == self.icons {
            1.0
        } else {
            self.text_scale
        }
    }

    /// The pixel size for a design size, whole: a fractional size puts the
    /// glyphs between pixels, which blurs them.
    pub fn px(&self, font: FontId, size: f32) -> f32 {
        (size * self.scale(font)).round()
    }
}

/// Icons: code points shared by Segoe Fluent Icons (Windows 11) and Segoe
/// MDL2 Assets (Windows 10), the fonts Windows draws its own icons with.
mod glyph {
    pub const CLOSE: &str = "\u{E8BB}";
    pub const SETTINGS: &str = "\u{E713}";
    pub const SEARCH: &str = "\u{E721}";
    pub const MONITOR: &str = "\u{E7F4}";
    pub const PANE: &str = "\u{E8A0}";
}

const SIDEBAR_MIN: f32 = 200.0;
const SIDEBAR_MAX: f32 = 420.0;
const RAIL: f32 = 64.0;

/// What the user asked for; the app carries it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Select(String),
    Enter(String),
    Exit(String),
    CopyInvite(String),
    Create(String),
    Join(String),
    Rename {
        room: String,
        name: String,
    },
    Remove {
        room: String,
        key: String,
    },
    Leave(String),
    /// Open the picker for sharing in this room (the app lists the sources).
    PickShare(String),
    Share {
        room: String,
        source: ShareSource,
    },
    StopShare,
}

/// Which dialog is open (one at a time).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Dialog {
    New,
    Rename,
    ConfirmRemove {
        key: String,
        name: String,
    },
    ConfirmLeave,
    /// What to share in this room.
    Share {
        room: String,
    },
}

/// The window's own state: layout, typing, dialogs, the last notice.
pub struct ViewState {
    search: String,
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    dialog: Option<Dialog>,
    /// Opens `dialog` on the next frame (popups open from outside them).
    open_dialog: bool,
    new_name: String,
    invite_text: String,
    rename: String,
    notice: Option<(String, Instant)>,
    /// What the share picker lists (filled when it opens).
    share_sources: Vec<ShareSource>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            search: String::new(),
            sidebar_width: 280.0,
            sidebar_collapsed: false,
            dialog: None,
            open_dialog: false,
            new_name: String::new(),
            invite_text: String::new(),
            rename: String::new(),
            notice: None,
            share_sources: Vec::new(),
        }
    }
}

impl ViewState {
    pub fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// Whether something on screen changes by itself soon (a notice fading).
    pub fn animating(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed().as_secs_f32() < NOTICE_SECS + 0.5)
    }

    fn open(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
        self.open_dialog = true;
    }

    /// Opens the share picker for `room` with what can be shared now.
    pub fn open_share(&mut self, room: String, sources: Vec<ShareSource>) {
        self.share_sources = sources;
        self.open(Dialog::Share { room });
    }
}

const NOTICE_SECS: f32 = 4.0;

// ---------------------------------------------------------------- drawing helpers

/// The logo drawn as shapes, not text: two z's joined by the i's stem, so the
/// word reads as one butterfly-like figure. Every part is whole pixels for a
/// letter height `h`, so the bars stay sharp at small sizes.
/// `scripts/make-icon.ps1` draws the same figure for the app icon.
#[derive(Clone, Copy, Debug)]
struct Mark {
    /// Letter height (the z's and the stem).
    h: f32,
    /// Thickness of the z's top and bottom bars.
    bar: f32,
    /// Width of the i's stem.
    stem: f32,
    /// Width of each z.
    z: f32,
    /// Horizontal width of the z's diagonal.
    slant: f32,
    /// The i's dot, and the space under it.
    dot: f32,
    gap: f32,
}

impl Mark {
    fn new(h: f32) -> Self {
        let h = h.round();
        let bar = (h * 0.22).round().max(1.0);
        let stem = (h * 0.3).round().max(bar);
        // The dot as wide as the stem or a bit wider, by an even amount so it
        // centers on the stem in whole pixels.
        let dot = stem + 2.0 * ((h * 0.36 - stem) / 2.0).round().max(0.0);
        Self {
            h,
            bar,
            stem,
            z: (h * 0.85).round(),
            // About 45 degrees, as thick across as the bars.
            slant: (h * 0.36).round().max(bar + 1.0),
            dot,
            gap: (h * 0.08).round().max(1.0),
        }
    }

    fn width(&self) -> f32 {
        self.z * 2.0 + self.stem
    }

    fn height(&self) -> f32 {
        self.dot + self.gap + self.h
    }
}

struct Painter<'a> {
    ui: &'a Ui,
    draw: dear_imgui_rs::DrawListMut<'a>,
    fonts: &'a Fonts,
}

impl Painter<'_> {
    fn px(&self, font: FontId, size: f32) -> f32 {
        self.fonts.px(font, size)
    }

    /// The small logo in the title bar, centered on (x, y).
    fn mini_logo(&self, x: f32, y: f32) {
        self.logo_pill(Mark::new(8.0), 17.0, 6.0, x, y);
    }

    /// The logo at the top of the sidebar, from `x`; returns its width.
    fn small_logo(&self, x: f32, center_y: f32) -> f32 {
        let mark = Mark::new(16.0);
        let w = mark.width() + 32.0;
        self.logo_pill(mark, 32.0, 16.0, x + w / 2.0, center_y);
        w
    }

    /// A blue pill `h` tall with the mark centered in it, centered on (x, y).
    /// The pill gets the mark's parity, so both center on whole pixels.
    fn logo_pill(&self, mark: Mark, h: f32, pad: f32, x: f32, y: f32) {
        let w = mark.width() + pad * 2.0;
        let (x0, y0) = ((x - w / 2.0).round(), (y - h / 2.0).round());
        self.rect([x0, y0], [x0 + w, y0 + h], Some(ACCENT), None, h / 2.0);
        let top = y0 + ((h - mark.height()) / 2.0).floor();
        self.mark(&mark, x0 + pad, top, WHITE);
    }

    /// Draws the mark with its top-left corner at (left, top), whole pixels.
    fn mark(&self, m: &Mark, left: f32, top: f32, color: u32) {
        let col = c(color);
        let base = top + m.dot + m.gap;
        let stem = left + m.z;
        // Rectangles with no rounding are not antialiased: the bars and the
        // stem stay sharp on whole pixels.
        let bar =
            |min: [f32; 2], max: [f32; 2]| self.draw.add_rect(min, max, col).filled(true).build();
        bar([stem, base], [stem + m.stem, base + m.h]);
        let dot = stem + (m.stem - m.dot) / 2.0;
        self.draw
            .add_rect([dot, top], [dot + m.dot, top + m.dot], col)
            .filled(true)
            .rounding(m.dot / 2.0)
            .build();
        for x in [left, stem + m.stem] {
            bar([x, base], [x + m.z, base + m.bar]);
            bar([x, base + m.h - m.bar], [x + m.z, base + m.h]);
            // The diagonal, from under the top bar's right end to over the
            // bottom bar's left end. It reaches half a pixel into both bars so
            // its antialiased ends leave no faint seam; clamped to the letter.
            let (y0, y1) = (base + m.bar - 0.5, base + m.h - m.bar + 0.5);
            let run = (m.z - m.slant) / (m.h - 2.0 * m.bar) * 0.5;
            let right = x + m.z;
            self.draw.add_quad_filled(
                [right - m.slant + run, y0],
                [(right + run).min(right), y0],
                [x + m.slant - run, y1],
                [(x - run).max(x), y1],
                col,
            );
        }
    }

    fn text_size(&self, font: FontId, size: f32, text: &str) -> [f32; 2] {
        let _token = self.ui.push_font_with_size(Some(font), self.px(font, size));
        self.ui.calc_text_size(text)
    }

    fn text(&self, font: FontId, size: f32, pos: [f32; 2], color: u32, text: &str) {
        // Whole pixels: text at fractional positions is resampled and blurs.
        let pos = [pos[0].round(), pos[1].round()];
        self.draw
            .add_text_with_font(font, self.px(font, size), pos, c(color), text, 0.0, None);
    }

    fn rect(
        &self,
        min: [f32; 2],
        max: [f32; 2],
        fill: Option<u32>,
        border: Option<(u32, f32)>,
        rounding: f32,
    ) {
        if let Some(fill) = fill {
            self.draw
                .add_rect(min, max, c(fill))
                .filled(true)
                .rounding(rounding)
                .build();
        }
        if let Some((color, thickness)) = border {
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
        let clicked = self
            .ui
            .invisible_button(id, [(max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0)]);
        let hovered = self.ui.is_item_hovered();
        if hovered {
            self.ui.set_mouse_cursor(Some(MouseCursor::Hand));
        }
        (clicked, hovered)
    }

    #[allow(clippy::too_many_arguments)]
    fn pill(
        &self,
        id: &str,
        min: [f32; 2],
        size: [f32; 2],
        fill: u32,
        border: Option<u32>,
        label: &str,
        font: FontId,
        font_size: f32,
        color: u32,
    ) -> bool {
        let max = [min[0] + size[0], min[1] + size[1]];
        let (clicked, hovered) = self.hit(id, min, max);
        let fill = if hovered { hover(fill) } else { fill };
        self.rect(
            min,
            max,
            Some(fill),
            border.map(|b| (b, 1.0)),
            size[1] / 2.0,
        );
        let t = self.text_size(font, font_size, label);
        self.text(
            font,
            font_size,
            [
                min[0] + (size[0] - t[0]) / 2.0,
                min[1] + (size[1] - t[1]) / 2.0,
            ],
            color,
            label,
        );
        clicked
    }

    #[allow(clippy::too_many_arguments)]
    fn badge(
        &self,
        pos: [f32; 2],
        text: &str,
        bg: u32,
        fg: u32,
        dot: Option<u32>,
        pad: [f32; 2],
        size: f32,
        border: Option<u32>,
    ) -> f32 {
        let t = self.text_size(self.fonts.bold, size, text);
        let dot_w = if dot.is_some() { 8.0 + 6.0 } else { 0.0 };
        let w = pad[0] * 2.0 + dot_w + t[0];
        let h = pad[1] * 2.0 + t[1];
        self.rect(
            pos,
            [pos[0] + w, pos[1] + h],
            Some(bg),
            border.map(|b| (b, 1.0)),
            h / 2.0,
        );
        if let Some(dot) = dot {
            self.draw
                .add_circle([pos[0] + pad[0] + 4.0, pos[1] + h / 2.0], 4.0, c(dot))
                .filled(true)
                .build();
        }
        self.text(
            self.fonts.bold,
            size,
            [pos[0] + pad[0] + dot_w, pos[1] + pad[1]],
            fg,
            text,
        );
        w
    }

    fn badge_height(&self, pad_y: f32, size: f32) -> f32 {
        pad_y * 2.0 + self.text_size(self.fonts.bold, size, "Ag")[1]
    }

    /// An icon from the Windows icon font, centered on `center`, at `px`
    /// pixels. The font is drawn for the pixel grid and hinted, so strokes
    /// come out whole and even at any size; lines and circles drawn here at
    /// fractional positions came out soft, lopsided or cut (one icon at a
    /// time). Sizes of 12, 16, 20 or 32 px suit its design grid best.
    fn icon(&self, glyph: &str, center: [f32; 2], px: f32, color: u32) {
        let t = self.text_size(self.fonts.icons, px, glyph);
        self.text(
            self.fonts.icons,
            px,
            [center[0] - t[0] / 2.0, center[1] - t[1] / 2.0],
            color,
            glyph,
        );
    }

    /// A plus of two 2-pixel bars on whole pixels (thin lines between pixels
    /// came out uneven once hinted).
    fn icon_plus(&self, origin: [f32; 2], size: f32, color: u32) {
        let (cx, cy) = (
            (origin[0] + size / 2.0).round(),
            (origin[1] + size / 2.0).round(),
        );
        let arm = (size * 0.32).round();
        self.rect(
            [cx - arm, cy - 1.0],
            [cx + arm, cy + 1.0],
            Some(color),
            None,
            0.0,
        );
        self.rect(
            [cx - 1.0, cy - arm],
            [cx + 1.0, cy + arm],
            Some(color),
            None,
            0.0,
        );
    }

    fn round_button(
        &self,
        id: &str,
        center: [f32; 2],
        d: f32,
        fill: u32,
        border: Option<u32>,
        icon: impl FnOnce([f32; 2]),
    ) -> bool {
        // On whole pixels, so the icon inside lands on the grid too.
        let center = [center[0].round(), center[1].round()];
        let (clicked, hovered) = self.hit(
            id,
            [center[0] - d / 2.0, center[1] - d / 2.0],
            [center[0] + d / 2.0, center[1] + d / 2.0],
        );
        self.draw
            .add_circle(center, d / 2.0, c(if hovered { hover(fill) } else { fill }))
            .filled(true)
            .build();
        if let Some(border) = border {
            self.draw
                .add_circle(center, d / 2.0 - 0.5, c(border))
                .thickness(1.0)
                .build();
        }
        icon(center);
        clicked
    }

    fn initial_avatar(
        &self,
        center: [f32; 2],
        d: f32,
        fill: u32,
        color: u32,
        name: &str,
        size: f32,
    ) {
        self.draw
            .add_circle(center, d / 2.0, c(fill))
            .filled(true)
            .build();
        let initial: String = name.chars().take(1).collect::<String>().to_uppercase();
        let t = self.text_size(self.fonts.bold, size, &initial);
        self.text(
            self.fonts.bold,
            size,
            [center[0] - t[0] / 2.0, center[1] - t[1] / 2.0],
            color,
            &initial,
        );
    }

    fn spaced_text(&self, font: FontId, size: f32, pos: [f32; 2], color: u32, text: &str, em: f32) {
        let mut x = pos[0];
        for ch in text.chars() {
            let s = ch.to_string();
            self.text(font, size, [x, pos[1]], color, &s);
            x += self.text_size(font, size, &s)[0] + em * size;
        }
    }

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

pub fn build(
    ui: &Ui,
    fonts: &Fonts,
    model: &Model,
    view: &mut ViewState,
    chrome: &Chrome,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let size = ui.io().display_size();
    ui.window("ziz")
        .position([0.0, 0.0], dear_imgui_rs::Condition::Always)
        .size(size, dear_imgui_rs::Condition::Always)
        .flags(
            WindowFlags::NO_DECORATION
                | WindowFlags::NO_MOVE
                | WindowFlags::NO_BACKGROUND
                | WindowFlags::NO_SAVED_SETTINGS
                | WindowFlags::NO_BRING_TO_FRONT_ON_FOCUS
                | WindowFlags::NO_SCROLL_WITH_MOUSE,
        )
        .build(|| {
            let p = Painter {
                ui,
                draw: ui.get_window_draw_list(),
                fonts,
            };
            title_bar(&p, chrome, size[0]);
            let sidebar_w = if view.sidebar_collapsed {
                RAIL
            } else {
                view.sidebar_width
            };
            if view.sidebar_collapsed {
                rail(&p, model, view, size[1], &mut actions);
            } else {
                sidebar(&p, model, view, size[1], &mut actions);
                resize_handle(&p, view, size[1]);
            }
            match model.selected_room() {
                Some(room) => {
                    room_view(&p, model, room, view, [sidebar_w, 0.0], size, &mut actions)
                }
                None => no_rooms(&p, view, [sidebar_w, 0.0], size),
            }
            notice(&p, view, [sidebar_w, 0.0], size);
        });
    dialogs(ui, fonts, model, view, &mut actions);
    actions
}

fn filtered<'m>(model: &'m Model, search: &str) -> (Vec<&'m RoomState>, Vec<&'m RoomState>) {
    let filter = search.to_lowercase();
    let matches = |r: &&RoomState| filter.is_empty() || r.name.to_lowercase().contains(&filter);
    let live = model
        .rooms
        .iter()
        .filter(|r| r.is_live())
        .filter(matches)
        .collect();
    let empty = model
        .rooms
        .iter()
        .filter(|r| !r.is_live())
        .filter(matches)
        .collect();
    (live, empty)
}

fn sidebar(
    p: &Painter<'_>,
    model: &Model,
    view: &mut ViewState,
    h: f32,
    actions: &mut Vec<Action>,
) {
    let w = view.sidebar_width;
    p.rect([0.0, TITLE_H], [w, h], Some(SIDEBAR), None, 0.0);
    p.draw
        .add_line([w - 0.5, TITLE_H], [w - 0.5, h], c(DIVIDER))
        .thickness(1.0)
        .build();
    let (left, right) = (14.0, w - 14.0);
    let mut y = TITLE_H + 20.0;

    let header_h = 34.0;
    p.small_logo(left + 4.0, y + header_h / 2.0);
    if p.round_button(
        "create",
        [right - 4.0 - 17.0, y + header_h / 2.0],
        34.0,
        ACCENT,
        Some(BORDER),
        |center| {
            p.icon_plus([center[0] - 8.0, center[1] - 8.0], 16.0, WHITE);
        },
    ) {
        view.open(Dialog::New);
    }
    y += header_h + 14.0;

    let sh = 38.0;
    p.rect(
        [left, y],
        [right, y + sh],
        Some(BG),
        Some((BORDER, 1.0)),
        sh / 2.0,
    );
    p.icon(glyph::SEARCH, [left + 24.0, y + sh / 2.0], 16.0, MUTED);
    {
        let _font =
            p.ui.push_font_with_size(Some(p.fonts.medium), p.fonts.px(p.fonts.medium, 14.0));
        let _frame =
            p.ui.push_style_color(StyleColor::FrameBg, [0.0, 0.0, 0.0, 0.0]);
        let _text = p.ui.push_style_color(StyleColor::Text, rgba(TEXT));
        let _hint = p.ui.push_style_color(StyleColor::TextDisabled, rgba(MUTED));
        let fh = p.ui.frame_height();
        p.ui.set_cursor_screen_pos([left + 16.0 + 16.0 + 6.0, y + (sh - fh) / 2.0]);
        p.ui.set_next_item_width((right - left - 16.0 - 16.0 - 6.0 - 12.0).max(10.0));
        p.ui.input_text("##search", &mut view.search)
            .hint("Buscar sala")
            .build();
    }
    y += sh + 14.0;

    let (live, empty) = filtered(model, &view.search);
    let user_card_h = 54.0;
    let list_bottom = h - 14.0 - user_card_h - 10.0;
    if model.rooms.is_empty() {
        p.text(
            p.fonts.regular,
            13.0,
            [left + 6.0, y + 4.0],
            MUTED,
            "Nenhuma sala ainda",
        );
    }
    for (title, rooms) in [("AO VIVO", &live), ("VAZIAS", &empty)] {
        if rooms.is_empty() {
            continue;
        }
        let label = format!("{title} · {}", rooms.len());
        p.spaced_text(
            p.fonts.bold,
            11.0,
            [left + 6.0, y + 6.0],
            MUTED,
            &label,
            0.06,
        );
        y += 6.0 + p.text_size(p.fonts.bold, 11.0, "Ag")[1] + 14.0;
        for room in rooms.iter() {
            let item_h = 36.0;
            if y + item_h > list_bottom {
                break;
            }
            if room_item(p, model, room, [left, y], [right, y + item_h]) {
                actions.push(Action::Select(room.id.clone()));
            }
            y += item_h + 2.0;
        }
        y += 8.0;
    }

    let cy = h - 14.0 - user_card_h;
    p.rect(
        [left, cy],
        [right, cy + user_card_h],
        Some(CARD),
        Some((BORDER, 1.0)),
        12.0,
    );
    p.initial_avatar(
        [left + 10.0 + 16.0, cy + user_card_h / 2.0],
        32.0,
        ACTIVE,
        TEXT,
        "Você",
        13.0,
    );
    let tx = left + 10.0 + 32.0 + 10.0;
    let name_h = p.text_size(p.fonts.semibold, 14.0, "Ag")[1];
    let status_h = p.text_size(p.fonts.regular, 12.0, "Ag")[1];
    let ty = cy + (user_card_h - name_h - 2.0 - status_h) / 2.0;
    p.text(p.fonts.semibold, 14.0, [tx, ty], TEXT, "Você");
    let status = p.fit(
        p.fonts.regular,
        12.0,
        &model.my_status(),
        right - 10.0 - 34.0 - 6.0 - tx,
    );
    p.text(
        p.fonts.regular,
        12.0,
        [tx, ty + name_h + 2.0],
        MUTED,
        &status,
    );
    p.round_button(
        "settings",
        [right - 10.0 - 17.0, cy + user_card_h / 2.0],
        34.0,
        CARD,
        None,
        |center| {
            p.icon(glyph::SETTINGS, center, 16.0, MUTED);
        },
    );
}

fn room_item(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    min: [f32; 2],
    max: [f32; 2],
) -> bool {
    let (clicked, hovered) = p.hit(&format!("room-{}", room.id), min, max);
    let selected = model.selected.as_deref() == Some(room.id.as_str());
    if selected {
        p.rect(min, max, Some(ACTIVE), Some((ACTIVE_BORDER, 1.0)), 10.0);
    } else if hovered {
        p.rect(min, max, Some(hover(SIDEBAR)), None, 10.0);
    }
    let live = room.is_live();
    let mid = (min[1] + max[1]) / 2.0;
    p.draw
        .add_circle(
            [min[0] + 10.0 + 4.0, mid],
            4.0,
            c(if live { LIVE_DOT } else { ACTIVE_BORDER }),
        )
        .filled(true)
        .build();
    let font = if selected {
        p.fonts.semibold
    } else {
        p.fonts.medium
    };
    let in_room = room.in_room_count();
    let count = if in_room > 0 {
        in_room.to_string()
    } else {
        String::new()
    };
    let cw = if count.is_empty() {
        0.0
    } else {
        p.text_size(p.fonts.regular, 12.0, &count)[0]
    };
    let name_x = min[0] + 10.0 + 8.0 + 10.0;
    let name = p.fit(font, 14.0, &room.name, max[0] - 10.0 - cw - 10.0 - name_x);
    let nh = p.text_size(font, 14.0, &name)[1];
    p.text(
        font,
        14.0,
        [name_x, mid - nh / 2.0],
        if live { TEXT } else { MUTED },
        &name,
    );
    if !count.is_empty() {
        let ch = p.text_size(p.fonts.regular, 12.0, &count)[1];
        p.text(
            p.fonts.regular,
            12.0,
            [max[0] - 10.0 - cw, mid - ch / 2.0],
            MUTED,
            &count,
        );
    }
    clicked
}

fn rail(p: &Painter<'_>, model: &Model, view: &mut ViewState, h: f32, actions: &mut Vec<Action>) {
    p.rect([0.0, TITLE_H], [RAIL, h], Some(SIDEBAR), None, 0.0);
    p.draw
        .add_line([RAIL - 0.5, TITLE_H], [RAIL - 0.5, h], c(DIVIDER))
        .thickness(1.0)
        .build();
    let x = RAIL / 2.0;
    let mut y = TITLE_H + 20.0 + 17.0;
    if p.round_button("create", [x, y], 34.0, ACCENT, Some(BORDER), |center| {
        p.icon_plus([center[0] - 8.0, center[1] - 8.0], 16.0, WHITE);
    }) {
        view.open(Dialog::New);
    }
    y += 17.0 + 18.0;
    let (live, empty) = filtered(model, "");
    let bottom = h - 14.0 - 40.0 - 10.0;
    for room in live.into_iter().chain(empty) {
        let d = 40.0;
        if y + d > bottom {
            break;
        }
        let center = [x, y + d / 2.0];
        let (clicked, hovered) = p.hit(
            &format!("rail-{}", room.id),
            [x - d / 2.0, y],
            [x + d / 2.0, y + d],
        );
        let selected = model.selected.as_deref() == Some(room.id.as_str());
        let fill = if selected {
            ACTIVE
        } else if hovered {
            hover(CARD)
        } else {
            CARD
        };
        let live_room = room.is_live();
        p.initial_avatar(
            center,
            d,
            fill,
            if live_room { TEXT } else { MUTED },
            &room.name,
            15.0,
        );
        p.draw
            .add_circle(
                center,
                d / 2.0 - 0.5,
                c(if selected { ACTIVE_BORDER } else { BORDER }),
            )
            .thickness(1.0)
            .build();
        if live_room {
            let dot = [center[0] + d / 2.0 - 5.0, center[1] - d / 2.0 + 5.0];
            p.draw.add_circle(dot, 5.5, c(SIDEBAR)).filled(true).build();
            p.draw
                .add_circle(dot, 4.0, c(LIVE_DOT))
                .filled(true)
                .build();
        }
        if hovered {
            p.ui.tooltip_text(&room.name);
        }
        if clicked {
            actions.push(Action::Select(room.id.clone()));
        }
        y += d + 8.0;
    }
    p.initial_avatar([x, h - 14.0 - 20.0], 40.0, ACTIVE, TEXT, "Você", 14.0);
}

fn resize_handle(p: &Painter<'_>, view: &mut ViewState, h: f32) {
    let x = view.sidebar_width;
    p.ui.set_cursor_screen_pos([x - 3.0, TITLE_H]);
    p.ui.invisible_button("sidebar-edge", [6.0, h - TITLE_H]);
    let (hovered, active) = (p.ui.is_item_hovered(), p.ui.is_item_active());
    if hovered || active {
        p.ui.set_mouse_cursor(Some(MouseCursor::ResizeEW));
        p.draw
            .add_line([x - 1.0, TITLE_H], [x - 1.0, h], c(ACCENT))
            .thickness(2.0)
            .build();
    }
    if active {
        view.sidebar_width = p.ui.io().mouse_pos()[0].clamp(SIDEBAR_MIN, SIDEBAR_MAX);
    }
}

/// The title row (sidebar toggle and a title); returns the y below it.
fn title_row(
    p: &Painter<'_>,
    view: &mut ViewState,
    left: f32,
    right: f32,
    top: f32,
    title: &str,
    live: bool,
) -> f32 {
    let title_h = p.text_size(p.fonts.bold, 30.0, "Ag")[1];
    let mid = top + title_h / 2.0;
    if p.round_button(
        "toggle-sidebar",
        [left + 17.0, mid],
        34.0,
        BG,
        Some(BORDER),
        |center| {
            p.icon(glyph::PANE, center, 16.0, TEXT);
        },
    ) {
        view.sidebar_collapsed = !view.sidebar_collapsed;
    }
    let mut x = left + 34.0 + 14.0;
    let title = p.fit(p.fonts.bold, 30.0, title, right - x - 120.0);
    let ts = p.text_size(p.fonts.bold, 30.0, &title);
    p.text(p.fonts.bold, 30.0, [x, top], TEXT, &title);
    x += ts[0] + 14.0;
    if live {
        let bh = p.badge_height(3.0, 12.0);
        p.badge(
            [x, mid - bh / 2.0],
            "Ao vivo",
            CARD,
            TEXT,
            Some(LIVE_DOT),
            [9.0, 3.0],
            12.0,
            Some(BORDER),
        );
    }
    top + title_h + 18.0
}

fn no_rooms(p: &Painter<'_>, view: &mut ViewState, origin: [f32; 2], [w, h]: [f32; 2]) {
    let (left, right) = (origin[0] + 36.0, w - 36.0);
    let (cx, cy) = ((left + right) / 2.0, (TITLE_H + h) / 2.0);
    let line = "Você ainda não está em nenhuma sala.";
    let s = p.text_size(p.fonts.semibold, 18.0, line);
    p.text(
        p.fonts.semibold,
        18.0,
        [cx - s[0] / 2.0, cy - 40.0],
        TEXT,
        line,
    );
    let label = "Criar sala ou entrar com convite";
    let bw = p.text_size(p.fonts.bold, 15.0, label)[0] + 56.0;
    if p.pill(
        "empty-new",
        [cx - bw / 2.0, cy],
        [bw, 48.0],
        ACCENT,
        Some(BORDER),
        label,
        p.fonts.bold,
        15.0,
        WHITE,
    ) {
        view.open(Dialog::New);
    }
}

/// The app's own title bar (no native one, as in Discord): drag area with
/// the logo in the middle and the window buttons at the right. Windows
/// handles dragging and the buttons through `window.rs` (hit-testing on the
/// layout published from here); this only draws them.
fn title_bar(p: &Painter<'_>, chrome: &Chrome, w: f32) {
    p.rect([0.0, 0.0], [w, TITLE_H], Some(TITLE_BG), None, 0.0);
    p.draw
        .add_line([0.0, TITLE_H - 0.5], [w, TITLE_H - 0.5], c(DIVIDER))
        .thickness(1.0)
        .build();
    p.mini_logo(w / 2.0, TITLE_H / 2.0);
    let bw = super::window::TITLE_BUTTON_WIDTH as f32;
    let left = w - 3.0 * bw;
    for (k, button) in [
        super::window::TITLE_MINIMIZE,
        super::window::TITLE_MAXIMIZE,
        super::window::TITLE_CLOSE,
    ]
    .into_iter()
    .enumerate()
    {
        let x = left + k as f32 * bw;
        let hovered = chrome.hovered == button;
        let close = button == super::window::TITLE_CLOSE;
        if hovered {
            p.rect(
                [x, 0.0],
                [x + bw, TITLE_H - 1.0],
                Some(if close { CLOSE_HOVER } else { ACTIVE }),
                None,
                0.0,
            );
        }
        let color = if hovered && close { WHITE } else { TEXT };
        // The glyphs Windows draws its own caption buttons with.
        let glyph = match button {
            super::window::TITLE_MINIMIZE => "\u{E921}",
            super::window::TITLE_MAXIMIZE if chrome.maximized => "\u{E923}",
            super::window::TITLE_MAXIMIZE => "\u{E922}",
            _ => glyph::CLOSE,
        };
        p.icon(
            glyph,
            [x + bw / 2.0, (TITLE_H - 1.0) / 2.0],
            CAPTION_GLYPH_PX,
            color,
        );
    }
}

fn connection_text(room: &RoomState) -> Option<String> {
    match &room.connection {
        Connection::Online => None,
        Connection::Connecting => Some("Conectando à sala…".to_owned()),
        Connection::Reconnecting(secs) => Some(format!(
            "Sem conexão com a sala; tentando de novo em {secs} s"
        )),
        Connection::Ended(reason) => Some(format!("Desconectado da sala ({reason})")),
    }
}

#[allow(clippy::too_many_arguments)]
fn room_view(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    origin: [f32; 2],
    [w, h]: [f32; 2],
    actions: &mut Vec<Action>,
) {
    let (left, right, top, bottom) = (origin[0] + 36.0, w - 36.0, TITLE_H + 30.0, h - 32.0);
    let screens = room.live_screens();
    let mut y = title_row(p, view, left, right, top, &room.name, !screens.is_empty());

    // Meta line (or the connection state).
    let meta = connection_text(room).unwrap_or_else(|| {
        let in_room = room.in_room_count();
        let screens_text = match screens.len() {
            0 => "ninguém transmitindo".to_owned(),
            1 => "1 tela ao vivo".to_owned(),
            n => format!("{n} telas ao vivo"),
        };
        let people = match in_room {
            0 => "ninguém na sala".to_owned(),
            1 => "1 pessoa na sala".to_owned(),
            n => format!("{n} pessoas na sala"),
        };
        format!(
            "{} participantes · {people} · {screens_text}",
            room.members.len()
        )
    });
    p.text(p.fonts.regular, 14.0, [left, y], MUTED, &meta);
    y += p.text_size(p.fonts.regular, 14.0, "Ag")[1] + 18.0;

    // Participants: a chip each, the dot says where they are.
    let i_own = room.role_of(&model.me) == Some(Role::Owner);
    let chip_h = 32.0;
    let mut cx = left;
    let mut members: Vec<_> = room.members.iter().collect();
    members.sort_by_key(|m| {
        (
            m.key != model.me,
            !room.presence.contains_key(&m.key),
            m.joined_at,
        )
    });
    for member in members {
        let you = member.key == model.me;
        let presence = room.presence.get(&member.key);
        let (dot, state) = match presence {
            Some(pr) if pr.screen.is_some() => (LIVE_DOT, "transmitindo"),
            Some(pr) if pr.entered => (ACCENT, "na sala"),
            Some(_) => (ONLINE_DOT, "online"),
            None => (ACTIVE_BORDER, "offline"),
        };
        let mut name = if you {
            format!("{} (você)", member.name)
        } else {
            member.name.clone()
        };
        if member.role == Role::Owner {
            name.push_str(" · dono");
        }
        let nw = p.text_size(p.fonts.semibold, 13.0, &name)[0];
        let removable = i_own && !you;
        let cw = 12.0 + 8.0 + 8.0 + nw + 12.0 + if removable { 22.0 } else { 0.0 };
        if cx + cw > right {
            break;
        }
        p.rect(
            [cx, y],
            [cx + cw, y + chip_h],
            Some(CARD),
            Some((BORDER, 1.0)),
            chip_h / 2.0,
        );
        p.draw
            .add_circle([cx + 12.0 + 4.0, y + chip_h / 2.0], 4.0, c(dot))
            .filled(true)
            .build();
        let nh = p.text_size(p.fonts.semibold, 13.0, &name)[1];
        let color = if presence.is_some() { TEXT } else { MUTED };
        p.text(
            p.fonts.semibold,
            13.0,
            [cx + 12.0 + 8.0 + 8.0, y + (chip_h - nh) / 2.0],
            color,
            &name,
        );
        p.ui.set_cursor_screen_pos([cx, y]);
        p.ui.invisible_button(
            format!("chip-{}", member.key),
            [cw - if removable { 22.0 } else { 0.0 }, chip_h],
        );
        if p.ui.is_item_hovered() {
            p.ui.tooltip_text(state);
        }
        if removable {
            let center = [cx + cw - 12.0 - 6.0, y + chip_h / 2.0];
            if p.round_button(
                &format!("remove-{}", member.key),
                center,
                20.0,
                CARD,
                None,
                |center| {
                    p.icon(glyph::CLOSE, center, 8.0, MUTED);
                },
            ) {
                view.open(Dialog::ConfirmRemove {
                    key: member.key.clone(),
                    name: member.name.clone(),
                });
            }
        }
        cx += cw + 8.0;
    }
    y += chip_h + 18.0;

    let button_h = 48.0;
    let by = bottom - button_h;
    screen_cards(p, &screens, [left, y], [right, by - 18.0]);
    room_buttons(p, model, room, view, [left, by], right, button_h, actions);
}

/// The live screens as cards (the pictures come with watching).
fn screen_cards(
    p: &Painter<'_>,
    screens: &[(String, String, rooms::Screen)],
    min: [f32; 2],
    max: [f32; 2],
) {
    if screens.is_empty() {
        let label = "Ninguém transmitindo nesta sala";
        let ls = p.text_size(p.fonts.semibold, 15.0, label);
        p.rect(min, max, Some(PREVIEW), Some((BORDER, 1.0)), 14.0);
        let block = 32.0 + 10.0 + ls[1];
        let top = (min[1] + max[1] - block) / 2.0;
        p.icon(
            glyph::MONITOR,
            [(min[0] + max[0]) / 2.0, top + 16.0],
            32.0,
            MUTED,
        );
        p.text(
            p.fonts.semibold,
            15.0,
            [(min[0] + max[0] - ls[0]) / 2.0, top + 42.0],
            MUTED,
            label,
        );
        return;
    }
    let gap = 16.0;
    let n = screens.len();
    let columns = n.min(3);
    let rows = n.div_ceil(columns);
    let tw = ((max[0] - min[0]) - gap * (columns - 1) as f32) / columns as f32;
    let th = (((max[1] - min[1]) - gap * (rows - 1) as f32) / rows as f32).min(tw * 9.0 / 16.0);
    for (i, (_, who, screen)) in screens.iter().enumerate() {
        let (col, row) = (i % columns, i / columns);
        let x = min[0] + col as f32 * (tw + gap);
        let y = min[1] + row as f32 * (th + gap);
        let (tmin, tmax) = ([x, y], [x + tw, y + th]);
        p.rect(tmin, tmax, Some(PREVIEW), None, 14.0);
        // The design's placeholder until watching shows the picture.
        let inset = (tw * 0.05).max(20.0);
        let top = y + (th * 0.08).max(18.0);
        p.rect(
            [x + inset, top],
            [x + tw - inset, y + th],
            Some(0x1B2A55),
            None,
            8.0,
        );
        p.rect(
            [x + inset, top],
            [x + tw - inset, top + 14.0],
            Some(0x2A3B6E),
            None,
            8.0,
        );
        p.rect(tmin, tmax, None, Some((BORDER, 1.0)), 14.0);
        let label = format!("{who} · {}", screen.info.name);
        let details = format!(
            "{}×{} · {} fps",
            screen.info.width, screen.info.height, screen.info.fps
        );
        let size = 12.0;
        let label = p.fit(p.fonts.bold, size, &label, tw - 24.0 - 13.0 - 6.0 - 24.0);
        let t = p.text_size(p.fonts.bold, size, &label);
        let bh = 5.0 * 2.0 + t[1];
        let bw = 12.0 * 2.0 + 13.0 + 6.0 + t[0];
        let bmin = [x + 12.0, y + th - 12.0 - bh];
        p.rect(
            bmin,
            [bmin[0] + bw, bmin[1] + bh],
            Some(CARD),
            Some((BORDER, 1.0)),
            bh / 2.0,
        );
        p.icon(
            glyph::MONITOR,
            [bmin[0] + 12.0 + 6.5, bmin[1] + bh / 2.0],
            12.0,
            TEXT,
        );
        p.text(
            p.fonts.bold,
            size,
            [bmin[0] + 12.0 + 13.0 + 6.0, bmin[1] + 5.0],
            TEXT,
            &label,
        );
        let ds = p.text_size(p.fonts.regular, 12.0, &details);
        p.text(
            p.fonts.regular,
            12.0,
            [x + tw - 12.0 - ds[0], y + 12.0],
            MUTED,
            &details,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn room_buttons(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    [x, y]: [f32; 2],
    right: f32,
    h: f32,
    actions: &mut Vec<Action>,
) {
    let here = model.entered.as_deref() == Some(room.id.as_str());
    let online = room.connection == Connection::Online;
    let mut x = x;
    let mut button =
        |id: &str, label: &str, fill: u32, color: u32, font: FontId, size: f32, pad: f32| -> bool {
            let w = p.text_size(font, size, label)[0] + pad * 2.0;
            let clicked = p.pill(
                id,
                [x, y],
                [w, h],
                fill,
                Some(BORDER),
                label,
                font,
                size,
                color,
            );
            x += w + 12.0;
            clicked && online
        };
    if here {
        if button(
            "exit",
            "Sair da sala",
            ACCENT,
            WHITE,
            p.fonts.bold,
            15.0,
            28.0,
        ) {
            actions.push(Action::Exit(room.id.clone()));
        }
    } else if button(
        "enter",
        "Entrar na sala",
        ACCENT,
        WHITE,
        p.fonts.bold,
        15.0,
        28.0,
    ) {
        actions.push(Action::Enter(room.id.clone()));
    }
    let sharing_here = model.sharing.as_ref().is_some_and(|s| s.room == room.id);
    if sharing_here {
        if button(
            "stop-share",
            "Parar transmissão",
            CARD,
            DANGER,
            p.fonts.semibold,
            14.0,
            18.0,
        ) {
            actions.push(Action::StopShare);
        }
    } else if button(
        "share",
        "Transmitir",
        CARD,
        TEXT,
        p.fonts.semibold,
        14.0,
        18.0,
    ) {
        actions.push(Action::PickShare(room.id.clone()));
    }
    if button(
        "invite",
        "Copiar convite",
        CARD,
        TEXT,
        p.fonts.semibold,
        14.0,
        18.0,
    ) {
        actions.push(Action::CopyInvite(room.id.clone()));
    }
    if room.role_of(&model.me) == Some(Role::Owner)
        && button(
            "rename",
            "Renomear",
            CARD,
            TEXT,
            p.fonts.semibold,
            14.0,
            18.0,
        )
    {
        view.rename = room.name.clone();
        view.open(Dialog::Rename);
    }
    // Leaving for good sits apart, at the right.
    let label = "Deixar a sala";
    let w = p.text_size(p.fonts.semibold, 14.0, label)[0] + 36.0;
    if p.pill(
        "leave",
        [right - w, y],
        [w, h],
        CARD,
        Some(BORDER),
        label,
        p.fonts.semibold,
        14.0,
        DANGER,
    ) && online
    {
        view.open(Dialog::ConfirmLeave);
    }
}

fn notice(p: &Painter<'_>, view: &mut ViewState, origin: [f32; 2], [w, h]: [f32; 2]) {
    let Some((text, at)) = &view.notice else {
        return;
    };
    let age = at.elapsed().as_secs_f32();
    if age > NOTICE_SECS {
        view.notice = None;
        return;
    }
    let t = p.text_size(p.fonts.semibold, 14.0, text);
    let (bw, bh) = (t[0] + 40.0, t[1] + 22.0);
    let x = (origin[0] + w - bw) / 2.0 + origin[0] / 2.0;
    let y = h - 32.0 - 48.0 - 18.0 - bh;
    p.rect(
        [x, y],
        [x + bw, y + bh],
        Some(ACTIVE),
        Some((ACTIVE_BORDER, 1.0)),
        bh / 2.0,
    );
    p.text(p.fonts.semibold, 14.0, [x + 20.0, y + 11.0], TEXT, text);
}

// ---------------------------------------------------------------- dialogs

fn dialogs(ui: &Ui, fonts: &Fonts, model: &Model, view: &mut ViewState, actions: &mut Vec<Action>) {
    const NAME: &str = "Sala";
    if view.open_dialog {
        view.open_dialog = false;
        ui.open_popup(NAME);
    }
    let Some(dialog) = view.dialog.clone() else {
        return;
    };
    let _font = ui.push_font_with_size(Some(fonts.medium), fonts.px(fonts.medium, 15.0));
    let center = ui.io().display_size();
    unsafe {
        // SAFETY: plain call on the current context, inside a frame.
        dear_imgui_sys::igSetNextWindowPos(
            dear_imgui_sys::ImVec2 {
                x: center[0] / 2.0,
                y: center[1] / 2.0,
            },
            dear_imgui_sys::ImGuiCond_Appearing,
            dear_imgui_sys::ImVec2 { x: 0.5, y: 0.5 },
        );
    }
    let Some(_token) = ui
        .begin_modal_popup_config(NAME)
        .flags(WindowFlags::ALWAYS_AUTO_RESIZE | WindowFlags::NO_TITLE_BAR | WindowFlags::NO_MOVE)
        .begin()
    else {
        view.dialog = None;
        return;
    };
    let mut close = ui.is_key_pressed(dear_imgui_rs::Key::Escape);
    match dialog {
        Dialog::New => {
            heading(ui, fonts, "Criar uma sala");
            ui.text_disabled("Dê um nome; depois é só copiar o convite e mandar aos amigos.");
            ui.set_next_item_width(440.0);
            let enter = ui
                .input_text("##new-name", &mut view.new_name)
                .hint("Nome da sala (ex.: Sexta à noite)")
                .enter_returns_true(true)
                .build();
            if (ui.button("Criar sala") || enter) && !view.new_name.trim().is_empty() {
                actions.push(Action::Create(view.new_name.trim().to_owned()));
                view.new_name.clear();
                close = true;
            }
            ui.separator();
            heading(ui, fonts, "Entrar com um convite");
            ui.text_disabled("Cole o convite que recebeu (começa com zizroom:).");
            ui.set_next_item_width(440.0);
            let enter = ui
                .input_text("##invite", &mut view.invite_text)
                .hint("zizroom:…")
                .enter_returns_true(true)
                .build();
            if ui.button("Colar")
                && let Ok(Some(text)) = crate::clipboard::get_text()
            {
                view.invite_text = text.trim().to_owned();
            }
            ui.same_line();
            if (ui.button("Entrar") || enter) && !view.invite_text.trim().is_empty() {
                actions.push(Action::Join(view.invite_text.trim().to_owned()));
                view.invite_text.clear();
                close = true;
            }
            ui.separator();
            if ui.button("Cancelar") {
                close = true;
            }
        }
        Dialog::Rename => {
            heading(ui, fonts, "Renomear a sala");
            ui.set_next_item_width(400.0);
            let enter = ui
                .input_text("##rename", &mut view.rename)
                .enter_returns_true(true)
                .build();
            if (ui.button("Salvar") || enter) && !view.rename.trim().is_empty() {
                if let Some(room) = model.selected.clone() {
                    actions.push(Action::Rename {
                        room,
                        name: view.rename.trim().to_owned(),
                    });
                }
                close = true;
            }
            ui.same_line();
            if ui.button("Cancelar") {
                close = true;
            }
        }
        Dialog::Share { room } => {
            heading(ui, fonts, "O que você quer transmitir?");
            ui.text_disabled(format!(
                "{}p · {} fps (o padrão; mais opções virão nas configurações)",
                share::MAX_SIZE.1,
                share::FPS
            ));
            let mut picked = None;
            let (monitors, windows): (Vec<_>, Vec<_>) = view
                .share_sources
                .iter()
                .partition(|s| matches!(s, ShareSource::Monitor { .. }));
            ui.separator();
            ui.text("Telas");
            for source in &monitors {
                if ui.selectable(source.label()) {
                    picked = Some((*source).clone());
                }
            }
            ui.separator();
            ui.text("Janelas");
            if windows.is_empty() {
                ui.text_disabled("Nenhuma janela aberta para transmitir.");
            } else {
                ui.child_window("share-windows")
                    .size([560.0, 260.0])
                    .border(true)
                    .build(ui, || {
                        for source in &windows {
                            if ui.selectable(source.label()) {
                                picked = Some((*source).clone());
                            }
                        }
                    });
            }
            if let Some(source) = picked {
                actions.push(Action::Share { room, source });
                close = true;
            }
            ui.separator();
            if ui.button("Cancelar") {
                close = true;
            }
        }
        Dialog::ConfirmRemove { key, name } => {
            heading(ui, fonts, &format!("Remover {name} da sala?"));
            ui.text_disabled("Para voltar, a pessoa vai precisar de um convite novo.");
            if ui.button("Remover") {
                if let Some(room) = model.selected.clone() {
                    actions.push(Action::Remove { room, key });
                }
                close = true;
            }
            ui.same_line();
            if ui.button("Cancelar") {
                close = true;
            }
        }
        Dialog::ConfirmLeave => {
            heading(ui, fonts, "Deixar a sala de vez?");
            ui.text_disabled(
                "Ela some da sua lista; para voltar, você vai precisar de um convite.",
            );
            ui.text_disabled(
                "Fechar o app não é deixar a sala: ela continua aí para a próxima vez.",
            );
            if ui.button("Deixar a sala") {
                if let Some(room) = model.selected.clone() {
                    actions.push(Action::Leave(room));
                }
                close = true;
            }
            ui.same_line();
            if ui.button("Cancelar") {
                close = true;
            }
        }
    }
    if close {
        ui.close_current_popup();
        view.dialog = None;
    }
}

fn heading(ui: &Ui, fonts: &Fonts, text: &str) {
    let _font = ui.push_font_with_size(Some(fonts.bold), fonts.px(fonts.bold, 18.0));
    ui.text(text);
}
