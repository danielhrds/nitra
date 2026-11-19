//! GPUI prototype: member list, go live / watch, settings,
//! and the 1080p60 hardware-decoded video in a `VideoPresenter` on its own
//! thread, either embedded in this window (a child window over the
//! layout's video area) or popped out in a window of its own.
//!
//! GPUI core has no ready-made widgets (combo box, slider, checkbox): the
//! settings screen uses plain clickable elements.
//!
//! Flags: `--watch` (start watching, embedded), `--popout` (with `--watch`:
//! in its own window), `--seconds N` (quit after N s). Prints start-up time
//! and the player's counters on exit.

mod player;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use feed::sample::{ENCODERS, FRAME_RATES, MEMBERS, Presence, RESOLUTIONS, Settings};
use gpui::{
    App, Application, Bounds, Context, Div, Hsla, IntoElement, Pixels, SharedString, Stateful,
    Window, WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb, size,
};
use player::{Mode, Player};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

struct Args {
    watch: bool,
    popout: bool,
    minimized: bool,
    seconds: Option<f64>,
}

fn parse_args() -> Args {
    let mut args = Args {
        watch: false,
        popout: false,
        minimized: false,
        seconds: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--watch" => args.watch = true,
            "--popout" => args.popout = true,
            "--minimized" => args.minimized = true,
            "--seconds" => args.seconds = iter.next().and_then(|v| v.parse().ok()),
            other => eprintln!("unknown argument {other}"),
        }
    }
    args
}

fn color([r, g, b]: [f32; 3]) -> Hsla {
    gpui::Rgba { r, g, b, a: 1.0 }.into()
}

/// x, y, width, height in physical pixels.
type Rect = (i32, i32, i32, i32);

struct Watching {
    member: usize,
    player: Player,
    /// Last video area sent to the player (physical pixels).
    sent: Rc<Cell<Option<Rect>>>,
}

struct Root {
    hwnd: isize,
    selected: usize,
    streaming: bool,
    show_settings: bool,
    settings: Settings,
    watching: Option<Watching>,
    summaries: Vec<String>,
    /// Bumped on every new player (ends the previous player's watcher).
    generation: u64,
}

impl Root {
    fn watch(&mut self, member: usize, mode: Mode, cx: &mut Context<Self>) {
        self.leave();
        self.watching = Some(Watching {
            member,
            player: Player::start(mode, self.hwnd),
            sent: Rc::new(Cell::new(None)),
        });
        self.generation += 1;
        let generation = self.generation;
        // While this player lives, notice when it ends on its own (pop-out
        // window closed by the user). No timer runs when nothing plays.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let current = this.update(cx, |root, cx| {
                    root.check_player(cx);
                    root.generation == generation && root.watching.is_some()
                });
                if !current.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn leave(&mut self) {
        if let Some(watching) = self.watching.take() {
            match watching.player.stop() {
                Ok(summary) => self.summaries.push(summary),
                Err(error) => self.summaries.push(format!("player error: {error}")),
            }
        }
    }

    /// Ends watching if the player stopped on its own (pop-out closed).
    fn check_player(&mut self, cx: &mut Context<Self>) {
        if self
            .watching
            .as_ref()
            .is_some_and(|watching| watching.player.has_ended())
        {
            self.leave();
            cx.notify();
        }
    }
}

