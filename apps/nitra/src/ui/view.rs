//! The window's drawing (the sidebar design of the prototype, `db76675`):
//! the rooms on the left (live and empty sections, search; resizable and
//! collapsible), the selected room on the right with its participants and
//! live screens, and the dialogs. It reads the [`Model`] and returns what the
//! user asked for as [`Action`]s; the app does the rest. Selecting a room
//! only shows it; "Entrar na sala" enters. Laid out by hand from the window
//! size and drawn with the draw list.

use std::time::Instant;

use dear_imgui_rs::{FontId, MouseCursor, StyleColor, Ui, WindowFlags};
use rooms::{Role, Screen};

use super::model::{Connection, Model, RoomState};
use super::settings::{self, Fluidity, PREVIEW_CHOICES, Settings};
use super::share::ShareSource;

// ---------------------------------------------------------------- palette

// From the visual guide (nitra-design-final): dark, thin borders, blue
// #0047FF with cyan and indigo beside it, red only for "live".
const BG: u32 = 0x0D0F17;
const SIDEBAR: u32 = 0x121520;
const CARD: u32 = 0x171B28;
const PREVIEW: u32 = 0x080A10;
/// Dear ImGui's own buttons (dialogs) and pressed states.
const ACTIVE: u32 = 0x232838;
const ACTIVE_BORDER: u32 = 0x343A50;
const BORDER: u32 = 0x262B3B;
const DIVIDER: u32 = 0x1E2232;
const ACCENT: u32 = 0x0047FF;
/// A selected item (room in the list, option of a selector): fill, with an
/// ACCENT border.
const SELECTED: u32 = 0x10235C;
/// Who is sharing: dot and badge text, and the badge fill.
const CYAN: u32 = 0x00C2FF;
const CYAN_BG: u32 = 0x0B2A3A;
/// A room with no screen live: dashed dot and italic name.
const EMPTY: u32 = 0x5B6070;
const TEXT: u32 = 0xEEF0F6;
const MUTED: u32 = 0x9097AB;
const LIVE_DOT: u32 = 0xFF3B30;
const ONLINE_DOT: u32 = 0x34C759;
const DANGER: u32 = 0xC9302C;
const WHITE: u32 = 0xFFFFFF;
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
    // Rounder curves: Dear ImGui draws them as straight segments, at most
    // this far (pixels) from the true curve (0.3 by default showed steps on
    // the pills).
    style.set_circle_tessellation_max_error(0.1);
    style.set_window_rounding(14.0);
    style.set_popup_rounding(14.0);
    style.set_frame_rounding(10.0);
    style.set_window_padding([22.0, 20.0]);
    style.set_frame_padding([12.0, 9.0]);
    style.set_item_spacing([10.0, 12.0]);
    // Room between a checkbox (or radio) and its label.
    style.set_item_inner_spacing([12.0, 8.0]);
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
        (StyleColor::CheckMark, ACCENT),
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
    /// Medium italic: the name of a room with no screen live.
    pub italic: FontId,
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
    /// The pane icon facing right (the share panel).
    pub const PANE_RIGHT: &str = "\u{EA5B}";
    pub const FULLSCREEN: &str = "\u{E740}";
    pub const VOLUME: &str = "\u{E767}";
    pub const MUTED: &str = "\u{E74F}";
    pub const MORE: &str = "\u{E712}";
    /// A window opening out (a watched screen into a floating window).
    pub const FLOAT: &str = "\u{E8A7}";
    /// An eye (the own share's preview).
    pub const VIEW: &str = "\u{E890}";
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
    /// Share on a GPU nitra was not tested on, as accepted in the warning;
    /// `remember`: do not ask again.
    ShareAnyway {
        room: String,
        source: ShareSource,
        remember: bool,
    },
    StopShare,
    /// Watch the screen `key` shares in `room`.
    Watch {
        room: String,
        key: String,
    },
    StopWatch(String),
    /// Show this watched screen large, or back to the grid.
    Focus(String),
    /// This watched screen fills the window, or back.
    Fullscreen(String),
    Mute(String),
    /// A watched screen out of the app into a window above every other, or
    /// back.
    Float(String),
    /// A watched screen's volume (percent); `done`: the slider was let go
    /// (saved for that person).
    Volume {
        key: String,
        percent: u8,
        done: bool,
    },
    /// Show or hide the numbers over a watched screen.
    Metrics(String),
    /// Hide or show the share panel beside the screens (saved).
    ToggleSharePanel,
    /// Show or hide the live picture of your own share (saved).
    ToggleOwnPreview,
    OpenSettings,
    SaveSettings(Box<Settings>),
    /// Open this profile's logs folder in Explorer.
    OpenLogs,
    /// Erase this profile from the PC and close (after the confirmation).
    EraseData,
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
    /// Sharing on a GPU nitra was not tested on: go ahead?
    ConfirmUntestedGpu {
        room: String,
        source: ShareSource,
    },
    /// The first start: the name others will see.
    Welcome,
    Settings,
    /// "Apagar meus dados deste PC": sure?
    ConfirmErase,
}

/// The window's own state: layout, typing, dialogs, the last notice.
pub struct ViewState {
    search: String,
    pub sidebar_width: f32,
    pub sidebar_collapsed: bool,
    /// The room panel's tab.
    panel_tab: PanelTab,
    dialog: Option<Dialog>,
    /// Opens `dialog` on the next frame (popups open from outside them).
    open_dialog: bool,
    new_name: String,
    invite_text: String,
    rename: String,
    notice: Option<(String, Instant)>,
    /// The span (left and right x) notices are centered on this frame: the
    /// screens inside a room, else the whole content area.
    notice_span: Option<[f32; 2]>,
    /// What the share picker lists (filled when it opens).
    share_sources: Vec<ShareSource>,
    /// Where each watched screen's picture goes this frame (watch id, x, y,
    /// width, height); not listed: hidden.
    pub video_rects: Vec<(u64, [f32; 4])>,
    /// Where our own share's live picture goes this frame (None: hidden).
    pub own_preview_rect: Option<[f32; 4]>,
    /// The settings being edited, and their text fields.
    draft: Settings,
    name_text: String,
    apps_text: String,
    /// The untested GPU warning's "do not ask again".
    remember_gpu: bool,
    /// The watched screen whose volume slider shows in its strip (opened by
    /// hovering the speaker; closed when the mouse leaves the strip).
    volume_open: Option<String>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            search: String::new(),
            sidebar_width: 280.0,
            sidebar_collapsed: false,
            panel_tab: PanelTab::default(),
            dialog: None,
            open_dialog: false,
            new_name: String::new(),
            invite_text: String::new(),
            rename: String::new(),
            notice: None,
            notice_span: None,
            share_sources: Vec::new(),
            video_rects: Vec::new(),
            own_preview_rect: None,
            draft: Settings::default(),
            name_text: String::new(),
            apps_text: String::new(),
            remember_gpu: true,
            volume_open: None,
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
            .is_some_and(|(text, at)| at.elapsed().as_secs_f32() < notice_secs(text) + 0.5)
    }

    fn open(&mut self, dialog: Dialog) {
        self.dialog = Some(dialog);
        self.open_dialog = true;
    }

    /// The first start's question: the name others will see (`suggestion`
    /// filled in).
    pub fn open_welcome(&mut self, suggestion: String) {
        self.name_text = suggestion;
        self.open(Dialog::Welcome);
    }

    /// The settings, to edit; `name` is the one in use.
    pub fn open_settings(&mut self, current: Settings, name: String) {
        self.name_text = name;
        self.apps_text = current.audio_exclude.join(", ");
        self.draft = current;
        self.open(Dialog::Settings);
    }

    /// Asks before sharing `source` on a GPU nitra was not tested on.
    pub fn open_untested_gpu(&mut self, room: String, source: ShareSource) {
        self.remember_gpu = true;
        self.open(Dialog::ConfirmUntestedGpu { room, source });
    }

    /// Opens the share picker for `room` with what can be shared now.
    pub fn open_share(&mut self, room: String, sources: Vec<ShareSource>) {
        self.share_sources = sources;
        self.open(Dialog::Share { room });
    }
}

/// How long a notice stays: 4 s, longer for long ones (time to read them).
fn notice_secs(text: &str) -> f32 {
    (4.0 + text.chars().count() as f32 / 25.0).min(12.0)
}

