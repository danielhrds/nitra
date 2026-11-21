//! Test helper (not part of the app): plays a quiet sine tone on the default
//! output and shows a small window titled "ziz tone <hz>", so tests can
//! check which apps' sound a capture takes. Tests copy this program under
//! other names (e.g. `Discord.exe`) to stand in for those apps.
//!
//! Usage: tone-player <hz> <seconds> [amplitude]

use std::time::{Duration, Instant};

use audio::opus::{CHANNELS, FRAME_SAMPLES, SAMPLE_RATE};
use audio::player::AudioPlayer;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG,
    PM_REMOVE, PeekMessageW, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, w};

unsafe extern "system" fn window_proc(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: forwards the arguments Windows gave us.
    unsafe { DefWindowProcW(window, message, w, l) }
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let hz: f32 = args
        .get(1)
        .and_then(|a| a.parse().ok())
        .ok_or("usage: tone-player <hz> <seconds> [amplitude]")?;
    let seconds: f32 = args
        .get(2)
        .and_then(|a| a.parse().ok())
        .ok_or("missing seconds")?;
    let amplitude: f32 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(0.05);

    let class = w!("ziz-tone-player");
    let title = HSTRING::from(format!("ziz tone {hz}"));
    // SAFETY: a plain window class and a small visible window; the window
    // procedure only forwards to DefWindowProcW. Destroyed before exit.
    let window = unsafe {
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: class,
            ..Default::default()
        });
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            320,
            120,
            None,
            None,
            None,
            None,
        )
        .map_err(|e| e.to_string())?
    };

    let player = AudioPlayer::start(Duration::from_millis(100))?;
    let started = Instant::now();
    let mut n: u64 = 0;
    let mut frame = 0_u64;
    while started.elapsed().as_secs_f32() < seconds {
        let samples: Vec<f32> = (0..FRAME_SAMPLES)
            .flat_map(|_| {
                let t = n as f32 / SAMPLE_RATE as f32;
                n += 1;
                [amplitude * (2.0 * std::f32::consts::PI * hz * t).sin(); CHANNELS]
            })
            .collect();
        player.push(&samples);
        frame += 1;
        // Keep the window responsive while pacing at 20 ms per frame.
        let mut message = MSG::default();
        // SAFETY: standard message pump on this thread's queue.
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let due = Duration::from_millis(20 * frame);
        if let Some(wait) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(wait);
        }
    }
    player.stop();
    // SAFETY: destroys the window created above, once.
    let _ = unsafe { DestroyWindow(window) };
    Ok(())
}