fn button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(0x3b3f8f))
        .hover(|style| style.bg(rgb(0x4b50b5)))
        .active(|style| style.bg(rgb(0x2c2f6b)))
        .cursor_pointer()
        .text_color(rgb(0xffffff))
        .child(label.into())
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let top_bar = div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .bg(rgb(0x1b1c22))
            .child(div().text_color(rgb(0xffffff)).child("ziz"))
            .child(
                div()
                    .text_color(rgb(0x8a8d99))
                    .child("Go Live prototype (GPUI)"),
            )
            .child(
                button(
                    "go-live",
                    if self.streaming {
                        "Stop streaming"
                    } else {
                        "Go live"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.streaming = !this.streaming;
                    cx.notify();
                })),
            )
            .child(
                button("settings", "Settings").on_click(cx.listener(|this, _, _, cx| {
                    this.show_settings = !this.show_settings;
                    cx.notify();
                })),
            )
            .when(self.streaming, |bar| {
                bar.child(
                    div()
                        .text_color(color(Presence::Live.color()))
                        .child("You are live"),
                )
            });

        let members = div()
            .id("members")
            .w(px(300.0))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(0x202128))
            .p_2()
            .child(
                div()
                    .text_color(rgb(0x8a8d99))
                    .text_sm()
                    .child(format!("MEMBERS - {}", MEMBERS.len())),
            )
            .children(MEMBERS.iter().enumerate().map(|(index, member)| {
                let selected = self.selected == index;
                div()
                    .id(("member", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(rgb(0x34363f)))
                    .hover(|style| style.bg(rgb(0x2c2e36)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = index;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(10.0))
                            .rounded_full()
                            .bg(color(member.presence.color())),
                    )
                    .child(div().flex_1().text_color(rgb(0xe8e9ee)).child(member.name))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8a8d99))
                            .child(member.presence.label()),
                    )
            }));

        let details = self.details(window, cx);
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(rgb(0xe8e9ee))
            .child(top_bar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(members)
                    .child(details),
            );
        if self.show_settings {
            root = root.child(self.settings_panel(cx));
        }
        root
    }
}

impl Root {
    fn details(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let panel = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .p_3()
            .gap_2()
            .bg(rgb(0x121317));
        if let Some(watching) = &self.watching {
            let name = MEMBERS[watching.member].name;
            let mode = watching.player.mode;
            let header = div()
                .flex()
                .items_center()
                .gap_3()
                .child(format!("Watching {name}"))
                .child(
                    button("leave", "Leave").on_click(cx.listener(|this, _, _, cx| {
                        this.leave();
                        cx.notify();
                    })),
                )
                .child(
                    button(
                        "switch-mode",
                        if mode == Mode::Embedded {
                            "Pop out"
                        } else {
                            "Show here"
                        },
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(member) = this.watching.as_ref().map(|w| w.member) {
                            let next = if mode == Mode::Embedded {
                                Mode::PopOut
                            } else {
                                Mode::Embedded
                            };
                            this.watch(member, next, cx);
                        }
                    })),
                );
            if mode == Mode::PopOut {
                return panel.child(header).child(
                    div()
                        .text_color(rgb(0x8a8d99))
                        .child("The stream is playing in its own window."),
                );
            }
            // Embedded: the player's child window follows this area.
            let scale = window.scale_factor();
            let commands = watching.player_bounds_sender();
            let sent = Rc::clone(&watching.sent);
            let area = canvas(
                |_, _, _| {},
                move |bounds: Bounds<Pixels>, (), _, _| {
                    let physical = (
                        (f32::from(bounds.origin.x) * scale).round() as i32,
                        (f32::from(bounds.origin.y) * scale).round() as i32,
                        (f32::from(bounds.size.width) * scale).round() as i32,
                        (f32::from(bounds.size.height) * scale).round() as i32,
                    );
                    if sent.get() != Some(physical) {
                        sent.set(Some(physical));
                        commands(physical);
                    }
                },
            )
            .flex_1()
            .size_full();
            // GPUI composites its whole window above child windows
            // (DirectComposition target with topmost = true), from a
            // transparent clear: the player shows only where nothing is
            // painted. So this area, and every element up to the root, has
            // no background; the other panels paint their own.
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(header.p_3().bg(rgb(0x121317)))
                .child(div().flex_1().min_h_0().child(area));
        }
        let index = self.selected;
        let member = &MEMBERS[index];
        let mut panel = panel.child(div().text_xl().child(member.name)).child(
            div()
                .text_color(color(member.presence.color()))
                .child(member.presence.label()),
        );
        if !member.activity.is_empty() {
            panel = panel.child(div().text_color(rgb(0x8a8d99)).child(member.activity));
        }
        if member.presence == Presence::Live {
            panel.child(
                div()
                    .flex()
                    .gap_2()
                    .child(button("watch", "Watch stream").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.watch(index, Mode::Embedded, cx);
                        },
                    ))),
            )
        } else {
            panel.child(div().text_color(rgb(0x8a8d99)).child("Not streaming."))
        }
    }

    fn settings_panel(&mut self, cx: &mut Context<Self>) -> Div {
        let s = self.settings;
        let row = |label: &'static str, control: Stateful<Div>| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().w(px(140.0)).child(label))
                .child(control)
        };
        div()
            .absolute()
            .top(px(60.0))
            .left(px(340.0))
            .w(px(440.0))
            .p_3()
            .gap_2()
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(0x26272f))
            .border_1()
            .border_color(rgb(0x3a3c47))
            .child(div().text_lg().child("Settings"))
            .child(row(
                "Resolution",
                button("res", RESOLUTIONS[s.resolution].to_owned()).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.settings.resolution =
                            (this.settings.resolution + 1) % RESOLUTIONS.len();
                        cx.notify();
                    },
                )),
            ))
            .child(row(
                "Frame rate",
                button("fps", FRAME_RATES[s.frame_rate].to_owned()).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.settings.frame_rate =
                            (this.settings.frame_rate + 1) % FRAME_RATES.len();
                        cx.notify();
                    },
                )),
            ))
            .child(row(
                "Encoder",
                button("enc", ENCODERS[s.encoder].to_owned()).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.settings.encoder = (this.settings.encoder + 1) % ENCODERS.len();
                        cx.notify();
                    },
                )),
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(140.0)).child("Bitrate"))
                    .child(
                        button("bitrate-down", "-").on_click(cx.listener(|this, _, _, cx| {
                            this.settings.bitrate_kbps =
                                (this.settings.bitrate_kbps - 1_000).max(2_000);
                            cx.notify();
                        })),
                    )
                    .child(format!("{} kbps", s.bitrate_kbps))
                    .child(
                        button("bitrate-up", "+").on_click(cx.listener(|this, _, _, cx| {
                            this.settings.bitrate_kbps =
                                (this.settings.bitrate_kbps + 1_000).min(20_000);
                            cx.notify();
                        })),
                    ),
            )
            .child(row(
                "Show cursor",
                button(
                    "cursor",
                    if s.show_cursor { "On" } else { "Off" }.to_owned(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings.show_cursor = !this.settings.show_cursor;
                    cx.notify();
                })),
            ))
            .child(row(
                "Start with Windows",
                button(
                    "autostart",
                    if s.start_with_windows { "On" } else { "Off" }.to_owned(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.settings.start_with_windows = !this.settings.start_with_windows;
                    cx.notify();
                })),
            ))
    }
}