/// `text` broken into lines at spaces, each at most `max_w` wide (a single
/// word wider than that keeps its own line).
fn wrap(p: &Painter<'_>, font: FontId, size: f32, text: &str, max_w: f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split(' ') {
        match lines.last_mut() {
            Some(line) if p.text_size(font, size, &format!("{line} {word}"))[0] <= max_w => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

// ---------------------------------------------------------------- drawing helpers

/// Pictures the view draws besides text and shapes.
pub struct Art {
    /// The logo (a blue pill with "nitra"), drawn at its own pixel size so
    /// it is never resized; None if it could not be loaded (then a plain
    /// text pill stands in).
    pub logo: Option<Picture>,
}

/// A texture Dear ImGui can draw, with its size in pixels.
#[derive(Clone, Copy, Debug)]
pub struct Picture {
    pub texture: u64,
    pub size: [f32; 2],
}

struct Painter<'a> {
    ui: &'a Ui,
    draw: dear_imgui_rs::DrawListMut<'a>,
    fonts: &'a Fonts,
    art: &'a Art,
}

impl Painter<'_> {
    fn px(&self, font: FontId, size: f32) -> f32 {
        self.fonts.px(font, size)
    }

    /// The logo at the top of the sidebar, from `x`, centered on `center_y`;
    /// returns its width. The picture sits on whole pixels, one texel per
    /// pixel, so its edges stay as smooth as they were drawn.
    fn logo(&self, x: f32, center_y: f32) -> f32 {
        let Some(logo) = self.art.logo else {
            return self.badge_logo(x, center_y);
        };
        let min = [x.round(), (center_y - logo.size[1] / 2.0).round()];
        let max = [min[0] + logo.size[0], min[1] + logo.size[1]];
        self.draw.add_image(
            dear_imgui_rs::TextureId::new(logo.texture),
            min,
            max,
            [0.0, 0.0],
            [1.0, 1.0],
            c(WHITE),
        );
        logo.size[0]
    }

    /// A segmented pill selector: equal options in a dark pill, the chosen
    /// one in blue (`accent`, for values) or with the "selected" look (for
    /// presets and tabs). Returns the option clicked.
    fn segmented(
        &self,
        id: &str,
        [x, y, w, h]: [f32; 4],
        labels: &[&str],
        selected: Option<usize>,
        accent: bool,
    ) -> Option<usize> {
        self.rect([x, y], [x + w, y + h], Some(BG), None, h / 2.0);
        let pad = 4.0;
        let ow = (w - pad * 2.0) / labels.len() as f32;
        let oh = h - pad * 2.0;
        let mut clicked = None;
        for (k, label) in labels.iter().enumerate() {
            let min = [(x + pad + ow * k as f32).round(), y + pad];
            let max = [(x + pad + ow * (k + 1) as f32).round(), y + pad + oh];
            let (click, hovered) = self.hit(&format!("{id}-{k}"), min, max);
            let chosen = selected == Some(k);
            let color = if chosen && accent {
                self.rect(min, max, Some(ACCENT), None, oh / 2.0);
                WHITE
            } else if chosen {
                self.rect(min, max, Some(SELECTED), Some((ACCENT, 1.0)), oh / 2.0);
                TEXT
            } else {
                if hovered {
                    self.rect(min, max, Some(hover(BG)), None, oh / 2.0);
                }
                TEXT
            };
            let ts = self.text_size(self.fonts.semibold, 12.0, label);
            self.text(
                self.fonts.semibold,
                12.0,
                [
                    (min[0] + max[0] - ts[0]) / 2.0,
                    (min[1] + max[1] - ts[1]) / 2.0,
                ],
                color,
                label,
            );
            if click {
                clicked = Some(k);
            }
        }
        clicked
    }

    /// A slider from `range.0` to `range.1` in steps of `step`: a thin track,
    /// the part up to the value in blue, a light knob with a blue ring.
    /// Returns the new value while it is dragged.
    fn slider(
        &self,
        id: &str,
        [x, y, w, h]: [f32; 4],
        value: u32,
        (lo, hi): (u32, u32),
        step: u32,
    ) -> Option<u32> {
        let span = (hi - lo) as f32;
        let at = |v: u32| x + w * (v.clamp(lo, hi) - lo) as f32 / span;
        self.ui.set_cursor_screen_pos([x, y]);
        self.ui.invisible_button(id, [w, h]);
        let mut changed = None;
        if self.ui.is_item_active() {
            let mouse = self.ui.io().mouse_pos()[0];
            let raw = lo as f32 + ((mouse - x) / w).clamp(0.0, 1.0) * span;
            let v = ((raw / step as f32).round() as u32 * step).clamp(lo, hi);
            if v != value {
                changed = Some(v);
            }
        }
        let mid = y + h / 2.0;
        let knob = at(changed.unwrap_or(value));
        self.rect(
            [x, mid - 3.0],
            [x + w, mid + 3.0],
            Some(BG),
            Some((BORDER, 1.0)),
            3.0,
        );
        self.rect([x, mid - 4.0], [knob, mid + 4.0], Some(ACCENT), None, 4.0);
        let center = [knob.round(), mid.round()];
        self.draw
            .add_circle(center, 9.0, c(ACCENT))
            .filled(true)
            .build();
        self.draw
            .add_circle(center, 6.0, c(TEXT))
            .filled(true)
            .build();
        changed
    }

    /// A circle outline in short dashes (an empty room's dot), `thickness`
    /// wide, of radius `r`.
    fn dashed_circle(&self, center: [f32; 2], r: f32, color: u32, thickness: f32) {
        const DASHES: usize = 6;
        const STEPS: usize = 4;
        let col = c(color);
        let slot = std::f32::consts::TAU / DASHES as f32;
        for k in 0..DASHES {
            // Each dash covers half its slot; the gaps are the other half.
            let start = k as f32 * slot;
            let points: Vec<[f32; 2]> = (0..=STEPS)
                .map(|s| {
                    let a = start + slot * 0.5 * s as f32 / STEPS as f32;
                    [center[0] + r * a.cos(), center[1] + r * a.sin()]
                })
                .collect();
            self.draw
                .add_polyline(points, col)
                .thickness(thickness)
                .build();
        }
    }

    /// The logo as text in a blue pill, if the picture is missing.
    fn badge_logo(&self, x: f32, center_y: f32) -> f32 {
        let [tw, th] = self.text_size(self.fonts.bold, 20.0, "nitra");
        let (w, h) = ((tw + 24.0).round(), (th + 6.0).round());
        let y = (center_y - h / 2.0).round();
        self.rect([x, y], [x + w, y + h], Some(ACCENT), None, h / 2.0);
        self.text(self.fonts.bold, 20.0, [x + 12.0, y + 3.0], WHITE, "nitra");
        w
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
        if let (Some(fill), Some((color, t))) = (fill, border) {
            // The border as a filled shape under the fill, which is inset by
            // it: filled shapes get an even, smooth edge on the curves, where
            // a 1 px stroke thins out and steps.
            self.draw
                .add_rect(min, max, c(color))
                .filled(true)
                .rounding(rounding)
                .build();
            self.draw
                .add_rect([min[0] + t, min[1] + t], [max[0] - t, max[1] - t], c(fill))
                .filled(true)
                .rounding((rounding - t).max(0.0))
                .build();
            return;
        }
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
        let fill = c(if hovered { hover(fill) } else { fill });
        // A border is a full disc under a smaller one: filled shapes get an
        // even, smooth edge, where a 1 px stroke thins out along the curve.
        let inner = match border {
            Some(border) => {
                self.draw
                    .add_circle(center, d / 2.0, c(border))
                    .filled(true)
                    .build();
                d / 2.0 - 1.0
            }
            None => d / 2.0,
        };
        self.draw
            .add_circle(center, inner, fill)
            .filled(true)
            .build();
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
    art: &Art,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let size = ui.io().display_size();
    view.video_rects.clear();
    view.own_preview_rect = None;
    ui.window("nitra")
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
                art,
            };
            if let Some(key) = &model.fullscreen {
                fullscreen_view(&p, model, view, key, size, &mut actions);
                return;
            }
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
            content_panel(&p, sidebar_w, size);
            view.notice_span = None;
            match model.selected_room() {
                Some(room) => {
                    room_view(&p, model, room, view, [sidebar_w, 0.0], size, &mut actions)
                }
                None => no_rooms(&p, view, [sidebar_w, 0.0], size),
            }
            let span = view.notice_span.unwrap_or([sidebar_w, size[0]]);
            notice(&p, view, span, size[1]);
        });
    dialogs(ui, fonts, model, view, &mut actions);
    // Video windows sit above everything we draw: hidden under a dialog.
    if view.dialog.is_some() {
        view.video_rects.clear();
        view.own_preview_rect = None;
    }
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
    let (left, right) = (14.0, w - 14.0);
    let mut y = TITLE_H + 20.0;

    let header_h = 34.0;
    p.logo(left + 4.0, y + header_h / 2.0);
    if p.round_button(
        "create",
        [right - 4.0 - 17.0, y + header_h / 2.0],
        34.0,
        ACCENT,
        // No ring: the blue disc as tall as the logo beside it.
        None,
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
    let user_card_h = USER_CARD_H;
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
    // Rooms without a live screen may still have people in them.
    for (title, rooms) in [("AO VIVO", &live), ("SEM TRANSMISSÃO", &empty)] {
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
    // No avatar: nitra has no profile pictures, a circle would only repeat
    // the name's first letter.
    let tx = left + 14.0;
    let name_h = p.text_size(p.fonts.semibold, 14.0, "Ag")[1];
    let status_h = p.text_size(p.fonts.regular, 12.0, "Ag")[1];
    let ty = cy + (user_card_h - name_h - 2.0 - status_h) / 2.0;
    let name = p.fit(
        p.fonts.semibold,
        14.0,
        &model.my_name,
        right - 10.0 - 34.0 - 6.0 - tx,
    );
    p.text(p.fonts.semibold, 14.0, [tx, ty], TEXT, &name);
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
    if p.round_button(
        "settings",
        [right - 10.0 - 17.0, cy + user_card_h / 2.0],
        34.0,
        CARD,
        None,
        |center| {
            p.icon(glyph::SETTINGS, center, 16.0, MUTED);
        },
    ) {
        actions.push(Action::OpenSettings);
    }
    if p.ui.is_item_hovered() {
        p.ui.tooltip_text("Configurações");
    }
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
        p.rect(min, max, Some(SELECTED), Some((ACCENT, 1.0)), 10.0);
    } else if hovered {
        p.rect(min, max, Some(hover(SIDEBAR)), None, 10.0);
    }
    let live = room.is_live();
    let mid = (min[1] + max[1]) / 2.0;
    let dot = [min[0] + 10.0 + 4.0, mid];
    if live {
        p.draw
            .add_circle(dot, 4.0, c(LIVE_DOT))
            .filled(true)
            .build();
    } else {
        p.dashed_circle(dot, 3.25, EMPTY, 1.5);
    }
    let font = if !live {
        p.fonts.italic
    } else if selected {
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
        if live { TEXT } else { EMPTY },
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
    let x = RAIL / 2.0;
    let mut y = TITLE_H + 20.0 + 17.0;
    if p.round_button("create", [x, y], 34.0, ACCENT, None, |center| {
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
    title: &str,
    live: bool,
) -> f32 {
    let title_h = p.text_size(p.fonts.bold, 30.0, "Ag")[1];
    // On the line of the sidebar's logo and "+" button.
    let mid = HEADER_MID;
    let top = mid - title_h / 2.0;
    sidebar_toggle(p, view, [left + 17.0, mid]);
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
    // Where the room view has it, so the sidebar folds here too.
    sidebar_toggle(p, view, [left + 17.0, HEADER_MID]);
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

/// The sidebar's user card (the bottom row) and its middle, from the window's
/// bottom edge (14 px of margin under it): the room's buttons line up on it.
const USER_CARD_H: f32 = 54.0;
const FOOTER_MID_FROM_BOTTOM: f32 = 14.0 + USER_CARD_H / 2.0;

/// The middle of the first row: the sidebar's logo and "+" button, and the
/// fold button and title beside them.
const HEADER_MID: f32 = TITLE_H + 20.0 + 17.0;

/// Corner radius of the content panel where it meets the sidebar and the
/// title bar.
const PANEL_CORNER: f32 = 12.0;

/// The content area as a panel set into the frame that the title bar and the
/// sidebar make (as in Discord): its top-left corner is rounded and a thin
/// border runs along its top and left edges. Drawn before the content.
fn content_panel(p: &Painter<'_>, x: f32, [w, h]: [f32; 2]) {
    let top = TITLE_H;
    // The frame's color behind the rounded corner (the window is cleared to
    // the panel's color).
    p.rect(
        [x, top],
        [x + PANEL_CORNER, top + PANEL_CORNER],
        Some(SIDEBAR),
        None,
        0.0,
    );
    // The border (top and left edges, and the corner) as a filled shape with
    // the panel 1 px inside it: an even, smooth curve, where a 1 px stroke
    // thins out and steps.
    for (inset, color) in [(0.0, BORDER), (1.0, BG)] {
        p.draw
            .add_rect([x + inset, top + inset], [w, h], c(color))
            .filled(true)
            .rounding(PANEL_CORNER - inset)
            .flags(dear_imgui_rs::DrawFlags::ROUND_TOP_LEFT)
            .build();
    }
}

/// The round button that folds the sidebar into a rail (or opens it back),
/// centered on `center`.
fn sidebar_toggle(p: &Painter<'_>, view: &mut ViewState, center: [f32; 2]) {
    if p.round_button("toggle-sidebar", center, 34.0, BG, Some(BORDER), |c| {
        p.icon(glyph::PANE, c, 16.0, TEXT);
    }) {
        view.sidebar_collapsed = !view.sidebar_collapsed;
    }
}

/// The app's own title bar (no native one, as in Discord): drag area with
/// the window buttons at the right (the logo is in the sidebar). Windows
/// handles dragging and the buttons through `window.rs` (hit-testing on the
/// layout published from here); this only draws them.
fn title_bar(p: &Painter<'_>, chrome: &Chrome, w: f32) {
    // The sidebar's color: title bar and sidebar read as one frame around
    // the content panel (as in Discord).
    p.rect([0.0, 0.0], [w, TITLE_H], Some(SIDEBAR), None, 0.0);
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
    if model.entered.as_deref() == Some(room.id.as_str()) {
        inside_room(p, model, room, view, origin, [w, h], actions);
        return;
    }
    // Not in the room: its preview (clicking a room only shows this).
    let (left, right) = (origin[0] + 36.0, w - 36.0);
    let screens = room.live_screens();
    let mut y = title_row(p, view, left, right, &room.name, !screens.is_empty());
    let meta = connection_text(room).unwrap_or_else(|| counts_text(room, screens.len()));
    p.text(p.fonts.regular, 14.0, [left, y], MUTED, &meta);
    y += p.text_size(p.fonts.regular, 14.0, "Ag")[1] + 18.0;
    if !screens.is_empty() {
        y = sharing_chips(p, &screens, left, right, y) + 18.0;
    }
    let button_h = 48.0;
    // Centered on the sidebar's user card, so the two bottom rows line up.
    let by = (h - FOOTER_MID_FROM_BOTTOM - button_h / 2.0).round();
    screens_area(
        p,
        model,
        room,
        view,
        [left, y, right - left, by - 18.0 - y],
        actions,
    );
    room_buttons(p, model, room, view, [left, by], right, button_h, actions);
}

/// "3 telas · 5 na sala".
fn counts_text(room: &RoomState, screens: usize) -> String {
    let screens = match screens {
        0 => "nenhuma tela".to_owned(),
        1 => "1 tela".to_owned(),
        n => format!("{n} telas"),
    };
    let people = match room.in_room_count() {
        0 => "ninguém na sala".to_owned(),
        n => format!("{n} na sala"),
    };
    format!("{screens} · {people}")
}

/// "Transmitindo:" and a chip for each person sharing (a cyan dot, no
/// avatar); returns the bottom of the row.
fn sharing_chips(
    p: &Painter<'_>,
    screens: &[(String, String, Screen)],
    left: f32,
    right: f32,
    y: f32,
) -> f32 {
    let chip_h = 32.0;
    let label = "Transmitindo:";
    let ls = p.text_size(p.fonts.regular, 13.0, label);
    p.text(
        p.fonts.regular,
        13.0,
        [left, y + (chip_h - ls[1]) / 2.0],
        MUTED,
        label,
    );
    let mut x = left + ls[0] + 12.0;
    for (_, name, _) in screens {
        let ns = p.text_size(p.fonts.semibold, 13.0, name);
        let cw = 12.0 + 8.0 + 8.0 + ns[0] + 12.0;
        if x + cw > right {
            break;
        }
        p.rect(
            [x, y],
            [x + cw, y + chip_h],
            Some(CARD),
            Some((BORDER, 1.0)),
            chip_h / 2.0,
        );
        p.draw
            .add_circle([x + 12.0 + 4.0, y + chip_h / 2.0], 4.0, c(CYAN))
            .filled(true)
            .build();
        p.text(
            p.fonts.semibold,
            13.0,
            [x + 28.0, y + (chip_h - ns[1]) / 2.0],
            TEXT,
            name,
        );
        x += cw + 8.0;
    }
    y + chip_h
}

/// Width of the panel beside the screens inside a room.
const PANEL_W: f32 = 290.0;
/// Height of the room's top bar.
const ROOM_BAR_H: f32 = 68.0;

/// Inside the room: a top bar (name, live badge, counts, leave), the screens,
/// and a panel with two tabs: how to share, and who is here.
fn inside_room(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    origin: [f32; 2],
    [w, h]: [f32; 2],
    actions: &mut Vec<Action>,
) {
    let screens = room.live_screens();
    let (left, right) = (origin[0] + 24.0, w - 24.0);
    let mid = HEADER_MID;
    // The fold button and the name on the sidebar header's line.
    sidebar_toggle(p, view, [left + 17.0, mid]);
    let name_x = left + 34.0 + 14.0;
    let th = p.text_size(p.fonts.bold, 22.0, "Ag")[1];
    // The right side first: its width limits the name. At the very right,
    // the share panel's fold button (like the sidebar's at the left).
    let hidden = model.settings.share_panel_hidden;
    let tip = if hidden {
        "Mostrar o painel de transmissão"
    } else {
        "Esconder o painel de transmissão"
    };
    if p.round_button(
        "toggle-panel",
        [right - 17.0, mid],
        34.0,
        BG,
        Some(BORDER),
        |c| {
            p.icon(glyph::PANE_RIGHT, c, 16.0, TEXT);
        },
    ) {
        actions.push(Action::ToggleSharePanel);
    }
    if p.ui.is_item_hovered() {
        p.ui.tooltip_text(tip);
    }
    let online = room.connection == Connection::Online;
    let label = "Sair da sala";
    let bw = p.text_size(p.fonts.semibold, 14.0, label)[0] + 36.0;
    let bx = right - 34.0 - 12.0 - bw;
    if p.pill(
        "exit",
        [bx, mid - 19.0],
        [bw, 38.0],
        CARD,
        Some(BORDER),
        label,
        p.fonts.semibold,
        14.0,
        TEXT,
    ) && online
    {
        actions.push(Action::Exit(room.id.clone()));
    }
    // Sharing with the panel hidden: say so here (a click shows the panel,
    // where the share is switched or stopped).
    let sharing_here = model.sharing.as_ref().is_some_and(|s| s.room == room.id);
    let mut meta_right = bx - 16.0;
    if hidden && sharing_here {
        let text = "Transmitindo";
        let (size, pad) = (13.0, [10.0, 6.0]);
        let lw = pad[0] * 2.0 + 8.0 + 6.0 + p.text_size(p.fonts.bold, size, text)[0];
        let lh = p.badge_height(pad[1], size);
        let lx = meta_right - lw;
        let (clicked, hovered) = p.hit(
            "live-badge",
            [lx, mid - lh / 2.0],
            [lx + lw, mid + lh / 2.0],
        );
        let fill = if hovered { hover(CARD) } else { CARD };
        p.badge(
            [lx, mid - lh / 2.0],
            text,
            fill,
            TEXT,
            Some(LIVE_DOT),
            pad,
            size,
            Some(BORDER),
        );
        if clicked {
            actions.push(Action::ToggleSharePanel);
        }
        meta_right = lx - 16.0;
    }
    let meta = connection_text(room).unwrap_or_else(|| counts_text(room, screens.len()));
    let ms = p.text_size(p.fonts.regular, 14.0, &meta);
    let mx = meta_right - ms[0];
    p.text(p.fonts.regular, 14.0, [mx, mid - ms[1] / 2.0], MUTED, &meta);
    let name = p.fit(p.fonts.bold, 22.0, &room.name, mx - 24.0 - 90.0 - name_x);
    let ns = p.text_size(p.fonts.bold, 22.0, &name);
    p.text(p.fonts.bold, 22.0, [name_x, mid - th / 2.0], TEXT, &name);
    if !screens.is_empty() {
        let bh = p.badge_height(3.0, 12.0);
        p.badge(
            [name_x + ns[0] + 14.0, mid - bh / 2.0],
            "Ao vivo",
            CARD,
            TEXT,
            Some(LIVE_DOT),
            [9.0, 3.0],
            12.0,
            Some(BORDER),
        );
    }
    let bar_bottom = TITLE_H + ROOM_BAR_H;
    p.draw
        .add_line(
            [origin[0] + 1.0, bar_bottom - 0.5],
            [w, bar_bottom - 0.5],
            c(DIVIDER),
        )
        .thickness(1.0)
        .build();
    let top = bar_bottom + 20.0;
    let bottom = h - 24.0;
    // Hidden, the panel gives its width to the screens.
    let screens_right = if hidden {
        right
    } else {
        right - PANEL_W - 20.0
    };
    view.notice_span = Some([left, screens_right]);
    screens_area(
        p,
        model,
        room,
        view,
        [left, top, screens_right - left, bottom - top],
        actions,
    );
    if !hidden {
        room_panel(
            p,
            model,
            room,
            view,
            [right - PANEL_W, top, PANEL_W, bottom - top],
            actions,
        );
    }
}

/// Which tab the room panel shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PanelTab {
    #[default]
    Share,
    People,
}

fn room_panel(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    [x, y, w, h]: [f32; 4],
    actions: &mut Vec<Action>,
) {
    p.rect(
        [x, y],
        [x + w, y + h],
        Some(CARD),
        Some((BORDER, 1.0)),
        16.0,
    );
    let (left, right) = (x + 20.0, x + w - 20.0);
    let mut cy = y + 18.0;
    let people = format!("Pessoas ({})", room.members.len());
    let tabs = ["Transmissão", people.as_str()];
    let current = match view.panel_tab {
        PanelTab::Share => 0,
        PanelTab::People => 1,
    };
    if let Some(k) = p.segmented(
        "tabs",
        [left, cy, right - left, 42.0],
        &tabs,
        Some(current),
        false,
    ) {
        view.panel_tab = if k == 0 {
            PanelTab::Share
        } else {
            PanelTab::People
        };
    }
    cy += 42.0 + 18.0;
    let area = [left, cy, right - left, y + h - 20.0 - cy];
    match view.panel_tab {
        PanelTab::Share => share_tab(p, model, room, area, actions),
        PanelTab::People => people_tab(p, model, room, view, area),
    }
}

/// Manual controls for how this PC shares: a preset fills the fields, any
/// change to a field makes it "manual". Saved at once.
fn share_tab(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    [x, y, w, h]: [f32; 4],
    actions: &mut Vec<Action>,
) {
    let s = &model.settings;
    let mut next = s.clone();
    let caption = |cy: f32, text: &str| {
        p.text(p.fonts.semibold, 12.0, [x, cy], MUTED, text);
        cy + p.text_size(p.fonts.semibold, 12.0, "Ag")[1] + 8.0
    };
    // Preset ("Manual" only shows that no preset matches).
    let mut cy = caption(y, "Predefinição");
    let mut labels: Vec<&str> = settings::Preset::ALL.iter().map(|q| q.label()).collect();
    labels.push("Manual");
    let chosen = s
        .preset()
        .and_then(|preset| settings::Preset::ALL.iter().position(|q| *q == preset))
        .unwrap_or(labels.len() - 1);
    if let Some(k) = p.segmented("preset", [x, cy, w, 38.0], &labels, Some(chosen), false)
        && let Some(&preset) = settings::Preset::ALL.get(k)
    {
        next.apply(preset);
    }
    cy += 38.0 + 16.0;
    // Resolution.
    cy = caption(cy, "Resolução");
    let labels: Vec<&str> = settings::Resolution::ALL
        .iter()
        .map(|r| r.label())
        .collect();
    let chosen = settings::Resolution::ALL
        .iter()
        .position(|r| *r == s.resolution);
    if let Some(k) = p.segmented("resolution", [x, cy, w, 38.0], &labels, chosen, true) {
        next.resolution = settings::Resolution::ALL[k];
    }
    cy += 38.0 + 8.0;
    p.text(
        p.fonts.regular,
        11.0,
        [x, cy],
        MUTED,
        "Fonte = resolução nativa da tela ou janela",
    );
    cy += p.text_size(p.fonts.regular, 11.0, "Ag")[1] + 16.0;
    // Frame rate.
    cy = caption(cy, "Quadros por segundo");
    let fps: Vec<String> = settings::FPS_CHOICES.iter().map(u32::to_string).collect();
    let labels: Vec<&str> = fps.iter().map(String::as_str).collect();
    let chosen = settings::FPS_CHOICES
        .iter()
        .position(|&f| f == s.share_fps());
    if let Some(k) = p.segmented("fps", [x, cy, w, 38.0], &labels, chosen, true) {
        next.fps = settings::FPS_CHOICES[k];
    }
    cy += 38.0 + 16.0;
    // Bitrate.
    let kbps = s.bitrate_bps() / 1000;
    p.text(p.fonts.semibold, 12.0, [x, cy + 3.0], MUTED, "Bitrate");
    let value = format!("{} kbps", settings::thousands(kbps));
    let bh = p.badge_height(3.0, 12.0);
    let vw = p.text_size(p.fonts.bold, 12.0, &value)[0] + 20.0;
    p.badge(
        [x + w - vw, cy],
        &value,
        BG,
        TEXT,
        None,
        [10.0, 3.0],
        12.0,
        Some(BORDER),
    );
    cy += bh + 10.0;
    let (lo, hi) = settings::BITRATE_RANGE_KBPS;
    if let Some(v) = p.slider(
        "bitrate",
        [x, cy, w, 18.0],
        kbps,
        (lo, hi),
        settings::BITRATE_STEP_KBPS,
    ) {
        next.bitrate_kbps = v;
    }
    cy += 18.0 + 8.0;
    let lo_text = format!("{} kbps", settings::thousands(lo));
    let hi_text = format!("{} kbps", settings::thousands(hi));
    let hw = p.text_size(p.fonts.regular, 11.0, &hi_text)[0];
    p.text(p.fonts.regular, 11.0, [x, cy], MUTED, &lo_text);
    p.text(p.fonts.regular, 11.0, [x + w - hw, cy], MUTED, &hi_text);
    // The button at the bottom, a note above it.
    let button_h = 52.0;
    let by = y + h - button_h;
    let sharing_here = model.sharing.as_ref().is_some_and(|sh| sh.room == room.id);
    let note = if sharing_here {
        "Mudanças valem na próxima transmissão".to_owned()
    } else {
        // kbps / 8000 = MB/s.
        let mbs = format!("{:.2}", f64::from(kbps) / 8000.0).replace('.', ",");
        format!("Upload estimado: ~{mbs} MB/s")
    };
    let nh = p.text_size(p.fonts.regular, 12.0, &note)[1];
    p.text(p.fonts.regular, 12.0, [x, by - 12.0 - nh], MUTED, &note);
    let online = room.connection == Connection::Online;
    if sharing_here {
        // Another screen or window without ending the share (viewers stay),
        // and stopping, side by side.
        const GAP: f32 = 10.0;
        let switch_w = ((w - GAP) * 0.34).round();
        if p.pill(
            "share-switch",
            [x, by],
            [switch_w, button_h],
            CARD,
            Some(BORDER),
            "Trocar",
            p.fonts.bold,
            14.0,
            TEXT,
        ) && online
        {
            actions.push(Action::PickShare(room.id.clone()));
        }
        if p.pill(
            "share",
            [x + switch_w + GAP, by],
            [w - switch_w - GAP, button_h],
            CARD,
            Some(BORDER),
            "Parar transmissão",
            p.fonts.bold,
            14.0,
            DANGER,
        ) {
            actions.push(Action::StopShare);
        }
    } else if p.pill(
        "share",
        [x, by],
        [w, button_h],
        ACCENT,
        Some(BORDER),
        "Transmitir minha tela",
        p.fonts.bold,
        15.0,
        WHITE,
    ) && online
    {
        actions.push(Action::PickShare(room.id.clone()));
    }
    if next != *s {
        actions.push(Action::SaveSettings(Box::new(next)));
    }
}

/// Who belongs to the room: name, where they are, a cyan dot and a
/// "transmitindo" badge for whoever shares; the owner can remove the others.
fn people_tab(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    [x, y, w, h]: [f32; 4],
) {
    let i_own = room.role_of(&model.me) == Some(Role::Owner);
    let mut members: Vec<_> = room.members.iter().collect();
    members.sort_by_key(|m| {
        (
            m.key != model.me,
            !room.presence.contains_key(&m.key),
            m.joined_at,
        )
    });
    let row_h = 44.0;
    let mut cy = y;
    for member in members {
        if cy + row_h > y + h {
            break;
        }
        let you = member.key == model.me;
        let presence = room.presence.get(&member.key);
        let sharing = presence.is_some_and(|pr| pr.screen.is_some());
        let (dot, state) = match presence {
            Some(_) if sharing => (CYAN, "transmitindo"),
            Some(pr) if pr.entered => (ACCENT, "na sala"),
            Some(_) => (ONLINE_DOT, "online"),
            None => (EMPTY, "offline"),
        };
        let mid = cy + row_h / 2.0;
        p.draw
            .add_circle([x + 4.0, mid], 4.0, c(dot))
            .filled(true)
            .build();
        let mut name = if you {
            format!("{} (você)", member.name)
        } else {
            member.name.clone()
        };
        if member.role == Role::Owner {
            name.push_str(" · dono");
        }
        let removable = i_own && !you;
        let mut name_right = x + w - if removable { 30.0 } else { 0.0 };
        if sharing {
            let label = "transmitindo";
            let bh = p.badge_height(2.0, 11.0);
            let bwid = p.text_size(p.fonts.bold, 11.0, label)[0] + 16.0;
            p.badge(
                [name_right - bwid, mid - bh / 2.0],
                label,
                CYAN_BG,
                CYAN,
                None,
                [8.0, 2.0],
                11.0,
                None,
            );
            name_right -= bwid + 8.0;
        }
        let name = p.fit(p.fonts.semibold, 14.0, &name, name_right - (x + 16.0));
        let ns = p.text_size(p.fonts.semibold, 14.0, &name);
        let color = if presence.is_some() { TEXT } else { MUTED };
        p.text(
            p.fonts.semibold,
            14.0,
            [x + 16.0, mid - ns[1] / 2.0],
            color,
            &name,
        );
        p.ui.set_cursor_screen_pos([x, cy]);
        p.ui.invisible_button(format!("person-{}", member.key), [name_right - x, row_h]);
        if p.ui.is_item_hovered() {
            p.ui.tooltip_text(state);
        }
        if removable
            && p.round_button(
                &format!("remove-{}", member.key),
                [x + w - 12.0, mid],
                24.0,
                CARD,
                None,
                |center| {
                    p.icon(glyph::CLOSE, center, 9.0, MUTED);
                },
            )
        {
            view.open(Dialog::ConfirmRemove {
                key: member.key.clone(),
                name: member.name.clone(),
            });
        }
        cy += row_h;
    }
}

/// Height of the text strip under each screen (name, numbers, buttons):
/// drawn by us, since nothing can be drawn over a video window.
const STRIP_H: f32 = 36.0;

/// Where one screen goes: its picture and the strip under it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tile {
    /// x, y, width, height of the picture (16:9).
    video: [f32; 4],
}

impl Tile {
    fn strip_top(&self) -> f32 {
        self.video[1] + self.video[3]
    }

    fn right(&self) -> f32 {
        self.video[0] + self.video[2]
    }
}

/// Lays out `n` screens in `area` (x, y, width, height): a grid of up to
/// three per row, or with `focus` the focused one large and the others in a
/// column at the right. Pictures keep 16:9, each with its strip under it.
fn layout(n: usize, focus: Option<usize>, area: [f32; 4]) -> Vec<Tile> {
    const GAP: f32 = 16.0;
    let [x, y, w, h] = area;
    if n == 0 || w <= 0.0 || h <= 0.0 {
        return Vec::new();
    }
    // The largest 16:9 picture (plus strip) inside w x h.
    let fit = |w: f32, h: f32| -> (f32, f32) {
        let vw = w.min((h - STRIP_H).max(0.0) * 16.0 / 9.0).max(0.0);
        (vw.floor(), (vw * 9.0 / 16.0).floor())
    };
    if let Some(focus) = focus.filter(|&f| f < n && n > 1) {
        let side_w = ((w - GAP) * 0.24).floor();
        let (bw, bh) = fit(w - GAP - side_w, h);
        let main_x = x + ((w - GAP - side_w) - bw) / 2.0;
        let mut tiles = vec![Tile { video: [0.0; 4] }; n];
        tiles[focus] = Tile {
            video: [
                main_x.floor(),
                (y + (h - bh - STRIP_H) / 2.0).floor(),
                bw,
                bh,
            ],
        };
        let others = n - 1;
        let slot_h = ((h - GAP * (others - 1) as f32) / others as f32).floor();
        let (sw, sh) = fit(side_w, slot_h);
        let side_x = x + w - side_w + (side_w - sw) / 2.0;
        let mut slot = 0;
        for (i, tile) in tiles.iter_mut().enumerate() {
            if i == focus {
                continue;
            }
            let top = y + slot as f32 * (slot_h + GAP) + (slot_h - sh - STRIP_H) / 2.0;
            *tile = Tile {
                video: [side_x.floor(), top.floor(), sw, sh],
            };
            slot += 1;
        }
        return tiles;
    }
    let columns = n.min(3);
    let rows = n.div_ceil(columns);
    let cell_w = (w - GAP * (columns - 1) as f32) / columns as f32;
    let cell_h = (h - GAP * (rows - 1) as f32) / rows as f32;
    let (vw, vh) = fit(cell_w, cell_h);
    let block_h = rows as f32 * (vh + STRIP_H) + (rows - 1) as f32 * GAP;
    let top = y + ((h - block_h) / 2.0).max(0.0);
    (0..n)
        .map(|i| {
            let (col, row) = (i % columns, i / columns);
            // The last row's tiles are centered when it is not full.
            let in_row = if row == rows - 1 {
                n - row * columns
            } else {
                columns
            };
            let row_w = in_row as f32 * vw + (in_row - 1) as f32 * GAP;
            let left = x + (w - row_w) / 2.0;
            Tile {
                video: [
                    (left + col as f32 * (vw + GAP)).floor(),
                    (top + row as f32 * (vh + STRIP_H + GAP)).floor(),
                    vw,
                    vh,
                ],
            }
        })
        .collect()
}

/// "LAN", "internet direta" or "IPv6": which way a connection goes.
fn route_label(route: transport::Route) -> &'static str {
    match route {
        transport::Route::Lan => "LAN",
        transport::Route::Internet => "internet direta",
        transport::Route::Ipv6 => "IPv6",
    }
}

/// "58 fps · 6,1 Mbps · perda 0,4 % · 12 ms · LAN": what a watched screen
/// shows ("conectando…" until the media flows).
fn stats_text(stats: &crate::viewer::LiveStats) -> String {
    let Some(route) = stats.route else {
        return "conectando…".to_owned();
    };
    let mut parts = vec![
        format!("{:.0} fps", stats.fps),
        format!("{:.1} Mbps", stats.mbps).replace('.', ","),
    ];
    if let Some(loss) = stats.loss {
        parts.push(format!("perda {:.1}%", loss * 100.0).replace('.', ","));
    }
    if let Some(rtt) = stats.rtt {
        parts.push(format!("{} ms", rtt.as_millis().max(1)));
    }
    parts.push(route_label(route).to_owned());
    parts.join(" · ")
}

/// The live screens of the room: watched ones show their picture (a video
/// window the app puts on `view.video_rects`), the others a card with
/// "Assistir", ours who is watching it.
fn screens_area(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    area: [f32; 4],
    actions: &mut Vec<Action>,
) {
    let screens = room.live_screens();
    let [ax, ay, aw, ah] = area;
    if screens.is_empty() {
        let (min, max) = ([ax, ay], [ax + aw, ay + ah]);
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
    let focus = model
        .focus
        .as_deref()
        .and_then(|key| screens.iter().position(|(k, _, _)| k == key));
    let tiles = layout(screens.len(), focus, area);
    for ((key, who, screen), tile) in screens.iter().zip(&tiles) {
        let [vx, vy, vw, vh] = tile.video;
        let watch = model.watch_of(&room.id, key);
        let mine = *key == model.me;
        if let Some(watch) = watch.filter(|w| w.floating) {
            // Out in a floating window: say so here, and bring it back.
            p.rect(
                [vx, vy],
                [vx + vw, vy + vh],
                Some(PREVIEW),
                Some((BORDER, 1.0)),
                10.0,
            );
            let center = [vx + vw / 2.0, vy + vh / 2.0];
            let title = "Assistindo numa janela flutuante";
            let t = p.text_size(p.fonts.semibold, 15.0, title);
            p.text(
                p.fonts.semibold,
                15.0,
                [center[0] - t[0] / 2.0, center[1] - t[1] - 12.0],
                TEXT,
                title,
            );
            let label = "Trazer de volta";
            let bw = p.text_size(p.fonts.semibold, 13.0, label)[0] + 32.0;
            if p.pill(
                &format!("dock-{key}"),
                [center[0] - bw / 2.0, center[1] + 4.0],
                [bw, 32.0],
                ACCENT,
                None,
                label,
                p.fonts.semibold,
                13.0,
                WHITE,
            ) {
                actions.push(Action::Float(watch.key.clone()));
            }
        } else if let Some(watch) = watch {
            // Black under the picture (shows while the video starts).
            p.rect([vx, vy], [vx + vw, vy + vh], Some(0x000000), None, 0.0);
            view.video_rects.push((watch.id, tile.video));
            // Clicks on the picture reach us (the video window ignores
            // input): one toggles the focus, a double one the full screen.
            p.ui.set_cursor_screen_pos([vx, vy]);
            p.ui.invisible_button(format!("video-{key}"), [vw.max(1.0), vh.max(1.0)]);
            if p.ui.is_item_hovered()
                && p.ui
                    .is_mouse_double_clicked(dear_imgui_rs::MouseButton::Left)
            {
                actions.push(Action::Fullscreen(key.clone()));
            } else if p.ui.is_item_clicked() && screens.len() > 1 {
                actions.push(Action::Focus(key.clone()));
            }
        } else {
            // Someone else's screen: a click anywhere on it watches it, like
            // "Assistir" (ours is not clickable).
            let hovered = !mine && {
                let (clicked, hovered) =
                    p.hit(&format!("card-{key}"), [vx, vy], [vx + vw, vy + vh]);
                if clicked && room.connection == Connection::Online {
                    actions.push(Action::Watch {
                        room: room.id.clone(),
                        key: key.clone(),
                    });
                }
                hovered
            };
            let border = if hovered {
                (ACCENT, 1.5)
            } else {
                (BORDER, 1.0)
            };
            p.rect(
                [vx, vy],
                [vx + vw, vy + vh],
                Some(PREVIEW),
                Some(border),
                10.0,
            );
            let center = [vx + vw / 2.0, vy + vh / 2.0];
            if mine && model.own_preview {
                // Our live picture: a video window the app puts here.
                p.rect([vx, vy], [vx + vw, vy + vh], Some(0x000000), None, 0.0);
                view.own_preview_rect = Some([vx, vy, vw, vh]);
            } else if mine {
                let title = "Você está transmitindo";
                let t = p.text_size(p.fonts.semibold, 15.0, title);
                let lines: Vec<String> = if model.audience.is_empty() {
                    vec!["Ninguém assistindo ainda".to_owned()]
                } else {
                    model
                        .audience
                        .iter()
                        .map(|w| {
                            let loss = w.link.loss.map_or_else(
                                || "perda —".to_owned(),
                                |l| format!("perda {:.1}%", l * 100.0).replace('.', ","),
                            );
                            let rtt = w.link.rtt.map_or_else(String::new, |r| {
                                format!(" · {} ms", r.as_millis().max(1))
                            });
                            let route = w
                                .link
                                .route
                                .map_or(String::new(), |r| format!(" · {}", route_label(r)));
                            format!("{} · {loss}{rtt}{route}", w.name)
                        })
                        .collect()
                };
                let line_h = p.text_size(p.fonts.regular, 13.0, "Ag")[1] + 4.0;
                let top = center[1] - (t[1] + 10.0 + line_h * lines.len() as f32) / 2.0;
                p.text(
                    p.fonts.semibold,
                    15.0,
                    [center[0] - t[0] / 2.0, top],
                    TEXT,
                    title,
                );
                for (i, line) in lines.iter().enumerate() {
                    let line = p.fit(p.fonts.regular, 13.0, line, vw - 24.0);
                    let lw = p.text_size(p.fonts.regular, 13.0, &line)[0];
                    p.text(
                        p.fonts.regular,
                        13.0,
                        [center[0] - lw / 2.0, top + t[1] + 10.0 + i as f32 * line_h],
                        MUTED,
                        &line,
                    );
                }
            } else if let Some(image) = room.previews.get(key) {
                // The latest still picture, fitted without stretching.
                let scale = (vw / image.width as f32).min(vh / image.height as f32);
                let (iw, ih) = (image.width as f32 * scale, image.height as f32 * scale);
                let min = [
                    (center[0] - iw / 2.0).round(),
                    (center[1] - ih / 2.0).round(),
                ];
                p.draw.add_image_rounded(
                    dear_imgui_rs::TextureId::new(image.texture),
                    min,
                    [min[0] + iw.round(), min[1] + ih.round()],
                    [0.0, 0.0],
                    [1.0, 1.0],
                    c(WHITE),
                    10.0,
                    dear_imgui_rs::DrawFlags::ROUND_ALL,
                );
            } else {
                p.icon(glyph::MONITOR, center, 32.0, MUTED);
            }
        }
        screen_strip(
            p, model, room, view, key, who, screen, tile, watch, mine, actions,
        );
    }
}

/// "Volume", the slider and the percent, right-aligned in `[left, right]`
/// of a strip whose middle is `mid`; the value is saved when let go.
fn volume_slider(
    p: &Painter<'_>,
    key: &str,
    percent: u8,
    [left, mid, right]: [f32; 3],
    actions: &mut Vec<Action>,
) {
    let value = format!("{percent}%");
    // Room for "100%", so the slider does not move as the number changes.
    let vw = p.text_size(p.fonts.semibold, 13.0, "100%")[0];
    let label = "Volume";
    let lw = p.text_size(p.fonts.regular, 12.0, label)[0];
    let sw = (right - left - lw - vw - 28.0).clamp(40.0, 180.0);
    let sx = right - vw - 10.0 - sw;
    let th = p.text_size(p.fonts.regular, 12.0, label)[1];
    if sx - 10.0 - lw >= left {
        p.text(
            p.fonts.regular,
            12.0,
            [sx - 14.0 - lw, mid - th / 2.0],
            MUTED,
            label,
        );
    }
    let changed = p.slider(
        &format!("volume-{key}"),
        [sx, mid - 9.0, sw, 18.0],
        u32::from(percent),
        (0, 100),
        1,
    );
    let let_go = p.ui.is_item_deactivated();
    let shown = changed.map_or(percent, |v| v as u8);
    let value = if changed.is_some() {
        format!("{shown}%")
    } else {
        value
    };
    let vh = p.text_size(p.fonts.semibold, 13.0, &value)[1];
    p.text(
        p.fonts.semibold,
        13.0,
        [right - vw, mid - vh / 2.0],
        TEXT,
        &value,
    );
    if changed.is_some() || let_go {
        actions.push(Action::Volume {
            key: key.to_owned(),
            percent: shown,
            done: let_go,
        });
    }
}

/// The strip under a screen: who and what, the numbers, the buttons.
#[allow(clippy::too_many_arguments)]
fn screen_strip(
    p: &Painter<'_>,
    model: &Model,
    room: &RoomState,
    view: &mut ViewState,
    key: &str,
    who: &str,
    screen: &rooms::Screen,
    tile: &Tile,
    watch: Option<&super::model::LocalWatch>,
    mine: bool,
    actions: &mut Vec<Action>,
) {
    let (x, y, right) = (tile.video[0], tile.strip_top(), tile.right());
    let mid = y + STRIP_H / 2.0;
    // Buttons from the right.
    let mut bx = right;
    let mut icon_button = |id: &str, glyph: &str, tip: &str| -> bool {
        bx -= 28.0;
        let clicked = p.round_button(
            &format!("{id}-{key}"),
            [bx + 14.0, mid],
            26.0,
            CARD,
            None,
            |center| p.icon(glyph, center, 12.0, TEXT),
        );
        if p.ui.is_item_hovered() {
            p.ui.tooltip_text(tip);
        }
        clicked
    };
    if let Some(watch) = watch {
        if icon_button("stop-watch", glyph::CLOSE, "Parar de assistir") {
            actions.push(Action::StopWatch(key.to_owned()));
        }
        let (mute_glyph, mute_tip) = if watch.muted {
            (glyph::MUTED, "Ligar o som")
        } else {
            (glyph::VOLUME, "Silenciar")
        };
        let mute_tip = format!("{mute_tip} · volume {}%", watch.volume);
        if icon_button("mute", mute_glyph, &mute_tip) {
            actions.push(Action::Mute(key.to_owned()));
        }
        // Hovering the speaker opens the volume slider in this strip (the
        // video window would cover anything drawn over the picture); its
        // wheel changes the volume in steps of 5%.
        if p.ui.is_item_hovered() {
            view.volume_open = Some(key.to_owned());
            let wheel = p.ui.io().mouse_wheel();
            if wheel != 0.0 {
                let step = if wheel > 0.0 { 5 } else { -5 };
                let next = (i32::from(watch.volume) + step).clamp(0, 100) as u8;
                if next != watch.volume {
                    actions.push(Action::Volume {
                        key: key.to_owned(),
                        percent: next,
                        done: true,
                    });
                }
            }
        }
        let float_tip = if watch.floating {
            "Trazer de volta para o app"
        } else {
            "Janela flutuante (por cima de tudo; dois cliques nela trazem de volta)"
        };
        if icon_button("float", glyph::FLOAT, float_tip) {
            actions.push(Action::Float(key.to_owned()));
        }
        if icon_button("fullscreen", glyph::FULLSCREEN, "Tela cheia (Esc sai)") {
            actions.push(Action::Fullscreen(key.to_owned()));
        }
        let metrics_tip = if watch.metrics {
            "Esconder as métricas"
        } else {
            "Métricas da transmissão"
        };
        if icon_button("metrics", glyph::MORE, metrics_tip) {
            actions.push(Action::Metrics(key.to_owned()));
        }
        if view.volume_open.as_deref() == Some(key) {
            // Open while the mouse is on the strip (or still dragging).
            let mouse = p.ui.io().mouse_pos();
            let on_strip =
                mouse[0] >= x && mouse[0] <= right && mouse[1] >= y && mouse[1] <= y + STRIP_H;
            let dragging = p.ui.is_mouse_down(dear_imgui_rs::MouseButton::Left);
            if on_strip || dragging {
                volume_slider(p, key, watch.volume, [x + 2.0, mid, bx - 8.0], actions);
                return;
            }
            view.volume_open = None;
        }
    } else if !mine {
        let label = "Assistir";
        let w = p.text_size(p.fonts.semibold, 13.0, label)[0] + 28.0;
        bx -= w;
        if p.pill(
            &format!("watch-{key}"),
            [bx, y + 4.0],
            [w, STRIP_H - 8.0],
            ACCENT,
            None,
            label,
            p.fonts.semibold,
            13.0,
            WHITE,
        ) && room.connection == Connection::Online
        {
            actions.push(Action::Watch {
                room: room.id.clone(),
                key: key.to_owned(),
            });
        }
    } else {
        // Ours: the live picture of what is shared, on or off.
        let tip = if model.settings.own_preview {
            "Esconder a prévia da sua transmissão"
        } else {
            "Ver a prévia da sua transmissão"
        };
        if icon_button("own-preview", glyph::VIEW, tip) {
            actions.push(Action::ToggleOwnPreview);
        }
    }
    let label = if mine {
        format!("Você · {}", screen.info.name)
    } else {
        format!("{who} · {}", screen.info.name)
    };
    // The numbers, or a shorter form when the strip is narrow; the name
    // keeps what fits beside them (or the whole strip without them).
    let stats = watch.and_then(|w| model.watch_stats.get(&w.key));
    let details = [
        match stats {
            Some(stats) => stats_text(stats),
            None => format!(
                "{}×{} · {} fps",
                screen.info.width, screen.info.height, screen.info.fps
            ),
        },
        stats.map_or_else(String::new, stats_short),
    ];
    let room_left = bx - 8.0 - x;
    let min_label = p.text_size(p.fonts.semibold, 13.0, &label)[0].min(90.0);
    let shown = details
        .iter()
        .filter(|d| !d.is_empty())
        .find(|d| min_label + 12.0 + p.text_size(p.fonts.regular, 12.0, d)[0] <= room_left);
    let dw = shown.map_or(0.0, |d| p.text_size(p.fonts.regular, 12.0, d)[0] + 12.0);
    let label = p.fit(p.fonts.semibold, 13.0, &label, room_left - dw);
    let lh = p.text_size(p.fonts.semibold, 13.0, &label);
    p.text(
        p.fonts.semibold,
        13.0,
        [x + 2.0, mid - lh[1] / 2.0],
        TEXT,
        &label,
    );
    if let Some(details) = shown {
        let dh = p.text_size(p.fonts.regular, 12.0, details)[1];
        p.text(
            p.fonts.regular,
            12.0,
            [x + 2.0 + lh[0] + 12.0, mid - dh / 2.0],
            MUTED,
            details,
        );
    }
}

/// "58 fps · 0,4% · 12 ms": the numbers for a narrow strip.
fn stats_short(stats: &crate::viewer::LiveStats) -> String {
    if stats.route.is_none() {
        return "conectando…".to_owned();
    }
    let mut parts = vec![format!("{:.0} fps", stats.fps)];
    if let Some(loss) = stats.loss {
        parts.push(format!("{:.1}%", loss * 100.0).replace('.', ","));
    }
    if let Some(rtt) = stats.rtt {
        parts.push(format!("{} ms", rtt.as_millis().max(1)));
    }
    parts.join(" · ")
}

/// Full screen: one watched picture fills the window; Esc or a double
/// click goes back.
fn fullscreen_view(
    p: &Painter<'_>,
    model: &Model,
    view: &mut ViewState,
    key: &str,
    size: [f32; 2],
    actions: &mut Vec<Action>,
) {
    p.rect([0.0, 0.0], size, Some(0x000000), None, 0.0);
    if let Some(watch) = model.watching.iter().find(|w| w.key == key) {
        view.video_rects
            .push((watch.id, [0.0, 0.0, size[0], size[1]]));
    }
    p.ui.set_cursor_screen_pos([0.0, 0.0]);
    p.ui.invisible_button("fullscreen-video", size);
    if (p.ui.is_item_hovered()
        && p.ui
            .is_mouse_double_clicked(dear_imgui_rs::MouseButton::Left))
        || p.ui.is_key_pressed(dear_imgui_rs::Key::Escape)
    {
        actions.push(Action::Fullscreen(key.to_owned()));
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
    // The room's preview: sharing and leaving it are inside the room.
    if button(
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

/// The last notice, centered on `[left, right]`, near the bottom of a
/// window `h` tall.
fn notice(p: &Painter<'_>, view: &mut ViewState, [left, right]: [f32; 2], h: f32) {
    let Some((text, at)) = &view.notice else {
        return;
    };
    let age = at.elapsed().as_secs_f32();
    if age > notice_secs(text) {
        view.notice = None;
        return;
    }
    // Long notices wrap inside the span (at most 640 px wide).
    let max_w = (right - left - 120.0).clamp(200.0, 640.0);
    let lines = wrap(p, p.fonts.semibold, 14.0, text, max_w);
    let line_h = p.text_size(p.fonts.semibold, 14.0, "Ag")[1] + 3.0;
    let text_w = lines
        .iter()
        .map(|l| p.text_size(p.fonts.semibold, 14.0, l)[0])
        .fold(0.0, f32::max);
    let (bw, bh) = (text_w + 40.0, line_h * lines.len() as f32 - 3.0 + 22.0);
    let x = ((left + right - bw) / 2.0).round();
    let y = h - 32.0 - 48.0 - 18.0 - bh;
    p.rect(
        [x, y],
        [x + bw, y + bh],
        Some(ACTIVE),
        Some((ACTIVE_BORDER, 1.0)),
        (bh / 2.0).min(20.0),
    );
    for (i, line) in lines.iter().enumerate() {
        let lw = p.text_size(p.fonts.semibold, 14.0, line)[0];
        p.text(
            p.fonts.semibold,
            14.0,
            [x + (bw - lw) / 2.0, y + 11.0 + i as f32 * line_h],
            TEXT,
            line,
        );
    }
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
            // Every frame, not only when it appears: the popup sizes itself
            // to its content, and on the frame it appears ImGui still has
            // the previous dialog's size, so it would stay off center (and
            // the window can be resized while it is open).
            dear_imgui_sys::ImGuiCond_Always,
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
            ui.text_disabled("Cole o convite que recebeu (começa com nitraroom:).");
            ui.set_next_item_width(440.0);
            let enter = ui
                .input_text("##invite", &mut view.invite_text)
                .hint("nitraroom:…")
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
        Dialog::Welcome => {
            // No way around it: Esc does not close this one.
            close = false;
            heading(ui, fonts, "Qual seu nome?");
            ui.text_disabled(
                "Esse nome aparece nas salas. Dá para mudar depois nas configurações.",
            );
            ui.set_next_item_width(400.0);
            let enter = ui
                .input_text("##welcome-name", &mut view.name_text)
                .hint("Seu nome")
                .enter_returns_true(true)
                .build();
            let name = settings::clean_name(&view.name_text);
            let disabled = ui.begin_disabled_with_cond(name.is_none());
            if (ui.button("Continuar") || enter)
                && let Some(name) = name
            {
                actions.push(Action::SaveSettings(Box::new(Settings {
                    name: Some(name),
                    ..view.draft.clone()
                })));
                close = true;
            }
            drop(disabled);
        }
        Dialog::Settings => {
            heading(ui, fonts, "Configurações");
            ui.text_disabled(format!(
                "nitra {}{}",
                env!("CARGO_PKG_VERSION"),
                if crate::BETA {
                    ""
                } else {
                    " · desenvolvimento"
                }
            ));
            if let Some(gpu) = &model.gpu {
                ui.text_disabled(format!(
                    "Placa de vídeo: {} · {}",
                    gpu.name,
                    gpu.support.label()
                ));
            }
            ui.dummy([0.0, 6.0]);
            // Two columns side by side: the window is wider than tall.
            const COLUMN: f32 = 380.0;
            let [left, top] = ui.cursor_screen_pos();
            ui.group(|| {
                ui.text("Seu nome nas salas");
                ui.set_next_item_width(COLUMN);
                ui.input_text("##settings-name", &mut view.name_text)
                    .build();
                ui.dummy([0.0, 8.0]);
                ui.text_disabled("A qualidade da transmissão fica no painel da sala.");
                ui.dummy([0.0, 8.0]);
                ui.text("Ao assistir");
                let labels: Vec<&str> = Fluidity::ALL.iter().map(|f| f.label()).collect();
                let mut index = Fluidity::ALL
                    .iter()
                    .position(|&f| f == view.draft.fluidity)
                    .unwrap_or(0);
                ui.set_next_item_width(COLUMN);
                if ui.combo_simple_string("##fluidity", &mut index, &labels) {
                    view.draft.fluidity = Fluidity::ALL[index];
                }
                // Two lines: one would run into the right column.
                ui.text_disabled("Segurar um pouco os quadros deixa a imagem");
                ui.text_disabled("mais fluida, com um pouco mais de atraso.");
            });
            let left_bottom = ui.item_rect_max()[1];
            // The right column starts at the same height (placed directly:
            // "same line" would push its first text down to the fields'
            // baseline).
            ui.set_cursor_screen_pos([left + COLUMN + 32.0, top]);
            ui.group(|| {
                ui.text("Prévia da sua tela para quem não assiste");
                let labels: Vec<String> = PREVIEW_CHOICES
                    .iter()
                    .map(|&minutes| match minutes {
                        0 => "Desligada".to_owned(),
                        1 => "A cada 1 minuto".to_owned(),
                        m => format!("A cada {m} minutos"),
                    })
                    .collect();
                let mut index = PREVIEW_CHOICES
                    .iter()
                    .position(|&m| m == view.draft.preview_minutes)
                    .unwrap_or(1);
                ui.set_next_item_width(COLUMN);
                if ui.combo_simple_string("##preview", &mut index, &labels) {
                    view.draft.preview_minutes = PREVIEW_CHOICES[index];
                }
                ui.dummy([0.0, 8.0]);
                ui.text("Som da tela inteira: deixar de fora também");
                ui.text_disabled("Discord e nitra já ficam de fora. Separe por vírgulas.");
                ui.set_next_item_width(COLUMN);
                ui.input_text("##apps", &mut view.apps_text)
                    .hint("TeamSpeak.exe, Mumble.exe")
                    .build();
                ui.dummy([0.0, 8.0]);
                ui.checkbox(
                    "Fechar no X deixa o nitra na bandeja",
                    &mut view.draft.close_to_tray,
                );
                ui.text_disabled("Clicar no ícone da bandeja esconde ou mostra a janela.");
                ui.dummy([0.0, 8.0]);
                ui.text("Algo deu errado?");
                if ui.button("Abrir pasta de logs") {
                    actions.push(Action::OpenLogs);
                }
            });
            // Go on below the taller column.
            let bottom = left_bottom.max(ui.item_rect_max()[1]);
            ui.set_cursor_screen_pos([left, bottom + 12.0]);
            ui.separator();
            let name = settings::clean_name(&view.name_text);
            let disabled = ui.begin_disabled_with_cond(name.is_none());
            if ui.button("Salvar") {
                actions.push(Action::SaveSettings(Box::new(Settings {
                    name,
                    audio_exclude: settings::parse_app_list(&view.apps_text),
                    ..view.draft.clone()
                })));
                close = true;
            }
            drop(disabled);
            ui.same_line();
            if ui.button("Cancelar") {
                close = true;
            }
            // Apart, at the right: it cannot be undone.
            let label = "Apagar meus dados deste PC";
            let width = ui.calc_text_size(label)[0] + ui.clone_style().frame_padding()[0] * 2.0;
            // Placed in screen coordinates, like the columns ("same line with
            // pos" takes the window's own, and a button past the edge would
            // widen the popup a little more every frame).
            ui.same_line();
            let y = ui.cursor_screen_pos()[1];
            ui.set_cursor_screen_pos([left + 2.0 * COLUMN + 32.0 - width, y]);
            let red = ui.push_style_color(dear_imgui_rs::StyleColor::Text, rgba(DANGER));
            if ui.button(label) {
                // Another dialog takes this one's place (one at a time).
                ui.close_current_popup();
                view.open(Dialog::ConfirmErase);
            }
            drop(red);
        }
        Dialog::ConfirmErase => {
            heading(ui, fonts, "Apagar seus dados deste PC?");
            ui.text_disabled("Apaga suas salas, configurações, logs e a chave deste PC.");
            ui.text_disabled(
                "Para voltar às salas, você vai precisar de convites novos. O nitra fecha em seguida.",
            );
            ui.text_disabled("Depois, é só apagar o nitra.exe para remover o app.");
            let red = ui.push_style_color(dear_imgui_rs::StyleColor::Text, rgba(DANGER));
            if ui.button("Apagar e fechar") {
                actions.push(Action::EraseData);
                close = true;
            }
            drop(red);
            ui.same_line();
            if ui.button("Cancelar") {
                close = true;
            }
        }
        Dialog::Share { room } => {
            heading(ui, fonts, "O que você quer transmitir?");
            ui.text_disabled(format!(
                "{} (muda no painel da sala)",
                model.settings.share_label()
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
        Dialog::ConfirmUntestedGpu { room, source } => {
            heading(ui, fonts, "Transmitir numa placa ainda não testada?");
            let name = model.gpu.as_ref().map_or("?", |g| g.name.as_str());
            ui.text(format!(
                "Sua placa de vídeo ({name}) ainda não foi testada com o nitra."
            ));
            ui.text_disabled(
                "A transmissão usa o codificador de vídeo que o driver dela instala no Windows.",
            );
            ui.text_disabled("Deve funcionar, mas pode falhar ou pesar mais que o esperado.");
            ui.text_disabled("Assistir às telas dos outros funciona como sempre.");
            ui.dummy([0.0, 4.0]);
            ui.checkbox("Não perguntar de novo", &mut view.remember_gpu);
            ui.separator();
            if ui.button("Transmitir mesmo assim") {
                actions.push(Action::ShareAnyway {
                    room,
                    source,
                    remember: view.remember_gpu,
                });
                close = true;
            }
            ui.same_line();
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

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: [f32; 4] = [100.0, 50.0, 1200.0, 700.0];

    fn inside(tile: &Tile) -> bool {
        let [x, y, w, h] = tile.video;
        x >= AREA[0]
            && y >= AREA[1]
            && x + w <= AREA[0] + AREA[2] + 0.5
            && y + h + STRIP_H <= AREA[1] + AREA[3] + 0.5
    }

    fn overlap(a: &Tile, b: &Tile) -> bool {
        let ([ax, ay, aw, ah], [bx, by, bw, bh]) = (a.video, b.video);
        let (ah, bh) = (ah + STRIP_H, bh + STRIP_H);
        ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
    }

    fn check(tiles: &[Tile]) {
        for (i, tile) in tiles.iter().enumerate() {
            assert!(inside(tile), "tile {i} outside: {tile:?}");
            let ratio = tile.video[2] / tile.video[3];
            assert!(
                (ratio - 16.0 / 9.0).abs() < 0.02,
                "tile {i} not 16:9: {tile:?}"
            );
            for other in &tiles[i + 1..] {
                assert!(!overlap(tile, other), "{tile:?} overlaps {other:?}");
            }
        }
    }

    #[test]
    fn grids_fit_without_overlap() {
        for n in 1..=7 {
            let tiles = layout(n, None, AREA);
            assert_eq!(tiles.len(), n);
            check(&tiles);
        }
        // One screen uses the height it can.
        let one = layout(1, None, AREA)[0];
        assert!(one.video[3] + STRIP_H >= AREA[3] - 1.0, "{one:?}");
    }

    #[test]
    fn focus_makes_one_large_and_the_others_a_column() {
        let tiles = layout(3, Some(1), AREA);
        check(&tiles);
        assert!(tiles[1].video[2] > tiles[0].video[2] * 2.0, "{tiles:?}");
        assert_eq!(tiles[0].video[0], tiles[2].video[0]);
        assert!(tiles[0].video[0] > tiles[1].right());
        // A lone screen has nothing to focus against: plain layout.
        assert_eq!(layout(1, Some(0), AREA), layout(1, None, AREA));
    }

    #[test]
    fn nothing_to_lay_out() {
        assert!(layout(0, None, AREA).is_empty());
        assert!(layout(2, None, [0.0, 0.0, 0.0, 300.0]).is_empty());
    }

    #[test]
    fn stats_read_like_the_design() {
        let stats = crate::viewer::LiveStats {
            fps: 59.6,
            mbps: 6.14,
            loss: Some(0.004),
            rtt: Some(std::time::Duration::from_millis(12)),
            route: Some(transport::Route::Internet),
            ..Default::default()
        };
        assert_eq!(
            stats_text(&stats),
            "60 fps · 6,1 Mbps · perda 0,4% · 12 ms · internet direta"
        );
        assert_eq!(stats_short(&stats), "60 fps · 0,4% · 12 ms");
        let early = crate::viewer::LiveStats::default();
        assert_eq!(stats_text(&early), "conectando…");
        assert_eq!(stats_short(&early), "conectando…");
    }
}
