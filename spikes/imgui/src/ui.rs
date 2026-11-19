//! The prototype's screens: member list with actions, settings, and the
//! overlay on top of the video while watching.

use dear_imgui_rs::{Condition, Key, Ui, WindowFlags};
use feed::sample::{ENCODERS, FRAME_RATES, MEMBERS, Presence, RESOLUTIONS, Settings};

pub enum Action {
    None,
    Watch(usize),
    Leave,
    ToggleFullscreen,
}

#[derive(Default)]
pub struct State {
    pub selected: usize,
    pub streaming: bool,
    pub show_settings: bool,
    pub settings: Settings,
}

/// What the overlay shows while watching.
pub struct WatchInfo {
    pub member: usize,
    pub fps: f64,
    pub decoded: u64,
    pub dropped: u64,
}

fn rgba([r, g, b]: [f32; 3]) -> [f32; 4] {
    [r, g, b, 1.0]
}

pub fn build(ui: &Ui, state: &mut State, watching: Option<&WatchInfo>) -> Action {
    if ui.is_key_pressed(Key::F11) {
        return Action::ToggleFullscreen;
    }
    match watching {
        Some(info) => overlay(ui, info),
        None => {
            let action = main_screen(ui, state);
            if state.show_settings {
                settings(ui, state);
            }
            action
        }
    }
}

fn main_screen(ui: &Ui, state: &mut State) -> Action {
    let mut action = Action::None;
    let size = ui.io().display_size();
    ui.window("main")
        .position([0.0, 0.0], Condition::Always)
        .size(size, Condition::Always)
        .flags(
            WindowFlags::NO_DECORATION
                | WindowFlags::NO_MOVE
                | WindowFlags::NO_SAVED_SETTINGS
                | WindowFlags::NO_BRING_TO_FRONT_ON_FOCUS,
        )
        .build(|| {
            // Top bar.
            ui.text("ziz");
            ui.same_line();
            ui.text_disabled("Go Live prototype (Dear ImGui)");
            ui.same_line();
            let label = if state.streaming {
                "Stop streaming"
            } else {
                "Go live"
            };
            if ui.button(label) {
                state.streaming = !state.streaming;
            }
            ui.same_line();
            if ui.button("Settings") {
                state.show_settings = !state.show_settings;
            }
            if state.streaming {
                ui.same_line();
                ui.text_colored(rgba(Presence::Live.color()), "You are live");
            }
            ui.separator();

            // Member list.
            ui.child_window("members")
                .size([300.0, 0.0])
                .border(true)
                .build(ui, || {
                    ui.text_disabled(format!("MEMBERS - {}", MEMBERS.len()));
                    for (index, member) in MEMBERS.iter().enumerate() {
                        ui.text_colored(rgba(member.presence.color()), "*");
                        ui.same_line();
                        let row = format!("{:<12} {}", member.name, member.presence.label());
                        if ui
                            .selectable_config(&row)
                            .selected(state.selected == index)
                            .build()
                        {
                            state.selected = index;
                        }
                    }
                });
            ui.same_line();

            // Selected member.
            ui.child_window("details").build(ui, || {
                let member = &MEMBERS[state.selected];
                ui.text(member.name);
                ui.text_colored(rgba(member.presence.color()), member.presence.label());
                if !member.activity.is_empty() {
                    ui.text_disabled(member.activity);
                }
                ui.spacing();
                if member.presence == Presence::Live {
                    if ui.button_with_size("Watch stream", [180.0, 36.0]) {
                        action = Action::Watch(state.selected);
                    }
                } else {
                    ui.text_disabled("Not streaming.");
                }
            });
        });
    action
}

fn settings(ui: &Ui, state: &mut State) {
    let mut open = state.show_settings;
    ui.window("Settings")
        .size([420.0, 260.0], Condition::FirstUseEver)
        .position([340.0, 80.0], Condition::FirstUseEver)
        .opened(&mut open)
        .flags(WindowFlags::NO_SAVED_SETTINGS)
        .build(|| {
            let s = &mut state.settings;
            ui.combo_simple_string("Resolution", &mut s.resolution, &RESOLUTIONS);
            ui.combo_simple_string("Frame rate", &mut s.frame_rate, &FRAME_RATES);
            ui.combo_simple_string("Encoder", &mut s.encoder, &ENCODERS);
            ui.slider_i32("Bitrate (kbps)", &mut s.bitrate_kbps, 2_000, 20_000);
            ui.checkbox("Show cursor", &mut s.show_cursor);
            ui.checkbox("Start with Windows", &mut s.start_with_windows);
        });
    state.show_settings = open;
}

fn overlay(ui: &Ui, info: &WatchInfo) -> Action {
    let mut action = Action::None;
    if ui.is_key_pressed(Key::Escape) {
        return Action::Leave;
    }
    ui.window("overlay")
        .position([12.0, 12.0], Condition::Always)
        .bg_alpha(0.6)
        .flags(
            WindowFlags::NO_DECORATION
                | WindowFlags::ALWAYS_AUTO_RESIZE
                | WindowFlags::NO_SAVED_SETTINGS
                | WindowFlags::NO_MOVE,
        )
        .build(|| {
            ui.text(format!("Watching {}", MEMBERS[info.member].name));
            ui.text_disabled(format!(
                "{:.1} fps shown, {} decoded, {} dropped",
                info.fps, info.decoded, info.dropped
            ));
            if ui.button("Leave (Esc)") {
                action = Action::Leave;
            }
            ui.same_line();
            if ui.button("Fullscreen (F11)") {
                action = Action::ToggleFullscreen;
            }
        });
    action
}