impl Watching {
    /// A `'static` sender of the video area, for the canvas paint callback.
    fn player_bounds_sender(&self) -> impl Fn((i32, i32, i32, i32)) + 'static {
        let commands = self.player.bounds_sender();
        move |(x, y, w, h)| commands(x, y, w, h)
    }
}

fn hwnd_of(window: &Window) -> isize {
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

fn main() {
    let args = parse_args();
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("ziz - GPUI prototype".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    window.on_next_frame(|_, _| {
                        println!(
                            "gpui: first frame {:.1} ms after process start",
                            feed::ms_since_process_start()
                        );
                    });
                    let hwnd = hwnd_of(window);
                    cx.new(|_| Root {
                        hwnd,
                        selected: 0,
                        streaming: false,
                        show_settings: false,
                        settings: Settings::default(),
                        watching: None,
                        summaries: Vec::new(),
                        generation: 0,
                    })
                },
            )
            .expect("open window");
        let root = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("root view");
        if args.watch {
            let mode = if args.popout {
                Mode::PopOut
            } else {
                Mode::Embedded
            };
            root.update(cx, |root, cx| root.watch(0, mode, cx));
        }
        if let Some(limit) = args.seconds {
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs_f64(limit))
                    .await;
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }
        let quit_root = root.clone();
        cx.on_window_closed(move |cx| {
            quit_root.update(cx, |root, _| root.leave());
            cx.quit();
        })
        .detach();
        cx.on_app_quit(move |cx| {
            let summaries = root.update(cx, |root, _| {
                root.leave();
                std::mem::take(&mut root.summaries)
            });
            for summary in summaries {
                println!("gpui: {summary}");
            }
            async {}
        })
        .detach();
        if args.minimized {
            let _ = window.update(cx, |_, window, _| window.minimize_window());
        } else {
            cx.activate(true);
        }
    });
}
